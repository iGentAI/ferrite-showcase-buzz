//! Pooled plane handles: keeping the connection the platform already pools.
//!
//! ## The problem this exists to solve
//!
//! A plane client owns its connection. From `ferrite-store`:
//!
//! ```text
//! pub struct ClusterClient {
//!     /// The pooled client (P2.x): one live, already-authenticated connection
//!     /// reused across requests; dropped on transport errors and replaced on
//!     /// failover.
//!     pooled: Option<(SocketAddr, StoreClient)>,
//!     /// Fresh dials made (pooled-connection reuse observability).
//!     dials: u64,
//! }
//! ```
//!
//! And `AppContext::store` builds a **fresh client on every call** (it goes
//! through `FerriteClient::new(conductor_addr)`). So `ctx.store("events")` in a
//! request handler is not a cheap handle lookup: it is a client whose pool
//! starts empty, and whose first operation therefore pays primary discovery, a
//! TCP dial, and the capability handshake before it does any work.
//!
//! One `POST /buzz/api/events` crossed six of those inside `ingest_event` alone
//! — config, events, guards, events again, feed, events again — before counting
//! the authenticating read, the idempotency record, and the dispatch publish.
//! The platform was pooling correctly; the app was throwing the pool away.
//!
//! ## What this module does
//!
//! Keeps the clients. A checkout hands out an idle client for a binding (or
//! builds one the first time), and [`Pooled`] returns it on drop with its
//! connection still warm — so every request after the first pays nothing for
//! setup. The clients are the platform's own, unmodified, so failover,
//! redirects, capability tokens and key-prefix scoping all behave exactly as
//! before; this changes only how long an instance lives.
//!
//! ## Why a process-global pool rather than a per-request bag
//!
//! Because it needs no signature changes. A per-request "planes" struct threaded
//! through every helper would touch ~130 call sites' types and every function
//! that takes a client; a checkout keyed by binding touches only the expression
//! that used to build the client. One process is one app instance, so a global
//! keyed by binding is exactly the right lifetime.
//!
//! ## Why returning a used client is safe
//!
//! * A client that hit a transport error has already dropped its own pooled
//!   connection ("dropped on transport errors and replaced on failover"), so a
//!   reused instance simply re-dials. Nothing sticky survives a failure.
//! * Capability token, key prefix and group membership are properties of the
//!   BINDING, and a bucket is keyed by binding AND topology — a client can never
//!   be handed to a caller wanting different authority, or a different fleet.
//! * The mutex is held only to move a client in or out, never across an await,
//!   so this adds no contention to the request path itself.
//!
//! ## Why QUEUE clients are deliberately excluded
//!
//! Store, APE, topics and blob clients are request/response: one operation
//! finishes before the next begins, so reusing an instance is invisible. A
//! queue client is not. It carries lease state and long-poll semantics, and one
//! instance would be shared between a CONSUMER that long-polls for the life of
//! the process and request tasks that PUBLISH — two roles that interleave on
//! the single connection the client keeps.
//!
//! That is not a theoretical objection; it was measured on this app's own
//! bounds gate. Pooling the api loop's reply publish made a 250 KiB
//! submission's reply never arrive at all: three re-sends, six consecutive
//! 120 s reply-budget timeouts, deterministic in an isolated run, with the app
//! itself idle (every thread parked, no panic, no error logged) — the request
//! had been consumed and its answer had gone nowhere. The identical test passes
//! with that ONE publish unpooled and every other plane still pooled.
//!
//! So there is no `queue` helper here, on purpose. `ctx.queue(binding)` is the
//! right call for a publish, and a long-lived consumer (the api, ws, stream and
//! turn loops) keeps its own client for its whole life — which needs no pool
//! either, since it never stops using it.

use std::io;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use ferrite_app::client::{
    ApeClusterClient, BlobClusterClient, StoreClusterClient, TopicLogClient,
};
use ferrite_app::AppContext;

/// Clients handed out, and clients BUILT.
///
/// The ratio is the whole point of this module, and the only honest way to state
/// it: a build is a fresh client, and a fresh client's first operation pays
/// primary discovery, a dial and the capability handshake. If `builds` tracks
/// `checkouts`, nothing is being reused and this module is not earning its
/// keep; if `builds` flattens while `checkouts` climbs, connections are being
/// kept. Both are reported on the operator metrics tier and asserted by a gate.
///
/// Passed in rather than read from a global inside [`checkout`] so a test can
/// count against its OWN instance: the process-global pair below is shared by
/// every caller, and a unit test that snapshotted it would be measuring whatever
/// sibling tests happened to be doing at the time.
struct Counters {
    checkouts: AtomicU64,
    builds: AtomicU64,
}

impl Counters {
    const fn new() -> Self {
        Self {
            checkouts: AtomicU64::new(0),
            builds: AtomicU64::new(0),
        }
    }
}

static COUNTERS: Counters = Counters::new();

/// The pool's EPOCH, bumped by [`reset`].
///
/// Every bucket records the epoch it was created in, and a checked-out client
/// carries its bucket's. A client is returned only if its bucket is still the one
/// it came from, which is what makes the instance boundary [`reset`] draws hold
/// even for a client that was ALREADY checked out when the reset happened — and
/// even if the next fleet resolves to the same addresses, where the topology
/// check alone would wave it through.
///
/// The comparison is bucket-against-client, never against this counter directly:
/// a bucket created before a bump keeps its own epoch, so bumping cannot
/// invalidate clients belonging to a pool nobody reset.
static EPOCH: AtomicU64 = AtomicU64::new(0);

/// Idle clients kept per binding before a returned one is simply dropped.
///
/// Sized off the app's own concurrency ceilings rather than picked: the api loop
/// serves up to `MAX_IN_FLIGHT` (32) requests at once, the ws loop the same, and
/// each request touches a handful of bindings — so a warm set this size covers
/// steady state, while the cap keeps a burst from parking sockets forever.
const MAX_IDLE_PER_BINDING: usize = 64;

/// One binding's idle clients, and the topology they are attached to.
///
/// The topology is part of the identity, not decoration: a pooled client is
/// pinned to a group and a member address list, so a client built for one
/// topology must never be handed to a caller resolving a different one.
struct Bucket<C> {
    binding: String,
    topology: String,
    /// The [`EPOCH`] this bucket was created or re-pointed in.
    epoch: u64,
    idle: Vec<C>,
}

/// Idle clients for one plane.
///
/// A `Vec` of buckets rather than a `HashMap` because `Vec::new()` is const (so
/// the pools are plain statics with no lazy initialisation) and a deployment has
/// ~16 bindings — a linear scan over that is cheaper than hashing.
type Pool<C> = Mutex<Vec<Bucket<C>>>;

static STORES: Pool<StoreClusterClient> = Mutex::new(Vec::new());
static APES: Pool<ApeClusterClient> = Mutex::new(Vec::new());
static TOPICS: Pool<TopicLogClient> = Mutex::new(Vec::new());
static BLOBS: Pool<BlobClusterClient> = Mutex::new(Vec::new());

/// Take the lock, treating a poisoned mutex as usable.
///
/// Poisoning here would mean a panic while a client was being moved in or out of
/// a `Vec`; the data is a list of clients, so there is no torn invariant to
/// protect and refusing service over it would be strictly worse than continuing.
fn lock<C>(pool: &'static Pool<C>) -> std::sync::MutexGuard<'static, Vec<Bucket<C>>> {
    match pool.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// A checked-out plane client. Derefs to the client, and returns it to the pool
/// when dropped — including on an error path or an unwind, which is why the
/// return lives in `Drop` rather than at the end of a happy path.
pub struct Pooled<C: 'static> {
    client: Option<C>,
    binding: String,
    topology: String,
    /// The epoch of the bucket this client came from.
    epoch: u64,
    pool: &'static Pool<C>,
}

impl<C: 'static> Deref for Pooled<C> {
    type Target = C;

    fn deref(&self) -> &C {
        self.client
            .as_ref()
            .expect("a checked-out client is present until drop")
    }
}

impl<C: 'static> DerefMut for Pooled<C> {
    fn deref_mut(&mut self) -> &mut C {
        self.client
            .as_mut()
            .expect("a checked-out client is present until drop")
    }
}

impl<C: 'static> Drop for Pooled<C> {
    fn drop(&mut self) {
        let Some(client) = self.client.take() else {
            return;
        };
        let mut pool = lock(self.pool);
        match pool
            .iter_mut()
            .find(|bucket| bucket.binding == self.binding)
        {
            // The binding still resolves where this client is attached, AND the
            // bucket is still the one it came from: keep it warm, up to the cap.
            Some(bucket) if bucket.topology == self.topology && bucket.epoch == self.epoch => {
                if bucket.idle.len() < MAX_IDLE_PER_BINDING {
                    bucket.idle.push(client);
                }
            }
            // The binding MOVED while this client was checked out, or a `reset`
            // drew a new instance boundary under it. Either way it belongs to a
            // world nobody resolves any more, so let it close here rather than
            // park a socket that could never serve a request.
            Some(_) => {}
            // Unreachable in practice: `checkout` creates the bucket before any
            // client exists to return. Dropping is the safe answer anyway —
            // pooling under an unknown topology is what the `Some(_)` arm above
            // exists to prevent.
            None => {}
        }
    }
}

/// Take an idle client for `binding` on its CURRENT `topology`, or build one.
///
/// Maintains exactly ONE bucket per binding, and that bucket records the
/// topology the binding currently resolves to. Two things depend on it:
///
/// * **Eviction.** When the topology changes, the bucket is re-pointed and its
///   idle clients are dropped there and then — they are pinned to addresses
///   nobody resolves any more, so keeping them would park sockets for the life
///   of the process.
/// * **A correct return.** [`Pooled::drop`] has no `AppContext` and so cannot
///   ask what is current; it compares against the topology recorded here. That
///   only works if the bucket SURVIVES the move — an earlier version removed it,
///   so a client returned after a move found no bucket and recreated one for the
///   dead topology, which is exactly the leak this is meant to prevent. (Its own
///   test caught that.)
fn checkout<C: 'static>(
    pool: &'static Pool<C>,
    binding: &str,
    topology: &str,
    counters: &Counters,
    build: impl FnOnce() -> io::Result<C>,
) -> io::Result<Pooled<C>> {
    // Scoped so the lock is released before `build` — which dials — runs.
    let (idle, epoch) = {
        let mut guard = lock(pool);
        let current = EPOCH.load(Ordering::Relaxed);
        match guard.iter_mut().find(|bucket| bucket.binding == binding) {
            Some(bucket) if bucket.topology == topology => (bucket.idle.pop(), bucket.epoch),
            Some(bucket) => {
                // The binding moved: retire what was pooled for the old place,
                // and treat the re-pointed bucket as a new one.
                bucket.topology = topology.to_string();
                bucket.epoch = current;
                bucket.idle.clear();
                (None, current)
            }
            None => {
                guard.push(Bucket {
                    binding: binding.to_string(),
                    topology: topology.to_string(),
                    epoch: current,
                    idle: Vec::new(),
                });
                (None, current)
            }
        }
    };
    let client = match idle {
        Some(client) => client,
        None => {
            // Counted only once it EXISTS: a failed construction dialled
            // nothing, and reporting it as a built client would inflate the very
            // number that is supposed to show reuse.
            let built = build()?;
            counters.builds.fetch_add(1, Ordering::Relaxed);
            built
        }
    };
    counters.checkouts.fetch_add(1, Ordering::Relaxed);
    Ok(Pooled {
        client: Some(client),
        binding: binding.to_string(),
        topology: topology.to_string(),
        epoch,
        pool,
    })
}

/// The topology a binding currently resolves to: its group and member fronts.
///
/// Part of the pool identity because a pooled client is pinned to those
/// addresses. If a deployment re-materialises a binding onto a different group
/// or different members, a client pooled under the bare name would be handed
/// back out still wired to the old topology and would spend its whole retry
/// budget on addresses nobody serves.
///
/// The in-process test harness depends on this too: several gates stand up their
/// OWN fleet inside one process, all using the same binding names, so name-only
/// keying would hand the second fleet a client connected to the first one's (now
/// gone) nodes, and the gate would hang retrying instead of failing.
fn topology_of(ctx: &AppContext, binding: &str) -> String {
    let Some(entry) = ctx.payload().services.get(binding) else {
        return String::new();
    };
    let mut topology = String::new();
    for (group, members) in shards_of(entry) {
        topology.push_str(group);
        for member in members {
            // The client front is what a pooled connection is actually attached to.
            topology.push('\u{1f}');
            topology.push_str(&member.client_addr.to_string());
        }
        topology.push('\u{1e}');
    }
    // Everything else the constructed client binds is identity too: the kind, the read
    // mode, the tenant key prefix and each shard's resource, plane generation and
    // capability token. Two contexts that share a cluster's seats but not that authority
    // or scoping must never share a pooled client, so the whole entry's digest is part of
    // the key (a digest, so the token itself is not held in the pool's key).
    if let Ok(bytes) = ferrite_app::serde_json::to_vec(entry) {
        topology.push('\u{1d}');
        for byte in ferrite_app::crypto::sha256(&bytes) {
            topology.push_str(&format!("{byte:02x}"));
        }
    }
    topology
}

/// The groups and member fronts behind a service entry: upstream Ferrite Core
/// carries a list of shards, each with its own group and members, and this is
/// the one place buzz reads that shape.
fn shards_of(entry: &ferrite_app::ServiceEntry) -> Vec<(&str, &[ferrite_app::WireMember])> {
    entry
        .shards
        .iter()
        .map(|shard| (shard.group.as_str(), shard.members.as_slice()))
        .collect()
}

/// A pooled Store client for `binding`.
pub fn store(ctx: &AppContext, binding: &str) -> io::Result<Pooled<StoreClusterClient>> {
    checkout(
        &STORES,
        binding,
        &topology_of(ctx, binding),
        &COUNTERS,
        || ctx.store(binding),
    )
}

/// A pooled APE client for `binding`.
pub fn ape(ctx: &AppContext, binding: &str) -> io::Result<Pooled<ApeClusterClient>> {
    checkout(
        &APES,
        binding,
        &topology_of(ctx, binding),
        &COUNTERS,
        || ctx.ape(binding),
    )
}

/// A pooled Topics client for `binding`.
pub fn topics(ctx: &AppContext, binding: &str) -> io::Result<Pooled<TopicLogClient>> {
    checkout(
        &TOPICS,
        binding,
        &topology_of(ctx, binding),
        &COUNTERS,
        || ctx.topics(binding),
    )
}

/// A pooled Blob client for `binding`.
pub fn blob(ctx: &AppContext, binding: &str) -> io::Result<Pooled<BlobClusterClient>> {
    checkout(
        &BLOBS,
        binding,
        &topology_of(ctx, binding),
        &COUNTERS,
        || ctx.blob(binding),
    )
}

/// Discard every pooled client.
///
/// The pool belongs to a SERVING INSTANCE, not to the process, and this is what
/// says so: [`crate::relay::ingress::serve`] calls it as it starts, so an
/// instance never inherits connections opened by a previous one.
///
/// In production that happens once per process and costs nothing — there is no
/// previous instance to inherit from. It earns its keep in the in-process test
/// harness, where several complete fleets are stood up in SEQUENCE inside one
/// process under the same binding names: without this, a fleet's app could be
/// handed a client attached to the previous fleet's (now gone) nodes, and the
/// request that used it never completed — measured as six consecutive
/// reply-budget timeouts in the bounds gate, reproducible only when the gate's
/// six fleets ran in one binary and never when the same test ran alone.
///
/// It is deliberately paired with topology keying rather than replacing it: this
/// covers a NEW instance inheriting old clients, and the topology check in
/// [`checkout`] covers the opposite order — an old instance returning a client
/// after the new one has already started. The [`EPOCH`] bump closes the gap
/// between them: a client checked out before the reset is refused on return even
/// if the new instance recreated the same binding at the same topology. The
/// counters are cumulative and are NOT reset: they measure the process's reuse,
/// and an operator reading them wants the total, not the interval.
pub fn reset() {
    // The epoch moves while every pool is held, so no checkout can stamp the new epoch on
    // a bucket that is then cleared and later recreated under that same epoch (an outgoing
    // instance's client would then rejoin the new instance's pool). Checkouts take one pool
    // lock at a time, so taking all four in this fixed order cannot deadlock.
    let mut stores = lock(&STORES);
    let mut apes = lock(&APES);
    let mut topics = lock(&TOPICS);
    let mut blobs = lock(&BLOBS);
    bump_epoch();
    stores.clear();
    apes.clear();
    topics.clear();
    blobs.clear();
}

/// Draw a new instance boundary without touching any particular pool.
///
/// [`reset`] is this plus discarding the four production pools. They are split
/// so the boundary can be exercised against a test's OWN pool: unit tests run in
/// parallel, and a test that called `reset` would clear pools belonging to the
/// process rather than to itself.
fn bump_epoch() {
    EPOCH.fetch_add(1, Ordering::Relaxed);
}

/// Idle client counts per plane, for the operator metrics tier.
///
/// This is the observable half of the change: a deployment serving traffic with
/// warm pools shows idle clients here, and `connection_dials()` on the clients
/// themselves stops climbing. Without it, "we pool now" would be an assertion
/// rather than something an operator can check.
#[must_use]
pub fn idle_counts() -> Vec<(&'static str, usize)> {
    fn total<C>(pool: &'static Pool<C>) -> usize {
        lock(pool).iter().map(|bucket| bucket.idle.len()).sum()
    }
    vec![
        ("store", total(&STORES)),
        ("ape", total(&APES)),
        ("topics", total(&TOPICS)),
        ("blob", total(&BLOBS)),
    ]
}

/// Clients handed out since start.
#[must_use]
pub fn checkouts() -> u64 {
    COUNTERS.checkouts.load(Ordering::Relaxed)
}

/// Clients BUILT since start — one fresh connection each.
///
/// On a warm deployment this settles near the number of (binding, topology)
/// pairs in use, and stops growing while `checkouts` keeps climbing. Growth that
/// tracks `checkouts` means reuse is not happening.
#[must_use]
pub fn builds() -> u64 {
    COUNTERS.builds.load(Ordering::Relaxed)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Counters local to one test, so a sibling running in parallel cannot move
    /// the numbers being asserted.
    fn counters() -> Counters {
        Counters::new()
    }

    /// The pool must hand back the SAME instance rather than building a second
    /// one — that identity is the whole point, because the connection lives on
    /// the instance.
    #[test]
    fn a_returned_client_is_the_one_handed_out_next() {
        static TEST_POOL: Pool<String> = Mutex::new(Vec::new());
        let counted = counters();
        {
            let first = checkout(&TEST_POOL, "events", "g1", &counted, || {
                Ok("client-1".to_string())
            })
            .unwrap();
            assert_eq!(&*first, "client-1");
        } // returned here
        let second = checkout(&TEST_POOL, "events", "g1", &counted, || {
            Ok("client-2".to_string())
        })
        .unwrap();
        assert_eq!(&*second, "client-1", "the idle client must be reused");
        assert_eq!(
            counted.builds.load(Ordering::Relaxed),
            1,
            "no second client should have been built"
        );
        assert_eq!(
            counted.checkouts.load(Ordering::Relaxed),
            2,
            "both hand-outs are counted"
        );
    }

    /// A build that FAILS produced no connection, so it must not be counted as
    /// one — otherwise the number that is supposed to demonstrate reuse would be
    /// inflated by failures.
    #[test]
    fn a_failed_build_is_not_counted() {
        static TEST_POOL: Pool<String> = Mutex::new(Vec::new());
        let counted = counters();
        let failed = checkout(&TEST_POOL, "broken", "g1", &counted, || {
            Err(io::Error::other("no such binding"))
        });
        assert!(failed.is_err());
        assert_eq!(counted.builds.load(Ordering::Relaxed), 0);
        assert_eq!(
            counted.checkouts.load(Ordering::Relaxed),
            0,
            "a checkout that produced nothing is not a hand-out"
        );
    }

    /// Two bindings never share a client: they can carry different capability
    /// tokens and key prefixes, so a cross-binding hand-out would be an
    /// authority bug, not merely a mix-up.
    #[test]
    fn bindings_do_not_share_clients() {
        static TEST_POOL: Pool<String> = Mutex::new(Vec::new());
        let counted = counters();
        drop(
            checkout(&TEST_POOL, "events", "g1", &counted, || {
                Ok("for-events".to_string())
            })
            .unwrap(),
        );
        let other = checkout(&TEST_POOL, "config", "g1", &counted, || {
            Ok("for-config".to_string())
        })
        .unwrap();
        assert_eq!(
            &*other, "for-config",
            "a client checked out for one binding must never serve another"
        );
    }

    /// Concurrent holders get distinct clients (one connection each), and the
    /// pool keeps only up to its cap when they all come back.
    #[test]
    fn concurrent_holders_get_distinct_clients_and_the_cap_holds() {
        static TEST_POOL: Pool<usize> = Mutex::new(Vec::new());
        let counted = counters();
        let mut held = Vec::new();
        for i in 0..MAX_IDLE_PER_BINDING + 8 {
            held.push(checkout(&TEST_POOL, "events", "g1", &counted, || Ok(i)).unwrap());
        }
        assert_eq!(
            held.len(),
            MAX_IDLE_PER_BINDING + 8,
            "every concurrent caller needs its own client"
        );
        drop(held);
        let idle: usize = lock(&TEST_POOL).iter().map(|b| b.idle.len()).sum();
        assert_eq!(idle, MAX_IDLE_PER_BINDING, "the cap bounds what is kept");
    }

    /// A binding that MOVES retires its old bucket instead of parking those
    /// sockets for the life of the process, and a client returned after the move
    /// is closed rather than pooled onto a topology nobody resolves.
    #[test]
    fn a_moved_binding_retires_its_stale_clients() {
        static TEST_POOL: Pool<String> = Mutex::new(Vec::new());
        let counted = counters();
        let build = |name: &str| {
            let name = name.to_string();
            move || Ok(name)
        };

        // Warm the pool on the first topology, and hold one across the move.
        drop(checkout(&TEST_POOL, "events", "fleet-a", &counted, build("a-1")).unwrap());
        drop(checkout(&TEST_POOL, "events", "fleet-a", &counted, build("a-2")).unwrap());
        let in_flight = checkout(&TEST_POOL, "events", "fleet-a", &counted, build("a-3")).unwrap();
        assert!(
            lock(&TEST_POOL)
                .iter()
                .any(|b| b.binding == "events" && b.topology == "fleet-a"),
            "the binding's bucket records the topology it currently resolves to"
        );

        // The binding now resolves elsewhere: the stale clients go, and the new
        // topology gets a fresh client rather than one wired to the old fleet.
        let moved = checkout(&TEST_POOL, "events", "fleet-b", &counted, build("b-1")).unwrap();
        assert_eq!(&*moved, "b-1", "a moved binding must not reuse old clients");
        assert!(
            !lock(&TEST_POOL)
                .iter()
                .any(|b| b.topology == "fleet-a" || !b.idle.is_empty()),
            "the stale clients must be retired, not retained"
        );

        // Returning the client held across the move must not resurrect the old
        // topology — the bucket survives the move precisely so this comparison
        // can be made.
        drop(in_flight);
        assert!(
            !lock(&TEST_POOL).iter().any(|b| b.topology == "fleet-a"),
            "a client returned after its binding moved must be closed, not pooled"
        );
        assert!(
            lock(&TEST_POOL).iter().all(|b| b.idle.is_empty()),
            "and it must not be parked on the new topology either"
        );
        drop(moved);
        let buckets = lock(&TEST_POOL)
            .iter()
            .filter(|b| b.binding == "events")
            .count();
        assert_eq!(buckets, 1, "one live bucket per binding");
        assert_eq!(
            lock(&TEST_POOL)
                .iter()
                .find(|b| b.binding == "events")
                .map(|b| b.idle.len()),
            Some(1),
            "the current-topology client is the one that stays warm"
        );
    }

    /// A client checked out BEFORE a reset must not be pooled after it — even
    /// when the new instance recreates the same binding at the SAME topology,
    /// which is the one case topology keying cannot exclude (two fleets can bind
    /// the same addresses if the OS hands the ports back).
    #[test]
    fn a_client_checked_out_before_a_reset_is_refused_afterwards() {
        static TEST_POOL: Pool<String> = Mutex::new(Vec::new());
        let counted = counters();

        // The previous instance takes a client and is still holding it.
        let in_flight = checkout(&TEST_POOL, "events", "same-topology", &counted, || {
            Ok("from-the-old-instance".to_string())
        })
        .unwrap();

        // A new instance starts. `reset` is a bump plus discarding the pools it
        // owns; here the boundary is drawn against THIS pool only, so a sibling
        // test's clients cannot be swept up by it.
        bump_epoch();
        lock(&TEST_POOL).clear();

        // It warms its own pool at the SAME topology, so topology alone would
        // not tell the two apart.
        drop(
            checkout(&TEST_POOL, "events", "same-topology", &counted, || {
                Ok("from-the-new-instance".to_string())
            })
            .unwrap(),
        );

        // Now the old instance lets go. Its client belongs to a bucket that no
        // longer exists, so it must close rather than join the new pool.
        drop(in_flight);
        let idle: Vec<String> = lock(&TEST_POOL)
            .iter()
            .flat_map(|b| b.idle.clone())
            .collect();
        assert_eq!(
            idle,
            vec!["from-the-new-instance".to_string()],
            "a client from before the reset must not be pooled after it"
        );

        // And the new instance's own client is still reused, so the boundary did
        // not cost the pool its purpose.
        let next = checkout(&TEST_POOL, "events", "same-topology", &counted, || {
            Ok("a-third-client".to_string())
        })
        .unwrap();
        assert_eq!(&*next, "from-the-new-instance");
    }

    /// The topology string separates fleets and groups that share a binding name.
    #[test]
    fn the_topology_separates_fleets_that_share_a_binding_name() {
        use std::collections::BTreeMap;

        use ferrite_app::{InjectionPayload, ServiceEntry, ShardTarget, WireMember};

        fn ctx_on(port: u16, group: &str) -> AppContext {
            let member = WireMember {
                zone: None,
                node_id: "n0".to_string(),
                client_addr: format!("127.0.0.1:{port}").parse().unwrap(),
                repl_addr: format!("127.0.0.1:{}", port + 1).parse().unwrap(),
            };
            let mut services = BTreeMap::new();
            services.insert(
                "events".to_string(),
                ServiceEntry {
                    kind: "store".to_string(),
                    key_prefix: None,
                    reads: Default::default(),
                    shards: vec![ShardTarget {
                        group: group.to_string(),
                        plane_generation: Some(1),
                        members: vec![member],
                        resource: None,
                        token: None,
                    }],
                },
            );
            let payload: InjectionPayload = ferrite_app::serde_json::from_value(
                ferrite_app::serde_json::json!({
                    "conductor_addrs": ["127.0.0.1:26000"],
                    "services": services,
                }),
            ).expect("the public context JSON supplies SDK defaults");
            AppContext::new(payload)
        }

        let fleet_a = ctx_on(26101, "buzz-events-g");
        assert_ne!(
            topology_of(&fleet_a, "events"),
            topology_of(&ctx_on(36101, "buzz-events-g"), "events"),
            "different member addresses must be different topologies"
        );
        assert_ne!(
            topology_of(&fleet_a, "events"),
            topology_of(&ctx_on(26101, "buzz-events-g2"), "events"),
            "a different group must be a different topology"
        );
        assert_eq!(
            topology_of(&fleet_a, "events"),
            topology_of(&ctx_on(26101, "buzz-events-g"), "events"),
            "the same topology must match, or nothing would ever be reused"
        );
        assert_eq!(
            topology_of(&fleet_a, "absent"),
            "",
            "an unbound name yields an empty topology rather than panicking"
        );
    }
}

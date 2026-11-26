//! Resolving access, the stream token key and the relay's own identity.

use ferrite_app::tracing;
use ferrite_app::serde_json;
use ferrite_app::AppContext;
use crate::relay::access::ChannelAccess;
use crate::relay::keys;
use crate::relay::planes;
use crate::relay::store;
use crate::relay::BuzzError;
use crate::relay::Result;
use super::{RELAY_IDENTITY_CONFIG, RELAY_IDENTITY_SECRET, STREAM_KEY_CONFIG, STREAM_KEY_SECRET};

/// Resolve a channel's visibility and the reader's membership, failing closed.
pub(super) async fn resolve_access(
    ctx: &AppContext,
    community: &str,
    channel: &str,
    reader: &crate::relay::event::PubKey,
    now_ms: u64,
) -> ChannelAccess {
    let Ok(mut events) = planes::store(ctx, store::EVENTS) else {
        return ChannelAccess::Unresolved;
    };
    let Ok(mut config) = planes::store(ctx, store::CONFIG) else {
        return ChannelAccess::Unresolved;
    };
    match super::community::community_is_closed(&mut config).await {
        Ok(false) => {}
        Ok(true) => match events.get(&keys::community_mem(community, reader)).await {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => return ChannelAccess::Unresolved,
        },
        Err(_) => return ChannelAccess::Unresolved,
    }
    match crate::relay::effects::restriction_of(&mut events, community, reader).await {
        Ok(Some(r)) if r.ban_blocks_access(now_ms) => return ChannelAccess::Unresolved,
        Ok(_) => {}
        Err(_) => return ChannelAccess::Unresolved,
    }
    // The channel document lives on the APE plane, because every mutation to it
    // is a guarded compare-and-swap; reading it from the Store would always miss
    // and quietly deny every channel-scoped event.
    let Ok(mut guards) = planes::ape(ctx, store::GUARDS) else {
        return ChannelAccess::Unresolved;
    };
    let Ok(guard_key) = keys::guard_channel(community, channel) else {
        // A channel id that cannot even be encoded as a key is not a channel we
        // have; refusing is the only safe answer.
        return ChannelAccess::Unresolved;
    };
    let doc = match guards.get(&guard_key).await {
        Ok(Some(bytes)) => match serde_json::from_slice::<crate::relay::channel::Channel>(&bytes) {
            Ok(c) => c,
            Err(_) => return ChannelAccess::Unresolved,
        },
        // An unknown channel is not an open channel.
        Ok(None) | Err(_) => return ChannelAccess::Unresolved,
    };
    if !doc.is_visible() {
        return ChannelAccess::Unresolved;
    }
    if !doc.visibility.is_private() {
        return ChannelAccess::Open;
    }
    let Ok(mem_key) = keys::mem(community, channel, reader) else {
        return ChannelAccess::Unresolved;
    };
    match store::get_doc::<crate::relay::channel::Membership>(&mut events, &mem_key).await {
        Ok(Some(m)) => ChannelAccess::Private {
            reader_is_member: m.is_live(),
        },
        Ok(None) => ChannelAccess::Private {
            reader_is_member: false,
        },
        Err(_) => ChannelAccess::Unresolved,
    }
}

/// Check a live stream/WebSocket subscriber's revocation tombstone, active ban
/// state, and closed-community membership in one batched `events` store read.
pub(super) async fn check_subscriber_standing(
    events_store: &mut ferrite_app::client::StoreClusterClient,
    config: &mut ferrite_app::client::StoreClusterClient,
    community: &str,
    subscriber: &crate::relay::event::PubKey,
    now_ms: u64,
) -> Result<(Option<Vec<u8>>, bool)> {
    let rev_key = keys::revoked(community, subscriber);
    let ban_key = keys::restriction(community, subscriber);
    let cmem_key = keys::community_mem(community, subscriber);
    let lookup_keys = [rev_key, ban_key.clone(), cmem_key];
    let rows = store::multi_get_raw(events_store, &lookup_keys).await?;
    let tombstone = rows.first().and_then(Clone::clone);
    let banned = match rows.get(1).and_then(Option::as_deref) {
        Some(bytes) => store::decode_doc::<crate::relay::effects::Restriction>(&ban_key, bytes)?
            .ban_blocks_access(now_ms),
        None => false,
    };
    if banned {
        return Ok((tombstone, true));
    }
    if super::community::community_is_closed(config).await? {
        let is_member = rows.get(2).is_some_and(Option::is_some);
        if !is_member {
            return Ok((tombstone, true));
        }
    }
    Ok((tombstone, false))
}

/// The sealed key that signs stream tokens: a secret when injected, else an
/// operator-written config value. Absent both, subscriptions are refused —
/// minting tokens under a guessable key would make them forgeable.
pub(super) async fn stream_key(
    ctx: &AppContext,
    config: &mut ferrite_app::client::StoreClusterClient,
) -> Result<Vec<u8>> {
    if let Some(s) = ctx.secret(STREAM_KEY_SECRET) {
        return Ok(s.as_bytes().to_vec());
    }
    match config.get(STREAM_KEY_CONFIG).await {
        Ok(Some(v)) if v.len() >= 32 => Ok(v),
        Ok(_) => Err(BuzzError::Internal(
            "stream token key is not configured; subscriptions are unavailable".into(),
        )),
        Err(e) => Err(BuzzError::Storage(format!("stream key read: {e}"))),
    }
}

/// The relay's OWN signing identity: the key it signs the NIP-29 group
/// projections (39000/39001/39002) with.
///
/// Same two-place lookup as [`stream_key`], for the same reason — a secret is
/// the preferred home, and the config-store fallback is what lets a dev
/// deployment run without a secrets round trip. The value is 64-hex, a
/// secp256k1 secret; a client can never submit these kinds
/// ([`crate::relay::kind::is_relay_only_kind`]), so this key IS the authority behind
/// every member list a client trusts.
///
/// Unlike [`stream_key`] this returns an `Option` rather than an error: an
/// absent identity must not fail a member's write. It disables projection
/// publishing and says so on a 15-minute cadence ([`warn_relay_identity`]) —
/// a per-event warning on a busy workspace would be its own outage, and a
/// once-per-process warning goes quiet exactly when a fleet restart silently
/// drops the secret.
pub(super) async fn relay_identity(
    ctx: &AppContext,
    config: &mut ferrite_app::client::StoreClusterClient,
) -> Option<[u8; 32]> {
    relay_identity_sourced(ctx, config)
        .await
        .map(|(seckey, _)| seckey)
}

/// Where a resolved relay identity came from — surfaced by `/buzz/api/metrics`
/// so an operator can tell a secret-backed deployment from a config-fallback
/// one without reading either value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IdentitySource {
    Secret,
    Config,
}

impl IdentitySource {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Secret => "secret",
            Self::Config => "config",
        }
    }
}

/// [`relay_identity`], also naming the plane the key came from.
pub(super) async fn relay_identity_sourced(
    ctx: &AppContext,
    config: &mut ferrite_app::client::StoreClusterClient,
) -> Option<([u8; 32], IdentitySource)> {
    let now_ms = ctx.now_ms();
    let (hex, source) = match ctx.secret(RELAY_IDENTITY_SECRET) {
        Some(injected) => (injected, IdentitySource::Secret),
        None => match config.get(RELAY_IDENTITY_CONFIG).await {
            Ok(Some(bytes)) => (
                String::from_utf8_lossy(&bytes).into_owned(),
                IdentitySource::Config,
            ),
            Ok(None) => {
                warn_relay_identity(ctx, "no relay identity is configured", now_ms).await;
                return None;
            }
            // A transient read failure is NOT the latched case: it can clear,
            // and silencing it behind the same latch would hide a real fault.
            Err(e) => {
                tracing::warn!("relay identity read failed: {e}");
                return None;
            }
        },
    };
    let Some(seckey) = crate::relay::crypto::from_hex32(hex.trim()) else {
        warn_relay_identity(ctx, "the configured relay identity is not 64-hex", now_ms).await;
        return None;
    };
    // Validated here rather than at the signing call, so a mis-seeded key is one
    // clear operator message instead of a signing failure per membership change.
    if crate::relay::crypto::schnorr::xonly_pubkey(&seckey).is_none() {
        warn_relay_identity(
            ctx,
            "the configured relay identity is not a valid secp256k1 secret",
            now_ms,
        )
        .await;
        return None;
    }
    Some((seckey, source))
}

/// How long a missing-identity warning stays quiet before repeating.
pub(super) const RELAY_IDENTITY_REWARN_MS: u64 = 15 * 60 * 1000;

/// Report a missing or unusable relay identity, naming what stops working —
/// at most once per [`RELAY_IDENTITY_REWARN_MS`] while the fault persists.
///
/// A process-lifetime `std::sync::Once` is the wrong latch for a fault that can
/// APPEAR mid-process: a fleet restart relaunches the app without its injected
/// secrets,
/// and a warning that already fired at some earlier boot would leave the loss
/// completely silent. A time-gated CAS re-warns every 15 minutes for as long
/// as the identity stays missing, and costs one atomic load on the happy path.
pub(super) async fn warn_relay_identity(ctx: &AppContext, why: &str, now_ms: u64) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST_WARN_MS: AtomicU64 = AtomicU64::new(0);
    let last = LAST_WARN_MS.load(Ordering::Relaxed);
    if last != 0 && now_ms.saturating_sub(last) < RELAY_IDENTITY_REWARN_MS {
        return;
    }
    // The CAS elects one warner per window; a raced loser simply stays quiet.
    // `max(1)` keeps a zero clock (the gates' frozen epoch) from re-arming.
    if LAST_WARN_MS
        .compare_exchange(last, now_ms.max(1), Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        tracing::warn!(
            "{why}: the NIP-29 group projections (kinds 39000/39001/39002) a client builds \
             its channel list and roster from will not be published. Seed 64-hex into the \
             {} secret or config key to enable them.",
            RELAY_IDENTITY_SECRET
        );
        // And raise it where an OPERATOR can see it. This fault is silent by
        // nature — chat keeps working while discovery quietly stops — so a log
        // line inside the process is not enough to notice it by.
        crate::relay::alerts::operational(
            ctx,
            "error",
            crate::relay::alerts::RELAY_IDENTITY,
            &format!(
                "{why}: NIP-29 group projections (39000/39001/39002) are not being \
                 published, so clients cannot build a channel list or a roster. Seed \
                 64-hex into the {RELAY_IDENTITY_SECRET} secret (or the matching config \
                 key) to restore them."
            ),
        )
        .await;
    }
}

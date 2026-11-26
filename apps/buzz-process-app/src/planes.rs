//! The platform layer: the only module that touches injected handles.
//!
//! Everything above this is pure and unit-tested; everything here is a thin
//! adapter over `ferrite-app`. The split is deliberate — a cell's DECISIONS
//! (which branch became a room, whether a member may land, whether a webhook
//! is authentic) are testable without a fleet, and this file is small enough
//! to read in one sitting.

use ferrite_app::serde::de::DeserializeOwned;
use ferrite_app::serde::Serialize;
use ferrite_app::{serde_json, AppContext, EgressHeader, EgressHttpMethod, EgressOutcome};

use crate::error::{ProcessError, Result};
use crate::rcs::Call;

/// The gateway's request queue for this cell (the GitHub cell's webhook face).
pub const REQUESTS: &str = "requests";
/// The gateway's reply queue.
pub const REPLIES: &str = "replies";
/// The cell's own durable state.
pub const STATE: &str = "state";
/// Egress to the platform control plane (native cell only).
pub const CONTROL_EGRESS: &str = "control-egress";
/// Egress to the workspace's own API.
pub const BUZZ_EGRESS: &str = "buzz-egress";

/// Largest reply this cell will hold in memory from any egress call.
pub const MAX_REPLY_BYTES: usize = 4 * 1024 * 1024;

/// The call's headers in the SDK's egress shape: upstream Core takes
/// `EgressHeader { name, value }` pairs and returns no response head.
fn wire(headers: &[(String, String)]) -> Vec<EgressHeader> {
    headers
        .iter()
        .map(|(name, value)| EgressHeader { name: name.clone(), value: value.clone() })
        .collect()
}

/// POST one built call through a scoped egress handle.
///
/// A DENIED outcome is the platform's destination scoping doing its job, and
/// it is reported as such rather than as a transport error: the difference is
/// "this deployment may not reach that host" versus "the host did not answer",
/// and only the first is an operator's problem to fix in the spec.
pub async fn deliver(ctx: &AppContext, handle: &str, call: &Call) -> Result<(u16, Vec<u8>)> {
    #[cfg(test)]
    if let Some(reply) = fake::deliver_as("POST", handle, call) {
        return reply;
    }
    let mut egress = ctx
        .egress(handle)
        .map_err(|e| ProcessError::Egress(format!("{handle} binding missing: {e}")))?;
    let outcome = egress
        .deliver_http(EgressHttpMethod::Post, &call.url, &wire(&call.headers), &call.body)
        .await
        .map_err(|e| ProcessError::Egress(format!("{handle} transport: {e}")))?;
    match outcome {
        EgressOutcome::Delivered { status, body } => {
            if body.len() > MAX_REPLY_BYTES {
                return Err(ProcessError::Egress(format!(
                    "{handle} reply is over {MAX_REPLY_BYTES} bytes"
                )));
            }
            Ok((status, body))
        }
        EgressOutcome::Denied { reason } => Err(ProcessError::Egress(format!(
            "{handle} destination denied by the platform: {reason}"
        ))),
    }
}

/// GET one URL through a scoped egress handle.
pub async fn fetch(ctx: &AppContext, handle: &str, call: &Call) -> Result<(u16, Vec<u8>)> {
    #[cfg(test)]
    if let Some(reply) = fake::deliver_as("GET", handle, call) {
        return reply;
    }
    let mut egress = ctx
        .egress(handle)
        .map_err(|e| ProcessError::Egress(format!("{handle} binding missing: {e}")))?;
    let outcome = egress
        .deliver_http(EgressHttpMethod::Get, &call.url, &wire(&call.headers), &[])
        .await
        .map_err(|e| ProcessError::Egress(format!("{handle} transport: {e}")))?;
    match outcome {
        EgressOutcome::Delivered { status, body } => {
            if body.len() > MAX_REPLY_BYTES {
                return Err(ProcessError::Egress(format!(
                    "{handle} reply is over {MAX_REPLY_BYTES} bytes"
                )));
            }
            Ok((status, body))
        }
        EgressOutcome::Denied { reason } => Err(ProcessError::Egress(format!(
            "{handle} destination denied by the platform: {reason}"
        ))),
    }
}

async fn read(ctx: &AppContext, key: &[u8]) -> Result<Option<Vec<u8>>> {
    #[cfg(test)]
    if let Some(found) = fake::read(key) {
        return Ok(found);
    }
    ctx.store(STATE)
        .map_err(|e| ProcessError::Storage(format!("state binding missing: {e}")))?
        .get(key)
        .await
        .map_err(|e| ProcessError::Storage(format!("state read: {e}")))
}

/// Write `value` at `key`, unconditionally or (`only_if_absent`) only while
/// the key holds nothing. `false` when that condition did not hold.
async fn write(ctx: &AppContext, key: &[u8], value: &[u8], only_if_absent: bool) -> Result<bool> {
    #[cfg(test)]
    if let Some(written) = fake::write(key, value, only_if_absent) {
        return Ok(written);
    }
    let mut store = ctx
        .store(STATE)
        .map_err(|e| ProcessError::Storage(format!("state binding missing: {e}")))?;
    let written = if only_if_absent {
        store.put_if_version(key, value, None).await.map(|r| r.is_ok())
    } else {
        store.put(key, value).await.map(|_| true)
    };
    written.map_err(|e| ProcessError::Storage(format!("state write: {e}")))
}

/// Read a durable document.
pub async fn load<T: DeserializeOwned>(ctx: &AppContext, key: &[u8]) -> Result<Option<T>> {
    match read(ctx, key).await? {
        None => Ok(None),
        Some(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| ProcessError::Storage(format!("state decode: {e}"))),
    }
}

pub(crate) async fn load_versioned<T: DeserializeOwned>(ctx: &AppContext, key: &[u8]) -> Result<Option<(T, u64)>> {
    #[cfg(test)]
    if let Some(found) = fake::read_versioned(key) {
        return found.map(|(bytes, version)| serde_json::from_slice(&bytes)
            .map(|value| (value, version)).map_err(|e| ProcessError::Storage(format!("state decode: {e}")))).transpose();
    }
    let found = ctx.store(STATE).map_err(|e| ProcessError::Storage(format!("state binding: {e}")))?
        .get_versioned(key).await.map_err(|e| ProcessError::Storage(format!("state read: {e}")))?;
    found.map(|(bytes, version)| serde_json::from_slice(&bytes)
        .map(|value| (value, version)).map_err(|e| ProcessError::Storage(format!("state decode: {e}")))).transpose()
}

pub(crate) async fn save_if_version<T: Serialize>(ctx: &AppContext, key: &[u8], value: &T, expected: Option<u64>) -> Result<bool> {
    let bytes = serde_json::to_vec(value).map_err(|e| ProcessError::Internal(format!("state encode: {e}")))?;
    #[cfg(test)]
    if let Some(written) = fake::write_versioned(key, &bytes, expected) {
        return Ok(written);
    }
    ctx.store(STATE).map_err(|e| ProcessError::Storage(format!("state binding: {e}")))?
        .put_if_version(key, &bytes, expected).await.map(|reply| reply.is_ok())
        .map_err(|e| ProcessError::Storage(format!("state CAS: {e}")))
}

/// Write a durable document.
pub async fn save<T: Serialize>(ctx: &AppContext, key: &[u8], value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)
        .map_err(|e| ProcessError::Internal(format!("state encode: {e}")))?;
    write(ctx, key, &bytes, false).await.map(|_| ())
}

/// Claim a key exactly once. `true` means THIS call created it.
///
/// The whole of webhook exactly-once processing: a delivery whose key already
/// exists has been seen, and a compare-and-swap against absence is what makes
/// that answer safe with several instances competing on one queue.
pub async fn claim_once(ctx: &AppContext, key: &[u8], value: &[u8]) -> Result<bool> {
    write(ctx, key, value, true).await
}

/// Read a claim's recorded value, if any.
pub async fn claimed(ctx: &AppContext, key: &[u8]) -> Result<Option<Vec<u8>>> {
    read(ctx, key).await
}

pub(crate) async fn poll_webhook(ctx: &AppContext, wait_ms: u64) -> std::io::Result<Option<ferrite_app::client::WireLease>> {
    #[cfg(test)]
    if let Some(reply) = fake::poll_webhook() {
        return Ok(reply);
    }
    ctx.queue(REQUESTS)?.poll_wait(wait_ms).await
}

/// Publish an ingress reply on the group the request named in `reply_to`.
///
/// The gateway names its OWN reply queue there (one per gateway instance when
/// it is scaled), not this app's `replies` binding, and it waits for the
/// answer only on that queue: a reply published anywhere else leaves the
/// caller to time out. The binding is the fallback for a request that names
/// no group.
pub(crate) async fn publish_webhook_reply(ctx: &AppContext, reply_to: &str, payload: &[u8]) -> std::io::Result<()> {
    #[cfg(test)]
    if let Some(reply) = fake::publish_reply((!reply_to.is_empty()).then_some(reply_to), payload) {
        return reply;
    }
    if reply_to.is_empty() {
        return ctx.queue(REPLIES)?.publish(payload).await.map(|_| ());
    }
    ctx.reply_to_queue(reply_to).await?.publish(payload).await.map(|_| ())
}

pub(crate) async fn ack_webhook(ctx: &AppContext, token: u64) -> std::io::Result<()> {
    #[cfg(test)]
    if let Some(acked) = fake::ack_webhook(token) {
        return if acked { Ok(()) } else { Err(std::io::Error::other("webhook lease was lost")) };
    }
    if ctx.queue(REQUESTS)?.ack(token).await? {
        Ok(())
    } else {
        Err(std::io::Error::other("webhook lease was lost"))
    }
}

/// The platform a unit test runs a cell against, per test thread: an
/// in-memory store and a scripted egress. A test that installs nothing
/// reaches the real handles.
#[cfg(test)]
pub(crate) mod fake {
    use std::collections::BTreeMap;
    use std::sync::{Mutex, MutexGuard};

    use ferrite_app::rt::{current_thread, ThreadIdentity};

    use crate::error::Result;
    use crate::rcs::Call;

    type Egress = Box<dyn FnMut(&str, &Call) -> Result<(u16, Vec<u8>)> + Send>;

    struct Platform {
        store: BTreeMap<Vec<u8>, Vec<u8>>,
        versions: BTreeMap<Vec<u8>, u64>,
        egress: Option<Egress>,
        requests: Vec<ferrite_app::client::WireLease>,
        replies: Vec<Vec<u8>>,
        /// The group each reply went to; `None` is the `replies` binding.
        reply_groups: Vec<Option<String>>,
        acked: Vec<u64>,
        failed_replies: usize,
        /// The HTTP method of every egress call, in order.
        methods: Vec<&'static str>,
    }

    // One platform per test thread, in a process global keyed by the thread:
    // the platform's code gate refuses `thread_local!`, test modules included.
    static PLATFORMS: Mutex<Vec<(ThreadIdentity, Platform)>> = Mutex::new(Vec::new());

    fn platforms() -> MutexGuard<'static, Vec<(ThreadIdentity, Platform)>> {
        PLATFORMS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn take_mine(platforms: &mut Vec<(ThreadIdentity, Platform)>) -> Option<Platform> {
        let me = current_thread();
        let at = platforms.iter().position(|(owner, _)| *owner == me)?;
        Some(platforms.swap_remove(at).1)
    }

    fn with_mine<R>(f: impl FnOnce(&mut Platform) -> R) -> Option<R> {
        let me = current_thread();
        platforms()
            .iter_mut()
            .find(|(owner, _)| *owner == me)
            .map(|(_, platform)| f(platform))
    }

    /// Install an empty store and `egress` as this thread's platform until
    /// the returned guard drops, which restores what was installed before.
    #[must_use]
    pub(crate) fn install(
        egress: impl FnMut(&str, &Call) -> Result<(u16, Vec<u8>)> + Send + 'static,
    ) -> Installed {
        let mut platforms = platforms();
        let previous = take_mine(&mut platforms);
        platforms.push((
            current_thread(),
            Platform {
                store: BTreeMap::new(),
                versions: BTreeMap::new(),
                egress: Some(Box::new(egress)),
                requests: Vec::new(),
                replies: Vec::new(),
                reply_groups: Vec::new(),
                acked: Vec::new(),
                failed_replies: 0,
                methods: Vec::new(),
            },
        ));
        Installed { previous, _thread_bound: std::marker::PhantomData }
    }

    pub(crate) struct Installed {
        previous: Option<Platform>,
        _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
    }

    impl Drop for Installed {
        fn drop(&mut self) {
            let mut platforms = platforms();
            take_mine(&mut platforms);
            if let Some(previous) = self.previous.take() {
                platforms.push((current_thread(), previous));
            }
        }
    }

    pub(super) fn read(key: &[u8]) -> Option<Option<Vec<u8>>> {
        with_mine(|p| p.store.get(key).cloned())
    }

    pub(super) fn write(key: &[u8], value: &[u8], only_if_absent: bool) -> Option<bool> {
        with_mine(|p| {
            if only_if_absent && p.store.contains_key(key) {
                return false;
            }
            p.store.insert(key.to_vec(), value.to_vec());
            *p.versions.entry(key.to_vec()).or_default() += 1;
            true
        })
    }

    pub(super) fn read_versioned(key: &[u8]) -> Option<Option<(Vec<u8>, u64)>> {
        with_mine(|p| p.store.get(key).map(|bytes| (bytes.clone(), p.versions[key])))
    }

    pub(super) fn write_versioned(key: &[u8], value: &[u8], expected: Option<u64>) -> Option<bool> {
        with_mine(|p| {
            if p.versions.get(key).copied() != expected {
                return false;
            }
            p.store.insert(key.to_vec(), value.to_vec());
            *p.versions.entry(key.to_vec()).or_default() += 1;
            true
        })
    }

    pub(crate) fn seed_webhook(payload: Vec<u8>, failed_replies: usize) {
        with_mine(|p| {
            p.requests.push(ferrite_app::client::WireLease { id: 1, token: 0, payload, attempt: 0 });
            p.failed_replies = failed_replies;
        }).expect("installed platform");
    }

    pub(crate) fn queue_snapshot() -> (usize, Vec<Vec<u8>>, Vec<u64>) {
        with_mine(|p| (p.requests.len(), p.replies.clone(), p.acked.clone())).expect("installed platform")
    }

    pub(super) fn poll_webhook() -> Option<Option<ferrite_app::client::WireLease>> {
        with_mine(|p| p.requests.first_mut().map(|lease| {
            lease.token += 1;
            lease.attempt += 1;
            lease.clone()
        }))
    }

    pub(crate) fn reply_groups() -> Vec<Option<String>> {
        with_mine(|p| p.reply_groups.clone()).expect("installed platform")
    }

    pub(super) fn publish_reply(group: Option<&str>, payload: &[u8]) -> Option<std::io::Result<()>> {
        with_mine(|p| {
            if p.failed_replies > 0 {
                p.failed_replies -= 1;
                return Err(std::io::Error::other("reply queue unavailable"));
            }
            p.replies.push(payload.to_vec());
            p.reply_groups.push(group.map(str::to_string));
            Ok(())
        })
    }

    pub(super) fn ack_webhook(token: u64) -> Option<bool> {
        with_mine(|p| {
            if let Some(at) = p.requests.iter().position(|lease| lease.token == token) {
                p.requests.remove(at);
                p.acked.push(token);
                true
            } else {
                false
            }
        })
    }

    /// The HTTP methods this thread's egress calls used, in order.
    pub(crate) fn methods() -> Vec<&'static str> {
        with_mine(|p| p.methods.clone()).unwrap_or_default()
    }

    pub(super) fn deliver_as(method: &'static str, handle: &str, call: &Call) -> Option<Result<(u16, Vec<u8>)>> {
        with_mine(|p| p.methods.push(method));
        deliver(handle, call)
    }

    pub(super) fn deliver(handle: &str, call: &Call) -> Option<Result<(u16, Vec<u8>)>> {
        // Taken out while it runs, so the script may itself read the store.
        let mut egress = with_mine(|p| p.egress.take()).flatten()?;
        let reply = egress(handle, call);
        with_mine(|p| p.egress = Some(egress));
        Some(reply)
    }
}

/// A required secret, or a refusal naming it.
pub fn secret(ctx: &AppContext, name: &str) -> Result<String> {
    ctx.secret(name)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ProcessError::Internal(format!("the {name} secret is not configured")))
}

/// A 32-byte key from a 64-hex secret.
pub fn secret_key(ctx: &AppContext, name: &str) -> Result<[u8; 32]> {
    let hex = secret(ctx, name)?;
    crate::crypto::from_hex32(&hex)
        .ok_or_else(|| ProcessError::Internal(format!("the {name} secret is not 64 hex characters")))
}

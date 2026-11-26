//! The shell's state: one workspace, one credential store, one broker, one
//! supervisor.
//!
//! Held in Tauri's managed state so every command sees the same instances. The
//! harness list is persisted next to the app's data as ordinary JSON — it holds
//! configuration and SECRET REFERENCES, never secrets, which is what makes a
//! plain file the right home for it.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use buzz_desktop_core::broker::Broker;
use buzz_desktop_core::credentials::{Key, Store};
use buzz_desktop_core::git::Git;
use buzz_desktop_core::harness::Harness;
use buzz_desktop_core::nostr::NonceSource;
use buzz_desktop_core::paths;
use buzz_desktop_core::project::Workspace;
use buzz_desktop_core::remote::RemoteConnection;
use buzz_desktop_core::{Error, Result};

use crate::keychain::Keychain;
use crate::supervisor::Supervisor;

pub struct AppState {
    pub workspace: Workspace,
    pub keychain: Keychain,
    pub broker: Mutex<Broker>,
    pub supervisor: Supervisor,
    /// Connected remote agents, by harness id. Separate from the supervisor's
    /// table because a remote harness has a connection and a session, not a child.
    pub remotes: Arc<Mutex<BTreeMap<String, RemoteConnection>>>,
    /// Harnesses whose remote connection should be removed once their in-flight
    /// prompt turn finishes (`remote::disconnect` called during an active turn).
    pub disconnecting: Arc<Mutex<BTreeSet<String>>>,
    /// Monotonic per-harness connection lifecycle generation. Taken before
    /// `remote::connect` begins its HTTP handshake and advanced by `connect`,
    /// `disconnect`, and harness reconfiguration/removal so a stale in-flight
    /// setup cannot undo a concurrent disconnect or overwrite a newer connect.
    pub remote_generations: Arc<Mutex<BTreeMap<String, u64>>>,
    pub harnesses: Mutex<Vec<Harness>>,
    pub flags: Mutex<Vec<HarnessFlags>>,
    /// Channel messages already handed to a harness, so a redelivery does not
    /// prompt an agent twice for the same message.
    pub delivered: Mutex<BTreeSet<String>>,
    /// Insertion order for [`Self::delivered`], so capacity eviction removes the
    /// oldest delivered marker rather than the lexicographically smallest one.
    delivered_order: Mutex<VecDeque<String>>,
    pub data_dir: PathBuf,
    /// Harnesses with a remote prompt in flight, so a second cannot interleave
    /// with it. Held in an `Arc` so the driver's `Drop` guard needs no Tauri borrow.
    pub prompting: Arc<Mutex<BTreeSet<String>>>,
    /// Per-request nonces for NIP-98 headers; one source per process prevents
    /// replay rejections.
    pub nonces: NonceSource,
    config_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HarnessFlags {
    pub id: String,
    #[serde(default)]
    pub auto_restart: bool,
    #[serde(default)]
    pub start_on_app_launch: bool,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct StoredConfig {
    #[serde(default)]
    harnesses: Vec<Harness>,
    #[serde(default)]
    flags: Vec<HarnessFlags>,
}

impl AppState {
    /// Open the shell's state.
    ///
    /// `home` decides where checkouts live (`~/.buzz/repos`) and `data_dir` where
    /// this app's own files go. Both are supplied rather than discovered so the
    /// caller — which has Tauri's path resolver — owns that decision.
    pub fn open(home: &Path, data_dir: &Path) -> Result<Self> {
        let repos_dir = paths::default_repos_dir(home);
        // Created here rather than lazily, so a first run has somewhere to clone
        // into and a failure is reported at startup instead of mid-clone.
        std::fs::create_dir_all(&repos_dir).map_err(|e| {
            Error::Internal(format!(
                "could not create {}: {e}",
                repos_dir.display()
            ))
        })?;
        std::fs::create_dir_all(data_dir).map_err(|e| {
            Error::Internal(format!("could not create {}: {e}", data_dir.display()))
        })?;
        let config_path = data_dir.join("harnesses.json");
        let stored = load_config(&config_path);
        Ok(Self {
            workspace: Workspace::new(Git::new(), &repos_dir)?,
            keychain: Keychain::open(data_dir),
            broker: Mutex::new(Broker::new()),
            supervisor: Supervisor::new(),
            remotes: Arc::new(Mutex::new(BTreeMap::new())),
            disconnecting: Arc::new(Mutex::new(BTreeSet::new())),
            remote_generations: Arc::new(Mutex::new(BTreeMap::new())),
            harnesses: Mutex::new(stored.harnesses),
            flags: Mutex::new(stored.flags),
            delivered: Mutex::new(BTreeSet::new()),
            delivered_order: Mutex::new(VecDeque::new()),
            data_dir: data_dir.to_path_buf(),
            prompting: Arc::new(Mutex::new(BTreeSet::new())),
            // Seeded with the boot instant, so two runs of this app never build
            // the same authorization event.
            nonces: NonceSource::new(&format!("buzz-desktop/{}", now_ms())),
            config_path,
        })
    }

    pub fn harness(&self, id: &str) -> Result<Harness> {
        self.harnesses
            .lock()
            .map_err(|_| Error::Internal("the harness list is poisoned".into()))?
            .iter()
            .find(|harness| harness.id() == id)
            .cloned()
            .ok_or_else(|| Error::Invalid(format!("no harness is configured with id {id:?}")))
    }

    pub fn upsert_harness(&self, harness: Harness) -> Result<()> {
        harness.validate()?;
        let id = harness.id().to_string();
        {
            let mut harnesses = self
                .harnesses
                .lock()
                .map_err(|_| Error::Internal("the harness list is poisoned".into()))?;
            match harnesses
                .iter_mut()
                .find(|existing| existing.id() == harness.id())
            {
                Some(existing) => *existing = harness,
                None => harnesses.push(harness),
            }
            self.disconnect_remote(&id);
        }
        self.save()
    }

    /// Forget a harness. Stopping it first is the caller's business — which is
    /// why `delete_managed_agent` does exactly that.
    pub fn remove_harness(&self, id: &str) -> Result<bool> {
        let removed = {
            let mut harnesses = self
                .harnesses
                .lock()
                .map_err(|_| Error::Internal("the harness list is poisoned".into()))?;
            let before = harnesses.len();
            harnesses.retain(|harness| harness.id() != id);
            let removed = harnesses.len() != before;
            if removed {
                self.disconnect_remote(id);
            }
            removed
        };
        if removed {
            let mut flags = self
                .flags
                .lock()
                .map_err(|_| Error::Internal("the harness flags are poisoned".into()))?;
            flags.retain(|flag| flag.id != id);
            drop(flags);
            self.save()?;
            // The agent's signing key goes with it: a deleted harness must not
            // leave a usable private key in the OS keychain and its index
            // (finding_fda42dfb7f4c0aca). Best effort, after the config write.
            let _ = self.keychain.delete(&Key::agent(id));
        }
        Ok(removed)
    }

    /// Reserve the next connection generation for a configured `harness` before
    /// starting a remote handshake, verifying under the `harnesses` lock that the
    /// harness configuration is still current.
    pub fn begin_remote_connect_for(
        &self,
        harness: &Harness,
    ) -> std::result::Result<u64, String> {
        let harnesses = self
            .harnesses
            .lock()
            .map_err(|_| "the harness list is unavailable".to_string())?;
        if !harnesses.iter().any(|existing| existing == harness) {
            return Err(format!(
                "{} was removed or reconfigured before connecting",
                harness.label()
            ));
        }
        self.begin_remote_connect(harness.id(), harness.label())
    }

    /// Reserve the next connection generation for `id` before starting a remote
    /// handshake. Refuses if a prompt turn is currently in flight.
    pub fn begin_remote_connect(
        &self,
        id: &str,
        label: &str,
    ) -> std::result::Result<u64, String> {
        let prompting = self
            .prompting
            .lock()
            .map_err(|_| "the in-flight prompt table is unavailable".to_string())?;
        if prompting.contains(id) {
            return Err(format!(
                "{label} is still answering a message; wait for the turn to finish before reconnecting"
            ));
        }
        let mut generations = self
            .remote_generations
            .lock()
            .map_err(|_| "the remote connection generation table is unavailable".to_string())?;
        let gen = generations.entry(id.to_string()).or_insert(0);
        *gen = gen.saturating_add(1);
        Ok(*gen)
    }

    /// Install a completed remote connection for a configured `harness`,
    /// holding the `harnesses` lock so a concurrent `upsert_harness` or
    /// `remove_harness` cannot interleave between configuration check and
    /// session installation.
    pub fn finish_remote_connect_for(
        &self,
        harness: &Harness,
        generation: u64,
        connection: RemoteConnection,
    ) -> std::result::Result<(), String> {
        let harnesses = self
            .harnesses
            .lock()
            .map_err(|_| "the harness list is unavailable".to_string())?;
        if !harnesses.iter().any(|existing| existing == harness) {
            return Err(format!(
                "{} was removed or reconfigured while connecting; refusing to install stale session",
                harness.label()
            ));
        }
        self.finish_remote_connect(harness.id(), harness.label(), generation, connection)
    }

    /// Install a completed remote connection only if no prompt started and its
    /// lifecycle `generation` has not been superseded by `disconnect`, another
    /// `connect`, or harness reconfiguration/removal.
    pub fn finish_remote_connect(
        &self,
        id: &str,
        label: &str,
        generation: u64,
        connection: RemoteConnection,
    ) -> std::result::Result<(), String> {
        let prompting = self
            .prompting
            .lock()
            .map_err(|_| "the in-flight prompt table is unavailable".to_string())?;
        if prompting.contains(id) {
            return Err(format!(
                "{label} started answering a message while connecting; refusing to replace the active session"
            ));
        }
        let generations = self
            .remote_generations
            .lock()
            .map_err(|_| "the remote connection generation table is unavailable".to_string())?;
        if generations.get(id).copied() != Some(generation) {
            return Err(format!(
                "{label} was disconnected or superseded while connecting; refusing to install stale session"
            ));
        }
        if let Ok(mut disconnecting) = self.disconnecting.lock() {
            disconnecting.remove(id);
        }
        self.remotes
            .lock()
            .map_err(|_| "the remote connection table is unavailable".to_string())?
            .insert(id.to_string(), connection);
        Ok(())
    }

    /// Advance the remote lifecycle generation for `id` so any in-flight
    /// `connect` started earlier cannot install its connection.
    pub fn invalidate_remote_generation(&self, id: &str) {
        if let Ok(mut generations) = self.remote_generations.lock() {
            let gen = generations.entry(id.to_string()).or_insert(0);
            *gen = gen.saturating_add(1);
        }
    }

    /// Forget a remote connection (or defer removal until the active prompt turn
    /// finishes) and advance its lifecycle generation so an in-flight `connect`
    /// cannot undo the disconnect.
    pub fn disconnect_remote(&self, id: &str) {
        let Ok(prompting) = self.prompting.lock() else {
            return;
        };
        self.invalidate_remote_generation(id);
        if prompting.contains(id) {
            if let Ok(mut disconnecting) = self.disconnecting.lock() {
                disconnecting.insert(id.to_string());
            }
            return;
        }
        if let Ok(mut disconnecting) = self.disconnecting.lock() {
            disconnecting.remove(id);
        }
        if let Ok(mut remotes) = self.remotes.lock() {
            remotes.remove(id);
        }
    }

    pub fn flags_for(&self, id: &str) -> HarnessFlags {
        self.flags
            .lock()
            .ok()
            .and_then(|flags| flags.iter().find(|flag| flag.id == id).cloned())
            .unwrap_or(HarnessFlags {
                id: id.to_string(),
                auto_restart: false,
                start_on_app_launch: false,
            })
    }

    pub fn set_flag(&self, id: &str, auto_restart: Option<bool>, on_launch: Option<bool>) -> Result<HarnessFlags> {
        let updated = {
            let mut flags = self
                .flags
                .lock()
                .map_err(|_| Error::Internal("the harness flags are poisoned".into()))?;
            let entry = match flags.iter_mut().find(|flag| flag.id == id) {
                Some(entry) => entry,
                None => {
                    flags.push(HarnessFlags {
                        id: id.to_string(),
                        auto_restart: false,
                        start_on_app_launch: false,
                    });
                    flags.last_mut().expect("just pushed")
                }
            };
            if let Some(value) = auto_restart {
                entry.auto_restart = value;
            }
            if let Some(value) = on_launch {
                entry.start_on_app_launch = value;
            }
            entry.clone()
        };
        self.save()?;
        Ok(updated)
    }

    pub fn already_delivered(&self, marker: &str) -> bool {
        self.delivered
            .lock()
            .map(|seen| seen.contains(marker))
            .unwrap_or(false)
    }

    /// Record a delivery. Bounded in FIFO insertion order so capacity eviction
    /// drops the oldest marker rather than the lexicographically smallest one.
    pub fn mark_delivered(&self, marker: &str) {
        if let (Ok(mut seen), Ok(mut order)) = (self.delivered.lock(), self.delivered_order.lock())
        {
            if !seen.insert(marker.to_string()) {
                return;
            }
            order.push_back(marker.to_string());
            while seen.len() > 4_096 {
                if let Some(oldest) = order.pop_front() {
                    seen.remove(&oldest);
                } else if let Some(first) = seen.iter().next().cloned() {
                    seen.remove(&first);
                } else {
                    break;
                }
            }
        }
    }

    /// When the harness configuration was last written, in epoch milliseconds.
    ///
    /// The client's managed-agent row carries created/updated timestamps and this
    /// shell records none per harness — so the config file's own mtime is the
    /// true fact it has. Better than an instant taken per read, which would make
    /// a panel say "updated just now" forever.
    pub fn config_modified_ms(&self) -> u64 {
        std::fs::metadata(&self.config_path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or_else(now_ms)
    }

    fn save(&self) -> Result<()> {
        // One save at a time, each snapshotting under the lock: the last save
        // to run then writes the newest state, never an older snapshot that
        // finished later.
        let _saving = CONFIG_SAVE_LOCK
            .lock()
            .map_err(|_| Error::Internal("the harness config writer is poisoned".into()))?;
        let harnesses = self
            .harnesses
            .lock()
            .map_err(|_| Error::Internal("the harness list is poisoned".into()))?
            .clone();
        let flags = self
            .flags
            .lock()
            .map_err(|_| Error::Internal("the harness flags are poisoned".into()))?
            .clone();
        let text = serde_json::to_string_pretty(&StoredConfig { harnesses, flags })
            .map_err(|e| Error::Internal(format!("could not encode the harness config: {e}")))?;
        write_atomically(&self.config_path, text.as_bytes()).map_err(|e| {
            Error::Internal(format!(
                "could not write {}: {e}",
                self.config_path.display()
            ))
        })
    }
}

/// Serializes [`AppState::save`].
static CONFIG_SAVE_LOCK: Mutex<()> = Mutex::new(());

/// Write `bytes` to `path` so that a reader, or the next launch after a crash,
/// sees the old file or the new one and never a truncated mix: written to a
/// sibling, synced, then renamed over it.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut staged_name = path.file_name().map(|name| name.to_os_string()).unwrap_or_default();
    staged_name.push(".tmp");
    let staged = path.with_file_name(staged_name);
    {
        let mut file = std::fs::File::create(&staged)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&staged, path)
}

/// Read the stored config, tolerating its absence and refusing to guess at a
/// broken one.
///
/// A malformed file yields the DEFAULT rather than an error, and that is a
/// deliberate trade: refusing to start because a harness list will not parse
/// would lock somebody out of their chat client over an agent setting. The
/// harnesses are then absent and visibly re-addable.
fn load_config(path: &Path) -> StoredConfig {
    let Ok(text) = std::fs::read_to_string(path) else {
        return StoredConfig::default();
    };
    match serde_json::from_str(&text) {
        Ok(stored) => stored,
        Err(e) => {
            eprintln!(
                "buzz-desktop: {} does not parse ({e}); starting with no configured harnesses",
                path.display()
            );
            StoredConfig::default()
        }
    }
}

#[must_use]
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivered_markers_evict_in_fifo_order_not_lexicographic_order() {
        let root = std::env::temp_dir().join(format!("buzz-desktop-state-test-{}", now_ms()));
        let home = root.join("home");
        let data = root.join("data");
        let state = AppState::open(&home, &data).expect("open state");

        // Insert a lexicographically small marker AFTER 4,095 larger ones, then
        // push two more markers past the 4,096 cap. FIFO eviction must drop the
        // earliest inserted markers (`m-0000`, `m-0001`), keeping the newer `0000-recent`.
        for i in 0..4_095 {
            state.mark_delivered(&format!("m-{i:04}"));
        }
        state.mark_delivered("0000-recent");
        assert!(state.already_delivered("0000-recent"));
        assert!(state.already_delivered("m-0000"));

        state.mark_delivered("z-4096");
        state.mark_delivered("z-4097");
        assert!(
            state.already_delivered("0000-recent"),
            "recently delivered lexicographically-small marker must not be evicted first"
        );
        assert!(
            !state.already_delivered("m-0000"),
            "oldest delivered marker should be evicted when capacity is exceeded"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remote_connect_generation_rejects_stale_connect_after_disconnect_or_overlap() {
        let root = std::env::temp_dir().join(format!("buzz-desktop-gen-test-{}", now_ms()));
        let home = root.join("home");
        let data = root.join("data");
        let state = AppState::open(&home, &data).expect("open state");

        // 1. A disconnect during an in-flight connect advances the generation and
        // prevents the stale connect from installing its session.
        let gen1 = state
            .begin_remote_connect("h1", "Agent 1")
            .expect("begin gen1");
        state.disconnect_remote("h1");
        let conn1 = RemoteConnection::new("https://example.com/acp").expect("conn1");
        let err = state
            .finish_remote_connect("h1", "Agent 1", gen1, conn1)
            .expect_err("disconnect during setup must reject stale connect");
        assert!(err.contains("disconnected or superseded"));
        assert!(!state.remotes.lock().unwrap().contains_key("h1"));

        // 2. Overlapping connects: second connect advances generation and succeeds;
        // first connect finishing later is rejected and cannot overwrite the second.
        let gen_a = state
            .begin_remote_connect("h1", "Agent 1")
            .expect("begin gen_a");
        let gen_b = state
            .begin_remote_connect("h1", "Agent 1")
            .expect("begin gen_b");
        assert!(gen_b > gen_a);
        let conn_b = RemoteConnection::new("https://example.com/acp").expect("conn_b");
        state
            .finish_remote_connect("h1", "Agent 1", gen_b, conn_b)
            .expect("newer connect installs cleanly");
        assert!(state.remotes.lock().unwrap().contains_key("h1"));

        let conn_a = RemoteConnection::new("https://example.com/acp").expect("conn_a");
        let err_a = state
            .finish_remote_connect("h1", "Agent 1", gen_a, conn_a)
            .expect_err("older overlapping connect must be rejected");
        assert!(err_a.contains("disconnected or superseded"));

        let _ = std::fs::remove_dir_all(&root);
    }
}

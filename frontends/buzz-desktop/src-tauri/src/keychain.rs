//! The OS keychain, and what happens where there is not one.
//!
//! `keyring` gives real durability on macOS, Windows and any Linux with a
//! Secret Service (GNOME Keyring, KWallet). A machine WITHOUT one — a headless
//! box, a container, a session with no unlocked keyring — is a real case, and the
//! interesting decision is what to do about it:
//!
//! * secrets fall back to memory, so the app still works for the session;
//! * and it SAYS SO — [`Keychain::is_durable`] is reported through
//!   `desktop_capabilities`, so a settings screen can tell the user their token
//!   will not survive a restart rather than letting them discover it.
//!
//! Silently pretending to persist is the one behaviour ruled out: a credential
//! that vanishes without warning reads as the app having lost it.
//!
//! ## Why there is an index file
//!
//! A keychain can be asked "what is the secret for this account" but not "what
//! accounts do you hold for me" — there is no portable enumeration API. Listing
//! which hosts have a credential is a real need (a settings screen), so the
//! ACCOUNT NAMES are kept in a small JSON file beside the app's data. It holds
//! names only, never secrets, which is why it can be an ordinary file.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use buzz_desktop_core::credentials::{Key, MemoryStore, Store, SERVICE};
use buzz_desktop_core::{Error, Result};

pub struct Keychain {
    /// Injectable so tests use their own namespace rather than overwriting a
    /// developer's real credentials in the production service.
    service: String,
    index_path: PathBuf,
    index: Mutex<BTreeSet<String>>,
    durable: bool,
    fallback: MemoryStore,
}

impl Keychain {
    /// Open the store, probing the platform keychain once with a real round trip
    /// because a Linux box may have the library with no running Secret Service.
    pub fn open(data_dir: &Path) -> Self {
        Self::open_with_service(data_dir, SERVICE)
    }

    pub fn open_with_service(data_dir: &Path, service: &str) -> Self {
        let index_path = data_dir.join("credential-index.json");
        let durable = probe(service);
        // When the platform cannot keep a secret, the index is SESSION-ONLY: a
        // name persisted from an earlier (durable) run would otherwise be
        // reported by `subjects()` while `get()` could not return its secret,
        // which reads as the app having lost a credential it never had here.
        let index = if durable {
            load_index(&index_path)
        } else {
            BTreeSet::new()
        };
        Self {
            service: service.to_string(),
            index_path,
            index: Mutex::new(index),
            durable,
            fallback: MemoryStore::new(),
        }
    }

    #[must_use]
    pub const fn is_durable(&self) -> bool {
        self.durable
    }

    fn entry(&self, key: &Key) -> Result<keyring::Entry> {
        keyring::Entry::new(&self.service, &key.account()).map_err(|e| {
            Error::Internal(format!("the OS keychain refused an entry handle: {e}"))
        })
    }

    fn update_index(&self, mutate: impl FnOnce(&mut BTreeSet<String>)) {
        if let Ok(mut index) = self.index.lock() {
            mutate(&mut index);
            // Only a durable store writes the index; see `open_with_service`.
            if self.durable {
                save_index(&self.index_path, &index);
            }
        }
    }

    fn remember(&self, key: &Key) {
        self.update_index(|index| {
            index.insert(key.account());
        });
    }

    fn forget(&self, key: &Key) {
        self.update_index(|index| {
            index.remove(&key.account());
        });
    }
}

impl Store for Keychain {
    fn get(&self, key: &Key) -> Result<Option<String>> {
        if !self.durable {
            return self.fallback.get(key);
        }
        match self.entry(key)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            // A LOCKED keychain is not an absent secret: reporting it as one
            // would make the app quietly ask for a credential the user has
            // already given it.
            Err(e) => Err(Error::Internal(format!("the OS keychain refused a read: {e}"))),
        }
    }

    fn set(&self, key: &Key, secret: &str) -> Result<()> {
        if secret.is_empty() {
            return Err(Error::Invalid("an empty secret is not a credential".into()));
        }
        if !self.durable {
            self.fallback.set(key, secret)?;
            self.remember(key);
            return Ok(());
        }
        self.entry(key)?
            .set_password(secret)
            .map_err(|e| Error::Internal(format!("the OS keychain refused a write: {e}")))?;
        self.remember(key);
        Ok(())
    }

    fn delete(&self, key: &Key) -> Result<()> {
        if self.durable {
            match self.entry(key)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => {}
                Err(e) => {
                    return Err(Error::Internal(format!(
                        "the OS keychain refused a delete: {e}"
                    )))
                }
            }
        } else {
            self.fallback.delete(key)?;
        }
        self.forget(key);
        Ok(())
    }

    fn subjects(&self, kind: &str) -> Result<Vec<String>> {
        let prefix = format!("{kind}:");
        let index = self
            .index
            .lock()
            .map_err(|_| Error::Internal("the credential index is poisoned".into()))?;
        Ok(index
            .iter()
            .filter_map(|account| account.strip_prefix(&prefix).map(str::to_string))
            .collect())
    }
}

/// Can this machine keep a secret?
///
/// Written and read back under a reserved account, then deleted. A probe that
/// only constructed an `Entry` would succeed on a machine with no running Secret
/// Service, which is exactly the case this needs to detect.
fn probe(service: &str) -> bool {
    let Ok(entry) = keyring::Entry::new(service, "probe:durability") else {
        return false;
    };
    if entry.set_password("probe").is_err() {
        return false;
    }
    let readable = matches!(entry.get_password().as_deref(), Ok("probe"));
    let _ = entry.delete_credential();
    readable
}

fn load_index(path: &Path) -> BTreeSet<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeSet::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// Best effort: the index is a convenience for listing, and losing it costs a
/// settings screen its list — never a secret, which lives in the keychain.
fn save_index(path: &Path, index: &BTreeSet<String>) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(index) {
        let _ = std::fs::write(path, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A keychain namespace of this test's own.
    ///
    /// Never [`SERVICE`]: on a developer's machine that service holds their real
    /// git and harness credentials, and a test writing `git:github.com` there
    /// would overwrite — then delete — the token they use every day. The account
    /// names are production-shaped on purpose (that is what is being tested);
    /// only the SERVICE is namespaced, which is what keeps them apart.
    fn scratch(label: &str) -> (PathBuf, Keychain) {
        let dir = std::env::temp_dir().join(format!("buzz-{label}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let service = format!(
            "buzz-desktop-test-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        );
        let keychain = Keychain::open_with_service(&dir, &service);
        (dir, keychain)
    }

    #[test]
    fn the_index_holds_names_and_never_secrets() {
        let (dir, keychain) = scratch("index");
        let key = Key::git("github.com");
        // Whether the platform keychain works here or not, the API answers.
        keychain.set(&key, "ghp_secret").expect("set");
        assert_eq!(keychain.subjects("git").expect("list"), vec!["github.com"]);
        if keychain.is_durable() {
            let index_text =
                std::fs::read_to_string(dir.join("credential-index.json")).expect("index file");
            assert!(index_text.contains("git:github.com"));
            assert!(
                !index_text.contains("ghp_secret"),
                "the index must never contain a secret: {index_text}"
            );
        } else {
            // Session-only: nothing is written, so a later run cannot claim a
            // credential it does not hold.
            assert!(
                !dir.join("credential-index.json").exists(),
                "a non-durable store must not persist account names"
            );
        }

        keychain.delete(&key).expect("delete");
        assert!(keychain.subjects("git").expect("list").is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_secret_is_readable_within_the_session_whatever_the_platform_keeps() {
        let (dir, keychain) = scratch("durable");
        let key = Key::harness("claude");
        keychain.set(&key, "sk-test").expect("set");
        assert_eq!(
            keychain.get(&key).expect("get").as_deref(),
            Some("sk-test"),
            "a secret is readable within the session either way"
        );
        keychain.delete(&key).expect("delete");
        assert_eq!(keychain.get(&key).expect("get"), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_stale_index_is_not_reloaded_when_the_platform_cannot_keep_secrets() {
        // The failure this prevents: an earlier durable run recorded names, the
        // keychain is now unavailable, and `subjects()` would report credentials
        // `get()` cannot return — which reads as the app losing them.
        let (dir, keychain) = scratch("stale");
        std::fs::write(
            dir.join("credential-index.json"),
            "[\"git:github.com\",\"harness:claude\"]",
        )
        .expect("seed index");
        let reopened = Keychain::open_with_service(&dir, "buzz-desktop-test-definitely-absent-service");
        if reopened.is_durable() {
            // This machine has a keychain, so the index is authoritative again.
            assert_eq!(reopened.subjects("git").expect("list"), vec!["github.com"]);
        } else {
            assert!(
                reopened.subjects("git").expect("list").is_empty(),
                "a session-only store must not claim last run's credentials"
            );
        }
        drop(keychain);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_empty_secret_is_refused_before_it_reaches_the_platform() {
        let (dir, keychain) = scratch("empty");
        assert!(keychain.set(&Key::git("github.com"), "").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}

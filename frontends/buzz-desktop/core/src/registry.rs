//! Contract C: which commands the shell owns.
//!
//! The registry is the shell's half of the hybrid dispatch. The shell
//! injects `window.__BUZZ_DESKTOP__ = { version, commands }` BEFORE any bundle
//! code runs, and the SPA's bridge routes exactly those commands to native IPC;
//! everything else stays on the browser seam. Because the marker is injected
//! rather than handshaked, an `isTauri()` call during first render sees the
//! truth — which an async handshake could not promise.
//!
//! Three rules hold this list honest, and each is enforced by a test rather
//! than by care:
//!
//! 1. **One owner per command.** Nothing here may also be answered by the
//!    browser seam (`commandCoverage.test.mjs` proves it both ways). A command
//!    answered twice is a command whose behaviour depends on which build you
//!    are running, which is the bug this whole contract exists to prevent.
//! 2. **Nothing invented.** Every entry is a command the client genuinely
//!    invokes. A registry entry the client never calls is a black hole: the
//!    bridge would route to native for something that never happens.
//! 3. **The document matches the code.** `native-commands.json` is checked in
//!    for the JS gate to read, and a test asserts it equals this compiled list.
//!
//! What is deliberately ABSENT is as load-bearing as what is here: huddle
//! audio, mesh-LLM hosting and device pairing stay refused by the seam, with
//! the seam's own sentences. This shell does not pretend to those.

use serde_json::{json, Value};

/// The marker version the SPA's bridge understands. A bundle that sees any
/// other value falls back to pure-seam mode rather than half-routing.
pub const MARKER_VERSION: u32 = 1;

pub struct Family {
    pub name: &'static str,
    /// Why a tab cannot answer these — the same reason the seam's refusal gives.
    pub why: &'static str,
    pub commands: &'static [&'static str],
}

pub const FAMILIES: &[Family] = &[
    Family {
        name: "project-git",
        why: "reads and writes a real working copy on this machine",
        commands: &[
            "clone_project_repository",
            "create_project_remote_branch",
            "delete_project_remote_branch",
            "discover_git_bash_prerequisite",
            "get_git_identity",
            "get_project_local_repo_diff",
            "get_project_local_repo_snapshot",
            "get_project_repo_diff",
            "get_project_repo_snapshot",
            "get_project_repo_sync_status",
            "list_project_local_repositories",
            "pull_project_local_repository",
            "push_project_local_repository",
        ],
    },
    Family {
        name: "project-merge",
        why: "merges in a real checkout and pushes the result",
        commands: &[
            "merge_project_pull_request",
            "publish_project_pull_request_merged_status",
        ],
    },
    Family {
        name: "project-terminal",
        why: "opens the OS terminal at a working copy",
        commands: &[
            "open_project_merge_recovery_terminal",
            "open_project_terminal",
        ],
    },
    Family {
        name: "harness",
        why: "supervises local agent processes over stdio ACP",
        commands: &[
            "connect_acp_runtime",
            "create_managed_agent",
            "delete_custom_harness",
            "delete_managed_agent",
            "discover_acp_auth_methods",
            "discover_managed_agent_prereqs",
            "get_managed_agent_log",
            "get_runtime_file_config",
            "has_managed_agent_channel_message_marker",
            "list_managed_agent_runtimes",
            "put_managed_agent_runtime_lifecycle",
            "restart_managed_agent_runtime",
            "save_custom_harness",
            "send_managed_agent_channel_message",
            "set_managed_agent_auto_restart",
            "set_managed_agent_start_on_app_launch",
            "start_managed_agent",
            "start_managed_agent_runtime",
            "stop_managed_agent",
            "stop_managed_agent_runtime",
            "update_managed_agent",
        ],
    },
    Family {
        name: "plugin",
        why: "Tauri implements these AS commands, and the bridge already speaks their \
              names — so registering them turns a stand-in into the real thing",
        commands: &[
            // A tab cannot open a link in the user's BROWSER: Tauri intercepts
            // `window.open`, so the seam's fallback does nothing inside a webview
            // and an external link would silently fail to open.
            "plugin:opener|open_url",
            // The dock/taskbar badge, and the window controls a tab genuinely
            // cannot perform on itself.
            "plugin:window|set_badge_count",
            "plugin:window|set_badge_label",
            "plugin:window|set_focus",
            "plugin:window|show",
            "plugin:window|unminimize",
        ],
    },
    Family {
        name: "meta",
        why: "lets an e2e run verify the injected marker against the compiled registry",
        commands: &["desktop_capabilities"],
    },
];

/// Commands the shell answers INSTEAD of the seam, and why the seam's answer is
/// wrong here.
///
/// The one-owner rule above has exactly this exception, and it is DECLARED rather
/// than discovered: a command whose truthful answer differs by face has to be
/// overridden, and the alternative — a seam answer that is right in a tab and
/// wrong in the shell — is the silent kind of wrong. `commandCoverage.test.mjs`
/// checks that every entry is genuinely answered by the seam, so an override that
/// overrides nothing fails a test instead of sitting here looking considered.
pub const OVERRIDES: &[(&str, &str)] = &[(
    "list_managed_agents",
    "the seam answers `[]` because a TAB hosts no agent processes — true of a tab, \
     and false of this shell, which supervises them. It is also the command the \
     whole local-agent path hangs from: the client learns an agent's pubkey here \
     and addresses everything downstream by it, so a `[]` would leave a configured \
     harness undiscoverable, unmentionable, and unable to answer anybody",
)];

/// Commands the shell deliberately LEAVES to the seam, with the reason.
///
/// Written down because the interesting question about a thin shell is not what
/// it does but what it declines to do, and a reader should not have to infer
/// that from an absence. Nothing here may be registered.
pub const DECLINED: &[(&str, &str)] = &[
    (
        "validate_repos_dir",
        "the seam already answers it, and one owner per command is the rule; the \
         shell validates the directory inside each git command instead, where a \
         failure can name the actual path that is wrong",
    ),
    (
        "sign_project_pull_request_status",
        "signing AS a managed agent needs that agent's key, and this shell holds no \
         signing key at all: the identity lives in the client, so one person has \
         one key across the browser and the desktop. The seam's honest refusal is \
         the better answer than a signature nobody can verify",
    ),
    (
        "sign_project_pull_request_review_request",
        "the same reason as the status event above: a review request is a SIGNED \
         event, and this shell has no key to sign it with. Both halves of the \
         signing pair are named here rather than one, so a reader does not have to \
         infer that the other is also declined",
    ),
    (
        "install_acp_runtime",
        "installing a harness means running a package manager as the user; the \
         shell reports what is missing (discover_managed_agent_prereqs) and lets \
         the person install it, rather than executing an installer on their behalf",
    ),
    (
        "plugin:updater|check",
        "there is no release manifest or signing key yet, so an updater would have \
         nothing to check against; the seam's refusal already lands the client in \
         its own 'automatic updates aren't available on this build' card, with the \
         Check Again button live",
    ),
    (
        "plugin:event|listen",
        "routing the event system natively is all-or-nothing (listen, unlisten AND \
         emit, or a same-page emit and a native listen would miss each other), and \
         this shell needs none of it: its permission dialog is NATIVE, and an \
         agent's answers reach the client through the relay like everyone else's",
    ),
];

#[must_use]
pub fn commands() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = FAMILIES
        .iter()
        .flat_map(|family| family.commands.iter().copied())
        .chain(OVERRIDES.iter().map(|(cmd, _)| *cmd))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

#[must_use]
pub fn is_registered(cmd: &str) -> bool {
    FAMILIES
        .iter()
        .any(|family| family.commands.contains(&cmd))
        || OVERRIDES.iter().any(|(name, _)| *name == cmd)
}

/// The marker document the shell injects, and the one `desktop_capabilities`
/// answers with. ONE function for both, so they cannot disagree.
#[must_use]
pub fn marker_document() -> Value {
    json!({
        "version": MARKER_VERSION,
        "commands": commands(),
        // Declared, so the client's coverage gate can tell a deliberate override
        // from an accidental double answer (see `OVERRIDES`). The bridge itself
        // reads only `version` and `commands`.
        "overrides": OVERRIDES.iter().map(|(cmd, _)| *cmd).collect::<Vec<_>>(),
    })
}

/// The initialization script the shell evaluates before the bundle loads.
///
/// Frozen because the marker is a CAPABILITY STATEMENT: a later script in the
/// page must not be able to add a command to it and make the bridge route
/// something the shell does not implement. `Object.freeze` is shallow, so the
/// array is frozen FIRST — freezing only the object would leave
/// `__BUZZ_DESKTOP__.commands.push("…")` working, which is exactly the mutation
/// worth preventing.
#[must_use]
pub fn init_script() -> String {
    format!(
        "window.__BUZZ_DESKTOP__ = (function () {{ \
         var marker = {}; \
         Object.freeze(marker.commands); \
         return Object.freeze(marker); \
         }})();",
        marker_document()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_has_no_duplicates() {
        // BOTH tables, because `commands()` routes both — so a name appearing in a
        // family AND in the override table is a duplicate too, and it is the one
        // hardest to spot by reading.
        let all: Vec<&str> = FAMILIES
            .iter()
            .flat_map(|family| family.commands.iter().copied())
            .chain(OVERRIDES.iter().map(|(cmd, _)| *cmd))
            .collect();
        let mut sorted = all.clone();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(before, sorted.len(), "a command is registered twice");
        assert_eq!(commands().len(), before);
    }

    #[test]
    fn every_family_is_documented_and_populated() {
        for family in FAMILIES {
            assert!(!family.commands.is_empty(), "{} is empty", family.name);
            assert!(
                family.why.len() > 20,
                "{} does not say why the shell owns it",
                family.name
            );
            for cmd in family.commands {
                // Two shapes, and only two: a bare command name, or Tauri's own
                // `plugin:<plugin>|<command>` — which is a command name too, and
                // the one the bridge sends for a plugin call.
                let plain = cmd
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
                let plugin = cmd.strip_prefix("plugin:").is_some_and(|rest| {
                    rest.split_once('|').is_some_and(|(name, command)| {
                        !name.is_empty()
                            && !command.is_empty()
                            && name
                                .chars()
                                .all(|c| c.is_ascii_lowercase() || c == '-')
                            && command.chars().all(|c| {
                                c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'
                            })
                    })
                });
                assert!(plain || plugin, "{cmd} is not a command name");
            }
        }
    }

    #[test]
    fn the_plugin_family_names_commands_the_bridge_actually_sends() {
        // These are not invented: each one is a string `desktop-bridge.ts` passes
        // to `mockedPluginCall`, so registering it is what turns that stand-in
        // into the real plugin call. A name that drifted would route nothing.
        for expected in [
            "plugin:opener|open_url",
            "plugin:window|set_badge_count",
            "plugin:window|set_badge_label",
            "plugin:window|set_focus",
            "plugin:window|show",
            "plugin:window|unminimize",
        ] {
            assert!(is_registered(expected), "{expected} must route native");
        }
        // ...and the two whose native routing would be wrong are NOT registered.
        assert!(!is_registered("plugin:updater|check"));
        assert!(!is_registered("plugin:event|listen"));
        assert!(!is_registered("plugin:event|emit"));
    }

    #[test]
    fn each_family_lists_its_commands_in_order() {
        // Sorted per family so a new entry lands in one obvious place and a
        // diff shows what changed rather than a reshuffle.
        for family in FAMILIES {
            let mut sorted = family.commands.to_vec();
            sorted.sort_unstable();
            assert_eq!(
                family.commands.to_vec(),
                sorted,
                "{} is not sorted",
                family.name
            );
        }
    }

    #[test]
    fn a_declined_command_is_never_registered() {
        for (cmd, why) in DECLINED {
            assert!(
                !is_registered(cmd),
                "{cmd} is both declined and registered"
            );
            assert!(why.len() > 40, "{cmd} is declined without a real reason");
        }
    }

    #[test]
    fn both_halves_of_the_signing_pair_are_declined() {
        // Declining one and silently omitting the other would leave a reader to
        // infer the second — and the reason (this shell holds no signing key)
        // applies to both, so it is written down for both.
        for signing in [
            "sign_project_pull_request_status",
            "sign_project_pull_request_review_request",
        ] {
            assert!(
                DECLINED.iter().any(|(cmd, _)| *cmd == signing),
                "{signing} must be declined explicitly"
            );
            assert!(!is_registered(signing));
        }
    }

    #[test]
    fn an_override_is_registered_reasoned_and_not_also_declined() {
        assert!(!OVERRIDES.is_empty(), "the table documents a real exception");
        for (cmd, why) in OVERRIDES {
            assert!(
                is_registered(cmd),
                "{cmd} must route native, or the override does nothing"
            );
            assert!(why.len() > 60, "{cmd} overrides the seam without a real reason");
            assert!(
                !DECLINED.iter().any(|(declined, _)| declined == cmd),
                "{cmd} cannot be both overridden and declined"
            );
        }
        // The marker carries them, which is what lets the JS gate distinguish a
        // deliberate override from an accidental double answer.
        let listed: Vec<String> = marker_document()["overrides"]
            .as_array()
            .expect("overrides array")
            .iter()
            .map(|v| v.as_str().expect("string").to_string())
            .collect();
        assert_eq!(
            listed,
            OVERRIDES.iter().map(|(cmd, _)| (*cmd).to_string()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_init_script_freezes_the_marker_and_its_command_array() {
        let script = init_script();
        assert!(script.starts_with("window.__BUZZ_DESKTOP__ = (function () {"));
        assert!(script.contains("\"version\":1"));
        assert!(script.contains("get_project_repo_snapshot"));
        // Both freezes, and the ARRAY first: `Object.freeze` is shallow, so
        // freezing only the object would leave `commands.push(…)` working.
        let array_at = script
            .find("Object.freeze(marker.commands)")
            .expect("the command array must be frozen");
        let object_at = script
            .find("Object.freeze(marker)")
            .expect("the marker must be frozen");
        assert!(array_at < object_at, "freeze the array before the object");
    }

    #[test]
    fn the_checked_in_document_matches_the_compiled_registry() {
        // The JS coverage gate reads the FILE; the shell injects the CODE. If
        // they can disagree, the gate is guarding something the shell does not
        // do — so they are compared here, in the one place that sees both.
        let generated = marker_document();
        assert_eq!(generated["version"], MARKER_VERSION);
        let listed: Vec<String> = generated["commands"]
            .as_array()
            .expect("commands array")
            .iter()
            .map(|v| v.as_str().expect("string").to_string())
            .collect();
        assert_eq!(listed, commands());

        let raw = include_str!("../../native-commands.json");
        let doc: Value = serde_json::from_str(raw).expect("native-commands.json is JSON");
        assert_eq!(
            doc, generated,
            "native-commands.json has drifted from registry::FAMILIES; \
             regenerate it from `marker_document()`"
        );
    }
}

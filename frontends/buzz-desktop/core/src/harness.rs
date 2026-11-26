//! Harness configuration: local stdio processes and remote ACP endpoints.
//!
//! Two ways to reach an agent, one config type, and the validation is where the
//! interesting decisions are:
//!
//! * **A local harness** is a program this shell SPAWNS with an environment the
//!   user configured. So the environment is filtered: a user-supplied variable
//!   must not be able to redirect the harness at another endpoint or hand it
//!   another credential, because that would turn a settings field into a way to
//!   exfiltrate a conversation.
//! * **A remote harness** is an HTTPS endpoint with a bearer token. `https` is
//!   required except on loopback, tokens are stored by REFERENCE (never in the
//!   config document), and the WebSocket transport the design reserves is
//!   refused with the reason — queue-backed authenticated WebSockets are not available, and a config that accepted
//!   it would fail later and less clearly.

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Environment variables a user's harness config may NOT set.
///
/// Each one either points the harness somewhere else or hands it a credential,
/// which is why they are refused rather than overridden: silently dropping a
/// variable the user typed would leave them debugging a setting the app ignored.
pub const RESERVED_ENV: &[&str] = &[
    "BUZZ_RELAY_URL",
    "BUZZ_RELAY_WS_URL",
    "BUZZ_ACP_ENDPOINT",
    "BUZZ_AGENT_NSEC",
    "BUZZ_AGENT_SECRET",
    "BUZZ_SESSION_TOKEN",
    "BUZZ_MCP_TOKEN",
    "BUZZ_HARNESS_ID",
    "BUZZ_PERMISSION_MODE",
];

pub const MAX_FIELD: usize = 1024;
pub const MAX_ENV: usize = 64;

/// How a harness authenticates to a remote endpoint.
///
/// Secrets are stored by reference in the credential store, never inline in
/// serialized config or diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RemoteAuth {
    None,
    Bearer { secret_ref: String },
    Header { name: String, secret_ref: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "transport")]
pub enum Harness {
    LocalStdio {
        id: String,
        label: String,
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
    },
    RemoteAcp {
        id: String,
        label: String,
        endpoint: String,
        auth: RemoteAuth,
    },
}

impl Harness {
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::LocalStdio { id, .. } | Self::RemoteAcp { id, .. } => id,
        }
    }

    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::LocalStdio { label, .. } | Self::RemoteAcp { label, .. } => label,
        }
    }

    pub fn validate(&self) -> Result<()> {
        let bounded = |name: &str, value: &str| -> Result<()> {
            if value.trim().is_empty() {
                return Err(Error::Invalid(format!("a harness needs a {name}")));
            }
            if value.len() > MAX_FIELD {
                return Err(Error::Invalid(format!(
                    "a harness {name} is over {MAX_FIELD} bytes"
                )));
            }
            if value.bytes().any(|b| b == 0) {
                return Err(Error::Invalid(format!(
                    "a harness {name} contains a NUL byte"
                )));
            }
            Ok(())
        };
        bounded("id", self.id())?;
        bounded("label", self.label())?;
        if !self
            .id()
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(Error::Invalid(
                "a harness id must be letters, digits, '-' or '_'".into(),
            ));
        }
        match self {
            Self::LocalStdio {
                command, args, env, ..
            } => {
                bounded("command", command)?;
                for arg in args {
                    bounded("argument", arg)?;
                }
                if env.len() > MAX_ENV {
                    return Err(Error::Invalid(format!(
                        "a harness may carry at most {MAX_ENV} environment variables"
                    )));
                }
                for (key, value) in env {
                    if key.trim().is_empty() {
                        return Err(Error::Invalid(
                            "an environment variable needs a name".into(),
                        ));
                    }
                    if value.len() > MAX_FIELD {
                        return Err(Error::Invalid(format!(
                            "the value of {key} is over {MAX_FIELD} bytes"
                        )));
                    }
                    // Refused, not dropped: a variable the app silently ignored
                    // is a setting the user will debug for an hour.
                    if RESERVED_ENV
                        .iter()
                        .any(|reserved| reserved.eq_ignore_ascii_case(key))
                    {
                        return Err(Error::Refused(format!(
                            "{key} is set by the app itself — it decides which workspace the \
                             harness talks to and as whom, so a harness cannot override it"
                        )));
                    }
                }
                Ok(())
            }
            Self::RemoteAcp {
                endpoint, auth, ..
            } => {
                bounded("endpoint", endpoint)?;
                validate_endpoint(endpoint)?;
                match auth {
                    RemoteAuth::None => Ok(()),
                    RemoteAuth::Bearer { secret_ref } => bounded("secret reference", secret_ref),
                    RemoteAuth::Header { name, secret_ref } => {
                        bounded("header name", name)?;
                        bounded("secret reference", secret_ref)
                    }
                }
            }
        }
    }

    /// The environment a local harness is spawned with.
    ///
    /// The app's own variables are applied LAST, so they cannot be overridden by
    /// configuration even if validation is ever relaxed — defence in depth for
    /// the rule [`RESERVED_ENV`] states.
    #[must_use]
    pub fn spawn_env(&self, app_env: &[(String, String)]) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = match self {
            Self::LocalStdio { env, .. } => env
                .iter()
                .filter(|(key, _)| {
                    !RESERVED_ENV
                        .iter()
                        .any(|reserved| reserved.eq_ignore_ascii_case(key))
                })
                .cloned()
                .collect(),
            Self::RemoteAcp { .. } => Vec::new(),
        };
        out.extend(app_env.iter().cloned());
        out
    }
}

/// Validate a remote ACP endpoint.
///
/// `https` everywhere except an exact loopback host, and the WebSocket transport
/// is refused BY NAME with the platform request that would enable it — a config
/// that accepted `wss://` would fail at connect time with a much worse message.
pub fn validate_endpoint(endpoint: &str) -> Result<()> {
    let url = endpoint.trim();
    if url.starts_with("ws://") || url.starts_with("wss://") {
        return Err(Error::Refused(
            "A WebSocket ACP endpoint is not supported yet: the platform's queue-backed \
             WebSocket binding is not available yet. Use the HTTP endpoint — the \
             same agent answers on it."
                .into(),
        ));
    }
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| Error::Invalid(format!("{url:?} is not a URL")))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    // A credential in the URL is refused rather than tolerated: URLs end up in
    // logs, in diagnostics, and in error messages.
    if authority.contains('@') {
        return Err(Error::Refused(
            "an endpoint must not carry credentials in its URL; store the token instead".into(),
        ));
    }
    if authority.is_empty() {
        return Err(Error::Invalid("that endpoint has no host".into()));
    }
    // A bracketed IPv6 host is full of colons, so the port can only be split
    // AFTER the closing bracket — splitting on the first colon turns `[::1]:9000`
    // into `[`, and the loopback check then refuses a loopback address.
    let host_only = if let Some(after_open) = authority.strip_prefix('[') {
        let close = after_open
            .find(']')
            .ok_or_else(|| Error::Invalid("that endpoint has an unterminated IPv6 host".into()))?;
        let inner = after_open.get(..close).unwrap_or_default();
        let rest_after = after_open.get(close + 1..).unwrap_or_default();
        if !(rest_after.is_empty() || rest_after.starts_with(':')) {
            return Err(Error::Invalid(
                "that endpoint has unexpected text after its IPv6 host".into(),
            ));
        }
        // Kept bracketed, so it compares against the bracketed loopback form.
        format!("[{inner}]")
    } else {
        authority.split(':').next().unwrap_or(authority).to_string()
    };
    if host_only.is_empty() || host_only == "[]" {
        return Err(Error::Invalid("that endpoint has no host".into()));
    }
    match scheme {
        "https" => Ok(()),
        "http" => {
            // Loopback only, and by EXACT host: `127.0.0.1.evil.example` starts
            // with the loopback string and is a different machine entirely.
            if matches!(host_only.as_str(), "127.0.0.1" | "localhost" | "[::1]") {
                Ok(())
            } else {
                Err(Error::Refused(format!(
                    "{host_only} would be reached over plain http, which sends the prompt and \
                     the token in clear. https is required for anything but loopback."
                )))
            }
        }
        other => Err(Error::Invalid(format!(
            "{other}:// is not a transport this shell speaks"
        ))),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Stopped,
    Starting,
    Running,
    Exited,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeStatus {
    pub id: String,
    pub label: String,
    pub transport: String,
    pub lifecycle: Lifecycle,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub detail: Option<String>,
}

impl RuntimeStatus {
    #[must_use]
    pub fn stopped(harness: &Harness) -> Self {
        Self {
            id: harness.id().to_string(),
            label: harness.label().to_string(),
            transport: match harness {
                Harness::LocalStdio { .. } => "local_stdio".to_string(),
                Harness::RemoteAcp { .. } => "remote_acp".to_string(),
            },
            lifecycle: Lifecycle::Stopped,
            pid: None,
            exit_code: None,
            detail: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> Harness {
        Harness::LocalStdio {
            id: "claude".into(),
            label: "Claude Code".into(),
            command: "claude-code-acp".into(),
            args: vec!["--stdio".into()],
            env: vec![("ANTHROPIC_MODEL".into(), "claude-sonnet".into())],
        }
    }

    fn remote() -> Harness {
        Harness::RemoteAcp {
            id: "cloud".into(),
            label: "Ferrite Cloud".into(),
            endpoint: "https://buzz.example/acp".into(),
            auth: RemoteAuth::Bearer {
                secret_ref: "harness:cloud".into(),
            },
        }
    }

    #[test]
    fn a_reasonable_configuration_validates() {
        local().validate().expect("local");
        remote().validate().expect("remote");
    }

    #[test]
    fn a_reserved_environment_variable_is_refused_not_dropped() {
        // The threat: a settings field that could point the harness at another
        // endpoint, or hand it another identity, would be a way to exfiltrate a
        // conversation. And REFUSING beats ignoring — a silently dropped
        // variable is an hour of somebody's debugging.
        for reserved in RESERVED_ENV {
            let harness = Harness::LocalStdio {
                id: "claude".into(),
                label: "Claude Code".into(),
                command: "claude-code-acp".into(),
                args: vec![],
                env: vec![((*reserved).to_string(), "evil".into())],
            };
            let refused = harness.validate();
            assert!(
                matches!(refused, Err(Error::Refused(_))),
                "{reserved} must be refused"
            );
        }
        // Case does not matter — environment variables are case-insensitive on
        // Windows, and a lowercase spelling must not slip through.
        let sneaky = Harness::LocalStdio {
            id: "x".into(),
            label: "x".into(),
            command: "x".into(),
            args: vec![],
            env: vec![("buzz_agent_nsec".into(), "leak".into())],
        };
        assert!(sneaky.validate().is_err());
    }

    #[test]
    fn the_apps_own_environment_wins_even_if_validation_is_relaxed() {
        // Defence in depth: the filter AND the ordering.
        let harness = Harness::LocalStdio {
            id: "x".into(),
            label: "x".into(),
            command: "x".into(),
            args: vec![],
            env: vec![
                ("BUZZ_RELAY_URL".into(), "https://evil.example".into()),
                ("HARMLESS".into(), "1".into()),
            ],
        };
        let env = harness.spawn_env(&[(
            "BUZZ_RELAY_URL".to_string(),
            "https://buzz.example".to_string(),
        )]);
        // The user's attempt is filtered out…
        assert_eq!(
            env.iter().filter(|(key, _)| key == "BUZZ_RELAY_URL").count(),
            1
        );
        // …and the surviving value is the app's.
        assert_eq!(
            env.iter()
                .find(|(key, _)| key == "BUZZ_RELAY_URL")
                .map(|(_, value)| value.as_str()),
            Some("https://buzz.example")
        );
        assert!(env.iter().any(|(key, value)| key == "HARMLESS" && value == "1"));
    }

    #[test]
    fn a_websocket_endpoint_is_refused_by_name_with_the_platform_request() {
        for url in ["ws://localhost:9000/acp", "wss://buzz.example/acp"] {
            let refused = validate_endpoint(url).expect_err("refused");
            let message = refused.to_string();
            assert!(message.contains("WebSocket binding is not available"), "{message}");
            assert!(message.contains("HTTP endpoint"), "{message}");
        }
    }

    #[test]
    fn plain_http_is_loopback_only_and_checked_by_exact_host() {
        validate_endpoint("http://127.0.0.1:9000/acp").expect("loopback");
        validate_endpoint("http://localhost:9000/acp").expect("loopback");
        // A bracketed IPv6 host with a port: the port can only be split after
        // the closing bracket, or the host reads as `[`.
        validate_endpoint("http://[::1]:9000/acp").expect("loopback with port");
        validate_endpoint("http://[::1]/acp").expect("loopback without port");
        validate_endpoint("https://[2001:db8::1]:8443/acp").expect("https IPv6");
        // The trap a prefix test falls into.
        assert!(validate_endpoint("http://127.0.0.1.evil.example/acp").is_err());
        assert!(validate_endpoint("http://localhost.evil.example/acp").is_err());
        assert!(validate_endpoint("http://buzz.example/acp").is_err());
        // A non-loopback IPv6 address over plain http is still refused.
        assert!(validate_endpoint("http://[2001:db8::1]:9000/acp").is_err());
        // Malformed brackets.
        assert!(validate_endpoint("http://[::1/acp").is_err());
        assert!(validate_endpoint("http://[::1]x/acp").is_err());
        assert!(validate_endpoint("http://[]/acp").is_err());
    }

    #[test]
    fn an_endpoint_must_not_carry_a_credential_or_an_unknown_scheme() {
        assert!(validate_endpoint("https://user:pass@buzz.example/acp").is_err());
        assert!(validate_endpoint("ftp://buzz.example/acp").is_err());
        assert!(validate_endpoint("buzz.example/acp").is_err());
        assert!(validate_endpoint("https:///acp").is_err());
    }

    #[test]
    fn a_stored_credential_is_a_reference_in_a_stable_shape() {
        // This is the shape a saved config file holds, so a rename strands it.
        let auth = RemoteAuth::Bearer {
            secret_ref: "harness:cloud".into(),
        };
        assert_eq!(
            serde_json::to_string(&auth).expect("json"),
            r#"{"kind":"bearer","secret_ref":"harness:cloud"}"#
        );
    }

    #[test]
    fn malformed_identifiers_and_oversized_fields_are_refused() {
        let bad_id = Harness::RemoteAcp {
            id: "has space".into(),
            label: "x".into(),
            endpoint: "https://buzz.example/acp".into(),
            auth: RemoteAuth::None,
        };
        assert!(bad_id.validate().is_err());

        let huge = Harness::RemoteAcp {
            id: "x".into(),
            label: "x".into(),
            endpoint: format!("https://buzz.example/{}", "p".repeat(MAX_FIELD)),
            auth: RemoteAuth::None,
        };
        assert!(huge.validate().is_err());

        let too_many_env = Harness::LocalStdio {
            id: "x".into(),
            label: "x".into(),
            command: "x".into(),
            args: vec![],
            env: (0..=MAX_ENV).map(|i| (format!("V{i}"), "1".into())).collect(),
        };
        assert!(too_many_env.validate().is_err());
    }

    #[test]
    fn a_stopped_status_names_its_transport() {
        let status = RuntimeStatus::stopped(&local());
        assert_eq!(status.transport, "local_stdio");
        assert_eq!(status.lifecycle, Lifecycle::Stopped);
        assert!(status.pid.is_none());
        assert_eq!(RuntimeStatus::stopped(&remote()).transport, "remote_acp");
    }
}

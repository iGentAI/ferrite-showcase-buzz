//! The workspace this shell posts to, and the identity it posts as.
//!
//! A local ACP adapter is not a buzz client (see [`buzz_desktop_core::nostr`]),
//! so when it answers, the SHELL puts those words in the room — signed by the
//! agent's own key, never by the user's.
//!
//! Two things are needed for that and neither can be guessed:
//!
//! * **the workspace origin**, because the shell serves the SPA from
//!   `tauri://localhost` and so has no page origin to derive one from;
//! * **an agent key**, generated once per harness and kept in the OS keychain.
//!
//! When the origin is not configured, posting is REFUSED with a reason naming how
//! to set it — rather than silently dropping an answer somebody is waiting for.

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use buzz_desktop_core::credentials::{Key, Store};
use buzz_desktop_core::nostr::{self, AgentKey};
use serde_json::Value;

use crate::state::AppState;

/// Checked before [`ORIGIN_FILE`] so a developer can point one build at another
/// deployment without editing files.
pub const ORIGIN_ENV: &str = "BUZZ_WORKSPACE_ORIGIN";
pub const ORIGIN_FILE: &str = "workspace.json";

const POST_TIMEOUT: Duration = Duration::from_secs(30);

/// Each managed agent's own workspace origin, `{harness id: origin}`, beside
/// [`ORIGIN_FILE`]. An agent belongs to the community whose relay URL it was
/// created with, so its profile and answers go there, never to whichever
/// workspace was configured first.
pub const AGENT_ORIGINS_FILE: &str = "agent-origins.json";

/// Serializes the read-modify-write of [`AGENT_ORIGINS_FILE`].
static AGENT_ORIGINS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serializes [`agent_key`]'s look-up-or-mint.
static AGENT_KEY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn parse_valid_port(port_str: &str) -> Result<u16, String> {
    if port_str.is_empty() || !port_str.bytes().all(|b| b.is_ascii_digit()) {
        return Err("a workspace origin port must be 1–65535".to_string());
    }
    let port: u16 = port_str
        .parse()
        .map_err(|_| "a workspace origin port must be 1–65535".to_string())?;
    if port == 0 {
        return Err("a workspace origin port must be 1–65535".to_string());
    }
    Ok(port)
}

/// Validate a workspace origin.
///
/// `https` everywhere except an exact loopback host, and no path, query, fragment
/// or userinfo — an origin carrying a path would be concatenated with the request
/// path and produce a URL nobody intended, and a credential in a URL ends up in
/// logs.
pub fn validate_origin(origin: &str) -> Result<String, String> {
    let trimmed = origin.trim().trim_end_matches('/');
    if trimmed.is_empty() || trimmed.len() > 512 {
        return Err("a workspace origin must be 1–512 characters".to_string());
    }
    if trimmed
        .bytes()
        .any(|b| b == b'\\' || b.is_ascii_control() || b.is_ascii_whitespace())
    {
        return Err("a workspace origin must not contain backslashes or whitespace".to_string());
    }
    let (scheme, authority) = trimmed
        .split_once("://")
        .ok_or_else(|| format!("{trimmed:?} is not a URL"))?;
    let scheme = scheme.to_ascii_lowercase();
    if authority.contains('/') || authority.contains('?') || authority.contains('#') {
        return Err("a workspace origin is scheme://host[:port] with no path".to_string());
    }
    if authority.contains('@') {
        return Err("a workspace origin must not carry credentials".to_string());
    }
    // A bracketed IPv6 host is full of colons, so the port can only be split
    // after the closing bracket — the same trap `harness::validate_endpoint`
    // documents.
    let (host, port) = if let Some(after) = authority.strip_prefix('[') {
        let close = after
            .find(']')
            .ok_or_else(|| "unterminated IPv6 host".to_string())?;
        let inner = after.get(..close).unwrap_or_default();
        if inner.parse::<std::net::Ipv6Addr>().is_err() {
            return Err("invalid IPv6 host in workspace origin".to_string());
        }
        let rest = after.get(close + 1..).unwrap_or_default();
        let port = if rest.is_empty() {
            None
        } else if let Some(port_str) = rest.strip_prefix(':') {
            Some(parse_valid_port(port_str)?)
        } else {
            return Err("unexpected text after the IPv6 host".to_string());
        };
        (format!("[{}]", inner.to_ascii_lowercase()), port)
    } else if let Some((raw_host, port_str)) = authority.split_once(':') {
        let port = parse_valid_port(port_str)?;
        (raw_host.to_ascii_lowercase(), Some(port))
    } else {
        (authority.to_ascii_lowercase(), None)
    };
    if host.is_empty()
        || host == "[]"
        || (!host.starts_with('[')
            && !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_')))
    {
        return Err("a workspace origin has no host".to_string());
    }
    let canonical = match port {
        Some(p) => format!("{scheme}://{host}:{p}"),
        None => format!("{scheme}://{host}"),
    };
    match scheme.as_str() {
        "https" => Ok(canonical),
        "http" if matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]") => Ok(canonical),
        "http" => Err(format!(
            "{host} would be reached over plain http, which sends an agent's words \
             and its authorization in clear; https is required for anything but loopback"
        )),
        other => Err(format!("{other}:// is not a workspace origin")),
    }
}

/// The configured workspace origin, or a refusal naming how to set one.
///
/// The environment value is passed IN rather than read here, so this is testable
/// without depending on the shell a test happens to run in: a function that reads
/// `std::env` directly passes or fails according to the developer's own
/// environment, which is not a property of the code.
pub fn origin_from(from_env: Option<&str>, data_dir: &Path) -> Result<String, String> {
    if let Some(configured) = from_env.map(str::trim).filter(|value| !value.is_empty()) {
        return validate_origin(configured);
    }
    let path = data_dir.join(ORIGIN_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Err(format!(
            "this shell does not know which workspace to post to, so an agent's \
             answer has nowhere to go. Set {ORIGIN_ENV}, or write {{\"origin\": \
             \"https://…\"}} to {}",
            path.display()
        ));
    };
    let parsed: Value = serde_json::from_str(&text)
        .map_err(|e| format!("{} does not parse: {e}", path.display()))?;
    let configured = parsed
        .get("origin")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{} has no \"origin\" field", path.display()))?;
    validate_origin(configured)
}

pub fn origin(data_dir: &Path) -> Result<String, String> {
    origin_from(std::env::var(ORIGIN_ENV).ok().as_deref(), data_dir)
}

fn read_agent_origins(data_dir: &Path) -> serde_json::Map<String, Value> {
    std::fs::read_to_string(data_dir.join(AGENT_ORIGINS_FILE))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

fn write_agent_origins(data_dir: &Path, origins: &serde_json::Map<String, Value>) -> Result<(), String> {
    let path = data_dir.join(AGENT_ORIGINS_FILE);
    let staged = data_dir.join(format!("{AGENT_ORIGINS_FILE}.tmp"));
    let text = serde_json::to_string_pretty(origins).map_err(|e| e.to_string())?;
    std::fs::write(&staged, text).map_err(|e| format!("could not write {}: {e}", staged.display()))?;
    std::fs::rename(&staged, &path).map_err(|e| format!("could not write {}: {e}", path.display()))
}

/// Bind one agent to the workspace it was created for.
pub fn bind_agent_origin(state: &AppState, harness_id: &str, origin_value: &str) -> Result<(), String> {
    bind_agent_origin_in(&state.data_dir, harness_id, origin_value)
}

fn bind_agent_origin_in(data_dir: &Path, harness_id: &str, origin_value: &str) -> Result<(), String> {
    let origin = validate_origin(origin_value)?;
    let _guard = AGENT_ORIGINS_LOCK.lock().map_err(|_| "the agent origins are unavailable".to_string())?;
    let mut origins = read_agent_origins(data_dir);
    if origins.get(harness_id).and_then(Value::as_str) == Some(origin.as_str()) {
        return Ok(());
    }
    origins.insert(harness_id.to_string(), Value::String(origin));
    write_agent_origins(data_dir, &origins)
}

/// Forget a deleted agent's workspace, so an id used again starts unbound.
pub fn forget_agent_origin(state: &AppState, harness_id: &str) {
    let Ok(_guard) = AGENT_ORIGINS_LOCK.lock() else {
        return;
    };
    let mut origins = read_agent_origins(&state.data_dir);
    if origins.remove(harness_id).is_some() {
        let _ = write_agent_origins(&state.data_dir, &origins);
    }
}

/// The workspace origin this agent posts to: its own when it has one, and
/// otherwise the shell's configured origin (an agent created before agents
/// were bound, or without a relay URL).
pub fn agent_origin(state: &AppState, harness_id: &str) -> Result<String, String> {
    agent_origin_from(std::env::var(ORIGIN_ENV).ok().as_deref(), &state.data_dir, harness_id)
}

fn agent_origin_from(from_env: Option<&str>, data_dir: &Path, harness_id: &str) -> Result<String, String> {
    match read_agent_origins(data_dir).get(harness_id).and_then(Value::as_str) {
        Some(bound) => validate_origin(bound),
        None => origin_from(from_env, data_dir),
    }
}

/// One harness's agent key, generated and stored on first use.
///
/// Generated ONCE and kept, because the key IS the agent's workspace identity: a
/// new key per launch would be a new stranger every time, with no membership and
/// no history, and every message it ever posted orphaned from the next.
pub fn agent_key(state: &AppState, harness_id: &str) -> Result<AgentKey, String> {
    // The read, the mint and the write are one step: two callers that both
    // read "none" would otherwise mint two keys, and the later write would
    // orphan the identity the first one had already published or shown.
    let _minting = AGENT_KEY_LOCK
        .lock()
        .map_err(|_| "the agent key store is unavailable".to_string())?;
    let slot = Key::agent(harness_id);
    if let Some(stored) = state.keychain.get(&slot).map_err(|e| e.to_string())? {
        return AgentKey::from_hex(&stored).map_err(|e| e.to_string());
    }
    let key = AgentKey::generate();
    state
        .keychain
        .set(&slot, &key.to_hex())
        .map_err(|e| e.to_string())?;
    Ok(key)
}

/// The pubkey a moderator adds to a room to let this agent speak in it.
///
/// Generates the key if there is none, because a settings screen asking "who is
/// this agent?" should get an answer rather than "start it first". The reason is
/// KEPT rather than flattened to an `Option`: a caller that must not serve an
/// unaddressable agent needs to be able to say why it could not (see
/// `commands::list_managed_agents`).
pub fn agent_identity(state: &AppState, harness_id: &str) -> Result<String, String> {
    agent_key(state, harness_id).map(|key| key.pubkey_hex())
}

pub fn agent_pubkey(state: &AppState, harness_id: &str) -> Option<String> {
    agent_identity(state, harness_id).ok()
}

/// The identity this harness ALREADY has, without minting one.
///
/// Separate from [`agent_pubkey`] because a LOOKUP must not have side effects:
/// resolving "which harness is this pubkey?" would otherwise mint a key for every
/// configured harness as it searched.
fn existing_agent_pubkey(state: &AppState, harness_id: &str) -> Option<String> {
    let stored = state.keychain.get(&Key::agent(harness_id)).ok()??;
    AgentKey::from_hex(&stored).ok().map(|key| key.pubkey_hex())
}

/// The harness whose AGENT identity is `pubkey`.
///
/// The client addresses a managed agent by its pubkey — the only name it has for
/// one, since a harness id is this shell's own bookkeeping. Resolving against the
/// same keychain-held key the answer will be signed with means a match names an
/// agent this shell can actually speak as; a harness that has never run has no
/// key, so it cannot be the one being addressed.
pub fn harness_for_agent(state: &AppState, pubkey: &str) -> Option<String> {
    let wanted = pubkey.trim().to_ascii_lowercase();
    if wanted.len() != 64 {
        return None;
    }
    let ids: Vec<String> = state
        .harnesses
        .lock()
        .ok()?
        .iter()
        .map(|harness| harness.id().to_string())
        .collect();
    ids.into_iter()
        .find(|id| existing_agent_pubkey(state, id).as_deref() == Some(wanted.as_str()))
}

/// POST one signed event to the workspace, as the key that signed it.
///
/// Shared by every event this shell publishes (an answer, a profile), so ONE
/// place knows the route, the authorization and how a refusal is reported — two
/// copies would be two chances to swallow the relay's own reason.
fn post_event(
    state: &AppState,
    origin: &str,
    key: &AgentKey,
    event: &nostr::Event,
) -> Result<String, String> {
    let body = serde_json::to_vec(event).map_err(|e| e.to_string())?;
    let path = "/buzz/api/events";
    // The relay binds the `u` tag's origin as well as its path, so the
    // header names the absolute URL this request is sent to.
    let header = nostr::nip98_header(
        key,
        "POST",
        &format!("{origin}{path}"),
        &body,
        event.created_at,
        &state.nonces.next(),
    )
    .map_err(|e| e.to_string())?;

    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout_connect(Duration::from_secs(15))
        .timeout(POST_TIMEOUT)
        .build();
    let request = agent
        .post(&format!("{origin}{path}"))
        .set("content-type", "application/json")
        .set("authorization", &header);
    let (status, response) = match request.send_bytes(&body) {
        Ok(response) => (response.status(), response),
        Err(ureq::Error::Status(status, response)) => (status, response),
        Err(e) => return Err(format!("the workspace could not be reached: {e}")),
    };
    if (300..400).contains(&status) {
        return Err(format!(
            "the workspace returned a redirect ({status}), which this shell refuses to follow"
        ));
    }
    let mut text_body = String::new();
    let _ = response
        .into_reader()
        .take(64 * 1024)
        .read_to_string(&mut text_body);
    if !(200..300).contains(&status) {
        // The relay's own sentence, which is far more useful than a paraphrase:
        // it says whether the agent is not a member, is rate-limited, or is
        // banned.
        return Err(format!(
            "the workspace refused this event ({status}): {}",
            text_body.trim()
        ));
    }
    let parsed: Value = serde_json::from_str(&text_body).unwrap_or(Value::Null);
    if parsed.get("ok").and_then(Value::as_bool) == Some(false) {
        return Err(format!(
            "the workspace refused this event: {}",
            parsed
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("no reason given")
        ));
    }
    Ok(parsed
        .get("event_id")
        .and_then(Value::as_str)
        .unwrap_or(&event.id)
        .to_string())
}

/// Post one agent message into a channel, as the agent.
///
/// The relay's own gates apply exactly as they do to a person: if the agent is
/// not a member of that room, the refusal comes back in the relay's words
/// (`restricted: not a member of this channel`) and is reported rather than
/// swallowed — which is the signal that a moderator has not added this agent yet.
pub fn post_as_agent(
    state: &AppState,
    harness_id: &str,
    target: &nostr::AnswerTarget,
    text: &str,
) -> Result<String, String> {
    // A turn can finish after its harness was stopped or deleted. Its answer
    // is not posted then, and no key is minted for an id that no longer exists
    // (finding_5083a6acdc67025a).
    if state.harness(harness_id).is_err() {
        return Err(format!("{harness_id} is no longer configured; its answer was not posted"));
    }
    if state
        .disconnecting
        .lock()
        .map(|disconnecting| disconnecting.contains(harness_id))
        .unwrap_or(true)
    {
        return Err(format!("{harness_id} is being stopped; its answer was not posted"));
    }
    let key = agent_key(state, harness_id)?;
    let created_at = crate::state::now_ms() / 1_000;
    let origin = agent_origin(state, harness_id)?;
    let event =
        nostr::channel_message(&key, target, text, created_at).map_err(|e| e.to_string())?;
    post_event(state, &origin, &key, &event)
}

/// Publish the agent's kind-0 profile, so it can be MENTIONED.
///
/// Without a profile a client renders a truncated pubkey and its mention picker —
/// which is built from profiles — never offers the agent, so nobody can ask it
/// for anything. Published on create and on rename, which is exactly when the
/// name a person typed changes.
pub fn publish_agent_profile(
    state: &AppState,
    harness_id: &str,
    name: &str,
) -> Result<String, String> {
    let key = agent_key(state, harness_id)?;
    let created_at = crate::state::now_ms() / 1_000;
    let origin = agent_origin(state, harness_id)?;
    let event = nostr::profile_event(&key, name, created_at).map_err(|e| e.to_string())?;
    post_event(state, &origin, &key, &event)
}

/// The agent's key as an `nsec`, for the client's backup panel.
///
/// The only place a secret leaves this shell, and it goes to the PERSON rather
/// than over a wire: an agent's whole history hangs off this key, so somebody who
/// wants to keep it has to be able to see it once.
pub fn agent_nsec(state: &AppState, harness_id: &str) -> Option<String> {
    agent_key(state, harness_id).ok().and_then(|key| key.nsec())
}

/// The workspace origin implied by a RELAY URL.
///
/// The client's agent dialog asks for a relay URL, and that names the same
/// deployment this shell must post to — so somebody who filled that field in has
/// already answered the origin question. `ws` becomes `http` and `wss` becomes
/// `https`, exactly as the browser seam derives its own HTTP base, and the path
/// is dropped because an origin has none.
#[must_use]
pub fn origin_from_relay_url(relay_url: &str) -> Option<String> {
    let trimmed = relay_url.trim();
    if trimmed
        .bytes()
        .any(|b| b == b'\\' || b.is_ascii_control() || b.is_ascii_whitespace())
    {
        return None;
    }
    let rewritten = if let Some(rest) = trimmed.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        trimmed.to_string()
    };
    let (scheme, rest) = rewritten.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    // Validated like any other origin, so a plaintext socket to a real host is
    // refused rather than silently upgraded to https — which would invent a
    // deployment nobody named.
    validate_origin(&format!("{scheme}://{authority}")).ok()
}

/// Record a workspace origin, if none is configured yet.
///
/// Create-if-absent on purpose: an origin somebody set deliberately — in the
/// environment or in the file — must not be replaced by one inferred from a form
/// field. `Ok(false)` means one was already configured and this changed nothing.
pub fn remember_origin(state: &AppState, origin_value: &str) -> Result<bool, String> {
    let candidate = validate_origin(origin_value)?;
    // Anything set deliberately wins, even when it does not parse: a set
    // environment variable, or a workspace.json that exists (a broken file is
    // the user's to fix, not this function's to replace).
    if std::env::var(ORIGIN_ENV).is_ok_and(|value| !value.trim().is_empty())
        || origin(&state.data_dir).is_ok()
    {
        return Ok(false);
    }
    let path = state.data_dir.join(ORIGIN_FILE);
    let document = serde_json::json!({ "origin": candidate });
    let text = serde_json::to_string_pretty(&document).map_err(|e| e.to_string())?;
    // create_new makes the existence check and the write one step, so a file
    // that appeared meanwhile, or one that exists but is invalid, is never
    // overwritten.
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(format!("could not write {}: {e}", path.display())),
    };
    std::io::Write::write_all(&mut file, text.as_bytes())
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_agent_posts_to_the_workspace_it_was_created_for() {
        // The gap: one app-wide origin, written once, sent a second
        // community's agent's profile and answers to the first community.
        let dir = std::env::temp_dir().join(format!("buzz-agent-origins-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(ORIGIN_FILE), r#"{"origin": "https://first.example"}"#).expect("write");
        bind_agent_origin_in(&dir, "agent-a", "https://first.example").expect("bind a");
        bind_agent_origin_in(&dir, "agent-b", "https://second.example").expect("bind b");
        assert_eq!(agent_origin_from(None, &dir, "agent-a").expect("a"), "https://first.example");
        assert_eq!(agent_origin_from(None, &dir, "agent-b").expect("b"), "https://second.example");
        // An agent bound before binding existed keeps the shell's origin.
        assert_eq!(agent_origin_from(None, &dir, "older").expect("older"), "https://first.example");
        // A binding is validated like any origin.
        assert!(bind_agent_origin_in(&dir, "agent-c", "http://plain.example").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_origin_is_validated_structurally() {
        assert_eq!(
            validate_origin("https://buzz.example/").expect("ok"),
            "https://buzz.example"
        );
        assert!(validate_origin("https://buzz.example:8443").is_ok());
        assert!(validate_origin("http://127.0.0.1:8080").is_ok(), "dev loopback");
        assert!(validate_origin("http://[::1]:8080").is_ok(), "dev loopback");
        for hostile in [
            "http://buzz.example",
            "http://127.0.0.1.evil.example",
            "https://user:pass@buzz.example",
            "https://buzz.example/buzz/api",
            "https://buzz.example?x=1",
            "https://buzz.example\\prefix",
            "http://127.0.0.1:8080\\custom\\prefix",
            "https://buzz.example:notaport",
            "https://buzz.example:0",
            "https://buzz.example:99999",
            "https://buzz.example:",
            "https://buzz.example:80:90",
            "https://[::1]:notaport",
            "ws://buzz.example",
            "buzz.example",
            "",
        ] {
            assert!(validate_origin(hostile).is_err(), "{hostile} must be refused");
        }
    }

    #[test]
    fn an_absent_origin_says_how_to_set_one() {
        let dir = std::env::temp_dir().join(format!("buzz-origin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        // The environment is SUPPLIED, so this test says the same thing on every
        // machine — including one where BUZZ_WORKSPACE_ORIGIN happens to be set.
        let refused = origin_from(None, &dir).expect_err("no origin configured");
        assert!(refused.contains(ORIGIN_ENV), "{refused}");
        assert!(refused.contains(ORIGIN_FILE), "{refused}");

        // The environment wins when it names one, and is validated like anything
        // else.
        assert_eq!(
            origin_from(Some("https://env.example/"), &dir).expect("from env"),
            "https://env.example"
        );
        assert!(origin_from(Some("http://evil.example"), &dir).is_err());
        // An empty or whitespace value is not a setting: the file still answers.
        assert!(origin_from(Some("   "), &dir).is_err(), "and no file yet");

        // A file is read and validated.
        std::fs::write(
            dir.join(ORIGIN_FILE),
            "{\"origin\":\"https://buzz.example\"}",
        )
        .expect("write");
        assert_eq!(
            origin_from(None, &dir).expect("configured"),
            "https://buzz.example"
        );
        // ...and the environment still takes precedence over it.
        assert_eq!(
            origin_from(Some("https://env.example"), &dir).expect("env wins"),
            "https://env.example"
        );

        // A file naming an unusable origin is an error rather than a silent
        // fallback to something else.
        std::fs::write(dir.join(ORIGIN_FILE), "{\"origin\":\"http://evil.example\"}")
            .expect("write");
        assert!(origin_from(None, &dir).is_err());
        std::fs::write(dir.join(ORIGIN_FILE), "not json").expect("write");
        assert!(origin_from(None, &dir).is_err());
        std::fs::write(dir.join(ORIGIN_FILE), "{}").expect("write");
        assert!(origin_from(None, &dir).is_err(), "no origin field");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_relay_url_implies_the_workspace_origin() {
        // The client's agent dialog asks for a relay URL, which names the same
        // deployment — so filling that in answers the origin question too, and a
        // person is not asked the same thing twice.
        assert_eq!(
            origin_from_relay_url("wss://buzz.example/buzz/").expect("origin"),
            "https://buzz.example"
        );
        assert_eq!(
            origin_from_relay_url("wss://buzz.example:8443/buzz/").expect("origin"),
            "https://buzz.example:8443"
        );
        assert_eq!(
            origin_from_relay_url("ws://127.0.0.1:8080/buzz/").expect("dev loopback"),
            "http://127.0.0.1:8080"
        );
        // An https URL is already an origin once its path is dropped.
        assert_eq!(
            origin_from_relay_url("https://buzz.example/buzz/api").expect("origin"),
            "https://buzz.example"
        );
        // A plaintext socket to a REAL host implies an origin this shell will not
        // post to, and it is refused rather than silently upgraded to https —
        // which would invent a deployment nobody named.
        assert!(origin_from_relay_url("ws://buzz.example/buzz/").is_none());
        assert!(origin_from_relay_url("wss://buzz.example\\evil/buzz/").is_none());
        assert!(origin_from_relay_url("wss://buzz.example:notaport/buzz/").is_none());
        assert!(origin_from_relay_url("not a url").is_none());
        assert!(origin_from_relay_url("").is_none());
    }

    #[test]
    fn post_event_refuses_to_follow_http_redirects() {
        use std::io::Write;
        use std::net::{TcpListener, TcpStream};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let target_listener = TcpListener::bind("127.0.0.1:0").expect("bind target");
        let target_port = target_listener.local_addr().expect("target addr").port();
        let target_hit = Arc::new(AtomicBool::new(false));
        let target_hit_worker = Arc::clone(&target_hit);
        let probe_done = Arc::new(AtomicBool::new(false));
        let probe_done_worker = Arc::clone(&probe_done);
        let target_thread = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = target_listener.accept() {
                if !probe_done_worker.load(Ordering::SeqCst) {
                    target_hit_worker.store(true, Ordering::SeqCst);
                }
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let body = "SECRET_INTERNAL_BODY";
                let reply = format!(
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes());
            }
        });

        let origin_listener = TcpListener::bind("127.0.0.1:0").expect("bind origin");
        let origin_port = origin_listener.local_addr().expect("origin addr").port();
        let origin_thread = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = origin_listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let reply = format!(
                    "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:{target_port}/secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.write_all(reply.as_bytes());
            }
        });

        let root = std::env::temp_dir().join(format!(
            "buzz-redirect-test-{}-{}",
            std::process::id(),
            crate::state::now_ms()
        ));
        let home = root.join("home");
        let data = root.join("data");
        let state = AppState::open(&home, &data).expect("open state");
        std::fs::write(
            data.join(ORIGIN_FILE),
            format!("{{\"origin\":\"http://127.0.0.1:{origin_port}\"}}"),
        )
        .expect("write origin");

        let key = AgentKey::generate();
        let event = nostr::profile_event(&key, "Tester", 1_700_000_000).expect("profile event");
        let origin = origin(&state.data_dir).expect("origin");
        let result = post_event(&state, &origin, &key, &event);
        probe_done.store(true, Ordering::SeqCst);

        if !target_hit.load(Ordering::SeqCst) {
            let _ = TcpStream::connect(("127.0.0.1", target_port));
        }
        let _ = origin_thread.join();
        let _ = target_thread.join();
        let _ = std::fs::remove_dir_all(&root);

        let err = result.expect_err("redirect response must not succeed");
        assert!(
            !target_hit.load(Ordering::SeqCst),
            "post_event followed HTTP redirect to internal target; err={err}"
        );
        assert!(
            !err.contains("SECRET_INTERNAL_BODY"),
            "redirected response body leaked into error: {err}"
        );
    }
}

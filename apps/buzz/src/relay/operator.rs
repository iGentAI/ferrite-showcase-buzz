//! The operator plane's vocabulary: who may operate this deployment, which
//! config keys the HTTP face may write, and the chunked UI-bundle upload types.
//!
//! This module is deliberately pure — parsing, validation, and constants only.
//! The I/O (the NIP-98 gate, the config-store writes, the blob assembly) lives
//! in [`crate::relay::ingress`] beside the other route handlers, mirroring the
//! [`crate::relay::agents`] split: decisions here, plumbing there.
//!
//! ## Why this module exists at all
//!
//! The platform's only tenant config channel is deploy-time `set_secret`, and
//! it has no blob-upload tool — so the relay's operator keys (`buzz-agents`,
//! `buzz-community-moderators`, `buzz-join-policy`, …) and its `ui/` asset blob
//! had no write path on a deployed app. The operator route family gives them
//! one, rooted in a `buzz-operators` pubkey set that only the platform owner
//! can establish (the secret plane) or a gate can seed (the config fallback,
//! which the route family itself refuses to write).
//!
//! ## The two security rules everything below serves
//!
//! * **Nothing unvalidated ever reaches the store.** Every writable key has a
//!   validator that either refuses the value or returns the CANONICAL bytes to
//!   store — and "canonical" is defined by the key's live consumer, not by this
//!   module's taste: `buzz-agents` goes through [`crate::relay::agents::Registry::parse`]
//!   (the exact dispatch parser, so the boundary and the consumer can never
//!   drift), and `buzz-community-closed` becomes the literal bytes
//!   `enforce_community_membership` compares.
//! * **Keys not in the table do not exist to this face.** The credentials
//!   (`buzz-operators`, `buzz-relay-identity`, `buzz-stream-token-key`,
//!   `buzz-invite-key`) stay on the owner-gated `set_secret` channel, and every
//!   `/`-containing key (`sess/…`, `replay/…`, `rate/…`, `upload/…`) misses the
//!   exact-match lookup by construction, so an operator write can never clobber
//!   a replay marker or a session row in the same store.

use std::collections::{BTreeMap, BTreeSet};

use ferrite_app::serde::{Deserialize, Serialize};
use ferrite_app::serde_json::{self, Value};

use crate::relay::crypto::sha256::{sha256, sha256_concat};
use crate::relay::crypto::{from_hex32, to_hex};
use crate::relay::event::PubKey;
use crate::relay::{BuzzError, Result};

/// The secret name holding the operator set. Checked FIRST; when the secret
/// exists the config fallback is ignored entirely, never unioned — a config-
/// plane writer must not be able to widen a secret-rooted set.
pub const OPERATOR_SET_SECRET: &str = "buzz-operators";

/// The config-store fallback key for the operator set — the
/// `relay_identity` secret-first/config-fallback pattern, and the only way the
/// severe gates can seed it (`ferrite-app-test` injects an empty secrets map).
/// The write route's own allowlist refuses this key, so the fallback is
/// seedable only from OUTSIDE the HTTP face.
pub const OPERATOR_SET_CONFIG: &[u8] = b"buzz-operators";

/// The config key holding the committed UI-bundle manifest. Written only by
/// the assets `finish` handler — the config route refuses it — and read by
/// `seed_ui_assets` as the "an uploaded bundle is authoritative" watermark.
pub const MANIFEST_CONFIG: &[u8] = b"buzz-web-assets-manifest";
/// [`MANIFEST_CONFIG`] as the table's key string.
pub const MANIFEST_KEY: &str = "buzz-web-assets-manifest";

/// Parse the operator set: a JSON array of 64-hex pubkeys. Strict like
/// [`crate::relay::agents::Registry::parse`] — a malformed document is an error, never
/// a partial set — and an EMPTY array is valid and means "no one operates".
pub fn parse_operator_set(bytes: &[u8]) -> Result<Vec<PubKey>> {
    let entries: Vec<String> = serde_json::from_slice(bytes)
        .map_err(|e| BuzzError::InvalidEvent(format!("buzz-operators: {e}")))?;
    let mut out: Vec<PubKey> = Vec::with_capacity(entries.len());
    for entry in &entries {
        let Some(pk) = from_hex32(entry) else {
            return Err(BuzzError::InvalidEvent(
                "buzz-operators: an entry is not a 32-byte hex pubkey".into(),
            ));
        };
        if !out.contains(&pk) {
            out.push(pk);
        }
    }
    Ok(out)
}

/// One key the operator config route may touch.
pub struct WritableKey {
    /// The exact config-store key (also the route path suffix).
    pub key: &'static str,
    /// Secret-valued keys accept writes but never return their value: reads
    /// and lists redact to `{present, bytes, sha256_16}`.
    pub secret_valued: bool,
    /// `false` for [`MANIFEST_KEY`]: enumerable and readable here, written
    /// only by the assets finish handler.
    pub writable: bool,
    validate: fn(&Value) -> Result<Vec<u8>>,
}

impl WritableKey {
    /// Validate a write body's `value` and return the canonical bytes to
    /// store. Nothing unvalidated ever reaches the store.
    pub fn validate(&self, value: &Value) -> Result<Vec<u8>> {
        (self.validate)(value)
    }
}

/// Every key the operator config family serves. Exact-match lookup only: a key
/// absent here — including every credential and every `/`-prefixed row family —
/// is refused as not-found at the boundary.
pub const WRITABLE_KEYS: &[WritableKey] = &[
    WritableKey {
        key: "buzz-community-moderators",
        secret_valued: false,
        writable: true,
        validate: validate_moderators,
    },
    WritableKey {
        key: "buzz-process-cells",
        secret_valued: false,
        writable: true,
        validate: validate_process_cells,
    },
    WritableKey {
        key: "buzz-agents",
        secret_valued: false,
        writable: true,
        validate: validate_personas,
    },
    WritableKey {
        key: "buzz-agent-harnesses",
        secret_valued: false,
        writable: true,
        validate: validate_harnesses,
    },
    WritableKey {
        key: "buzz-join-policy",
        secret_valued: false,
        writable: true,
        validate: validate_join_policy,
    },
    WritableKey {
        key: "buzz-community-closed",
        secret_valued: false,
        writable: true,
        validate: validate_community_closed,
    },
    WritableKey {
        key: "buzz-relay-url",
        secret_valued: false,
        writable: true,
        validate: validate_relay_url,
    },
    WritableKey {
        key: "buzz-fabric-a2a-endpoint",
        secret_valued: false,
        writable: true,
        validate: validate_fabric_a2a_endpoint,
    },
    WritableKey {
        key: "buzz-nip05-names",
        secret_valued: false,
        writable: true,
        validate: validate_nip05_names,
    },
    WritableKey {
        key: MANIFEST_KEY,
        secret_valued: false,
        writable: false,
        validate: refuse_manifest_write,
    },
];

/// The one ROW FAMILY the operator face may seed: a persona's signing key.
///
/// Everything else in this table is a flat `buzz-*` key. Persona keys are not:
/// their consumer ([`crate::relay::ingress::persona_seckey`]) falls back to
/// `keyring/{community}/{agent}` on the config store, and `/`-keyed families are
/// unreachable from this face by design. But a persona with no key cannot sign,
/// and the only other seeding channel is `set_secret` — which needs a REDEPLOY
/// to inject. That makes registering an agent a minutes-long, charged operation,
/// which defeats the point of having an operator plane at all.
///
/// So the family is reachable under its FLAT name — `buzz-keyring-{agent}` —
/// and this function is the only thing that maps it onto the row the consumer
/// reads. The path never contains a `/`, so the "no `/`-keyed rows" property of
/// the route is preserved verbatim: an operator cannot address
/// `keyring/…` (or `sess/…`, `rate/…`, `upload/…`) even now.
pub const KEYRING_KEY_PREFIX: &str = "buzz-keyring-";

/// The agent id inside a `buzz-keyring-{agent}` operator key, if it is one and
/// the id is a safe component. `None` means "not this family", which the caller
/// turns into the same 404 every unknown key gets.
#[must_use]
pub fn keyring_agent(key: &str) -> Option<&str> {
    let agent = key.strip_prefix(KEYRING_KEY_PREFIX)?;
    // The SAME bound and charset the persona registry enforces on an agent id,
    // so a key can only name an agent that could actually be registered.
    if agent.is_empty() || agent.len() > 64 {
        return None;
    }
    let safe = agent
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    safe.then_some(agent)
}

/// The second flat row family: an agent DEFINITION
/// (docs/_archive/agent-pack-definition.md), reachable as `buzz-agent-def-{agent}` and
/// stored on the row the agent runtime's loader reads. This is the seeding path
/// for definitions on a buzz deployment (the agents app's HTTP face is not
/// routed); without it, personas run on the default surface. Operator-writable,
/// like the registry that names the personas themselves.
pub const AGENT_DEF_KEY_PREFIX: &str = "buzz-agent-def-";

/// The agent id inside a `buzz-agent-def-{agent}` operator key. Narrower than
/// the keyring charset: definition rows use the runtime's own agent-id rule
/// (alphanumeric plus `-`/`_`; a dotted id is a synthesized subagent scope and
/// never has a stored definition).
#[must_use]
pub fn agent_def_agent(key: &str) -> Option<&str> {
    let agent = key.strip_prefix(AGENT_DEF_KEY_PREFIX)?;
    if agent.is_empty() || agent.len() > 64 {
        return None;
    }
    let safe = agent
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
    safe.then_some(agent)
}

/// The stored row for one agent's v1 definition document — the row
/// [`crate::agents::definition::load_agent_definition`] reads (delegated to
/// the loader's own key builder, so the two can never drift).
#[must_use]
pub fn agent_def_row(agent: &str) -> Vec<u8> {
    crate::agents::definition::agent_def_key(agent)
}

/// Validate an agent definition document through the runtime's OWN parser —
/// the boundary and the consumer can never drift — and return the canonical
/// JSON bytes to store.
pub fn validate_agent_definition(agent: &str, value: &Value) -> Result<Vec<u8>> {
    let bytes = canonical_json(value)?;
    if crate::agents::definition::parse_agent_definition(
        &bytes,
        agent,
        crate::agents::turns::MAX_STEPS,
    )
    .is_none()
    {
        return Err(BuzzError::InvalidEvent(format!(
            "buzz-agent-def-{agent}: not a valid agent definition document"
        )));
    }
    Ok(bytes)
}

/// The definition fields a persona's creator or a community moderator may edit
/// from a client: exactly what `POST /buzz/api/agents/config` reads back.
pub const SESSION_DEFINITION_FIELDS: &[&str] = &["instructions", "default_model"];

/// The fields that name the document rather than configure the agent. The
/// runtime's parser checks both, so a merge carries them without judging them.
const IDENTITY_DEFINITION_FIELDS: &[&str] = &["version", "agent_id"];

/// How an update document is applied to a stored agent definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefinitionUpdate {
    /// The shape the web client sends: the SESSION fields are taken exactly as
    /// given (absent means cleared, because that is how the client clears a
    /// model), every other field the document names replaces the stored one,
    /// and every field it does not name is carried over from the stored
    /// definition.
    Set,
    /// A top-level merge patch: a named field replaces the stored one, a field
    /// named as `null` is removed, and an unnamed field is kept.
    Patch,
}

/// Apply `update` to `stored` without losing what the update does not mention.
///
/// A definition carries far more than a client edits — `tool_deny`,
/// `approvals`, `limits`, `subagents`, `skills`, `schedules`, `tools`. Storing
/// the client's two-field document WHOLE would silently remove an agent's deny
/// list and its human approval gates when its model changes. Every client-facing
/// write goes through this merge, and
/// the result is judged by [`validate_agent_definition`] like any other.
///
/// A missing or unreadable stored definition merges onto an empty document
/// that names only `version` and `agent_id`.
pub fn merge_agent_definition(
    agent: &str,
    stored: Option<&Value>,
    update: &Value,
    mode: DefinitionUpdate,
) -> Result<Value> {
    let Some(fields) = update.as_object() else {
        return Err(BuzzError::InvalidEvent(format!(
            "buzz-agent-def-{agent}: an update must be a JSON object"
        )));
    };
    let mut merged = match stored.and_then(Value::as_object) {
        Some(doc) => doc.clone(),
        None => {
            let mut doc = serde_json::Map::new();
            doc.insert(
                "version".into(),
                Value::from(crate::agents::definition::AGENT_DEF_VERSION),
            );
            doc.insert("agent_id".into(), Value::from(agent));
            doc
        }
    };
    if mode == DefinitionUpdate::Set {
        for field in SESSION_DEFINITION_FIELDS {
            if !fields.contains_key(*field) {
                merged.remove(*field);
            }
        }
    }
    for (field, value) in fields {
        if value.is_null() {
            merged.remove(field);
        } else {
            merged.insert(field.clone(), value.clone());
        }
    }
    Ok(Value::Object(merged))
}

/// The configuration fields that differ between `stored` and `merged`, other
/// than the session and identity fields, in name order.
///
/// These are the agent's guardrails and capabilities: its tool allow and deny
/// lists, approval gates, limits, tools, subagents, skills and schedules. Only
/// an operator may change them through the client-facing routes, so a
/// persona's creator or a moderator cannot widen what an operator narrowed.
#[must_use]
pub fn changed_guarded_definition_fields(stored: Option<&Value>, merged: &Value) -> Vec<String> {
    let empty = serde_json::Map::new();
    let before = stored.and_then(Value::as_object).unwrap_or(&empty);
    let after = merged.as_object().unwrap_or(&empty);
    let names: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    names
        .into_iter()
        .filter(|name| {
            !SESSION_DEFINITION_FIELDS.contains(&name.as_str())
                && !IDENTITY_DEFINITION_FIELDS.contains(&name.as_str())
        })
        .filter(|name| before.get(*name) != after.get(*name))
        .cloned()
        .collect()
}

/// A persona signing key: 64 hex chars that are a valid secp256k1 secret.
///
/// Validated to a real key here rather than at signing time, so a mis-seeded
/// persona is one clear 400 to the operator instead of a failure per dispatched
/// turn. Stored RAW because the consumer reads the bytes and hex-decodes them.
/// Secret-valued: writes are accepted, reads redact.
pub fn validate_persona_key(value: &Value) -> Result<Vec<u8>> {
    let Some(hex) = value.as_str() else {
        return Err(BuzzError::InvalidEvent(
            "a persona key must be a 64-hex string".into(),
        ));
    };
    let hex = hex.trim();
    let Some(seckey) = crate::relay::crypto::from_hex32(hex) else {
        return Err(BuzzError::InvalidEvent(
            "a persona key must be 32 bytes of hex".into(),
        ));
    };
    if crate::relay::crypto::schnorr::xonly_pubkey(&seckey).is_none() {
        return Err(BuzzError::InvalidEvent(
            "a persona key must be a valid secp256k1 secret".into(),
        ));
    }
    Ok(hex.to_ascii_lowercase().into_bytes())
}

/// Exact-match lookup into [`WRITABLE_KEYS`]. `None` for everything else —
/// which is the boundary's 404, and what keeps credentials and `/`-keyed rows
/// unreachable by construction.
#[must_use]
pub fn registry_entry(key: &str) -> Option<&'static WritableKey> {
    WRITABLE_KEYS.iter().find(|entry| entry.key == key)
}

/// The most moderators one community can configure.
pub const MAX_MODERATORS: usize = 256;

/// `buzz-community-moderators`: a JSON array of ≤ [`MAX_MODERATORS`] 64-hex
/// pubkeys, deduped and canonicalized to lowercase — the shape
/// `is_community_moderator` string-compares against.
fn validate_moderators(value: &Value) -> Result<Vec<u8>> {
    let Some(entries) = value.as_array() else {
        return Err(BuzzError::InvalidEvent(
            "buzz-community-moderators must be a JSON array of hex pubkeys".into(),
        ));
    };
    if entries.len() > MAX_MODERATORS {
        return Err(BuzzError::InvalidEvent(format!(
            "buzz-community-moderators holds more than {MAX_MODERATORS} entries"
        )));
    }
    let mut out: Vec<String> = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(pk) = entry.as_str().and_then(from_hex32) else {
            return Err(BuzzError::InvalidEvent(
                "buzz-community-moderators: an entry is not a 32-byte hex pubkey".into(),
            ));
        };
        let hex = to_hex(&pk);
        if !out.contains(&hex) {
            out.push(hex);
        }
    }
    canonical_json(&out)
}

/// The most process cells one community can register.
///
/// A cell is a deployed app holding platform authority, not a person; a
/// workspace runs one or two (native RCS, GitHub). The small cap is the point:
/// this list is the whole set of identities whose `KIND_PROCESS_RESULT`
/// events the ingest gate admits.
pub const MAX_PROCESS_CELLS: usize = 8;

/// Parse the process-cell registry STRICTLY — the one reader both the operator
/// write boundary and the ingest authorship gate use, so what the boundary
/// accepts and what the gate trusts can never drift.
///
/// Strict in every direction, because this document IS an authority list:
/// every entry must be a 32-byte hex pubkey, the cap is enforced on the input
/// (before dedup, so a padded document cannot slip past), and ANY fault is an
/// error rather than a partial set. A tolerant reader here would admit the
/// valid entries of a half-corrupt document — which is precisely the
/// "malformed config still authorizes somebody" hole Contract D closes.
///
/// An EMPTY array is valid and means "no process cell is registered", which
/// the gate turns into a refusal of the kind.
pub fn parse_process_cells(bytes: &[u8]) -> Result<Vec<PubKey>> {
    let entries: Vec<Value> = serde_json::from_slice(bytes).map_err(|e| {
        BuzzError::InvalidEvent(format!("buzz-process-cells: not a JSON array: {e}"))
    })?;
    if entries.len() > MAX_PROCESS_CELLS {
        return Err(BuzzError::InvalidEvent(format!(
            "buzz-process-cells holds more than {MAX_PROCESS_CELLS} entries"
        )));
    }
    let mut out: Vec<PubKey> = Vec::with_capacity(entries.len());
    for entry in &entries {
        let Some(pk) = entry.as_str().and_then(from_hex32) else {
            return Err(BuzzError::InvalidEvent(
                "buzz-process-cells: an entry is not a 32-byte hex pubkey".into(),
            ));
        };
        if !out.contains(&pk) {
            out.push(pk);
        }
    }
    Ok(out)
}

/// `buzz-process-cells`: validated by [`parse_process_cells`] — the EXACT
/// reader the ingest gate uses — and canonicalized to the deduped lowercase
/// array that reader will see next time, mirroring the moderator set so the
/// two authority documents cannot drift in format.
fn validate_process_cells(value: &Value) -> Result<Vec<u8>> {
    if !value.is_array() {
        return Err(BuzzError::InvalidEvent(
            "buzz-process-cells must be a JSON array of hex pubkeys".into(),
        ));
    }
    let bytes = canonical_json(value)?;
    let cells = parse_process_cells(&bytes)?;
    canonical_json(
        &cells
            .iter()
            .map(|pk| to_hex(pk.as_slice()))
            .collect::<Vec<String>>(),
    )
}

/// `buzz-agents`: validated by [`crate::relay::agents::Registry::parse`] — the EXACT
/// parser the dispatch and persona paths use, so what this boundary accepts and
/// what the seam executes can never drift. Canonicalized with every entry's
/// harness explicit (the parser's own default filled in).
fn validate_personas(value: &Value) -> Result<Vec<u8>> {
    let bytes = canonical_json(value)?;
    let registry = crate::relay::agents::Registry::parse(&bytes).map_err(|e| match e {
        // The parser reports Internal (its callers own a config they already
        // trusted); at this boundary the same fault is the CLIENT's bad value.
        BuzzError::Internal(msg) => BuzzError::InvalidEvent(msg),
        other => other,
    })?;
    let entries: Vec<Value> = registry
        .personas
        .iter()
        .map(|p| {
            serde_json::json!({
                "agent": p.agent,
                "pubkey": to_hex(&p.pubkey),
                "harness": p.harness,
            })
        })
        .collect();
    canonical_json(&entries)
}

/// The most harness rows one deployment can declare.
pub const MAX_HARNESSES: usize = 32;

/// `buzz-agent-harnesses`: a JSON array of ≤ [`MAX_HARNESSES`] harness rows,
/// each in exactly the shape `GET /buzz/api/agent-harnesses` serves — unknown
/// fields refused, because this document goes to every client verbatim.
fn validate_harnesses(value: &Value) -> Result<Vec<u8>> {
    const KINDS: [&str; 4] = ["cloud", "remote", "a2a", "cli"];
    const AVAILABILITIES: [&str; 3] = ["available", "not_installed", "needs_auth"];
    let refuse = |why: &str| BuzzError::InvalidEvent(format!("buzz-agent-harnesses: {why}"));

    let Some(entries) = value.as_array() else {
        return Err(refuse("must be a JSON array of harness objects"));
    };
    if entries.len() > MAX_HARNESSES {
        return Err(refuse("more than 32 harness rows"));
    }
    let mut seen_ids: BTreeSet<&str> = BTreeSet::new();
    let mut out: Vec<Value> = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(obj) = entry.as_object() else {
            return Err(refuse("a row is not an object"));
        };
        for key in obj.keys() {
            if !matches!(
                key.as_str(),
                "id" | "label"
                    | "kind"
                    | "availability"
                    | "endpoint"
                    | "install_hint"
                    | "auth_status"
            ) {
                return Err(refuse("a row carries an unknown field"));
            }
        }
        let id = obj.get("id").and_then(Value::as_str).unwrap_or_default();
        if id.is_empty()
            || id.len() > 64
            || !id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(refuse("a row's id is not [a-z0-9-]{1,64}"));
        }
        if !seen_ids.insert(id) {
            return Err(refuse("two rows share an id"));
        }
        let label = obj.get("label").and_then(Value::as_str).unwrap_or_default();
        if label.is_empty() || label.chars().count() > 128 {
            return Err(refuse("a row's label is empty or over 128 characters"));
        }
        let kind = obj.get("kind").and_then(Value::as_str).unwrap_or_default();
        if !KINDS.contains(&kind) {
            return Err(refuse("a row's kind is not cloud|remote|a2a|cli"));
        }
        let availability = obj
            .get("availability")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !AVAILABILITIES.contains(&availability) {
            return Err(refuse(
                "a row's availability is not available|not_installed|needs_auth",
            ));
        }
        let endpoint = match obj.get("endpoint") {
            None | Some(Value::Null) => None,
            Some(Value::String(url)) => {
                if url.len() > 512
                    || !is_plausible_url(url)
                    || !(url.starts_with("http://")
                        || url.starts_with("https://")
                        || url.starts_with("ws://")
                        || url.starts_with("wss://"))
                {
                    return Err(refuse("a row's endpoint is not an http(s)/ws(s) URL"));
                }
                // This document is served to ANY client by the public
                // agent-harnesses route, so an endpoint that smuggles a
                // credential in its authority (`https://svc:TOKEN@host/…`)
                // would be published to the world. Refused at write time.
                if authority_has_userinfo(url) {
                    return Err(refuse(
                        "a row's endpoint must not carry userinfo (user:pass@host); \
                         the document is served publicly",
                    ));
                }
                Some(url.clone())
            }
            Some(_) => return Err(refuse("a row's endpoint is not a string")),
        };
        let install_hint = match obj.get("install_hint") {
            None | Some(Value::Null) => None,
            Some(Value::String(hint)) if hint.len() <= 512 => Some(hint.clone()),
            Some(_) => return Err(refuse("a row's install_hint is not a string ≤512 bytes")),
        };
        let auth_status = match obj.get("auth_status") {
            None | Some(Value::Null) => None,
            Some(status @ Value::Object(_)) => Some(status.clone()),
            Some(_) => return Err(refuse("a row's auth_status is not an object")),
        };
        out.push(serde_json::json!({
            "id": id,
            "label": label,
            "kind": kind,
            "availability": availability,
            "endpoint": endpoint,
            "install_hint": install_hint,
            "auth_status": auth_status,
        }));
    }
    let bytes = canonical_json(&out)?;
    if bytes.len() > 64 * 1024 {
        return Err(refuse("document is over 64 KiB"));
    }
    Ok(bytes)
}

/// `buzz-join-policy`: an object with ONLY the fields the claim path reads.
/// An unknown top-level field is refused rather than stored — a policy the
/// consumer silently ignores half of is an operator surprise, not a feature.
/// `version` is canonicalized to a string because the acceptance receipt is
/// HMAC-bound to `policy["version"].as_str()`: a numeric version would bind
/// every receipt to `""` without anyone noticing.
fn validate_join_policy(value: &Value) -> Result<Vec<u8>> {
    let refuse = |why: &str| BuzzError::InvalidEvent(format!("buzz-join-policy: {why}"));
    let Some(obj) = value.as_object() else {
        return Err(refuse("must be a JSON object"));
    };
    let mut out = serde_json::Map::new();
    for (key, field) in obj {
        match key.as_str() {
            "version" => {
                let version = match field {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    _ => return Err(refuse("version must be a string or number")),
                };
                if version.len() > 128 {
                    return Err(refuse("version is over 128 bytes"));
                }
                out.insert("version".into(), Value::String(version));
            }
            "terms_markdown" | "privacy_markdown" => {
                let Some(text) = field.as_str() else {
                    return Err(refuse("markdown fields must be strings"));
                };
                if text.len() > 32 * 1024 {
                    return Err(refuse("a markdown field is over 32 KiB"));
                }
                out.insert(key.clone(), field.clone());
            }
            "age_attestation_required" => {
                if !field.is_boolean() {
                    return Err(refuse("age_attestation_required must be a boolean"));
                }
                out.insert(key.clone(), field.clone());
            }
            _ => return Err(refuse("unknown top-level field")),
        }
    }
    canonical_json(&Value::Object(out))
}

/// `buzz-community-closed`: JSON `true`/`false` (or the strings `"true"` /
/// `"false"`), canonicalized to the literal stored bytes `b"true"` / `b"false"`
/// — byte-identical to what `enforce_community_membership` trims and compares,
/// so the write and the gate can never disagree about what "closed" is spelled.
fn validate_community_closed(value: &Value) -> Result<Vec<u8>> {
    let closed = match value {
        Value::Bool(b) => *b,
        Value::String(s) if s == "true" => true,
        Value::String(s) if s == "false" => false,
        _ => {
            return Err(BuzzError::InvalidEvent(
                "buzz-community-closed must be true or false".into(),
            ))
        }
    };
    Ok(if closed {
        b"true".to_vec()
    } else {
        b"false".to_vec()
    })
}

/// `buzz-relay-url`: one trimmed ws(s)/http(s) URL, ≤512 bytes, no whitespace
/// or control bytes. Stored as the RAW string — the NIP-42 host check reads the
/// bytes with `from_utf8_lossy(..).trim()`, so a JSON-quoted form would put
/// literal quotes into the URL it compares against.
fn validate_relay_url(value: &Value) -> Result<Vec<u8>> {
    let refuse = |why: &str| BuzzError::InvalidEvent(format!("buzz-relay-url: {why}"));
    let Some(url) = value.as_str() else {
        return Err(refuse("must be a string"));
    };
    let url = url.trim();
    if url.is_empty() || url.len() > 512 {
        return Err(refuse("must be 1–512 bytes"));
    }
    if !is_plausible_url(url) {
        return Err(refuse("carries whitespace or control bytes"));
    }
    if !(url.starts_with("ws://")
        || url.starts_with("wss://")
        || url.starts_with("http://")
        || url.starts_with("https://"))
    {
        return Err(refuse("scheme must be ws, wss, http, or https"));
    }
    Ok(url.as_bytes().to_vec())
}

/// `buzz-fabric-a2a-endpoint`: one credential-free HTTPS URL, stored as raw
/// text for the A2A client. The manifest remains the final destination bound;
/// this value merely selects one of the HTTPS prefixes the deployment grants.
fn validate_fabric_a2a_endpoint(value: &Value) -> Result<Vec<u8>> {
    let refuse = |why: &str| BuzzError::InvalidEvent(format!("buzz-fabric-a2a-endpoint: {why}"));
    let Some(url) = value.as_str() else {
        return Err(refuse("must be a string"));
    };
    let url = url.trim();
    if url.is_empty() || url.len() > 512 {
        return Err(refuse("must be 1–512 bytes"));
    }
    if !is_plausible_url(url) || !url.starts_with("https://") {
        return Err(refuse("must be an HTTPS URL without whitespace or control bytes"));
    }
    if authority_has_userinfo(url) {
        return Err(refuse("must not carry userinfo (user:pass@host)"));
    }
    Ok(url.as_bytes().to_vec())
}

/// Printable, single-token URL text: no whitespace, no control bytes, ASCII.
fn is_plausible_url(url: &str) -> bool {
    !url.is_empty() && url.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// Whether a URL's authority component carries userinfo (`user:pass@host`) —
/// an `@` before the first `/`, `?`, or `#` after the `//`.
pub(crate) fn authority_has_userinfo(url: &str) -> bool {
    let Some(start) = url.find("//") else {
        return false;
    };
    url.get(start + 2..)
        .unwrap_or_default()
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .contains('@')
}

/// The most NIP-05 names one deployment can map.
pub const MAX_NIP05_NAMES: usize = 512;

/// `buzz-nip05-names`: a JSON object of ≤ [`MAX_NIP05_NAMES`] entries mapping
/// lowercase `[a-z0-9-_.]{1,64}` names to 64-hex pubkeys, canonicalized to the
/// sorted-key object the `nip05` route deserializes.
fn validate_nip05_names(value: &Value) -> Result<Vec<u8>> {
    let refuse = |why: &str| BuzzError::InvalidEvent(format!("buzz-nip05-names: {why}"));
    let Some(obj) = value.as_object() else {
        return Err(refuse("must be a JSON object of name → pubkey"));
    };
    if obj.len() > MAX_NIP05_NAMES {
        return Err(refuse("more than 512 names"));
    }
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for (name, pubkey) in obj {
        if name.is_empty()
            || name.len() > 64
            || !name.bytes().all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_' || b == b'.'
            })
        {
            return Err(refuse("a name is not lowercase [a-z0-9-_.]{1,64}"));
        }
        let Some(pk) = pubkey.as_str().and_then(from_hex32) else {
            return Err(refuse("a value is not a 32-byte hex pubkey"));
        };
        out.insert(name.clone(), to_hex(&pk));
    }
    let bytes = canonical_json(&out)?;
    if bytes.len() > 64 * 1024 {
        return Err(refuse("document is over 64 KiB"));
    }
    Ok(bytes)
}

/// [`MANIFEST_KEY`] is in the table so lists and reads can see it, but only the
/// assets finish handler writes it: a manifest naming blobs that were never
/// verified would defeat the whole integrity chain.
fn refuse_manifest_write(_value: &Value) -> Result<Vec<u8>> {
    Err(BuzzError::InvalidEvent(
        "buzz-web-assets-manifest is written only by the assets finish step".into(),
    ))
}

/// Serialize a canonical JSON document (the stored form for JSON-valued keys).
fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|e| BuzzError::Internal(format!("canonical encode: {e}")))
}

/// First 16 hex characters of a value's SHA-256 — the redacted fingerprint
/// lists, reads, and audit entries carry instead of the value.
#[must_use]
pub fn sha256_16(bytes: &[u8]) -> String {
    let mut hex = to_hex(&sha256(bytes));
    hex.truncate(16);
    hex
}

/// One raw chunk: 128 KiB. Base64 in a JSON body inflates that to ~171 KiB,
/// safely under the gateway's 256 KiB serialized ingress bound with zero
/// JSON-escaping ambiguity (base64 never needs escaping).
pub const CHUNK_BYTES: usize = 131_072;
/// The most files one bundle may name (the real bundle is ~500).
pub const MAX_BUNDLE_FILES: usize = 2048;
/// Per-file cap: headroom under the 16 MiB fleet-default blob object bound
/// (the largest real file today is 4.3 MB).
pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Whole-bundle cap (the real bundle is ~23 MB).
pub const MAX_BUNDLE_BYTES: u64 = 64 * 1024 * 1024;
/// How long an upload session and its staged chunks live. Expiry IS the
/// cleanup: an abandoned upload self-erases with no sweeper, which is why the
/// staging rows live in the config store rather than the blob plane (no TTL,
/// no scan there).
pub const UPLOAD_TTL_MS: u64 = 2 * 60 * 60 * 1000;

/// One file a bundle promises: where it goes, how big it is, what it hashes to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct ManifestFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// One in-flight upload, written with a TTL at `keys::upload(id)`.
///
/// **Written ONCE, at `begin`, and never rewritten.** A growing `done` set here
/// would make every finished file a read-modify-write of a document naming all
/// ~500 files: quadratic bytes, and a compare-and-swap that concurrent file
/// workers contend on. Per-file completion lives in its own row,
/// `keys::updone(id, path_sha16)`, so a chunk write is O(1) and `finish` reads
/// the completions back in batches.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct UploadSession {
    pub files: Vec<ManifestFile>,
    /// Whether finish deletes blob keys the new manifest no longer names.
    pub prune: bool,
    /// Hex pubkey of the operator who began the upload.
    pub by: String,
    pub created_ms: u64,
    /// Paths the CURRENT manifest already covers byte-identically, decided once
    /// at `begin`. These need no upload and no per-path row — writing ~500 of
    /// them at `begin` for an incremental push (where almost everything is
    /// unchanged) would just move the quadratic cost to the other end.
    ///
    /// Named `pre_done` rather than `done` precisely because it is not the live
    /// completion set: `finish` takes the union of this and the `updone/` rows.
    #[serde(default, alias = "done")]
    pub pre_done: BTreeSet<String>,
}

impl UploadSession {
    /// When this session and every row staged under it expire. Derived from
    /// `created_ms`, never from "now", so a late chunk cannot extend an upload's
    /// life indefinitely.
    #[must_use]
    pub const fn expires_at_ms(&self) -> u64 {
        self.created_ms.saturating_add(UPLOAD_TTL_MS)
    }

    /// The paths this session still needs a verified `updone/` row for — the
    /// exact set `finish` has to prove, and the only reads it has to make.
    #[must_use]
    pub fn awaiting_verification(&self) -> Vec<&str> {
        self.files
            .iter()
            .map(|f| f.path.as_str())
            .filter(|path| !self.pre_done.contains(*path))
            .collect()
    }
}

/// The committed bundle: what `seed_ui_assets` defers to and what the next
/// upload's `begin` diffs against.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct Manifest {
    /// SHA-256 over the sorted `path:sha256` lines — one stable id per bundle.
    pub etag: String,
    pub files: Vec<ManifestFile>,
    pub committed_ms: u64,
    /// Hex pubkey of the committing operator.
    pub by: String,
}

/// Validate one asset path: `[A-Za-z0-9._-]+` segments joined by `/`, no
/// leading `/`, no `.` or `..` segment, ≤256 bytes. Strictly narrower than
/// [`crate::relay::keys::ui_asset`] (which serves whatever an old seeding wrote);
/// what comes IN through the upload route is held to the tighter rule.
pub fn validate_asset_path(path: &str) -> Result<()> {
    let refuse = || BuzzError::InvalidEvent("an asset path is not a safe relative path".into());
    if path.is_empty() || path.len() > 256 {
        return Err(refuse());
    }
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(refuse());
        }
        if !segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
        {
            return Err(refuse());
        }
    }
    Ok(())
}

/// Validate a whole bundle manifest and return it canonicalized (lowercase
/// hashes). Refuses: no files, too many files, duplicate paths, unsafe paths,
/// oversized files, an oversized total, or a malformed digest.
pub fn validate_bundle(files: &[ManifestFile]) -> Result<Vec<ManifestFile>> {
    if files.is_empty() {
        return Err(BuzzError::InvalidEvent(
            "a bundle needs at least one file".into(),
        ));
    }
    if files.len() > MAX_BUNDLE_FILES {
        return Err(BuzzError::InvalidEvent(format!(
            "a bundle may name at most {MAX_BUNDLE_FILES} files"
        )));
    }
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut total: u64 = 0;
    let mut out = Vec::with_capacity(files.len());
    for file in files {
        validate_asset_path(&file.path)?;
        if !seen.insert(&file.path) {
            return Err(BuzzError::InvalidEvent(
                "a bundle names the same path twice".into(),
            ));
        }
        if file.size > MAX_FILE_BYTES {
            return Err(BuzzError::InvalidEvent(format!(
                "a file is over the {MAX_FILE_BYTES}-byte cap"
            )));
        }
        total = total.saturating_add(file.size);
        let Some(digest) = from_hex32(&file.sha256) else {
            return Err(BuzzError::InvalidEvent(
                "a file's sha256 is not a 32-byte hex digest".into(),
            ));
        };
        out.push(ManifestFile {
            path: file.path.clone(),
            size: file.size,
            sha256: to_hex(&digest),
        });
    }
    if total > MAX_BUNDLE_BYTES {
        return Err(BuzzError::InvalidEvent(format!(
            "the bundle is over the {MAX_BUNDLE_BYTES}-byte cap"
        )));
    }
    Ok(out)
}

/// The bundle's etag: SHA-256 over the sorted `path:sha256` lines. Sorted so
/// the same set of files always names the same bundle whatever order a client
/// listed them in.
#[must_use]
pub fn manifest_etag(files: &[ManifestFile]) -> String {
    let mut lines: Vec<String> = files
        .iter()
        .map(|f| format!("{}:{}", f.path, f.sha256))
        .collect();
    lines.sort();
    let mut h = crate::relay::crypto::sha256::Sha256::new();
    for line in &lines {
        h.update(line.as_bytes());
        h.update(b"\n");
    }
    to_hex(&h.finalize())
}

/// The fixed-width staging-key component for one path: first 16 hex of its
/// SHA-256, so an arbitrary-length path never inflates or breaks a store key.
#[must_use]
pub fn path_sha16(path: &str) -> String {
    sha256_16(path.as_bytes())
}

/// How many chunks a file of `size` bytes stages (a zero-byte file is one
/// empty chunk, so it still has something to verify and name).
#[must_use]
pub fn chunk_count(size: u64) -> u64 {
    core::cmp::max(1, size.div_ceil(CHUNK_BYTES as u64))
}

/// The chunk index a `(size, offset)` pair addresses. Refuses a misaligned
/// offset and an offset outside the file, so a hostile chunk can neither shear
/// the assembly nor stage rows past the manifest's promise.
pub fn chunk_index(size: u64, offset: u64) -> Result<u64> {
    let chunk = CHUNK_BYTES as u64;
    if !offset.is_multiple_of(chunk) {
        return Err(BuzzError::InvalidEvent(format!(
            "chunk offset must be a multiple of {CHUNK_BYTES}"
        )));
    }
    if offset >= size && !(offset == 0 && size == 0) {
        return Err(BuzzError::InvalidEvent(
            "chunk offset is outside the manifest's size for this file".into(),
        ));
    }
    Ok(offset / chunk)
}

/// Mint an upload id: 16 hex of a digest over the request's correlation id,
/// the instant, and the caller — the same "no entropy source, so derive from
/// per-request uniqueness" rule the invite nonce uses.
#[must_use]
pub fn upload_id(correlation_id: &str, now_ms: u64, by: &PubKey) -> String {
    sha256_16(&sha256_concat(&[
        correlation_id.as_bytes(),
        &now_ms.to_be_bytes(),
        by,
    ]))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const PK_HEX: &str = "0101010101010101010101010101010101010101010101010101010101010101";
    const PK2_HEX: &str = "0202020202020202020202020202020202020202020202020202020202020202";

    fn validate(key: &str, value: &Value) -> Result<Vec<u8>> {
        registry_entry(key)
            .expect("key is in the table")
            .validate(value)
    }

    #[test]
    fn operator_set_parses_strictly() {
        assert_eq!(parse_operator_set(b"[]").unwrap(), Vec::<PubKey>::new());
        let set = parse_operator_set(format!(r#"["{PK_HEX}"]"#).as_bytes()).unwrap();
        assert_eq!(set, vec![[0x01; 32]]);
        // Dedup, and uppercase canonicalizes onto the same key.
        let set =
            parse_operator_set(format!(r#"["{PK_HEX}","{}"]"#, PK_HEX.to_uppercase()).as_bytes())
                .unwrap();
        assert_eq!(set.len(), 1);
        for bad in [
            &b"not json"[..],
            br#"{"a":1}"#,
            br#"["zz"]"#,
            br#"["abcd"]"#,
            br#"[1]"#,
            br#"[null]"#,
        ] {
            assert!(parse_operator_set(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn credentials_and_slash_keys_are_not_in_the_table() {
        for absent in [
            "buzz-operators",
            "buzz-relay-identity",
            "buzz-stream-token-key",
            "buzz-invite-key",
            "sess/main/s-1",
            "replay/main/deadbeef",
            "rate/main/pk/msg",
            "upload/abc",
            "buzz-agents/extra",
            "",
        ] {
            assert!(registry_entry(absent).is_none(), "served {absent:?}");
        }
        // ...while every declared key resolves.
        for entry in WRITABLE_KEYS {
            assert!(registry_entry(entry.key).is_some());
        }
    }

    #[test]
    fn the_manifest_key_is_readable_but_never_writable() {
        let entry = registry_entry(MANIFEST_KEY).unwrap();
        assert!(!entry.writable);
        assert!(entry.validate(&serde_json::json!({"etag": "x"})).is_err());
    }

    #[test]
    fn the_agent_def_family_maps_and_validates() {
        // Family resolution mirrors the keyring family, on the runtime's
        // narrower agent-id charset.
        assert_eq!(agent_def_agent("buzz-agent-def-revka"), Some("revka"));
        assert_eq!(agent_def_agent("buzz-agent-def-a_b-2"), Some("a_b-2"));
        for bad in [
            "buzz-agent-def-",
            "buzz-agent-def-a.b",
            "buzz-agent-def-a/b",
            "buzz-keyring-x",
            "buzz-agents",
        ] {
            assert!(agent_def_agent(bad).is_none(), "accepted {bad:?}");
        }
        // The stored row is the loader's own key (pinned against drift).
        assert_eq!(agent_def_row("revka"), b"agent-def:v1:revka".to_vec());
        // Validation runs the runtime's parser: a valid document canonicalizes,
        // a malformed one is the client's 400.
        assert!(validate_agent_definition(
            "revka",
            &serde_json::json!({ "version": 1, "agent_id": "revka", "default_model": "claude-sonnet-5" }),
        )
        .is_ok());
        assert!(validate_agent_definition("revka", &serde_json::json!({ "version": 99 })).is_err());
        assert!(validate_agent_definition("revka", &serde_json::json!("not an object")).is_err());
    }

    fn guarded_definition() -> Value {
        serde_json::json!({
            "version": 1,
            "agent_id": "revka",
            "instructions": "Be brief.",
            "default_model": "claude-sonnet-5",
            "tool_deny": ["shell"],
            "approvals": ["send_email"],
            "limits": { "max_steps": 4 },
            "skills": ["triage"],
        })
    }

    #[test]
    fn a_session_edit_keeps_every_field_it_does_not_name() {
        // F163: the web client's two-field document, exactly as it sends it.
        let stored = guarded_definition();
        let update = serde_json::json!({
            "version": 1,
            "agent_id": "revka",
            "default_model": "claude-haiku-4-5",
            "instructions": "Be very brief.",
        });
        let merged =
            merge_agent_definition("revka", Some(&stored), &update, DefinitionUpdate::Set).unwrap();
        assert_eq!(merged["default_model"], "claude-haiku-4-5");
        assert_eq!(merged["instructions"], "Be very brief.");
        for kept in ["tool_deny", "approvals", "limits", "skills"] {
            assert_eq!(merged[kept], stored[kept], "{kept} was dropped");
        }
        assert!(changed_guarded_definition_fields(Some(&stored), &merged).is_empty());
        assert!(validate_agent_definition("revka", &merged).is_ok());
    }

    #[test]
    fn set_clears_an_unnamed_session_field_and_patch_keeps_it() {
        let stored = guarded_definition();
        // The client clears a model by leaving it out of `set`.
        let cleared = merge_agent_definition(
            "revka",
            Some(&stored),
            &serde_json::json!({ "version": 1, "agent_id": "revka", "instructions": "x" }),
            DefinitionUpdate::Set,
        )
        .unwrap();
        assert!(cleared.get("default_model").is_none());
        assert_eq!(cleared["tool_deny"], stored["tool_deny"]);
        // A patch keeps what it does not name and removes what it names as null.
        let patched = merge_agent_definition(
            "revka",
            Some(&stored),
            &serde_json::json!({ "instructions": null, "default_model": "claude-haiku-4-5" }),
            DefinitionUpdate::Patch,
        )
        .unwrap();
        assert!(patched.get("instructions").is_none());
        assert_eq!(patched["default_model"], "claude-haiku-4-5");
        assert_eq!(patched["approvals"], stored["approvals"]);
    }

    #[test]
    fn changing_a_guardrail_is_reported_by_name() {
        let stored = guarded_definition();
        let wiped = merge_agent_definition(
            "revka",
            Some(&stored),
            &serde_json::json!({ "tool_deny": [], "approvals": null }),
            DefinitionUpdate::Patch,
        )
        .unwrap();
        assert_eq!(
            changed_guarded_definition_fields(Some(&stored), &wiped),
            vec!["approvals".to_string(), "tool_deny".to_string()]
        );
        // A missing stored definition merges onto an empty document, and adding
        // a guardrail to it is still a guarded change.
        let fresh = merge_agent_definition(
            "revka",
            None,
            &serde_json::json!({ "limits": { "max_steps": 2 } }),
            DefinitionUpdate::Patch,
        )
        .unwrap();
        assert_eq!(fresh["agent_id"], "revka");
        assert_eq!(
            changed_guarded_definition_fields(None, &fresh),
            vec!["limits".to_string()]
        );
        assert!(merge_agent_definition(
            "revka",
            None,
            &serde_json::json!("nope"),
            DefinitionUpdate::Set
        )
        .is_err());
    }

    #[test]
    fn moderators_canonicalize_and_refuse_junk() {
        let bytes = validate(
            "buzz-community-moderators",
            &serde_json::json!([PK_HEX.to_uppercase(), PK_HEX, PK2_HEX]),
        )
        .unwrap();
        // Deduped, lowercased — the exact shape is_community_moderator compares.
        let list: Vec<String> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(list, vec![PK_HEX.to_string(), PK2_HEX.to_string()]);

        for bad in [
            serde_json::json!("not an array"),
            serde_json::json!({"pk": PK_HEX}),
            serde_json::json!(["zz"]),
            serde_json::json!([12]),
            serde_json::json!([PK_HEX, "abcd"]),
        ] {
            assert!(
                validate("buzz-community-moderators", &bad).is_err(),
                "{bad}"
            );
        }
        // The cap bounds the INPUT array (before dedup), so 256 entries pass
        // and 257 refuse whether or not they repeat.
        let at_cap: Vec<String> = (0..MAX_MODERATORS)
            .map(|i| to_hex(&sha256(&(i as u64).to_be_bytes())))
            .collect();
        assert!(validate("buzz-community-moderators", &serde_json::json!(at_cap)).is_ok());
        let over: Vec<String> = (0..=MAX_MODERATORS)
            .map(|i| to_hex(&sha256(&(i as u64).to_be_bytes())))
            .collect();
        assert!(validate("buzz-community-moderators", &serde_json::json!(over)).is_err());
    }

    #[test]
    fn personas_go_through_the_dispatch_parser() {
        let bytes = validate(
            "buzz-agents",
            &serde_json::json!([{ "agent": "revka", "pubkey": PK_HEX }]),
        )
        .unwrap();
        // The canonical form is re-parseable by the SAME parser and fills the
        // default harness in explicitly.
        let registry = crate::relay::agents::Registry::parse(&bytes).unwrap();
        assert_eq!(registry.personas.len(), 1);
        assert_eq!(
            registry.personas[0].harness,
            crate::relay::agents::DEFAULT_HARNESS
        );

        for bad in [
            serde_json::json!([{ "agent": "x", "pubkey": "zz" }]),
            serde_json::json!([{ "agent": "", "pubkey": PK_HEX }]),
            serde_json::json!("not an array"),
        ] {
            assert!(validate("buzz-agents", &bad).is_err(), "{bad}");
        }
        // An empty registry is a valid "no personas".
        assert!(validate("buzz-agents", &serde_json::json!([])).is_ok());
    }

    #[test]
    fn harnesses_validate_shape_and_vocabulary() {
        let good = serde_json::json!([{
            "id": "ferrite-cloud",
            "label": "Ferrite Cloud",
            "kind": "cloud",
            "availability": "available",
            "endpoint": "https://example.test/agents",
            "auth_status": { "status": "not_applicable" },
        }]);
        assert!(validate("buzz-agent-harnesses", &good).is_ok());

        for bad in [
            serde_json::json!([{ "id": "UPPER", "label": "x", "kind": "cloud", "availability": "available" }]),
            serde_json::json!([{ "id": "a", "label": "", "kind": "cloud", "availability": "available" }]),
            serde_json::json!([{ "id": "a", "label": "x", "kind": "warp", "availability": "available" }]),
            serde_json::json!([{ "id": "a", "label": "x", "kind": "cloud", "availability": "maybe" }]),
            serde_json::json!([{ "id": "a", "label": "x", "kind": "cloud", "availability": "available",
                     "endpoint": "ftp://x" }]),
            // The document is served publicly, so a credential smuggled into
            // the endpoint's authority is refused at write time.
            serde_json::json!([{ "id": "a", "label": "x", "kind": "a2a", "availability": "available",
                     "endpoint": "https://svc:TOKEN@fabric.internal/a2a/" }]),
            serde_json::json!([{ "id": "a", "label": "x", "kind": "cloud", "availability": "available",
                     "surprise": true }]),
            serde_json::json!([
                { "id": "a", "label": "x", "kind": "cloud", "availability": "available" },
                { "id": "a", "label": "y", "kind": "cli", "availability": "needs_auth" },
            ]),
            serde_json::json!("not an array"),
        ] {
            assert!(validate("buzz-agent-harnesses", &bad).is_err(), "{bad}");
        }
        let too_many: Vec<Value> = (0..=MAX_HARNESSES)
            .map(|i| {
                serde_json::json!({ "id": format!("h{i}"), "label": "x", "kind": "cli",
                        "availability": "not_installed" })
            })
            .collect();
        assert!(validate("buzz-agent-harnesses", &serde_json::json!(too_many)).is_err());
    }

    #[test]
    fn join_policy_refuses_unknown_fields_and_stringifies_version() {
        let bytes = validate(
            "buzz-join-policy",
            &serde_json::json!({ "version": 3, "terms_markdown": "# hi", "age_attestation_required": true }),
        )
        .unwrap();
        let policy: Value = serde_json::from_slice(&bytes).unwrap();
        // Numeric versions become strings, because the acceptance receipt is
        // bound to `version.as_str()` and a number would bind to "".
        assert_eq!(policy.get("version").and_then(Value::as_str), Some("3"));

        for bad in [
            serde_json::json!({ "version": "1", "surprise": "field" }),
            serde_json::json!({ "terms_markdown": 7 }),
            serde_json::json!({ "age_attestation_required": "yes" }),
            serde_json::json!({ "terms_markdown": "x".repeat(32 * 1024 + 1) }),
            serde_json::json!("not an object"),
            serde_json::json!(["not", "an", "object"]),
        ] {
            assert!(validate("buzz-join-policy", &bad).is_err(), "{bad}");
        }
        // An empty policy object is valid (it still forces the receipt flow).
        assert!(validate("buzz-join-policy", &serde_json::json!({})).is_ok());
    }

    #[test]
    fn community_closed_canonicalizes_to_the_gate_bytes() {
        assert_eq!(
            validate("buzz-community-closed", &serde_json::json!(true)).unwrap(),
            b"true"
        );
        assert_eq!(
            validate("buzz-community-closed", &serde_json::json!(false)).unwrap(),
            b"false"
        );
        assert_eq!(
            validate("buzz-community-closed", &serde_json::json!("true")).unwrap(),
            b"true"
        );
        assert_eq!(
            validate("buzz-community-closed", &serde_json::json!("false")).unwrap(),
            b"false"
        );
        for bad in [serde_json::json!("banana"), serde_json::json!(1), serde_json::json!(null), serde_json::json!("True")] {
            assert!(validate("buzz-community-closed", &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn relay_url_is_scheme_checked_and_stored_raw() {
        assert_eq!(
            validate("buzz-relay-url", &serde_json::json!("  wss://relay.example/buzz/ ")).unwrap(),
            b"wss://relay.example/buzz/"
        );
        for scheme in ["ws", "wss", "http", "https"] {
            assert!(validate("buzz-relay-url", &serde_json::json!(format!("{scheme}://h"))).is_ok());
        }
        let giant = format!("wss://{}", "a".repeat(600));
        for bad in [
            serde_json::json!("ftp://x"),
            serde_json::json!("relay.example"),
            serde_json::json!("wss://a b"),
            serde_json::json!("wss://a\nb"),
            serde_json::json!(""),
            serde_json::json!(giant),
            serde_json::json!(42),
        ] {
            assert!(validate("buzz-relay-url", &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn fabric_a2a_endpoint_is_https_credential_free_and_stored_raw() {
        assert_eq!(
            validate(
                "buzz-fabric-a2a-endpoint",
                &serde_json::json!("  https://fabric.staging.example/a2a/ "),
            )
            .unwrap(),
            b"https://fabric.staging.example/a2a/"
        );
        let giant = format!("https://{}", "a".repeat(600));
        for bad in [
            serde_json::json!("http://fabric.example/a2a/"),
            serde_json::json!("wss://fabric.example/a2a/"),
            serde_json::json!("https://user:token@fabric.example/a2a/"),
            serde_json::json!("https://fabric.example/a 2a/"),
            serde_json::json!(""),
            serde_json::json!(giant),
            serde_json::json!(42),
        ] {
            assert!(
                validate("buzz-fabric-a2a-endpoint", &bad).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn nip05_names_are_lowercase_and_hex_valued() {
        let bytes = validate(
            "buzz-nip05-names",
            &serde_json::json!({ "bo.nilsson_2": PK_HEX.to_uppercase() }),
        )
        .unwrap();
        let map: BTreeMap<String, String> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(map.get("bo.nilsson_2").map(String::as_str), Some(PK_HEX));

        for bad in [
            serde_json::json!({ "Bo": PK_HEX }),
            serde_json::json!({ "a/b": PK_HEX }),
            serde_json::json!({ "bo": "zz" }),
            serde_json::json!({ "": PK_HEX }),
            serde_json::json!({ "a".repeat(65): PK_HEX }),
            serde_json::json!(["bo", PK_HEX]),
        ] {
            assert!(validate("buzz-nip05-names", &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn asset_paths_refuse_traversal_and_junk() {
        for good in [
            "index.html",
            "assets/honey-Beh0Z_j5.js",
            "a/b/c.map",
            "_x/-y/z.",
        ] {
            assert!(validate_asset_path(good).is_ok(), "refused {good:?}");
        }
        for bad in [
            "",
            "/abs",
            "trailing/",
            "a//b",
            "../x",
            "a/../b",
            "a/./b",
            ".",
            "..",
            "with space.js",
            "back\\slash",
            "quo\"te",
            "semi;colon",
            "café.js",
        ] {
            assert!(validate_asset_path(bad).is_err(), "accepted {bad:?}");
        }
        assert!(validate_asset_path(&"a".repeat(257)).is_err());
        assert!(validate_asset_path(&"a".repeat(256)).is_ok());
    }

    #[test]
    fn bundles_are_bounded_and_canonicalized() {
        let file = |path: &str, size: u64| ManifestFile {
            path: path.into(),
            size,
            sha256: PK_HEX.to_uppercase(),
        };
        let out = validate_bundle(&[file("index.html", 10)]).unwrap();
        assert_eq!(out[0].sha256, PK_HEX, "hashes canonicalize to lowercase");

        assert!(validate_bundle(&[]).is_err());
        assert!(validate_bundle(&[file("../x", 1)]).is_err());
        assert!(validate_bundle(&[file("a", 1), file("a", 1)]).is_err());
        assert!(validate_bundle(&[file("a", MAX_FILE_BYTES + 1)]).is_err());
        assert!(validate_bundle(&[ManifestFile {
            path: "a".into(),
            size: 1,
            sha256: "zz".into(),
        }])
        .is_err());
        let too_many: Vec<ManifestFile> = (0..=MAX_BUNDLE_FILES)
            .map(|i| file(&format!("f{i}"), 1))
            .collect();
        assert!(validate_bundle(&too_many).is_err());
        // Nine 8 MiB files exceed the 64 MiB bundle cap.
        let too_big: Vec<ManifestFile> = (0..9)
            .map(|i| file(&format!("f{i}"), MAX_FILE_BYTES))
            .collect();
        assert!(validate_bundle(&too_big).is_err());
    }

    #[test]
    fn etags_are_order_independent_and_content_sensitive() {
        let a = ManifestFile {
            path: "a".into(),
            size: 1,
            sha256: PK_HEX.into(),
        };
        let b = ManifestFile {
            path: "b".into(),
            size: 2,
            sha256: PK2_HEX.into(),
        };
        assert_eq!(
            manifest_etag(&[a.clone(), b.clone()]),
            manifest_etag(&[b.clone(), a.clone()])
        );
        let mut a2 = a.clone();
        a2.sha256 = PK2_HEX.into();
        assert_ne!(manifest_etag(&[a, b.clone()]), manifest_etag(&[a2, b]));
    }

    #[test]
    fn chunk_math_covers_the_edges() {
        assert_eq!(chunk_count(0), 1);
        assert_eq!(chunk_count(1), 1);
        assert_eq!(chunk_count(CHUNK_BYTES as u64), 1);
        assert_eq!(chunk_count(CHUNK_BYTES as u64 + 1), 2);
        assert_eq!(chunk_index(CHUNK_BYTES as u64 + 1, 0).unwrap(), 0);
        assert_eq!(
            chunk_index(CHUNK_BYTES as u64 + 1, CHUNK_BYTES as u64).unwrap(),
            1
        );
        // A zero-byte file has exactly one (empty) chunk at offset 0.
        assert_eq!(chunk_index(0, 0).unwrap(), 0);
        // Misaligned and out-of-range offsets are refused.
        assert!(chunk_index(1_000_000, 3).is_err());
        assert!(chunk_index(10, CHUNK_BYTES as u64).is_err());
        assert!(chunk_index(0, CHUNK_BYTES as u64).is_err());
    }

    #[test]
    fn upload_ids_differ_per_request_and_caller() {
        let a = upload_id("corr-1", 1_000, &[1; 32]);
        assert_eq!(a.len(), 16);
        assert_ne!(a, upload_id("corr-2", 1_000, &[1; 32]));
        assert_ne!(a, upload_id("corr-1", 1_001, &[1; 32]));
        assert_ne!(a, upload_id("corr-1", 1_000, &[2; 32]));
        assert_eq!(a, upload_id("corr-1", 1_000, &[1; 32]), "deterministic");
    }

    #[test]
    fn path_sha16_is_fixed_width_and_separator_free() {
        for path in ["index.html", "assets/very/deep/path.js", &"p".repeat(256)] {
            let sha = path_sha16(path);
            assert_eq!(sha.len(), 16);
            assert!(sha.bytes().all(|b| b.is_ascii_hexdigit()));
        }
    }

    fn session(paths: &[&str], pre_done: &[&str]) -> UploadSession {
        UploadSession {
            files: paths
                .iter()
                .map(|p| ManifestFile {
                    path: (*p).to_string(),
                    size: 1,
                    sha256: PK_HEX.to_string(),
                })
                .collect(),
            prune: false,
            by: PK_HEX.to_string(),
            created_ms: 1_700_000_000_000,
            pre_done: pre_done.iter().map(|p| (*p).to_string()).collect(),
        }
    }

    #[test]
    fn an_upload_expiry_is_derived_from_its_start_not_from_now() {
        // The self-erase contract: every row staged under a session (the session
        // document, the chunks, the completion marks) carries THIS instant, so a
        // late chunk cannot keep an abandoned upload alive by refreshing it.
        let s = session(&["a"], &[]);
        assert_eq!(s.expires_at_ms(), s.created_ms + UPLOAD_TTL_MS);
        let mut late = session(&["a"], &[]);
        late.created_ms = u64::MAX - 5;
        assert_eq!(late.expires_at_ms(), u64::MAX, "saturating, never wrapping");
    }

    #[test]
    fn only_paths_without_a_pre_verified_mark_need_a_completion_row() {
        let s = session(&["index.html", "a.js", "b.js"], &["a.js"]);
        assert_eq!(s.awaiting_verification(), vec!["index.html", "b.js"]);
        // An incremental push where nothing changed reads NO completion rows at
        // all — the other half of the quadratic this restructure removed.
        let unchanged = session(&["index.html", "a.js"], &["index.html", "a.js"]);
        assert!(unchanged.awaiting_verification().is_empty());
    }

    #[test]
    fn a_session_written_before_the_split_still_decodes() {
        // The field was `done`; it is read back as `pre_done` (a serde alias), so
        // an upload in flight across a deployment keeps every mark it had rather
        // than being asked to re-send the whole bundle.
        let legacy = br#"{"files":[{"path":"a.js","size":1,"sha256":"ab"}],
            "prune":false,"by":"op","created_ms":7,"done":["a.js"]}"#;
        let decoded: UploadSession = serde_json::from_slice(legacy).expect("legacy decodes");
        assert!(decoded.pre_done.contains("a.js"));
        assert!(decoded.awaiting_verification().is_empty());
    }
}

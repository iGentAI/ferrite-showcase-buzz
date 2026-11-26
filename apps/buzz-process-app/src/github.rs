//! The GitHub cell: webhook admission, and the projection into room events.
//!
//! Admission comes FIRST and in this order, because each step protects the
//! next: bound the body, require the delivery headers, verify the HMAC over
//! the RAW bytes, and only then parse JSON. Verifying a signature over
//! re-serialized JSON would verify nothing — the bytes GitHub signed are the
//! bytes on the wire, and a parse that normalizes anything breaks the
//! equality. That is why [`verify_signature`] takes `&[u8]` and why the
//! delivery digest is taken over the same slice.
//!
//! The projection is deliberately LABELLED. Everything this cell publishes is
//! reconciled from a provider — it is not a Nostr signature by the person who
//! pushed — so every projected event carries a provenance tag saying so. A
//! GitHub actor is never presented as a workspace member.

use std::collections::BTreeMap;

use ferrite_app::serde::{Deserialize, Serialize};
use ferrite_app::serde_json::{self, Value};

use crate::binding::Identity;
use crate::crypto::hmac::hmac_sha256;
use crate::crypto::sha256::sha256;
use crate::crypto::{ct_eq, from_hex, to_hex};
use crate::error::{ProcessError, Result};

/// The header carrying GitHub's HMAC over the raw body.
pub const SIGNATURE_HEADER: &str = "x-hub-signature-256";
/// The header carrying the delivery's unique id.
pub const DELIVERY_HEADER: &str = "x-github-delivery";
/// The header naming the event.
pub const EVENT_HEADER: &str = "x-github-event";
/// The largest webhook body this cell accepts.
///
/// OUR limit, not GitHub's — GitHub's own documented payload cap is 25 MB.
/// One MiB is chosen because this is a constrained app: the body arrives
/// through the gateway's ingress envelope (itself far smaller), the cell holds
/// it in memory to verify an HMAC over the raw bytes, and no projection here
/// reads a field that a legitimate 25 MB push carries and a 1 MiB one does
/// not. A delivery over this bound is refused with a reason rather than
/// truncated.
pub const MAX_WEBHOOK_BYTES: usize = 1024 * 1024;
/// Provenance tag every projected event carries.
pub const TAG_PROVENANCE: &str = "process-provenance";
/// The provenance value for anything reconciled from GitHub.
pub const PROVENANCE_GITHUB: &str = "reconciled:github";

// NIP-34 kinds the projection produces. Registered and delivered by the relay
// already — this cell adds no new vocabulary, which is why an unchanged client
// renders these in the Projects UI it has.
pub const KIND_GIT_PULL_REQUEST: u32 = 1618;
pub const KIND_GIT_ISSUE: u32 = 1621;
pub const KIND_GIT_STATUS_OPEN: u32 = 1630;
pub const KIND_GIT_STATUS_MERGED: u32 = 1631;
pub const KIND_GIT_STATUS_CLOSED: u32 = 1632;
pub const KIND_GIT_STATUS_DRAFT: u32 = 1633;
/// The room message kind.
pub const KIND_STREAM_MESSAGE: u32 = 9;

/// Verify GitHub's `sha256=<hex>` signature over the RAW request body.
///
/// Constant-time on the digest comparison, and length-checked before it: a
/// short or malformed header must not short-circuit into a partial compare.
#[must_use]
pub fn verify_signature(secret: &[u8], raw_body: &[u8], header: &str) -> bool {
    if secret.is_empty() {
        return false;
    }
    let Some(hex) = header.trim().strip_prefix("sha256=") else {
        return false;
    };
    if hex.len() != 64 {
        return false;
    }
    let Some(claimed) = from_hex(hex) else {
        return false;
    };
    let expected = hmac_sha256(secret, raw_body);
    ct_eq(&claimed, &expected)
}

/// The (event, action) pairs this cell will act on.
///
/// An allowlist rather than a filter: an event nobody has thought about must
/// not reach the projection and become a room message of unknown shape. An
/// empty action list means the event carries no action.
pub const ALLOWED: &[(&str, &[&str])] = &[
    ("push", &[]),
    ("create", &[]),
    ("delete", &[]),
    (
        "pull_request",
        &[
            "opened",
            "reopened",
            "closed",
            "synchronize",
            "ready_for_review",
            "converted_to_draft",
            "edited",
        ],
    ),
    ("pull_request_review", &["submitted", "edited", "dismissed"]),
    ("issues", &["opened", "closed", "reopened", "edited"]),
    ("check_run", &["created", "completed", "rerequested"]),
    ("check_suite", &["completed", "requested", "rerequested"]),
    ("workflow_run", &["completed", "requested", "in_progress"]),
    ("ping", &[]),
];

/// Whether `(event, action)` is on the allowlist.
#[must_use]
pub fn is_allowed(event: &str, action: Option<&str>) -> bool {
    for (name, actions) in ALLOWED {
        if *name != event {
            continue;
        }
        return match action {
            None => actions.is_empty(),
            Some(a) => actions.contains(&a),
        };
    }
    false
}

/// One admitted delivery: what it was, and the bytes it carried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct Delivery {
    pub uuid: String,
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// SHA-256 over the raw body, so a REPLAYED uuid carrying different bytes
    /// is a security failure rather than a duplicate.
    pub body_sha256: String,
}

impl Delivery {
    /// The durable key that makes a delivery exactly-once.
    #[must_use]
    pub fn key(&self, community: &str) -> Vec<u8> {
        format!("ghdelivery/{community}/{}", self.uuid).into_bytes()
    }
}

/// A header lookup that does not care about case, as HTTP requires.
#[must_use]
pub fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Admit a webhook: bound, identify, VERIFY, then parse.
///
/// Returns the delivery and the parsed payload. Every refusal is final — an
/// unsigned or unknown delivery cannot become admissible by being retried.
pub fn admit(
    secret: &[u8],
    headers: &[(String, String)],
    raw_body: &[u8],
) -> Result<(Delivery, Value)> {
    if raw_body.len() > MAX_WEBHOOK_BYTES {
        return Err(ProcessError::Invalid(format!(
            "webhook body is over {MAX_WEBHOOK_BYTES} bytes"
        )));
    }
    let Some(uuid) = header(headers, DELIVERY_HEADER) else {
        return Err(ProcessError::Invalid(format!(
            "webhook is missing {DELIVERY_HEADER}"
        )));
    };
    let Some(event) = header(headers, EVENT_HEADER) else {
        return Err(ProcessError::Invalid(format!(
            "webhook is missing {EVENT_HEADER}"
        )));
    };
    if uuid.is_empty() || uuid.len() > 128 || event.is_empty() || event.len() > 64 {
        return Err(ProcessError::Invalid(
            "webhook delivery id or event name is empty or too long".into(),
        ));
    }
    let Some(signature) = header(headers, SIGNATURE_HEADER) else {
        return Err(ProcessError::Refused(format!(
            "webhook is missing {SIGNATURE_HEADER}"
        )));
    };
    // The signature is verified over the RAW bytes, BEFORE any parse.
    if !verify_signature(secret, raw_body, signature) {
        return Err(ProcessError::Refused(
            "webhook signature does not verify".into(),
        ));
    }
    let payload: Value = serde_json::from_slice(raw_body)
        .map_err(|e| ProcessError::Invalid(format!("webhook body is not JSON: {e}")))?;
    let action = payload
        .get("action")
        .and_then(Value::as_str)
        .map(str::to_string);
    if !is_allowed(event, action.as_deref()) {
        return Err(ProcessError::Refused(format!(
            "webhook event {event}{} is not on the allowlist",
            action
                .as_deref()
                .map(|a| format!(".{a}"))
                .unwrap_or_default()
        )));
    }
    Ok((
        Delivery {
            uuid: uuid.to_string(),
            event: event.to_string(),
            action,
            body_sha256: to_hex(&sha256(raw_body)),
        },
        payload,
    ))
}

/// One event the projection wants published.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct Draft {
    pub kind: u32,
    pub tags: Vec<Vec<String>>,
    pub content: String,
}

/// What a delivery becomes in the workspace.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct Plan {
    /// The branch incarnation this delivery is about, when it names one — the
    /// room to materialize and post into.
    pub identity: Option<Identity>,
    /// A human-readable line for the room.
    pub summary: String,
    /// NIP-34 events to publish beside it.
    pub drafts: Vec<Draft>,
}

/// The repository's stable id and display name, from any GitHub payload.
fn repository(payload: &Value) -> Result<(String, String)> {
    let repo = payload
        .get("repository")
        .ok_or_else(|| ProcessError::Invalid("webhook has no repository".into()))?;
    let id = repo
        .get("id")
        .and_then(|v| {
            v.as_u64()
                .map(|n| n.to_string())
                .or_else(|| v.as_str().map(str::to_string))
        })
        .ok_or_else(|| ProcessError::Invalid("webhook repository has no stable id".into()))?;
    let display = repo
        .get("full_name")
        .and_then(Value::as_str)
        .unwrap_or("unknown/unknown")
        .to_string();
    Ok((id, display))
}

/// The provenance tags every projected event carries.
fn provenance_tags(delivery: &Delivery) -> Vec<Vec<String>> {
    vec![
        vec![TAG_PROVENANCE.to_string(), PROVENANCE_GITHUB.to_string()],
        vec!["process-delivery".to_string(), delivery.uuid.clone()],
    ]
}

/// The head repository's stable id from a `pull_request` node, when it differs
/// from the base — i.e. when the PR comes from a FORK.
///
/// Shared by the `pull_request` and `pull_request_review` projections on
/// purpose: a review of a fork PR must resolve to the SAME binding identity as
/// the pull request itself, or the two land in different rooms — and worse, a
/// fork branch named like one in the base repository would collide with it.
fn head_fork_id(pr: &Value, base_repo_id: &str) -> Option<String> {
    pr.get("head")
        .and_then(|h| h.get("repo"))
        .and_then(|r| r.get("id"))
        .and_then(|v| {
            v.as_u64()
                .map(|n| n.to_string())
                .or_else(|| v.as_str().map(str::to_string))
        })
        .filter(|id| id != base_repo_id)
}

/// The head repository of a check or workflow run when it is not the base
/// repository: `workflow_run.head_repository`, a check run's suite's head
/// repository, or the head of the first linked pull request.
fn check_head_repo(node: &Value, base_repo_id: &str) -> Option<String> {
    let id_of = |v: Option<&Value>| {
        v.and_then(|r| r.get("id")).and_then(|v| {
            v.as_u64()
                .map(|n| n.to_string())
                .or_else(|| v.as_str().map(str::to_string))
        })
    };
    id_of(node.get("head_repository"))
        .or_else(|| id_of(node.get("check_suite").and_then(|s| s.get("head_repository"))))
        .or_else(|| {
            node.get("pull_requests")
                .and_then(Value::as_array)
                .and_then(|prs| prs.first())
                .and_then(|pr| id_of(pr.get("head").and_then(|h| h.get("repo"))))
        })
        .filter(|id| id != base_repo_id)
}

/// Plan the workspace effects of an admitted delivery.
///
/// `generation` is supplied by the caller (the cell's durable incarnation
/// counter for this ref) rather than derived here, because "is this the same
/// branch lifetime?" is a question only durable state can answer.
pub fn plan(
    delivery: &Delivery,
    payload: &Value,
    instance: &str,
    generation: &str,
) -> Result<Plan> {
    // A ping is a real delivery that names nothing: acknowledge, project
    // nothing. Saying so beats treating it as a malformed push.
    if delivery.event == "ping" {
        return Ok(Plan {
            identity: None,
            summary: "GitHub webhook connected.".to_string(),
            drafts: Vec::new(),
        });
    }
    let (repo_id, display) = repository(payload)?;
    let mut tags = provenance_tags(delivery);

    match delivery.event.as_str() {
        "push" | "create" | "delete" => {
            let git_ref = payload
                .get("ref")
                .and_then(Value::as_str)
                .ok_or_else(|| ProcessError::Invalid("push webhook has no ref".into()))?;
            // `create`/`delete` carry a bare name plus a ref_type; `push`
            // carries a full ref. Normalize to a full ref either way.
            let full_ref = if git_ref.starts_with("refs/") {
                git_ref.to_string()
            } else {
                match payload.get("ref_type").and_then(Value::as_str) {
                    Some("tag") => format!("refs/tags/{git_ref}"),
                    _ => format!("refs/heads/{git_ref}"),
                }
            };
            let identity = Identity::github(instance, &repo_id, None, &full_ref, generation);
            identity.validate()?;
            let after = payload
                .get("after")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let pusher = payload
                .get("pusher")
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                .or_else(|| {
                    payload
                        .get("sender")
                        .and_then(|s| s.get("login"))
                        .and_then(Value::as_str)
                })
                .unwrap_or("someone");
            let summary = match delivery.event.as_str() {
                "create" => {
                    format!("{display}: {pusher} created {full_ref} (reconciled from GitHub)")
                }
                "delete" => {
                    format!("{display}: {pusher} deleted {full_ref} (reconciled from GitHub)")
                }
                _ => format!(
                    "{display}: {pusher} pushed {full_ref}{} (reconciled from GitHub)",
                    if after.is_empty() {
                        String::new()
                    } else {
                        format!(" → {}", short(after))
                    }
                ),
            };
            Ok(Plan {
                identity: Some(identity),
                summary,
                drafts: Vec::new(),
            })
        }
        "pull_request" => {
            let pr = payload.get("pull_request").ok_or_else(|| {
                ProcessError::Invalid("pull_request webhook has no pull_request".into())
            })?;
            let number = pr.get("number").and_then(Value::as_u64).unwrap_or_default();
            let title = pr.get("title").and_then(Value::as_str).unwrap_or("");
            let head_ref = pr
                .get("head")
                .and_then(|h| h.get("ref"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let base_ref = pr
                .get("base")
                .and_then(|b| b.get("ref"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let head_repo_id = head_fork_id(pr, &repo_id);
            let identity = Identity::github(
                instance,
                &repo_id,
                head_repo_id.as_deref(),
                &format!("refs/heads/{head_ref}"),
                generation,
            );
            identity.validate()?;
            let merged = pr.get("merged").and_then(Value::as_bool).unwrap_or(false);
            let draft = pr.get("draft").and_then(Value::as_bool).unwrap_or(false);
            let action = delivery.action.as_deref().unwrap_or("updated");
            let status_kind = match (action, merged, draft) {
                ("closed", true, _) => Some(KIND_GIT_STATUS_MERGED),
                ("closed", false, _) => Some(KIND_GIT_STATUS_CLOSED),
                ("opened" | "reopened" | "ready_for_review", _, false) => {
                    Some(KIND_GIT_STATUS_OPEN)
                }
                ("opened" | "reopened" | "converted_to_draft", _, true) => {
                    Some(KIND_GIT_STATUS_DRAFT)
                }
                _ => None,
            };
            tags.push(vec!["process-pr".to_string(), number.to_string()]);
            let mut drafts = vec![Draft {
                kind: KIND_GIT_PULL_REQUEST,
                tags: tags.clone(),
                content: serde_json::to_string(&serde_json::json!({
                    "number": number,
                    "title": title,
                    "head": head_ref,
                    "base": base_ref,
                    "repository": display,
                }))
                .unwrap_or_default(),
            }];
            if let Some(kind) = status_kind {
                drafts.push(Draft {
                    kind,
                    tags: tags.clone(),
                    content: String::new(),
                });
            }
            Ok(Plan {
                identity: Some(identity),
                summary: format!(
                    "{display}: pull request #{number} {action} — {title} ({head_ref} → {base_ref}, reconciled from GitHub)"
                ),
                drafts,
            })
        }
        "pull_request_review" => {
            let review = payload
                .get("review")
                .ok_or_else(|| ProcessError::Invalid("review webhook has no review".into()))?;
            let state = review
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("commented");
            let reviewer = review
                .get("user")
                .and_then(|u| u.get("login"))
                .and_then(Value::as_str)
                .unwrap_or("someone");
            let number = payload
                .get("pull_request")
                .and_then(|pr| pr.get("number"))
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let head_ref = payload
                .get("pull_request")
                .and_then(|pr| pr.get("head"))
                .and_then(|h| h.get("ref"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            // A review of a FORK pull request must resolve to the same identity
            // the pull request itself did — the same helper, so the two cannot
            // drift apart and land in two rooms.
            let head_repo_id = payload
                .get("pull_request")
                .and_then(|pr| head_fork_id(pr, &repo_id));
            let identity = Identity::github(
                instance,
                &repo_id,
                head_repo_id.as_deref(),
                &format!("refs/heads/{head_ref}"),
                generation,
            );
            identity.validate()?;
            Ok(Plan {
                identity: Some(identity),
                summary: format!(
                    "{display}: {reviewer} reviewed #{number} — {state} (reconciled from GitHub)"
                ),
                drafts: Vec::new(),
            })
        }
        "issues" => {
            let issue = payload
                .get("issue")
                .ok_or_else(|| ProcessError::Invalid("issues webhook has no issue".into()))?;
            let number = issue
                .get("number")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let title = issue.get("title").and_then(Value::as_str).unwrap_or("");
            let action = delivery.action.as_deref().unwrap_or("updated");
            tags.push(vec!["process-issue".to_string(), number.to_string()]);
            Ok(Plan {
                identity: None,
                summary: format!(
                    "{display}: issue #{number} {action} — {title} (reconciled from GitHub)"
                ),
                drafts: vec![Draft {
                    kind: KIND_GIT_ISSUE,
                    tags,
                    content: serde_json::to_string(&serde_json::json!({
                        "number": number,
                        "title": title,
                        "repository": display,
                    }))
                    .unwrap_or_default(),
                }],
            })
        }
        "check_run" | "check_suite" | "workflow_run" => {
            let node = payload
                .get(&delivery.event)
                .ok_or_else(|| ProcessError::Invalid("check webhook has no subject".into()))?;
            let name = node
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(&delivery.event);
            let status = node
                .get("conclusion")
                .and_then(Value::as_str)
                .or_else(|| node.get("status").and_then(Value::as_str))
                .unwrap_or("unknown");
            let head_branch = node
                .get("head_branch")
                .and_then(Value::as_str)
                .or_else(|| {
                    node.get("check_suite")
                        .and_then(|s| s.get("head_branch"))
                        .and_then(Value::as_str)
                })
                .unwrap_or_default();
            let head_repo_id = check_head_repo(node, &repo_id);
            let identity = if head_branch.is_empty() {
                None
            } else {
                // A fork's run names a branch of the FORK: binding it to the
                // base repository's room of the same name would post a
                // contributor's results into another branch's room.
                let id = Identity::github(
                    instance,
                    &repo_id,
                    head_repo_id.as_deref(),
                    &format!("refs/heads/{head_branch}"),
                    generation,
                );
                id.validate()?;
                Some(id)
            };
            Ok(Plan {
                identity,
                summary: format!(
                    "{display}: {name} — {status}{} (reconciled from GitHub)",
                    if head_branch.is_empty() {
                        String::new()
                    } else {
                        format!(" on {head_branch}")
                    }
                ),
                drafts: Vec::new(),
            })
        }
        other => Err(ProcessError::Refused(format!(
            "no projection for webhook event {other}"
        ))),
    }
}

/// A short commit id for a human-readable line.
pub(crate) fn short(commit: &str) -> String {
    commit.chars().take(8).collect()
}

/// A read-side reconciliation request: the branches of a repository.
///
/// GET-with-headers over the app's scoped GitHub egress. The token is a
/// header, never a query parameter, so it cannot end up in a log of URLs.
#[must_use]
pub fn branches_request(api_base: &str, owner_repo: &str, token: &str) -> crate::rcs::Call {
    crate::rcs::Call {
        url: format!(
            "{}/repos/{owner_repo}/branches?per_page=100",
            api_base.trim_end_matches('/')
        ),
        body: Vec::new(),
        headers: vec![
            ("authorization".to_string(), format!("Bearer {token}")),
            (
                "accept".to_string(),
                "application/vnd.github+json".to_string(),
            ),
            ("x-github-api-version".to_string(), "2022-11-28".to_string()),
        ],
    }
}

/// Parse a branches listing into `name -> tip`.
pub fn parse_branches(status: u16, body: &[u8]) -> Result<BTreeMap<String, String>> {
    if !(200..300).contains(&status) {
        return Err(ProcessError::Egress(format!("GitHub answered {status}")));
    }
    let rows: Value = serde_json::from_slice(body)
        .map_err(|e| ProcessError::Egress(format!("GitHub reply is not JSON: {e}")))?;
    let rows = rows
        .as_array()
        .ok_or_else(|| ProcessError::Egress("GitHub branches reply is not an array".into()))?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (Some(name), Some(sha)) = (
            row.get("name").and_then(Value::as_str),
            row.get("commit")
                .and_then(|c| c.get("sha"))
                .and_then(Value::as_str),
        ) else {
            return Err(ProcessError::Egress(
                "a GitHub branch row is missing its name or sha".into(),
            ));
        };
        out.insert(name.to_string(), sha.to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_app::serde_json;

    const SECRET: &[u8] = b"webhook-secret";

    fn signature(body: &[u8]) -> String {
        format!("sha256={}", to_hex(&hmac_sha256(SECRET, body)))
    }

    fn headers(body: &[u8], event: &str) -> Vec<(String, String)> {
        vec![
            ("X-GitHub-Delivery".into(), "d-1".into()),
            ("X-GitHub-Event".into(), event.into()),
            ("X-Hub-Signature-256".into(), signature(body)),
        ]
    }

    #[test]
    fn a_signature_verifies_over_the_raw_bytes() {
        let body = br#"{"zen":"keep it logically awesome"}"#;
        assert!(verify_signature(SECRET, body, &signature(body)));
        // One byte different anywhere, and it must fail.
        assert!(!verify_signature(
            SECRET,
            br#"{"zen":"other"}"#,
            &signature(body)
        ));
        assert!(!verify_signature(b"wrong-secret", body, &signature(body)));
    }

    #[test]
    fn malformed_signature_headers_are_refused() {
        let body = b"{}";
        for header in [
            "",
            "sha256=",
            "sha1=abcd",
            "sha256=not-hex-at-all-not-hex-at-all-not-hex-at-all-not-hex-at-all!!",
            // Right length, wrong content.
            &format!("sha256={}", "0".repeat(64)),
            // Truncated digest.
            &format!("sha256={}", &to_hex(&hmac_sha256(SECRET, body))[..32]),
        ] {
            assert!(!verify_signature(SECRET, body, header), "{header}");
        }
        // An empty secret can never verify anything.
        assert!(!verify_signature(b"", body, &signature(body)));
    }

    #[test]
    fn admission_verifies_before_parsing_and_bounds_the_body() {
        let body = serde_json::to_vec(&serde_json::json!({"zen": "x"})).expect("encode");
        let (delivery, _) = admit(SECRET, &headers(&body, "ping"), &body).expect("admit");
        assert_eq!(delivery.uuid, "d-1");
        assert_eq!(delivery.event, "ping");
        assert_eq!(delivery.body_sha256, to_hex(&sha256(&body)));

        // A bad signature is refused — and the body is never parsed, which is
        // why INVALID JSON with a bad signature still refuses on the signature.
        let mut bad = headers(&body, "ping");
        bad.retain(|(k, _)| !k.eq_ignore_ascii_case(SIGNATURE_HEADER));
        bad.push((SIGNATURE_HEADER.into(), signature(b"other")));
        let err = admit(SECRET, &bad, b"not json").expect_err("refused");
        assert!(matches!(err, ProcessError::Refused(_)), "{err:?}");

        // Missing headers.
        for drop in [DELIVERY_HEADER, EVENT_HEADER, SIGNATURE_HEADER] {
            let mut h = headers(&body, "ping");
            h.retain(|(k, _)| !k.eq_ignore_ascii_case(drop));
            assert!(admit(SECRET, &h, &body).is_err(), "missing {drop}");
        }

        // Oversized.
        let huge = vec![b'x'; MAX_WEBHOOK_BYTES + 1];
        assert!(admit(SECRET, &headers(&huge, "ping"), &huge).is_err());
    }

    #[test]
    fn the_event_allowlist_is_closed() {
        assert!(is_allowed("push", None));
        assert!(is_allowed("pull_request", Some("opened")));
        assert!(!is_allowed("pull_request", Some("assigned")));
        assert!(
            !is_allowed("pull_request", None),
            "an action is required here"
        );
        assert!(
            !is_allowed("push", Some("opened")),
            "push carries no action"
        );
        assert!(!is_allowed("repository_vulnerability_alert", None));
        assert!(!is_allowed("", None));
    }

    #[test]
    fn an_unlisted_action_is_refused_at_admission() {
        let body = serde_json::to_vec(&serde_json::json!({
            "action": "assigned",
            "repository": {"id": 1, "full_name": "o/r"},
            "pull_request": {"number": 1}
        }))
        .expect("encode");
        let err = admit(SECRET, &headers(&body, "pull_request"), &body).expect_err("refused");
        assert!(matches!(err, ProcessError::Refused(_)), "{err:?}");
    }

    #[test]
    fn a_push_projects_a_branch_room_with_provenance() {
        let body = serde_json::to_vec(&serde_json::json!({
            "ref": "refs/heads/feature/x",
            "after": "abcdef1234567890",
            "pusher": {"name": "ana"},
            "repository": {"id": 12345, "full_name": "acme/widgets"},
        }))
        .expect("encode");
        let (delivery, payload) = admit(SECRET, &headers(&body, "push"), &body).expect("admit");
        let plan = plan(&delivery, &payload, "github.com", "g1").expect("plan");
        let identity = plan.identity.expect("a push names a branch");
        assert_eq!(identity.provider, crate::binding::PROVIDER_GITHUB);
        assert_eq!(identity.stable_id, "12345", "the NUMERIC id, not the name");
        assert_eq!(identity.git_ref, "refs/heads/feature/x");
        assert!(plan.summary.contains("ana"));
        assert!(
            plan.summary.contains("reconciled from GitHub"),
            "provenance must be visible to a reader: {}",
            plan.summary
        );
    }

    #[test]
    fn a_created_branch_normalizes_its_bare_ref() {
        let body = serde_json::to_vec(&serde_json::json!({
            "ref": "feature/y",
            "ref_type": "branch",
            "sender": {"login": "bo"},
            "repository": {"id": 7, "full_name": "acme/widgets"},
        }))
        .expect("encode");
        let (delivery, payload) = admit(SECRET, &headers(&body, "create"), &body).expect("admit");
        let plan = plan(&delivery, &payload, "github.com", "g1").expect("plan");
        assert_eq!(
            plan.identity.expect("identity").git_ref,
            "refs/heads/feature/y"
        );
    }

    #[test]
    fn a_merged_pull_request_projects_the_merged_status() {
        let body = serde_json::to_vec(&serde_json::json!({
            "action": "closed",
            "repository": {"id": 12345, "full_name": "acme/widgets"},
            "pull_request": {
                "number": 42,
                "title": "Add the thing",
                "merged": true,
                "draft": false,
                "head": {"ref": "feature/x", "repo": {"id": 12345}},
                "base": {"ref": "main"},
            },
        }))
        .expect("encode");
        let (delivery, payload) =
            admit(SECRET, &headers(&body, "pull_request"), &body).expect("admit");
        let plan = plan(&delivery, &payload, "github.com", "g1").expect("plan");
        let kinds: Vec<u32> = plan.drafts.iter().map(|d| d.kind).collect();
        assert_eq!(kinds, vec![KIND_GIT_PULL_REQUEST, KIND_GIT_STATUS_MERGED]);
        for draft in &plan.drafts {
            assert!(
                draft
                    .tags
                    .iter()
                    .any(|t| t.first().is_some_and(|k| k == TAG_PROVENANCE)),
                "every projected event carries provenance"
            );
        }
        // Same-repo PR: no head_stable_id, so it is not a fork binding.
        assert!(plan.identity.expect("identity").head_stable_id.is_none());
    }

    #[test]
    fn a_fork_pull_request_carries_its_head_repository() {
        let body = serde_json::to_vec(&serde_json::json!({
            "action": "opened",
            "repository": {"id": 12345, "full_name": "acme/widgets"},
            "pull_request": {
                "number": 7,
                "title": "From a fork",
                "draft": false,
                "head": {"ref": "feature/z", "repo": {"id": 999}},
                "base": {"ref": "main"},
            },
        }))
        .expect("encode");
        let (delivery, payload) =
            admit(SECRET, &headers(&body, "pull_request"), &body).expect("admit");
        let plan = plan(&delivery, &payload, "github.com", "g1").expect("plan");
        let identity = plan.identity.expect("identity");
        assert_eq!(identity.head_stable_id.as_deref(), Some("999"));
        let kinds: Vec<u32> = plan.drafts.iter().map(|d| d.kind).collect();
        assert_eq!(kinds, vec![KIND_GIT_PULL_REQUEST, KIND_GIT_STATUS_OPEN]);
    }

    #[test]
    fn reviews_checks_and_issues_project_summaries() {
        let cases: Vec<(&str, Value)> = vec![
            (
                "pull_request_review",
                serde_json::json!({
                    "action": "submitted",
                    "repository": {"id": 1, "full_name": "o/r"},
                    "review": {"state": "approved", "user": {"login": "cy"}},
                    "pull_request": {"number": 3, "head": {"ref": "feature/a"}},
                }),
            ),
            (
                "check_run",
                serde_json::json!({
                    "action": "completed",
                    "repository": {"id": 1, "full_name": "o/r"},
                    "check_run": {"name": "build", "conclusion": "success", "head_branch": "feature/a"},
                }),
            ),
            (
                "workflow_run",
                serde_json::json!({
                    "action": "completed",
                    "repository": {"id": 1, "full_name": "o/r"},
                    "workflow_run": {"name": "ci", "conclusion": "failure", "head_branch": "feature/a"},
                }),
            ),
            (
                "issues",
                serde_json::json!({
                    "action": "opened",
                    "repository": {"id": 1, "full_name": "o/r"},
                    "issue": {"number": 9, "title": "Broken"},
                }),
            ),
        ];
        for (event, payload) in cases {
            let body = serde_json::to_vec(&payload).expect("encode");
            let (delivery, parsed) = admit(SECRET, &headers(&body, event), &body).expect(event);
            let plan = plan(&delivery, &parsed, "github.com", "g1").expect(event);
            assert!(plan.summary.contains("reconciled from GitHub"), "{event}");
        }
    }

    #[test]
    fn a_fork_review_shares_the_pull_requests_identity() {
        // The bug this pins: a review projected without the head repository id
        // would name a DIFFERENT room than its own pull request — and would
        // collide with a same-named branch in the base repository.
        let pr_body = serde_json::to_vec(&serde_json::json!({
            "action": "opened",
            "repository": {"id": 12345, "full_name": "acme/widgets"},
            "pull_request": {
                "number": 7,
                "title": "From a fork",
                "draft": false,
                "head": {"ref": "feature/z", "repo": {"id": 999}},
                "base": {"ref": "main"},
            },
        }))
        .expect("encode");
        let (pr_delivery, pr_payload) =
            admit(SECRET, &headers(&pr_body, "pull_request"), &pr_body).expect("admit");
        let pr_plan = plan(&pr_delivery, &pr_payload, "github.com", "g1").expect("plan");

        let review_body = serde_json::to_vec(&serde_json::json!({
            "action": "submitted",
            "repository": {"id": 12345, "full_name": "acme/widgets"},
            "review": {"state": "approved", "user": {"login": "cy"}},
            "pull_request": {
                "number": 7,
                "head": {"ref": "feature/z", "repo": {"id": 999}},
            },
        }))
        .expect("encode");
        let (rv_delivery, rv_payload) = admit(
            SECRET,
            &headers(&review_body, "pull_request_review"),
            &review_body,
        )
        .expect("admit");
        let rv_plan = plan(&rv_delivery, &rv_payload, "github.com", "g1").expect("plan");

        let pr_identity = pr_plan.identity.expect("pr identity");
        let rv_identity = rv_plan.identity.expect("review identity");
        assert_eq!(pr_identity, rv_identity, "one PR, one room");
        assert_eq!(rv_identity.head_stable_id.as_deref(), Some("999"));
        assert_eq!(
            pr_identity.room_id().expect("room"),
            rv_identity.room_id().expect("room")
        );

        // ...and a same-named branch in the BASE repository is a different room.
        let base_identity =
            Identity::github("github.com", "12345", None, "refs/heads/feature/z", "g1");
        assert_ne!(
            base_identity.room_id().expect("room"),
            rv_identity.room_id().expect("room"),
            "a fork branch must not collide with the base repository's"
        );
    }

    #[test]
    fn a_same_repo_review_is_not_a_fork_binding() {
        let body = serde_json::to_vec(&serde_json::json!({
            "action": "submitted",
            "repository": {"id": 12345, "full_name": "acme/widgets"},
            "review": {"state": "approved", "user": {"login": "cy"}},
            "pull_request": {
                "number": 8,
                "head": {"ref": "feature/y", "repo": {"id": 12345}},
            },
        }))
        .expect("encode");
        let (delivery, payload) =
            admit(SECRET, &headers(&body, "pull_request_review"), &body).expect("admit");
        let plan = plan(&delivery, &payload, "github.com", "g1").expect("plan");
        assert!(plan.identity.expect("identity").head_stable_id.is_none());
    }

    #[test]
    fn a_payload_without_a_repository_is_refused() {
        let body = serde_json::to_vec(&serde_json::json!({"ref": "refs/heads/x"})).expect("encode");
        let (delivery, payload) = admit(SECRET, &headers(&body, "push"), &body).expect("admit");
        assert!(plan(&delivery, &payload, "github.com", "g1").is_err());
    }

    #[test]
    fn a_ping_is_acknowledged_and_projects_nothing() {
        let body = serde_json::to_vec(&serde_json::json!({"zen": "x"})).expect("encode");
        let (delivery, payload) = admit(SECRET, &headers(&body, "ping"), &body).expect("admit");
        let plan = plan(&delivery, &payload, "github.com", "g1").expect("plan");
        assert!(plan.identity.is_none());
        assert!(plan.drafts.is_empty());
    }

    #[test]
    fn a_delivery_key_is_scoped_and_stable() {
        let d = Delivery {
            uuid: "d-1".into(),
            event: "push".into(),
            action: None,
            body_sha256: "ab".repeat(32),
        };
        assert_eq!(d.key("main"), b"ghdelivery/main/d-1".to_vec());
        assert_ne!(d.key("main"), d.key("other"));
    }

    #[test]
    fn a_branches_request_keeps_the_token_in_a_header() {
        let call = branches_request("https://api.github.com/", "acme/widgets", "ghp_x");
        assert_eq!(
            call.url,
            "https://api.github.com/repos/acme/widgets/branches?per_page=100"
        );
        assert!(!call.url.contains("ghp_x"));
        assert!(call
            .headers
            .iter()
            .any(|(k, v)| k == "authorization" && v == "Bearer ghp_x"));
    }

    #[test]
    fn branch_listings_parse_and_refuse_junk() {
        let body = serde_json::to_vec(&serde_json::json!([
            {"name": "main", "commit": {"sha": "c1"}},
            {"name": "feature/x", "commit": {"sha": "c2"}},
        ]))
        .expect("encode");
        let parsed = parse_branches(200, &body).expect("parse");
        assert_eq!(parsed.get("main").map(String::as_str), Some("c1"));
        assert_eq!(parsed.get("feature/x").map(String::as_str), Some("c2"));

        assert!(parse_branches(500, &body).is_err());
        assert!(parse_branches(200, b"{}").is_err());
        let missing = serde_json::to_vec(&serde_json::json!([{"name": "main"}])).expect("encode");
        assert!(parse_branches(200, &missing).is_err());
    }
}

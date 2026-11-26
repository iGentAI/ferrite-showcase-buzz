//! The Ferrite RCS control plane, as this cell speaks it.
//!
//! The cell holds the ONE injected platform token and is the only thing in the
//! workspace that does. Everything here is therefore request BUILDING and
//! reply PARSING — pure, testable, and deliberately separate from the egress
//! call that carries it, so the wire shapes can be pinned without a fleet.
//!
//! Two properties are load-bearing:
//!
//! * **The token never leaves the header.** It is attached per request as a
//!   bearer and never placed in a URL, a body, a log line, or a room event.
//!   [`Call::redacted`] is what a log statement is given.
//! * **A refusal is reported, never smoothed over.** `rcs_land` answers three
//!   different ways — landed, stale, rejected-by-verdict — and the difference
//!   is exactly what a member needs to see, so [`RcsReply`] keeps them apart
//!   instead of collapsing them into an error string.

use ferrite_app::serde_json::{self, Value};

use crate::error::{ProcessError, Result};

/// One outbound control-plane call: where it goes, what it carries, and the
/// headers that authorize it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub url: String,
    pub body: Vec<u8>,
    pub headers: Vec<(String, String)>,
}

impl Call {
    /// The same call with every credential replaced — what a log line gets.
    #[must_use]
    pub fn redacted(&self) -> Self {
        Self {
            url: self.url.clone(),
            body: self.body.clone(),
            headers: self
                .headers
                .iter()
                .map(|(k, v)| {
                    if k.eq_ignore_ascii_case("authorization") {
                        (k.clone(), "<redacted>".to_string())
                    } else {
                        (k.clone(), v.clone())
                    }
                })
                .collect(),
        }
    }
}

/// The control-plane endpoint and the token that authorizes it.
#[derive(Clone)]
pub struct ControlPlane {
    endpoint: String,
    token: String,
    app: String,
}

impl ControlPlane {
    /// `endpoint` is the platform's tool endpoint (a declared egress
    /// destination); `app` is the `{owner}/{app}` repository this cell drives.
    pub fn new(endpoint: &str, token: &str, app: &str) -> Result<Self> {
        if !endpoint.starts_with("https://") {
            return Err(ProcessError::Invalid(
                "the control-plane endpoint must be https".into(),
            ));
        }
        if token.is_empty() {
            return Err(ProcessError::Invalid(
                "the control-plane token is not configured".into(),
            ));
        }
        if app.is_empty() {
            return Err(ProcessError::Invalid("no RCS app is configured".into()));
        }
        Ok(Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            token: token.to_string(),
            app: app.to_string(),
        })
    }

    #[must_use]
    pub fn app(&self) -> &str {
        &self.app
    }

    /// One MCP `tools/call`. The platform's tool surface is uniform, so every
    /// verb below is this plus arguments.
    #[must_use]
    pub fn call(&self, tool: &str, mut arguments: Value) -> Call {
        if let Some(map) = arguments.as_object_mut() {
            map.insert("app".to_string(), Value::String(self.app.clone()));
        }
        let body = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": tool, "arguments": arguments }
        }))
        .unwrap_or_else(|_| b"{}".to_vec());
        Call {
            url: self.endpoint.clone(),
            body,
            headers: vec![
                (
                    "authorization".to_string(),
                    format!("Bearer {}", self.token),
                ),
                ("content-type".to_string(), "application/json".to_string()),
            ],
        }
    }

    #[must_use]
    pub fn branches(&self) -> Call {
        self.call("rcs_branches", serde_json::json!({}))
    }

    #[must_use]
    pub fn log(&self, branch: &str, limit: u32) -> Call {
        self.call("rcs_log", serde_json::json!({ "branch": branch, "limit": limit }))
    }

    /// Create `branch` at `from` (a ref or the trunk head when absent).
    #[must_use]
    pub fn branch(&self, branch: &str, from: Option<&str>) -> Call {
        let mut args = serde_json::json!({ "branch": branch });
        if let (Some(map), Some(from)) = (args.as_object_mut(), from) {
            map.insert("from".to_string(), Value::String(from.to_string()));
        }
        self.call("rcs_branch", args)
    }

    /// Commit files on a member's behalf — the mediated write path.
    #[must_use]
    pub fn commit(
        &self,
        files: &std::collections::BTreeMap<String, String>,
        message: &str,
        branch: Option<&str>,
    ) -> Call {
        let mut args = serde_json::json!({ "files": files, "message": message });
        if let (Some(map), Some(branch)) = (args.as_object_mut(), branch) {
            map.insert("branch".to_string(), Value::String(branch.to_string()));
        }
        self.call("rcs_commit", args)
    }

    /// Three-way merge into a non-trunk branch.
    #[must_use]
    pub fn merge(&self, from: &str, into: &str) -> Call {
        self.call("rcs_merge", serde_json::json!({ "from": from, "into": into }))
    }

    /// The gated trunk land: fast-forward descent plus a green verdict.
    #[must_use]
    pub fn land(&self, from: &str, into: &str) -> Call {
        self.call("rcs_land", serde_json::json!({ "from": from, "into": into }))
    }

    /// Run the app's tests against a committed revision, producing the
    /// commit-bound verdict a land consults.
    #[must_use]
    pub fn test_ref(&self, reference: &str) -> Call {
        self.call("test_ref", serde_json::json!({ "reference": reference }))
    }
}

/// One commit, as the control plane reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub commit: String,
    pub message: String,
    pub author: String,
    pub timestamp_ms: u64,
}

/// How a merge ended, TYPED rather than left as free-form status text.
///
/// The distinction is the whole value of the reply: a completed merge and a
/// refused one are opposite answers, and a caller deciding by string compare
/// on an unrecognized status would get the dangerous one wrong. An unknown
/// status is therefore [`MergeStatus::Unknown`] and treated as NOT successful
/// — the conservative direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeStatus {
    /// The merge took effect.
    Merged,
    /// Refused: the source is behind its target.
    Stale,
    /// Refused: paths diverged. No commit was made.
    Conflict,
    /// A status this version does not recognize.
    Unknown(String),
}

impl MergeStatus {
    #[must_use]
    pub fn parse(status: &str) -> Self {
        match status {
            "merged" | "ok" | "fast-forward" | "fast_forward" => Self::Merged,
            "stale" => Self::Stale,
            "conflict" | "conflicted" => Self::Conflict,
            other => Self::Unknown(other.to_string()),
        }
    }

    #[must_use]
    pub const fn succeeded(&self) -> bool {
        matches!(self, Self::Merged)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Merged => "merged",
            Self::Stale => "stale",
            Self::Conflict => "conflict",
            Self::Unknown(s) => s,
        }
    }
}

/// What the control plane answered. The variants keep the three land outcomes
/// apart, because "refused because you are behind" and "refused because your
/// tests fail" ask a member for different things.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RcsReply {
    Branches(Vec<(String, String)>),
    Log(Vec<LogEntry>),
    Committed {
        branch: String,
        commit: String,
    },
    Merged {
        into: String,
        commit: Option<String>,
        status: MergeStatus,
        conflicts: Vec<String>,
    },
    Landed {
        into: String,
        commit: String,
    },
    Rejected {
        reason: String,
    },
    BuildStarted {
        operation: String,
    },
}

impl RcsReply {
    /// Whether this reply means the verb took effect.
    ///
    /// A `Merged` counts exactly when its status says the merge happened — a
    /// stale or conflicted merge changed nothing upstream and must not be
    /// reported to a member as done.
    #[must_use]
    pub const fn succeeded(&self) -> bool {
        match self {
            Self::Committed { .. } | Self::Landed { .. } | Self::BuildStarted { .. } => true,
            Self::Merged { status, .. } => status.succeeded(),
            Self::Branches(_) | Self::Log(_) | Self::Rejected { .. } => false,
        }
    }
}

/// Parse a control-plane reply.
///
/// A non-2xx status is a transient EGRESS fault rather than a refusal: the
/// platform's own refusals arrive as 2xx documents (`Rejected`, or a `merged`
/// with a conflict list), and treating a 502 as "the land was rejected" would
/// tell a member their branch failed its tests when the truth is the network
/// blipped.
pub fn parse_reply(status: u16, body: &[u8]) -> Result<RcsReply> {
    if !(200..300).contains(&status) {
        return Err(ProcessError::Egress(format!(
            "the control plane answered {status}"
        )));
    }
    let mut value: Value = serde_json::from_slice(body)
        .map_err(|e| ProcessError::Egress(format!("control-plane reply is not JSON: {e}")))?;

    // DEV serves the control plane as stateless MCP. The older direct outcome
    // envelope is accepted too, so a cell can be rolled across a control-plane
    // change without an all-at-once cutover.
    if let Some(error) = value.get("error") {
        return Err(ProcessError::Egress(format!(
            "control-plane MCP error: {}",
            error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown MCP error")
        )));
    }
    if let Some(result) = value.get("result").cloned() {
        value = result.get("structuredContent").cloned().unwrap_or(result);
    }

    // The documented envelope is a single-key object naming the outcome.
    if let Some(v) = value.get("RcsBranches") {
        // A MISSING array is a malformed reply, not an empty repository: a
        // cell that read it as "no branches" would retire every room it had.
        let rows = v.get("branches").and_then(Value::as_array).ok_or_else(|| {
            ProcessError::Egress("RcsBranches reply has no branches array".into())
        })?;
        let mut out = Vec::new();
        for pair in rows {
            let name = pair.get(0).and_then(Value::as_str).unwrap_or_default();
            let tip = pair.get(1).and_then(Value::as_str).unwrap_or_default();
            if name.is_empty() || tip.is_empty() {
                return Err(ProcessError::Egress(
                    "a branch row is missing its name or tip".into(),
                ));
            }
            out.push((name.to_string(), tip.to_string()));
        }
        return Ok(RcsReply::Branches(out));
    }
    if let Some(v) = value.get("RcsLog") {
        let rows = v
            .get("commits")
            .and_then(Value::as_array)
            .ok_or_else(|| ProcessError::Egress("RcsLog reply has no commits array".into()))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(LogEntry {
                commit: string_at(row, "commit")?,
                message: row
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                author: row
                    .get("author")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                timestamp_ms: row
                    .get("timestamp_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
            });
        }
        return Ok(RcsReply::Log(out));
    }
    if let Some(v) = value.get("RcsCommitted") {
        return Ok(RcsReply::Committed {
            branch: string_at(v, "branch")?,
            commit: string_at(v, "commit")?,
        });
    }
    if let Some(v) = value.get("RcsMerged") {
        return Ok(RcsReply::Merged {
            into: string_at(v, "into")?,
            commit: v.get("commit").and_then(Value::as_str).map(str::to_string),
            status: MergeStatus::parse(v.get("status").and_then(Value::as_str).unwrap_or("merged")),
            conflicts: v
                .get("conflicts")
                .and_then(Value::as_array)
                .map(|rows| {
                    rows.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        });
    }
    if let Some(v) = value.get("RcsLanded") {
        return Ok(RcsReply::Landed {
            into: string_at(v, "into")?,
            commit: string_at(v, "commit")?,
        });
    }
    if let Some(v) = value.get("Rejected") {
        let listed: Vec<&str> = v
            .get("reasons")
            .and_then(Value::as_array)
            .map(|rows| rows.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        return Ok(RcsReply::Rejected {
            reason: v
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| (!listed.is_empty()).then(|| listed.join("; ")))
                .unwrap_or_else(|| "the platform rejected this request".to_string()),
        });
    }
    // The platform's other deterministic refusals arrive as 200 results too.
    // They are final answers, not transport faults: retrying cannot change
    // them, and the member must be told why.
    if let Some(v) = value.get("Error") {
        let reason = v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
        return Ok(RcsReply::Rejected { reason });
    }
    if let Some(v) = value.get("Tested") {
        if v.get("passed").and_then(Value::as_bool) == Some(false) {
            let summary = v.get("summary").and_then(Value::as_str).unwrap_or("tests failed");
            return Ok(RcsReply::Rejected { reason: format!("tests failed: {summary}") });
        }
    }
    for refusal in ["NotOwner", "QuotaExceeded", "InsufficientCredit", "InsufficientCredits", "Frozen"] {
        if let Some(v) = value.get(refusal) {
            let detail = v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
            return Ok(RcsReply::Rejected { reason: format!("{refusal}: {detail}") });
        }
    }
    if let Some(v) = value.get("BuildStarted") {
        return Ok(RcsReply::BuildStarted {
            operation: v
                .get("operation")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        });
    }
    Err(ProcessError::Egress(format!(
        "unrecognized control-plane reply: {}",
        first_key(&value)
    )))
}

fn string_at(value: &Value, key: &str) -> Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ProcessError::Egress(format!("control-plane reply is missing {key}")))
}

fn first_key(value: &Value) -> String {
    value
        .as_object()
        .and_then(|m| m.keys().next().cloned())
        .unwrap_or_else(|| "<not an object>".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_mcp_refusals_are_rejections_with_their_reason() {
        let wrap = |inner: &str| format!(r#"{{"jsonrpc":"2.0","id":1,"result":{{"structuredContent":{inner}}}}}"#);
        for (inner, needle) in [
            (r#"{"Error":"branch exists"}"#, "branch exists"),
            (r#"{"Tested":{"passed":false,"summary":"2 failed"}}"#, "2 failed"),
            (r#"{"NotOwner":"acct/x"}"#, "NotOwner"),
            (r#"{"Rejected":{"reasons":["frozen","quota"]}}"#, "frozen; quota"),
        ] {
            match parse_reply(200, wrap(inner).as_bytes()) {
                Ok(RcsReply::Rejected { reason }) => assert!(reason.contains(needle), "{reason}"),
                other => panic!("{inner}: {other:?}"),
            }
        }
    }

    fn cp() -> ControlPlane {
        ControlPlane::new("https://control.example/", "tok-abc", "acct/my-app").expect("cp")
    }

    #[test]
    fn an_endpoint_must_be_https_and_carry_a_token_and_app() {
        assert!(ControlPlane::new("http://control.example", "t", "a").is_err());
        assert!(ControlPlane::new("https://control.example", "", "a").is_err());
        assert!(ControlPlane::new("https://control.example", "t", "").is_err());
        assert!(ControlPlane::new("https://control.example", "t", "a").is_ok());
    }

    #[test]
    fn every_call_carries_the_app_and_a_bearer_and_never_the_token_in_the_url() {
        let call = cp().branches();
        assert_eq!(call.url, "https://control.example");
        assert!(
            !call.url.contains("tok-abc"),
            "the token must not be in the URL"
        );
        let body: Value = serde_json::from_slice(&call.body).expect("json");
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["method"], "tools/call");
        assert_eq!(body["params"]["name"], "rcs_branches");
        assert_eq!(body["params"]["arguments"]["app"], "acct/my-app");
        assert!(
            !String::from_utf8_lossy(&call.body).contains("tok-abc"),
            "the token must not be in the body"
        );
        assert_eq!(
            call.headers
                .iter()
                .find(|(k, _)| k == "authorization")
                .map(|(_, v)| v.as_str()),
            Some("Bearer tok-abc")
        );
    }

    #[test]
    fn redaction_removes_the_credential_for_logging() {
        let redacted = cp().land("feature", "main").redacted();
        let auth = redacted
            .headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .map(|(_, v)| v.clone())
            .expect("header");
        assert_eq!(auth, "<redacted>");
        assert!(!format!("{redacted:?}").contains("tok-abc"));
    }

    #[test]
    fn each_verb_builds_its_documented_arguments() {
        let cp = cp();
        let cases: Vec<(Call, &str)> = vec![
            (cp.branches(), "rcs_branches"),
            (cp.log("main", 10), "rcs_log"),
            (cp.branch("feature", Some("main")), "rcs_branch"),
            (cp.merge("feature", "develop"), "rcs_merge"),
            (cp.land("feature", "main"), "rcs_land"),
            (cp.test_ref("feature"), "test_ref"),
        ];
        for (call, tool) in cases {
            let body: Value = serde_json::from_slice(&call.body).expect("json");
            assert_eq!(body["params"]["name"], tool);
        }
        let branch_no_from = cp.branch("feature", None);
        let body: Value = serde_json::from_slice(&branch_no_from.body).expect("json");
        assert!(
            body["params"]["arguments"].get("from").is_none(),
            "absent means trunk head"
        );

        let mut files = std::collections::BTreeMap::new();
        files.insert("a.txt".to_string(), "x".to_string());
        let commit = cp.commit(&files, "msg", Some("feature"));
        let body: Value = serde_json::from_slice(&commit.body).expect("json");
        assert_eq!(body["params"]["name"], "rcs_commit");
        assert_eq!(body["params"]["arguments"]["files"]["a.txt"], "x");
        assert_eq!(body["params"]["arguments"]["branch"], "feature");
    }

    #[test]
    fn branches_and_log_parse() {
        let reply = parse_reply(
            200,
            br#"{"RcsBranches":{"app":"a","branches":[["main","c1"],["feature","c2"]]}}"#,
        )
        .expect("parse");
        assert_eq!(
            reply,
            RcsReply::Branches(vec![
                ("main".into(), "c1".into()),
                ("feature".into(), "c2".into())
            ])
        );
        let reply = parse_reply(
            200,
            br#"{"RcsLog":{"commits":[{"commit":"c1","message":"m","author":"a","timestamp_ms":7}]}}"#,
        )
        .expect("parse");
        assert_eq!(
            reply,
            RcsReply::Log(vec![LogEntry {
                commit: "c1".into(),
                message: "m".into(),
                author: "a".into(),
                timestamp_ms: 7,
            }])
        );
    }

    #[test]
    fn mcp_structured_content_envelope_parses() {
        let reply = parse_reply(
            200,
            br#"{"jsonrpc":"2.0","id":1,"result":{"structuredContent":{"RcsBranches":{"app":"a","branches":[["main","c1"]]}}}}"#,
        )
        .expect("parse MCP envelope");
        assert_eq!(
            reply,
            RcsReply::Branches(vec![("main".into(), "c1".into())])
        );
    }

    #[test]
    fn the_three_land_outcomes_stay_apart() {
        // Landed.
        assert_eq!(
            parse_reply(
                200,
                br#"{"RcsLanded":{"app":"a","into":"main","commit":"c9"}}"#
            )
            .expect("parse"),
            RcsReply::Landed {
                into: "main".into(),
                commit: "c9".into()
            }
        );
        // Stale — reported as a MERGED envelope with a status, per the API.
        let stale =
            parse_reply(200, br#"{"RcsMerged":{"into":"main","status":"stale"}}"#).expect("parse");
        assert_eq!(
            stale,
            RcsReply::Merged {
                into: "main".into(),
                commit: None,
                status: MergeStatus::Stale,
                conflicts: vec![]
            }
        );
        assert!(!stale.succeeded());
        // Rejected by verdict.
        assert_eq!(
            parse_reply(200, br#"{"Rejected":{"reason":"tests failed"}}"#).expect("parse"),
            RcsReply::Rejected {
                reason: "tests failed".into()
            }
        );
    }

    #[test]
    fn a_completed_merge_is_reported_as_success() {
        // The bug this pins: a merge that TOOK EFFECT must not be reported to a
        // member as a failure.
        let merged = parse_reply(
            200,
            br#"{"RcsMerged":{"into":"develop","status":"merged","commit":"c7"}}"#,
        )
        .expect("parse");
        assert!(merged.succeeded(), "{merged:?}");
        // The default (status absent) is a successful merge, per the envelope.
        let implied =
            parse_reply(200, br#"{"RcsMerged":{"into":"develop","commit":"c7"}}"#).expect("parse");
        assert!(implied.succeeded());
        // ...and neither refusal counts.
        for body in [
            &br#"{"RcsMerged":{"into":"d","status":"stale"}}"#[..],
            &br#"{"RcsMerged":{"into":"d","status":"conflict","conflicts":["a"]}}"#[..],
            // An unrecognized status is conservatively NOT success.
            &br#"{"RcsMerged":{"into":"d","status":"who-knows"}}"#[..],
        ] {
            assert!(
                !parse_reply(200, body).expect("parse").succeeded(),
                "{body:?}"
            );
        }
        assert_eq!(
            MergeStatus::parse("who-knows"),
            MergeStatus::Unknown("who-knows".into())
        );
        assert_eq!(MergeStatus::parse("fast-forward"), MergeStatus::Merged);
    }

    #[test]
    fn a_conflict_lists_its_paths_and_makes_no_commit() {
        let reply = parse_reply(
            200,
            br#"{"RcsMerged":{"into":"develop","status":"conflict","conflicts":["a.txt","b.rs"]}}"#,
        )
        .expect("parse");
        match reply {
            RcsReply::Merged {
                commit,
                status,
                conflicts,
                ..
            } => {
                assert_eq!(status, MergeStatus::Conflict);
                assert_eq!(conflicts, vec!["a.txt".to_string(), "b.rs".to_string()]);
                assert!(commit.is_none(), "a conflict makes no commit");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_reply_missing_its_required_array_is_refused() {
        // "No branches array" must never read as "this repository has no
        // branches" — a cell believing that would retire every room it holds.
        assert!(parse_reply(200, br#"{"RcsBranches":{"app":"a"}}"#).is_err());
        assert!(parse_reply(200, br#"{"RcsBranches":{"branches":"main"}}"#).is_err());
        assert!(parse_reply(200, br#"{"RcsLog":{"app":"a"}}"#).is_err());
        assert!(parse_reply(200, br#"{"RcsLog":{"commits":{}}}"#).is_err());
        // An EMPTY array is a legitimate answer, and stays one.
        assert_eq!(
            parse_reply(200, br#"{"RcsBranches":{"branches":[]}}"#).expect("parse"),
            RcsReply::Branches(vec![])
        );
    }

    #[test]
    fn a_transport_failure_is_never_read_as_a_refusal() {
        for status in [500u16, 502, 401, 429] {
            let err = parse_reply(status, b"{}").expect_err("must be an error");
            assert!(
                matches!(err, ProcessError::Egress(_)),
                "{status} must be transient, not a refusal: {err:?}"
            );
            assert!(!err.is_final());
        }
    }

    #[test]
    fn an_unrecognized_or_malformed_reply_is_an_egress_fault() {
        assert!(parse_reply(200, b"not json").is_err());
        assert!(parse_reply(200, br#"{"Surprise":{}}"#).is_err());
        // A branch row missing its tip must not silently become an empty tip.
        assert!(parse_reply(200, br#"{"RcsBranches":{"branches":[["main"]]}}"#).is_err());
        // A commit envelope missing its commit id.
        assert!(parse_reply(200, br#"{"RcsCommitted":{"branch":"main"}}"#).is_err());
    }
}

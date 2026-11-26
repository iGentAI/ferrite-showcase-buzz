//! The contextual-fragment framework: the recognition core compaction needs,
//! plus the environment/turn-aborted/subagent/memory fragments prompt assembly
//! uses. The `<goal_context>` marker stays in the recognition registry so an
//! injected goal nudge, if ever present, is still excluded from real-user
//! retention.

use ferrite_app::serde_json;

/// A fragment's conversational role. The integration layer maps Developer→system
/// and User→user when building gateway messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FragmentRole {
    Developer,
    User,
}

/// A rendered fragment ready to become a model message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedFragment {
    pub role: FragmentRole,
    pub text: String,
}

/// A context fragment: a role, a marker pair, and a body.
pub trait ContextFragment {
    fn role(&self) -> FragmentRole;
    fn start_marker(&self) -> &'static str;
    fn end_marker(&self) -> &'static str;
    fn body(&self) -> String;

    /// `START + body + END`, no separators.
    fn render(&self) -> String {
        format!(
            "{}{}{}",
            self.start_marker(),
            self.body(),
            self.end_marker()
        )
    }

    fn rendered(&self) -> RenderedFragment {
        RenderedFragment {
            role: self.role(),
            text: self.render(),
        }
    }
}

// Marker constants (mirror Codex `protocol/src/protocol.rs` marker consts).
pub const GOAL_CONTEXT_START: &str = "<goal_context>";
pub const GOAL_CONTEXT_END: &str = "</goal_context>";
pub const ENVIRONMENT_CONTEXT_START: &str = "<environment_context>";
pub const ENVIRONMENT_CONTEXT_END: &str = "</environment_context>";

/// Inlined from `steering/templates.rs:167` — XML text escaping.
fn escape_xml_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}

/// Network access info inside an [`EnvironmentContext`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkInfo {
    pub enabled: bool,
    pub allowed: Vec<String>,
    pub denied: Vec<String>,
}

impl NetworkInfo {
    fn render(&self) -> String {
        format!(
            "  <network enabled=\"{}\"><allowed>{}</allowed><denied>{}</denied></network>\n",
            self.enabled,
            self.allowed.join(","),
            self.denied.join(",")
        )
    }
}

/// The environment-context fragment: a user-role message describing the runtime
/// environment. Each present field is XML-escaped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvironmentContext {
    pub cwd: Option<String>,
    pub shell: Option<String>,
    pub current_date: Option<String>,
    pub timezone: Option<String>,
    pub network: Option<NetworkInfo>,
    pub subagents: Option<String>,
}

impl ContextFragment for EnvironmentContext {
    fn role(&self) -> FragmentRole {
        FragmentRole::User
    }
    fn start_marker(&self) -> &'static str {
        ENVIRONMENT_CONTEXT_START
    }
    fn end_marker(&self) -> &'static str {
        ENVIRONMENT_CONTEXT_END
    }
    fn body(&self) -> String {
        let mut s = String::from("\n");
        if let Some(v) = &self.cwd {
            s.push_str(&format!("  <cwd>{}</cwd>\n", escape_xml_text(v)));
        }
        if let Some(v) = &self.shell {
            s.push_str(&format!("  <shell>{}</shell>\n", escape_xml_text(v)));
        }
        if let Some(v) = &self.current_date {
            s.push_str(&format!(
                "  <current_date>{}</current_date>\n",
                escape_xml_text(v)
            ));
        }
        if let Some(v) = &self.timezone {
            s.push_str(&format!("  <timezone>{}</timezone>\n", escape_xml_text(v)));
        }
        if let Some(n) = &self.network {
            s.push_str(&n.render());
        }
        if let Some(v) = &self.subagents {
            s.push_str(&format!(
                "  <subagents>{}</subagents>\n",
                escape_xml_text(v)
            ));
        }
        s
    }
}

pub const TURN_ABORTED_START: &str = "<turn_aborted>";
pub const TURN_ABORTED_END: &str = "</turn_aborted>";
pub const SUBAGENT_NOTIFICATION_START: &str = "<subagent_notification>";
pub const SUBAGENT_NOTIFICATION_END: &str = "</subagent_notification>";
pub const MEMORY_INSTRUCTIONS_START: &str = "<memory_instructions>";
pub const MEMORY_INSTRUCTIONS_END: &str = "</memory_instructions>";

/// Verbatim guidance for the user-role turn-aborted fragment.
pub const INTERRUPTED_GUIDANCE: &str = "The user interrupted the previous turn on purpose. Any running unified exec processes may still be running in the background. If any tools/commands were aborted, they may have partially executed.";
/// Verbatim guidance for the developer-role variant.
pub const INTERRUPTED_DEVELOPER_GUIDANCE: &str = "The previous turn was interrupted on purpose. Any running unified exec processes may still be running in the background. If any tools/commands were aborted, they may have partially executed.";

/// The turn-aborted fragment, injected after a user interrupt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnAborted {
    ContextualUser,
    Developer,
}

impl ContextFragment for TurnAborted {
    fn role(&self) -> FragmentRole {
        match self {
            TurnAborted::ContextualUser => FragmentRole::User,
            TurnAborted::Developer => FragmentRole::Developer,
        }
    }
    fn start_marker(&self) -> &'static str {
        TURN_ABORTED_START
    }
    fn end_marker(&self) -> &'static str {
        TURN_ABORTED_END
    }
    fn body(&self) -> String {
        let guidance = match self {
            TurnAborted::ContextualUser => INTERRUPTED_GUIDANCE,
            TurnAborted::Developer => INTERRUPTED_DEVELOPER_GUIDANCE,
        };
        format!("\n{guidance}\n")
    }
}

/// The subagent-notification fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentNotification {
    pub agent_path: String,
    pub status: String,
}

impl ContextFragment for SubagentNotification {
    fn role(&self) -> FragmentRole {
        FragmentRole::User
    }
    fn start_marker(&self) -> &'static str {
        SUBAGENT_NOTIFICATION_START
    }
    fn end_marker(&self) -> &'static str {
        SUBAGENT_NOTIFICATION_END
    }
    fn body(&self) -> String {
        let json = serde_json::json!({ "agent_path": self.agent_path, "status": self.status });
        format!("\n{json}\n")
    }
}

/// The memory read-path fragment: a developer-role policy message carrying the
/// memory-usage policy plus the budgeted recall preamble.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryReadPolicy {
    /// The memory-usage policy text.
    pub policy: String,
    /// The budgeted recall preamble.
    pub recall_preamble: String,
}

impl ContextFragment for MemoryReadPolicy {
    fn role(&self) -> FragmentRole {
        FragmentRole::Developer
    }
    fn start_marker(&self) -> &'static str {
        MEMORY_INSTRUCTIONS_START
    }
    fn end_marker(&self) -> &'static str {
        MEMORY_INSTRUCTIONS_END
    }
    fn body(&self) -> String {
        if self.recall_preamble.trim().is_empty() {
            format!("\n{}\n", self.policy)
        } else {
            format!("\n{}\n\n{}\n", self.policy, self.recall_preamble)
        }
    }
}

/// The marker pairs of all recognizable injected fragments. Used to distinguish
/// machine-injected context from real user input.
const RECOGNIZED_MARKERS: &[(&str, &str)] = &[
    (GOAL_CONTEXT_START, GOAL_CONTEXT_END),
    (ENVIRONMENT_CONTEXT_START, ENVIRONMENT_CONTEXT_END),
    (TURN_ABORTED_START, TURN_ABORTED_END),
    (SUBAGENT_NOTIFICATION_START, SUBAGENT_NOTIFICATION_END),
    (MEMORY_INSTRUCTIONS_START, MEMORY_INSTRUCTIONS_END),
];

/// Whether `text` matches a recognizable injected-fragment envelope. A match
/// requires non-empty markers and the (whitespace-trimmed) text to both start
/// with the start marker and end with the end marker.
pub fn is_injected_fragment(text: &str) -> bool {
    let trimmed = text.trim();
    RECOGNIZED_MARKERS.iter().any(|(start, end)| {
        !start.is_empty() && !end.is_empty() && trimmed.starts_with(start) && trimmed.ends_with(end)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_context_renders_present_fields_only() {
        let env = EnvironmentContext {
            cwd: Some("/work/repo".into()),
            shell: Some("bash".into()),
            current_date: Some("2026-06-28".into()),
            timezone: Some("UTC".into()),
            network: Some(NetworkInfo {
                enabled: true,
                allowed: vec!["a.com".into(), "b.com".into()],
                denied: vec!["c.com".into()],
            }),
            subagents: None,
        };
        let r = env.rendered();
        assert_eq!(r.role, FragmentRole::User);
        assert!(r
            .text
            .starts_with("<environment_context>\n  <cwd>/work/repo</cwd>\n"));
        assert!(r.text.contains("  <shell>bash</shell>\n"));
        assert!(r.text.contains("<network enabled=\"true\"><allowed>a.com,b.com</allowed><denied>c.com</denied></network>"));
        assert!(!r.text.contains("<subagents>"));
        assert!(r.text.ends_with("</environment_context>"));
        assert!(is_injected_fragment(&r.text));
    }

    #[test]
    fn environment_context_escapes_fields() {
        let env = EnvironmentContext {
            cwd: Some("/work/<evil>&".into()),
            ..Default::default()
        };
        assert!(env.rendered().text.contains("&lt;evil&gt;&amp;"));
    }

    #[test]
    fn turn_aborted_variants_have_verbatim_guidance_and_roles() {
        let u = TurnAborted::ContextualUser.rendered();
        assert_eq!(u.role, FragmentRole::User);
        assert_eq!(
            u.text,
            format!("<turn_aborted>\n{INTERRUPTED_GUIDANCE}\n</turn_aborted>")
        );
        let d = TurnAborted::Developer.rendered();
        assert_eq!(d.role, FragmentRole::Developer);
        assert!(d.text.contains(INTERRUPTED_DEVELOPER_GUIDANCE));
        assert!(is_injected_fragment(&u.text));
    }

    #[test]
    fn subagent_notification_emits_json_body() {
        let f = SubagentNotification {
            agent_path: "a/b".into(),
            status: "done".into(),
        };
        let r = f.rendered();
        assert_eq!(r.role, FragmentRole::User);
        assert!(r.text.contains("\"agent_path\":\"a/b\""));
        assert!(r.text.contains("\"status\":\"done\""));
        assert!(is_injected_fragment(&r.text));
    }

    #[test]
    fn memory_read_policy_includes_preamble_when_present() {
        let with = MemoryReadPolicy {
            policy: "POLICY".into(),
            recall_preamble: "RECALL".into(),
        };
        let r = with.rendered();
        assert_eq!(r.role, FragmentRole::Developer);
        assert!(r
            .text
            .starts_with("<memory_instructions>\nPOLICY\n\nRECALL\n"));
        assert!(r.text.ends_with("</memory_instructions>"));
        let without = MemoryReadPolicy {
            policy: "POLICY".into(),
            recall_preamble: "  ".into(),
        };
        assert_eq!(
            without.rendered().text,
            "<memory_instructions>\nPOLICY\n</memory_instructions>"
        );
    }

    #[test]
    fn recognition_distinguishes_injected_from_user_input() {
        assert!(is_injected_fragment(
            "  <environment_context>\nx\n</environment_context>  "
        ));
        assert!(is_injected_fragment("<goal_context>\nx\n</goal_context>"));
        assert!(!is_injected_fragment("please continue working on the goal"));
        assert!(!is_injected_fragment(
            "I typed <goal_context> in my message"
        ));
    }
}

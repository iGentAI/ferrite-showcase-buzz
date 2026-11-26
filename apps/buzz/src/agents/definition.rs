//! AgentDefinition v1 — the compiled agent-pack document that hydrates a turn
//! (see `docs/_archive/agent-pack-definition.md`). A definition is a REQUEST, not a
//! grant: it can only narrow what the app already does (filter the tool
//! surface, lower the step budget, swap prompt text, pick a default model).
//! It cannot add bindings, grant egress, or raise platform bounds.
//!
//! Fail-safe contract: an absent, oversized, unparseable, wrong-version,
//! unknown-field, or bound-violating document is ignored ENTIRELY — the turn
//! runs byte-identical to the no-definition path. Partial application is
//! forbidden; the tests below defend each invalid-document class.

use ferrite_app::serde::Deserialize;
use ferrite_app::serde_json;
use ferrite_app::AppContext;

use crate::agents::agent_loop::ToolRegistry;
use crate::agents::error::Result;
use crate::agents::tools::{ToolSchema, MAX_TOOL_OUTPUT};
use crate::agents::turns::clean_model_id;

/// The only schema version this loader accepts.
pub const AGENT_DEF_VERSION: u64 = 1;
/// The platform per-turn step bound every tier validates definitions against
/// (equals the deployed app's `MAX_STEPS`; a definition can lower it, never
/// raise it). Node-tier loaders use this same
/// constant so a pack accepted on one tier is accepted on the other.
///
/// Eight is below the floor for the work this agent is asked to do: checking out
/// an app's source, editing it, building, reading a failure, fixing it and
/// deploying is more than eight tool calls, so a turn at that bound spends its
/// budget orienting and dies with `tool loop exceeded 8 steps` before reaching
/// the task — a failure that reads like a broken agent rather than a budget.
///
/// It is a COST ceiling, not a safety one: every step is a model round trip, so
/// this is the most a single turn may spend. The bounds that keep a turn safe are
/// elsewhere — per-tool output caps, the worker's fuel and egress
/// timeouts, approval gating on mutations, and the run_code dedup cache. Lower it
/// per agent with `limits.max_steps` when a cheap agent should stay cheap.
///
/// The node tier's default is 24 (`ferrite_agents_web::turn::DEFAULT_MAX_STEPS`).
/// 32 keeps the deployed ceiling at or above the tier that runs the same packs,
/// so a lower cap does not silently clamp that default.
pub const PLATFORM_MAX_STEPS_CAP: usize = 32;
/// Whole-document byte cap (well under the 256 KiB session-state and 64 KiB
/// event caps so a definition can never be the thing that trips a plane bound).
pub const MAX_DEFINITION_BYTES: usize = 48 * 1024;
/// Instructions byte cap.
pub const MAX_INSTRUCTIONS_BYTES: usize = 32 * 1024;
/// The protected tool: always shown, always dispatchable, undeniable. The
/// base prompt tells the model to call `end_turn` when work is complete, so
/// filtering it would route prompted calls into the soft `unknown tool`
/// result and burn steps. It is a prompt-level signal only — the turn
/// actually finalizes on the tool-call-free completion
/// ([`ModelStep::Final`](crate::agents::agent_loop::ModelStep)), not on this tool.
pub const PROTECTED_TOOL: &str = "end_turn";
/// The skill-loading tool: permitted exactly when the definition
/// declares skills — the declaration IS the grant, so allow-lists need not
/// name it and a document that declares skills while denying it is
/// contradictory (refused whole).
pub const LOAD_SKILL_TOOL: &str = "load_skill";
/// The question-asking tool, available when `ask_question` is set.
pub const ASK_QUESTION_TOOL: &str = "ask_question";
/// Most tools one definition may gate behind approval.
pub const MAX_APPROVALS: usize = 32;
/// Most schedules one definition may declare.
pub const MAX_SCHEDULES: usize = 32;
/// A scheduled prompt byte cap (well under a turn message).
pub const MAX_SCHEDULE_PROMPT_BYTES: usize = 8 * 1024;

/// The delegation tool (subagents): permitted exactly when the definition
/// declares subagents — the declaration IS the grant, like `load_skill`.
pub const DELEGATE_TOOL: &str = "delegate";
/// Most subagents one parent may declare. Equal to the parent step cap, so a
/// parent can never name more children than it has steps to reach.
pub const MAX_SUBAGENTS: usize = 8;
/// Subagent id byte cap. Keeps the derived child scope agent
/// `{parent≤64}.sub.{id≤32}` at most 101 bytes, under
/// `ferrite_agents::MAX_SCOPE_FIELD_BYTES` (128) with margin.
pub const MAX_SUBAGENT_ID_BYTES: usize = 32;
/// A subagent's instructions byte cap: half the parent's, so several children
/// fit under the whole-document cap.
pub const MAX_SUBAGENT_INSTRUCTIONS_BYTES: usize = 16 * 1024;
/// A child turn's step ceiling — strictly below the parent's
/// [`PLATFORM_MAX_STEPS_CAP`] so a child costs less than a top-level turn.
///
/// Worst case per parent turn is bounded by
/// `MAX_DELEGATIONS_PER_TURN` — 32 parent steps plus 4 × 8 child steps.
pub const PLATFORM_MAX_SUBAGENT_STEPS_CAP: usize = 8;
/// Most `delegate` calls one parent turn may execute (fan-out cap). A delegate
/// is also one of the parent's `≤ MAX_STEPS` steps, so this only tightens.
pub const MAX_DELEGATIONS_PER_TURN: usize = 4;

/// One declared schedule: a cron expression and the prompt the
/// agent runs when it fires. Task-mode only (Eve's `{cron, markdown}`); the
/// prompt drives a normal turn. Live firing is a `ferrite-workflows` cron
/// definition that dispatches this schedule; the dispatch path is the same
/// one an operator/handler calls.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(crate = "ferrite_app::serde", deny_unknown_fields)]
pub struct ScheduleSpec {
    pub name: String,
    /// Standard 5-field cron. Shape-validated here; the workflows engine owns
    /// the semantics/timezone.
    pub cron: String,
    /// The prompt the scheduled turn runs.
    pub prompt: String,
}

/// Whether a string is a shape-valid 5-field cron expression (minute hour dom
/// month dow). Field semantics/ranges are the workflows engine's job; this
/// only rejects obvious garbage so a bad value fails at compile/seed, not at
/// fire time.
fn valid_cron(expr: &str) -> bool {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    fields.len() == 5
        && fields.iter().all(|f| {
            !f.is_empty()
                && f.chars()
                    .all(|c| c.is_ascii_digit() || matches!(c, '*' | '/' | ',' | '-'))
        })
}
/// Most skills one definition may declare.
pub const MAX_SKILLS: usize = 32;
/// Store key for one skill document (beside the definition, same store).
pub fn skill_key(agent: &str, skill: &str) -> Vec<u8> {
    format!("agent-skill:v1:{agent}:{skill}").into_bytes()
}

/// Most pack-defined tools one definition may declare.
pub const MAX_DEFINED_TOOLS: usize = 16;
/// Per-tool description byte cap.
pub const MAX_TOOL_DESCRIPTION_BYTES: usize = 1024;
/// Most egress hosts one defined tool may request.
pub const MAX_TOOL_EGRESS_HOSTS: usize = 8;
/// The worker's default fuel budget; a defined tool may only lower it.
pub const MAX_TOOL_FUEL: u64 = 500_000_000;

/// Store key for an agent's definition document (in the `web-config` store,
/// beside `agents-client-token`).
pub fn agent_def_key(agent: &str) -> Vec<u8> {
    format!("agent-def:v1:{agent}").into_bytes()
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(crate = "ferrite_app::serde", deny_unknown_fields)]
pub struct AgentLimits {
    #[serde(default)]
    pub max_steps: Option<u64>,
}

/// One pack-defined tool: metadata only — the compiled snippet
/// lives in its own store document (`agent-tool:v1:{agent}:{name}`), loaded
/// server-side and never model-visible.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(crate = "ferrite_app::serde", deny_unknown_fields)]
pub struct DefinedTool {
    pub name: String,
    pub description: String,
    pub input_schema: ferrite_app::serde_json::Value,
    /// Outbound hosts the snippet may `fetch`; absent = no network. The app
    /// clamps the worker request to exactly this list — a definition can
    /// never widen egress beyond what the worker's own gate allows.
    #[serde(default)]
    pub egress_hosts: Option<Vec<String>>,
    /// Fuel budget for the snippet; absent = the worker default. May only
    /// lower the budget.
    #[serde(default)]
    pub fuel: Option<u64>,
    /// Runtime-only logical store grants lowered from AgentBundle v2. These
    /// never deserialize from the v1 document and therefore cannot widen the
    /// v1 format's authority surface.
    #[serde(skip)]
    pub store_grants: Vec<ToolStoreGrant>,
    /// Runtime-only logical blob grants lowered from AgentBundle v2.
    #[serde(skip)]
    pub blob_grants: Vec<ToolBlobGrant>,
    /// Runtime callback names requested by this tool. The worker still
    /// requires a concrete handler; a name alone never creates authority.
    #[serde(skip)]
    pub callbacks: Vec<String>,
    #[serde(skip)]
    pub required_capabilities: Vec<String>,
    #[serde(skip)]
    pub optional_capabilities: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolStoreGrant {
    pub name: String,
    pub prefix: String,
    pub read: bool,
    pub write: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolBlobGrant {
    pub name: String,
    pub prefix: String,
    pub read: bool,
    pub write: bool,
}

/// One declared subagent (delegation target). Inline in the parent definition
/// and synthesized into an in-memory child [`AgentDefinition`] at delegate
/// time — never stored or loaded from the config store. It inherits NOTHING
/// from the parent: its own identity (`instructions`), its own model, and a
/// toolset that is only a NARROWING of the built-in surface. It deliberately
/// has no `subagents` field — that is the type-level depth-1 guarantee (a
/// child can never delegate further).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(crate = "ferrite_app::serde", deny_unknown_fields)]
pub struct SubagentSpec {
    /// Child id; `valid_tool_name` shape and `≤ MAX_SUBAGENT_ID_BYTES`. The
    /// derived child scope agent is `{parent}.sub.{id}`.
    pub id: String,
    /// The child's system prompt (required, non-blank).
    pub instructions: String,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub tool_allow: Option<Vec<String>>,
    #[serde(default)]
    pub tool_deny: Vec<String>,
    #[serde(default)]
    pub limits: AgentLimits,
}

/// AgentDefinition v1. `deny_unknown_fields` is the version discipline:
/// additions bump `version` rather than slipping past old loaders.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(crate = "ferrite_app::serde", deny_unknown_fields)]
pub struct AgentDefinition {
    pub version: u64,
    pub agent_id: String,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub tool_allow: Option<Vec<String>>,
    #[serde(default)]
    pub tool_deny: Vec<String>,
    /// Skill names this agent may load. Each name has a matching
    /// `agent-skill:v1:{agent}:{name}` document seeded beside this one; the
    /// compiler emits both so they cannot drift.
    #[serde(default)]
    pub skills: Vec<String>,
    /// Pack-defined tools: declared here, code in sibling store
    /// documents. Declaration is the grant (like skills) — but this bundle
    /// has no worker code-mode sibling, so executing one answers an honest
    /// "not available on this deployment" (see [`crate::agents`]).
    #[serde(default)]
    pub tools: Vec<DefinedTool>,
    /// Tools that require a durable human approval before they run (Phase 6,
    /// HITL). A call to one of these without a recorded grant returns a soft
    /// "approval required" result; the model relays it and ends the turn
    /// (park at a step boundary, zero compute). A structured approve action
    /// records an `approval_granted` session event; the next turn re-issues
    /// the call and it runs. No timers, no mid-step continuation tokens.
    #[serde(default)]
    pub approvals: Vec<String>,
    /// Whether the `ask_question` tool is available: the model
    /// asks the user something and ends the turn; the answer arrives as the
    /// next message. Opt-in so the no-definition tool surface is unchanged.
    #[serde(default)]
    pub ask_question: bool,
    /// Cron-scheduled prompts. Declaration only; live firing is a
    /// ferrite-workflows cron definition that dispatches the schedule.
    #[serde(default)]
    pub schedules: Vec<ScheduleSpec>,
    /// Delegation targets (subagents). Declaration is the grant for the
    /// `delegate` tool. Empty (the default) → the compiler omits the key and
    /// the no-subagents turn is byte-identical.
    #[serde(default)]
    pub subagents: Vec<SubagentSpec>,
    #[serde(default)]
    pub limits: AgentLimits,
    /// Set only when this definition was lowered from an authenticated v2
    /// bundle. Runtime-only metadata: never accepted from v1 JSON.
    #[serde(skip)]
    pub bundle_generation: Option<String>,
    /// Logical required capabilities declared by AgentBundle v2.
    #[serde(skip)]
    pub capability_required: Vec<String>,
    /// Logical optional capabilities declared by AgentBundle v2.
    #[serde(skip)]
    pub capability_optional: Vec<String>,
    /// Optional capabilities absent from the delivered runtime handles.
    #[serde(skip)]
    pub capability_unavailable: Vec<String>,
}

fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Parse and validate one definition document. `None` means "inert": the
/// caller MUST behave exactly as if no document existed. Every rule here has
/// a matching compiler refusal in the definition compiler that produced these
/// documents — the compiler is the loud gate, this is the quiet one. NOTE: plan
/// 031 retired `tools/agent-pack-compiler`, so this v2 persona format currently
/// has NO compiler in this repository; the loader is the only gate that remains,
/// which is why it refuses rather than repairs.
pub fn parse_agent_definition(
    bytes: &[u8],
    expected_agent: &str,
    max_steps_cap: usize,
) -> Option<AgentDefinition> {
    if bytes.len() > MAX_DEFINITION_BYTES {
        return None;
    }
    let mut def: AgentDefinition = ferrite_app::serde_json::from_slice(bytes).ok()?;
    if def.version != AGENT_DEF_VERSION || def.agent_id != expected_agent {
        return None;
    }
    if let Some(instructions) = &def.instructions {
        if instructions.len() > MAX_INSTRUCTIONS_BYTES {
            return None;
        }
    }
    if let Some(model) = &def.default_model {
        // Normalize through the same rules request models pass.
        def.default_model = Some(clean_model_id(model)?);
    }
    fn unique(names: &[String]) -> bool {
        let mut seen = std::collections::BTreeSet::new();
        names.iter().all(|n| seen.insert(n.as_str()))
    }
    if let Some(allow) = &def.tool_allow {
        if allow.is_empty() || !allow.iter().all(|t| valid_tool_name(t)) || !unique(allow) {
            return None;
        }
    }
    if !def.tool_deny.iter().all(|t| valid_tool_name(t)) || !unique(&def.tool_deny) {
        return None;
    }
    if def.tool_deny.iter().any(|t| t == PROTECTED_TOOL) {
        return None;
    }
    if !def.skills.is_empty() {
        if def.skills.len() > MAX_SKILLS {
            return None;
        }
        if !def.skills.iter().all(|s| valid_tool_name(s)) {
            return None;
        }
        let mut seen = std::collections::BTreeSet::new();
        if !def.skills.iter().all(|s| seen.insert(s.as_str())) {
            return None;
        }
        // Declaring skills while denying the loader is contradictory.
        if def.tool_deny.iter().any(|t| t == LOAD_SKILL_TOOL) {
            return None;
        }
    }
    if !def.tools.is_empty() {
        if def.tools.len() > MAX_DEFINED_TOOLS {
            return None;
        }
        let mut seen = std::collections::BTreeSet::new();
        // Every name the deployed surface can advertise, not only the coding
        // tools: a pack tool named like a buzz, schedule, web, MCP or Fabric
        // tool would otherwise be "granted" by its declaration and dispatch
        // to the real builtin past the document's own allow-list.
        let builtin_names = builtin_tool_names();
        for tool in &def.tools {
            if !valid_tool_name(&tool.name)
                || !seen.insert(tool.name.as_str())
                || builtin_names.contains(&tool.name)
            {
                return None;
            }
            if tool.description.trim().is_empty()
                || tool.description.len() > MAX_TOOL_DESCRIPTION_BYTES
            {
                return None;
            }
            if !tool.input_schema.is_object() {
                return None;
            }
            if let Some(hosts) = &tool.egress_hosts {
                if hosts.is_empty()
                    || hosts.len() > MAX_TOOL_EGRESS_HOSTS
                    || !hosts.iter().all(|h| valid_egress_host(h))
                {
                    return None;
                }
            }
            if let Some(fuel) = tool.fuel {
                if fuel == 0 || fuel > MAX_TOOL_FUEL {
                    return None;
                }
            }
            // Denying a tool the document itself defines is contradictory.
            if def.tool_deny.iter().any(|t| t == &tool.name) {
                return None;
            }
        }
    }
    if !def.approvals.is_empty() {
        if def.approvals.len() > MAX_APPROVALS {
            return None;
        }
        let mut seen = std::collections::BTreeSet::new();
        if !def
            .approvals
            .iter()
            .all(|t| valid_tool_name(t) && seen.insert(t.as_str()))
        {
            return None;
        }
        // Gating the loop-completion tool behind approval would deadlock the
        // turn; ask_question is itself the park mechanism.
        if def
            .approvals
            .iter()
            .any(|t| t == PROTECTED_TOOL || t == ASK_QUESTION_TOOL)
        {
            return None;
        }
    }
    if !def.schedules.is_empty() {
        if def.schedules.len() > MAX_SCHEDULES {
            return None;
        }
        let mut seen = std::collections::BTreeSet::new();
        for s in &def.schedules {
            if !valid_tool_name(&s.name)
                || !seen.insert(s.name.as_str())
                || !valid_cron(&s.cron)
                || s.prompt.trim().is_empty()
                || s.prompt.len() > MAX_SCHEDULE_PROMPT_BYTES
            {
                return None;
            }
        }
    }
    if !def.subagents.is_empty() {
        if def.subagents.len() > MAX_SUBAGENTS {
            return None;
        }
        // Declaring subagents while denying the delegate tool is contradictory
        // (the declaration IS the grant).
        if def.tool_deny.iter().any(|t| t == DELEGATE_TOOL) {
            return None;
        }
        let mut seen = std::collections::BTreeSet::new();
        for s in &mut def.subagents {
            if !valid_tool_name(&s.id) || s.id.len() > MAX_SUBAGENT_ID_BYTES {
                return None;
            }
            if !seen.insert(s.id.clone()) {
                return None;
            }
            if s.instructions.trim().is_empty()
                || s.instructions.len() > MAX_SUBAGENT_INSTRUCTIONS_BYTES
            {
                return None;
            }
            if let Some(m) = &s.default_model {
                s.default_model = Some(clean_model_id(m)?);
            }
            if let Some(allow) = &s.tool_allow {
                if allow.is_empty() || !allow.iter().all(|t| valid_tool_name(t)) || !unique(allow) {
                    return None;
                }
            }
            if !s.tool_deny.iter().all(|t| valid_tool_name(t)) || !unique(&s.tool_deny) {
                return None;
            }
            // A child cannot deny its own loop-completion tool.
            if s.tool_deny.iter().any(|t| t == PROTECTED_TOOL) {
                return None;
            }
            // Child step budget is bounded by the CHILD cap, not the parent's.
            if let Some(steps) = s.limits.max_steps {
                if steps == 0 || steps > PLATFORM_MAX_SUBAGENT_STEPS_CAP as u64 {
                    return None;
                }
            }
        }
    }
    if let Some(steps) = def.limits.max_steps {
        if steps == 0 || steps > max_steps_cap as u64 {
            return None;
        }
    }
    Some(def)
}

/// Bare hostname (optionally with a port); never a scheme or path.
fn valid_egress_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 260
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'))
        && !host.contains("://")
        && !host.contains('/')
}

impl AgentDefinition {
    /// Whether `tool` may be shown to the model and dispatched. Deny wins over
    /// allow; the protected tool is always permitted; the skill loader is
    /// permitted exactly when skills are declared (the declaration is the
    /// grant — allow-lists need not name it).
    pub fn permits(&self, tool: &str) -> bool {
        if self.tool_capability_unavailable(tool) {
            return false;
        }
        if tool == PROTECTED_TOOL {
            return true;
        }
        if tool == LOAD_SKILL_TOOL {
            return !self.skills.is_empty();
        }
        if tool == ASK_QUESTION_TOOL {
            // Declared AND not denied: deny wins over this grant too, so a
            // document that both declares and denies it never offers it.
            return self.ask_question && !self.tool_deny.iter().any(|t| t == tool);
        }
        // Delegation: declaring subagents is the grant (like load_skill). A
        // synthesized child has no subagents, so a child never sees delegate.
        if tool == DELEGATE_TOOL {
            return !self.subagents.is_empty();
        }
        // Deny wins over every grant below, including a declaration.
        if self.tool_deny.iter().any(|t| t == tool) {
            return false;
        }
        // Pack-defined tools: the declaration is the grant.
        if self.tools.iter().any(|t| t.name == tool) {
            return true;
        }
        match &self.tool_allow {
            Some(allow) => allow.iter().any(|t| t == tool),
            None => true,
        }
    }

    fn tool_capability_unavailable(&self, tool: &str) -> bool {
        if let Some(defined) = self.tools.iter().find(|item| item.name == tool) {
            // An authored tool runs as code: "code" unavailable disables it even
            // when its own capability lists do not repeat it.
            return self.capability_unavailable.iter().any(|item| item == "code")
                || defined
                    .required_capabilities
                    .iter()
                    .chain(defined.optional_capabilities.iter())
                    .any(|name| self.capability_unavailable.iter().any(|item| item == name));
        }
        let required = if self.tools.iter().any(|item| item.name == tool) || tool == "run_code" {
            Some("code")
        } else if crate::agents::tools::is_memory_tool(tool) {
            Some("memory")
        } else if tool.starts_with("workflow_") {
            Some("workflows")
        } else if crate::agents::tools::is_web_tool(tool) {
            // The same predicate the registry dispatches on, so the two lists
            // cannot drift apart.
            Some("web")
        } else if tool.starts_with("ferrite_") {
            Some("ferrite_control")
        } else if tool.starts_with("buzz_") {
            Some("buzz")
        } else {
            None
        };
        required.is_some_and(|name| self.capability_unavailable.iter().any(|item| item == name))
    }

    /// The pack-defined tool named `tool`, if declared.
    pub fn defined_tool(&self, tool: &str) -> Option<&DefinedTool> {
        self.tools.iter().find(|t| t.name == tool)
    }

    /// Whether `tool` requires a human approval grant before it runs.
    pub fn requires_approval(&self, tool: &str) -> bool {
        self.approvals.iter().any(|t| t == tool)
    }

    /// The declared schedule with this name, if any.
    pub fn schedule(&self, name: &str) -> Option<&ScheduleSpec> {
        self.schedules.iter().find(|s| s.name == name)
    }

    /// The declared subagent with this id, if any.
    pub fn subagent(&self, id: &str) -> Option<&SubagentSpec> {
        self.subagents.iter().find(|s| s.id == id)
    }

    /// Model-visible schemas for the pack-defined tools.
    pub fn defined_tool_schemas(&self) -> Vec<ToolSchema> {
        self.tools
            .iter()
            .map(|t| ToolSchema {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: t.input_schema.clone(),
            })
            .collect()
    }

    /// The model-visible tool surface under this definition.
    pub fn filter_schemas(&self, schemas: Vec<ToolSchema>) -> Vec<ToolSchema> {
        schemas
            .into_iter()
            .filter(|s| self.permits(&s.name))
            .collect()
    }

    /// The effective per-turn step budget: never above the platform cap.
    pub fn effective_max_steps(&self, cap: usize) -> usize {
        match self.limits.max_steps {
            Some(steps) => (steps as usize).min(cap),
            None => cap,
        }
    }

    /// The part of this definition's tool policy every delegated child
    /// inherits (see [`synth_child_def`]).
    pub fn child_policy(&self) -> InheritedToolPolicy {
        InheritedToolPolicy {
            approvals: self.approvals.clone(),
            tool_allow: self.tool_allow.clone(),
            tool_deny: self.tool_deny.clone(),
            capability_unavailable: self.capability_unavailable.clone(),
        }
    }
}

/// Every tool name the turn pipeline can advertise from its own code: the
/// coding, buzz, schedule, web, MCP and Fabric A2A tools plus the
/// definition-gated ones. A pack-defined tool may not reuse any of them.
fn builtin_tool_names() -> std::collections::BTreeSet<String> {
    use crate::agents::tools::{
        buzz_tool_schemas, coding_tool_schemas, fabric_a2a_tool_schemas, mcp_tool_schemas,
        schedule_tool_schemas, web_tool_schemas,
    };
    let mut names: std::collections::BTreeSet<String> = coding_tool_schemas()
        .into_iter()
        .chain(buzz_tool_schemas())
        .chain(schedule_tool_schemas())
        .chain(web_tool_schemas())
        .chain(mcp_tool_schemas())
        .chain(fabric_a2a_tool_schemas())
        .map(|s| s.name)
        .collect();
    for name in [
        PROTECTED_TOOL,
        LOAD_SKILL_TOOL,
        ASK_QUESTION_TOOL,
        DELEGATE_TOOL,
    ] {
        names.insert(name.to_string());
    }
    names
}

/// The parent's tool policy a delegated child inherits, so delegation can
/// only narrow what the parent itself may do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InheritedToolPolicy {
    /// The parent's approval gate (see [`synth_child_def`]).
    pub approvals: Vec<String>,
    /// The parent's allow-list; the child's own allow-list is intersected
    /// with it.
    pub tool_allow: Option<Vec<String>>,
    /// The parent's deny-list; added to the child's own.
    pub tool_deny: Vec<String>,
    /// Capabilities unavailable to the parent, and so to the child.
    pub capability_unavailable: Vec<String>,
}

/// Synthesize the in-memory child definition a `delegate` call runs. The child
/// gets its own `agent_id` (the server-derived `{parent}.sub.{id}`), its own
/// instructions and model, and a toolset that is only a narrowing of the
/// built-in surface. `subagents` is forced empty — the depth-1 guarantee, so
/// `permits(DELEGATE_TOOL)` is false for the child and a grandchild can never
/// spawn. Step budget is clamped to [`PLATFORM_MAX_SUBAGENT_STEPS_CAP`]
/// (defaulting to it when unspecified).
///
/// `inherited.approvals` is the PARENT's approval list, propagated so the HITL
/// gate is NOT bypassable through delegation: a child is single-shot with no
/// human to relay to and no grants recorded on its own thread, so an
/// approval-gated tool always soft-parks (`requires_approval` true, `granted`
/// empty) and returns an `approval_required` result the parent relays — the
/// child can never EXECUTE a parent-gated tool, it must escalate back to the
/// parent (where the human approves). Without this, a child's empty approvals +
/// full built-in surface would let the model route a gated action (e.g.
/// `ferrite_deploy_source`) through `delegate` to escape the gate.
///
/// The rest of the parent's tool policy is inherited for the same reason: the
/// child's allow-list is intersected with the parent's (the parent's applies
/// when the child declares none), the deny-lists are united, and the parent's
/// unavailable capabilities stay unavailable. A child can therefore never run
/// a tool its parent's definition filters out.
pub fn synth_child_def(
    spec: &SubagentSpec,
    child_agent: String,
    inherited: InheritedToolPolicy,
) -> AgentDefinition {
    let steps = spec
        .limits
        .max_steps
        .unwrap_or(PLATFORM_MAX_SUBAGENT_STEPS_CAP as u64)
        .min(PLATFORM_MAX_SUBAGENT_STEPS_CAP as u64);
    let tool_allow = match (inherited.tool_allow, spec.tool_allow.clone()) {
        (None, child) => child,
        (Some(parent), None) => Some(parent),
        (Some(parent), Some(child)) => Some(
            child
                .into_iter()
                .filter(|t| parent.iter().any(|p| p == t))
                .collect(),
        ),
    };
    let mut tool_deny = spec.tool_deny.clone();
    for t in inherited.tool_deny {
        if !tool_deny.contains(&t) {
            tool_deny.push(t);
        }
    }
    AgentDefinition {
        version: AGENT_DEF_VERSION,
        agent_id: child_agent,
        instructions: Some(spec.instructions.clone()),
        default_model: spec.default_model.clone(),
        tool_allow,
        tool_deny,
        skills: Vec::new(),
        tools: Vec::new(),
        approvals: inherited.approvals,
        ask_question: false,
        schedules: Vec::new(),
        subagents: Vec::new(),
        limits: AgentLimits {
            max_steps: Some(steps),
        },
        bundle_generation: None,
        capability_required: Vec::new(),
        capability_optional: Vec::new(),
        capability_unavailable: inherited.capability_unavailable,
    }
}

/// Load the agent's definition from the config store. `Ok(None)` means no
/// definition is stored; an unreadable store or an unusable stored definition
/// is an error, so a caller fails closed instead of running the persona
/// without its guardrails.
pub async fn load_agent_definition(
    ctx: &AppContext,
    store_handle: &str,
    agent: &str,
    max_steps_cap: usize,
) -> std::result::Result<Option<AgentDefinition>, crate::agents::bundle::DefinitionLoadError> {
    crate::agents::bundle::load_agent_definition_dual(ctx, store_handle, agent, max_steps_cap).await
}

/// A [`ToolRegistry`] view that enforces a definition's tool filter at
/// dispatch (defense in depth beside the prompt-surface filter). With no
/// definition it is a pure pass-through. A filtered tool returns the same
/// soft `unknown tool: {name}` result a nonexistent tool returns from
/// [`CloudToolRegistry`](crate::agents::tools::CloudToolRegistry) — filtered and
/// nonexistent are indistinguishable to the model.
pub struct FilteredTools<'a, T> {
    inner: &'a T,
    def: Option<&'a AgentDefinition>,
    /// Tools with a recorded human approval grant for this thread,
    /// read from the session history at turn start. A required tool not in
    /// this set is soft-parked.
    granted: std::collections::BTreeSet<String>,
}

impl<'a, T> FilteredTools<'a, T> {
    pub fn new(inner: &'a T, def: Option<&'a AgentDefinition>) -> Self {
        Self {
            inner,
            def,
            granted: std::collections::BTreeSet::new(),
        }
    }

    /// Supply the approval grants recorded for this thread.
    pub fn with_granted_approvals(mut self, granted: std::collections::BTreeSet<String>) -> Self {
        self.granted = granted;
        self
    }

    fn soft(value: ferrite_app::serde_json::Value) -> Vec<u8> {
        let mut bytes = value.to_string().into_bytes();
        bytes.truncate(MAX_TOOL_OUTPUT);
        bytes
    }
}

impl<T: ToolRegistry + Sync> ToolRegistry for FilteredTools<'_, T> {
    fn replay_safe(&self, tool: &str) -> bool {
        self.inner.replay_safe(tool)
    }

    fn note_recorded(&self, tool: &str) {
        self.inner.note_recorded(tool)
    }

    async fn invoke(&self, tool: &str, args: &[u8]) -> Result<Vec<u8>> {
        if let Some(def) = self.def {
            if !def.permits(tool) {
                return Ok(Self::soft(
                    serde_json::json!({ "error": format!("unknown tool: {tool}") }),
                ));
            }
            // ask_question is the park mechanism: acknowledge so the model
            // relays the question and ends the turn; the answer arrives as
            // the next message (no durable state, no timers).
            if tool == ASK_QUESTION_TOOL {
                let v: ferrite_app::serde_json::Value =
                    ferrite_app::serde_json::from_slice(args).unwrap_or(serde_json::json!({}));
                let question = v.get("question").and_then(|q| q.as_str()).unwrap_or("");
                if question.trim().is_empty() {
                    return Ok(Self::soft(
                        serde_json::json!({ "error": "ask_question requires a 'question'" }),
                    ));
                }
                return Ok(Self::soft(serde_json::json!({
                    "asked": true,
                    "note": "Relay this question to the user verbatim, then call end_turn. Their answer will arrive as the next message."
                })));
            }
            // Approval gate: a required tool without a recorded grant parks —
            // the model relays the request and ends the turn; a structured
            // approve action records the grant and the next turn runs it.
            if def.requires_approval(tool) && !self.granted.contains(tool) {
                return Ok(Self::soft(serde_json::json!({
                    "approval_required": tool,
                    "note": format!(
                        "Running '{tool}' needs the user's approval. Tell the user what you want to do and that they must approve it, then call end_turn. Do not retry without approval."
                    )
                })));
            }
        }
        self.inner.invoke(tool, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::tools::coding_tool_schemas;

    fn doc(json: &str) -> Option<AgentDefinition> {
        parse_agent_definition(json.as_bytes(), "agent", 8)
    }

    #[test]
    fn minimal_valid_document_parses() {
        let def = doc(r#"{"version":1,"agent_id":"agent"}"#).expect("valid");
        assert_eq!(def.instructions, None);
        assert_eq!(def.default_model, None);
        assert_eq!(def.tool_allow, None);
        assert!(def.tool_deny.is_empty());
        assert_eq!(def.effective_max_steps(8), 8);
    }

    #[test]
    fn full_valid_document_parses_and_normalizes_model() {
        let def = doc(
            r#"{"version":1,"agent_id":"agent","instructions":"You are a weather bot.",
                "default_model":"  claude-opus-5  ",
                "tool_allow":["read_file","run_code"],"tool_deny":["run_code"],
                "limits":{"max_steps":3}}"#,
        )
        .expect("valid");
        assert_eq!(def.default_model.as_deref(), Some("claude-opus-5"));
        assert_eq!(def.effective_max_steps(8), 3);
    }

    #[test]
    fn invalid_documents_are_inert() {
        // Not JSON.
        assert_eq!(doc("not json"), None);
        // Wrong version.
        assert_eq!(doc(r#"{"version":2,"agent_id":"agent"}"#), None);
        // Wrong agent.
        assert_eq!(doc(r#"{"version":1,"agent_id":"other"}"#), None);
        // Unknown field (version discipline; `approval` stays reserved for
        // Phase 6 — `skills` became a defined field in Phase 4).
        assert_eq!(
            doc(r#"{"version":1,"agent_id":"agent","approval":{}}"#),
            None
        );
        // Empty allowlist.
        assert_eq!(
            doc(r#"{"version":1,"agent_id":"agent","tool_allow":[]}"#),
            None
        );
        // Malformed tool name.
        assert_eq!(
            doc(r#"{"version":1,"agent_id":"agent","tool_allow":["bad name"]}"#),
            None
        );
        // Denying the protected tool.
        assert_eq!(
            doc(r#"{"version":1,"agent_id":"agent","tool_deny":["end_turn"]}"#),
            None
        );
        // Duplicate entries (a byte-cap inflation vector: N duplicates pass
        // membership and validity while bloating the document).
        assert_eq!(
            doc(r#"{"version":1,"agent_id":"agent","tool_allow":["read_file","read_file"]}"#),
            None
        );
        assert_eq!(
            doc(r#"{"version":1,"agent_id":"agent","tool_deny":["run_code","run_code"]}"#),
            None
        );
        // Invalid model id.
        assert_eq!(
            doc(r#"{"version":1,"agent_id":"agent","default_model":"bad model!"}"#),
            None
        );
        // Step budget out of range (0 and above the cap).
        assert_eq!(
            doc(r#"{"version":1,"agent_id":"agent","limits":{"max_steps":0}}"#),
            None
        );
        assert_eq!(
            doc(r#"{"version":1,"agent_id":"agent","limits":{"max_steps":9}}"#),
            None
        );
    }

    #[test]
    fn adversarial_documents_are_inert_not_crashes() {
        // Depth bomb: serde_json's recursion limit must turn this into a
        // parse error (inert), never a stack overflow.
        let mut bomb = String::from(r#"{"version":1,"agent_id":"agent","limits":"#);
        bomb.push_str(&"[".repeat(1_000));
        bomb.push_str(&"]".repeat(1_000));
        bomb.push('}');
        assert_eq!(doc(&bomb), None);
        // Non-UTF8 bytes.
        assert_eq!(
            parse_agent_definition(&[0x80, 0xff, 0xfe, 0x7b], "agent", 8),
            None
        );
        // Duplicate field: serde's deny_unknown_fields machinery refuses
        // duplicates, so a doc cannot smuggle a second `version`.
        assert_eq!(doc(r#"{"version":1,"version":2,"agent_id":"agent"}"#), None);
        // Type confusion: string version, float steps, negative steps,
        // object instructions — every one is a whole-document refusal.
        assert_eq!(doc(r#"{"version":"1","agent_id":"agent"}"#), None);
        assert_eq!(
            doc(r#"{"version":1,"agent_id":"agent","limits":{"max_steps":3.5}}"#),
            None
        );
        assert_eq!(
            doc(r#"{"version":1,"agent_id":"agent","limits":{"max_steps":-2}}"#),
            None
        );
        assert_eq!(
            doc(r#"{"version":1,"agent_id":"agent","instructions":{"a":1}}"#),
            None
        );
        // Unicode confusables in tool names: a zero-width space makes the
        // name non-ASCII, refusing the document rather than silently never
        // matching a real tool.
        assert_eq!(
            doc("{\"version\":1,\"agent_id\":\"agent\",\"tool_allow\":[\"read_file\u{200b}\"]}"),
            None
        );
        // Denying the protected tool cannot be smuggled via allow+deny combo.
        assert_eq!(
            doc(
                r#"{"version":1,"agent_id":"agent","tool_allow":["end_turn"],"tool_deny":["end_turn"]}"#
            ),
            None
        );
        // Empty agent_id / whitespace agent_id never match a real key's agent.
        assert_eq!(doc(r#"{"version":1,"agent_id":""}"#), None);
        assert_eq!(doc(r#"{"version":1,"agent_id":" agent"}"#), None);
    }

    #[test]
    fn oversized_documents_are_inert() {
        let padding = "x".repeat(MAX_DEFINITION_BYTES);
        let big = format!(r#"{{"version":1,"agent_id":"agent","instructions":"{padding}"}}"#);
        assert_eq!(doc(&big), None);
        // Instructions cap alone also refuses (document under 48 KiB, text over 32 KiB).
        let mid = "y".repeat(MAX_INSTRUCTIONS_BYTES + 1);
        let over = format!(r#"{{"version":1,"agent_id":"agent","instructions":"{mid}"}}"#);
        assert!(over.len() <= MAX_DEFINITION_BYTES);
        assert_eq!(doc(&over), None);
    }

    #[test]
    fn unavailable_memory_and_web_withhold_every_tool_of_theirs() {
        let mut def = doc(r#"{"version":1,"agent_id":"agent"}"#).expect("valid");
        def.capability_unavailable = vec!["memory".into(), "web".into()];
        for tool in ["remember_memory", "recall_memory", "forget_memory", "news_search", "browser_automate"] {
            assert!(!def.permits(tool), "{tool} must be withheld");
        }
        assert!(def.permits("read_file"));
    }

    #[test]
    fn allow_deny_semantics() {
        let def = doc(
            r#"{"version":1,"agent_id":"agent","tool_allow":["read_file","write_file"],
                "tool_deny":["write_file"]}"#,
        )
        .expect("valid");
        assert!(def.permits("read_file"));
        assert!(!def.permits("write_file"), "deny wins over allow");
        assert!(!def.permits("list_files"), "not in allowlist");
        assert!(def.permits("end_turn"), "protected tool is implicit");
        assert!(
            !def.permits("read_file_version"),
            "tool names must EXACT-match: 'read_file' in the allowlist grants nothing to 'read_file_version'"
        );
    }

    #[test]
    fn no_filters_permit_everything() {
        let def = doc(r#"{"version":1,"agent_id":"agent"}"#).expect("valid");
        let schemas = coding_tool_schemas();
        let filtered = def.filter_schemas(coding_tool_schemas());
        assert_eq!(
            schemas.iter().map(|s| &s.name).collect::<Vec<_>>(),
            filtered.iter().map(|s| &s.name).collect::<Vec<_>>(),
            "a definition without filters must not change the tool surface"
        );
    }

    #[test]
    fn filter_schemas_keeps_allowed_and_protected() {
        let def =
            doc(r#"{"version":1,"agent_id":"agent","tool_allow":["read_file"]}"#).expect("valid");
        let names: Vec<String> = def
            .filter_schemas(coding_tool_schemas())
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(
            names,
            ["read_file", "end_turn"],
            "allowlist must exact-match tool names (schema declaration order); \
             read_file_version must NOT be granted by the read_file entry"
        );
    }

    #[test]
    fn skills_declaration_grants_load_skill_and_validates() {
        // Declared skills grant load_skill implicitly, even under an
        // allowlist that does not name it.
        let def = doc(
            r#"{"version":1,"agent_id":"agent","tool_allow":["read_file"],
                "skills":["weather-playbook","escalation"]}"#,
        )
        .expect("valid");
        assert!(def.permits(LOAD_SKILL_TOOL));
        assert!(def.permits("read_file"));
        assert!(!def.permits("write_file"));
        // No skills -> no loader, even if an allowlist names it.
        let bare =
            doc(r#"{"version":1,"agent_id":"agent","tool_allow":["load_skill"]}"#).expect("valid");
        assert!(!bare.permits(LOAD_SKILL_TOOL));
        // Invalid skill declarations refuse the whole document: bad name,
        // duplicate, over the count cap, and the contradictory deny.
        assert_eq!(
            doc(r#"{"version":1,"agent_id":"agent","skills":["bad name"]}"#),
            None
        );
        assert_eq!(
            doc(r#"{"version":1,"agent_id":"agent","skills":["a","a"]}"#),
            None
        );
        let many: Vec<String> = (0..=MAX_SKILLS).map(|i| format!("s{i}")).collect();
        let json = format!(
            r#"{{"version":1,"agent_id":"agent","skills":{}}}"#,
            ferrite_app::serde_json::to_string(&many).unwrap()
        );
        assert_eq!(doc(&json), None);
        assert_eq!(
            doc(r#"{"version":1,"agent_id":"agent","skills":["a"],"tool_deny":["load_skill"]}"#),
            None
        );
    }

    #[test]
    fn defined_tools_grant_and_validate() {
        let tool = |extra: &str| {
            format!(
                r#"{{"version":1,"agent_id":"agent","tool_allow":["read_file"],
                    "tools":[{{"name":"probe","description":"x","input_schema":{{"type":"object"}}{extra}}}]}}"#
            )
        };
        // Declaration is the grant, even under an allowlist.
        let def = doc(&tool("")).expect("valid");
        assert!(def.permits("probe"));
        assert!(def.defined_tool("probe").is_some());
        assert_eq!(def.defined_tool_schemas().len(), 1);
        assert!(!def.permits("other_tool"));
        // Egress and fuel bounds.
        assert!(doc(&tool(r#","egress_hosts":["api.open-meteo.com"]"#)).is_some());
        assert_eq!(doc(&tool(r#","egress_hosts":[]"#)), None);
        assert_eq!(doc(&tool(r#","egress_hosts":["https://x.com/p"]"#)), None);
        assert!(doc(&tool(r#","fuel":1000000"#)).is_some());
        assert_eq!(doc(&tool(r#","fuel":0"#)), None);
        assert_eq!(doc(&tool(r#","fuel":600000000"#)), None);
        // Collisions with builtins / load_skill, bad schema, bad name.
        assert_eq!(
            doc(
                r#"{"version":1,"agent_id":"agent","tools":[{"name":"read_file","description":"x","input_schema":{}}]}"#
            ),
            None
        );
        assert_eq!(
            doc(
                r#"{"version":1,"agent_id":"agent","tools":[{"name":"load_skill","description":"x","input_schema":{}}]}"#
            ),
            None
        );
        assert_eq!(
            doc(
                r#"{"version":1,"agent_id":"agent","tools":[{"name":"probe","description":"x","input_schema":3}]}"#
            ),
            None
        );
        assert_eq!(
            doc(
                r#"{"version":1,"agent_id":"agent","tools":[{"name":"bad name","description":"x","input_schema":{}}]}"#
            ),
            None
        );
        // Duplicates and the contradictory deny.
        assert_eq!(
            doc(r#"{"version":1,"agent_id":"agent","tools":[
                {"name":"probe","description":"x","input_schema":{}},
                {"name":"probe","description":"y","input_schema":{}}]}"#),
            None
        );
        assert_eq!(
            doc(r#"{"version":1,"agent_id":"agent","tool_deny":["probe"],
                "tools":[{"name":"probe","description":"x","input_schema":{}}]}"#),
            None
        );
    }

    #[test]
    fn hitl_approvals_and_ask_question() {
        // ask_question is opt-in and off by default.
        let off = doc(r#"{"version":1,"agent_id":"agent"}"#).expect("valid");
        assert!(!off.permits(ASK_QUESTION_TOOL));
        assert!(!off.requires_approval("run_code"));
        let on = doc(
            r#"{"version":1,"agent_id":"agent","ask_question":true,"approvals":["deploy_source","run_code"]}"#,
        )
        .expect("valid");
        assert!(on.permits(ASK_QUESTION_TOOL));
        assert!(on.requires_approval("deploy_source"));
        assert!(on.requires_approval("run_code"));
        assert!(!on.requires_approval("read_file"));
        // Deny wins over the ask_question declaration too: neither the prompt
        // surface nor dispatch may offer an explicitly denied tool.
        let denied = doc(r#"{"version":1,"agent_id":"agent","ask_question":true,"tool_deny":["ask_question"]}"#)
            .expect("valid");
        assert!(!denied.permits(ASK_QUESTION_TOOL));
        assert!(denied
            .filter_schemas(vec![crate::agents::tools::ask_question_tool_schema()])
            .is_empty());
        // Gating end_turn or ask_question behind approval would deadlock.
        assert_eq!(
            doc(r#"{"version":1,"agent_id":"agent","approvals":["end_turn"]}"#),
            None
        );
        assert_eq!(
            doc(r#"{"version":1,"agent_id":"agent","approvals":["ask_question"]}"#),
            None
        );
        // Bad name / duplicate refuse the whole document.
        assert_eq!(
            doc(r#"{"version":1,"agent_id":"agent","approvals":["bad name"]}"#),
            None
        );
        assert_eq!(
            doc(r#"{"version":1,"agent_id":"agent","approvals":["run_code","run_code"]}"#),
            None
        );
    }

    #[tokio::test]
    async fn approval_gate_parks_until_granted() {
        let def =
            doc(r#"{"version":1,"agent_id":"agent","approvals":["run_code"],"ask_question":true}"#)
                .expect("valid");
        // Not granted: a required tool parks (soft approval_required), the
        // inner tool never runs.
        let ungranted = FilteredTools::new(&EchoTools, Some(&def));
        let out = ungranted.invoke("run_code", b"{}").await.unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("approval_required"), "{out}");
        assert!(
            !out.contains("ran:run_code"),
            "the tool must not run: {out}"
        );
        // Granted: it runs.
        let mut granted = std::collections::BTreeSet::new();
        granted.insert("run_code".to_string());
        let ok = FilteredTools::new(&EchoTools, Some(&def)).with_granted_approvals(granted);
        assert_eq!(ok.invoke("run_code", b"{}").await.unwrap(), b"ran:run_code");
        // A non-required tool never parks.
        assert_eq!(
            ungranted.invoke("read_file", b"{}").await.unwrap(),
            b"ran:read_file"
        );
        // ask_question acknowledges without hitting the inner registry.
        let asked = ungranted
            .invoke("ask_question", br#"{"question":"which city?"}"#)
            .await
            .unwrap();
        let asked = String::from_utf8(asked).unwrap();
        assert!(asked.contains("\"asked\":true"), "{asked}");
    }

    #[tokio::test]
    async fn grant_isolation_is_per_tool() {
        // Security invariant: approving ONE gated tool must never authorize a
        // different gated tool. (A regression collapsing the per-tool check
        // into `!granted.is_empty()` would pass every other HITL test.)
        let def =
            doc(r#"{"version":1,"agent_id":"agent","approvals":["run_code","deploy_source"]}"#)
                .expect("valid");
        let mut granted = std::collections::BTreeSet::new();
        granted.insert("run_code".to_string()); // grant ONLY run_code
        let tools = FilteredTools::new(&EchoTools, Some(&def)).with_granted_approvals(granted);
        assert_eq!(
            tools.invoke("run_code", b"{}").await.unwrap(),
            b"ran:run_code"
        );
        let out = String::from_utf8(tools.invoke("deploy_source", b"{}").await.unwrap()).unwrap();
        assert!(
            out.contains("approval_required"),
            "a different gated tool must still park: {out}"
        );
        assert!(
            !out.contains("ran:deploy_source"),
            "an ungranted gated tool must never reach the inner registry: {out}"
        );
    }

    #[test]
    fn unavailable_optional_capability_removes_authored_tool_from_prompt_and_dispatch() {
        let mut def = doc(r#"{"version":1,"agent_id":"agent","tools":[{"name":"report","description":"x","input_schema":{}}]}"#)
            .expect("valid definition");
        def.tools[0].optional_capabilities = vec!["store".into()];
        def.capability_unavailable = vec!["store".into()];
        assert!(!def.permits("report"));
        let schemas = def.filter_schemas(vec![ToolSchema {
            name: "report".into(),
            description: "x".into(),
            input_schema: serde_json::json!({}),
        }]);
        assert!(schemas.is_empty());
    }

    #[test]
    fn schedules_validate_and_look_up() {
        let def = doc(r#"{"version":1,"agent_id":"agent","schedules":[
                {"name":"daily-digest","cron":"0 9 * * *","prompt":"Summarize overnight events."},
                {"name":"heartbeat","cron":"*/5 * * * *","prompt":"ping"}
            ]}"#)
        .expect("valid");
        assert_eq!(def.schedules.len(), 2);
        assert_eq!(def.schedule("daily-digest").unwrap().cron, "0 9 * * *");
        assert!(def.schedule("nope").is_none());
        // Invalid cron (wrong field count), bad name, dup, empty prompt refuse.
        assert_eq!(
            doc(
                r#"{"version":1,"agent_id":"agent","schedules":[{"name":"s","cron":"0 9 * *","prompt":"x"}]}"#
            ),
            None
        );
        assert_eq!(
            doc(
                r#"{"version":1,"agent_id":"agent","schedules":[{"name":"bad name","cron":"* * * * *","prompt":"x"}]}"#
            ),
            None
        );
        assert_eq!(
            doc(r#"{"version":1,"agent_id":"agent","schedules":[
                {"name":"s","cron":"* * * * *","prompt":"x"},
                {"name":"s","cron":"* * * * *","prompt":"y"}]}"#),
            None
        );
        assert_eq!(
            doc(
                r#"{"version":1,"agent_id":"agent","schedules":[{"name":"s","cron":"* * * * *","prompt":"  "}]}"#
            ),
            None
        );
        // A cron field with letters is garbage.
        assert_eq!(
            doc(
                r#"{"version":1,"agent_id":"agent","schedules":[{"name":"s","cron":"0 9 * * MON","prompt":"x"}]}"#
            ),
            None
        );
    }

    #[test]
    fn subagents_declaration_grants_delegate_and_validates() {
        let def = doc(
            r#"{"version":1,"agent_id":"agent","subagents":[
                {"id":"researcher","instructions":"You research.","tool_allow":["web_search","read_file"]},
                {"id":"reviewer","instructions":"You review.","default_model":"  claude-opus-5  ","limits":{"max_steps":2}}
            ]}"#,
        )
        .expect("valid");
        assert_eq!(def.subagents.len(), 2);
        // Declaration is the grant for delegate (like load_skill).
        assert!(def.permits(DELEGATE_TOOL));
        assert!(def.subagent("researcher").is_some());
        assert!(def.subagent("nope").is_none());
        assert_eq!(
            def.subagent("reviewer").unwrap().default_model.as_deref(),
            Some("claude-opus-5"),
            "child model normalized in place like the parent's (clean_model_id trims)"
        );
        // No subagents -> no delegate, even if an allowlist names it.
        let bare =
            doc(r#"{"version":1,"agent_id":"agent","tool_allow":["delegate"]}"#).expect("valid");
        assert!(!bare.permits(DELEGATE_TOOL));

        // --- every invalid class refuses the WHOLE document (fail-safe) ---
        let sub =
            |body: &str| format!(r#"{{"version":1,"agent_id":"agent","subagents":[{body}]}}"#);
        // Blank / oversize instructions.
        assert_eq!(doc(&sub(r#"{"id":"x","instructions":"  "}"#)), None);
        let big = "z".repeat(MAX_SUBAGENT_INSTRUCTIONS_BYTES + 1);
        assert_eq!(
            doc(&sub(&format!(r#"{{"id":"x","instructions":"{big}"}}"#))),
            None
        );
        // Bad id charset / oversize id.
        assert_eq!(doc(&sub(r#"{"id":"bad id","instructions":"x"}"#)), None);
        let bigid = "a".repeat(MAX_SUBAGENT_ID_BYTES + 1);
        assert_eq!(
            doc(&sub(&format!(r#"{{"id":"{bigid}","instructions":"x"}}"#))),
            None
        );
        // Duplicate ids.
        assert_eq!(
            doc(r#"{"version":1,"agent_id":"agent","subagents":[
                {"id":"a","instructions":"x"},{"id":"a","instructions":"y"}]}"#),
            None
        );
        // Bad child model.
        assert_eq!(
            doc(&sub(
                r#"{"id":"x","instructions":"x","default_model":"bad model!"}"#
            )),
            None
        );
        // Empty / invalid / duplicate child tool_allow.
        assert_eq!(
            doc(&sub(r#"{"id":"x","instructions":"x","tool_allow":[]}"#)),
            None
        );
        assert_eq!(
            doc(&sub(
                r#"{"id":"x","instructions":"x","tool_allow":["bad name"]}"#
            )),
            None
        );
        assert_eq!(
            doc(&sub(
                r#"{"id":"x","instructions":"x","tool_allow":["a","a"]}"#
            )),
            None
        );
        // Child denying end_turn.
        assert_eq!(
            doc(&sub(
                r#"{"id":"x","instructions":"x","tool_deny":["end_turn"]}"#
            )),
            None
        );
        // Child step budget 0, or above the CHILD cap even though still within the
        // parent's. Written against the constants: with a literal, moving either
        // cap turns this into an assertion about nothing.
        assert_eq!(
            doc(&sub(
                r#"{"id":"x","instructions":"x","limits":{"max_steps":0}}"#
            )),
            None
        );
        let over_child_cap = PLATFORM_MAX_SUBAGENT_STEPS_CAP + 1;
        assert!(
            over_child_cap <= PLATFORM_MAX_STEPS_CAP,
            "the child cap must stay strictly below the parent's for this case to test what it claims"
        );
        assert_eq!(
            doc(&sub(&format!(
                r#"{{"id":"x","instructions":"x","limits":{{"max_steps":{over_child_cap}}}}}"#
            ))),
            None
        );
        assert!(doc(&sub(
            r#"{"id":"x","instructions":"x","limits":{"max_steps":4}}"#
        ))
        .is_some());
        // Unknown field inside a SubagentSpec (deny_unknown_fields).
        assert_eq!(
            doc(&sub(r#"{"id":"x","instructions":"x","skills":["s"]}"#)),
            None
        );
        // Too many subagents.
        let many: Vec<String> = (0..=MAX_SUBAGENTS)
            .map(|i| format!(r#"{{"id":"s{i}","instructions":"x"}}"#))
            .collect();
        assert_eq!(
            doc(&format!(
                r#"{{"version":1,"agent_id":"agent","subagents":[{}]}}"#,
                many.join(",")
            )),
            None
        );
        // Declaring subagents while denying delegate is contradictory.
        assert_eq!(
            doc(r#"{"version":1,"agent_id":"agent","tool_deny":["delegate"],
                "subagents":[{"id":"x","instructions":"x"}]}"#),
            None
        );
        // A pack tool named `delegate` collides with the synthesized tool.
        assert_eq!(
            doc(
                r#"{"version":1,"agent_id":"agent","tools":[{"name":"delegate","description":"x","input_schema":{"type":"object"}}]}"#
            ),
            None
        );
    }

    #[test]
    fn synth_child_def_is_depth_capped_and_inherits_nothing() {
        let parent = doc(
            r#"{"version":1,"agent_id":"agent","skills":["s"],"subagents":[
                {"id":"kid","instructions":"child prompt"}]}"#,
        )
        .expect("valid");
        let spec = parent.subagent("kid").unwrap();
        let child = synth_child_def(spec, "agent.sub.kid".to_string(), Default::default());
        // Depth-1: the child can NEVER delegate.
        assert!(child.subagents.is_empty());
        assert!(!child.permits(DELEGATE_TOOL));
        // Inherits nothing: no skills/tools/approvals/schedules from the parent.
        assert!(child.skills.is_empty());
        assert!(child.tools.is_empty());
        assert!(!child.permits(LOAD_SKILL_TOOL));
        assert_eq!(child.instructions.as_deref(), Some("child prompt"));
        // Unspecified child budget defaults to the CHILD cap, clamped ≤ 4 even
        // against the parent's cap of 8.
        assert_eq!(
            child.effective_max_steps(PLATFORM_MAX_STEPS_CAP),
            PLATFORM_MAX_SUBAGENT_STEPS_CAP
        );
        // An explicit child budget is honored (still ≤ 4).
        let spec2 = SubagentSpec {
            id: "k2".into(),
            instructions: "p".into(),
            default_model: None,
            tool_allow: None,
            tool_deny: Vec::new(),
            limits: AgentLimits { max_steps: Some(2) },
        };
        assert_eq!(
            synth_child_def(&spec2, "agent.sub.k2".into(), Default::default()).effective_max_steps(8),
            2
        );
    }

    #[tokio::test]
    async fn child_inherits_parent_approval_gate_and_cannot_bypass_it() {
        // Security: a parent-gated tool must NOT become runnable by delegating
        // it to a child. The child inherits the parent's approvals, has no
        // recorded grant on its own thread, so the gated tool soft-parks.
        let parent = doc(r#"{"version":1,"agent_id":"agent","approvals":["run_code"],
                "subagents":[{"id":"worker","instructions":"You work."}]}"#)
        .expect("valid");
        let child = synth_child_def(
            parent.subagent("worker").unwrap(),
            "agent.sub.worker".into(),
            parent.child_policy(),
        );
        assert!(
            child.requires_approval("run_code"),
            "gate propagates to the child"
        );
        // A child turn records no grants (fresh thread, no human), so a gated
        // tool always parks instead of running.
        let tools = FilteredTools::new(&EchoTools, Some(&child));
        let out = String::from_utf8(tools.invoke("run_code", b"{}").await.unwrap()).unwrap();
        assert!(
            out.contains("approval_required"),
            "gated tool parks in the child: {out}"
        );
        assert!(
            !out.contains("ran:run_code"),
            "gated tool must NOT run in the child: {out}"
        );
    }

    /// Review finding F056: a child declared without its own allow/deny used
    /// to permit the whole built-in surface, so `delegate` escaped the
    /// parent's tool filter.
    #[tokio::test]
    async fn child_inherits_parent_allow_deny_and_unavailable_capabilities() {
        let parent = doc(
            r#"{"version":1,"agent_id":"agent","tool_allow":["read_file","web_search"],
                "tool_deny":["web_search"],
                "subagents":[
                    {"id":"open","instructions":"no filter of its own"},
                    {"id":"wide","instructions":"asks for more","tool_allow":["read_file","buzz_post"]}]}"#,
        )
        .expect("valid");
        let open = synth_child_def(
            parent.subagent("open").unwrap(),
            "agent.sub.open".into(),
            parent.child_policy(),
        );
        assert!(open.permits("read_file"));
        assert!(!open.permits("buzz_post"), "parent allow-list applies");
        assert!(!open.permits("web_search"), "parent deny applies");
        assert!(!open.permits("buzz_schedule"));
        assert!(open.permits(PROTECTED_TOOL));
        // Dispatch agrees with the prompt surface.
        let tools = FilteredTools::new(&EchoTools, Some(&open));
        let out = String::from_utf8(tools.invoke("buzz_post", b"{}").await.unwrap()).unwrap();
        assert!(out.contains("unknown tool"), "{out}");

        let wide = synth_child_def(
            parent.subagent("wide").unwrap(),
            "agent.sub.wide".into(),
            parent.child_policy(),
        );
        assert!(wide.permits("read_file"));
        assert!(!wide.permits("buzz_post"), "child allow is intersected with the parent's");

        let mut limited = parent.clone();
        limited.tool_allow = None;
        limited.tool_deny = Vec::new();
        limited.capability_unavailable = vec!["buzz".into()];
        let child = synth_child_def(
            limited.subagent("open").unwrap(),
            "agent.sub.open".into(),
            limited.child_policy(),
        );
        assert!(!child.permits("buzz_post"), "unavailable capability stays unavailable");
        assert!(child.permits("read_file"));
    }

    /// Review finding F225: a pack tool named like a non-coding builtin was
    /// accepted and its declaration re-granted the real builtin past the
    /// document's own allow-list.
    #[test]
    fn pack_tools_cannot_shadow_any_builtin() {
        for name in [
            "buzz_post",
            "web_search",
            "mcp_call_tool",
            "fabric_ask",
            "ask_question",
            "end_turn",
        ] {
            let json = format!(
                r#"{{"version":1,"agent_id":"agent","tool_allow":["read_file"],
                    "tools":[{{"name":"{name}","description":"x","input_schema":{{}}}}]}}"#
            );
            assert!(doc(&json).is_none(), "{name} must be refused as a pack tool");
        }
        for schemas in [
            crate::agents::tools::buzz_tool_schemas(),
            crate::agents::tools::schedule_tool_schemas(),
            crate::agents::tools::mcp_tool_schemas(),
        ] {
            for s in schemas {
                assert!(builtin_tool_names().contains(&s.name), "{}", s.name);
            }
        }
    }

    #[test]
    fn a_deny_wins_over_a_declared_pack_tool() {
        // The parser refuses this combination; `permits` must agree on its own.
        let mut def = doc(
            r#"{"version":1,"agent_id":"agent",
                "tools":[{"name":"probe","description":"x","input_schema":{}}]}"#,
        )
        .expect("valid");
        assert!(def.permits("probe"));
        def.tool_deny.push("probe".into());
        assert!(!def.permits("probe"));
    }

    #[test]
    fn skill_key_shape() {
        assert_eq!(
            skill_key("agent", "weather-playbook"),
            b"agent-skill:v1:agent:weather-playbook".to_vec()
        );
    }

    struct EchoTools;
    impl ToolRegistry for EchoTools {
        async fn invoke(&self, tool: &str, _args: &[u8]) -> Result<Vec<u8>> {
            Ok(format!("ran:{tool}").into_bytes())
        }
    }

    #[tokio::test]
    async fn filtered_tools_pass_through_without_definition() {
        let tools = FilteredTools::new(&EchoTools, None);
        let out = tools.invoke("write_file", b"{}").await.unwrap();
        assert_eq!(out, b"ran:write_file");
    }

    #[tokio::test]
    async fn filtered_tools_deny_matches_unknown_tool_shape() {
        let def =
            doc(r#"{"version":1,"agent_id":"agent","tool_allow":["read_file"]}"#).expect("valid");
        let tools = FilteredTools::new(&EchoTools, Some(&def));
        let out = tools.invoke("write_file", b"{}").await.unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{"error":"unknown tool: write_file"}"#,
            "filtered must be indistinguishable from nonexistent (tools/mod.rs unknown-tool arm)"
        );
        let allowed = tools.invoke("read_file", b"{}").await.unwrap();
        assert_eq!(allowed, b"ran:read_file");
        let protected = tools.invoke("end_turn", b"{}").await.unwrap();
        assert_eq!(protected, b"ran:end_turn");
    }
}

//! AgentBundle v2 parser and dual-read loader.
//!
//! A v2 head is authoritative when present: invalid heads/manifests/resources
//! are wholly inert and never fall through to v1. Absence of a v2 head keeps
//! the existing v1 path byte-for-byte.

use ferrite_app::serde::Deserialize;
use ferrite_app::serde_json::{self, Map, Value};
use ferrite_app::AppContext;

use crate::agents::contract_generated::{
    BUNDLE_FORMAT, BUNDLE_VERSION, CAPABILITY_CATALOG_VERSION, MAX_BUNDLE_BYTES,
    MAX_BUNDLE_RESOURCES, MAX_BUNDLE_RESOURCE_BYTES, RUNTIME_ABI,
};
use crate::agents::definition::{
    agent_def_key, parse_agent_definition, AgentDefinition, AgentLimits, DefinedTool, ScheduleSpec,
    SubagentSpec, ToolBlobGrant, ToolStoreGrant,
};

pub fn bundle_head_key(agent: &str) -> Vec<u8> {
    format!("agent-bundle-head:v2:{agent}").into_bytes()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(crate = "ferrite_app::serde", deny_unknown_fields)]
pub struct BundleHead {
    pub version: u64,
    pub agent_id: String,
    pub bundle_id: String,
    pub manifest_key: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(crate = "ferrite_app::serde", deny_unknown_fields)]
struct CompilerIdentity {
    name: String,
    version: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(crate = "ferrite_app::serde", deny_unknown_fields)]
struct RuntimeAbi {
    min: u64,
    max: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(crate = "ferrite_app::serde", deny_unknown_fields)]
struct CapabilityRequests {
    catalog_version: u64,
    #[serde(default)]
    required: Vec<String>,
    #[serde(default)]
    optional: Vec<String>,
    #[serde(default)]
    options: Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(crate = "ferrite_app::serde", deny_unknown_fields)]
struct BundleModel {
    #[serde(default)]
    default: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(crate = "ferrite_app::serde", deny_unknown_fields)]
struct BundleToolPolicy {
    #[serde(default)]
    allow: Option<Vec<String>>,
    #[serde(default)]
    deny: Vec<String>,
    #[serde(default)]
    approvals: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(crate = "ferrite_app::serde", deny_unknown_fields)]
struct BundleSkill {
    name: String,
    resource_ref: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(crate = "ferrite_app::serde", deny_unknown_fields)]
struct BundleTool {
    name: String,
    description: String,
    input_schema: Value,
    #[serde(default)]
    egress_hosts: Option<Vec<String>>,
    #[serde(default)]
    fuel: Option<u64>,
    code_ref: String,
    #[serde(default)]
    capabilities: Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(crate = "ferrite_app::serde", deny_unknown_fields)]
struct Contributions {
    #[serde(default)]
    tools: Vec<BundleTool>,
    #[serde(default)]
    skills: Vec<BundleSkill>,
    #[serde(default)]
    hooks: Vec<Value>,
    #[serde(default)]
    state: Vec<Value>,
    #[serde(default)]
    connections: Vec<Value>,
    #[serde(default)]
    modules: Vec<Value>,
    #[serde(default)]
    subagents: Vec<SubagentSpec>,
    #[serde(default)]
    schedules: Vec<ScheduleSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(crate = "ferrite_app::serde", deny_unknown_fields)]
struct BundleAgent {
    id: String,
    instructions_ref: String,
    model: BundleModel,
    #[serde(default)]
    limits: AgentLimits,
    tool_policy: BundleToolPolicy,
    capabilities: CapabilityRequests,
    contributions: Contributions,
    #[serde(default)]
    ask_question: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(crate = "ferrite_app::serde", deny_unknown_fields)]
struct BundleResource {
    id: String,
    kind: String,
    media_type: String,
    bytes: usize,
    digest: String,
    source: String,
    content: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(crate = "ferrite_app::serde", deny_unknown_fields)]
struct AgentBundle {
    format: String,
    version: u64,
    compiler: CompilerIdentity,
    runtime_abi: RuntimeAbi,
    profile: String,
    bundle_id: String,
    agents: Vec<BundleAgent>,
    resources: Vec<BundleResource>,
    provenance: Vec<Value>,
}

fn canonicalize(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize).collect()),
        Value::Object(values) => {
            let mut entries: Vec<_> = values.into_iter().collect();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            let mut sorted = Map::new();
            for (key, value) in entries {
                sorted.insert(key, canonicalize(value));
            }
            Value::Object(sorted)
        }
        other => other,
    }
}

fn digest_id(bytes: &[u8]) -> String {
    let digest = sha256(bytes);
    let mut text = String::with_capacity(71);
    text.push_str("sha256:");
    for byte in digest {
        use core::fmt::Write;
        let _ = write!(text, "{byte:02x}");
    }
    text
}

fn valid_capability(name: &str) -> bool {
    matches!(
        name,
        "model"
            | "sessions"
            | "memory"
            | "code"
            | "workflows"
            | "web"
            | "ferrite_control"
            | "store"
            | "blob"
            // The buzz workspace seam. Additive within catalog
            // version 1: an older runtime without this arm refuses the name
            // (fail-closed), so no version bump is needed for the addition.
            | "buzz"
            // The Fabric A2A seam, which `capability_available` probes.
            | "fabric-a2a"
    )
}

fn valid_logical_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn valid_prefix(prefix: &str) -> bool {
    prefix.len() <= 128 && !prefix.starts_with('/') && !prefix.split('/').any(|part| part == "..")
}

struct ParsedToolCapabilities {
    store_grants: Vec<ToolStoreGrant>,
    blob_grants: Vec<ToolBlobGrant>,
    callbacks: Vec<String>,
    egress_hosts: Vec<String>,
    required: Vec<String>,
    optional: Vec<String>,
}

fn parse_tool_capabilities(value: &Value) -> Option<ParsedToolCapabilities> {
    let root = value.as_object()?;
    if root
        .keys()
        .any(|key| !matches!(key.as_str(), "required" | "optional" | "options"))
    {
        return None;
    }
    let strings = |name: &str| -> Option<Vec<String>> {
        match root.get(name) {
            None => Some(Vec::new()),
            Some(value) => value
                .as_array()?
                .iter()
                .map(|item| item.as_str().map(str::to_owned))
                .collect(),
        }
    };
    let required = strings("required")?;
    let optional = strings("optional")?;
    if required
        .iter()
        .chain(optional.iter())
        .any(|name| !valid_capability(name))
    {
        return None;
    }
    let requested: std::collections::BTreeSet<_> = required
        .iter()
        .chain(optional.iter())
        .map(String::as_str)
        .collect();
    let options = root.get("options").and_then(Value::as_object)?;
    if options
        .keys()
        .any(|name| !requested.contains(name.as_str()))
    {
        return None;
    }
    let mut stores = Vec::new();
    let mut blobs = Vec::new();
    let mut callbacks = Vec::new();
    let mut egress = Vec::new();
    for (capability, raw) in options {
        let item = raw.as_object()?;
        if item.keys().any(|key| {
            !matches!(
                key.as_str(),
                "recall" | "egressHosts" | "stores" | "blobs" | "callbacks"
            )
        }) {
            return None;
        }
        if let Some(hosts) = item.get("egressHosts") {
            for host in hosts.as_array()? {
                let host = host.as_str()?;
                if host.is_empty()
                    || host.len() > 255
                    || host.contains('/')
                    || host.contains('@')
                    || host.chars().any(char::is_whitespace)
                {
                    return None;
                }
                egress.push(host.to_string());
            }
        }
        if let Some(raw_grants) = item.get("stores") {
            if capability != "store" {
                return None;
            }
            for (name, raw_grant) in raw_grants.as_object()? {
                let grant = raw_grant.as_object()?;
                if !valid_logical_name(name)
                    || grant
                        .keys()
                        .any(|key| !matches!(key.as_str(), "read" | "write" | "prefix"))
                {
                    return None;
                }
                let prefix = grant.get("prefix").and_then(Value::as_str).unwrap_or("");
                if !valid_prefix(prefix) {
                    return None;
                }
                stores.push(ToolStoreGrant {
                    name: name.clone(),
                    prefix: prefix.to_string(),
                    read: grant.get("read").and_then(Value::as_bool).unwrap_or(true),
                    write: grant.get("write").and_then(Value::as_bool).unwrap_or(false),
                });
            }
        }
        if let Some(raw_grants) = item.get("blobs") {
            if capability != "blob" {
                return None;
            }
            for (name, raw_grant) in raw_grants.as_object()? {
                let grant = raw_grant.as_object()?;
                if !valid_logical_name(name)
                    || grant
                        .keys()
                        .any(|key| !matches!(key.as_str(), "read" | "write" | "prefix"))
                {
                    return None;
                }
                let prefix = grant.get("prefix").and_then(Value::as_str).unwrap_or("");
                if !valid_prefix(prefix) {
                    return None;
                }
                blobs.push(ToolBlobGrant {
                    name: name.clone(),
                    prefix: prefix.to_string(),
                    read: grant.get("read").and_then(Value::as_bool).unwrap_or(true),
                    write: grant.get("write").and_then(Value::as_bool).unwrap_or(false),
                });
            }
        }
        if let Some(raw_callbacks) = item.get("callbacks") {
            for name in raw_callbacks.as_array()? {
                let name = name.as_str()?;
                if !valid_logical_name(name) {
                    return None;
                }
                callbacks.push(name.to_string());
            }
        }
    }
    stores.sort_by(|left, right| left.name.cmp(&right.name));
    blobs.sort_by(|left, right| left.name.cmp(&right.name));
    callbacks.sort();
    callbacks.dedup();
    egress.sort();
    egress.dedup();
    Some(ParsedToolCapabilities {
        store_grants: stores,
        blob_grants: blobs,
        callbacks,
        egress_hosts: egress,
        required,
        optional,
    })
}

/// Parse, authenticate, and lower a bundle into the existing durable runtime
/// definition. The lowering is intentionally conservative: v2 metadata that
/// has no runtime adapter yet is validated but cannot grant a tool.
pub fn parse_agent_bundle(
    bytes: &[u8],
    expected_agent: &str,
    expected_bundle_id: &str,
    max_steps_cap: usize,
) -> Option<AgentDefinition> {
    if bytes.len() > MAX_BUNDLE_BYTES {
        return None;
    }
    let raw: Value = ferrite_app::serde_json::from_slice(bytes).ok()?;
    let mut digest_input = raw.clone();
    digest_input.as_object_mut()?.remove("bundle_id")?;
    let canonical = ferrite_app::serde_json::to_vec(&canonicalize(digest_input)).ok()?;
    if digest_id(&canonical) != expected_bundle_id {
        return None;
    }
    let bundle: AgentBundle = ferrite_app::serde_json::from_value(raw).ok()?;
    if bundle.format != BUNDLE_FORMAT
        || bundle.version != BUNDLE_VERSION
        || bundle.bundle_id != expected_bundle_id
        || bundle.compiler.name != "agentc"
        || bundle.compiler.version.is_empty()
        || bundle.runtime_abi.min > RUNTIME_ABI
        || bundle.runtime_abi.max < RUNTIME_ABI
        || !matches!(bundle.profile.as_str(), "ferrite-v1" | "eve-compatible")
        || bundle.resources.len() > MAX_BUNDLE_RESOURCES
        || bundle.agents.is_empty()
        || bundle.provenance.len() > MAX_BUNDLE_RESOURCES
    {
        return None;
    }
    let mut resources = std::collections::BTreeMap::new();
    for resource in &bundle.resources {
        if resource.bytes != resource.content.len()
            || resource.bytes > MAX_BUNDLE_RESOURCE_BYTES
            || resource.id != format!("resource:{}", resource.digest)
            || digest_id(resource.content.as_bytes()) != resource.digest
            || resource.source.starts_with('/')
            || resource.source.split('/').any(|part| part == "..")
            || resource.media_type.is_empty()
            || resources.insert(resource.id.as_str(), resource).is_some()
        {
            return None;
        }
    }
    let agent = bundle
        .agents
        .iter()
        .find(|agent| agent.id == expected_agent)?;
    if bundle
        .agents
        .iter()
        .filter(|item| item.id == expected_agent)
        .count()
        != 1
        || agent.capabilities.catalog_version != CAPABILITY_CATALOG_VERSION
        || !agent
            .capabilities
            .required
            .iter()
            .chain(agent.capabilities.optional.iter())
            .all(|name| valid_capability(name))
        || !agent.capabilities.options.is_object()
    {
        return None;
    }
    let instructions = resources.get(agent.instructions_ref.as_str())?;
    if instructions.kind != "instructions" {
        return None;
    }
    let mut skills = Vec::new();
    for skill in &agent.contributions.skills {
        let resource = resources.get(skill.resource_ref.as_str())?;
        if resource.kind != "skill" {
            return None;
        }
        skills.push(skill.name.clone());
    }
    let mut tools = Vec::new();
    for tool in &agent.contributions.tools {
        let resource = resources.get(tool.code_ref.as_str())?;
        if resource.kind != "tool-code" || !tool.capabilities.is_object() {
            return None;
        }
        let parsed = parse_tool_capabilities(&tool.capabilities)?;
        let mut egress_hosts = tool.egress_hosts.clone().unwrap_or_default();
        egress_hosts.extend(parsed.egress_hosts);
        egress_hosts.sort();
        egress_hosts.dedup();
        tools.push(DefinedTool {
            name: tool.name.clone(),
            description: tool.description.clone(),
            input_schema: tool.input_schema.clone(),
            egress_hosts: (!egress_hosts.is_empty()).then_some(egress_hosts),
            fuel: tool.fuel,
            store_grants: parsed.store_grants,
            blob_grants: parsed.blob_grants,
            callbacks: parsed.callbacks,
            required_capabilities: parsed.required,
            optional_capabilities: parsed.optional,
        });
    }
    // Hooks/connections/modules are declarative in ABI 2 and contribute no
    // authority by themselves. Their runtime adapters are selected only from
    // delivered capabilities.
    if agent.contributions.hooks.len() > 64
        || agent.contributions.state.len() > 32
        || agent.contributions.connections.len() > 32
        || agent.contributions.modules.len() > 32
    {
        return None;
    }
    let candidate = AgentDefinition {
        version: crate::agents::definition::AGENT_DEF_VERSION,
        agent_id: agent.id.clone(),
        instructions: Some(instructions.content.clone()),
        default_model: agent.model.default.clone(),
        tool_allow: agent.tool_policy.allow.clone(),
        tool_deny: agent.tool_policy.deny.clone(),
        skills,
        tools,
        approvals: agent.tool_policy.approvals.clone(),
        ask_question: agent.ask_question,
        schedules: agent.contributions.schedules.clone(),
        subagents: agent.contributions.subagents.clone(),
        limits: agent.limits.clone(),
        bundle_generation: Some(bundle.bundle_id),
        capability_required: agent.capabilities.required.clone(),
        capability_optional: agent.capabilities.optional.clone(),
        capability_unavailable: Vec::new(),
    };
    // Reuse the v1 semantic validator by serializing only its public document
    // shape. This is an independent second oracle for bounds and collisions.
    let public = serde_json::json!({
        "version": candidate.version,
        "agent_id": candidate.agent_id,
        "instructions": candidate.instructions,
        "default_model": candidate.default_model,
        "tool_allow": candidate.tool_allow,
        "tool_deny": candidate.tool_deny,
        "skills": candidate.skills,
        "tools": candidate.tools.iter().map(|tool| serde_json::json!({
            "name": tool.name,
            "description": tool.description,
            "input_schema": tool.input_schema,
            "egress_hosts": tool.egress_hosts,
            "fuel": tool.fuel,
        })).collect::<Vec<_>>(),
        "approvals": candidate.approvals,
        "ask_question": candidate.ask_question,
        "schedules": candidate.schedules.iter().map(|schedule| serde_json::json!({
            "name": schedule.name,
            "cron": schedule.cron,
            "prompt": schedule.prompt,
        })).collect::<Vec<_>>(),
        "subagents": candidate.subagents.iter().map(|child| serde_json::json!({
            "id": child.id,
            "instructions": child.instructions,
            "default_model": child.default_model,
            "tool_allow": child.tool_allow,
            "tool_deny": child.tool_deny,
            "limits": { "max_steps": child.limits.max_steps },
        })).collect::<Vec<_>>(),
        "limits": { "max_steps": candidate.limits.max_steps },
    });
    let validated = parse_agent_definition(
        &ferrite_app::serde_json::to_vec(&public).ok()?,
        expected_agent,
        max_steps_cap,
    )?;
    Some(AgentDefinition {
        bundle_generation: candidate.bundle_generation,
        capability_required: candidate.capability_required,
        capability_optional: candidate.capability_optional,
        tools: candidate.tools,
        ..validated
    })
}

fn capability_available(ctx: &AppContext, name: &str) -> bool {
    // Buzz runs personas in one bundle: the model is reached through the
    // provider egress and the Buzz workspace seam is in-process, so the
    // capabilities probe the bindings declared in `apps/buzz/ferrite.json`,
    // not the queues and stores of the earlier separate agent service.
    match name {
        "model" => ctx.egress("provider-egress").is_ok(),
        "sessions" => ctx.ape("agent-sessions").is_ok() && ctx.store("config").is_ok(),
        "memory" => ctx.store("agent-memory").is_ok(),
        "code" => {
            ctx.queue("worker-requests").is_ok()
                && ctx.queue("worker-replies").is_ok()
                && ctx.store("worker-results").is_ok()
        }
        "workflows" => ctx.store("workflows-defs").is_ok() && ctx.queue("workflows-work").is_ok(),
        "web" => ctx.egress("web-egress").is_ok(),
        "ferrite_control" => {
            ctx.store("ferrite-control-config").is_ok()
                && ctx.egress("ferrite-control-egress").is_ok()
        }
        "store" => ctx.store("worker-results").is_ok(),
        "blob" => {
            ctx.blob("agent-workspace-content").is_ok()
                && ctx.store("agent-workspace-index").is_ok()
        }
        "buzz" => ctx.store("events").is_ok() && ctx.queue("agent-turns").is_ok(),
        // The Fabric A2A seam: outbound egress to the configured A2A peer.
        // The client-token secret is checked at wiring time (ingress), not
        // here — a definition may declare the capability wherever the egress
        // is bound, and the tools degrade legibly if the token is missing.
        "fabric-a2a" => ctx.egress("fabric-a2a-egress").is_ok(),
        _ => false,
    }
}

/// Why a persona's stored definition could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefinitionLoadError {
    /// The config store could not be reached or read; retrying may succeed.
    Unavailable(String),
    /// A definition or bundle head is stored but is invalid, fails its digest
    /// chain, or requires a capability this deployment does not bind.
    Invalid(String),
}

impl std::fmt::Display for DefinitionLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(why) => write!(f, "the agent definition could not be read: {why}"),
            Self::Invalid(why) => write!(f, "the agent definition is unusable: {why}"),
        }
    }
}

/// Dual-read loader. Presence of a v2 head suppresses v1 fallback even when
/// the v2 chain is invalid, preventing generation/resource mix-and-match.
///
/// `Ok(None)` means only that no definition is stored. A store that cannot be
/// read, or a stored definition that cannot be used, is an error: collapsing
/// either into "no definition" would run the persona on the unrestricted
/// default surface, without its tool filter, approvals or step budget.
pub async fn load_agent_definition_dual(
    ctx: &AppContext,
    store_handle: &str,
    agent: &str,
    max_steps_cap: usize,
) -> Result<Option<AgentDefinition>, DefinitionLoadError> {
    use DefinitionLoadError::{Invalid, Unavailable};
    let mut store = ctx.store(store_handle).map_err(|e| Unavailable(e.to_string()))?;
    let head_bytes = store
        .get(&bundle_head_key(agent))
        .await
        .map_err(|e| Unavailable(e.to_string()))?;
    let Some(head_bytes) = head_bytes else {
        let Some(bytes) = store
            .get(&agent_def_key(agent))
            .await
            .map_err(|e| Unavailable(e.to_string()))?
        else {
            return Ok(None);
        };
        return parse_agent_definition(&bytes, agent, max_steps_cap)
            .map(Some)
            .ok_or_else(|| Invalid("the stored v1 definition does not parse".into()));
    };
    let head: BundleHead = ferrite_app::serde_json::from_slice(&head_bytes)
        .map_err(|e| Invalid(format!("the bundle head does not parse: {e}")))?;
    if head.version != BUNDLE_VERSION
        || head.agent_id != agent
        || !head.bundle_id.starts_with("sha256:")
        || head.bundle_id.len() != 71
        || head.manifest_key != format!("agent-bundle:v2:{}", &head.bundle_id[7..])
    {
        return Err(Invalid("the bundle head does not name a valid bundle for this agent".into()));
    }
    let manifest = store
        .get(head.manifest_key.as_bytes())
        .await
        .map_err(|e| Unavailable(e.to_string()))?
        .ok_or_else(|| Invalid("the bundle head names a manifest that is not stored".into()))?;
    let mut definition = parse_agent_bundle(&manifest, agent, &head.bundle_id, max_steps_cap)
        .ok_or_else(|| Invalid("the bundle manifest does not verify".into()))?;
    let mut unavailable = Vec::new();
    for name in definition
        .capability_required
        .iter()
        .chain(definition.capability_optional.iter())
    {
        if !capability_available(ctx, name) {
            unavailable.push(name.clone());
        }
    }
    if let Some(missing) = definition
        .capability_required
        .iter()
        .find(|name| unavailable.iter().any(|missing| missing == *name))
    {
        return Err(Invalid(format!(
            "it requires the {missing:?} capability, which this deployment does not provide"
        )));
    }
    definition.capability_unavailable = unavailable;
    Ok(Some(definition))
}

fn sha256(input: &[u8]) -> [u8; 32] {
    ferrite_app::crypto::sha256(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_bundle() -> (Vec<u8>, String) {
        let instructions = "# Agent\n";
        let resource_digest = digest_id(instructions.as_bytes());
        let resource_id = format!("resource:{resource_digest}");
        let mut value = serde_json::json!({
            "format": "ferrite-agent-bundle",
            "version": 2,
            "compiler": { "name": "agentc", "version": "0.2.0" },
            "runtime_abi": { "min": 2, "max": 2 },
            "profile": "ferrite-v1",
            "agents": [{
                "id": "agent",
                "instructions_ref": resource_id,
                "model": { "default": "claude-sonnet-5" },
                "limits": { "max_steps": 4 },
                "tool_policy": { "allow": null, "deny": [], "approvals": [] },
                "capabilities": {
                    "catalog_version": 1,
                    "required": ["model", "sessions"],
                    "optional": ["web"],
                    "options": {}
                },
                "contributions": {
                    "tools": [], "skills": [], "hooks": [], "state": [],
                    "connections": [], "modules": [], "subagents": [], "schedules": []
                },
                "ask_question": false
            }],
            "resources": [{
                "id": resource_id,
                "kind": "instructions",
                "media_type": "text/markdown; charset=utf-8",
                "bytes": instructions.len(),
                "digest": resource_digest,
                "source": "instructions.md",
                "content": instructions
            }],
            "provenance": [{ "kind": "agent", "source": "agent.ts", "profile": "ferrite-v1" }]
        });
        let canonical = ferrite_app::serde_json::to_vec(&canonicalize(value.clone())).unwrap();
        let bundle_id = digest_id(&canonical);
        value
            .as_object_mut()
            .unwrap()
            .insert("bundle_id".into(), Value::String(bundle_id.clone()));
        (
            ferrite_app::serde_json::to_vec(&canonicalize(value)).unwrap(),
            bundle_id,
        )
    }

    #[test]
    fn sha256_matches_nist_vectors() {
        assert_eq!(
            digest_id(b""),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            digest_id(b"abc"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn tool_grants_lower_logical_authority_only() {
        let value = serde_json::json!({
            "required": ["store"],
            "optional": ["blob"],
            "options": {
                "store": { "stores": { "reports": {
                    "read": true, "write": true, "prefix": "reports/"
                }}},
                "blob": { "blobs": { "artifacts": {
                    "read": true, "write": false, "prefix": "exports/"
                }}}
            }
        });
        let parsed = parse_tool_capabilities(&value).expect("valid logical grants");
        assert_eq!(parsed.store_grants[0].name, "reports");
        assert_eq!(parsed.store_grants[0].prefix, "reports/");
        assert!(parsed.store_grants[0].write);
        assert_eq!(parsed.blob_grants[0].name, "artifacts");
        assert!(!parsed.blob_grants[0].write);
        assert!(parsed.callbacks.is_empty());
        assert!(parsed.egress_hosts.is_empty());
        assert_eq!(parsed.required, ["store"]);
        assert_eq!(parsed.optional, ["blob"]);
    }

    #[test]
    fn tool_grants_reject_physical_or_escaping_authority() {
        let physical = serde_json::json!({
            "required": ["store"],
            "optional": [],
            "options": { "store": { "stores": { "reports": {
                "read": true, "write": true, "prefix": "../secrets/", "binding": "root"
            }}}}
        });
        assert!(parse_tool_capabilities(&physical).is_none());
    }

    #[test]
    fn v2_bundle_parses_and_pins_generation() {
        let (bytes, bundle_id) = fixture_bundle();
        let definition =
            parse_agent_bundle(&bytes, "agent", &bundle_id, 8).expect("valid v2 bundle");
        assert_eq!(definition.instructions.as_deref(), Some("# Agent\n"));
        assert_eq!(definition.default_model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(definition.effective_max_steps(8), 4);
        assert_eq!(
            definition.bundle_generation.as_deref(),
            Some(bundle_id.as_str())
        );
        assert_eq!(definition.capability_required, ["model", "sessions"]);
        assert_eq!(definition.capability_optional, ["web"]);
    }

    #[test]
    fn v2_tampering_and_unknown_fields_are_inert() {
        let (bytes, bundle_id) = fixture_bundle();
        let mut tampered: Value = ferrite_app::serde_json::from_slice(&bytes).unwrap();
        tampered["agents"][0]["limits"]["max_steps"] = Value::from(5);
        assert!(parse_agent_bundle(
            &ferrite_app::serde_json::to_vec(&tampered).unwrap(),
            "agent",
            &bundle_id,
            8
        )
        .is_none());

        let (bytes, bundle_id) = fixture_bundle();
        let mut unknown: Value = ferrite_app::serde_json::from_slice(&bytes).unwrap();
        unknown["compiler"]["surprise"] = Value::Bool(true);
        let mut without_id = unknown.clone();
        without_id.as_object_mut().unwrap().remove("bundle_id");
        let new_id =
            digest_id(&ferrite_app::serde_json::to_vec(&canonicalize(without_id)).unwrap());
        unknown["bundle_id"] = Value::String(new_id.clone());
        assert!(parse_agent_bundle(
            &ferrite_app::serde_json::to_vec(&unknown).unwrap(),
            "agent",
            &new_id,
            8
        )
        .is_none());
        assert_ne!(new_id, bundle_id);
    }
}

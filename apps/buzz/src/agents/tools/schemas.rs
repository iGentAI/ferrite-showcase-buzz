use ferrite_app::serde_json::{self, Value};

use crate::agents::apply_patch;

/// One declared tool the model may call.
#[derive(Clone)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

fn tool(name: &str, description: &str, input_schema: Value) -> ToolSchema {
    ToolSchema {
        name: name.to_string(),
        description: description.to_string(),
        input_schema,
    }
}

/// The coding tool surface a model is shown.
pub fn coding_tool_schemas() -> Vec<ToolSchema> {
    let tools = vec![
        tool(
            "list_files",
            "List every file path in the workspace. Call this first to understand project structure.",
            serde_json::json!({"type":"object","properties":{},"required":[]}),
        ),
        tool(
            "read_file",
            "Read the full current contents of a file. You MUST read a file before editing it.",
            serde_json::json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        ),
        tool(
            "write_file",
            "Create a file or fully replace its contents.",
            serde_json::json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}),
        ),
        tool(
            "edit_file",
            "Replace an exact, unique substring in a file with new text (anti-hallucination: old_str must appear exactly once). Read the file first. For multi-file or multi-hunk edits, prefer apply_patch.",
            serde_json::json!({"type":"object","properties":{
                "path":{"type":"string"},
                "old_str":{"type":"string","description":"exact text to replace; must be unique in the file"},
                "new_str":{"type":"string"}
            },"required":["path","old_str","new_str"]}),
        ),
        tool(
            apply_patch::APPLY_PATCH_TOOL_NAME,
            apply_patch::APPLY_PATCH_INSTRUCTIONS,
            apply_patch::apply_patch_spec()["parameters"].clone(),
        ),
        tool(
            "search_files",
            "Search file contents (the grep). Literal substring only on this runtime (regex is unavailable). Optionally restrict files with a glob. Returns matching path:line:text (capped).",
            serde_json::json!({"type":"object","properties":{
                "query":{"type":"string","description":"literal substring to find"},
                "glob":{"type":"string","description":"optional glob restricting which files are searched, e.g. 'src/**/*.rs'"}
            },"required":["query"]}),
        ),
        tool(
            "find_files",
            "List workspace file paths matching a glob (the find): '*' within a segment, '**' across directories, '?' one char. E.g. '*.md', 'src/**/*.rs'.",
            serde_json::json!({"type":"object","properties":{"glob":{"type":"string"}},"required":["glob"]}),
        ),
        tool(
            "file_history",
            "List the durable version numbers that exist for a file in the versioned Ferrite workspace.",
            serde_json::json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        ),
        tool(
            "read_file_version",
            "Read an earlier durable version of a file from the versioned Ferrite workspace.",
            serde_json::json!({"type":"object","properties":{
                "path":{"type":"string"},
                "version":{"type":"integer","minimum":1}
            },"required":["path","version"]}),
        ),
        tool(
            "end_turn",
            "Finish the turn. Call this only when the task is fully complete and verified. Provide a short summary of what you did.",
            serde_json::json!({"type":"object","properties":{"summary":{"type":"string"}},"required":["summary"]}),
        ),
        tool(
            "remember_memory",
            "Store a durable memory through ferrite-memory or the native Store-backed memory binding.",
            serde_json::json!({"type":"object","properties":{
                "text":{"type":"string"},
                "category":{"type":"string"},
                "entities":{"type":"array","items":{"type":"string"}}
            },"required":["text"]}),
        ),
        tool(
            "get_memory",
            "Read one durable memory record by id.",
            serde_json::json!({"type":"object","properties":{"id":{"type":"string"}},"required":["id"]}),
        ),
        tool(
            "list_memory",
            "List durable memory records in the current agent scope.",
            serde_json::json!({"type":"object","properties":{
                "include_superseded":{"type":"boolean","description":"include records hidden by a superseding correction"}
            },"required":[]}),
        ),
        tool(
            "correct_memory",
            "Correct a durable memory record in place.",
            serde_json::json!({"type":"object","properties":{
                "id":{"type":"string"},
                "text":{"type":"string"}
            },"required":["id","text"]}),
        ),
        tool(
            "supersede_memory",
            "Create a replacement memory that supersedes an existing record.",
            serde_json::json!({"type":"object","properties":{
                "supersedes":{"type":"string"},
                "text":{"type":"string"},
                "category":{"type":"string"},
                "entities":{"type":"array","items":{"type":"string"}}
            },"required":["supersedes","text"]}),
        ),
        tool(
            "forget_memory",
            "Forget/delete one durable memory record by id.",
            serde_json::json!({"type":"object","properties":{"id":{"type":"string"}},"required":["id"]}),
        ),
        tool(
            "recall_memory",
            "Recall durable memories through ferrite-memory or the native Store-backed memory binding.",
            serde_json::json!({"type":"object","properties":{
                "query":{"type":"string"},
                "limit":{"type":"integer","minimum":1,"maximum":20}
            },"required":["query"]}),
        ),
    ];
    tools
}

/// The `load_skill` tool : shown only
/// when the active agent definition declares skills; the description
/// enumerates exactly the loadable names so the model never guesses.
pub fn load_skill_tool_schema(skills: &[String]) -> ToolSchema {
    tool(
        "load_skill",
        &format!(
            "Load one of this agent's skill documents (markdown playbooks) into the \
             conversation. Available skills: {}. Load a skill BEFORE attempting the \
             task it covers.",
            skills.join(", ")
        ),
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The skill to load (one of the available skills)."
                }
            },
            "required": ["name"]
        }),
    )
}

/// The `delegate` tool (subagents): available only when the agent definition
/// declares subagents. The model hands a focused task to one declared child;
/// the child runs with FRESH history and its own tool surface, and its final
/// answer returns as the tool result. The `subagent` enum + per-id summary
/// give the model target discoverability.
pub fn delegate_tool_schema(subagents: &[crate::agents::definition::SubagentSpec]) -> ToolSchema {
    let ids: Vec<ferrite_app::serde_json::Value> = subagents.iter().map(|s| serde_json::json!(s.id)).collect();
    let roster = subagents
        .iter()
        .map(|s| {
            let first = s
                .instructions
                .lines()
                .map(|l| l.trim_start_matches('#').trim())
                .find(|l| !l.is_empty())
                .unwrap_or("");
            format!("'{}' — {}", s.id, first)
        })
        .collect::<Vec<_>>()
        .join("; ");
    tool(
        "delegate",
        &format!(
            "Delegate a focused subtask to one of this agent's subagents. Each runs \
             with FRESH history and sees ONLY the task you pass, then returns an \
             answer. Subagents: {roster}."
        ),
        serde_json::json!({
            "type": "object",
            "properties": {
                "subagent": {
                    "type": "string",
                    "enum": ids,
                    "description": "Which subagent to delegate to (one of the declared ids)."
                },
                "task": {
                    "type": "string",
                    "description": "The focused task. The child starts with fresh history and sees only this."
                }
            },
            "required": ["subagent", "task"],
            "additionalProperties": false
        }),
    )
}

/// The `ask_question` tool: available only when the agent
/// definition opts in. The model asks the user something and ends the turn;
/// the answer arrives as the next message.
pub fn ask_question_tool_schema() -> ToolSchema {
    tool(
        "ask_question",
        "Ask the user a question when you need information or a decision to proceed. \
         After calling this, relay the question and call end_turn; the user's answer \
         will arrive as the next message.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question to put to the user."
                }
            },
            "required": ["question"]
        }),
    )
}

/// The buzz workspace tool surface: what a persona can do in
/// its community. Every write is executed BY THE RELAY under the persona's
/// relay-held key and passes the relay's own gates (membership, moderation,
/// rate limits), so nothing here grants what the workspace would refuse.
pub fn buzz_tool_schemas() -> Vec<ToolSchema> {
    vec![
        tool(
            "buzz_post",
            "Post a chat message into a buzz workspace channel as this persona. \
             Optionally reply in-thread by passing reply_to (the event id being answered). \
             The write is membership-gated by the workspace: posting into a channel the \
             persona is not a member of is refused.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "channel": {"type": "string", "description": "The channel id to post into."},
                    "content": {"type": "string", "description": "The message text."},
                    "reply_to": {"type": "string", "description": "Optional: the 64-hex event id this message answers; threads the reply under that event."}
                },
                "required": ["channel", "content"]
            }),
        ),
        tool(
            "buzz_react",
            "React to a buzz workspace event (e.g. acknowledge a request with an emoji).",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "target": {"type": "string", "description": "The 64-hex event id to react to."},
                    "emoji": {"type": "string", "description": "The reaction emoji, e.g. \"👍\"."}
                },
                "required": ["target", "emoji"]
            }),
        ),
        tool(
            "buzz_read",
            "Read a buzz channel's recent top-level timeline (newest first, with thread \
             summaries). Access-gated as this persona: a private channel the persona is \
             not a member of reads as empty. Page with the returned next_cursor while \
             bounds.has_more is true.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "channel": {"type": "string", "description": "The channel id to read."},
                    "cursor": {"type": "string", "description": "Optional: the next_cursor from a previous page."},
                    "limit": {"type": "integer", "description": "Rows per page (1-200, default 50)."}
                },
                "required": ["channel"]
            }),
        ),
    ]
}

/// The scheduling tool surface: a persona arranging its own future turn.
///
/// Always available where the buzz seam is, because the mechanism needs nothing
/// this deployment might not have — a wake is a delayed publish of the same
/// `TurnRequest` a mention makes, on the queue the turn pipeline already consumes.
///
/// A persona may schedule only ITSELF. The agent id comes from the turn's own
/// binding rather than from the arguments, so there is no cross-agent scheduling
/// to authorize: it does not exist.
///
/// The bounds are IN the descriptions because the reader is a model. A refusal it
/// can predict is one it does not spend a step discovering.
pub fn schedule_tool_schemas() -> Vec<ToolSchema> {
    vec![
        tool(
            "buzz_schedule",
            "Arrange for YOU to be woken later with an instruction, and answer in a \
             channel then. Use this instead of promising a follow-up you cannot \
             perform: nothing else will remind you. The wake runs a fresh turn with \
             the prompt you give here, so write it as an instruction to your future \
             self, including any context it will need. Recurring schedules must be at \
             least 60000 ms apart and run at most 1000 times. Asking twice for the \
             same thing is ONE schedule, not two — the id is derived from the request \
             — so check buzz_schedules before adding another.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "channel": {"type": "string", "description": "The channel id your answer should appear in. You must be able to post there."},
                    "prompt": {"type": "string", "description": "The instruction for your future self. It is the whole of what you will be told, so include the context."},
                    "in_ms": {"type": "integer", "description": "How long from now the first wake should happen, in milliseconds. Omit for as soon as possible."},
                    "interval_ms": {"type": "integer", "description": "Optional: repeat this often, in milliseconds. At least 60000 (one minute)."},
                    "occurrences": {"type": "integer", "description": "Optional: how many times a recurring schedule should run (max 1000). Ignored without interval_ms."}
                },
                "required": ["channel", "prompt"]
            }),
        ),
        tool(
            "buzz_schedules",
            "List the wakes YOU already have scheduled, with their ids and when each is \
             next due. Call this before scheduling something you may already have \
             arranged, and to find the id you need for buzz_unschedule.",
            serde_json::json!({"type": "object", "properties": {}, "required": []}),
        ),
        tool(
            "buzz_unschedule",
            "Cancel a schedule YOU created, by its id from buzz_schedules. A wake that \
             is already queued for it will find the cancellation and do nothing, so \
             this stops future occurrences even at the last moment.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "schedule": {"type": "string", "description": "The schedule id from buzz_schedules."}
                },
                "required": ["schedule"]
            }),
        ),
    ]
}

/// The MCP connector tool surface: what a persona can reach on the
/// third-party MCP servers this workspace has connected.
///
/// Added by the turn pipeline ONLY when the deployment carries an `mcp-egress`
/// handle, the same rule the web and Fabric surfaces follow: advertising a tool
/// the egress plane would refuse is a trap for the model.
///
/// `mcp_connectors` exists because a tool DESCRIPTION is static text while the
/// catalog and its per-connector connection state are data — there is no way to
/// enumerate them in a schema, so the model is given a way to ask. It reports
/// `connected` per row precisely so a model can tell "this workspace could reach
/// Linear if somebody authorized it" from "this workspace has no Linear", and say
/// the useful one out loud instead of calling into a 401 it cannot act on.
pub fn mcp_tool_schemas() -> Vec<ToolSchema> {
    vec![
        tool(
            "mcp_connectors",
            "List the third-party MCP servers this workspace can reach, and whether each \
             is authenticated. Call this FIRST: the set is a property of the deployment, \
             so it cannot be listed in a tool description. A connector with \
             connected: false needs an operator to authorize it — say so in the channel \
             rather than retrying, because no amount of retrying will connect it.",
            serde_json::json!({"type": "object", "properties": {}, "required": []}),
        ),
        tool(
            "mcp_list_tools",
            "List the tools one connected MCP server offers, with their input schemas. \
             Call this before mcp_call_tool: the tool names and arguments are the \
             CONNECTOR's vocabulary, not this workspace's, so they cannot be guessed.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "connector": {
                        "type": "string",
                        "description": "The connector id from mcp_connectors, e.g. \"linear\"."
                    }
                },
                "required": ["connector"]
            }),
        ),
        tool(
            "mcp_call_tool",
            "Call a tool on a connected MCP server. Use the exact tool name and argument \
             shape mcp_list_tools reported for that connector. The call goes out over \
             this deployment's audited egress under the workspace's own credential, so \
             it acts as the workspace rather than as you.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "connector": {
                        "type": "string",
                        "description": "The connector id, e.g. \"linear\"."
                    },
                    "tool": {
                        "type": "string",
                        "description": "The tool name exactly as mcp_list_tools reported it."
                    },
                    "arguments": {
                        "type": "object",
                        "description": "The arguments, matching that tool's own input schema."
                    }
                },
                "required": ["connector", "tool"]
            }),
        ),
    ]
}

/// The Fabric A2A tool surface (the `a2a`/`fabric` harness seam): ask the
/// workspace's configured A2A peer a question and poll for its answer. Shown
/// only when the `fabric-a2a-egress` handle AND its client-token secret are
/// both present, like the buzz seam. The latency descriptions are deliberate:
/// a Fabric turn takes 30-120 seconds, so the model must treat `working` as
/// progress, never as failure.
pub fn fabric_a2a_tool_schemas() -> Vec<ToolSchema> {
    vec![
        tool(
            "fabric_ask",
            "Ask the configured Fabric A2A peer agent a question. Returns a task_id \
             immediately while the peer works; a Fabric turn takes 30-120 seconds, so \
             call fabric_ask_result with the task_id until its state is terminal \
             (completed/failed/canceled). Pass the same thread_key to keep one durable \
             Fabric conversation per topic, or task_id to continue an earlier task \
             (the peer remembers that task's context). Do NOT start a second ask on \
             the same thread_key while one is still working.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "The question or instruction for the peer agent."},
                    "thread_key": {"type": "string", "description": "Optional conversation key; within this conversation, the same key always reaches the same Fabric thread."},
                    "task_id": {"type": "string", "description": "Optional: a previous fabric_ask task id from this conversation to continue (the peer keeps that conversation's context)."}
                },
                "required": ["prompt"]
            }),
        ),
        tool(
            "fabric_ask_result",
            "Poll a Fabric A2A task started with fabric_ask. `working` means the peer \
             is still thinking — normal for 30-120 seconds; wait and call again rather \
             than treating it as failure. On `completed` the peer's reply is in `text`. \
             A `failed` task usually means the turn was superseded: re-ask with a fresh \
             thread_key instead of trusting that context again.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "task_id": {"type": "string", "description": "The task id returned by fabric_ask."}
                },
                "required": ["task_id"]
            }),
        ),
    ]
}

fn prop_string(description: &str) -> Value {
    serde_json::json!({"type":"string","description":description})
}

fn prop_integer(description: &str) -> Value {
    serde_json::json!({"type":"integer","description":description})
}

fn prop_object(description: &str) -> Value {
    serde_json::json!({"type":"object","description":description})
}

fn web_target_props(extra: Value) -> Value {
    let mut base = serde_json::json!({
        "url": prop_string("the page URL to render (http/https). Provide url OR html, not both."),
        "html": prop_string("raw HTML to render instead of fetching a URL"),
        "options": prop_object("passthrough Cloudflare options: viewport, gotoOptions, waitForSelector, etc.")
    });
    if let (Some(base), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            base.insert(key.clone(), value.clone());
        }
    }
    base
}

/// The web tool surface (Brave search, Perplexity, Cloudflare browser
/// rendering). Added by the turn pipeline ONLY when the deployment carries a
/// `web-egress` handle: advertising a tool the egress plane would refuse is a
/// trap for the model, and the buzz manifest grants provider + Fabric egress
/// only. A deployment that adds the grant lights these up with no code change.
pub fn web_tool_schemas() -> Vec<ToolSchema> {
    vec![
        tool(
            "web_search",
            "Search the web with Brave Search. Requires configured Brave credentials and uses Ferrite GET egress for Brave's API.",
            serde_json::json!({"type":"object","properties":{
                "query": prop_string("the search query, max about 400 characters"),
                "count": prop_integer("number of results, max 20"),
                "country": prop_string("2-letter country code, e.g. us"),
                "freshness": prop_string("recency filter: pd|pw|pm|py or YYYY-MM-DDtoYYYY-MM-DD"),
                "safesearch": prop_string("off|moderate|strict")
            },"required":["query"]}),
        ),
        tool(
            "news_search",
            "Search recent news with Brave Search. Requires configured Brave credentials and uses Ferrite GET egress for Brave's API.",
            serde_json::json!({"type":"object","properties":{
                "query": prop_string("the news query"),
                "count": prop_integer("number of results, max 50"),
                "country": prop_string("2-letter country code"),
                "freshness": prop_string("recency filter: pd|pw|pm|py"),
                "safesearch": prop_string("off|moderate|strict")
            },"required":["query"]}),
        ),
        tool(
            "perplexity_ask",
            "Ask Perplexity Sonar for a synthesized, web-grounded answer plus citations.",
            serde_json::json!({"type":"object","properties":{
                "query": prop_string("the question to answer"),
                "model": prop_string("sonar|sonar-pro|sonar-reasoning-pro|sonar-deep-research"),
                "recency": prop_string("hour|day|week|month|year"),
                "domains": {"type":"array","items":{"type":"string"},"description":"restrict to these domains"},
                "search_mode": prop_string("web|academic|sec"),
                "max_tokens": prop_integer("max answer length")
            },"required":["query"]}),
        ),
        tool(
            "perplexity_search",
            "Use Perplexity Search API for ranked web results without a generated answer.",
            serde_json::json!({"type":"object","properties":{
                "query": prop_string("the search query"),
                "max_results": prop_integer("number of results, 1-20")
            },"required":["query"]}),
        ),
        tool(
            "browser_content",
            "Fetch a webpage with Cloudflare Browser Rendering and return fully-rendered HTML.",
            serde_json::json!({"type":"object","properties":web_target_props(serde_json::json!({})),"required":[]}),
        ),
        tool(
            "browser_markdown",
            "Fetch a webpage with Cloudflare Browser Rendering and return clean Markdown.",
            serde_json::json!({"type":"object","properties":web_target_props(serde_json::json!({})),"required":[]}),
        ),
        tool(
            "browser_links",
            "Fetch a webpage with Cloudflare Browser Rendering and return links.",
            serde_json::json!({"type":"object","properties":web_target_props(serde_json::json!({
                "visibleLinksOnly": {"type":"boolean","description":"only links visible in the viewport"},
                "excludeExternalLinks": {"type":"boolean","description":"only same-site links"}
            })),"required":[]}),
        ),
        tool(
            "browser_scrape",
            "Scrape specific elements from a rendered page by CSS selector.",
            serde_json::json!({"type":"object","properties":web_target_props(serde_json::json!({
                "elements": {"type":"array","items":{"type":"object"},"description":"[{selector:'...'}] elements to extract"}
            })),"required":["elements"]}),
        ),
        tool(
            "browser_json",
            "Extract structured data from a rendered page using Cloudflare Browser Rendering AI extraction.",
            serde_json::json!({"type":"object","properties":web_target_props(serde_json::json!({
                "prompt": prop_string("what structured data to extract"),
                "response_format": prop_object("JSON schema response format")
            })),"required":[]}),
        ),
        tool(
            "browser_snapshot",
            "Capture multiple rendered formats of a page.",
            serde_json::json!({"type":"object","properties":web_target_props(serde_json::json!({
                "formats": {"type":"array","items":{"type":"string"},"description":"content|screenshot|markdown|accessibilityTree"}
            })),"required":[]}),
        ),
        tool(
            "browser_screenshot",
            "Screenshot a rendered page and return base64 bytes.",
            serde_json::json!({"type":"object","properties":web_target_props(serde_json::json!({
                "screenshotOptions": prop_object("{fullPage, type:'png'|'jpeg', quality, omitBackground}"),
                "selector": prop_string("screenshot only the matching CSS selector")
            })),"required":[]}),
        ),
        tool(
            "browser_pdf",
            "Render a page to PDF and return base64 bytes.",
            serde_json::json!({"type":"object","properties":web_target_props(serde_json::json!({
                "pdfOptions": prop_object("{format:'a4', landscape, printBackground, margin}")
            })),"required":[]}),
        ),
        tool(
            "browser_crawl",
            "Start a Cloudflare Browser Rendering crawl job. Poll with browser_crawl_status.",
            serde_json::json!({"type":"object","properties":{
                "url": prop_string("the starting URL"),
                "options": prop_object("crawl params: limit, depth, formats, render, source")
            },"required":["url"]}),
        ),
        tool(
            "browser_crawl_status",
            "Poll a Cloudflare crawl job. Requires Ferrite egress method support for Cloudflare's GET status API.",
            serde_json::json!({"type":"object","properties":{
                "job_id": prop_string("the crawl job id"),
                "cursor": prop_integer("pagination cursor"),
                "limit": prop_integer("max records to return")
            },"required":["job_id"]}),
        ),
        tool(
            "browser_automate",
            "Drive an interactive browser session through Ferrite WebSocket egress and Cloudflare Browser Rendering CDP.",
            serde_json::json!({"type":"object","properties":{
                "url": prop_string("optional page to open first"),
                "steps": {"type":"array","items":{"type":"object"},"description":"ordered actions: goto, title, text, html, evaluate, click, type, screenshot, wait"},
                "timeout_ms": prop_integer("per-CDP-command timeout in milliseconds")
            },"required":[]}),
        ),
    ]
}

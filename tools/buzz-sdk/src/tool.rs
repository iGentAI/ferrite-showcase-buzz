//! Tools: the agent's hands.
//!
//! A tool is a plain async function over a typed argument struct:
//!
//! ```no_run
//! use buzz_sdk::{tool, Result};
//! use serde::Deserialize;
//!
//! #[derive(Deserialize)]
//! struct ReviewArgs {
//!     target: String,
//! }
//!
//! async fn review_patch(args: ReviewArgs) -> Result<String> {
//!     Ok(format!("reviewed {}", args.target))
//! }
//!
//! let t = tool("review_patch", "Review a patch or branch", review_patch);
//! ```
//!
//! Registration is the whole point of this module: `tool(name, description, f)`
//! erases the argument type into a JSON-in/String-out handler so the registry can
//! hold heterogeneous tools, while the function you wrote stays typed.
//!
//! ## The schema, honestly
//!
//! The SDK does **not** reflect over `A`'s fields. There is no schema crate here
//! (the dependency budget goes to the protocol, not to derive macros), and
//! hand-rolling a partial JSON-Schema deriver would be worse than not having one:
//! it would be right often enough to be trusted and wrong often enough to
//! mislead a model. So by default a tool advertises the permissive
//! `{"type":"object"}`, and:
//!
//! * the model sees the tool's **name and description**, which is where you
//!   should spend your words — describe the arguments in prose there;
//! * if the model sends fields your type rejects, `serde` fails and the
//!   deserialization error is handed back to the model as a tool error
//!   ([`SdkError::Tool`]) rather than killing the turn, so it can correct itself
//!   on the next step;
//! * when you want the model to get it right first time, attach the schema
//!   yourself with [`Tool::schema`] or [`tool_with_schema`].
//!
//! ## Cost
//!
//! One `Arc` clone to put a tool in a registry; one `serde_json::from_value` per
//! call (which copies the arguments into your struct); one boxed future per call.

use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use crate::backend::ToolSpec;
use crate::error::{Result, SdkError};
use crate::transport::BoxFuture;

/// The type-erased handler a [`Tool`] holds.
type Handler = Arc<dyn Fn(Value) -> BoxFuture<'static, Result<String>> + Send + Sync>;

/// A registered tool: a name, a description the model reads, the schema the model
/// is shown, and the function to run.
///
/// `Clone` is an `Arc` bump — cloning a tool does not clone your closure.
#[derive(Clone)]
pub struct Tool {
    name: String,
    description: String,
    schema: Value,
    handler: Handler,
}

impl core::fmt::Debug for Tool {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Tool")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("schema", &self.schema)
            .finish_non_exhaustive()
    }
}

/// The permissive schema a tool advertises when the caller supplies none.
///
/// `{"type":"object"}` says "an object, shape unspecified" — true, and the only
/// thing the SDK can honestly claim without reflecting over the argument type.
#[must_use]
pub fn permissive_schema() -> Value {
    json!({ "type": "object" })
}

impl Tool {
    /// Register a typed async function as a tool.
    ///
    /// Prefer the free function [`tool`], which reads better at a call site;
    /// this is the same thing spelled as a constructor.
    pub fn new<A, F, Fut>(name: impl Into<String>, description: impl Into<String>, f: F) -> Self
    where
        A: DeserializeOwned + Send + 'static,
        F: Fn(A) -> Fut + Send + Sync + 'static,
        Fut: core::future::Future<Output = Result<String>> + Send + 'static,
    {
        let name = name.into();
        let for_errors = name.clone();
        let handler: Handler = Arc::new(move |args: Value| {
            // Deserialize eagerly so a bad-argument error is reported as this
            // tool's error, not as a panic or a silent default.
            match serde_json::from_value::<A>(args) {
                Ok(parsed) => {
                    let fut = f(parsed);
                    Box::pin(fut)
                }
                Err(e) => {
                    let tool = for_errors.clone();
                    Box::pin(async move {
                        Err(SdkError::tool(
                            tool,
                            format!("arguments did not match this tool's type: {e}"),
                        ))
                    })
                }
            }
        });
        Self {
            name,
            description: description.into(),
            schema: permissive_schema(),
            handler,
        }
    }

    /// Register a tool that takes raw JSON, for arguments with no Rust type.
    pub fn raw<F, Fut>(name: impl Into<String>, description: impl Into<String>, f: F) -> Self
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: core::future::Future<Output = Result<String>> + Send + 'static,
    {
        let handler: Handler = Arc::new(move |args: Value| Box::pin(f(args)));
        Self {
            name: name.into(),
            description: description.into(),
            schema: permissive_schema(),
            handler,
        }
    }

    /// Attach an explicit JSON Schema for the arguments.
    ///
    /// Chainable. Worth doing for any tool with more than one field, or with an
    /// enum: it is the difference between the model guessing your struct and
    /// being told it.
    #[must_use]
    pub fn schema(mut self, schema: Value) -> Self {
        self.schema = schema;
        self
    }

    /// Replace the description the model reads.
    #[must_use]
    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// The registered name, which is how the model refers to it.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The description the model reads.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// The schema the model is shown.
    #[must_use]
    pub const fn json_schema(&self) -> &Value {
        &self.schema
    }

    /// Whether this tool is still advertising the permissive default schema.
    ///
    /// Useful in a startup log or a test that wants every tool documented.
    #[must_use]
    pub fn has_explicit_schema(&self) -> bool {
        self.schema != permissive_schema()
    }

    /// The wire form a [`crate::Backend`] sends to the model.
    #[must_use]
    pub fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: self.schema.clone(),
        }
    }

    /// Run the tool.
    ///
    /// # Errors
    ///
    /// [`SdkError::Tool`] if the arguments do not deserialize, or whatever the
    /// tool itself returned.
    pub async fn call(&self, arguments: Value) -> Result<String> {
        (self.handler)(arguments).await
    }
}

/// Register a typed async function as a tool.
///
/// The ergonomic entry point:
///
/// ```ignore
/// .tool(tool("cut_release", "Cut a release for a tag", cut_release))
/// ```
///
/// `A` is your argument struct (anything `serde` can deserialize), and the
/// function may be any async `Fn(A) -> Result<String>` — a free `async fn`, a
/// closure returning an async block, or a boxed future.
pub fn tool<A, F, Fut>(name: impl Into<String>, description: impl Into<String>, f: F) -> Tool
where
    A: DeserializeOwned + Send + 'static,
    F: Fn(A) -> Fut + Send + Sync + 'static,
    Fut: core::future::Future<Output = Result<String>> + Send + 'static,
{
    Tool::new(name, description, f)
}

/// Register a typed async function together with the schema the model should see.
///
/// Same as [`tool`] plus [`Tool::schema`]; provided so the schema sits next to
/// the description it documents.
pub fn tool_with_schema<A, F, Fut>(
    name: impl Into<String>,
    description: impl Into<String>,
    schema: Value,
    f: F,
) -> Tool
where
    A: DeserializeOwned + Send + 'static,
    F: Fn(A) -> Fut + Send + Sync + 'static,
    Fut: core::future::Future<Output = Result<String>> + Send + 'static,
{
    Tool::new(name, description, f).schema(schema)
}

/// The tools one agent has, keyed by name.
///
/// Ordered: the model sees tools in registration order, which keeps a prompt
/// (and therefore a provider's prompt cache) stable across runs.
#[derive(Clone, Debug, Default)]
pub struct ToolRegistry {
    tools: Vec<Tool>,
}

impl ToolRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a tool, replacing any existing tool of the same name.
    ///
    /// Returns the tool that was displaced, if there was one — registering two
    /// tools under one name is a bug worth surfacing, not a silent overwrite.
    pub fn insert(&mut self, tool: Tool) -> Option<Tool> {
        if let Some(slot) = self.tools.iter_mut().find(|t| t.name == tool.name) {
            return Some(core::mem::replace(slot, tool));
        }
        self.tools.push(tool);
        None
    }

    /// Look up a tool by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Tool> {
        self.tools.iter().find(|t| t.name == name)
    }

    /// The specs to send to the model, in registration order.
    #[must_use]
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(Tool::spec).collect()
    }

    /// The registered names, in order.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.tools.iter().map(|t| t.name.as_str()).collect()
    }

    /// The names of tools still advertising the permissive default schema, in
    /// order — what [`AgentBuilder::require_tool_schemas`](crate::AgentBuilder::require_tool_schemas)
    /// refuses on.
    #[must_use]
    pub fn undocumented(&self) -> Vec<&str> {
        self.tools
            .iter()
            .filter(|t| !t.has_explicit_schema())
            .map(|t| t.name.as_str())
            .collect()
    }

    /// How many tools are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether no tools are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Run the named tool.
    ///
    /// # Errors
    ///
    /// [`SdkError::Tool`] if no tool has that name (the model hallucinated one —
    /// the error text is written to be useful *to the model*, since that is where
    /// it goes), or whatever the tool returned.
    pub async fn dispatch(&self, name: &str, arguments: Value) -> Result<String> {
        let Some(tool) = self.get(name) else {
            return Err(SdkError::tool(
                name,
                format!(
                    "no such tool; available tools are: {}",
                    if self.tools.is_empty() {
                        "(none)".to_string()
                    } else {
                        self.names().join(", ")
                    }
                ),
            ));
        };
        tool.call(arguments).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Deserialize)]
    struct Args {
        target: String,
        #[serde(default)]
        deep: bool,
    }

    async fn review(args: Args) -> Result<String> {
        Ok(format!("reviewed {} (deep={})", args.target, args.deep))
    }

    #[tokio::test]
    async fn a_typed_async_fn_registers_and_dispatches() {
        let mut registry = ToolRegistry::new();
        assert!(registry.is_empty());
        registry.insert(tool("review_patch", "Review a patch", review));

        assert_eq!(registry.len(), 1);
        assert_eq!(registry.names(), ["review_patch"]);

        let out = registry
            .dispatch("review_patch", json!({"target": "main", "deep": true}))
            .await
            .unwrap();
        assert_eq!(out, "reviewed main (deep=true)");
    }

    #[tokio::test]
    async fn closures_and_captured_state_work_as_tools() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let t = tool("count", "Count invocations", move |args: Args| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(args.target)
            }
        });
        assert_eq!(t.call(json!({"target": "a"})).await.unwrap(), "a");
        assert_eq!(t.call(json!({"target": "b"})).await.unwrap(), "b");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn bad_arguments_come_back_as_a_tool_error_naming_the_tool() {
        let t = tool("review_patch", "Review a patch", review);
        let err = t.call(json!({"wrong": "field"})).await.unwrap_err();
        let SdkError::Tool { tool, message } = &err else {
            panic!("expected a tool error, got {err:?}");
        };
        assert_eq!(tool, "review_patch");
        assert!(message.contains("target"), "{message}");
    }

    #[tokio::test]
    async fn an_unknown_tool_tells_the_model_what_does_exist() {
        let mut registry = ToolRegistry::new();
        registry.insert(tool("review_patch", "Review a patch", review));
        let err = registry.dispatch("deploy", json!({})).await.unwrap_err();
        assert!(err.to_string().contains("review_patch"), "{err}");

        let empty = ToolRegistry::new();
        let err = empty.dispatch("anything", json!({})).await.unwrap_err();
        assert!(err.to_string().contains("(none)"), "{err}");
    }

    #[test]
    fn the_default_schema_is_permissive_and_overridable() {
        let plain = tool("t", "d", review);
        assert_eq!(plain.json_schema(), &json!({"type": "object"}));
        assert!(!plain.has_explicit_schema());

        let described = tool_with_schema(
            "t",
            "d",
            json!({
                "type": "object",
                "properties": {"target": {"type": "string"}},
                "required": ["target"],
            }),
            review,
        );
        assert!(described.has_explicit_schema());
        assert_eq!(described.spec().input_schema["required"][0], "target");
    }

    #[tokio::test]
    async fn re_registering_a_name_replaces_and_reports() {
        let mut registry = ToolRegistry::new();
        assert!(registry.insert(tool("t", "first", review)).is_none());
        let displaced = registry
            .insert(Tool::raw("t", "second", |_v| async { Ok("raw".into()) }))
            .expect("the first registration should be handed back");
        assert_eq!(displaced.description(), "first");
        assert_eq!(registry.len(), 1, "no duplicate name in the registry");
        assert_eq!(registry.dispatch("t", json!({})).await.unwrap(), "raw");
    }

    #[test]
    fn specs_keep_registration_order() {
        let mut registry = ToolRegistry::new();
        registry.insert(tool("a", "", review));
        registry.insert(tool("b", "", review));
        registry.insert(tool("c", "", review));
        let specs = registry.specs();
        assert_eq!(
            specs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"],
            "prompt stability depends on this order"
        );
    }
}

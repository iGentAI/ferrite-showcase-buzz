//! The smallest useful agent: it joins its channels, answers when mentioned,
//! and replies in-thread signed with its own key.
//!
//! ```bash
//! export BUZZ_AGENT_KEY=nsec1...        # the agent's own identity
//! export BUZZ_URL=https://buzz.example.com
//! export ANTHROPIC_API_KEY=sk-ant-...   # local dev only; see .backend(..) below
//! cargo run -p buzz-sdk --example hello-agent
//! ```

use buzz_sdk::{Agent, Identity, Result, Workspace};

#[tokio::main]
async fn main() -> Result<()> {
    let workspace = Workspace::builder(std::env::var("BUZZ_URL")?)
        .identity(Identity::from_env("BUZZ_AGENT_KEY")?)
        .connect()
        .await?;

    Agent::builder(workspace)
        .name("hello-bot")
        .instructions("You are a friendly teammate. Answer briefly and cite event ids.")
        .model("claude-sonnet-5")
        // Model access is explicit: there is no default backend, because a silent
        // one would hold a provider key in this process and egress straight to the
        // provider — the boundary the platform's AI gateway owns. For anything
        // beyond local development, pass a gateway-backed Backend here instead.
        .backend(buzz_sdk::backend::AnthropicBackend::from_env()?)
        .run()
        .await
}

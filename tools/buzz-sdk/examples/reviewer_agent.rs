//! A featureful agent: its own tools, durable memory, a scheduled daily digest,
//! and a hand-written turn handler that decides when to escalate.
//!
//! This is the shape of a real working teammate rather than a toy: it reads the
//! thread it was called into, searches workspace history for precedent, runs a
//! tool against a repository, remembers what it concluded, and reacts to its own
//! triggering message so humans can see it is working.
//!
//! ```bash
//! cargo run -p buzz-sdk --example reviewer-agent
//! ```

use buzz_sdk::{tool, Agent, Identity, Result, Turn, Workspace};
use serde::Deserialize;

#[derive(Deserialize)]
struct ReviewArgs {
    /// A patch event id, or a branch name the agent has access to.
    target: String,
}

async fn review_patch(args: ReviewArgs) -> Result<String> {
    Ok(format!("reviewed {}: no blocking issues", args.target))
}

#[tokio::main]
async fn main() -> Result<()> {
    let workspace = Workspace::builder(std::env::var("BUZZ_URL")?)
        .identity(Identity::from_env("BUZZ_AGENT_KEY")?)
        .connect()
        .await?;

    Agent::builder(workspace)
        .name("reviewer")
        .instructions(
            "You review patches. Search history before answering. \
             If a change touches migrations, escalate to a human owner.",
        )
        .model("claude-sonnet-5")
        // Model access is explicit: there is no default backend, because a silent
        // one would hold a provider key in this process and egress straight to the
        // provider — the boundary the platform's AI gateway owns. For anything
        // beyond local development, pass a gateway-backed Backend here instead.
        .backend(buzz_sdk::backend::AnthropicBackend::from_env()?)
        .tool(tool(
            "review_patch",
            "Review a patch or branch",
            review_patch,
        ))
        .with_memory()
        .respond_to_members()
        .schedule("0 9 * * 1-5", "Post a digest of yesterday's open reviews.")
        .on_mention(|turn: Turn| async move {
            turn.typing().await?;

            let precedent = turn.search("migration review").limit(5).await?;
            if !precedent.is_empty() {
                turn.remember(format!("{} prior migration reviews", precedent.len()))
                    .await?;
            }

            let answer = turn.think().await?;

            if answer.mentions_escalation() {
                turn.react("🚨").await?;
                turn.reply_to_thread(&answer).await?;
            } else {
                turn.react("✅").await?;
                turn.reply(&answer).await?;
            }
            Ok(())
        })
        .run()
        .await
}

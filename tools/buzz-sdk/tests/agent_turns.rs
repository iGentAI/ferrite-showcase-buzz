//! End-to-end agent behavior, over the public API only.
//!
//! Everything here runs in-process: [`MemoryTransport`] stands in for the relay
//! and [`MockBackend`] for the model, so these tests exercise the whole path an
//! agent takes — subscribe, admit, batch, prompt, tool, reply — with no network,
//! no clock drift and no API key. That is also the shape an agent author's own
//! tests should take, which is why both fakes ship in the library.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use buzz_sdk::testing::MemoryTransport;
use buzz_sdk::{
    kind, tool, Agent, AgentBuilder, Completion, Event, FixedClock, Identity, MockBackend,
    StopReason, ToolCall, Transport, Turn, Workspace,
};
use serde::Deserialize;

/// The agent's key.
const AGENT_SK: [u8; 32] = [
    0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
    0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xee,
];
/// A human member's key.
const HUMAN_SK: [u8; 32] = [
    0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10,
    0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x11,
];

const CHANNEL: &str = "eng";
const NOW_MS: u64 = 1_700_000_000_000;

fn identity() -> Identity {
    Identity::from_secret(AGENT_SK).unwrap()
}

/// A message from the human that mentions the agent.
fn mention(text: &str, at: u64) -> Event {
    Event::sign_new(
        &HUMAN_SK,
        kind::KIND_STREAM_MESSAGE,
        vec![
            vec!["h".to_string(), CHANNEL.to_string()],
            vec!["p".to_string(), identity().pubkey_hex()],
        ],
        text,
        at,
    )
    .unwrap()
}

/// A message from the human that does not mention the agent.
fn chatter(text: &str, at: u64) -> Event {
    Event::sign_new(
        &HUMAN_SK,
        kind::KIND_STREAM_MESSAGE,
        vec![vec!["h".to_string(), CHANNEL.to_string()]],
        text,
        at,
    )
    .unwrap()
}

async fn workspace(transport: Arc<MemoryTransport>) -> Workspace {
    Workspace::builder("")
        .identity(identity())
        .transport(transport as Arc<dyn Transport>)
        .clock(Arc::new(FixedClock::new(NOW_MS)))
        .default_channel(CHANNEL)
        .connect()
        .await
        .unwrap()
}

async fn agent(
    transport: Arc<MemoryTransport>,
    backend: Arc<MockBackend>,
    configure: impl FnOnce(AgentBuilder) -> AgentBuilder,
) -> Agent {
    let workspace = workspace(transport).await;
    configure(
        Agent::builder(workspace)
            .name("reviewer")
            .instructions("You review patches.")
            .model("claude-sonnet-5")
            .backend(backend),
    )
    .build()
    .unwrap()
}

/// Poll a condition to a deadline, so no test depends on a fixed sleep.
async fn until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

#[derive(Deserialize)]
struct ReviewArgs {
    target: String,
}

// ───────────────────────────────────────────────── tools, end to end

/// The headline path: a registered typed tool is offered to the model, the model
/// asks for it, the SDK runs it, feeds the result back, and the answer is posted
/// in-thread.
#[tokio::test]
async fn a_registered_tool_is_offered_run_and_fed_back() {
    let transport = Arc::new(MemoryTransport::new());
    let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let seen = Arc::clone(&calls);
    let review = tool(
        "review_patch",
        "Review a patch or branch",
        move |args: ReviewArgs| {
            let seen = Arc::clone(&seen);
            async move {
                seen.lock().unwrap().push(args.target.clone());
                Ok(format!("reviewed {}: no blocking issues", args.target))
            }
        },
    );

    // The model asks for the tool once, then answers.
    let backend = Arc::new(MockBackend::scripted(
        vec![Completion::calling(vec![ToolCall {
            id: "toolu_1".into(),
            name: "review_patch".into(),
            arguments: serde_json::json!({ "target": "main" }),
        }])
        .with_text("let me look")],
        "reviewed main, nothing blocking",
    ));

    let agent = agent(Arc::clone(&transport), Arc::clone(&backend), |b| {
        b.tool(review)
    })
    .await;

    let trigger = mention("@reviewer please look at main", 1_700_000_000);
    agent
        .run_turn(agent.turn_for(vec![trigger.clone()]))
        .await
        .unwrap();

    // The tool ran, with the arguments the model supplied, exactly once.
    assert_eq!(calls.lock().unwrap().as_slice(), ["main"]);

    // The model was offered the tool, with its description.
    let first = &backend.requests()[0];
    assert_eq!(first.tools.len(), 1);
    assert_eq!(first.tools[0].name, "review_patch");
    assert_eq!(first.tools[0].description, "Review a patch or branch");
    assert_eq!(first.model, "claude-sonnet-5");
    assert!(first.system.contains("You review patches."));

    // The second call carried the assistant's tool_use turn and the result.
    let second = &backend.requests()[1];
    assert_eq!(backend.calls(), 2);
    let echoed = &second.messages[second.messages.len() - 2];
    assert_eq!(echoed.tool_calls[0].id, "toolu_1");
    let results = second.messages.last().unwrap();
    assert_eq!(results.tool_results[0].id, "toolu_1");
    assert!(results.tool_results[0]
        .content
        .contains("no blocking issues"));
    assert!(!results.tool_results[0].is_error);

    // And the answer was posted as a threaded channel message.
    let replies = transport.published_kind(kind::KIND_STREAM_MESSAGE);
    assert_eq!(replies.len(), 1);
    let reply = &replies[0];
    reply.verify().unwrap();
    assert_eq!(reply.content, "reviewed main, nothing blocking");
    assert_eq!(reply.channel_id(), Some(CHANNEL));
    assert_eq!(
        reply.first_tag("e"),
        Some(buzz::relay::crypto::to_hex(&trigger.id).as_str()),
        "the reply is threaded onto the message that triggered it"
    );
    assert_eq!(
        reply.first_tag("p"),
        Some(buzz::relay::crypto::to_hex(&trigger.pubkey).as_str()),
        "the author is notified"
    );
    assert_eq!(reply.pubkey, identity().pubkey(), "signed by the agent");
}

#[tokio::test]
async fn a_failing_tool_is_reported_to_the_model_and_the_turn_still_answers() {
    let transport = Arc::new(MemoryTransport::new());
    let backend = Arc::new(MockBackend::scripted(
        vec![Completion::calling(vec![ToolCall {
            id: "toolu_1".into(),
            name: "review_patch".into(),
            // Missing `target`: deserialization into ReviewArgs fails.
            arguments: serde_json::json!({}),
        }])],
        "I could not read that patch",
    ));
    let agent = agent(Arc::clone(&transport), Arc::clone(&backend), |b| {
        b.tool(tool(
            "review_patch",
            "Review a patch",
            |args: ReviewArgs| async move { Ok(args.target) },
        ))
    })
    .await;

    agent
        .run_turn(agent.turn_for(vec![mention("@reviewer look", 1_700_000_000)]))
        .await
        .unwrap();

    let requests = backend.requests();
    let results = requests[1].messages.last().unwrap();
    assert!(results.tool_results[0].is_error, "flagged for the model");
    assert!(results.tool_results[0].content.contains("target"));
    assert_eq!(
        transport.published_kind(kind::KIND_STREAM_MESSAGE).len(),
        1,
        "a failed tool must not swallow the turn"
    );
}

#[tokio::test]
async fn a_hallucinated_tool_name_comes_back_as_an_error_not_a_panic() {
    let transport = Arc::new(MemoryTransport::new());
    let backend = Arc::new(MockBackend::scripted(
        vec![Completion::calling(vec![ToolCall {
            id: "toolu_1".into(),
            name: "deploy_to_production".into(),
            arguments: serde_json::json!({}),
        }])],
        "I do not have that tool",
    ));
    let agent = agent(Arc::clone(&transport), Arc::clone(&backend), |b| {
        b.tool(tool(
            "review_patch",
            "Review a patch",
            |args: ReviewArgs| async move { Ok(args.target) },
        ))
    })
    .await;

    agent
        .run_turn(agent.turn_for(vec![mention("@reviewer deploy", 1_700_000_000)]))
        .await
        .unwrap();

    let requests = backend.requests();
    let results = requests[1].messages.last().unwrap();
    assert!(results.tool_results[0].is_error);
    assert!(
        results.tool_results[0].content.contains("review_patch"),
        "the model is told what it may actually call: {}",
        results.tool_results[0].content
    );
}

/// A model that keeps reaching for tools is cut off at `max_steps`, and the turn
/// still ends with something posted: one final call with the tools withdrawn.
#[tokio::test]
async fn a_runaway_tool_loop_is_bounded_and_still_answers() {
    let transport = Arc::new(MemoryTransport::new());
    // Three tool-calling turns — exactly the step budget — then the fallback
    // answers, which is what the forced finish receives.
    let asking = Completion::calling(vec![ToolCall {
        id: "toolu_x".into(),
        name: "review_patch".into(),
        arguments: serde_json::json!({ "target": "main" }),
    }]);
    let backend = Arc::new(MockBackend::scripted(
        vec![asking.clone(), asking.clone(), asking],
        "I kept looking and ran out of budget",
    ));

    let agent = agent(Arc::clone(&transport), Arc::clone(&backend), |b| {
        b.max_steps(3).tool(tool(
            "review_patch",
            "Review a patch",
            |args: ReviewArgs| async move { Ok(args.target) },
        ))
    })
    .await;

    agent
        .run_turn(agent.turn_for(vec![mention("@reviewer look", 1_700_000_000)]))
        .await
        .unwrap();

    assert_eq!(backend.calls(), 4, "3 steps plus one forced finish");
    let final_call = backend.last_request().unwrap();
    assert!(
        final_call.tools.is_empty(),
        "the last call withdraws the tools so the turn must end"
    );
    assert!(final_call.system.contains("tool budget"));
    let replies = transport.published_kind(kind::KIND_STREAM_MESSAGE);
    assert_eq!(replies.len(), 1);
    assert!(replies[0].content.contains("ran out of budget"));
}

/// The pathological case: a model that will not answer even with no tools on the
/// table. The turn must fail loudly and post nothing, rather than post silence.
#[tokio::test]
async fn a_model_that_never_answers_fails_the_turn_without_posting() {
    let transport = Arc::new(MemoryTransport::new());
    let asking = Completion::calling(vec![ToolCall {
        id: "toolu_x".into(),
        name: "review_patch".into(),
        arguments: serde_json::json!({ "target": "main" }),
    }]);
    // Every call, including the forced finish, asks for tools again.
    let backend = Arc::new(MockBackend::scripted(
        vec![asking.clone(), asking.clone(), asking.clone(), asking],
        "",
    ));
    let agent = agent(Arc::clone(&transport), Arc::clone(&backend), |b| {
        b.max_steps(3).tool(tool(
            "review_patch",
            "Review a patch",
            |args: ReviewArgs| async move { Ok(args.target) },
        ))
    })
    .await;

    let err = agent
        .run_turn(agent.turn_for(vec![mention("@reviewer look", 1_700_000_000)]))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("did not converge"), "{err}");
    assert!(transport
        .published_kind(kind::KIND_STREAM_MESSAGE)
        .is_empty());
}

// ─────────────────────────────────────────────── prompt assembly

#[tokio::test]
async fn a_turn_reads_channel_history_but_not_its_own_trigger_twice() {
    let transport = Arc::new(MemoryTransport::new());
    transport.seed(chatter("we should review the migration", 1_699_999_000));
    transport.seed(chatter("agreed", 1_699_999_100));
    // Noise the context must not spend budget on.
    transport.seed(
        Event::sign_new(
            &HUMAN_SK,
            kind::KIND_REACTION,
            vec![vec!["h".to_string(), CHANNEL.to_string()]],
            "+",
            1_699_999_200,
        )
        .unwrap(),
    );

    let backend = Arc::new(MockBackend::answering("looking now"));
    let agent = agent(Arc::clone(&transport), Arc::clone(&backend), |b| b).await;

    let trigger = mention("@reviewer thoughts?", 1_700_000_000);
    transport.seed(trigger.clone());
    agent
        .run_turn(agent.turn_for(vec![trigger.clone()]))
        .await
        .unwrap();

    let request = backend.last_request().unwrap();
    let rendered: Vec<String> = request.messages.iter().map(|m| m.text.clone()).collect();
    let joined = rendered.join("\n");
    assert!(joined.contains("we should review the migration"));
    assert!(joined.contains("agreed"));
    assert!(!joined.contains('+'), "reactions are not conversation");

    // The trigger appears exactly once — in the final user message.
    let trigger_hex = buzz::relay::crypto::to_hex(&trigger.id);
    let short = &trigger_hex[..8];
    assert_eq!(
        joined.matches(short).count(),
        1,
        "the triggering message must not be shown twice: {joined}"
    );
    assert!(
        request.messages.last().unwrap().text.contains("thoughts?"),
        "the ask is the final message"
    );
    // Event ids are quoted so the agent can cite them, as its instructions ask.
    // History is rendered as labelled <message> blocks rather than a
    // `[id] author:` prefix — models imitated the prefix and put fabricated
    // event ids into their replies. The id is still present, as an attribute.
    assert!(
        joined.contains(&format!("event=\"{short}\"")),
        "the rendered history does not carry the event id: {joined}"
    );
}

#[tokio::test]
async fn a_batched_turn_shows_the_model_every_mention_and_answers_once() {
    let transport = Arc::new(MemoryTransport::new());
    let backend = Arc::new(MockBackend::answering("answering both"));
    let agent = agent(Arc::clone(&transport), Arc::clone(&backend), |b| b).await;

    let first = mention("@reviewer look at main", 1_700_000_000);
    let second = mention("@reviewer and the migration too", 1_700_000_001);
    agent
        .run_turn(agent.turn_for(vec![first, second]))
        .await
        .unwrap();

    let ask = backend
        .last_request()
        .unwrap()
        .messages
        .last()
        .unwrap()
        .text
        .clone();
    assert!(ask.contains("look at main"));
    assert!(ask.contains("and the migration too"));
    assert!(
        ask.contains("mentioned 2 times"),
        "the model is told why: {ask}"
    );
    assert_eq!(
        transport.published_kind(kind::KIND_STREAM_MESSAGE).len(),
        1,
        "one answer for the batch, not one per mention"
    );
}

#[tokio::test]
async fn a_refusal_posts_nothing() {
    let transport = Arc::new(MemoryTransport::new());
    let backend = Arc::new(MockBackend::scripted(
        vec![Completion {
            text: String::new(),
            tool_calls: Vec::new(),
            stop: StopReason::Refusal,
            usage: Default::default(),
        }],
        "unreachable",
    ));
    let agent = agent(Arc::clone(&transport), Arc::clone(&backend), |b| b).await;

    let err = agent
        .run_turn(agent.turn_for(vec![mention("@reviewer do something bad", 1_700_000_000)]))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("declined"), "{err}");
    assert!(
        transport
            .published_kind(kind::KIND_STREAM_MESSAGE)
            .is_empty(),
        "a refusal must not become a channel message"
    );
}

// ─────────────────────────────────────────────── memory and search

#[tokio::test]
async fn memory_is_written_recalled_and_reaches_the_prompt() {
    let transport = Arc::new(MemoryTransport::new());
    let backend = Arc::new(MockBackend::answering("noted"));
    let agent = agent(Arc::clone(&transport), Arc::clone(&backend), |b| {
        b.with_memory()
    })
    .await;

    let turn = agent.turn_for(vec![mention(
        "@reviewer remember the migration rule",
        1_700_000_000,
    )]);
    let note = turn
        .remember("migrations always need a rollback plan")
        .await
        .unwrap()
        .expect("memory is on");
    assert_eq!(note.text, "migrations always need a rollback plan");

    // Stored as an engram addressed to the agent itself.
    let engrams = transport.published_kind(buzz_sdk::memory::MEMORY_KIND);
    assert_eq!(engrams.len(), 1);
    assert_eq!(
        engrams[0].first_tag("p"),
        Some(identity().pubkey_hex().as_str())
    );

    // And recalled into the next turn's system prompt — the ask says "migration",
    // the note says "migrations", and the shared relevance heuristic bridges that.
    turn.think().await.unwrap();
    let system = backend.last_request().unwrap().system;
    assert!(
        system.contains("rollback plan"),
        "memory belongs in the system prompt: {system}"
    );
    assert!(system.contains("What you remember"));
}

#[tokio::test]
async fn search_finds_precedent_in_the_channel() {
    let transport = Arc::new(MemoryTransport::new());
    transport.seed(chatter(
        "the migration review found a missing index",
        1_699_000_000,
    ));
    transport.seed(chatter("lunch?", 1_699_000_100));
    let backend = Arc::new(MockBackend::answering("ok"));
    let agent = agent(Arc::clone(&transport), Arc::clone(&backend), |b| b).await;

    let turn = agent.turn_for(vec![mention("@reviewer any precedent?", 1_700_000_000)]);
    let hits = turn.search("migration review").limit(5).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].content.contains("missing index"));

    assert!(
        turn.search("kubernetes").limit(5).await.unwrap().is_empty(),
        "no overlap means no hits"
    );
}

// ─────────────────────────────────────────────── custom handlers

/// The reviewer example's shape: typing, search, remember, think, react, reply.
#[tokio::test]
async fn a_custom_handler_owns_the_turn() {
    let transport = Arc::new(MemoryTransport::new());
    let backend = Arc::new(MockBackend::answering(
        "Escalating to a human owner: this touches migrations.",
    ));
    let agent = agent(Arc::clone(&transport), Arc::clone(&backend), |b| {
        b.with_memory()
            .respond_to_members()
            .on_mention(|turn: Turn| async move {
                turn.typing().await?;
                let precedent = turn.search("migration").limit(5).await?;
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
    })
    .await;

    agent
        .run_turn(agent.turn_for(vec![mention(
            "@reviewer check the migration",
            1_700_000_000,
        )]))
        .await
        .unwrap();

    // A typing indicator, ephemeral and channel-scoped.
    let typing = transport.published_kind(kind::KIND_TYPING_INDICATOR);
    assert_eq!(typing.len(), 1);
    assert_eq!(typing[0].channel_id(), Some(CHANNEL));

    // A reaction, which carries no channel of its own (the relay resolves it from
    // the event it targets).
    let reactions = transport.published_kind(kind::KIND_REACTION);
    assert_eq!(reactions.len(), 1);
    assert_eq!(reactions[0].content, "🚨");
    assert_eq!(reactions[0].channel_id(), None);
    assert!(reactions[0].first_tag("e").is_some());

    // The escalation went to the thread.
    let replies = transport.published_kind(kind::KIND_STREAM_MESSAGE);
    assert_eq!(replies.len(), 1);
    assert!(replies[0].content.contains("Escalating"));
    let markers: Vec<&str> = replies[0]
        .tags
        .iter()
        .filter(|t| t.first().map(String::as_str) == Some("e"))
        .filter_map(|t| t.get(3).map(String::as_str))
        .collect();
    assert_eq!(
        markers,
        ["root"],
        "a thread-level reply marks only the root"
    );
}

#[tokio::test]
async fn a_handler_that_fails_does_not_take_the_agent_down() {
    let transport = Arc::new(MemoryTransport::new());
    let backend = Arc::new(MockBackend::answering("ok"));
    let attempts = Arc::new(AtomicUsize::new(0));
    let agent = agent(Arc::clone(&transport), Arc::clone(&backend), |b| {
        b.on_mention(move |_turn| {
            let attempts = Arc::clone(&attempts);
            async move {
                if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(buzz_sdk::SdkError::model("provider exploded"))
                } else {
                    Ok(())
                }
            }
        })
    })
    .await;

    let err = agent
        .run_turn(agent.turn_for(vec![mention("@reviewer hi", 1_700_000_000)]))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("provider exploded"));
    agent
        .run_turn(agent.turn_for(vec![mention("@reviewer hi again", 1_700_000_001)]))
        .await
        .expect("a failed turn must not poison the next one");
}

// ─────────────────────────────────────────────── the live run loop

/// The whole loop: subscribe, receive a frame, admit it, run the turn, reply — and
/// while that turn is running, batch the two mentions that arrive behind it into a
/// single second turn.
#[tokio::test]
async fn the_run_loop_answers_and_batches_what_arrives_while_busy() {
    let transport = Arc::new(MemoryTransport::new());
    let backend = Arc::new(MockBackend::answering("on it"));

    // Records the size of every batch a turn was given.
    let batches: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
    // Zero permits: the first handler blocks until the test releases it.
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let entered = Arc::new(AtomicUsize::new(0));

    let handler_batches = Arc::clone(&batches);
    let handler_gate = Arc::clone(&gate);
    let handler_entered = Arc::clone(&entered);

    let agent = agent(Arc::clone(&transport), Arc::clone(&backend), move |b| {
        b.on_mention(move |turn: Turn| {
            let batches = Arc::clone(&handler_batches);
            let gate = Arc::clone(&handler_gate);
            let entered = Arc::clone(&handler_entered);
            async move {
                batches.lock().unwrap().push(turn.mentions().len());
                entered.fetch_add(1, Ordering::SeqCst);
                // Hold the turn open until the test says otherwise.
                let permit = gate.acquire().await;
                drop(permit);
                let answer = turn.think().await?;
                turn.reply(&answer).await?;
                Ok(())
            }
        })
    })
    .await;

    let loop_handle = tokio::spawn(agent.run());

    // The loop subscribes before anything else.
    until("the agent to subscribe", || transport.subscriptions() == 1).await;

    // One mention: the first turn starts and blocks in the handler.
    transport.deliver(mention("@reviewer first", 1_700_000_000));
    until("the first turn to start", || {
        entered.load(Ordering::SeqCst) == 1
    })
    .await;

    // Two more arrive while it is busy. Waiting for the frames to be consumed is
    // what makes this deterministic — no sleep, no guessing.
    transport.deliver(mention("@reviewer second", 1_700_000_001));
    transport.deliver(mention("@reviewer third", 1_700_000_002));
    until("the loop to consume both frames", || {
        transport.undelivered() == 0
    })
    .await;
    assert_eq!(
        entered.load(Ordering::SeqCst),
        1,
        "single-flight: no second turn while the first is running"
    );

    // Release both turns.
    gate.add_permits(2);
    until("the batched turn to run", || {
        batches.lock().unwrap().len() == 2
    })
    .await;
    until("both replies to be posted", || {
        transport.published_kind(kind::KIND_STREAM_MESSAGE).len() == 2
    })
    .await;

    assert_eq!(
        batches.lock().unwrap().as_slice(),
        [1, 2],
        "the two queued mentions were answered together"
    );

    loop_handle.abort();
}

/// Noise must not wake the agent, and it must never answer itself.
#[tokio::test]
async fn the_run_loop_ignores_noise_and_its_own_replies() {
    let transport = Arc::new(MemoryTransport::new());
    let backend = Arc::new(MockBackend::answering("@reviewer talking to myself"));
    let turns = Arc::new(AtomicUsize::new(0));

    let counted = Arc::clone(&turns);
    let agent = agent(Arc::clone(&transport), Arc::clone(&backend), move |b| {
        b.on_mention(move |turn: Turn| {
            let counted = Arc::clone(&counted);
            async move {
                counted.fetch_add(1, Ordering::SeqCst);
                let answer = turn.think().await?;
                // The reply mentions the agent's own name; if the loop admitted
                // its own events this would run forever.
                turn.reply_to_thread(&answer).await?;
                Ok(())
            }
        })
    })
    .await;

    let loop_handle = tokio::spawn(agent.run());
    until("the agent to subscribe", || transport.subscriptions() == 1).await;

    // Not a mention.
    transport.deliver(chatter("just chatting", 1_700_000_000));
    // A mention of somebody else.
    transport.deliver(
        Event::sign_new(
            &HUMAN_SK,
            kind::KIND_STREAM_MESSAGE,
            vec![
                vec!["h".to_string(), CHANNEL.to_string()],
                vec!["p".to_string(), buzz::relay::crypto::to_hex(&[7u8; 32])],
            ],
            "@someone-else hi",
            1_700_000_001,
        )
        .unwrap(),
    );
    // A real mention.
    let real = mention("@reviewer hello", 1_700_000_002);
    transport.deliver(real.clone());
    // The same mention again, as an at-least-once redelivery.
    transport.deliver(real);

    until("the one real mention to be answered", || {
        transport.published_kind(kind::KIND_STREAM_MESSAGE).len() == 1
    })
    .await;
    until("the loop to drain", || transport.undelivered() == 0).await;

    assert_eq!(
        turns.load(Ordering::SeqCst),
        1,
        "one turn: noise, other people's mentions, redeliveries and the agent's own \
         reply all ignored"
    );
    loop_handle.abort();
}

/// A cold start must not replay a community's history into the model.
#[tokio::test]
async fn a_cold_start_ignores_mentions_older_than_the_backfill_window() {
    let transport = Arc::new(MemoryTransport::new());
    let backend = Arc::new(MockBackend::answering("ok"));
    let agent = agent(Arc::clone(&transport), Arc::clone(&backend), |b| {
        b.max_backfill(Duration::from_secs(60))
    })
    .await;

    let loop_handle = tokio::spawn(agent.run());
    until("the agent to subscribe", || transport.subscriptions() == 1).await;

    // The workspace clock is fixed at NOW_MS, so this is an hour stale.
    transport.deliver(mention("@reviewer ancient history", NOW_MS / 1_000 - 3_600));
    // ...and this one is inside the window.
    transport.deliver(mention("@reviewer recent", NOW_MS / 1_000 - 10));

    until("the recent mention to be answered", || {
        transport.published_kind(kind::KIND_STREAM_MESSAGE).len() == 1
    })
    .await;
    until("the loop to drain", || transport.undelivered() == 0).await;

    let asked = backend.last_request().unwrap();
    let ask = asked.messages.last().unwrap().text.clone();
    assert!(ask.contains("recent"), "{ask}");
    assert!(!ask.contains("ancient"), "{ask}");
    loop_handle.abort();
}

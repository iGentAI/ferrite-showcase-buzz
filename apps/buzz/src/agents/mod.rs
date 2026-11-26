//! The persona agent runtime: the event-sourced session engine, compaction,
//! prompt assembly, tool loop, file workspace, local memory, agent
//! definitions/skills, and subagent delegation, composed in-process:
//!
//! * the model boundary is a direct call into [`crate::llm`];
//! * the workspace boundary is a direct call into [`crate::relay::persona`];
//! * the trigger boundary keeps its queue: [`turns`] consumes the app's own
//!   `agent-turns` binding, because a mention must survive a crash, and the
//!   session admission (msg id = trigger event id) absorbs redeliveries into
//!   exactly one turn.
//!
//! Memory is the Store-backed local engine on the `agent-memory` binding;
//! `run_code` and pack-defined tools answer with an honest
//! "not available on this deployment".

pub mod agent_loop;
pub mod agent_prompt;
pub mod apply_patch;
pub mod approvals;
pub mod bundle;
pub mod compaction;
pub mod contract_generated;
pub mod conv;
pub mod definition;
pub mod error;
pub mod fragments;
pub mod local_memory;
pub mod model_client;
pub mod session;
pub mod tools;
pub mod turns;
pub mod workspace;

//! Complete Buzz integration suite, linked once for hosted workspace capacity.
//! Each module preserves its existing tests, private names and fixture ownership.

mod common;

mod manifest_forward_headers;

mod relay_fix_channel_ids;

mod relay_fix_operator_plane;

mod severe_access_gate;

mod severe_acp_gate;

mod severe_agent_approval_gate;

mod severe_agent_gate;

mod severe_agent_harness_gate;

mod severe_agent_pause_gate;

mod severe_archive_gate;

mod severe_audit_chain_gate;

mod severe_auth_gate;

mod severe_auth_idempotency_gate;

mod severe_boot_reconcile_gate;

mod severe_bounds_gate;

mod severe_channel_edit_gate;

mod severe_cloud_agent_gate;

mod severe_community_cleanup_gate;

mod severe_deletion_gate;

mod severe_discovery_gate;

mod severe_dm_gate;

mod severe_dm_projection_gate;

mod severe_eviction_gate;

mod severe_feed_gate;

mod severe_feed_retirement_gate;

mod severe_guest_gate;

mod severe_inert_verbs_gate;

mod severe_invite_gate;

mod severe_join_role_gate;

mod severe_lapsed_ban_gate;

mod severe_mcp_gate;

mod severe_media_gate;

mod severe_membership_gate;

mod severe_membership_notice_gate;

mod severe_moderation_expiry_gate;

mod severe_moderation_gate;

mod severe_moderation_refusal_gate;

mod severe_nip29_create_gate;

mod severe_nip29_metadata_gate;

mod severe_operator_metrics_gate;

mod severe_persona_contract_gate;

mod severe_presence_rate_gate;

mod severe_private_removal_notice_gate;

mod severe_process_contract_gate;

mod severe_projection_read_cost_gate;

mod severe_protocol_gate;

mod severe_reaction_gate;

mod severe_replacement_gate;

mod severe_reply_header_gate;

mod severe_roster_gate;

mod severe_roster_scale_gate;

mod severe_saga_gate;

mod severe_schedule_gate;

mod severe_schedule_seeding_gate;

mod severe_search_gate;

mod severe_subscription_cap_gate;

mod severe_thread_gate;

mod severe_timeline_projection_gate;

mod severe_ui_upload_scale_gate;

mod severe_upstream_wire_gate;

mod severe_window_gate;

mod severe_workflow_gate;

mod severe_ws_concurrency_gate;

mod severe_ws_gate;

mod topic_retention_gate;

#[test]
#[should_panic(expected = "fixture scenario completes without a panic")]
fn async_fixture_worker_propagates_a_scenario_panic() {
    common::run_scenario(|| async move { panic!("fixture sentinel") });
}

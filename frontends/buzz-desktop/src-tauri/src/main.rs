//! The buzz desktop shell.
//!
//! Hosts the SAME `buzz-web` bundle a browser tab runs, and answers only the
//! command families that bundle's browser seam cannot. Everything interesting is
//! in `buzz-desktop-core`; this file is the composition.
//!
//! Two things happen at startup and both matter:
//!
//! 1. The window is created with the Contract C marker as an INITIALIZATION
//!    SCRIPT, so `window.__BUZZ_DESKTOP__` exists before any bundle code runs.
//!    The client's `isTauri()` is consulted during first render — an async
//!    handshake would race it, which is why the marker is injected rather than
//!    requested.
//! 2. The permission-expiry thread starts, so "nobody answered" becomes a
//!    refusal even when nothing else is happening in the app.

#![forbid(unsafe_code)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod keychain;
mod remote;
mod state;
mod supervisor;
mod workspace;

use buzz_desktop_core::registry;
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        // The bridge already sends `plugin:opener|open_url`; registering it (see
        // `registry`) is what makes an external link open in the user's own
        // browser instead of doing nothing — Tauri intercepts `window.open`, so
        // the seam's fallback has nowhere to go inside a webview.
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let handle = app.handle().clone();
            // The two directories the shell needs: where checkouts live, and
            // where its own files do. Resolved through Tauri, with a fallback
            // that keeps a machine without a resolvable home usable.
            let home = app
                .path()
                .home_dir()
                .unwrap_or_else(|_| std::env::temp_dir().join("buzz-home"));
            let data_dir = app
                .path()
                .app_data_dir()
                .unwrap_or_else(|_| home.join(".buzz"));
            let state = state::AppState::open(&home, &data_dir)
                .map_err(|e| format!("the buzz desktop shell could not start: {e}"))?;
            app.manage(state);

            WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("buzz")
                .inner_size(1_280.0, 860.0)
                .min_inner_size(880.0, 600.0)
                .initialization_script(registry::init_script())
                .build()?;

            supervisor::start_expiry_thread(handle);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::desktop_capabilities,
            commands::get_git_identity,
            commands::discover_git_bash_prerequisite,
            commands::list_project_local_repositories,
            commands::clone_project_repository,
            commands::get_project_repo_snapshot,
            commands::get_project_repo_diff,
            commands::get_project_local_repo_snapshot,
            commands::get_project_local_repo_diff,
            commands::get_project_repo_sync_status,
            commands::push_project_local_repository,
            commands::pull_project_local_repository,
            commands::create_project_remote_branch,
            commands::delete_project_remote_branch,
            commands::open_project_terminal,
            commands::open_project_merge_recovery_terminal,
            commands::merge_project_pull_request,
            commands::publish_project_pull_request_merged_status,
            commands::list_managed_agents,
            commands::create_managed_agent,
            commands::update_managed_agent,
            commands::delete_managed_agent,
            commands::save_custom_harness,
            commands::delete_custom_harness,
            commands::list_managed_agent_runtimes,
            commands::start_managed_agent,
            commands::start_managed_agent_runtime,
            commands::stop_managed_agent,
            commands::stop_managed_agent_runtime,
            commands::restart_managed_agent_runtime,
            commands::put_managed_agent_runtime_lifecycle,
            commands::get_managed_agent_log,
            commands::discover_managed_agent_prereqs,
            commands::discover_acp_auth_methods,
            commands::connect_acp_runtime,
            commands::get_runtime_file_config,
            commands::set_managed_agent_auto_restart,
            commands::set_managed_agent_start_on_app_launch,
            commands::send_managed_agent_channel_message,
            commands::has_managed_agent_channel_message_marker,
        ])
        .run(tauri::generate_context!())
        .expect("the buzz desktop shell failed to start");
}

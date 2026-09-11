use std::path::Path;
use std::process::Command;

use serde::Deserialize;
use serde_json::{json, Value};
use tauri::{Manager, State};

use crate::lifecycle::RuntimeMutationDomain;
use crate::runtime::capability_catalog::diagnostics_for_profile;
use crate::runtime::diagnostics::{
    build_status_response, proxy_status_last_error, science_diagnostics, status_lights,
    ScienceDiagnosticsInput, StatusProbeInput,
};
use crate::runtime::failure::{OneClickFailureKind, TypedOneClickFailure};
use crate::runtime::operation;
use crate::runtime::profile::profile_capabilities;
use crate::runtime::provider::{
    current_shim_mode_for_adapter, gateway_kind_for_adapter, resolve_launch_plan,
    status_upstream_endpoint,
};
use crate::runtime::science::{
    science_runtime_preflight as runtime_preflight,
    science_runtime_update_action as runtime_update_action,
    science_runtime_update_status as runtime_update_status, settings_change_needs_teardown,
    ScienceHostAdapter, SCIENCE_DOWNLOAD_URL,
};
use crate::runtime::settings::{
    managed_sandbox_ssh_stub_path, preflight_managed_sandbox_ssh_stub_cleanup,
    remove_managed_sandbox_ssh_stub_exact, system_ssh_config_path, validate_runtime_ports,
};
use crate::runtime::ssh_bridge::{
    preflight_science_ssh_bridge_cleanup, revoke_science_ssh_bridge_exact,
    science_ssh_bridge_asset_paths, system_ssh_hosts,
};
use crate::runtime::system::open_in_browser;
use crate::{
    config, lock, proc, run_blocking, run_blocking_typed, AppState, SharedAppState, SharedLifecycle,
};

mod actions;
pub(crate) mod config_mutation;
mod gateway;
mod lifecycle;
mod one_click;
mod status;

pub(crate) use gateway::FetchModelsReq;
#[cfg(test)]
pub(crate) use lifecycle::stop_sandbox_state;
pub(crate) use lifecycle::{execute_process_local_science_stop_with, UiSettings};
pub(crate) use one_click::one_click_login_cmd;

#[cfg(test)]
pub(crate) fn stop_all_inner_cmd_for_test<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
) -> Result<(), String> {
    lifecycle::stop_all_inner_cmd(app, state, lifecycle)
}

#[tauri::command]
pub(crate) async fn set_mode(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    mode: String,
) -> Result<Value, String> {
    if mode != "proxy" {
        return Err("此版本仅支持第三方 API 模式。".into());
    }
    lifecycle::set_mode_command(app, state, lifecycle, mode).await
}

/// 官方模式：干净地打开用户【真实】的 Claude Science（不碰/复制真实凭证，抹掉 ANTHROPIC_*）。
#[tauri::command]
pub(crate) fn open_official() -> Result<(), String> {
    actions::open_official_inner()
}

#[tauri::command]
pub(crate) async fn set_settings(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    cfg: UiSettings,
) -> Result<Value, String> {
    lifecycle::set_settings_command(app, state, lifecycle, cfg).await
}

#[tauri::command]
pub(crate) async fn fetch_models(
    app: tauri::AppHandle,
    lifecycle: State<'_, SharedLifecycle>,
    req: FetchModelsReq,
) -> Result<serde_json::Value, crate::commands::codex::RuntimeCommandError> {
    gateway::fetch_models_command(app, lifecycle, req).await
}

#[tauri::command]
pub(crate) async fn stop_all(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
) -> Result<(), String> {
    lifecycle::stop_all_command(app, state, lifecycle).await
}

#[tauri::command]
pub(crate) async fn one_click_login<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    runtime_choice: Option<String>,
) -> Result<serde_json::Value, crate::commands::codex::RuntimeCommandError> {
    one_click::one_click_login_command(app, state, lifecycle, runtime_choice).await
}

#[tauri::command]
pub(crate) fn finalize_consumer_state(outcome: Value) -> Result<Value, String> {
    crate::runtime::finalize_consumer::project_finalize_consumer_state(
        &config::default_dir(),
        &outcome,
    )
    .and_then(|state| serde_json::to_value(state).map_err(|error| error.to_string()))
}

#[tauri::command]
pub(crate) async fn restore_history_choice<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    reference: String,
    resume: Option<bool>,
) -> Result<serde_json::Value, crate::commands::codex::RuntimeCommandError> {
    if resume.unwrap_or(false) {
        crate::api_only::require_api_profile(&config::default_dir(), None)?;
    }
    one_click::restore_history_choice_command(app, state, lifecycle, reference, resume).await
}

#[tauri::command]
pub(crate) async fn science_runtime_preflight(
    state: State<'_, SharedAppState>,
) -> Result<Value, String> {
    status::science_runtime_preflight_command(state).await
}

#[tauri::command]
pub(crate) async fn science_runtime_update_status() -> Result<Value, String> {
    run_blocking(runtime_update_status).await
}

#[tauri::command]
pub(crate) async fn science_runtime_update_action(
    action: String,
    expected_sha256: String,
) -> Result<Value, String> {
    run_blocking(move || runtime_update_action(&action, &expected_sha256)).await
}

#[tauri::command]
pub(crate) fn open_science_download_page() -> Result<(), String> {
    actions::open_science_download_page_inner()
}

#[tauri::command]
pub(crate) fn status(state: State<'_, SharedAppState>) -> serde_json::Value {
    status::status_inner(state)
}

#[tauri::command]
pub(crate) fn boot_snapshot(state: State<'_, SharedAppState>) -> serde_json::Value {
    status::boot_snapshot_inner(state)
}

#[tauri::command]
pub(crate) async fn open_url(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
) -> Result<serde_json::Value, String> {
    actions::open_url_command(app, state, lifecycle).await
}

#[tauri::command]
pub(crate) async fn quit_app(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
) -> Result<(), String> {
    lifecycle::quit_app_command(app, state, lifecycle).await
}

#[cfg(test)]
use actions::{manual_open_result, open_url_inner};
#[cfg(test)]
use one_click::project_one_click_failure;
#[cfg(test)]
use status::{
    config_last_error_json, status_response_for_config_error, status_runtime_identity,
    status_upstream_applicable,
};

#[cfg(test)]
#[path = "runtime/tests.rs"]
pub(crate) mod tests;

// Keep the existing stop ownership checks and replacement under one lifecycle
// lease. request_restart preserves the native exit cleanup event path.
pub(crate) async fn install_verified_app_update(
    app: tauri::AppHandle,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
    update: tauri_plugin_updater::Update,
    bytes: Vec<u8>,
) -> Result<(), String> {
    let restart_app = app.clone();
    run_blocking(move || {
        let coordination_app = app.clone();
        let updates = coordination_app.state::<super::app_update::AppUpdateState>();
        let installation = updates.claim_installation()?;
        let result = lifecycle::stop_all_then_with(
            app,
            state,
            lifecycle,
            RuntimeMutationDomain::Terminal,
            ScienceHostAdapter::claim_stop,
            |app, request| ScienceHostAdapter::execute_stop(app, request).into_parts(),
            || {
                update
                    .install(&bytes)
                    .map_err(|_| "更新安装失败；请检查应用目录写入权限".to_string())?;
                Ok(())
            },
        );
        // Both the lifecycle lock and install gate must be released before
        // posting restart, so its native exit cleanup can run on the UI thread.
        drop(installation);
        result?;
        restart_app.request_restart();
        Ok(())
    })
    .await
}

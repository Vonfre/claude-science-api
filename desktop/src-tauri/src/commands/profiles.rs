use std::path::Path;

use tauri::State;

use crate::runtime::profile::{
    acknowledge_pending_notice_inner, build_get_config, clear_profile_key_inner,
    create_profile_with_catalog_inner, delete_profile_inner, persist_profile_candidate_inner,
    update_profile_metadata_inner, CatalogEdit, ConnectionEdit,
};
use crate::runtime::profile_switch::scratch_validate_candidate;
use crate::runtime::provider::{reject_openai_custom_anthropic_base, resolve_launch_plan};
use crate::{
    config, lifecycle, lock, run_blocking_typed, AppState, SharedAppState, SharedLifecycle,
};

fn catalog_edit_from_parts(
    legacy_model_present: bool,
    model_catalog: Option<Vec<crate::model_catalog::ModelRoute>>,
    default_model_route_id: Option<String>,
    role_bindings: Option<crate::model_catalog::RoleBindings>,
) -> Result<Option<CatalogEdit>, String> {
    let catalog_edit = match (model_catalog, default_model_route_id, role_bindings) {
        (None, None, None) => None,
        (Some(routes), Some(default_model_route_id), Some(role_bindings)) => Some(CatalogEdit {
            routes,
            default_model_route_id,
            role_bindings,
        }),
        _ => {
            return Err("model_catalog 必须与 default_model_route_id/role_bindings 一起提交".into())
        }
    };
    if legacy_model_present && catalog_edit.is_some() {
        return Err("legacy model 与完整 model_catalog 不能同时提交".into());
    }
    Ok(catalog_edit)
}

fn load_without_runtime_transaction(dir: &Path) -> Result<config::Config, String> {
    let cfg = config::load_from(dir).map_err(|error| error.to_string())?;
    config::require_no_runtime_transaction(&cfg)?;
    Ok(cfg)
}

#[tauri::command]
pub(crate) fn get_config() -> Result<serde_json::Value, String> {
    build_get_config(&config::default_dir()).map(crate::api_only::project_config)
}

#[tauri::command]
pub(crate) fn acknowledge_pending_notice(
    expected_notice_id: String,
) -> Result<serde_json::Value, String> {
    acknowledge_pending_notice_inner(&config::default_dir(), &expected_notice_id)
}

// ---------- profile CRUD 命令（薄包装 *_inner，统一经串行器） ----------
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_profile(
    lifecycle: State<'_, SharedLifecycle>,
    template_id: String,
    name: String,
    key: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    model_catalog: Option<Vec<crate::model_catalog::ModelRoute>>,
    default_model_route_id: Option<String>,
    role_bindings: Option<crate::model_catalog::RoleBindings>,
) -> Result<String, String> {
    crate::api_only::require_api_template(&template_id)?;
    let catalog_edit = catalog_edit_from_parts(
        model.is_some(),
        model_catalog,
        default_model_route_id,
        role_bindings,
    )?;
    lifecycle.with_mutation(lifecycle::RuntimeMutationDomain::Intent, |_| {
        create_profile_with_catalog_inner(
            &config::default_dir(),
            &template_id,
            &name,
            key.as_deref(),
            base_url.as_deref(),
            model.as_deref(),
            catalog_edit,
        )
    })
}

#[tauri::command]
pub(crate) fn update_profile_metadata(
    lifecycle: State<'_, SharedLifecycle>,
    id: String,
    name: String,
    notes: Option<String>,
) -> Result<(), String> {
    lifecycle.with_mutation(lifecycle::RuntimeMutationDomain::Intent, |_| {
        crate::api_only::require_api_mutation_target(&config::default_dir(), &id)?;
        update_profile_metadata_inner(&config::default_dir(), &id, &name, notes.as_deref())
    })
}

/// 清 key：经串行器；若清的是【生效】profile → bump_generation 作废在途启动 + 停运行中代理
/// （不再拿旧 key 服务，比照 spec §8.2 运行态撤销）。
#[tauri::command]
pub(crate) fn clear_profile_key(
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    id: String,
) -> Result<serde_json::Value, crate::commands::codex::RuntimeCommandError> {
    crate::api_only::require_api_mutation_target(&config::default_dir(), &id)?;
    clear_profile_key_p2b(
        &config::default_dir(),
        state.inner(),
        lifecycle.as_ref(),
        &id,
    )
    .map_err(crate::commands::codex::RuntimeCommandError::from)
}

/// 删 profile：经串行器；删的是【生效】profile → active 置空（inner 内）+ bump + 停代理。
#[tauri::command]
pub(crate) fn delete_profile(
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    id: String,
) -> Result<serde_json::Value, crate::commands::codex::RuntimeCommandError> {
    crate::api_only::require_api_mutation_target(&config::default_dir(), &id)?;
    delete_profile_p2b(
        &config::default_dir(),
        state.inner(),
        lifecycle.as_ref(),
        &id,
    )
    .map_err(crate::commands::codex::RuntimeCommandError::from)
}

fn profile_mutation_attention(
    operation: &mut crate::commands::runtime::config_mutation::OpenConfigMutation,
    effect_index: usize,
    cause: &str,
    detail: &str,
) -> Result<serde_json::Value, String> {
    let _ = operation.checkpoint_effect(
        effect_index,
        crate::commands::runtime::config_mutation::ConfigMutationEffectState::Uncertain,
        Some(cause),
    );
    match operation.finish(
        "attention",
        "before",
        "unknown",
        Some(cause),
        config::ConfigMutationTerminalConfigImage::Before,
    ) {
        Ok(_) => Err(detail.to_string()),
        Err(error) => Err(crate::commands::runtime::config_mutation::command_error_string(&error)),
    }
}

fn scrub_profile_rolling_backup(
    operation: &mut crate::commands::runtime::config_mutation::OpenConfigMutation,
    effect_index: usize,
    dir: &Path,
) -> Result<(), String> {
    operation
        .checkpoint_effect_or_attention(
            effect_index,
            crate::commands::runtime::config_mutation::ConfigMutationEffectState::InProgress,
            None,
            "rolling_backup_checkpoint_failed",
            "after",
            "stopped",
            config::ConfigMutationTerminalConfigImage::After,
        )
        .map_err(|error| crate::commands::runtime::config_mutation::command_error_string(&error))?;
    if let Err(error) = config::drop_rolling_backup(dir) {
        let _ = operation.checkpoint_effect(
            effect_index,
            crate::commands::runtime::config_mutation::ConfigMutationEffectState::Uncertain,
            Some("rolling_backup_delete_uncertain"),
        );
        return match operation.finish(
            "attention",
            "after",
            "stopped",
            Some("rolling_backup_delete_uncertain"),
            config::ConfigMutationTerminalConfigImage::After,
        ) {
            Ok(_) => Err(format!("滚动备份删除结果不确定：{error}")),
            Err(attention) => Err(
                crate::commands::runtime::config_mutation::command_error_string(
                    &attention.with_message("滚动备份删除结果不确定；已保留 durable attention"),
                ),
            ),
        };
    }
    operation
        .checkpoint_effect_or_attention(
            effect_index,
            crate::commands::runtime::config_mutation::ConfigMutationEffectState::Succeeded,
            Some("absent"),
            "rolling_backup_checkpoint_failed",
            "after",
            "stopped",
            config::ConfigMutationTerminalConfigImage::After,
        )
        .map_err(|error| crate::commands::runtime::config_mutation::command_error_string(&error))
}

fn clear_profile_key_p2b(
    dir: &Path,
    state: &SharedAppState,
    lifecycle: &lifecycle::Lifecycle,
    id: &str,
) -> Result<serde_json::Value, String> {
    lifecycle.with_mutation(lifecycle::RuntimeMutationDomain::Destructive, |_| {
        let before = load_without_runtime_transaction(dir)?;
        let applied = before
            .runtime_binding
            .as_ref()
            .is_some_and(|binding| binding.profile_id == id);
        let target_exists = before.profile_by_id(id).is_some();
        if !applied {
            clear_profile_key_inner(dir, id)?;
            return Ok(crate::commands::runtime::config_mutation::typed_intent_outcome(
                "clear_profile_key",
                if target_exists { "committed" } else { "no_change" },
                "committed",
                Some(id.to_string()),
                before.runtime_binding.as_ref().map(|binding| binding.profile_id.clone()),
                Some("not_run"),
                Some(false),
            ));
        }

        let mut after = before.clone();
        if let Some(profile) = after.profile_by_id_mut(id) {
            profile.api_key.clear();
        }
        after.runtime_binding = None;
        let mut operation = crate::commands::runtime::config_mutation::begin(
            dir,
            crate::commands::runtime::config_mutation::ConfigMutationOperation::ClearAppliedProfileKey,
            &before,
            Some(&after),
            crate::commands::runtime::config_mutation::MutationTarget {
                profile_id: Some(id.to_string()),
                ..Default::default()
            },
            crate::commands::runtime::config_mutation::RuntimePlan {
                owner_generation: lifecycle.current_generation(),
                ..Default::default()
            },
            vec![
                crate::commands::runtime::config_mutation::ConfigMutationEffectKind::StopGateway,
                crate::commands::runtime::config_mutation::ConfigMutationEffectKind::ConfigCommit,
                crate::commands::runtime::config_mutation::ConfigMutationEffectKind::DeleteRollingBackup,
            ],
            None,
            None,
        )
        .map_err(|error| crate::commands::runtime::config_mutation::command_error_string(&error))?;

        operation
            .checkpoint_effect_or_attention(
                0,
                crate::commands::runtime::config_mutation::ConfigMutationEffectState::InProgress,
                None,
                "gateway_stop_checkpoint_failed",
                "before",
                "unknown",
                config::ConfigMutationTerminalConfigImage::Before,
            )
            .map_err(|error| {
                crate::commands::runtime::config_mutation::command_error_string(&error)
            })?;
        lifecycle.bump_generation();
        if let Err(error) = lock(state).stop_proxy().require_stopped(
            "清除已应用 profile key 前无法安全停止 Gateway；配置未修改",
        ) {
            return profile_mutation_attention(
                &mut operation,
                0,
                "gateway_stop_uncertain",
                &error,
            );
        }
        operation
            .checkpoint_effect_or_attention(
                0,
                crate::commands::runtime::config_mutation::ConfigMutationEffectState::Succeeded,
                Some("stopped"),
                "gateway_stop_checkpoint_failed",
                "before",
                "stopped",
                config::ConfigMutationTerminalConfigImage::Before,
            )
            .map_err(|error| {
                crate::commands::runtime::config_mutation::command_error_string(&error)
            })?;
        let before_fingerprint = operation.fence().before_config_fingerprint.clone();
        operation
            .checkpoint_effect_or_attention(
                1,
                crate::commands::runtime::config_mutation::ConfigMutationEffectState::InProgress,
                None,
                "config_commit_checkpoint_failed",
                "before",
                "stopped",
                config::ConfigMutationTerminalConfigImage::Before,
            )
            .map_err(|error| {
                crate::commands::runtime::config_mutation::command_error_string(&error)
            })?;
        if let Err(error) = crate::runtime::profile::clear_profile_key_with_mutation(
            dir,
            operation.fence(),
            operation.receipt_bytes(),
            id,
            &before_fingerprint,
        ) {
            return profile_mutation_attention(
                &mut operation,
                1,
                "config_commit_failed",
                &error,
            );
        }
        operation
            .checkpoint_effect_or_attention(
                1,
                crate::commands::runtime::config_mutation::ConfigMutationEffectState::Succeeded,
                Some("committed"),
                "config_commit_checkpoint_failed",
                "after",
                "stopped",
                config::ConfigMutationTerminalConfigImage::After,
            )
            .map_err(|error| {
                crate::commands::runtime::config_mutation::command_error_string(&error)
            })?;
        scrub_profile_rolling_backup(&mut operation, 2, dir)?;
        let outcome = operation
            .finish(
                "completed",
                "after",
                "stopped",
                None,
                config::ConfigMutationTerminalConfigImage::After,
            )
            .map_err(|error| crate::commands::runtime::config_mutation::command_error_string(&error))?;
        Ok(crate::commands::runtime::config_mutation::outcome_json(&outcome))
    })
}

fn delete_profile_p2b(
    dir: &Path,
    state: &SharedAppState,
    lifecycle: &lifecycle::Lifecycle,
    id: &str,
) -> Result<serde_json::Value, String> {
    lifecycle.with_mutation(lifecycle::RuntimeMutationDomain::Destructive, |_| {
        let before = load_without_runtime_transaction(dir)?;
        let applied = before
            .runtime_binding
            .as_ref()
            .is_some_and(|binding| binding.profile_id == id);
        let target_exists = before.profile_by_id(id).is_some();
        if !applied {
            delete_profile_inner(dir, id)?;
            return Ok(crate::commands::runtime::config_mutation::typed_intent_outcome(
                "delete_profile",
                if target_exists { "committed" } else { "no_change" },
                "committed",
                Some(id.to_string()),
                before.runtime_binding.as_ref().map(|binding| binding.profile_id.clone()),
                Some("not_run"),
                Some(false),
            ));
        }

        let mut after = before.clone();
        after.profiles.retain(|profile| profile.id != id);
        if after.active_id == id {
            after.active_id.clear();
        }
        after.runtime_binding = None;
        let mut operation = crate::commands::runtime::config_mutation::begin(
            dir,
            crate::commands::runtime::config_mutation::ConfigMutationOperation::DeleteAppliedProfile,
            &before,
            Some(&after),
            crate::commands::runtime::config_mutation::MutationTarget {
                profile_id: Some(id.to_string()),
                ..Default::default()
            },
            crate::commands::runtime::config_mutation::RuntimePlan {
                owner_generation: lifecycle.current_generation(),
                ..Default::default()
            },
            vec![
                crate::commands::runtime::config_mutation::ConfigMutationEffectKind::StopGateway,
                crate::commands::runtime::config_mutation::ConfigMutationEffectKind::ConfigCommit,
                crate::commands::runtime::config_mutation::ConfigMutationEffectKind::DeleteRollingBackup,
            ],
            None,
            None,
        )
        .map_err(|error| crate::commands::runtime::config_mutation::command_error_string(&error))?;

        operation
            .checkpoint_effect_or_attention(
                0,
                crate::commands::runtime::config_mutation::ConfigMutationEffectState::InProgress,
                None,
                "gateway_stop_checkpoint_failed",
                "before",
                "unknown",
                config::ConfigMutationTerminalConfigImage::Before,
            )
            .map_err(|error| {
                crate::commands::runtime::config_mutation::command_error_string(&error)
            })?;
        lifecycle.bump_generation();
        if let Err(error) = lock(state).stop_proxy().require_stopped(
            "删除已应用 profile 前无法安全停止 Gateway；配置未修改",
        ) {
            return profile_mutation_attention(
                &mut operation,
                0,
                "gateway_stop_uncertain",
                &error,
            );
        }
        operation
            .checkpoint_effect_or_attention(
                0,
                crate::commands::runtime::config_mutation::ConfigMutationEffectState::Succeeded,
                Some("stopped"),
                "gateway_stop_checkpoint_failed",
                "before",
                "stopped",
                config::ConfigMutationTerminalConfigImage::Before,
            )
            .map_err(|error| {
                crate::commands::runtime::config_mutation::command_error_string(&error)
            })?;
        let before_fingerprint = operation.fence().before_config_fingerprint.clone();
        operation
            .checkpoint_effect_or_attention(
                1,
                crate::commands::runtime::config_mutation::ConfigMutationEffectState::InProgress,
                None,
                "config_commit_checkpoint_failed",
                "before",
                "stopped",
                config::ConfigMutationTerminalConfigImage::Before,
            )
            .map_err(|error| {
                crate::commands::runtime::config_mutation::command_error_string(&error)
            })?;
        if let Err(error) = crate::runtime::profile::delete_profile_with_mutation(
            dir,
            operation.fence(),
            operation.receipt_bytes(),
            id,
            &before_fingerprint,
        ) {
            return profile_mutation_attention(
                &mut operation,
                1,
                "config_commit_failed",
                &error,
            );
        }
        operation
            .checkpoint_effect_or_attention(
                1,
                crate::commands::runtime::config_mutation::ConfigMutationEffectState::Succeeded,
                Some("committed"),
                "config_commit_checkpoint_failed",
                "after",
                "stopped",
                config::ConfigMutationTerminalConfigImage::After,
            )
            .map_err(|error| {
                crate::commands::runtime::config_mutation::command_error_string(&error)
            })?;
        scrub_profile_rolling_backup(&mut operation, 2, dir)?;
        let outcome = operation
            .finish(
                "completed",
                "after",
                "stopped",
                None,
                config::ConfigMutationTerminalConfigImage::After,
            )
            .map_err(|error| crate::commands::runtime::config_mutation::command_error_string(&error))?;
        Ok(crate::commands::runtime::config_mutation::outcome_json(&outcome))
    })
}

fn clear_profile_key_cmd(
    dir: &Path,
    state: &SharedAppState,
    lifecycle: &lifecycle::Lifecycle,
    id: &str,
) -> Result<(), String> {
    clear_profile_key_cmd_with(dir, state, lifecycle, id, AppState::stop_proxy)
}

fn clear_profile_key_cmd_with<StopGateway>(
    dir: &Path,
    state: &SharedAppState,
    lifecycle: &lifecycle::Lifecycle,
    id: &str,
    stop_gateway: StopGateway,
) -> Result<(), String>
where
    StopGateway: FnOnce(&mut AppState) -> crate::GatewayStopOutcome,
{
    lifecycle.with_mutation(lifecycle::RuntimeMutationDomain::Destructive, |_| {
        let cfg = load_without_runtime_transaction(dir)?;
        let was_applied = cfg
            .runtime_binding
            .as_ref()
            .map(|binding| binding.profile_id.as_str())
            == Some(id);
        if was_applied {
            lifecycle.bump_generation();
            let mut st = lock(state);
            stop_gateway(&mut st)
                .require_stopped("清除已应用 profile key 前无法安全停止 Gateway；配置未修改")?;
        }
        clear_profile_key_inner(dir, id)?;
        Ok(())
    })
}

fn delete_profile_cmd(
    dir: &Path,
    state: &SharedAppState,
    lifecycle: &lifecycle::Lifecycle,
    id: &str,
) -> Result<(), String> {
    delete_profile_cmd_with(dir, state, lifecycle, id, AppState::stop_proxy)
}

fn delete_profile_cmd_with<StopGateway>(
    dir: &Path,
    state: &SharedAppState,
    lifecycle: &lifecycle::Lifecycle,
    id: &str,
    stop_gateway: StopGateway,
) -> Result<(), String>
where
    StopGateway: FnOnce(&mut AppState) -> crate::GatewayStopOutcome,
{
    lifecycle.with_mutation(lifecycle::RuntimeMutationDomain::Destructive, |_| {
        let cfg = load_without_runtime_transaction(dir)?;
        let was_applied = cfg
            .runtime_binding
            .as_ref()
            .map(|binding| binding.profile_id.as_str())
            == Some(id);
        if was_applied {
            lifecycle.bump_generation();
            let mut st = lock(state);
            stop_gateway(&mut st)
                .require_stopped("删除已应用 profile 前无法安全停止 Gateway；配置未修改")?;
        }
        delete_profile_inner(dir, id)?;
        Ok(())
    })
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn update_profile_connection(
    app: tauri::AppHandle,
    lifecycle: State<'_, SharedLifecycle>,
    id: String,
    base_url: Option<String>,
    api_format: Option<String>,
    model: Option<String>,
    key: Option<String>,
    model_catalog: Option<Vec<crate::model_catalog::ModelRoute>>,
    default_model_route_id: Option<String>,
    role_bindings: Option<crate::model_catalog::RoleBindings>,
) -> Result<serde_json::Value, crate::commands::codex::RuntimeCommandError> {
    crate::api_only::require_api_profile(&config::default_dir(), Some(&id))?;
    let lifecycle = lifecycle.inner().clone();
    run_blocking_typed(move || {
        update_profile_connection_inner_cmd(
            app,
            lifecycle,
            id,
            base_url,
            api_format,
            model,
            key,
            model_catalog,
            default_model_route_id,
            role_bindings,
        )
    })
    .await
}

#[allow(clippy::too_many_arguments)]
fn update_profile_connection_inner_cmd(
    app: tauri::AppHandle,
    lifecycle: SharedLifecycle,
    id: String,
    base_url: Option<String>,
    api_format: Option<String>,
    model: Option<String>,
    key: Option<String>,
    model_catalog: Option<Vec<crate::model_catalog::ModelRoute>>,
    default_model_route_id: Option<String>,
    role_bindings: Option<crate::model_catalog::RoleBindings>,
) -> Result<serde_json::Value, crate::commands::codex::RuntimeCommandError> {
    let dir = config::default_dir();
    let prepare_app = app.clone();
    let validate_app = app;
    let preflight_id = id.clone();
    update_profile_connection_with(
        lifecycle.as_ref(),
        &dir,
        id,
        base_url,
        api_format,
        model,
        key,
        model_catalog,
        default_model_route_id,
        role_bindings,
        move |_candidate, target_adapter| {
            let (preflight_adapter, preflight_target) = if target_adapter == "codex" {
                (
                    "codex",
                    crate::commands::codex::CodexPreflightTarget::Profile(preflight_id),
                )
            } else {
                (
                    target_adapter,
                    crate::commands::codex::CodexPreflightTarget::NoProfile,
                )
            };
            crate::commands::codex::prepare_provider_auth(
                &prepare_app,
                preflight_adapter,
                preflight_target,
            )
        },
        |prepared, _dir| {
            if let Some(prepared) = prepared.as_ref() {
                prepared.verify_unchanged()?;
            }
            Ok(())
        },
        move |candidate, prepared| {
            scratch_validate_candidate(
                &validate_app,
                candidate,
                prepared.as_ref().map(|prepared| prepared.proof()),
            )
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn update_profile_connection_with<P, Prepare, Verify, Validate>(
    lifecycle: &lifecycle::Lifecycle,
    dir: &Path,
    id: String,
    base_url: Option<String>,
    api_format: Option<String>,
    model: Option<String>,
    key: Option<String>,
    model_catalog: Option<Vec<crate::model_catalog::ModelRoute>>,
    default_model_route_id: Option<String>,
    role_bindings: Option<crate::model_catalog::RoleBindings>,
    prepare: Prepare,
    verify: Verify,
    validate: Validate,
) -> Result<serde_json::Value, crate::commands::codex::RuntimeCommandError>
where
    Prepare:
        FnOnce(&config::Profile, &str) -> Result<P, crate::commands::codex::RuntimeCommandError>,
    Verify: FnOnce(&P, &Path) -> Result<(), String>,
    Validate: FnOnce(&config::Profile, &P) -> Result<bool, String>,
{
    let catalog_edit = catalog_edit_from_parts(
        model.is_some(),
        model_catalog,
        default_model_route_id,
        role_bindings,
    )
    .map_err(crate::commands::codex::RuntimeCommandError::from)?;
    let preflight_cfg = load_without_runtime_transaction(dir)?;
    let mut preflight_candidate = preflight_cfg
        .profile_by_id(&id)
        .cloned()
        .ok_or_else(|| format!("找不到 profile：{id}"))?;
    let preflight_edit = ConnectionEdit::new(
        base_url.clone(),
        api_format.clone(),
        model.clone(),
        key.clone(),
    )
    .with_catalog(catalog_edit.clone());
    preflight_edit.apply(&mut preflight_candidate)?;
    let target_adapter = resolve_launch_plan(&preflight_candidate)?.adapter;
    let prepared = prepare(&preflight_candidate, &target_adapter)?;
    lifecycle
        .with_mutation(
            lifecycle::RuntimeMutationDomain::Intent,
            |_| -> Result<_, String> {
                verify(&prepared, dir)?;
                let edit = ConnectionEdit::new(
                    base_url.clone(),
                    api_format.clone(),
                    model.clone(),
                    key.clone(),
                )
                .with_catalog(catalog_edit.clone());
                commit_profile_connection_in_dir(dir, &id, edit, |candidate| {
                    validate(candidate, &prepared)
                })
            },
        )
        .map_err(crate::commands::codex::RuntimeCommandError::from)
}

fn commit_profile_connection_in_dir(
    dir: &Path,
    id: &str,
    edit: ConnectionEdit,
    validate: impl FnOnce(&config::Profile) -> Result<bool, String>,
) -> Result<serde_json::Value, String> {
    let cfg = load_without_runtime_transaction(dir)?;
    // 未命中 id → Err（不静默 Ok）。
    let mut candidate = cfg
        .profile_by_id(id)
        .cloned()
        .ok_or_else(|| format!("找不到 profile：{id}"))?;
    config::require_template_enabled(&cfg, &candidate.template_id)?;
    // 生效【后】的候选连接（None=不改则沿用旧值），active/非 active 共用一份。
    edit.apply(&mut candidate)?;
    let resolved = resolve_launch_plan(&candidate)?;
    reject_openai_custom_anthropic_base(&resolved.adapter, &candidate.base_url)?;
    // 保存前守卫（修 P2）：relay/自定义端点清空 base_url → 不可用连接（激活必失败）。
    // 校验生效后的 base_url，空则拒绝落盘、绝不谎报「已保存」；native 走硬编码端点，空无妨。
    if resolved.endpoint_policy == crate::provider_contracts::EndpointPolicy::ProfileRequired
        && candidate.base_url.trim().is_empty()
    {
        return Err("中转 / 自定义端点必须填写连接地址（base_url），连接未保存。".to_string());
    }
    // 保存前守卫（修 #9 P1-a）：relay/自定义端点空 model → 无 force → 退回 passthrough（显示 claude）。
    if resolved.model_policy == crate::provider_contracts::ModelPolicy::SavedCatalog
        && candidate.model.trim().is_empty()
    {
        return Err("中转 / 自定义端点必须选择或填写一个模型，连接未保存。".to_string());
    }
    // Saving a connection never applies it. Scratch validation remains
    // isolated and one-click is the only runtime apply/start boundary.
    let validated = validate(&candidate)?;
    persist_profile_candidate_inner(dir, id, &candidate)?;
    let mut result = crate::commands::runtime::config_mutation::typed_intent_outcome(
        "update_profile_connection",
        "committed",
        "committed",
        None,
        None,
        Some(if validated {
            "accepted"
        } else {
            "inconclusive"
        }),
        None,
    );
    if let Some(object) = result.as_object_mut() {
        object.insert("validated".into(), serde_json::Value::Bool(validated));
        object.insert("committed".into(), serde_json::Value::Bool(true));
        object.insert("status".into(), serde_json::Value::String("ok".into()));
        object.insert(
            "message".into(),
            serde_json::Value::String("已保存连接；下次一键开始时核验并应用。".into()),
        );
    }
    Ok(result)
}

/// 只把 profile 设为当前选择；真正 apply/start 只发生在一键开始。
#[tauri::command]
pub(crate) async fn set_active_profile(
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    id: String,
) -> Result<serde_json::Value, crate::commands::codex::RuntimeCommandError> {
    crate::api_only::require_api_profile(&config::default_dir(), Some(&id))?;
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking_typed(move || set_active_profile_inner_cmd(state, lifecycle, id)).await
}

fn set_active_profile_inner_cmd(
    state: SharedAppState,
    lifecycle: SharedLifecycle,
    id: String,
) -> Result<serde_json::Value, crate::commands::codex::RuntimeCommandError> {
    lifecycle
        .with_mutation(lifecycle::RuntimeMutationDomain::Intent, |_| {
            pin_active_profile_in_dir(&config::default_dir(), &state, &id)
        })
        .map_err(crate::commands::codex::RuntimeCommandError::from)
}

fn pin_active_profile_in_dir(
    dir: &Path,
    state: &SharedAppState,
    id: &str,
) -> Result<serde_json::Value, String> {
    let (applied_profile_id, changed) = config::update_result(dir, |cfg| {
        config::require_no_runtime_transaction(cfg)?;
        let profile = cfg
            .profile_by_id(id)
            .ok_or_else(|| format!("找不到 profile：{id}"))?;
        config::require_template_enabled(cfg, &profile.template_id)?;
        // This is local structural validation only. It must not read auth,
        // probe upstreams, or mutate either managed runtime.
        resolve_launch_plan(profile)?;
        let applied = cfg
            .runtime_binding
            .as_ref()
            .map(|binding| binding.profile_id.clone());
        let changed = cfg.active_id != id;
        cfg.active_id = id.to_string();
        Ok(((applied, changed), changed))
    })?;

    let science_running = {
        let app_state = crate::lock(state);
        app_state.sandbox.is_some() || app_state.science_runtime.is_some()
    };
    let hint = if science_running {
        "已设为当前选择，待应用。当前运行链和 Science 会话保持不变；下次一键开始时核验并应用。"
    } else {
        "已设为当前选择，待应用。当前运行链保持不变；下次一键开始时核验并应用。"
    };
    let mut result = crate::commands::runtime::config_mutation::typed_intent_outcome(
        "set_active_profile",
        if changed { "committed" } else { "no_change" },
        "committed",
        Some(id.to_string()),
        applied_profile_id.clone(),
        Some("accepted"),
        Some(science_running),
    );
    if let Some(object) = result.as_object_mut() {
        let apply_state = if applied_profile_id.as_deref() == Some(id) {
            "applied"
        } else {
            "pending"
        };
        object.insert("committed".into(), serde_json::Value::Bool(true));
        object.insert("status".into(), serde_json::Value::String("ok".into()));
        object.insert(
            "selected_profile_id".into(),
            serde_json::Value::String(id.into()),
        );
        object.insert(
            "applied_profile_id".into(),
            applied_profile_id.map_or(serde_json::Value::Null, serde_json::Value::String),
        );
        object.insert(
            "apply_state".into(),
            serde_json::Value::String(apply_state.into()),
        );
        object.insert(
            "science_running".into(),
            serde_json::Value::Bool(science_running),
        );
        object.insert("hint".into(), serde_json::Value::String(hint.into()));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::{
        acknowledge_pending_notice_inner, catalog_edit_from_parts, clear_profile_key_cmd,
        clear_profile_key_cmd_with, clear_profile_key_p2b, delete_profile_cmd,
        delete_profile_cmd_with, delete_profile_p2b, persist_profile_candidate_inner,
        pin_active_profile_in_dir, update_profile_connection_with, update_profile_metadata_inner,
    };
    use crate::{
        config::{self, Config, Profile, RuntimeBindingCommit, RuntimeTransactionJournal},
        lifecycle, lock, AppState, SharedAppState,
    };
    use std::{
        cell::Cell,
        fs,
        process::Command,
        sync::{mpsc, Arc, Mutex},
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("csswitch-{name}-{n}"))
    }

    fn profile(id: &str, key: &str) -> Profile {
        let (model_catalog, default_model_route_id, role_bindings) =
            crate::model_catalog::new_profile_catalog("deepseek", "anthropic", None).unwrap();
        let model = model_catalog
            .iter()
            .find(|route| route.selector_id == default_model_route_id)
            .unwrap()
            .upstream_model
            .clone();
        Profile {
            id: id.into(),
            name: id.into(),
            template_id: "deepseek".into(),
            category: "cn_official".into(),
            api_format: "anthropic".into(),
            base_url: "https://api.deepseek.com/anthropic".into(),
            api_key: key.into(),
            model,
            model_catalog,
            default_model_route_id,
            role_bindings,
            model_policy: crate::provider_contracts::ModelPolicy::SavedCatalog,
            ..Default::default()
        }
    }

    fn state_with_proxy_identity() -> SharedAppState {
        let mut state = AppState::default();
        state.secret = "runtime-secret".into();
        state.provider = "deepseek".into();
        state.gateway_kind = "rust".into();
        state.shim_mode = "off".into();
        state.launch_id = "launch-current".into();
        state.key_fp = 42;
        Arc::new(Mutex::new(state))
    }

    fn binding(profile_id: &str) -> RuntimeBindingCommit {
        RuntimeBindingCommit {
            profile_id: profile_id.into(),
            route_fp: "route".into(),
            catalog_fp: "catalog".into(),
            binding_fp: "binding".into(),
            science_adoption_attempt_id: None,
        }
    }

    fn assert_gateway_identity(state: &SharedAppState, expected_present: bool) {
        let state = lock(state);
        assert_eq!(!state.launch_id.is_empty(), expected_present);
        assert_eq!(!state.secret.is_empty(), expected_present);
        assert_eq!(state.sandbox_url.as_deref(), Some("http://127.0.0.1:18765"));
    }

    fn install_fake_science_child(state: &SharedAppState) -> u32 {
        let child = Command::new("/bin/sleep").arg("5").spawn().unwrap();
        let pid = child.id();
        lock(state).sandbox = Some(child);
        pid
    }

    fn assert_fake_science_child_running(state: &SharedAppState, pid: u32) {
        let mut state = lock(state);
        let child = state.sandbox.as_mut().expect("fake Science stays tracked");
        assert_eq!(child.id(), pid);
        assert!(child.try_wait().unwrap().is_none());
    }

    fn reap_fake_science_child(state: &SharedAppState) {
        let mut child = lock(state)
            .sandbox
            .take()
            .expect("fake Science remains available for explicit cleanup");
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn clear_active_profile_key_stops_runtime_proxy_identity() {
        let dir = tmpdir("clear-active-key");
        let cfg = Config {
            profiles: vec![profile("active", "sk-active")],
            active_id: "active".into(),
            runtime_binding: Some(binding("active")),
            ..Default::default()
        };
        config::save_to(&dir, &cfg).unwrap();
        let state = state_with_proxy_identity();
        let lifecycle = lifecycle::Lifecycle::new();
        let before = lifecycle.current_generation();

        clear_profile_key_cmd(&dir, &state, &lifecycle, "active").unwrap();

        let after = config::load_from(&dir).unwrap();
        assert_eq!(after.profile_by_id("active").unwrap().api_key, "");
        assert!(after.runtime_binding.is_none());
        assert!(lifecycle.current_generation() > before);
        let st = lock(&state);
        assert!(st.secret.is_empty());
        assert!(st.provider.is_empty());
        assert!(st.gateway_kind.is_empty());
        assert!(st.shim_mode.is_empty());
        assert!(st.launch_id.is_empty());
        assert_eq!(st.key_fp, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn profile_clear_and_delete_fail_before_commit_on_gateway_uncertainty() {
        for operation in ["clear", "delete"] {
            let dir = tmpdir(&format!("{operation}-active-gateway-uncertain"));
            let cfg = Config {
                profiles: vec![profile("active", "sk-active")],
                active_id: "active".into(),
                runtime_binding: Some(binding("active")),
                ..Default::default()
            };
            config::save_to(&dir, &cfg).unwrap();
            let before = fs::read(dir.join("config.json")).unwrap();
            let state = state_with_proxy_identity();
            let lifecycle = lifecycle::Lifecycle::new();
            let stop_uncertain = |_: &mut AppState| crate::GatewayStopOutcome::Uncertain {
                owned_count: 1,
                reason: "injected retained child".into(),
            };

            let error = match operation {
                "clear" => {
                    clear_profile_key_cmd_with(&dir, &state, &lifecycle, "active", stop_uncertain)
                        .unwrap_err()
                }
                "delete" => {
                    delete_profile_cmd_with(&dir, &state, &lifecycle, "active", stop_uncertain)
                        .unwrap_err()
                }
                _ => unreachable!(),
            };
            assert!(error.contains("配置未修改"), "{operation}: {error}");
            assert_eq!(fs::read(dir.join("config.json")).unwrap(), before);
            let after = config::load_from(&dir).unwrap();
            assert_eq!(after.profile_by_id("active").unwrap().api_key, "sk-active");
            assert!(after.runtime_binding.is_some());
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn clear_non_active_profile_key_leaves_runtime_proxy_identity() {
        let dir = tmpdir("clear-non-active-key");
        let cfg = Config {
            profiles: vec![profile("active", "sk-active"), profile("other", "sk-other")],
            active_id: "other".into(),
            runtime_binding: Some(binding("active")),
            ..Default::default()
        };
        config::save_to(&dir, &cfg).unwrap();
        let state = state_with_proxy_identity();
        let lifecycle = lifecycle::Lifecycle::new();
        let before = lifecycle.current_generation();

        clear_profile_key_cmd(&dir, &state, &lifecycle, "other").unwrap();

        let after = config::load_from(&dir).unwrap();
        assert_eq!(after.profile_by_id("other").unwrap().api_key, "");
        assert_eq!(lifecycle.current_generation(), before);
        let st = lock(&state);
        assert_eq!(st.secret, "runtime-secret");
        assert_eq!(st.provider, "deepseek");
        assert_eq!(st.gateway_kind, "rust");
        assert_eq!(st.shim_mode, "off");
        assert_eq!(st.launch_id, "launch-current");
        assert_eq!(st.key_fp, 42);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_active_profile_stops_runtime_proxy_identity() {
        let dir = tmpdir("delete-active-profile");
        let cfg = Config {
            profiles: vec![profile("active", "sk-active")],
            active_id: "active".into(),
            runtime_binding: Some(binding("active")),
            ..Default::default()
        };
        config::save_to(&dir, &cfg).unwrap();
        let state = state_with_proxy_identity();
        let lifecycle = lifecycle::Lifecycle::new();
        let before = lifecycle.current_generation();

        delete_profile_cmd(&dir, &state, &lifecycle, "active").unwrap();

        let after = config::load_from(&dir).unwrap();
        assert!(after.active_id.is_empty());
        assert!(after.profile_by_id("active").is_none());
        assert!(after.runtime_binding.is_none());
        assert!(lifecycle.current_generation() > before);
        let st = lock(&state);
        assert!(st.secret.is_empty());
        assert!(st.provider.is_empty());
        assert!(st.gateway_kind.is_empty());
        assert!(st.shim_mode.is_empty());
        assert!(st.launch_id.is_empty());
        assert_eq!(st.key_fp, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pin_changes_only_active_id_and_preserves_applied_and_in_memory_runtime() {
        let dir = tmpdir("pin-only");
        let binding = binding("active");
        let cfg = Config {
            profiles: vec![profile("active", "sk-active"), profile("next", "sk-next")],
            active_id: "active".into(),
            runtime_binding: Some(binding.clone()),
            ..Default::default()
        };
        config::save_to(&dir, &cfg).unwrap();
        let state = state_with_proxy_identity();
        lock(&state).boot.transition(
            crate::BootState::Attention,
            Some(serde_json::json!({"status": "keep"})),
        );

        let result = pin_active_profile_in_dir(&dir, &state, "next").unwrap();

        let after = config::load_from(&dir).unwrap();
        let mut expected = cfg.clone();
        expected.active_id = "next".into();
        assert_eq!(after, expected);
        assert_eq!(after.runtime_binding, Some(binding));
        assert!(after.runtime_transaction.is_none());
        assert_eq!(result["selected_profile_id"], "next");
        assert_eq!(result["applied_profile_id"], "active");
        assert_eq!(result["apply_state"], "pending");
        assert_eq!(result["science_running"], false);
        let st = lock(&state);
        assert_eq!(st.secret, "runtime-secret");
        assert_eq!(st.provider, "deepseek");
        assert_eq!(st.gateway_kind, "rust");
        assert_eq!(st.launch_id, "launch-current");
        assert_eq!(st.boot.state, crate::BootState::Attention);
        assert_eq!(st.boot.payload, Some(serde_json::json!({"status": "keep"})));
        assert!(st.sandbox.is_none());
        assert!(st.science_runtime.is_none());
        drop(st);

        config::update(&dir, |cfg| {
            cfg.profile_by_id_mut("active").unwrap().api_key = "sk-edited".into();
        })
        .unwrap();
        let result = pin_active_profile_in_dir(&dir, &state, "active").unwrap();
        assert_eq!(result["selected_profile_id"], "active");
        assert_eq!(result["applied_profile_id"], "active");
        assert_eq!(result["apply_state"], "applied");
        assert_eq!(
            config::load_from(&dir)
                .unwrap()
                .profile_by_id("active")
                .unwrap()
                .api_key,
            "sk-edited"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_selected_and_applied_profiles_use_runtime_binding_truth() {
        let dir = tmpdir("delete-selected-vs-applied");
        let cfg = Config {
            profiles: vec![profile("applied", "sk-a"), profile("selected", "sk-b")],
            active_id: "selected".into(),
            runtime_binding: Some(binding("applied")),
            ..Default::default()
        };
        config::save_to(&dir, &cfg).unwrap();
        let state = state_with_proxy_identity();
        let lifecycle = lifecycle::Lifecycle::new();
        let before = lifecycle.current_generation();

        delete_profile_cmd(&dir, &state, &lifecycle, "selected").unwrap();
        let after_selected = config::load_from(&dir).unwrap();
        assert!(after_selected.active_id.is_empty());
        assert_eq!(
            after_selected
                .runtime_binding
                .as_ref()
                .map(|b| b.profile_id.as_str()),
            Some("applied")
        );
        assert_eq!(lifecycle.current_generation(), before);
        assert_eq!(lock(&state).launch_id, "launch-current");

        delete_profile_cmd(&dir, &state, &lifecycle, "applied").unwrap();
        let after_applied = config::load_from(&dir).unwrap();
        assert!(after_applied.runtime_binding.is_none());
        assert!(lifecycle.current_generation() > before);
        assert!(lock(&state).launch_id.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn r0_select_profile_role_matrix_preserves_applied_and_live_state() {
        for (label, selected, applied, target) in [
            ("changed-to-applied", "selected", "applied", "applied"),
            ("changed-to-neither", "selected", "applied", "other"),
            ("noop-selected", "selected", "applied", "selected"),
            ("noop-selected-applied", "selected", "selected", "selected"),
        ] {
            let dir = tmpdir(&format!("r0-select-{label}"));
            let profiles = vec![
                profile("selected", "sk-selected"),
                profile("applied", "sk-applied"),
                profile("other", "sk-other"),
            ];
            let cfg = Config {
                profiles,
                active_id: selected.into(),
                runtime_binding: Some(binding(applied)),
                ..Default::default()
            };
            config::save_to(&dir, &cfg).unwrap();
            let state = state_with_proxy_identity();
            lock(&state).sandbox_url = Some("http://127.0.0.1:18765".into());

            let result = pin_active_profile_in_dir(&dir, &state, target).unwrap();

            let after = config::load_from(&dir).unwrap();
            let mut expected = cfg.clone();
            expected.active_id = target.into();
            assert_eq!(after, expected, "{label}");
            assert_eq!(result["selected_profile_id"], target, "{label}");
            assert_eq!(result["applied_profile_id"], applied, "{label}");
            assert_eq!(
                result["apply_state"],
                if target == applied {
                    "applied"
                } else {
                    "pending"
                },
                "{label}"
            );
            assert_gateway_identity(&state, true);
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn r0_update_connection_validation_tristate_preserves_applied_runtime() {
        use crate::scratch::ProbeOutcome;

        let cases = [
            ("ok", ProbeOutcome::Ok, Some(true)),
            ("unsupported", ProbeOutcome::Unsupported(405), Some(false)),
            (
                "rate-limited",
                ProbeOutcome::Ambiguous(Some(429)),
                Some(false),
            ),
            (
                "server-error",
                ProbeOutcome::Ambiguous(Some(503)),
                Some(false),
            ),
            ("no-response", ProbeOutcome::NoResponse, Some(false)),
            ("auth-reject", ProbeOutcome::Auth(401), None),
            ("model-reject", ProbeOutcome::ModelError(404), None),
        ];
        for (index, (label, outcome, expected_validated)) in cases.into_iter().enumerate() {
            let dir = tmpdir(&format!("r0-update-connection-{label}"));
            let target = if index % 2 == 0 {
                "selected"
            } else {
                "applied"
            };
            let cfg = Config {
                profiles: vec![
                    profile("selected", "sk-selected"),
                    profile("applied", "sk-applied"),
                ],
                active_id: "selected".into(),
                runtime_binding: Some(binding("applied")),
                ..Default::default()
            };
            config::save_to(&dir, &cfg).unwrap();
            let lifecycle = lifecycle::Lifecycle::new();
            let generation = lifecycle.current_generation();
            let state = state_with_proxy_identity();
            lock(&state).sandbox_url = Some("http://127.0.0.1:18765".into());
            let edited_key = format!("sk-edited-{label}");
            let result = update_profile_connection_with(
                &lifecycle,
                &dir,
                target.to_string(),
                None,
                None,
                None,
                Some(edited_key.clone()),
                None,
                None,
                None,
                |_candidate, adapter| {
                    assert_eq!(adapter, "deepseek");
                    Ok(())
                },
                |_prepared, _dir| Ok(()),
                |_candidate, _prepared| crate::runtime::profile::nonactive_probe_verdict(&outcome),
            );
            let after = config::load_from(&dir).unwrap();

            assert_eq!(after.active_id, cfg.active_id, "{label}");
            assert_eq!(after.runtime_binding, cfg.runtime_binding, "{label}");
            assert!(after.runtime_transaction.is_none(), "{label}");
            assert_eq!(lifecycle.current_generation(), generation, "{label}");
            assert_gateway_identity(&state, true);
            if let Some(expected_validated) = expected_validated {
                let result = result.unwrap();
                assert_eq!(result["committed"], true, "{label}");
                assert_eq!(result["validated"], expected_validated, "{label}");
                assert_eq!(
                    after.profile_by_id(target).unwrap().api_key,
                    edited_key,
                    "{label}"
                );
            } else {
                assert!(result.is_err(), "{label}");
                assert_eq!(after, cfg, "{label}");
            }
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn r0_update_connection_rechecks_auth_and_config_inside_lifecycle() {
        let dir = tmpdir("r0-update-connection-proof-drift");
        let cfg = Config {
            profiles: vec![
                profile("selected", "sk-selected"),
                profile("applied", "sk-applied"),
            ],
            active_id: "selected".into(),
            runtime_binding: Some(binding("applied")),
            ..Default::default()
        };
        config::save_to(&dir, &cfg).unwrap();
        let lifecycle = Arc::new(lifecycle::Lifecycle::new());
        let generation = lifecycle.current_generation();
        let state = state_with_proxy_identity();
        lock(&state).sandbox_url = Some("http://127.0.0.1:18765".into());
        let live_before = {
            let state = lock(&state);
            (
                state.secret.clone(),
                state.provider.clone(),
                state.launch_id.clone(),
                state.key_fp,
                state.sandbox_url.clone(),
            )
        };

        let (held_tx, held_rx) = mpsc::channel();
        let (prepared_tx, prepared_rx) = mpsc::channel();
        let holder_lifecycle = lifecycle.clone();
        let holder_dir = dir.clone();
        let holder = thread::spawn(move || {
            holder_lifecycle.with_serialized(|| {
                held_tx.send(()).unwrap();
                prepared_rx.recv().unwrap();
                config::update(&holder_dir, |current| {
                    current.profile_by_id_mut("selected").unwrap().api_key = "sk-concurrent".into();
                })
                .unwrap();
            });
        });
        held_rx.recv().unwrap();

        let validation_called = Cell::new(false);
        let result = update_profile_connection_with(
            lifecycle.as_ref(),
            &dir,
            "selected".into(),
            None,
            None,
            None,
            Some("sk-command-edit".into()),
            None,
            None,
            None,
            |_candidate, adapter| {
                assert_eq!(adapter, "deepseek");
                let snapshot = config::load_from(&dir).unwrap();
                prepared_tx.send(()).unwrap();
                Ok(snapshot)
            },
            |expected, verify_dir| {
                if config::load_from(verify_dir).map_err(|error| error.to_string())? == *expected {
                    Ok(())
                } else {
                    Err("config_changed_retry：认证检查期间配置发生变化".into())
                }
            },
            |_candidate, _prepared| {
                validation_called.set(true);
                Ok(true)
            },
        );
        holder.join().unwrap();

        let error = result.unwrap_err().to_string();
        let after = config::load_from(&dir).unwrap();
        let live_after = {
            let state = lock(&state);
            (
                state.secret.clone(),
                state.provider.clone(),
                state.launch_id.clone(),
                state.key_fp,
                state.sandbox_url.clone(),
            )
        };
        assert!(error.contains("config_changed_retry"), "{error}");
        assert!(!validation_called.get());
        assert_eq!(after.active_id, cfg.active_id);
        assert_eq!(after.runtime_binding, cfg.runtime_binding);
        assert!(after.runtime_transaction.is_none());
        assert_eq!(
            after.profile_by_id("selected").unwrap().api_key,
            "sk-concurrent",
            "the command edit must not overwrite the concurrent mutation"
        );
        assert_ne!(
            after.profile_by_id("selected").unwrap().api_key,
            "sk-command-edit"
        );
        assert_eq!(lifecycle.current_generation(), generation);
        assert_eq!(live_after, live_before);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn r0_profile_revocation_selected_applied_role_matrix() {
        for operation in ["clear", "delete"] {
            for (role, selected, applied, target, stops_gateway) in [
                ("selected-only", "target", "applied", "target", false),
                ("applied-only", "selected", "target", "target", true),
                ("selected-applied", "target", "target", "target", true),
                ("neither", "selected", "applied", "target", false),
            ] {
                let label = format!("{operation}-{role}");
                let dir = tmpdir(&format!("r0-revoke-{label}"));
                let cfg = Config {
                    profiles: vec![
                        profile("selected", "sk-selected"),
                        profile("applied", "sk-applied"),
                        profile("target", "sk-target"),
                    ],
                    active_id: selected.into(),
                    runtime_binding: Some(binding(applied)),
                    ..Default::default()
                };
                config::save_to(&dir, &cfg).unwrap();
                let state = state_with_proxy_identity();
                lock(&state).sandbox_url = Some("http://127.0.0.1:18765".into());
                let science_pid = install_fake_science_child(&state);
                let lifecycle = lifecycle::Lifecycle::new();
                let generation = lifecycle.current_generation();

                match operation {
                    "clear" => clear_profile_key_cmd(&dir, &state, &lifecycle, target).unwrap(),
                    "delete" => delete_profile_cmd(&dir, &state, &lifecycle, target).unwrap(),
                    _ => unreachable!(),
                }

                let after = config::load_from(&dir).unwrap();
                if operation == "clear" {
                    assert_eq!(after.profile_by_id(target).unwrap().api_key, "", "{label}");
                    assert_eq!(after.active_id, selected, "{label}");
                } else {
                    assert!(after.profile_by_id(target).is_none(), "{label}");
                    assert_eq!(
                        after.active_id,
                        if selected == target { "" } else { selected },
                        "{label}"
                    );
                }
                assert_eq!(
                    after
                        .runtime_binding
                        .as_ref()
                        .map(|binding| binding.profile_id.as_str()),
                    if applied == target {
                        None
                    } else {
                        Some(applied)
                    },
                    "{label}"
                );
                assert_eq!(
                    lifecycle.current_generation() > generation,
                    stops_gateway,
                    "{label}"
                );
                assert_gateway_identity(&state, !stops_gateway);
                assert_fake_science_child_running(&state, science_pid);
                reap_fake_science_child(&state);
                let _ = fs::remove_dir_all(&dir);
            }
        }
    }

    #[test]
    fn pin_rejects_transaction_unknown_disabled_and_invalid_without_writes() {
        for (label, cfg, id) in [
            (
                "transaction",
                Config {
                    profiles: vec![profile("active", "sk-active"), profile("next", "sk-next")],
                    active_id: "active".into(),
                    runtime_transaction: Some(
                        RuntimeTransactionJournal {
                            transaction_id: "txn".into(),
                            target_profile_id: "next".into(),
                            stage: "start_gateway".into(),
                            previous_binding: None,
                            previous_gateway: None,
                        }
                        .into(),
                    ),
                    ..Default::default()
                },
                "next",
            ),
            (
                "unknown",
                Config {
                    profiles: vec![profile("active", "sk-active")],
                    active_id: "active".into(),
                    ..Default::default()
                },
                "missing",
            ),
            (
                "disabled",
                Config {
                    profiles: vec![Profile {
                        id: "codex".into(),
                        name: "codex".into(),
                        template_id: "codex".into(),
                        api_format: "openai_responses".into(),
                        credential_source:
                            crate::provider_contracts::CredentialSource::CsswitchOauth,
                        credential_ref: Some("csswitch:codex:default".into()),
                        model_policy: crate::provider_contracts::ModelPolicy::DynamicCatalog,
                        ..Default::default()
                    }],
                    active_id: String::new(),
                    experimental_codex_enabled: false,
                    ..Default::default()
                },
                "codex",
            ),
        ] {
            let dir = tmpdir(&format!("pin-reject-{label}"));
            config::save_to(&dir, &cfg).unwrap();
            let before = fs::read(dir.join("config.json")).unwrap();
            let state = state_with_proxy_identity();
            assert!(
                pin_active_profile_in_dir(&dir, &state, id).is_err(),
                "{label} must fail closed"
            );
            assert_eq!(
                fs::read(dir.join("config.json")).unwrap(),
                before,
                "{label}"
            );
            assert_eq!(config::load_from(&dir).unwrap(), cfg, "{label}");
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn transaction_journal_blocks_clear_delete_and_candidate_persist_without_side_effects() {
        let dir = tmpdir("profile-mutations-txn-guard");
        let journal = RuntimeTransactionJournal {
            transaction_id: "txn".into(),
            target_profile_id: "selected".into(),
            stage: "start_gateway".into(),
            previous_binding: Some(binding("applied")),
            previous_gateway: None,
        };
        let cfg = Config {
            profiles: vec![profile("applied", "sk-a"), profile("selected", "sk-b")],
            active_id: "selected".into(),
            runtime_binding: Some(binding("applied")),
            runtime_transaction: Some(journal.into()),
            ..Default::default()
        };
        config::save_to(&dir, &cfg).unwrap();
        let backup_path = dir.join("config.json.bak");
        fs::write(&backup_path, b"unique-existing-backup").unwrap();
        let state = state_with_proxy_identity();
        let lifecycle = lifecycle::Lifecycle::new();
        let generation = lifecycle.current_generation();
        let before = fs::read(dir.join("config.json")).unwrap();
        let backup_before = fs::read(&backup_path).unwrap();

        for error in [
            clear_profile_key_cmd(&dir, &state, &lifecycle, "applied").unwrap_err(),
            delete_profile_cmd(&dir, &state, &lifecycle, "selected").unwrap_err(),
        ] {
            assert!(error.contains("code=runtime_transaction_in_progress"));
            assert_eq!(fs::read(dir.join("config.json")).unwrap(), before);
            assert_eq!(fs::read(&backup_path).unwrap(), backup_before);
            assert_eq!(lifecycle.current_generation(), generation);
            assert_eq!(lock(&state).launch_id, "launch-current");
        }

        let mut candidate = cfg.profile_by_id("selected").unwrap().clone();
        candidate.api_key = "sk-edited".into();
        let error = persist_profile_candidate_inner(&dir, "selected", &candidate).unwrap_err();
        assert!(error.contains("code=runtime_transaction_in_progress"));
        assert_eq!(fs::read(dir.join("config.json")).unwrap(), before);
        assert_eq!(fs::read(&backup_path).unwrap(), backup_before);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn p2b_applied_profile_revoke_crash_matrix() {
        for operation in ["clear", "delete"] {
            let dir = tmpdir(&format!("p2b-revoke-{operation}"));
            let cfg = Config {
                profiles: vec![profile("applied", "sk-applied")],
                active_id: "applied".into(),
                runtime_binding: Some(binding("applied")),
                ..Default::default()
            };
            config::save_to(&dir, &cfg).unwrap();
            let state = state_with_proxy_identity();
            let lifecycle = lifecycle::Lifecycle::new();
            let result = if operation == "clear" {
                clear_profile_key_p2b(&dir, &state, &lifecycle, "applied")
            } else {
                delete_profile_p2b(&dir, &state, &lifecycle, "applied")
            };
            let outcome = result.unwrap();
            assert_eq!(outcome["schema_version"], 1, "{operation}");
            assert_eq!(
                outcome["operation"],
                if operation == "clear" {
                    "clear_applied_profile_key"
                } else {
                    "delete_applied_profile"
                }
            );
            assert_eq!(outcome["disposition"], "completed");
            let after = config::load_from(&dir).unwrap();
            if operation == "clear" {
                assert_eq!(after.profile_by_id("applied").unwrap().api_key, "");
            } else {
                assert!(after.profile_by_id("applied").is_none());
            }
            assert!(after.runtime_binding.is_none());
            assert!(lock(&state).proxy.is_none());
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn p2b_applied_profile_backup_scrub_is_an_exact_durable_effect() {
        for operation in ["clear", "delete"] {
            for scenario in ["absent", "present", "unlink_failure", "sync_failure"] {
                let dir = tmpdir(&format!("p2b-revoke-backup-{operation}-{scenario}"));
                let cfg = Config {
                    profiles: vec![profile("applied", "sk-backup-secret")],
                    active_id: "applied".into(),
                    runtime_binding: Some(binding("applied")),
                    ..Default::default()
                };
                config::save_to(&dir, &cfg).unwrap();
                if scenario != "absent" {
                    config::write_rolling_backup(&dir).unwrap();
                    assert!(fs::read_to_string(dir.join("config.json.bak"))
                        .unwrap()
                        .contains("sk-backup-secret"));
                }
                let _fault = match scenario {
                    "unlink_failure" => Some(config::test_arm_drop_rolling_backup_failure(
                        dir.clone(),
                        config::DropRollingBackupFault::Unlink,
                    )),
                    "sync_failure" => Some(config::test_arm_drop_rolling_backup_failure(
                        dir.clone(),
                        config::DropRollingBackupFault::DirectorySync,
                    )),
                    _ => None,
                };
                let state = state_with_proxy_identity();
                let lifecycle = lifecycle::Lifecycle::new();
                let result = if operation == "clear" {
                    clear_profile_key_p2b(&dir, &state, &lifecycle, "applied")
                } else {
                    delete_profile_p2b(&dir, &state, &lifecycle, "applied")
                };
                let failed = scenario.ends_with("failure");
                if failed {
                    let error = result.unwrap_err();
                    assert!(
                        error.contains("config_mutation_attention"),
                        "{operation}/{scenario}: {error}"
                    );
                    let receipt: serde_json::Value = serde_json::from_slice(
                        &config::read_config_mutation_operation_receipt(&dir)
                            .unwrap()
                            .expect("failed backup scrub must retain receipt"),
                    )
                    .unwrap();
                    assert_eq!(receipt["terminal"]["state"], "attention");
                    assert_eq!(receipt["terminal"]["config_state"], "after");
                    assert_eq!(receipt["effects"][1]["state"], "succeeded");
                    assert_eq!(receipt["effects"][2]["kind"], "delete_rolling_backup");
                    assert_eq!(receipt["effects"][2]["state"], "uncertain");
                    assert!(config::load_from(&dir)
                        .unwrap()
                        .config_mutation_operation_fence()
                        .unwrap()
                        .is_some());
                    assert_eq!(
                        dir.join("config.json.bak").exists(),
                        scenario == "unlink_failure"
                    );
                } else {
                    let outcome = result.unwrap();
                    assert_eq!(
                        outcome["disposition"], "completed",
                        "{operation}/{scenario}"
                    );
                    assert!(!dir.join("config.json.bak").exists());
                    assert!(config::read_config_mutation_operation_receipt(&dir)
                        .unwrap()
                        .is_none());
                }
                let _ = fs::remove_dir_all(&dir);
            }
        }
    }

    #[test]
    fn p2b_intent_only_outcomes_have_zero_runtime_effect() {
        let dir = tmpdir("p2b-intent-only");
        let cfg = Config {
            profiles: vec![profile("one", "sk-one"), profile("two", "sk-two")],
            active_id: "one".into(),
            runtime_binding: Some(binding("one")),
            ..Default::default()
        };
        config::save_to(&dir, &cfg).unwrap();
        let state = state_with_proxy_identity();
        let result = pin_active_profile_in_dir(&dir, &state, "two").unwrap();
        assert_eq!(result["schema_version"], 1);
        assert_eq!(result["operation"], "set_active_profile");
        assert_eq!(result["selected_profile_id"], "two");
        assert_eq!(result["applied_profile_id"], "one");
        assert_eq!(result["science_running"], false);
        assert_eq!(result["disposition"], "committed");
        assert_eq!(result["apply_state"], "pending");
        let unchanged = pin_active_profile_in_dir(&dir, &state, "two").unwrap();
        assert_eq!(unchanged["disposition"], "no_change");
        assert_eq!(unchanged["apply_state"], "pending");

        config::update(&dir, |current| {
            current.active_id = "one".into();
            current.runtime_binding = Some(binding("two"));
        })
        .unwrap();
        let selected_commit = pin_active_profile_in_dir(&dir, &state, "two").unwrap();
        assert_eq!(selected_commit["disposition"], "committed");
        assert_eq!(selected_commit["applied_profile_id"], "two");
        assert_eq!(selected_commit["apply_state"], "applied");
        assert_eq!(lock(&state).launch_id, "launch-current");
        assert!(config::read_config_mutation_operation_receipt(&dir)
            .unwrap()
            .is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn p2b_notice_create_metadata_writers_reject_open_fence() {
        let dir = tmpdir("p2b-ordinary-writers");
        let cfg = Config {
            profiles: vec![profile("one", "sk-one")],
            active_id: "one".into(),
            ..Default::default()
        };
        config::save_to(&dir, &cfg).unwrap();
        let fence = config::ConfigMutationOperationFence::begin(
            config::new_id(),
            "set_mode_official".into(),
            "a".repeat(64),
            config::config_mutation_config_fingerprint(&cfg).unwrap(),
            None,
        );
        config::begin_config_mutation_operation(&dir, &cfg, &fence, br#"{"receipt":1}"#).unwrap();
        assert!(update_profile_metadata_inner(&dir, "one", "changed", None).is_err());
        assert!(acknowledge_pending_notice_inner(&dir, "notice").is_err());
        assert!(config::read_config_mutation_operation_receipt(&dir)
            .unwrap()
            .is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pin_rejects_invalid_local_structure_without_rewriting_it() {
        let dir = tmpdir("pin-invalid-structure");
        let cfg = Config {
            profiles: vec![profile("invalid", "sk-invalid")],
            ..Default::default()
        };
        config::save_to(&dir, &cfg).unwrap();
        let path = dir.join("config.json");
        let mut raw: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        raw["profiles"][0]["credential_ref"] = serde_json::json!("not-allowed");
        fs::write(&path, serde_json::to_vec_pretty(&raw).unwrap()).unwrap();
        let before = fs::read(&path).unwrap();
        let state = state_with_proxy_identity();

        assert!(pin_active_profile_in_dir(&dir, &state, "invalid").is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_pin_is_local_and_does_not_require_oauth_proof() {
        let dir = tmpdir("pin-codex-local");
        let cfg = Config {
            profiles: vec![Profile {
                id: "codex".into(),
                name: "codex".into(),
                template_id: "codex".into(),
                api_format: "openai_responses".into(),
                credential_source: crate::provider_contracts::CredentialSource::CsswitchOauth,
                credential_ref: Some("csswitch:codex:default".into()),
                model_policy: crate::provider_contracts::ModelPolicy::DynamicCatalog,
                ..Default::default()
            }],
            experimental_codex_enabled: true,
            ..Default::default()
        };
        config::save_to(&dir, &cfg).unwrap();
        let state = Arc::new(Mutex::new(AppState::default()));

        let result = pin_active_profile_in_dir(&dir, &state, "codex").unwrap();

        assert_eq!(result["apply_state"], "pending");
        assert_eq!(config::load_from(&dir).unwrap().active_id, "codex");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn catalog_edit_requires_all_three_fields_and_rejects_legacy_mix() {
        let route = crate::model_catalog::ModelRoute {
            selector_id: "claude-csswitch-test-model-123456789abc".into(),
            display_name: "Model".into(),
            upstream_model: "model-upstream".into(),
            supports_tools: Some(true),
            ..Default::default()
        };
        let roles = crate::model_catalog::RoleBindings {
            sonnet: route.selector_id.clone(),
            opus: route.selector_id.clone(),
            haiku: route.selector_id.clone(),
            fable: route.selector_id.clone(),
            ..Default::default()
        };
        assert!(catalog_edit_from_parts(false, None, None, None)
            .unwrap()
            .is_none());
        assert!(catalog_edit_from_parts(
            false,
            Some(vec![route.clone()]),
            Some(route.selector_id.clone()),
            Some(roles.clone()),
        )
        .unwrap()
        .is_some());
        assert!(catalog_edit_from_parts(false, Some(vec![route.clone()]), None, None).is_err());
        assert!(catalog_edit_from_parts(
            true,
            Some(vec![route]),
            Some("claude-csswitch-test-model-123456789abc".into()),
            Some(roles),
        )
        .is_err());
    }
}

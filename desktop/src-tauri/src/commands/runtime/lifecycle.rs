use super::*;

fn require_confirmed_gateway_stop(
    outcome: crate::GatewayStopOutcome,
    context: &str,
) -> Result<(), String> {
    outcome.require_stopped(context)
}

pub(super) fn settings_runtime_state(teardown: bool) -> &'static str {
    if teardown {
        "stopped"
    } else {
        "preserved"
    }
}

/// 切换运行模式（"proxy" 第三方 / "official" 官方）。切官方要先拆第三方链路成功再落盘。
pub(super) async fn set_mode_command(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    mode: String,
) -> Result<serde_json::Value, String> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking(move || set_mode_inner(app, state, lifecycle, mode)).await
}

pub(super) fn set_mode_inner<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
    mode: String,
) -> Result<serde_json::Value, String> {
    set_mode_inner_with(
        app,
        state,
        lifecycle,
        mode,
        config::default_dir(),
        ScienceHostAdapter::claim_stop,
        |app, request| ScienceHostAdapter::execute_stop(app, request).into_parts(),
        AppState::stop_proxy,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn set_mode_inner_with<R, Claim, Execute, StopGateway>(
    app: tauri::AppHandle<R>,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
    mode: String,
    dir: std::path::PathBuf,
    claim_science: Claim,
    execute_science: Execute,
    stop_gateway: StopGateway,
) -> Result<serde_json::Value, String>
where
    R: tauri::Runtime,
    Claim: FnOnce(
        Option<&crate::runtime::science::ScienceRuntimeIdentity>,
    ) -> Result<
        crate::runtime::science::ScienceStopRequest,
        crate::runtime::science::ScienceStopFailure,
    >,
    Execute: FnOnce(
        &tauri::AppHandle<R>,
        crate::runtime::science::ScienceStopRequest,
    ) -> (crate::runtime::science::ScienceStopOutcome, bool),
    StopGateway: FnOnce(&mut AppState) -> crate::GatewayStopOutcome,
{
    if mode != "proxy" && mode != "official" {
        return Err(format!("未知模式：{mode}（只支持 proxy / official）。"));
    }
    // 经串行器（修 P1-b）：切官方的「拆链路 + 落盘」必须与「一键开始」等互斥，否则一键起到一半时
    // 切官方会先停链路、一键随后又把沙箱/OAuth 起起来 → 显示官方却有第三方沙箱在跑。bump_generation
    // 作废任何在途启动，防被停后又拿旧配置写回运行态。
    lifecycle.with_mutation(RuntimeMutationDomain::Destructive, |_| {
        let preflight = config::load_from(&dir).map_err(|error| error.to_string())?;
        config::require_no_runtime_transaction(&preflight)?;
        let (science_effect, gateway_effect) = {
            let current = lock(&state);
            (
                current.science_runtime.is_some() || current.sandbox.is_some(),
                current.proxy.is_some(),
            )
        };
        let mut after = preflight.clone();
        after.mode = mode.clone();
        let mut mutation = if mode == "official" && (science_effect || gateway_effect) {
            let effects = [
                science_effect.then_some(config_mutation::ConfigMutationEffectKind::StopScience),
                gateway_effect.then_some(config_mutation::ConfigMutationEffectKind::StopGateway),
                Some(config_mutation::ConfigMutationEffectKind::ConfigCommit),
            ]
            .into_iter()
            .flatten()
            .collect();
            Some(
                config_mutation::begin(
                    &dir,
                    config_mutation::ConfigMutationOperation::SetModeOfficial,
                    &preflight,
                    Some(&after),
                    config_mutation::MutationTarget {
                        mode: Some("official".into()),
                        ..Default::default()
                    },
                    config_mutation::RuntimePlan {
                        owner_generation: lifecycle.current_generation(),
                        ..Default::default()
                    },
                    effects,
                    None,
                    None,
                )
                .map_err(|error| config_mutation::command_error_string(&error))?,
            )
        } else {
            None
        };
        if mode == "official" {
            let mut science_index = None;
            let mut gateway_index = None;
            let mut next_effect = 0usize;
            if science_effect {
                science_index = Some(next_effect);
                next_effect += 1;
            }
            if gateway_effect {
                gateway_index = Some(next_effect);
            }
            if let Some(operation) = mutation.as_mut() {
                if let Some(index) = science_index {
                    operation
                        .checkpoint_effect_or_attention(
                            index,
                            config_mutation::ConfigMutationEffectState::InProgress,
                            None,
                            "science_stop_checkpoint_failed",
                            "before",
                            "unknown",
                            config::ConfigMutationTerminalConfigImage::Before,
                        )
                        .map_err(|error| config_mutation::command_error_string(&error))?;
                }
                if let Some(index) = gateway_index {
                    operation
                        .checkpoint_effect_or_attention(
                            index,
                            config_mutation::ConfigMutationEffectState::InProgress,
                            None,
                            "gateway_stop_checkpoint_failed",
                            "before",
                            "unknown",
                            config::ConfigMutationTerminalConfigImage::Before,
                        )
                        .map_err(|error| config_mutation::command_error_string(&error))?;
                }
            }
            // Generation invalidation is part of the destructive stop effect,
            // so it must remain behind every durable InProgress checkpoint.
            let generation = lifecycle.bump_generation();
            let (owner, request) =
                claim_process_local_science_stop(&state, generation, claim_science);
            // The stop script and bounded TERM/KILL waits intentionally run
            // without AppState so status can continue reading the current
            // process-local owner while set_mode holds the mutation lease.
            let execution = request.map(|request| execute_science(&app, request));
            let mut st = lock(&state);
            let science_result = publish_process_local_science_stop(
                &mut st,
                lifecycle.current_generation(),
                owner,
                execution,
            );
            if let Err(error) = science_result {
                if let Some(mut operation) = mutation.take() {
                    if let Some(index) = science_index {
                        let _ = operation.checkpoint_effect(
                            index,
                            config_mutation::ConfigMutationEffectState::Uncertain,
                            Some("science_stop_uncertain"),
                        );
                    }
                    let attention = operation.finish(
                        "attention",
                        "before",
                        "unknown",
                        Some("science_stop_uncertain"),
                        config::ConfigMutationTerminalConfigImage::Before,
                    );
                    if let Err(attention) = attention {
                        return Err(config_mutation::command_error_string(
                            &attention.with_message(
                                if error.to_string().contains("process-local owner 已变化") {
                                    "停止沙箱失败：process-local owner 已变化"
                                } else {
                                    "停止沙箱失败，未切换到官方模式"
                                },
                            ),
                        ));
                    }
                }
                return Err(format!(
                    "停止沙箱失败，未切换到官方模式：{error}（真实实例 8765 未受影响）"
                ));
            }
            if let Some(operation) = mutation.as_mut() {
                if let Some(index) = science_index {
                    operation
                        .checkpoint_effect_or_attention(
                            index,
                            config_mutation::ConfigMutationEffectState::Succeeded,
                            Some("stopped"),
                            "science_stop_checkpoint_failed",
                            "before",
                            "stopped",
                            config::ConfigMutationTerminalConfigImage::Before,
                        )
                        .map_err(|error| config_mutation::command_error_string(&error))?;
                }
            }
            let gateway_result = stop_gateway(&mut st);
            if let Err(error) = require_confirmed_gateway_stop(
                gateway_result,
                "Gateway 停止结果未确认，未切换到官方模式",
            ) {
                if let Some(mut operation) = mutation.take() {
                    if let Some(index) = gateway_index {
                        let _ = operation.checkpoint_effect(
                            index,
                            config_mutation::ConfigMutationEffectState::Uncertain,
                            Some("gateway_stop_uncertain"),
                        );
                    }
                    let attention = operation.finish(
                        "attention",
                        "before",
                        "unknown",
                        Some("gateway_stop_uncertain"),
                        config::ConfigMutationTerminalConfigImage::Before,
                    );
                    if let Err(attention) = attention {
                        return Err(config_mutation::command_error_string(
                            &attention.with_message("Gateway 停止结果未确认，未切换到官方模式"),
                        ));
                    }
                }
                return Err(error);
            }
            if let Some(operation) = mutation.as_mut() {
                if let Some(index) = gateway_index {
                    operation
                        .checkpoint_effect_or_attention(
                            index,
                            config_mutation::ConfigMutationEffectState::Succeeded,
                            Some("stopped"),
                            "gateway_stop_checkpoint_failed",
                            "before",
                            "stopped",
                            config::ConfigMutationTerminalConfigImage::Before,
                        )
                        .map_err(|error| config_mutation::command_error_string(&error))?;
                }
            }
        }
        let outcome = if let Some(mut operation) = mutation {
            let mode_for_commit = mode.clone();
            let before_fingerprint = operation.fence().before_config_fingerprint.clone();
            let commit_index = operation.receipt().effects.len().saturating_sub(1);
            operation
                .checkpoint_effect_or_attention(
                    commit_index,
                    config_mutation::ConfigMutationEffectState::InProgress,
                    None,
                    "config_commit_checkpoint_failed",
                    "before",
                    if science_effect || gateway_effect {
                        "stopped"
                    } else {
                        "preserved"
                    },
                    config::ConfigMutationTerminalConfigImage::Before,
                )
                .map_err(|error| config_mutation::command_error_string(&error))?;
            if let Err(error) = operation.update_config(move |c| {
                if config::config_mutation_config_fingerprint(c)
                    .map_err(|error| error.to_string())?
                    != before_fingerprint
                {
                    return Err("Config mutation before-image 在 commit 前发生变化".into());
                }
                c.mode = mode_for_commit;
                Ok(((), true))
            }) {
                let attention = operation.finish(
                    "attention",
                    "before",
                    "unknown",
                    Some("config_commit_failed"),
                    config::ConfigMutationTerminalConfigImage::Before,
                );
                return Err(match attention {
                    Err(attention) => {
                        config_mutation::command_error_string(&attention.with_message(
                            if error.contains("test-only config update commit failure") {
                                "test-only config update commit failure"
                            } else {
                                "官方模式配置未提交"
                            },
                        ))
                    }
                    Ok(_) => error,
                });
            }
            operation
                .checkpoint_effect_or_attention(
                    commit_index,
                    config_mutation::ConfigMutationEffectState::Succeeded,
                    Some("committed"),
                    "config_commit_checkpoint_failed",
                    "after",
                    "stopped",
                    config::ConfigMutationTerminalConfigImage::After,
                )
                .map_err(|error| config_mutation::command_error_string(&error))?;
            let outcome = operation
                .finish(
                    "completed",
                    "after",
                    "stopped",
                    None,
                    config::ConfigMutationTerminalConfigImage::After,
                )
                .map_err(|error| config_mutation::command_error_string(&error))?;
            config_mutation::outcome_json(&outcome)
        } else {
            config::update_result(&dir, {
                let mode = mode.clone();
                move |c| {
                    config::require_no_runtime_transaction(c)?;
                    c.mode = mode;
                    Ok(((), true))
                }
            })
            .map_err(|e| e.to_string())?;
            config_mutation::typed_intent_outcome(
                "set_mode",
                "committed",
                "committed",
                None,
                None,
                Some("not_run"),
                Some(false),
            )
        };
        {
            let mut app_state = lock(&state);
            app_state.history_recovery = None;
        }
        crate::clear_boot_attention(&app);
        Ok(outcome)
    })
}

#[derive(Deserialize)]
pub(crate) struct UiSettings {
    pub(super) proxy_port: u16,
    pub(super) sandbox_port: u16,
    #[serde(default)]
    pub(super) reuse_system_ssh: bool,
    #[serde(default)]
    pub(super) allow_science_host_home: bool,
}

pub(super) struct SetSettingsPaths {
    pub(super) config_dir: std::path::PathBuf,
    pub(super) sandbox_home: std::path::PathBuf,
}

/// 运行设置（端口 + Home / 系统 SSH 配置授权；provider/连接改走 profile CRUD + set_active_profile）。
/// 经串行器（修 P1-c）：端口或 SSH 授权一旦变化，正在跑的沙箱都必须拆掉，
/// 与新端口不一致；此处把这条陈旧链路拆掉（只停我们的沙箱、绝不碰 8765），逼下次「一键开始」按新端口重建，
/// 杜绝「复用旧沙箱指向死端口、UI 却报沿用不变」。
pub(super) async fn set_settings_command(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    cfg: UiSettings,
) -> Result<serde_json::Value, String> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking(move || set_settings_inner(app, state, lifecycle, cfg)).await
}

pub(super) fn set_settings_inner<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
    cfg: UiSettings,
) -> Result<serde_json::Value, String> {
    validate_runtime_ports(cfg.proxy_port, cfg.sandbox_port)?;
    if cfg.reuse_system_ssh {
        system_ssh_config_path()?;
        system_ssh_hosts()?;
    }
    set_settings_inner_with(
        app,
        state,
        lifecycle,
        cfg,
        SetSettingsPaths {
            config_dir: config::default_dir(),
            sandbox_home: crate::runtime::science::sandbox_home(),
        },
        ScienceHostAdapter::claim_stop,
        |app, request| ScienceHostAdapter::execute_stop(app, request).into_parts(),
        AppState::stop_proxy,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn set_settings_inner_with<R, Claim, Execute, StopGateway>(
    app: tauri::AppHandle<R>,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
    cfg: UiSettings,
    paths: SetSettingsPaths,
    claim_science: Claim,
    execute_science: Execute,
    stop_gateway: StopGateway,
) -> Result<serde_json::Value, String>
where
    R: tauri::Runtime,
    Claim: FnOnce(
        Option<&crate::runtime::science::ScienceRuntimeIdentity>,
    ) -> Result<
        crate::runtime::science::ScienceStopRequest,
        crate::runtime::science::ScienceStopFailure,
    >,
    Execute: FnOnce(
        &tauri::AppHandle<R>,
        crate::runtime::science::ScienceStopRequest,
    ) -> (crate::runtime::science::ScienceStopOutcome, bool),
    StopGateway: FnOnce(&mut AppState) -> crate::GatewayStopOutcome,
{
    lifecycle.with_mutation(RuntimeMutationDomain::Destructive, |_| {
        let old = config::load_from(&paths.config_dir).map_err(|e| e.to_string())?;
        config::require_no_runtime_transaction(&old)?;
        let teardown = settings_change_needs_teardown(
            old.proxy_port,
            cfg.proxy_port,
            old.sandbox_port,
            cfg.sandbox_port,
        ) || old.reuse_system_ssh != cfg.reuse_system_ssh
            || old.allow_science_host_home != cfg.allow_science_host_home;

        // Even false -> false is not automatically a no-op: an earlier
        // interrupted launch may have left one of the three CSSwitch-owned
        // SSH leaves behind.  Read-only preflight classifies absent/owned;
        // foreign or ambiguous state fails before runtime teardown.
        let bridge_owned = if !cfg.reuse_system_ssh {
            preflight_science_ssh_bridge_cleanup(&paths.sandbox_home)?
        } else {
            false
        };
        let stub_owned = if !cfg.reuse_system_ssh {
            preflight_managed_sandbox_ssh_stub_cleanup(&paths.sandbox_home)?
        } else {
            false
        };
        let (bridge_config_path, bridge_sidecar_path) =
            science_ssh_bridge_asset_paths(&paths.sandbox_home);
        let bridge_config_before = bridge_owned
            .then(|| config_mutation::capture_asset_identity(&bridge_config_path))
            .transpose()?
            .flatten();
        let bridge_sidecar_before = bridge_owned
            .then(|| config_mutation::capture_asset_identity(&bridge_sidecar_path))
            .transpose()?
            .flatten();
        if bridge_owned
            && (bridge_config_before.is_none() || bridge_sidecar_before.is_none())
        {
            return Err("SSH bridge exact cleanup identity 缺失".into());
        }
        let stub_path = managed_sandbox_ssh_stub_path(&paths.sandbox_home);
        let stub_before = stub_owned
            .then(|| config_mutation::capture_asset_identity(&stub_path))
            .transpose()?
            .flatten();
        if stub_owned && stub_before.is_none() {
            return Err("managed SSH stub exact cleanup identity 缺失".into());
        }
        let destructive = teardown || bridge_owned || stub_owned;
        let mut after = old.clone();
        after.proxy_port = cfg.proxy_port;
        after.sandbox_port = cfg.sandbox_port;
        after.reuse_system_ssh = cfg.reuse_system_ssh;
        after.allow_science_host_home = cfg.allow_science_host_home;
        let mut mutation = if destructive {
            let mut effects = Vec::new();
            if teardown {
                let current = lock(&state);
                if current.science_runtime.is_some() || current.sandbox.is_some() {
                    effects.push(config_mutation::ConfigMutationEffectKind::StopScience);
                }
                if current.proxy.is_some() {
                    effects.push(config_mutation::ConfigMutationEffectKind::StopGateway);
                }
            }
            if bridge_owned {
                effects.push(config_mutation::ConfigMutationEffectKind::DeleteSshBridgeSidecar);
            }
            if stub_owned {
                effects.push(config_mutation::ConfigMutationEffectKind::DeleteManagedSshStub);
            }
            effects.push(config_mutation::ConfigMutationEffectKind::ConfigCommit);
            Some(
                config_mutation::begin(
                    &paths.config_dir,
                    config_mutation::ConfigMutationOperation::SetSettingsDestructive,
                    &old,
                    Some(&after),
                    config_mutation::MutationTarget {
                        proxy_port: Some(cfg.proxy_port),
                        sandbox_port: Some(cfg.sandbox_port),
                        reuse_system_ssh: Some(cfg.reuse_system_ssh),
                        ..Default::default()
                    },
                    config_mutation::RuntimePlan {
                        owner_generation: lifecycle.current_generation(),
                        ..Default::default()
                    },
                    effects,
                    None,
                    Some(config_mutation::SshPlan {
                        bridge_config: bridge_config_before.clone().map(|identity| {
                            config_mutation::SshLeafReceipt {
                                before_identity_or_absent: Some(identity),
                                expected_after: "restored".into(),
                                observed_after_identity_or_absent: None,
                            }
                        }),
                        bridge_sidecar: bridge_sidecar_before.clone().map(|identity| {
                            config_mutation::SshLeafReceipt {
                                before_identity_or_absent: Some(identity),
                                expected_after: "absent".into(),
                                observed_after_identity_or_absent: None,
                            }
                        }),
                        managed_stub: stub_before.clone().map(|identity| {
                            config_mutation::SshLeafReceipt {
                                before_identity_or_absent: Some(identity),
                                expected_after: "absent".into(),
                                observed_after_identity_or_absent: None,
                            }
                        }),
                    }),
                )
                .map_err(|error| config_mutation::command_error_string(&error))?,
            )
        } else {
            None
        };

        let mut effect_cursor = 0usize;
        let science_index = if destructive && teardown {
            let current = lock(&state);
            let has_science = current.science_runtime.is_some() || current.sandbox.is_some();
            drop(current);
            has_science.then(|| {
                let index = effect_cursor;
                effect_cursor += 1;
                index
            })
        } else {
            None
        };
        let gateway_index = if destructive && teardown {
            let current = lock(&state);
            let has_gateway = current.proxy.is_some();
            drop(current);
            has_gateway.then(|| {
                let index = effect_cursor;
                effect_cursor += 1;
                index
            })
        } else {
            None
        };
        let bridge_index = bridge_owned.then(|| {
            let index = effect_cursor;
            effect_cursor += 1;
            index
        });
        let stub_index = stub_owned.then(|| {
            let index = effect_cursor;
            effect_cursor += 1;
            index
        });
        let commit_index = effect_cursor;

        if let Some(operation) = mutation.as_mut() {
            for index in [science_index, gateway_index]
                .into_iter()
                .flatten()
            {
                operation
                    .checkpoint_effect_or_attention(
                        index,
                        config_mutation::ConfigMutationEffectState::InProgress,
                        None,
                        "runtime_stop_checkpoint_failed",
                        "before",
                        "unknown",
                        config::ConfigMutationTerminalConfigImage::Before,
                    )
                    .map_err(|error| config_mutation::command_error_string(&error))?;
            }
        }

        if teardown {
            let generation = lifecycle.current_generation();
            let (owner, request) =
                claim_process_local_science_stop(&state, generation, claim_science);
            let execution = request.map(|request| execute_science(&app, request));
            let mut st = lock(&state);
            let science_result = publish_process_local_science_stop(
                &mut st,
                lifecycle.current_generation(),
                owner,
                execution,
            );
            if let Err(error) = science_result {
                if let Some(mut operation) = mutation.take() {
                    if let Some(index) = science_index {
                        let _ = operation.checkpoint_effect(
                            index,
                            config_mutation::ConfigMutationEffectState::Uncertain,
                            Some("science_stop_uncertain"),
                        );
                    }
                    let attention = operation.finish(
                        "attention",
                        "before",
                        "unknown",
                        Some("science_stop_uncertain"),
                        config::ConfigMutationTerminalConfigImage::Before,
                    );
                    if let Err(attention) = attention {
                        return Err(config_mutation::command_error_string(
                            &attention.with_message(if error
                                .to_string()
                                .contains("process-local owner 已变化")
                            {
                                "设置未更改：process-local owner 已变化"
                            } else {
                                "设置未更改：无法安全完成沙箱停止"
                            }),
                        ));
                    }
                }
                return Err(format!(
                    "设置未更改：无法停止仍使用旧端口或旧 SSH 授权的沙箱（{error}）。请手动停止沙箱或重启 app 后重试。（真实实例 8765 未受影响）"
                ));
            }
            if let Some(operation) = mutation.as_mut() {
                if let Some(index) = science_index {
                    operation
                        .checkpoint_effect_or_attention(
                            index,
                            config_mutation::ConfigMutationEffectState::Succeeded,
                            Some("stopped"),
                            "science_stop_checkpoint_failed",
                            "before",
                            "stopped",
                            config::ConfigMutationTerminalConfigImage::Before,
                        )
                        .map_err(|error| config_mutation::command_error_string(&error))?;
                }
            }
            lifecycle.bump_generation();
            if let Err(error) = require_confirmed_gateway_stop(
                stop_gateway(&mut st),
                "设置未更改：Gateway 停止结果未确认",
            ) {
                if let Some(mut operation) = mutation.take() {
                    if let Some(index) = gateway_index {
                        let _ = operation.checkpoint_effect(
                            index,
                            config_mutation::ConfigMutationEffectState::Uncertain,
                            Some("gateway_stop_uncertain"),
                        );
                    }
                    let attention = operation.finish(
                        "attention",
                        "before",
                        "unknown",
                        Some("gateway_stop_uncertain"),
                        config::ConfigMutationTerminalConfigImage::Before,
                    );
                    if let Err(attention) = attention {
                        return Err(config_mutation::command_error_string(
                            &attention.with_message("设置未更改：Gateway 停止结果未确认"),
                        ));
                    }
                }
                return Err(error);
            }
            if let Some(operation) = mutation.as_mut() {
                if let Some(index) = gateway_index {
                    operation
                        .checkpoint_effect_or_attention(
                            index,
                            config_mutation::ConfigMutationEffectState::Succeeded,
                            Some("stopped"),
                            "gateway_stop_checkpoint_failed",
                            "before",
                            "stopped",
                            config::ConfigMutationTerminalConfigImage::Before,
                        )
                        .map_err(|error| config_mutation::command_error_string(&error))?;
                }
            }
        }

        if bridge_owned {
            if let Some(operation) = mutation.as_mut() {
                operation
                    .checkpoint_effect_or_attention(
                        bridge_index.expect("bridge effect index"),
                        config_mutation::ConfigMutationEffectState::InProgress,
                        None,
                        "ssh_bridge_checkpoint_failed",
                        "before",
                        settings_runtime_state(teardown),
                        config::ConfigMutationTerminalConfigImage::Before,
                    )
                    .map_err(|error| config_mutation::command_error_string(&error))?;
            }
            let bridge_after = revoke_science_ssh_bridge_exact(
                &paths.sandbox_home,
                bridge_config_before
                    .as_ref()
                    .expect("bridge config identity preflight"),
                bridge_sidecar_before
                    .as_ref()
                    .expect("bridge sidecar identity preflight"),
            );
            if let Err(error) = bridge_after.as_ref() {
                if let Some(mut operation) = mutation.take() {
                    let _ = operation.checkpoint_effect(
                        bridge_index.expect("bridge effect index"),
                        config_mutation::ConfigMutationEffectState::Uncertain,
                        Some("ssh_bridge_revoke_uncertain"),
                    );
                    let attention = operation.finish(
                        "attention",
                        "before",
                        "unknown",
                        Some("ssh_bridge_revoke_uncertain"),
                        config::ConfigMutationTerminalConfigImage::Before,
                    );
                    if let Err(attention) = attention {
                        return Err(config_mutation::command_error_string(
                            &attention.with_message("撤销隔离 SSH config 失败"),
                        ));
                    }
                }
                return Err(error.clone());
            }
            if let Some(operation) = mutation.as_mut() {
                let bridge_after = bridge_after.expect("checked bridge after-image");
                if let Err(error) = operation.update_receipt(|receipt| {
                    if let Some(plan) = receipt.ssh_plan.as_mut() {
                        if let Some(config) = plan.bridge_config.as_mut() {
                            config.observed_after_identity_or_absent = Some(bridge_after);
                        }
                    }
                }) {
                    return Err(config_mutation::command_error_string(
                        &operation.retain_attention(
                            "ssh_bridge_after_checkpoint_failed",
                            "before",
                            settings_runtime_state(teardown),
                            config::ConfigMutationTerminalConfigImage::Before,
                            error,
                        ),
                    ));
                }
                operation
                    .checkpoint_effect_or_attention(
                        bridge_index.expect("bridge effect index"),
                        config_mutation::ConfigMutationEffectState::Succeeded,
                        Some("absent"),
                        "ssh_bridge_checkpoint_failed",
                        "before",
                        settings_runtime_state(teardown),
                        config::ConfigMutationTerminalConfigImage::Before,
                    )
                    .map_err(|error| config_mutation::command_error_string(&error))?;
            }
        }
        if stub_owned {
            if let Some(operation) = mutation.as_mut() {
                operation
                    .checkpoint_effect_or_attention(
                        stub_index.expect("stub effect index"),
                        config_mutation::ConfigMutationEffectState::InProgress,
                        None,
                        "ssh_stub_checkpoint_failed",
                        "before",
                        settings_runtime_state(teardown),
                        config::ConfigMutationTerminalConfigImage::Before,
                    )
                    .map_err(|error| config_mutation::command_error_string(&error))?;
            }
            if let Err(error) = remove_managed_sandbox_ssh_stub_exact(
                &paths.sandbox_home,
                stub_before.as_ref().expect("stub identity preflight"),
            ) {
                if let Some(mut operation) = mutation.take() {
                    let _ = operation.checkpoint_effect(
                        stub_index.expect("stub effect index"),
                        config_mutation::ConfigMutationEffectState::Uncertain,
                        Some("ssh_stub_remove_uncertain"),
                    );
                    let attention = operation.finish(
                        "attention",
                        "before",
                        "unknown",
                        Some("ssh_stub_remove_uncertain"),
                        config::ConfigMutationTerminalConfigImage::Before,
                    );
                    if let Err(attention) = attention {
                        return Err(config_mutation::command_error_string(
                            &attention.with_message("撤销隔离 SSH config 失败"),
                        ));
                    }
                }
                return Err(error);
            }
            if let Some(operation) = mutation.as_mut() {
                operation
                    .checkpoint_effect_or_attention(
                        stub_index.expect("stub effect index"),
                        config_mutation::ConfigMutationEffectState::Succeeded,
                        Some("absent"),
                        "ssh_stub_checkpoint_failed",
                        "before",
                        settings_runtime_state(teardown),
                        config::ConfigMutationTerminalConfigImage::Before,
                    )
                    .map_err(|error| config_mutation::command_error_string(&error))?;
            }
        }

        if let Some(mut operation) = mutation {
            let before_fingerprint = operation.fence().before_config_fingerprint.clone();
            let next_cfg = cfg;
            operation
                .checkpoint_effect_or_attention(
                    commit_index,
                    config_mutation::ConfigMutationEffectState::InProgress,
                    None,
                    "config_commit_checkpoint_failed",
                    "before",
                    settings_runtime_state(teardown),
                    config::ConfigMutationTerminalConfigImage::Before,
                )
                .map_err(|error| config_mutation::command_error_string(&error))?;
            operation
                .update_config(move |current| {
                    if config::config_mutation_config_fingerprint(current)
                        .map_err(|error| error.to_string())?
                        != before_fingerprint
                    {
                        return Err("Config mutation before-image 在 settings commit 前发生变化".into());
                    }
                    current.proxy_port = next_cfg.proxy_port;
                    current.sandbox_port = next_cfg.sandbox_port;
                    current.reuse_system_ssh = next_cfg.reuse_system_ssh;
                    current.allow_science_host_home = next_cfg.allow_science_host_home;
                    Ok(((), true))
                })
                .map_err(|error| {
                    let attention = operation.finish(
                        "attention",
                        "before",
                        "unknown",
                        Some("config_commit_failed"),
                        config::ConfigMutationTerminalConfigImage::Before,
                    );
                    match attention {
                        Err(attention) => config_mutation::command_error_string(
                            &attention.with_message(if error
                                .contains("test-only config update commit failure")
                            {
                                "test-only config update commit failure"
                            } else {
                                "设置未更改：配置未提交"
                            }),
                        ),
                        Ok(_) => error,
                    }
                })?;
            operation
                .checkpoint_effect_or_attention(
                    commit_index,
                    config_mutation::ConfigMutationEffectState::Succeeded,
                    Some("committed"),
                    "config_commit_checkpoint_failed",
                    "after",
                    settings_runtime_state(teardown),
                    config::ConfigMutationTerminalConfigImage::After,
                )
                .map_err(|error| config_mutation::command_error_string(&error))?;
            let outcome = operation
                .finish(
                    "completed",
                    "after",
                    settings_runtime_state(teardown),
                    None,
                    config::ConfigMutationTerminalConfigImage::After,
                )
                .map_err(|error| config_mutation::command_error_string(&error))?;
            {
                let mut app_state = lock(&state);
                app_state.history_recovery = None;
            }
            crate::clear_boot_attention(&app);
            Ok(config_mutation::outcome_json(&outcome))
        } else {
            let changed = config::update_result(&paths.config_dir, move |current| {
                config::require_no_runtime_transaction(current)?;
                let changed = current.proxy_port != cfg.proxy_port
                    || current.sandbox_port != cfg.sandbox_port
                    || current.reuse_system_ssh != cfg.reuse_system_ssh
                    || current.allow_science_host_home != cfg.allow_science_host_home;
                current.proxy_port = cfg.proxy_port;
                current.sandbox_port = cfg.sandbox_port;
                current.reuse_system_ssh = cfg.reuse_system_ssh;
                current.allow_science_host_home = cfg.allow_science_host_home;
                Ok((changed, changed))
            })
            .map_err(|error| error.to_string())?;
            {
                let mut app_state = lock(&state);
                app_state.history_recovery = None;
            }
            crate::clear_boot_attention(&app);
            Ok(config_mutation::typed_intent_outcome(
                "set_settings",
                if changed { "committed" } else { "no_change" },
                "committed",
                None,
                None,
                Some("not_run"),
                Some(false),
            ))
        }
    })
}

pub(super) async fn stop_all_command(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
) -> Result<(), String> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking(move || stop_all_inner_cmd(app, state, lifecycle)).await
}

pub(super) fn stop_all_inner_cmd<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
) -> Result<(), String> {
    stop_all_inner_with(
        app,
        state,
        lifecycle,
        RuntimeMutationDomain::Destructive,
        ScienceHostAdapter::claim_stop,
        |app, request| ScienceHostAdapter::execute_stop(app, request).into_parts(),
    )
}

#[derive(Clone)]
struct ScienceProcessLocalOwner {
    generation: u64,
    runtime: Option<crate::runtime::science::ScienceRuntimeIdentity>,
    confirmed_stopped: Option<crate::runtime::science::ScienceRuntimeIdentity>,
    sandbox_child_pid: Option<u32>,
    sandbox_port: u16,
    sandbox_url: Option<String>,
}

impl ScienceProcessLocalOwner {
    fn claim(st: &AppState, generation: u64) -> Self {
        Self {
            generation,
            runtime: st.science_runtime.clone(),
            confirmed_stopped: st.science_confirmed_stopped.clone(),
            sandbox_child_pid: st.sandbox.as_ref().map(std::process::Child::id),
            sandbox_port: st.sandbox_port,
            sandbox_url: st.sandbox_url.clone(),
        }
    }

    fn still_owns(&self, st: &AppState, current_generation: u64) -> bool {
        self.generation == current_generation
            && self.runtime == st.science_runtime
            && self.confirmed_stopped == st.science_confirmed_stopped
            && self.sandbox_child_pid == st.sandbox.as_ref().map(std::process::Child::id)
            && self.sandbox_port == st.sandbox_port
            && self.sandbox_url == st.sandbox_url
    }
}

fn claim_process_local_science_stop<Claim>(
    state: &SharedAppState,
    generation: u64,
    claim_science: Claim,
) -> (
    ScienceProcessLocalOwner,
    Result<
        crate::runtime::science::ScienceStopRequest,
        crate::runtime::science::ScienceStopFailure,
    >,
)
where
    Claim: FnOnce(
        Option<&crate::runtime::science::ScienceRuntimeIdentity>,
    ) -> Result<
        crate::runtime::science::ScienceStopRequest,
        crate::runtime::science::ScienceStopFailure,
    >,
{
    let st = lock(state);
    let owner = ScienceProcessLocalOwner::claim(&st, generation);
    let request = claim_science(owner.runtime.as_ref());
    (owner, request)
}

#[allow(clippy::result_large_err)]
fn publish_process_local_science_stop(
    st: &mut AppState,
    current_generation: u64,
    owner: ScienceProcessLocalOwner,
    execution: Result<
        (crate::runtime::science::ScienceStopOutcome, bool),
        crate::runtime::science::ScienceStopFailure,
    >,
) -> crate::runtime::science::ScienceStopOutcome {
    match execution {
        Err(error) => Err(error),
        Ok((_outcome, _clear_tracking)) if !owner.still_owns(st, current_generation) => Err(
            crate::runtime::science::ScienceStopFailure::outcome_publication_failure(
                "Science stop 完成时 process-local owner 已变化；已保留 replacement runtime。",
            ),
        ),
        Ok((outcome, clear_tracking)) => {
            if clear_tracking {
                crate::runtime::system::kill_child(&mut st.sandbox);
                st.sandbox_url = None;
            }
            if let Ok(verified) = outcome.as_ref() {
                st.science_confirmed_stopped = verified.confirmed_runtime().cloned();
                st.science_runtime = None;
            }
            outcome
        }
    }
}

#[allow(clippy::result_large_err)]
pub(crate) fn execute_process_local_science_stop_with<R, Prepare, Claim, Execute, AfterSuccess>(
    app: &tauri::AppHandle<R>,
    state: &SharedAppState,
    lifecycle: &crate::lifecycle::Lifecycle,
    prepare_claim: Prepare,
    claim_science: Claim,
    execute_science: Execute,
    after_success: AfterSuccess,
) -> crate::runtime::science::ScienceStopOutcome
where
    R: tauri::Runtime,
    Prepare: FnOnce(&mut AppState, u64) -> Result<(), crate::runtime::science::ScienceStopFailure>,
    Claim: FnOnce(
        Option<&crate::runtime::science::ScienceRuntimeIdentity>,
    ) -> Result<
        crate::runtime::science::ScienceStopRequest,
        crate::runtime::science::ScienceStopFailure,
    >,
    Execute: FnOnce(
        &tauri::AppHandle<R>,
        crate::runtime::science::ScienceStopRequest,
    ) -> (crate::runtime::science::ScienceStopOutcome, bool),
    AfterSuccess: FnOnce(&mut AppState) -> Result<(), crate::runtime::science::ScienceStopFailure>,
{
    let (owner, request) = {
        let mut st = lock(state);
        let generation = lifecycle.current_generation();
        prepare_claim(&mut st, generation)?;
        let owner = ScienceProcessLocalOwner::claim(&st, generation);
        let request = claim_science(owner.runtime.as_ref());
        (owner, request)
    };
    let execution = request.map(|request| execute_science(app, request));
    let mut st = lock(state);
    let outcome = publish_process_local_science_stop(
        &mut st,
        lifecycle.current_generation(),
        owner,
        execution,
    );
    if outcome.is_ok() {
        after_success(&mut st)?;
    }
    outcome
}

pub(super) fn stop_all_inner_with<R, Claim, Execute>(
    app: tauri::AppHandle<R>,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
    domain: RuntimeMutationDomain,
    claim_science: Claim,
    execute_science: Execute,
) -> Result<(), String>
where
    R: tauri::Runtime,
    Claim: FnOnce(
        Option<&crate::runtime::science::ScienceRuntimeIdentity>,
    ) -> Result<
        crate::runtime::science::ScienceStopRequest,
        crate::runtime::science::ScienceStopFailure,
    >,
    Execute: FnOnce(
        &tauri::AppHandle<R>,
        crate::runtime::science::ScienceStopRequest,
    ) -> (crate::runtime::science::ScienceStopOutcome, bool),
{
    lifecycle.with_mutation(domain, |_| {
        let generation = lifecycle.bump_generation(); // 作废任何在途启动（防被停后又拿旧 key 复活）
        let (owner, request) = claim_process_local_science_stop(&state, generation, claim_science);
        // The stop script and bounded TERM/KILL waits intentionally run without
        // the AppState mutex so high-frequency status can copy its read model.
        let execution = request.map(|request| execute_science(&app, request));
        let mut st = lock(&state);
        let sandbox_res = publish_process_local_science_stop(
            &mut st,
            lifecycle.current_generation(),
            owner,
            execution,
        );
        let gateway_res = st.stop_proxy();
        match (sandbox_res, gateway_res) {
            (Ok(_), crate::GatewayStopOutcome::Stopped) => Ok(()),
            (Err(error), crate::GatewayStopOutcome::Stopped) => {
                Err(format!("代理已停；但{error}真实实例 8765 未受影响。"))
            }
            (Ok(_), crate::GatewayStopOutcome::Uncertain { reason, .. }) => Err(format!(
                "Gateway child 停止结果未确认；应用仍保留 process-local cleanup owner：{reason}"
            )),
            (
                Err(error),
                crate::GatewayStopOutcome::Uncertain { reason, .. },
            ) => Err(format!(
                "Gateway child 停止结果未确认且 Science 停止失败；应用仍保留 process-local cleanup owner：{reason}；{error}"
            )),
        }
    })
}

pub(super) async fn quit_app_command(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
) -> Result<(), String> {
    let exit_app = app.clone();
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    let stopped = run_blocking(move || {
        stop_all_inner_with(
            app,
            state,
            lifecycle,
            RuntimeMutationDomain::Terminal,
            ScienceHostAdapter::claim_stop,
            |app, request| ScienceHostAdapter::execute_stop(app, request).into_parts(),
        )
    })
    .await;
    exit_after_stop_success(stopped, || exit_app.exit(0))
}

pub(super) fn exit_after_stop_success(
    stopped: Result<(), String>,
    exit: impl FnOnce(),
) -> Result<(), String> {
    stopped?;
    exit();
    Ok(())
}

#[cfg(test)]
pub(crate) fn stop_sandbox_state<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    st: &mut AppState,
) -> crate::runtime::science::ScienceStopOutcome {
    let runtime = st.science_runtime.clone();
    let request = crate::runtime::science::ScienceStopRequest::recover(runtime.as_ref());
    let result = ScienceHostAdapter::stop(app, &mut st.sandbox, &mut st.sandbox_url, request);
    if let Ok(verified) = result.as_ref() {
        st.science_confirmed_stopped = verified.confirmed_runtime().cloned();
        st.science_runtime = None;
    }
    result
}

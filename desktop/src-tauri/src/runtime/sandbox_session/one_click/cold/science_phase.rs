use super::*;

#[allow(clippy::result_large_err, clippy::too_many_arguments)]
pub(super) fn run_managed_science_launch_phase<R: Runtime>(
    app: &tauri::AppHandle<R>,
    state: &SharedAppState,
    lifecycle: &lifecycle::Lifecycle,
    auth_proof: Option<&crate::codex_auth_supervisor::CodexAuthReadyProof>,
    trace: &OperationTrace,
    dir: &Path,
    cfg: &config::Config,
    auth_dir: &Path,
    sbx_home: &Path,
    launch: &Path,
    launch_runtime: &mut ScienceRuntimeIdentity,
    login_action: &oauth_forge::LoginAction,
    sport: u16,
    pport: u16,
    secret: &str,
    ssh_hosts: Vec<String>,
    authority_transaction: &mut AuthorityTransaction,
    transaction_identity: &OneClickTransactionIdentity,
    journal_progress: &mut OneClickJournalProgress,
    rollback_context: &mut OneClickRollbackContext,
) -> Result<RegistrationStatus, OneClickFailure> {
    rollback_context.set_kind(OneClickFailureKind::SandboxLaunch);
    one_click_step(
        write_one_click_checkpoint(
            dir,
            transaction_identity,
            journal_progress,
            config::RuntimeTransactionPhase::StartScienceEnvironmentPending,
        ),
        rollback_context,
    )?;
    let installer_bridge = one_click_step(skill_install_bridge_dir(secret), rollback_context)?;
    let installer = match current_skill_install_bridge_key() {
        Ok(installer_key) => {
            register_before_science_start(app, auth_dir, &installer_bridge, &installer_key)
        }
        Err(error) => RegistrationStatus::Warning(error),
    };
    let proxy_url = format!("http://127.0.0.1:{pport}/{secret}");
    let logf = match open_log("sandbox.log") {
        Ok(file) => file,
        Err(error) => return Err(rollback_context.failure(format!("建日志失败：{error}"))),
    };
    {
        use std::io::Write;
        let mut writer = &logf;
        let _ = writeln!(
            writer,
            "[oauth] 虚拟登录已就绪（Rust，零 node；action={:?}；isolated=true）",
            login_action
        );
    }
    let logf2 = one_click_step(logf.try_clone(), rollback_context)?;
    trace.stage(OperationStage::SandboxLaunch, format!("port={sport}"));
    if ScienceHostAdapter::validate_launch_runtime(launch_runtime).is_err() {
        return Err(
            rollback_context.failure("Science runtime 在预检后发生变化；已拒绝启动，请重试")
        );
    }
    one_click_step(
        authority_transaction.validate_science_restore_root(),
        rollback_context,
    )?;
    #[cfg(test)]
    if let Some(foreign_stub) =
        std::env::var_os("CSSWITCH_TEST_SSH_LATE_FOREIGN_STUB").map(std::path::PathBuf::from)
    {
        let parent = match foreign_stub.parent() {
            Some(parent) => parent,
            None => {
                return Err(rollback_context.failure("SSH late-failure test stub has no parent"))
            }
        };
        one_click_step(std::fs::create_dir_all(parent), rollback_context)?;
        one_click_step(
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)),
            rollback_context,
        )?;
        let mut file = one_click_step(
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&foreign_stub),
            rollback_context,
        )?;
        one_click_step(
            std::io::Write::write_all(&mut file, b"foreign-test-stub-must-survive\n"),
            rollback_context,
        )?;
        one_click_step(file.sync_all(), rollback_context)?;
    }
    one_click_step(
        authority_transaction.validate_science_restore_root(),
        rollback_context,
    )?;
    let opaque_bindings = authority_transaction.science_opaque_bindings_env();
    let ssh_hosts = ssh_hosts.join(" ");
    let attempt = match ScienceHostAdapter::spawn_launch(
        ScienceLaunchSpec::one_click(
            launch,
            launch_runtime,
            sport,
            &proxy_url,
            cfg.reuse_system_ssh,
            cfg.allow_science_host_home,
            &ssh_hosts,
            Some(opaque_bindings.as_str()),
            operation::SANDBOX_HEALTH_BUDGET_MS,
            POLL_INTERVAL_MS,
            operation::LOCAL_HEALTH_TIMEOUT_MS,
        ),
        logf,
        logf2,
    ) {
        Ok(attempt) => attempt,
        Err(error) => {
            rollback_context.launch_environment = error.environment();
            if error.kind() == ScienceLaunchFailureKind::WaitFailed {
                rollback_context.candidate_stop_proof = ManagedScienceCandidateStopProof::Unproven;
                return Err(rollback_context.failure(format!(
                    "起沙箱状态未知：{}；code=science_candidate_stop_unproven",
                    error.message()
                )));
            }
            return Err(rollback_context.failure(format!("起沙箱失败：{}", error.message())));
        }
    };
    rollback_context.launch_environment = attempt.environment();
    if let Some(transaction) = rollback_context.ssh_stub_transaction.as_mut() {
        transaction.observe_after_launch(sbx_home);
    }
    let attempt = match ScienceHostAdapter::accept_launch_script(attempt) {
        Ok(attempt) => attempt,
        Err(error) => {
            let tail = redact(&tail_file(&log_path("sandbox.log"), 600), secret);
            let exit_code = error
                .exit_code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "unavailable".into());
            trace.stage(
                OperationStage::SandboxLaunch,
                format!(
                    "outcome=error kind={} exit_code={exit_code}",
                    error.kind().as_str()
                ),
            );
            return Err(rollback_context.failure(format!(
                "起沙箱脚本失败（kind={}；exit_code={exit_code}）：{}\n{tail}",
                error.kind().as_str(),
                error.message()
            )));
        }
    };
    {
        let mut current = lock(state);
        current.sandbox_port = sport;
        current.science_runtime = Some(launch_runtime.clone());
        current.science_confirmed_stopped = None;
    }
    rollback_context.set_kind(OneClickFailureKind::SandboxHealth);
    let healthy = match ScienceHostAdapter::verify_health(attempt) {
        Ok(healthy) => {
            trace.stage(OperationStage::SandboxHealth, "ready");
            healthy
        }
        Err(_) => {
            trace.stage(OperationStage::SandboxHealth, "not_ready");
            let tail = redact(&tail_file(&log_path("sandbox.log"), 600), secret);
            return Err(
                rollback_context.failure(format!("沙箱起后探活超时（端口 {sport}）。\n{tail}"))
            );
        }
    };
    let verified = match ScienceHostAdapter::verify_identity(healthy) {
        Ok(verified) => verified,
        Err(_) => {
            return Err(rollback_context.failure(format!(
                "端口 {sport} 有服务响应，但按 data-dir 确认不是本沙箱 Science（疑似被其它服务占用）。"
            )));
        }
    };
    match ScienceHostAdapter::commit_launch(verified) {
        Ok(receipt) => {
            *launch_runtime = receipt.runtime().clone();
            rollback_context.launch_runtime = launch_runtime.clone();
            rollback_context.launch_token = Some(receipt.ownership().clone());
            lock(state).science_runtime = Some(launch_runtime.clone());
        }
        Err(error) => {
            rollback_context.launch_token = error.ownership().cloned();
            return Err(rollback_context.failure(format!(
                "Science 已启动但受管启动身份无法安全提交：{}",
                error.message()
            )));
        }
    }
    rollback_context.set_kind(OneClickFailureKind::ScienceDbReverify);
    one_click_step(
        write_one_click_checkpoint(
            dir,
            transaction_identity,
            journal_progress,
            config::RuntimeTransactionPhase::WaitScienceDbReverify,
        ),
        rollback_context,
    )?;
    let first_token = match rollback_context.launch_token.clone() {
        Some(token) => token,
        None => {
            return Err(rollback_context.failure("Science DB 检查缺少受管启动身份"));
        }
    };
    match one_click_step(
        wait_for_science_db_reverify(sport, launch_runtime, &first_token),
        rollback_context,
    )? {
        proc::ScienceDbHealth::Ready => {}
        proc::ScienceDbHealth::ReverifyPending => unreachable!(),
        proc::ScienceDbHealth::RestartRequired => {
            one_click_step(
                execute_transaction_science_stop_with(
                    state,
                    lifecycle,
                    TransactionScienceStopTarget::new(
                        TransactionScienceStopBoundary::ManagedDbRestart,
                        launch_runtime,
                        sport,
                    ),
                    || {
                        Ok(ScienceStopRequest::exact(
                            launch_runtime,
                            ScienceStopOwnershipReceipt::from_managed_launch(&first_token),
                        ))
                    },
                    |request| ScienceHostAdapter::execute_stop(app, request).into_parts(),
                    |_state, _confirmed_runtime| {},
                ),
                rollback_context,
            )?;
            rollback_context.launch_confirmed_stopped = true;
            if proc::loopback_port_in_use(sport, operation::LOCAL_HEALTH_TIMEOUT_MS)
                || ScienceHostAdapter::receipt_process_is_alive(&first_token)
                || dir.join("science-managed-launch.v1.json").exists()
            {
                return Err(rollback_context
                    .failure("Science DB recovery 的首次受管进程未完成 verified stop"));
            }
            one_click_step(
                write_one_click_checkpoint(
                    dir,
                    transaction_identity,
                    journal_progress,
                    config::RuntimeTransactionPhase::RestartScienceAfterDbHeal,
                ),
                rollback_context,
            )?;
            let recovery = PriorScienceContext {
                runtime: launch_runtime.clone(),
                port: sport,
                // restart_science_identity_with_budget does not consume the
                // stopped token; retaining it gives compensation an exact
                // absence proof until the fresh receipt is committed.
                launch_token: first_token.clone(),
            };
            if let Err(error) = restart_science_identity_with_budget(
                app,
                state,
                lifecycle,
                auth_proof,
                &recovery.runtime,
                recovery.port,
                science_db_recovery_restart_budget_ms(),
                None,
                None,
            ) {
                rollback_context.candidate_stop_proof = error.candidate_stop_proof;
                return Err(rollback_context.failure(error.to_string()));
            }
            let second_token = one_click_step(
                ScienceHostAdapter::managed_receipt(sport, launch_runtime)
                    .ok_or("Science DB recovery restart 缺少 fresh managed receipt"),
                rollback_context,
            )?;
            rollback_context.launch_token = Some(second_token.clone());
            rollback_context.launch_confirmed_stopped = false;
            one_click_step(
                write_one_click_checkpoint(
                    dir,
                    transaction_identity,
                    journal_progress,
                    config::RuntimeTransactionPhase::VerifyScienceDbAfterRestart,
                ),
                rollback_context,
            )?;
            let second_state = one_click_step(
                wait_for_science_db_reverify(sport, launch_runtime, &second_token),
                rollback_context,
            )?;
            if !ScienceHostAdapter::receipt_is_current(&second_token, launch_runtime)
                || second_state != proc::ScienceDbHealth::Ready
            {
                return Err(rollback_context.failure(format!(
                    "Science DB recovery 的第二次启动未达到 clear/clear：{second_state:?}"
                )));
            }
        }
    }
    Ok(installer)
}

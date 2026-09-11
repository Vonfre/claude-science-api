use super::*;
use crate::runtime::failure::ProjectedRecovery;

fn project_history_replay_failure(error: String) -> serde_json::Value {
    project_one_click_failure(
        TypedOneClickFailure::new(
            OneClickFailureKind::AuthoritySnapshot,
            format!("检测到未完成的 history recovery，安全重放失败并已保留事务：{error}"),
        )
        .with_recovery(ProjectedRecovery::MANUAL_RECOVERY_REQUIRED),
    )
}

fn project_compensation_replay_failure(error: String) -> serde_json::Value {
    project_one_click_failure(
        TypedOneClickFailure::new(
            OneClickFailureKind::AuthoritySnapshot,
            format!("检测到未完成的 durable compensation，安全重放失败并已保留事务：{error}"),
        )
        .with_recovery(ProjectedRecovery::MANUAL_RECOVERY_REQUIRED),
    )
}

fn replay_compensation_before_auth<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &SharedAppState,
    lifecycle: &SharedLifecycle,
) -> Result<bool, String> {
    if !crate::runtime::sandbox_session::interrupted_compensation_requires_pre_auth_replay()? {
        return Ok(false);
    }
    lifecycle.with_mutation(RuntimeMutationDomain::Destructive, |_| {
        crate::runtime::sandbox_session::replay_interrupted_compensation_before_auth(
            app,
            state,
            lifecycle.as_ref(),
        )
    })
}

fn replay_compensation_to_convergence<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &SharedAppState,
    lifecycle: &SharedLifecycle,
) -> Result<bool, String> {
    let mut replayed = false;
    loop {
        match replay_compensation_before_auth(app, state, lifecycle)? {
            true => replayed = true,
            false => return Ok(replayed),
        }
    }
}

fn replay_history_before_auth_if_required(
    state: &SharedAppState,
    lifecycle: &SharedLifecycle,
) -> Result<bool, String> {
    if !crate::runtime::sandbox_session::interrupted_history_recovery_requires_pre_auth_replay()? {
        return Ok(false);
    }
    lifecycle.with_mutation(RuntimeMutationDomain::Destructive, |_| {
        crate::runtime::sandbox_session::replay_interrupted_history_recovery_before_auth(state)
    })
}

fn replay_history_before_auth_error_return(
    state: &SharedAppState,
    lifecycle: &SharedLifecycle,
) -> Result<(), String> {
    lifecycle
        .with_mutation(RuntimeMutationDomain::Destructive, |_| {
            crate::runtime::sandbox_session::replay_interrupted_history_recovery_before_auth(state)
        })
        .map(|_| ())
}

pub(super) async fn one_click_login_command<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    runtime_choice: Option<String>,
) -> Result<serde_json::Value, crate::commands::codex::RuntimeCommandError> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking_typed(move || one_click_login_cmd(app, state, lifecycle, runtime_choice)).await
}

pub(crate) fn one_click_login_cmd<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
    runtime_choice: Option<String>,
) -> Result<serde_json::Value, crate::commands::codex::RuntimeCommandError> {
    // History credential replay is local authority recovery. It must converge
    // before provider-auth preflight so unavailable Codex auth cannot strand a
    // recoverable credential before-image behind an open journal.
    let (entry, prepared) = loop {
        match replay_compensation_to_convergence(&app, &state, &lifecycle) {
            Ok(true) => continue,
            Ok(false) => {}
            Err(error) => return Ok(project_compensation_replay_failure(error)),
        }
        match replay_history_before_auth_if_required(&state, &lifecycle) {
            Ok(true) => continue,
            Ok(false) => {}
            Err(error) => return Ok(project_history_replay_failure(error)),
        }
        let entry = match crate::runtime::sandbox_session::OneClickEntryPreflight::capture(&state) {
            Ok(entry) => entry,
            Err(failure) => return Ok(project_one_click_failure(failure)),
        };
        #[cfg(test)]
        apply_pre_auth_history_race_record()?;
        match replay_compensation_to_convergence(&app, &state, &lifecycle) {
            Ok(true) => continue,
            Ok(false) => {}
            Err(error) => return Ok(project_compensation_replay_failure(error)),
        }
        match replay_history_before_auth_if_required(&state, &lifecycle) {
            Ok(true) => continue,
            Ok(false) => {}
            Err(error) => return Ok(project_history_replay_failure(error)),
        }
        let auth_lease =
            match config::acquire_runtime_compensation_auth_lease(&config::default_dir()) {
                Ok(lease) => lease,
                Err(error) => {
                    return Ok(project_one_click_failure(TypedOneClickFailure::new(
                        OneClickFailureKind::Prepare,
                        format!("provider auth 前无法取得 compensation absence fence：{error}"),
                    )))
                }
            };
        let compensation_raced =
            match crate::runtime::sandbox_session::interrupted_compensation_requires_pre_auth_replay(
            ) {
                Ok(required) => required,
                Err(error) => return Ok(project_compensation_replay_failure(error)),
            };
        let history_raced = match crate::runtime::sandbox_session::
            interrupted_history_recovery_requires_pre_auth_replay()
        {
            Ok(required) => required,
            Err(error) => return Ok(project_history_replay_failure(error)),
        };
        if compensation_raced || history_raced {
            drop(auth_lease);
            continue;
        }
        // Local recovery must remain reachable even when the old account profile is hidden.
        crate::api_only::require_api_profile(&config::default_dir(), None)?;
        let auth_result = crate::commands::codex::prepare_provider_auth(
            &app,
            entry.auth_adapter(),
            crate::commands::codex::CodexPreflightTarget::ActiveProfile,
        );
        drop(auth_lease);
        let prepared = match auth_result {
            Ok(prepared) => prepared,
            Err(crate::commands::codex::RuntimeCommandError::Message(message)) => {
                if let Err(error) = replay_compensation_to_convergence(&app, &state, &lifecycle) {
                    return Ok(project_compensation_replay_failure(error));
                }
                if let Err(error) = replay_history_before_auth_error_return(&state, &lifecycle) {
                    return Ok(project_history_replay_failure(error));
                }
                return Ok(project_one_click_failure(TypedOneClickFailure::new(
                    OneClickFailureKind::AuthPreflight,
                    message,
                )));
            }
            Err(auth @ crate::commands::codex::RuntimeCommandError::Auth(_)) => {
                if let Err(error) = replay_compensation_to_convergence(&app, &state, &lifecycle) {
                    return Ok(project_compensation_replay_failure(error));
                }
                if let Err(error) = replay_history_before_auth_error_return(&state, &lifecycle) {
                    return Ok(project_history_replay_failure(error));
                }
                return Err(auth);
            }
            Err(crate::commands::codex::RuntimeCommandError::Disable(error)) => {
                return Ok(project_one_click_failure(TypedOneClickFailure::new(
                    OneClickFailureKind::Prepare,
                    error.safe_message(),
                )));
            }
            Err(mutation @ crate::commands::codex::RuntimeCommandError::Mutation(_)) => {
                return Err(mutation);
            }
        };
        match replay_compensation_to_convergence(&app, &state, &lifecycle) {
            Ok(true) => continue,
            Ok(false) => {}
            Err(error) => return Ok(project_compensation_replay_failure(error)),
        }
        match replay_history_before_auth_if_required(&state, &lifecycle) {
            Ok(true) => continue,
            Ok(false) => break (entry, prepared),
            Err(error) => return Ok(project_history_replay_failure(error)),
        }
    };
    match lifecycle.with_mutation(
        RuntimeMutationDomain::Destructive,
        |_| -> Result<_, TypedOneClickFailure> {
            entry.verify_unchanged(&state)?;
            if let Some(prepared) = prepared.as_ref() {
                prepared.verify_unchanged().map_err(|message| {
                    TypedOneClickFailure::new(OneClickFailureKind::AuthPreflight, message)
                })?;
            }
            crate::runtime::sandbox_session::one_click_login_entry(
                app,
                state,
                lifecycle.as_ref(),
                runtime_choice.as_deref(),
                prepared.as_ref().map(|prepared| prepared.proof()),
            )
        },
    ) {
        Ok(value) => Ok(value),
        Err(failure) => {
            #[cfg(feature = "acceptance-build")]
            eprintln!(
                "csswitch-acceptance one_click_failure kind={:?} stage={} detail={}",
                failure.kind(),
                failure.coarse_stage(),
                failure.safe_detail.replace(['\r', '\n'], " ")
            );
            Ok(project_one_click_failure(failure))
        }
    }
}

#[cfg(test)]
static PRE_AUTH_HISTORY_RACE_RECORD: std::sync::Mutex<Option<config::RuntimeTransactionRecord>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
pub(super) struct PreAuthHistoryRaceGuard;

#[cfg(test)]
impl Drop for PreAuthHistoryRaceGuard {
    fn drop(&mut self) {
        *PRE_AUTH_HISTORY_RACE_RECORD
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
    }
}

#[cfg(test)]
pub(super) fn test_arm_pre_auth_history_race(
    record: config::RuntimeTransactionRecord,
) -> PreAuthHistoryRaceGuard {
    *PRE_AUTH_HISTORY_RACE_RECORD
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(record);
    PreAuthHistoryRaceGuard
}

#[cfg(test)]
fn apply_pre_auth_history_race_record() -> Result<(), crate::commands::codex::RuntimeCommandError> {
    let record = PRE_AUTH_HISTORY_RACE_RECORD
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    let Some(record) = record else {
        return Ok(());
    };
    config::update(&config::default_dir(), |current| {
        current.runtime_transaction = Some(record);
    })
    .map(|_| ())
    .map_err(|error| crate::commands::codex::RuntimeCommandError::Message(error.to_string()))
}

pub(super) async fn restore_history_choice_command<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    reference: String,
    resume: Option<bool>,
) -> Result<serde_json::Value, crate::commands::codex::RuntimeCommandError> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    let resume = resume.unwrap_or(false);
    let entry = resume
        .then(|| crate::runtime::sandbox_session::OneClickEntryPreflight::capture(&state))
        .transpose()
        .map_err(|failure| {
            crate::commands::codex::RuntimeCommandError::Message(failure.to_string())
        })?;
    let prepared = if let Some(entry) = entry.as_ref() {
        crate::commands::codex::prepare_provider_auth(
            &app,
            entry.auth_adapter(),
            crate::commands::codex::CodexPreflightTarget::ActiveProfile,
        )?
    } else {
        None
    };
    run_blocking_typed(move || {
        lifecycle
            .with_mutation(RuntimeMutationDomain::Destructive, |_| {
                if let Some(entry) = entry.as_ref() {
                    entry
                        .verify_unchanged(&state)
                        .map_err(|failure| failure.to_string())?;
                }
                if let Some(prepared) = prepared.as_ref() {
                    prepared
                        .verify_unchanged()
                        .map_err(|error| error.to_string())?;
                }
                crate::runtime::sandbox_session::restore_history_choice_entry(
                    app,
                    state,
                    lifecycle.as_ref(),
                    &reference,
                    resume,
                    prepared.as_ref().map(|prepared| prepared.proof()),
                )
            })
            .map_err(crate::commands::codex::RuntimeCommandError::Message)
    })
    .await
}

#[cfg(test)]
pub(super) fn test_arm_history_restore_post_stop_config_drift(
) -> crate::runtime::sandbox_session::HistoryRestorePostStopConfigDriftGuard {
    crate::runtime::sandbox_session::test_arm_history_restore_post_stop_config_drift()
}

#[cfg(test)]
pub(super) fn test_arm_history_restore_post_snapshot_config_drift(
) -> crate::runtime::sandbox_session::HistoryRestorePostSnapshotConfigDriftGuard {
    crate::runtime::sandbox_session::test_arm_history_restore_post_snapshot_config_drift()
}

#[cfg(test)]
pub(super) fn test_arm_history_restore_credential_interrupt(
) -> crate::runtime::sandbox_session::HistoryRestoreCredentialInterruptGuard {
    crate::runtime::sandbox_session::test_arm_history_restore_credential_interrupt()
}

#[cfg(test)]
pub(super) fn test_arm_history_replay_interrupt_after_first_restore(
) -> crate::runtime::sandbox_session::HistoryReplayInterruptAfterFirstRestoreGuard {
    crate::runtime::sandbox_session::test_arm_history_replay_interrupt_after_first_restore()
}

pub(super) fn project_one_click_failure(failure: TypedOneClickFailure) -> serde_json::Value {
    let journal_open = config::load_from(&config::default_dir())
        .ok()
        .is_some_and(|cfg| cfg.has_open_runtime_journal());
    failure
        .apply_open_journal_degraded(journal_open)
        .project_dto()
}

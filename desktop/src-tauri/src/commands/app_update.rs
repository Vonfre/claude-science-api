//! App-only signed updates. The UI cannot supply a URL, public key or package.
//! A checked candidate is process-local; installation requires its exact version.
use serde::Serialize;
use std::sync::{
    atomic::{AtomicBool, AtomicU8, Ordering},
    Mutex,
};
use std::time::Duration;
use tauri::{Manager, State};
use tauri_plugin_updater::{Update, UpdaterExt};

const UPDATE_ENDPOINT: &str =
    "https://github.com/Vonfre/claude-science-api/releases/latest/download/latest.json";

#[derive(Default)]
pub(crate) struct AppUpdateState {
    busy: AtomicBool,
    terminal_phase: AtomicU8,
    pending: Mutex<Option<Update>>,
}
struct UpdateLease<'a>(&'a AtomicBool);
impl Drop for UpdateLease<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
// Native exit runs on the event loop, which the macOS privileged installer
// needs for its prompt. Claim this gate before the lifecycle lock on either
// path; the event loop must never wait on an installation-held lifecycle lock.
const IDLE: u8 = 0;
const INSTALLING: u8 = 1;
const EXITING: u8 = 2;
pub(crate) struct InstallationLease<'a>(&'a AtomicU8);
impl Drop for InstallationLease<'_> {
    fn drop(&mut self) {
        self.0.store(IDLE, Ordering::Release);
    }
}
impl AppUpdateState {
    pub(crate) fn claim_installation(&self) -> Result<InstallationLease<'_>, String> {
        self.terminal_phase
            .compare_exchange(IDLE, INSTALLING, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "应用正在退出或安装更新，请稍后重试".to_string())?;
        Ok(InstallationLease(&self.terminal_phase))
    }
    pub(crate) fn with_native_exit(
        &self,
        cleanup: impl FnOnce() -> crate::GatewayStopOutcome,
    ) -> crate::GatewayStopOutcome {
        match self.terminal_phase.compare_exchange(
            IDLE,
            EXITING,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) | Err(EXITING) => {}
            Err(_) => {
                return crate::GatewayStopOutcome::Uncertain {
                    owned_count: 0,
                    reason: "正在安装应用更新，请等待完成后再退出".into(),
                }
            }
        }
        let outcome = cleanup();
        if matches!(outcome, crate::GatewayStopOutcome::Uncertain { .. }) {
            self.terminal_phase.store(IDLE, Ordering::Release);
        }
        outcome
    }

    fn claim(&self) -> Result<UpdateLease<'_>, String> {
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "应用更新正在进行，请稍后重试".to_string())?;
        Ok(UpdateLease(&self.busy))
    }
}

#[derive(Serialize)]
pub(crate) struct AppUpdateStatus {
    status: &'static str,
    current_version: String,
    version: Option<String>,
}

#[tauri::command]
pub(crate) async fn check_app_update(
    app: tauri::AppHandle,
    state: State<'_, AppUpdateState>,
) -> Result<AppUpdateStatus, String> {
    let _lease = state.claim()?;
    let current_version = app.package_info().version.to_string();
    let Some(key) = normalized_public_key(option_env!("SCIPORT_UPDATER_PUBLIC_KEY")) else {
        return Ok(AppUpdateStatus {
            status: "not_configured",
            current_version,
            version: None,
        });
    };
    let update = app
        .updater_builder()
        .pubkey(key)
        .endpoints(vec![UPDATE_ENDPOINT.parse().map_err(|_| "更新地址无效")?])
        .map_err(|_| "更新地址无效")?
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|_| "更新配置无效")?
        .check()
        .await
        .map_err(|_| "无法检查应用更新，请稍后重试")?;
    // A release feed must point back to this project's versioned HTTPS asset.
    // Signature verification remains mandatory even for an allowed URL.
    if let Some(candidate) = &update {
        if !valid_download(candidate) {
            return Err("更新下载地址不属于正式发布".into());
        }
    }
    let version = update.as_ref().map(|candidate| candidate.version.clone());
    *state.pending.lock().map_err(|_| "更新状态不可用")? = update;
    Ok(AppUpdateStatus {
        status: if version.is_some() {
            "available"
        } else {
            "current"
        },
        current_version,
        version,
    })
}

fn normalized_public_key(key: Option<&str>) -> Option<&str> {
    key.map(str::trim).filter(|key| !key.is_empty())
}

fn valid_download(update: &Update) -> bool {
    valid_release_asset(update.download_url.as_str(), &update.version)
}
fn valid_release_asset(url: &str, version: &str) -> bool {
    let parts: Vec<_> = version.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        && url == format!("https://github.com/Vonfre/claude-science-api/releases/download/v{version}/SciPort_{version}_aarch64.app.tar.gz")
}

#[tauri::command]
pub(crate) async fn install_app_update(
    app: tauri::AppHandle,
    state: State<'_, AppUpdateState>,
    expected_version: String,
) -> Result<(), String> {
    let _lease = state.claim()?;
    if cfg!(debug_assertions) {
        return Err("开发构建不能替换应用；请使用正式安装包".into());
    }
    let update = state
        .pending
        .lock()
        .map_err(|_| "更新状态不可用")?
        .as_ref()
        .filter(|update| update.version == expected_version)
        .cloned()
        .ok_or("更新候选已变化，请重新检查")?;
    if !valid_download(&update) {
        return Err("更新下载地址无效".into());
    }
    // Download verifies the detached signature before any runtime teardown or
    // app replacement. Failed checks never stop a healthy Science instance.
    let bytes = update
        .download(|_, _| {}, || {})
        .await
        .map_err(|_| "下载或签名校验失败；未安装更新")?;
    let runtime_state = app.state::<crate::SharedAppState>().inner().clone();
    let lifecycle = app.state::<crate::SharedLifecycle>().inner().clone();
    super::runtime::install_verified_app_update(app, runtime_state, lifecycle, update, bytes).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn app_update_plugin_initializes_with_actual_candidate_configuration() {
        let candidate: tauri::Config =
            serde_json::from_str(include_str!("../../tauri.conf.json")).unwrap();
        let mut context = tauri::test::mock_context(tauri::test::noop_assets());
        context.config_mut().plugins = candidate.plugins;
        let app = tauri::test::mock_builder()
            .plugin(tauri_plugin_updater::Builder::new().build())
            .build(context)
            .expect("the updater must initialize even without a release key");
        let _builder = app.updater_builder();
    }

    #[test]
    fn app_update_public_key_normalization_matches_release_verifier() {
        use base64::{engine::general_purpose::STANDARD, Engine};
        let encoded_fixture = STANDARD.encode("isolated public-key fixture");
        let padded = format!(" \n{encoded_fixture}\r\n");
        let client_key = normalized_public_key(Some(&padded)).unwrap();
        assert_eq!(client_key, padded.trim());
        assert_eq!(
            STANDARD.decode(client_key).unwrap(),
            b"isolated public-key fixture"
        );
        assert_eq!(normalized_public_key(None), None);
        assert_eq!(normalized_public_key(Some(" \n")), None);
    }

    #[test]
    fn app_update_privileged_install_keeps_event_loop_free_for_prompt() {
        use crate::lifecycle::{Lifecycle, RuntimeMutationDomain};
        use std::sync::{mpsc, Arc};
        let updates = Arc::new(AppUpdateState::default());
        let lifecycle = Arc::new(Lifecycle::new());
        let (ready_tx, ready_rx) = mpsc::channel();
        let (prompt_tx, prompt_rx) = mpsc::channel();
        let worker_updates = updates.clone();
        let worker_lifecycle = lifecycle.clone();
        let worker = std::thread::spawn(move || {
            let _installation = worker_updates.claim_installation().unwrap();
            worker_lifecycle.with_mutation(RuntimeMutationDomain::Terminal, |_| {
                ready_tx.send(()).unwrap();
                // Models Update::install waiting for its event-loop privilege prompt.
                prompt_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            });
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let exit = updates.with_native_exit(|| panic!("exit must not enter blocking cleanup"));
        assert!(matches!(exit, crate::GatewayStopOutcome::Uncertain { .. }));
        assert!(lifecycle
            .try_acquire_mutation(RuntimeMutationDomain::Terminal)
            .is_none());
        prompt_tx.send(()).unwrap();
        worker.join().unwrap();
        assert_eq!(
            updates.with_native_exit(|| {
                let _lease = lifecycle
                    .try_acquire_mutation(RuntimeMutationDomain::Terminal)
                    .unwrap();
                crate::GatewayStopOutcome::Stopped
            }),
            crate::GatewayStopOutcome::Stopped
        );
        assert!(
            updates.claim_installation().is_err(),
            "approved exit prevents later installation"
        );
    }

    #[test]
    fn app_update_exit_first_and_uncertain_cleanup_never_race_installation() {
        let updates = AppUpdateState::default();
        let outcome = updates.with_native_exit(|| {
            assert!(updates.claim_installation().is_err());
            crate::GatewayStopOutcome::Uncertain {
                owned_count: 1,
                reason: "fixture".into(),
            }
        });
        assert!(matches!(
            outcome,
            crate::GatewayStopOutcome::Uncertain { .. }
        ));
        let install = updates.claim_installation().unwrap();
        assert!(updates.claim_installation().is_err());
        drop(install);
        assert!(updates.claim_installation().is_ok());
    }

    #[test]
    fn only_exact_stable_project_asset_is_accepted() {
        let good = "https://github.com/Vonfre/claude-science-api/releases/download/v0.9.2/SciPort_0.9.2_aarch64.app.tar.gz";
        assert!(valid_release_asset(good, "0.9.2"));
        for bad in [
            good.replace("https:", "http:"),
            good.replace("Vonfre", "other"),
            format!("{good}?redirect=1"),
            good.replace("v0.9.2/", "latest/"),
            good.replace("github.com/", "github.com.evil/"),
        ] {
            assert!(!valid_release_asset(&bad, "0.9.2"));
        }
        assert!(!valid_release_asset(good, "0.9.3"));
        assert!(!valid_release_asset(good, "0.9.2-beta.1"));
    }
    #[test]
    fn only_one_check_or_install_owns_update_state() {
        let state = AppUpdateState::default();
        let lease = state.claim().unwrap();
        assert!(state.claim().is_err());
        drop(lease);
        assert!(state.claim().is_ok());
    }
}

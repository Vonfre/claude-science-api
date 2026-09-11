use super::{
    config_last_error_json, manual_open_result, project_one_click_failure,
    status_response_for_config_error, status_runtime_identity, status_upstream_applicable,
};
use crate::{
    config::{self, Config, Profile},
    lifecycle, lock,
    runtime::{proxy_lifecycle, sandbox_session, science},
    AppState, SharedAppState,
};
use std::{
    collections::BTreeMap,
    env, fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::{symlink, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tauri::{Listener, Manager};

fn start_internal_gateway<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &SharedAppState,
    lifecycle: &lifecycle::Lifecycle,
    profile: &Profile,
) -> Result<proxy_lifecycle::GatewayReceipt, crate::commands::codex::RuntimeCommandError> {
    let adapter = crate::runtime::provider::resolve_launch_plan(profile)?.adapter;
    let prepared = crate::commands::codex::prepare_provider_auth(
        app,
        &adapter,
        crate::commands::codex::CodexPreflightTarget::Profile(profile.id.clone()),
    )?;
    if let Some(prepared) = prepared.as_ref() {
        prepared.verify_unchanged()?;
    }
    proxy_lifecycle::GatewayController::start_for(
        app,
        state,
        lifecycle,
        profile,
        None,
        None,
        prepared.as_ref().map(|prepared| prepared.proof()),
    )
    .map_err(Into::into)
}

fn start_internal_active_gateway<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &SharedAppState,
    lifecycle: &lifecycle::Lifecycle,
) -> Result<proxy_lifecycle::GatewayReceipt, crate::commands::codex::RuntimeCommandError> {
    let cfg = config::load_from(&config::default_dir()).map_err(|error| error.to_string())?;
    let profile = cfg
        .active_profile()
        .cloned()
        .ok_or("missing active profile")?;
    start_internal_gateway(app, state, lifecycle, &profile)
}

fn assert_complete_gateway_receipt(
    receipt: &proxy_lifecycle::GatewayReceipt,
    profile: &Profile,
    science_runtime: Option<&science::ScienceRuntimeIdentity>,
) {
    let formal = crate::runtime::provider::resolve_launch_plan(profile)
        .unwrap()
        .formal();
    assert_ne!(receipt.port, 0);
    assert!(!receipt.route_secret.is_empty());
    assert_eq!(receipt.health.gateway_kind, "rust");
    assert_eq!(receipt.health.provider, formal.adapter);
    assert_eq!(
        receipt.health.shim_mode,
        crate::runtime::provider::current_shim_mode_for_adapter(&formal.adapter)
    );
    assert!(!receipt.health.launch_id.is_empty());
    assert_eq!(receipt.health.provider_contract_id, formal.contract_id);
    assert_eq!(
        receipt.health.provider_contract_digest,
        formal.contract_digest
    );
    assert_eq!(receipt.health.intent, "formal");
    match receipt.catalog.expected_fingerprint.as_deref() {
        Some(expected) => {
            assert!(!expected.is_empty());
            assert_eq!(receipt.catalog.accepted_fingerprint, expected);
        }
        None => assert!(receipt.catalog.accepted_fingerprint.is_empty()),
    }
    assert_eq!(&receipt.recipe.profile, profile);
    assert_eq!(receipt.recipe.science_runtime.as_ref(), science_runtime);
}

#[test]
fn config_last_error_json_preserves_typed_config_error() {
    let err = config_last_error_json(&"bad config");
    assert_eq!(
        err.get("type").and_then(|v| v.as_str()),
        Some("config_error")
    );
    assert_eq!(
        err.get("message").and_then(|v| v.as_str()),
        Some("bad config")
    );
}

#[test]
fn status_response_for_config_error_is_fail_closed() {
    let v = status_response_for_config_error(&"bad config");
    assert_eq!(v["proxy"], "amber");
    assert_eq!(v["sandbox"], "amber");
    assert_eq!(v["upstream"], "amber");
    assert_eq!(v["active_profile"], serde_json::Value::Null);
    assert_eq!(v["science"]["sandbox"]["port"], 0);
    assert_eq!(v["last_error"]["type"], "config_error");
    assert_eq!(v["last_error"]["message"], "bad config");
}

#[test]
fn upstream_applicability_is_provider_semantics_not_endpoint_parse_success() {
    assert!(!status_upstream_applicable(""));
    assert!(!status_upstream_applicable("codex"));
    assert!(status_upstream_applicable("relay"));
    assert!(status_upstream_applicable("deepseek"));
}

#[test]
fn status_runtime_identity_prefers_launched_identity_and_fail_closes_partial_launch() {
    let (gateway, shim, catalog_shim) =
        status_runtime_identity("deepseek", "", String::new(), String::new());
    assert_eq!(gateway, "rust");
    assert_eq!(shim, "rewrite");
    assert_eq!(catalog_shim, "rewrite");

    let (gateway, shim, catalog_shim) =
        status_runtime_identity("deepseek", "secret-present", "rust".into(), "off".into());
    assert_eq!(gateway, "rust");
    assert_eq!(shim, "off");
    assert_eq!(catalog_shim, "rewrite");

    let (gateway, shim, catalog_shim) =
        status_runtime_identity("deepseek", "secret-present", String::new(), String::new());
    assert_eq!(gateway, "");
    assert_eq!(shim, "");
    assert_eq!(catalog_shim, "rewrite");
}

#[test]
fn science_operation_failures_have_stable_structured_stages() {
    use crate::runtime::failure::{OneClickFailureKind, TypedOneClickFailure};
    use crate::runtime::proxy_lifecycle::{
        InterruptedGatewayRecoveryError, InterruptedGatewayRecoveryErrorKind,
        InterruptedGatewayStopUnknownKind,
    };

    let project = |kind: OneClickFailureKind, message: &str| {
        project_one_click_failure(TypedOneClickFailure::new(kind, message))
    };
    assert_eq!(
        project(OneClickFailureKind::ScienceStop, "停止旧进程失败")["stage"],
        "science_stop"
    );
    assert_eq!(
        project(OneClickFailureKind::ProxyHealth, "代理探活失败")["stage"],
        "gateway_start"
    );
    assert_eq!(
        project(OneClickFailureKind::CatalogVerify, "模型目录不一致")["stage"],
        "catalog_verify"
    );
    assert_eq!(
        project(
            OneClickFailureKind::CatalogVerify,
            "gateway 模型目录探活无响应"
        )["stage"],
        "catalog_verify"
    );
    assert_eq!(
        project(
            OneClickFailureKind::CatalogVerify,
            "Codex published model snapshot 为空或包含非法 alias"
        )["stage"],
        "catalog_verify"
    );
    assert_eq!(
        project(OneClickFailureKind::SandboxHealth, "沙箱起后超时")["stage"],
        "science_start"
    );
    assert_eq!(
        project(
            OneClickFailureKind::ScienceStart,
            "science_api_health_status_401"
        )["stage"],
        "science_start"
    );
    assert_eq!(
        project(
            OneClickFailureKind::ScienceDbReverify,
            "science_db_reverify_timeout"
        )["stage"],
        "science_start"
    );
    assert_eq!(
        project(OneClickFailureKind::Prepare, "配置不可用")["stage"],
        "prepare"
    );
    // Message text must not override kind.
    assert_eq!(
        project(
            OneClickFailureKind::CatalogVerify,
            "代理 gateway 停止旧进程 沙箱"
        )["stage"],
        "catalog_verify"
    );

    let recovery_failure = |kind, message| {
        sandbox_session::typed_interrupted_gateway_recovery_error(
            InterruptedGatewayRecoveryError::new(kind, message),
        )
    };
    let authority_failure = recovery_failure(
        InterruptedGatewayRecoveryErrorKind::AuthoritySnapshot,
        "message deliberately has no authority keyword",
    );
    assert_eq!(
        authority_failure.kind(),
        OneClickFailureKind::AuthoritySnapshot
    );
    let authority_dto = project_one_click_failure(authority_failure);
    assert_eq!(authority_dto["stage"], "science_start");
    assert_eq!(authority_dto["recovery_status"], "manual_recovery_required");
    assert_eq!(authority_dto["environment_status"], "not_exposed");
    let not_managed_failure = recovery_failure(
        InterruptedGatewayRecoveryErrorKind::NotManaged,
        "Science authority/environment authority 快照 Science 环境暴露",
    );
    assert_eq!(
        not_managed_failure.kind(),
        OneClickFailureKind::GatewayStart
    );
    let not_managed_dto = project_one_click_failure(not_managed_failure);
    assert_eq!(not_managed_dto["stage"], "gateway_start");
    assert_eq!(not_managed_dto["recovery_status"], "degraded");
    assert_eq!(not_managed_dto["environment_status"], "not_exposed");
    let stop_unknown_failure = recovery_failure(
        InterruptedGatewayRecoveryErrorKind::StopUnknown(
            InterruptedGatewayStopUnknownKind::SignalFailed,
        ),
        "authority 快照",
    );
    assert_eq!(
        stop_unknown_failure.kind(),
        OneClickFailureKind::GatewayStart
    );
    let stop_unknown_dto = project_one_click_failure(stop_unknown_failure);
    assert_eq!(stop_unknown_dto["stage"], "gateway_start");
    assert_eq!(stop_unknown_dto["recovery_status"], "degraded");
    assert_eq!(stop_unknown_dto["environment_status"], "not_exposed");
}

#[test]
fn manual_browser_failure_returns_the_same_fresh_url_for_copy_and_retry() {
    let url = "http://127.0.0.1:8990/?nonce=fresh".to_string();
    let failed = manual_open_result(url.clone(), Err("opener rejected".into()));
    assert_eq!(failed["status"], "error");
    assert_eq!(failed["fallback_url"], url);
    assert!(failed["message"]
        .as_str()
        .unwrap()
        .contains("打开浏览器失败"));

    let opened = manual_open_result(url, Ok(()));
    assert_eq!(opened["status"], "ok");
    assert!(opened["fallback_url"].is_null());
}

struct EnvGuard {
    saved: Vec<(String, Option<std::ffi::OsString>)>,
}

impl EnvGuard {
    fn new() -> Self {
        Self { saved: Vec::new() }
    }

    fn set(&mut self, key: &str, value: impl AsRef<std::ffi::OsStr>) {
        self.saved.push((key.to_string(), env::var_os(key)));
        env::set_var(key, value);
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.saved.iter().rev() {
            match value {
                Some(v) => env::set_var(key, v),
                None => env::remove_var(key),
            }
        }
    }
}

fn tmpdir(label: &str) -> PathBuf {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = env::temp_dir().join(format!("csswitch-{label}-{}-{now}", std::process::id()));
    fs::create_dir_all(&path).unwrap();
    path.canonicalize().unwrap()
}

static IGNORED_RUNTIME_CHARACTERIZATION_LOCK: Mutex<()> = Mutex::new(());

fn run_exact_ignored_runtime_characterization(test_name: &str, environment: &[(&str, &str)]) {
    let _characterization_guard = IGNORED_RUNTIME_CHARACTERIZATION_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .arg("--exact")
        .arg(test_name)
        .arg("--ignored")
        .arg("--nocapture");
    for (key, value) in environment {
        command.env(key, value);
    }
    let output = command.output().unwrap();
    let exact_test_completed = exact_child_test_completed(test_name, &output);
    assert!(
        output.status.success() && exact_test_completed,
        "isolated characterization {test_name} failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn exact_child_test_completed(test_name: &str, output: &std::process::Output) -> bool {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let exact_result = format!("test {test_name} ... ok");
    stdout.lines().any(|line| line == "running 1 test")
        && stdout.lines().any(|line| line == exact_result)
        && stdout.lines().any(|line| {
            line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured;")
        })
}

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    assert_ne!(port, 8765);
    port
}

struct SshFixturePortReservations {
    proxy_port: u16,
    sandbox_port: u16,
    proxy: Option<TcpListener>,
    sandbox: Option<TcpListener>,
    preview: Option<TcpListener>,
}

impl SshFixturePortReservations {
    fn release_sandbox(&mut self) {
        self.sandbox.take();
    }

    fn release_proxy(&mut self) {
        self.proxy.take();
    }

    fn release_one_click_ports(&mut self) {
        self.proxy.take();
        self.sandbox.take();
        self.preview.take();
    }
}

fn reserve_ssh_fixture_ports() -> SshFixturePortReservations {
    for _ in 0..128 {
        let proxy = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let sandbox = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let proxy_port = proxy.local_addr().unwrap().port();
        let sandbox_port = sandbox.local_addr().unwrap().port();
        if proxy_port == 8765 || matches!(sandbox_port, 8764 | 8765 | 65535) {
            continue;
        }
        let preview_port = sandbox_port + 1;
        if preview_port == 8765 || proxy_port == sandbox_port || proxy_port == preview_port {
            continue;
        }
        if let Ok(preview) = TcpListener::bind(("127.0.0.1", preview_port)) {
            return SshFixturePortReservations {
                proxy_port,
                sandbox_port,
                proxy: Some(proxy),
                sandbox: Some(sandbox),
                preview: Some(preview),
            };
        }
    }
    panic!("could not allocate a safe pairwise-distinct proxy/sandbox/preview port set");
}

fn ssh_fixture_ports() -> (u16, u16) {
    let reservation = reserve_ssh_fixture_ports();
    (reservation.proxy_port, reservation.sandbox_port)
}

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn write_padded_test_updater(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    write_executable(path, body);
    let mut updater = fs::OpenOptions::new().append(true).open(path).unwrap();
    updater.write_all(b"\n#").unwrap();
    updater.write_all(&vec![b' '; 1024 * 1024]).unwrap();
    updater.sync_all().unwrap();
}

pub(crate) fn write_test_bins(dir: &Path) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    write_executable(
        &dir.join("open"),
        r#"#!/bin/sh
if [ -n "${CSSWITCH_FAKE_OPEN_LOG:-}" ]; then
  printf '%s\n' "$*" >> "$CSSWITCH_FAKE_OPEN_LOG"
fi
if [ -n "${CSSWITCH_FAKE_OPEN_FAIL_ONCE_FILE:-}" ] && [ ! -e "$CSSWITCH_FAKE_OPEN_FAIL_ONCE_FILE" ]; then
  : > "$CSSWITCH_FAKE_OPEN_FAIL_ONCE_FILE"
  exit 1
fi
exit 0
"#,
    );
    write_executable(
        &dir.join("security"),
        r#"#!/bin/sh
exit 0
"#,
    );
    let science_bin = dir.join("claude-science");
    write_executable(
        &science_bin,
        r#"#!/bin/sh
set -eu
cmd="${1:-}"
if [ "$#" -gt 0 ]; then shift; fi
if [ "$cmd" = "--version" ]; then
  if [ -n "${CSSWITCH_FAKE_SCIENCE_CALL_LOG:-}" ]; then
    printf '%s\n' "$cmd" >> "$CSSWITCH_FAKE_SCIENCE_CALL_LOG"
  fi
  echo "claude-science 0.0.0-csswitch-test"
  exit 0
fi
data_dir=""
port=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --data-dir) data_dir="$2"; shift 2 ;;
    --port) port="$2"; shift 2 ;;
    *) shift ;;
  esac
done
state="$data_dir/fake-science"
mkdir -p "$state"
call_log="${CSSWITCH_FAKE_SCIENCE_CALL_LOG:-}"
if [ -z "$call_log" ] && [ -f "$state/call-log-path" ]; then
  call_log="$(cat "$state/call-log-path")"
fi
if [ -n "$call_log" ]; then
  printf '%s\n' "$cmd" >> "$call_log"
fi
case "$cmd" in
  serve)
    count="$(cat "$state/serve-count" 2>/dev/null || echo 0)"
    count=$((count + 1))
    printf '%s' "$count" > "$state/serve-count"
    failure_control="$state/fail-after-inplace-mutation"
    if [ -f "$failure_control" ]; then
      mutation_file="$(/usr/bin/sed -n '1p' "$failure_control")"
      failure_marker="$(/usr/bin/sed -n '2p' "$failure_control")"
      printf '%s\n' "mutated-by-failing-science" > "$mutation_file"
      chmod 0644 "$mutation_file"
      if [ -n "$failure_marker" ]; then
        printf '%s\n' "serve-mutated-before-exit" > "$failure_marker"
      fi
      exit 23
    fi
    unbound_pid_path="${CSSWITCH_FAKE_SCIENCE_UNBOUND_PID:-}"
    unbound_mutation_path="${CSSWITCH_FAKE_SCIENCE_UNBOUND_MUTATION:-}"
    [ -n "$unbound_pid_path" ] || unbound_pid_path="$(cat "$state/unbound-pid-path" 2>/dev/null || true)"
    [ -n "$unbound_mutation_path" ] || unbound_mutation_path="$(cat "$state/unbound-mutation-path" 2>/dev/null || true)"
    if [ "$count" -eq 2 ] && { [ -n "${CSSWITCH_FAKE_SCIENCE_SECOND_BOOT_BLOCKS:-}" ] || [ -f "$state/second-boot-blocks" ]; }; then
      trap '' HUP
      printf '%s\n' "mutated-by-blocked-second-candidate" > "$unbound_mutation_path"
      printf '%s' "$$" > "$unbound_pid_path"
      while :; do sleep 60; done
    fi
    if [ "$count" -eq 2 ] && { [ -n "${CSSWITCH_FAKE_SCIENCE_SECOND_BOOT_NO_LISTENER:-}" ] || [ -f "$state/second-boot-no-listener" ]; }; then
      python3 - "$unbound_pid_path" "$unbound_mutation_path" >/dev/null 2>&1 <<'PY' &
import os
import sys
import time
pidfile = sys.argv[1]
mutation = sys.argv[2]
with open(mutation, "w", encoding="utf-8") as f:
    f.write("mutated-by-unbound-second-candidate\n")
with open(pidfile, "w", encoding="utf-8") as f:
    f.write(str(os.getpid()))
while True:
    time.sleep(60)
PY
      for _ in 1 2 3 4 5 6 7 8 9 10; do
        [ -s "$unbound_pid_path" ] && break
        sleep 0.02
      done
      exit 0
    fi
    db_health="${CSSWITCH_FAKE_SCIENCE_DB_HEALTH:-}"
    [ -n "$db_health" ] || db_health="$(cat "$state/db-health" 2>/dev/null || echo clear)"
    printf '%s' "$port" > "$state/port"
    python3 - "$port" "$state/pid" "$data_dir" "$count" "$db_health" >/dev/null 2>&1 <<'PY' &
import http.server
import os
import socketserver
import sys
import time
import urllib.parse
port = int(sys.argv[1])
pidfile = sys.argv[2]
data_dir = sys.argv[3]
generation = sys.argv[4]
db_health = sys.argv[5]
origin = f"http://127.0.0.1:{port}"
auth_cookie = generation.zfill(64)
verdict = os.path.join(data_dir, "fake-db-damage-verdict")
boot_skipped = db_health == "stateful" and os.path.exists(verdict)
class Handler(http.server.BaseHTTPRequestHandler):
    health_polls = 0
    def log_message(self, *args):
        pass
    def reject_auth(self):
        self.send_response(401)
        self.send_header("content-type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"detail":"invalid bearer token"}')
    def do_POST(self):
        if self.path != "/api/auth/nonce":
            self.send_response(404)
            self.end_headers()
            return
        if self.headers.get("Origin") != origin:
            self.reject_auth()
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            self.reject_auth()
            return
        if length <= 0 or length > 4096:
            self.reject_auth()
            return
        form = urllib.parse.parse_qs(
            self.rfile.read(length).decode("ascii"),
            keep_blank_values=True,
        )
        try:
            with open(os.path.join(data_dir, "fake-science", "url-nonce"), encoding="utf-8") as f:
                expected_nonce = f.read().strip()
        except OSError:
            self.reject_auth()
            return
        if form.get("nonce") != [expected_nonce] or form.get("dest") != ["/"]:
            self.reject_auth()
            return
        self.send_response(200)
        self.send_header(
            "Set-Cookie",
            f"operon_auth={auth_cookie}; Path=/; HttpOnly; SameSite=Strict",
        )
        self.send_header("content-type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"ok":true}')
    def do_GET(self):
        if self.path.startswith("/api/health"):
            cookies = {}
            for item in self.headers.get("Cookie", "").split(";"):
                if "=" in item:
                    name, value = item.split("=", 1)
                    cookies[name.strip()] = value.strip()
            if self.headers.get("Origin") != origin or cookies.get("operon_auth") != auth_cookie:
                self.reject_auth()
                return
            if db_health == "stalled":
                time.sleep(2)
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.end_headers()
            Handler.health_polls += 1
            if boot_skipped:
                if Handler.health_polls == 1:
                    self.wfile.write(b'{"db_corruption":{"flagged":true,"kind":"damage"},"db_migrations_skipped":true}')
                    return
                try:
                    os.unlink(verdict)
                except FileNotFoundError:
                    pass
                self.wfile.write(b'{"db_corruption":{"flagged":false,"kind":null},"db_migrations_skipped":true}')
            elif db_health == "skipped":
                self.wfile.write(b'{"db_corruption":{"flagged":false,"kind":null},"db_migrations_skipped":true}')
            elif db_health == "damage-reverify":
                self.wfile.write(b'{"db_corruption":{"flagged":true,"kind":"damage"},"db_migrations_skipped":true}')
            elif db_health == "damage":
                self.wfile.write(b'{"db_corruption":{"flagged":true,"kind":"damage"},"db_migrations_skipped":false}')
            elif db_health == "io-errors":
                self.wfile.write(b'{"db_corruption":{"flagged":true,"kind":"io_errors"},"db_migrations_skipped":false}')
            elif db_health == "missing-kind":
                self.wfile.write(b'{"db_corruption":{"flagged":true},"db_migrations_skipped":false}')
            else:
                self.wfile.write(b'{"db_corruption":{"flagged":false,"kind":null},"db_migrations_skipped":false}')
        elif self.path.startswith("/health"):
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b'{"status":"ok"}')
        else:
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b"fake science")
socketserver.TCPServer.allow_reuse_address = True
with open(pidfile, "w", encoding="utf-8") as f:
    f.write(str(os.getpid()))
with socketserver.TCPServer(("127.0.0.1", port), Handler) as httpd:
    httpd.serve_forever()
PY
    exit 0
    ;;
  status)
    pid="$(cat "$state/pid" 2>/dev/null || true)"
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
      echo '{"running":true}'
    else
      echo '{"running":false}'
      exit 1
    fi
    ;;
  url)
    p="$(cat "$state/port")"
    count="$(cat "$state/url-count" 2>/dev/null || echo 0)"
    count=$((count + 1))
    printf '%s' "$count" > "$state/url-count"
    nonce="$(printf '%064x' "$count")"
    printf '%s' "$nonce" > "$state/url-nonce"
    echo "http://127.0.0.1:$p/?nonce=$nonce"
    ;;
  stop)
    if [ -n "${CSSWITCH_TEST_PORT_OBSERVATION_SEQUENCE_DIR:-}" ]; then
      mkdir -p "$CSSWITCH_TEST_PORT_OBSERVATION_SEQUENCE_DIR"
      : > "$CSSWITCH_TEST_PORT_OBSERVATION_SEQUENCE_DIR/armed"
      echo "stopped-with-false-then-true-port-observation"
      exit 0
    fi
    if [ -n "${CSSWITCH_TEST_PROCESS_START_DRIFT_MARKER:-}" ]; then
      : > "$CSSWITCH_TEST_PROCESS_START_DRIFT_MARKER"
      echo "stopped-with-identity-drift"
      exit 0
    fi
    pid="$(cat "$state/pid" 2>/dev/null || true)"
    if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || true; fi
    rm -f "$state/pid"
    echo "stopped"
    ;;
  *)
    echo "unsupported fake science command: $cmd" >&2
    exit 2
    ;;
esac
"#,
    );
    science_bin
}

struct MockUpstream {
    port: u16,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Drop for MockUpstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct RuntimeSmokeCleanup<R: tauri::Runtime> {
    handle: tauri::AppHandle<R>,
    state: SharedAppState,
    temp: PathBuf,
    sandbox_port: u16,
    proxy_port: u16,
    armed: bool,
}

impl<R: tauri::Runtime> RuntimeSmokeCleanup<R> {
    fn new(
        handle: tauri::AppHandle<R>,
        state: SharedAppState,
        temp: PathBuf,
        sandbox_port: u16,
        proxy_port: u16,
    ) -> Self {
        Self {
            handle,
            state,
            temp,
            sandbox_port,
            proxy_port,
            armed: true,
        }
    }

    fn cleanup(&mut self) -> Result<(), String> {
        if !self.armed {
            return Ok(());
        }
        self.armed = false;
        let stop_result = {
            let mut st = lock(&self.state);
            let AppState {
                sandbox,
                sandbox_url,
                science_runtime,
                ..
            } = &mut *st;
            let runtime = science_runtime.clone();
            let result = science::stop_sandbox(
                &self.handle,
                sandbox,
                sandbox_url,
                science::ScienceStopRequest::recover(runtime.as_ref()),
            );
            let _ = st.stop_proxy();
            result
        };
        stop_result.map_err(|error| error.to_string())?;
        for (label, port) in [("Science", self.sandbox_port), ("Gateway", self.proxy_port)] {
            let mut closed = false;
            for _ in 0..50 {
                if TcpStream::connect(("127.0.0.1", port)).is_err() {
                    closed = true;
                    break;
                }
                thread::sleep(std::time::Duration::from_millis(100));
            }
            if !closed {
                return Err(format!(
                    "{label} fixture port {port} remained reachable; preserved {}",
                    self.temp.display()
                ));
            }
        }
        let managed_launch = self
            .temp
            .join("home")
            .join(config::CONFIG_DIR_NAME)
            .join("science-managed-launch.v1.json");
        if managed_launch.exists() {
            return Err(format!(
                "confirmed Science stop left managed launch receipt {}; preserved {}",
                managed_launch.display(),
                self.temp.display()
            ));
        }
        fs::remove_dir_all(&self.temp)
            .map_err(|error| format!("failed to remove {}: {error}", self.temp.display()))
    }

    fn finish(mut self) -> Result<(), String> {
        self.cleanup()
    }
}

impl<R: tauri::Runtime> Drop for RuntimeSmokeCleanup<R> {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            eprintln!("isolated runtime cleanup failed: {error}");
        }
    }
}

fn start_mock_upstream() -> MockUpstream {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    assert_ne!(port, 8765);
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let worker = thread::spawn(move || {
        while !worker_stop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut buf = [0; 512];
                    let _ = stream.read(&mut buf);
                    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK");
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });
    MockUpstream {
        port,
        stop,
        worker: Some(worker),
    }
}

fn wait_http_health(port: u16) {
    for _ in 0..50 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("mock service on port {port} did not become reachable");
}

fn wait_http_unreachable(port: u16) {
    for _ in 0..50 {
        if TcpStream::connect(("127.0.0.1", port)).is_err() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("mock service on port {port} remained reachable");
}

fn force_stop_isolated_listener(port: u16) {
    let Some(pid) = listener_pid_if_unique(port) else {
        return;
    };
    assert!(pid > 1, "isolated listener PID must be signal-safe");
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    for _ in 0..50 {
        if TcpStream::connect(("127.0.0.1", port)).is_err() {
            return;
        }
        thread::sleep(std::time::Duration::from_millis(20));
    }
    if listener_pid_if_unique(port) == Some(pid) {
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
    wait_http_unreachable(port);
}

fn force_cleanup_isolated_fixture(
    state: &SharedAppState,
    temp: &Path,
    sandbox_port: u16,
    proxy_port: u16,
) {
    if let Some(mut proxy) = lock(state).proxy.take() {
        let _ = proxy.kill();
        let _ = proxy.wait();
    }
    force_stop_isolated_listener(sandbox_port);
    force_stop_isolated_listener(proxy_port);
    if temp.exists() {
        fs::remove_dir_all(temp).unwrap();
    }
}

fn start_managed_fake_science(
    fake_science: &Path,
    sandbox_home: &Path,
    science_data: &Path,
    sandbox_port: u16,
    runtime: &science::ScienceRuntimeIdentity,
) -> u32 {
    let status = std::process::Command::new(fake_science)
        .arg("serve")
        .arg("--port")
        .arg(sandbox_port.to_string())
        .arg("--data-dir")
        .arg(science_data)
        .env("HOME", sandbox_home)
        .status()
        .unwrap();
    assert!(status.success(), "managed fake Science must launch");
    wait_http_health(sandbox_port);
    let listener_pid = unique_listener_pid(sandbox_port);
    science::record_managed_science_launch(sandbox_port, runtime)
        .expect("managed fake Science must commit its initial receipt");
    listener_pid
}

fn call_count(path: &Path, command: &str) -> usize {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| *line == command)
        .count()
}

fn invoke_json<W: AsRef<tauri::Webview<tauri::test::MockRuntime>>>(
    webview: &W,
    command: &str,
    body: serde_json::Value,
) -> Result<serde_json::Value, String> {
    tauri::test::get_ipc_response(
        webview,
        tauri::webview::InvokeRequest {
            cmd: command.into(),
            callback: tauri::ipc::CallbackFn(0),
            error: tauri::ipc::CallbackFn(1),
            url: "tauri://localhost".parse().unwrap(),
            body: tauri::ipc::InvokeBody::Json(body),
            headers: Default::default(),
            invoke_key: tauri::test::INVOKE_KEY.into(),
        },
    )
    .and_then(|body| {
        body.deserialize::<serde_json::Value>()
            .map_err(|error| serde_json::Value::String(error.to_string()))
    })
    .map_err(|error| {
        error.as_str().map(str::to_string).unwrap_or_else(|| {
            let code = error
                .get("code")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            let cause = error
                .get("cause")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("none");
            format!("typed IPC error code={code} cause={cause}")
        })
    })
}

fn listener_pid_if_unique(port: u16) -> Option<u32> {
    let output = std::process::Command::new("/usr/sbin/lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"])
        .output()
        .expect("lsof should inspect the isolated listener");
    if !output.status.success() {
        return None;
    }
    let pids: Vec<u32> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::trim)
        .filter(|pid| !pid.is_empty())
        .map(|pid| pid.parse().unwrap())
        .collect();
    (pids.len() == 1).then(|| pids[0])
}

fn unique_listener_pid(port: u16) -> u32 {
    listener_pid_if_unique(port).expect("isolated port must have one listener")
}

fn process_start_identity_if_alive(pid: u32) -> Option<String> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .env_clear()
        .output()
        .expect("ps should inspect the isolated process");
    if !output.status.success() {
        return None;
    }
    assert!(output.stderr.is_empty());
    let identity = String::from_utf8(output.stdout).unwrap();
    let identity = identity.trim();
    (!identity.is_empty()).then(|| identity.to_string())
}

fn process_start_identity(pid: u32) -> String {
    process_start_identity_if_alive(pid).expect("isolated process must remain alive")
}

fn ssh_fixture_config(mock_upstream_port: u16, proxy_port: u16, sandbox_port: u16) -> Config {
    let route_id = "claude-csswitch-relay-ssh-transaction-0123456789ab";
    let profile = Profile {
        id: "ssh-transaction".into(),
        name: "SSH transaction fixture".into(),
        template_id: "custom".into(),
        category: "custom".into(),
        api_format: "anthropic".into(),
        base_url: format!("http://127.0.0.1:{mock_upstream_port}/anthropic"),
        api_key: "ssh-transaction-fake-key-never-log".into(),
        model: "mock-model".into(),
        model_catalog: vec![crate::model_catalog::ModelRoute {
            selector_id: route_id.into(),
            display_name: "Mock model".into(),
            upstream_model: "mock-model".into(),
            supports_tools: Some(true),
            ..Default::default()
        }],
        default_model_route_id: route_id.into(),
        role_bindings: crate::model_catalog::RoleBindings {
            sonnet: route_id.into(),
            opus: route_id.into(),
            haiku: route_id.into(),
            fable: route_id.into(),
            ..Default::default()
        },
        ..Default::default()
    };
    Config {
        profiles: vec![profile],
        active_id: "ssh-transaction".into(),
        proxy_port,
        sandbox_port,
        reuse_system_ssh: true,
        ..Default::default()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AuthorityTreeEntry {
    kind: &'static str,
    mode: u32,
    bytes: Vec<u8>,
}

fn authority_tree(root: &Path) -> BTreeMap<PathBuf, AuthorityTreeEntry> {
    fn walk(root: &Path, current: &Path, result: &mut BTreeMap<PathBuf, AuthorityTreeEntry>) {
        let metadata = match fs::symlink_metadata(current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                panic!(
                    "authority snapshot failed for {}: {error}",
                    current.display()
                )
            }
        };
        let relative = current.strip_prefix(root).unwrap().to_path_buf();
        if metadata.file_type().is_symlink() {
            result.insert(
                relative,
                AuthorityTreeEntry {
                    kind: "symlink",
                    mode: metadata.permissions().mode() & 0o777,
                    bytes: fs::read_link(current)
                        .unwrap()
                        .as_os_str()
                        .as_encoded_bytes()
                        .to_vec(),
                },
            );
            return;
        }
        if metadata.is_file() {
            result.insert(
                relative,
                AuthorityTreeEntry {
                    kind: "file",
                    mode: metadata.permissions().mode() & 0o777,
                    bytes: fs::read(current).unwrap(),
                },
            );
            return;
        }
        assert!(
            metadata.is_dir(),
            "authority fixture contains a special file"
        );
        result.insert(
            relative,
            AuthorityTreeEntry {
                kind: "dir",
                mode: metadata.permissions().mode() & 0o777,
                bytes: Vec::new(),
            },
        );
        let mut children = fs::read_dir(current)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        children.sort();
        for child in children {
            walk(root, &child, result);
        }
    }

    let mut result = BTreeMap::new();
    walk(root, root, &mut result);
    result
}

fn science_authority_projection(root: &Path) -> BTreeMap<PathBuf, AuthorityTreeEntry> {
    let mut result = BTreeMap::new();
    for entry in crate::runtime::sandbox_session::SCIENCE_PROTECTED_AUTHORITY_ENTRIES {
        for (relative, value) in authority_tree(&root.join(entry)) {
            let projected = if relative.as_os_str().is_empty() {
                PathBuf::from(entry)
            } else {
                PathBuf::from(entry).join(relative)
            };
            result.insert(projected, value);
        }
    }
    result
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AppAuthorityProjection {
    proxy_present: bool,
    proxy_port: u16,
    secret: String,
    provider: String,
    gateway_kind: String,
    shim_mode: String,
    launch_id: String,
    key_fp: u64,
    sandbox_present: bool,
    sandbox_port: u16,
    sandbox_url: Option<String>,
    science_runtime: Option<science::ScienceRuntimeIdentity>,
    science_confirmed_stopped: Option<science::ScienceRuntimeIdentity>,
    history_recovery_present: bool,
}

fn app_authority_projection(state: &SharedAppState) -> AppAuthorityProjection {
    let state = lock(state);
    AppAuthorityProjection {
        proxy_present: state.proxy.is_some(),
        proxy_port: state.proxy_port,
        secret: state.secret.clone(),
        provider: state.provider.clone(),
        gateway_kind: state.gateway_kind.clone(),
        shim_mode: state.shim_mode.clone(),
        launch_id: state.launch_id.clone(),
        key_fp: state.key_fp,
        sandbox_present: state.sandbox.is_some(),
        sandbox_port: state.sandbox_port,
        sandbox_url: state.sandbox_url.clone(),
        science_runtime: state.science_runtime.clone(),
        science_confirmed_stopped: state.science_confirmed_stopped.clone(),
        history_recovery_present: state.history_recovery.is_some(),
    }
}

#[test]
#[ignore = "explicit Acceptance-boundary SSH transaction smoke; temp HOME, fake OAuth, fake Science, and loopback only"]
fn isolated_ssh_prevalidation_precedes_oauth_mutation() {
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let requested_case = env::var("CSSWITCH_TEST_SSH_PREVALIDATION_CASE").unwrap_or_default();
    let cases = [
        ("missing-system-config", "无法读取已授权的 SSH config"),
        ("no-concrete-host", "没有可枚举的具体 Host alias"),
        (
            "oversized-system-config",
            "SSH config 不是普通文件或超过安全大小上限",
        ),
        (
            "foreign-sandbox-stub",
            "隔离 SSH config 不是 CSSwitch 管理的安全入口",
        ),
        (
            "symlink-sandbox-stub",
            "隔离 SSH config 不是 CSSwitch 管理的安全入口",
        ),
        (
            "conflicting-science-bridge",
            "隔离 Science ssh_hosts 在授权期间被外部修改",
        ),
        (
            "group-writable-science-authority",
            "隔离 Science SSH authority 不可写",
        ),
        (
            "other-writable-science-authority",
            "隔离 Science SSH authority 不可写",
        ),
        (
            "missing-packaged-wrapper",
            "打包的 CSSwitch SSH bridge 缺失",
        ),
        (
            "unsafe-packaged-wrapper",
            "打包的 CSSwitch SSH bridge 不是安全的可执行文件",
        ),
    ];
    assert!(
        requested_case.is_empty() || cases.iter().any(|(case, _)| *case == requested_case),
        "unknown SSH prevalidation case selector: {requested_case}"
    );
    let mut executed_cases = 0;
    for (case, expected_error) in cases {
        if !requested_case.is_empty() && requested_case != case {
            continue;
        }
        executed_cases += 1;
        let tmp = tmpdir(&format!("ssh-prevalidation-{case}"));
        let home = tmp.join("home");
        let bin_dir = tmp.join("bin");
        fs::create_dir_all(home.join(".ssh")).unwrap();
        if case != "missing-system-config" {
            if case == "oversized-system-config" {
                fs::write(home.join(".ssh/config"), vec![b'x'; 256 * 1024 + 1]).unwrap();
            } else {
                let body = if case == "no-concrete-host" {
                    "Host * !blocked\n"
                } else {
                    "Host isolated-test-host\n"
                };
                fs::write(home.join(".ssh/config"), body).unwrap();
            }
            fs::set_permissions(home.join(".ssh/config"), fs::Permissions::from_mode(0o600))
                .unwrap();
        }
        let fake_science = write_test_bins(&bin_dir).canonicalize().unwrap();
        let mock_upstream = start_mock_upstream();
        let (proxy_port, sandbox_port) = ssh_fixture_ports();

        let mut env_guard = EnvGuard::new();
        env_guard.set("HOME", &home);
        env_guard.set("SCIENCE_BIN", &fake_science);
        env_guard.set("CSSWITCH_TEST_FAKE_SCIENCE_IDENTITY", "1");
        env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
        env_guard.set(
            "PATH",
            format!(
                "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                bin_dir.to_string_lossy()
            ),
        );
        env_guard.set("CSSWITCH_REPO", &repository);
        if case == "missing-packaged-wrapper" {
            env_guard.set(
                "CSSWITCH_TEST_SSH_WRAPPER_OVERRIDE",
                tmp.join("missing-wrapper"),
            );
        } else if case == "unsafe-packaged-wrapper" {
            let unsafe_wrapper = tmp.join("unsafe-wrapper");
            fs::write(&unsafe_wrapper, b"not executable\n").unwrap();
            fs::set_permissions(&unsafe_wrapper, fs::Permissions::from_mode(0o600)).unwrap();
            env_guard.set("CSSWITCH_TEST_SSH_WRAPPER_OVERRIDE", unsafe_wrapper);
        }

        let config_dir = config::default_dir();
        let before = ssh_fixture_config(mock_upstream.port, proxy_port, sandbox_port);
        config::save_to(&config_dir, &before).unwrap();
        let sandbox_home = home
            .join(config::CONFIG_DIR_NAME)
            .join("sandbox")
            .join("home");
        let science_data = sandbox_home.join(".claude-science");
        let mut foreign_target = None;
        if case == "foreign-sandbox-stub" {
            fs::create_dir_all(sandbox_home.join(".ssh")).unwrap();
            fs::write(
                sandbox_home.join(".ssh/config"),
                b"foreign-prevalidation-stub\n",
            )
            .unwrap();
            fs::set_permissions(
                sandbox_home.join(".ssh/config"),
                fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        } else if case == "symlink-sandbox-stub" {
            fs::create_dir_all(sandbox_home.join(".ssh")).unwrap();
            let target = tmp.join("foreign-stub-target");
            fs::write(&target, b"foreign-symlink-target\n").unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
            symlink(&target, sandbox_home.join(".ssh/config")).unwrap();
            foreign_target = Some(target);
        }
        if case == "conflicting-science-bridge" {
            fs::create_dir_all(&science_data).unwrap();
            fs::write(
                science_data.join("config.toml"),
                "ssh_hosts = [\"foreign-edit\"]\n",
            )
            .unwrap();
            fs::set_permissions(
                science_data.join("config.toml"),
                fs::Permissions::from_mode(0o600),
            )
            .unwrap();
            fs::write(
                    science_data.join("csswitch-ssh-bridge.v1.json"),
                    br#"{"schema_version":1,"original_ssh_hosts":null,"effective_ssh_hosts":["prior-managed"],"managed_hosts":["prior-managed"]}"#,
                )
                .unwrap();
            fs::set_permissions(
                science_data.join("csswitch-ssh-bridge.v1.json"),
                fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        } else if matches!(
            case,
            "group-writable-science-authority" | "other-writable-science-authority"
        ) {
            fs::create_dir_all(&science_data).unwrap();
            fs::write(science_data.join("config.toml"), b"quiet_logs = true\n").unwrap();
            fs::set_permissions(
                science_data.join("config.toml"),
                fs::Permissions::from_mode(0o600),
            )
            .unwrap();
            let mode = if case == "group-writable-science-authority" {
                0o720
            } else {
                0o702
            };
            fs::set_permissions(&science_data, fs::Permissions::from_mode(mode)).unwrap();
        }
        let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
        let config_before = config::load_from(&config_dir).unwrap();
        let app_before = app_authority_projection(&state);
        let system_ssh_before = authority_tree(&home.join(".ssh"));
        let science_before = science_authority_projection(&science_data);
        let private_state_before = authority_tree(&sandbox_home.parent().unwrap().join("state"));
        let runtime_before = authority_tree(&config_dir.join("runtime"));
        let stub_before = authority_tree(&sandbox_home.join(".ssh"));
        let foreign_target_before = foreign_target
            .as_deref()
            .map(authority_tree)
            .unwrap_or_default();
        let receipt_before = authority_tree(&config_dir.join("science-managed-launch.v1.json"));
        let lifecycle = Arc::new(lifecycle::Lifecycle::new());
        let app = tauri::test::mock_builder()
            .manage(state.clone())
            .manage(lifecycle.clone())
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        let handle = app.handle().clone();
        let cleanup = RuntimeSmokeCleanup::new(
            handle.clone(),
            state.clone(),
            tmp.clone(),
            sandbox_port,
            proxy_port,
        );

        let result = super::one_click_login_cmd(handle, state.clone(), lifecycle.clone(), None);
        let exact_error = match &result {
            Err(error) => error.to_string().contains(expected_error),
            Ok(outcome) => {
                outcome["action"] == "failed"
                    && outcome["stage"] == "prepare"
                    && outcome["recovery_status"] == "not_needed"
                    && outcome["environment_status"] == "not_exposed"
                    && outcome["message"]
                        .as_str()
                        .is_some_and(|message| message.contains(expected_error))
            }
        };
        let authorities_unchanged = config::load_from(&config_dir).unwrap() == config_before
            && app_authority_projection(&state) == app_before
            && authority_tree(&home.join(".ssh")) == system_ssh_before
            && science_authority_projection(&science_data) == science_before
            && authority_tree(&sandbox_home.parent().unwrap().join("state"))
                == private_state_before
            && authority_tree(&config_dir.join("runtime")) == runtime_before
            && authority_tree(&sandbox_home.join(".ssh")) == stub_before
            && foreign_target
                .as_deref()
                .map(authority_tree)
                .unwrap_or_default()
                == foreign_target_before
            && authority_tree(&config_dir.join("science-managed-launch.v1.json")) == receipt_before
            && TcpStream::connect(("127.0.0.1", proxy_port)).is_err()
            && TcpStream::connect(("127.0.0.1", sandbox_port)).is_err();
        cleanup
            .finish()
            .expect("prevalidation failure fixture must leave no process residue");

        assert!(
            exact_error,
            "{case} must reach its exact SSH-specific prevalidation refusal; observed={result:?}"
        );
        assert!(
                authorities_unchanged,
                "{case} must be rejected before OAuth, profile, Gateway, Science, bridge, stub, installer, receipt, or journal authority changes"
            );
    }
    assert_eq!(
        executed_cases,
        if requested_case.is_empty() {
            cases.len()
        } else {
            1
        },
        "SSH prevalidation selector must execute the exact requested oracle set"
    );
}

#[test]
#[ignore = "explicit Acceptance-boundary SSH transaction smoke; temp HOME, fake OAuth, fake Science, and loopback only"]
fn isolated_ssh_late_failure_compensates_every_authority_and_retry_is_idempotent() {
    let oracle_first = env::var("CSSWITCH_TEST_SSH_ORACLE_FIRST").unwrap_or_default();
    let failure_edge =
        env::var("CSSWITCH_TEST_SSH_LATE_FAILURE_EDGE").unwrap_or_else(|_| "foreign-stub".into());
    assert!(
        matches!(
            oracle_first.as_str(),
            "" | "oauth"
                | "gateway"
                | "gateway-child"
                | "codex-gateway-child"
                | "journal"
                | "bridge"
        ),
        "unknown SSH late-failure oracle selector: {oracle_first}"
    );
    let owned_gateway_oracle = matches!(
        oracle_first.as_str(),
        "gateway-child" | "codex-gateway-child"
    );
    let codex_gateway_oracle = oracle_first == "codex-gateway-child";
    assert!(
        matches!(
            failure_edge.as_str(),
            "foreign-stub"
                | "spawn-error"
                | "serve-mutates-then-exits"
                | "host-proof-drift"
                | "db-health-skipped"
                | "db-health-recovery"
                | "db-health-never-clears"
                | "db-health-stalled"
                | "db-health-io-errors"
                | "db-health-missing-kind"
                | "db-restart-no-listener"
                | "db-restart-launch-blocks"
                | "post-status-receipt"
        ),
        "unknown SSH late-failure edge selector: {failure_edge}"
    );
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let tmp = tmpdir("ssh-late-failure");
    let home = tmp.join("home");
    let bin_dir = tmp.join("bin");
    fs::create_dir_all(home.join(".ssh")).unwrap();
    fs::write(home.join(".ssh/config"), "Host isolated-test-host\n").unwrap();
    fs::set_permissions(home.join(".ssh/config"), fs::Permissions::from_mode(0o600)).unwrap();
    let fake_science = write_test_bins(&bin_dir).canonicalize().unwrap();
    let open_log = tmp.join("open.log");
    let science_call_log = tmp.join("science-call.log");
    let mock_upstream = start_mock_upstream();
    let mut port_reservations = reserve_ssh_fixture_ports();
    let proxy_port = port_reservations.proxy_port;
    let sandbox_port = port_reservations.sandbox_port;

    let mut env_guard = EnvGuard::new();
    env_guard.set("HOME", &home);
    env_guard.set("CSSWITCH_REPO", &root);
    env_guard.set("SCIENCE_BIN", &fake_science);
    env_guard.set("CSSWITCH_TEST_OPEN_BIN", bin_dir.join("open"));
    env_guard.set("CSSWITCH_TEST_FAKE_SCIENCE_IDENTITY", "1");
    env_guard.set("CSSWITCH_FAKE_OPEN_LOG", &open_log);
    env_guard.set("CSSWITCH_FAKE_SCIENCE_CALL_LOG", &science_call_log);
    env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
    let gateway_publish_log = tmp.join("gateway-publish.log");
    if owned_gateway_oracle {
        fs::write(&gateway_publish_log, b"").unwrap();
        fs::set_permissions(&gateway_publish_log, fs::Permissions::from_mode(0o600)).unwrap();
        env_guard.set("CSSWITCH_TEST_GATEWAY_PUBLISH_LOG", &gateway_publish_log);
    }
    env_guard.set(
        "PATH",
        format!(
            "{}:/usr/bin:/bin:/usr/sbin:/sbin",
            bin_dir.to_string_lossy()
        ),
    );

    let config_dir = config::default_dir();
    let mut before = ssh_fixture_config(mock_upstream.port, proxy_port, sandbox_port);
    let operation_api_canary = format!("ssh-transaction-key-{}", config::new_id());
    before
        .profile_by_id_mut("ssh-transaction")
        .expect("candidate fixture profile must exist")
        .api_key = operation_api_canary.clone();
    if codex_gateway_oracle {
        before.experimental_codex_enabled = true;
        before.profiles.push(Profile {
            id: "prior-codex-gateway".into(),
            name: "Prior fake Codex Gateway".into(),
            template_id: "codex".into(),
            category: "experimental".into(),
            api_format: "openai_responses".into(),
            credential_source: crate::provider_contracts::CredentialSource::CsswitchOauth,
            credential_ref: Some("csswitch:codex:default".into()),
            model_policy: crate::provider_contracts::ModelPolicy::DynamicCatalog,
            ..Default::default()
        });
    }
    let candidate_profile_id = before.active_id.clone();
    let prior_codex_profile = codex_gateway_oracle.then(|| {
        before
            .profile_by_id("prior-codex-gateway")
            .expect("prior Codex fixture profile must exist")
            .clone()
    });
    if codex_gateway_oracle {
        before.active_id = "prior-codex-gateway".into();
    }
    before.secret = config::new_id();
    before.runtime_transaction = Some(
        config::RuntimeTransactionJournal {
            transaction_id: "prior-ssh-transaction-journal".into(),
            target_profile_id: candidate_profile_id.clone(),
            stage: "start_gateway".into(),
            previous_binding: before.runtime_binding.clone(),
            previous_gateway: None,
        }
        .into(),
    );
    config::save_to(&config_dir, &before).unwrap();
    let sandbox_home = home
        .join(config::CONFIG_DIR_NAME)
        .join("sandbox")
        .join("home");
    let science_data = sandbox_home.join(".claude-science");
    fs::create_dir_all(science_data.join("mcp")).unwrap();
    crate::oauth_forge::ensure_virtual_login(
        &science_data,
        "virtual@localhost.invalid",
        &sandbox_home,
    )
    .unwrap();
    fs::remove_file(science_data.join("active-org.json")).unwrap();
    fs::write(
        science_data.join("config.toml"),
        "quiet_logs = true\nssh_hosts = [\"prior-user-host\"]\n",
    )
    .unwrap();
    fs::set_permissions(
        science_data.join("config.toml"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    fs::write(
        science_data.join("mcp/local-mcp.json"),
        br#"{"servers":[]}"#,
    )
    .unwrap();
    fs::set_permissions(
        science_data.join("mcp/local-mcp.json"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    fs::write(
        science_data.join(".csswitch-route-state.json"),
        br#"{"prior":"route-authority"}"#,
    )
    .unwrap();
    fs::set_permissions(
        science_data.join(".csswitch-route-state.json"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let installer_key = config_dir.join("runtime/skill-install-bridge.key");
    let installer_key_canary = format!("prior-installer-authority-{}", config::new_id());
    fs::create_dir_all(installer_key.parent().unwrap()).unwrap();
    fs::write(&installer_key, format!("{installer_key_canary}\n")).unwrap();
    fs::set_permissions(&installer_key, fs::Permissions::from_mode(0o600)).unwrap();
    let prior_runtime =
        science::select_science_runtime_cached(None, &science::ScienceVersionCache::default())
            .unwrap();
    let foreign_stub = sandbox_home.join(".ssh/config");
    let inplace_authority = science_data.join("orgs/csswitch-test/prior-inplace.db");
    fs::create_dir_all(inplace_authority.parent().unwrap()).unwrap();
    let failure_marker = tmp.join("serve-mutation-reached");
    let failure_control = science_data.join("fake-science/fail-after-inplace-mutation");
    let unbound_candidate_pid = tmp.join("unbound-second-candidate.pid");
    if failure_edge == "foreign-stub" {
        env_guard.set("CSSWITCH_TEST_SSH_LATE_FOREIGN_STUB", &foreign_stub);
    } else if failure_edge == "host-proof-drift" {
        env_guard.set("CSSWITCH_TEST_SSH_HOSTS_AFTER_CAPTURE", "drifted-test-host");
    } else if failure_edge == "db-health-skipped" {
        env_guard.set("CSSWITCH_FAKE_SCIENCE_DB_HEALTH", "skipped");
    } else if failure_edge == "db-health-recovery" {
        env_guard.set("CSSWITCH_FAKE_SCIENCE_DB_HEALTH", "stateful");
        fs::create_dir_all(&science_data).unwrap();
        fs::write(science_data.join("fake-db-damage-verdict"), b"flagged\n").unwrap();
    } else if failure_edge == "db-health-never-clears" {
        env_guard.set("CSSWITCH_FAKE_SCIENCE_DB_HEALTH", "damage-reverify");
        env_guard.set("CSSWITCH_TEST_DB_REVERIFY_BUDGET_MS", "250");
    } else if failure_edge == "db-health-stalled" {
        env_guard.set("CSSWITCH_FAKE_SCIENCE_DB_HEALTH", "stalled");
        env_guard.set("CSSWITCH_TEST_DB_REVERIFY_BUDGET_MS", "250");
    } else if failure_edge == "db-health-io-errors" {
        env_guard.set("CSSWITCH_FAKE_SCIENCE_DB_HEALTH", "io-errors");
    } else if failure_edge == "db-health-missing-kind" {
        env_guard.set("CSSWITCH_FAKE_SCIENCE_DB_HEALTH", "missing-kind");
    } else if failure_edge == "spawn-error" {
        let fake_state = science_data.join("fake-science");
        fs::create_dir_all(&fake_state).unwrap();
        fs::write(
            fake_state.join("call-log-path"),
            science_call_log.to_string_lossy().as_bytes(),
        )
        .unwrap();
    } else if matches!(
        failure_edge.as_str(),
        "db-restart-no-listener" | "db-restart-launch-blocks"
    ) {
        env_guard.set("CSSWITCH_FAKE_SCIENCE_DB_HEALTH", "stateful");
        if failure_edge == "db-restart-no-listener" {
            env_guard.set("CSSWITCH_FAKE_SCIENCE_SECOND_BOOT_NO_LISTENER", "1");
        } else {
            env_guard.set("CSSWITCH_FAKE_SCIENCE_SECOND_BOOT_BLOCKS", "1");
        }
        env_guard.set("CSSWITCH_FAKE_SCIENCE_UNBOUND_PID", &unbound_candidate_pid);
        env_guard.set("CSSWITCH_FAKE_SCIENCE_UNBOUND_MUTATION", &inplace_authority);
        env_guard.set("CSSWITCH_TEST_DB_RECOVERY_RESTART_BUDGET_MS", "250");
        fs::create_dir_all(&science_data).unwrap();
        let fake_state = science_data.join("fake-science");
        fs::create_dir_all(&fake_state).unwrap();
        fs::write(fake_state.join("db-health"), b"stateful\n").unwrap();
        fs::write(
            fake_state.join("call-log-path"),
            science_call_log.to_string_lossy().as_bytes(),
        )
        .unwrap();
        fs::write(
            fake_state.join("unbound-pid-path"),
            unbound_candidate_pid.to_string_lossy().as_bytes(),
        )
        .unwrap();
        fs::write(
            fake_state.join("unbound-mutation-path"),
            inplace_authority.to_string_lossy().as_bytes(),
        )
        .unwrap();
        fs::write(
            fake_state.join(if failure_edge == "db-restart-no-listener" {
                "second-boot-no-listener"
            } else {
                "second-boot-blocks"
            }),
            b"1\n",
        )
        .unwrap();
        fs::write(science_data.join("fake-db-damage-verdict"), b"flagged\n").unwrap();
        fs::write(&inplace_authority, b"prior-inplace-authority\n").unwrap();
    } else if failure_edge == "serve-mutates-then-exits" {
        fs::write(&inplace_authority, b"prior-inplace-authority\n").unwrap();
        fs::set_permissions(&inplace_authority, fs::Permissions::from_mode(0o600)).unwrap();
        fs::create_dir_all(failure_control.parent().unwrap()).unwrap();
        fs::write(
            &failure_control,
            format!(
                "{}\n{}\n",
                inplace_authority.display(),
                failure_marker.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&failure_control, fs::Permissions::from_mode(0o600)).unwrap();
    } else if failure_edge == "post-status-receipt" {
        env_guard.set("CSSWITCH_TEST_MANAGED_LAUNCH_COMMIT_FAILURE", "1");
    }
    let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
    let lifecycle = Arc::new(lifecycle::Lifecycle::new());
    let codex_auth_supervisor =
        Arc::new(crate::codex_auth_supervisor::CodexAuthSupervisor::default());
    let app = tauri::test::mock_builder()
        .manage(state.clone())
        .manage(lifecycle.clone())
        .manage(codex_auth_supervisor.clone())
        .invoke_handler(tauri::generate_handler![
            super::one_click_login,
            super::status,
            super::boot_snapshot
        ])
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    let handle = app.handle().clone();
    let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .unwrap();
    let event_sinks = Arc::new(Mutex::new(Vec::<String>::new()));
    for event_name in ["codex-auth://operation", "boot://publication"] {
        let observed = event_sinks.clone();
        handle.listen_any(event_name, move |event| {
            observed
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(event.payload().to_string());
        });
    }
    let auth_call_log = tmp.join("codex-auth-call.log");
    if codex_gateway_oracle {
        let real_gateway = proxy_lifecycle::gateway_bin_path(&handle)
            .expect("fixture must locate the local Gateway before installing its wrapper");
        let gateway_wrapper = bin_dir.join("csswitch-gateway-auth-wrapper");
        write_executable(
            &gateway_wrapper,
            &format!(
                r#"#!/bin/sh
printf '%s %s\n' "$1" "$2" >> '{}'
if [ "$1" = "codex-auth" ] && [ "$2" = "status" ]; then
  printf '%s\n' '{{"schema_version":3,"ok":true,"command":"status","status":{{"authenticated":true,"reason":"ready","account_hash":"abababababababababababababababab","expiry_state":"valid","expires_at":2000000000,"auth_epoch":"cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd","auth_generation":1}}}}'
  exit 0
fi
exec '{}' "$@"
"#,
                auth_call_log.display(),
                real_gateway.display()
            ),
        );
        env_guard.set("CSSWITCH_GATEWAY_BIN", &gateway_wrapper);
    }
    let mut prior_gateway_health = None;
    let mut prior_gateway_pid = None;
    if owned_gateway_oracle {
        port_reservations.release_proxy();
        let prior_profile = if codex_gateway_oracle {
            prior_codex_profile
                .as_ref()
                .expect("prior Codex fixture profile must exist")
                .clone()
        } else {
            before.active_profile().unwrap().clone()
        };
        let receipt = start_internal_gateway(&handle, &state, lifecycle.as_ref(), &prior_profile)
            .expect("internal Gateway core must establish the prior managed Gateway");
        assert!(matches!(
            receipt.action,
            crate::runtime::proxy::ProxyAction::Restarted
        ));
        let mut authority = lock(&state);
        assert!(authority.proxy.is_some());
        prior_gateway_pid = authority.proxy.as_ref().map(std::process::Child::id);
        prior_gateway_health = crate::proc::http_gateway_health(
            authority.proxy_port,
            Some(&authority.secret),
            crate::runtime::operation::LOCAL_HEALTH_TIMEOUT_MS,
        );
        assert!(prior_gateway_health.is_some());
        authority.sandbox_port = 43211;
        authority.sandbox_url = Some("http://127.0.0.1:43211/prior".into());
        authority.science_confirmed_stopped = Some(prior_runtime.clone());
        drop(authority);
        if codex_gateway_oracle {
            before.active_id = candidate_profile_id.clone();
            config::save_to(&config_dir, &before).unwrap();
            fs::write(&auth_call_log, b"").unwrap();
        }
    } else {
        let mut authority = lock(&state);
        authority.proxy_port = 43210;
        authority.secret = "prior-memory-secret".into();
        authority.provider = "prior-provider".into();
        authority.gateway_kind = "prior-gateway".into();
        authority.shim_mode = "prior-shim".into();
        authority.launch_id = "prior-launch-id".into();
        authority.key_fp = 4242;
        authority.sandbox_port = 43211;
        authority.sandbox_url = Some("http://127.0.0.1:43211/prior".into());
        authority.science_confirmed_stopped = Some(prior_runtime.clone());
    }
    let config_before = config::load_from(&config_dir).unwrap();
    let app_authority_before = app_authority_projection(&state);
    let system_ssh_before = authority_tree(&home.join(".ssh"));
    let science_authority_before = science_authority_projection(&science_data);
    let private_state_before = authority_tree(&sandbox_home.parent().unwrap().join("state"));
    let installer_key_before = authority_tree(&config_dir.join("runtime"));
    let managed_receipt = config_dir.join("science-managed-launch.v1.json");
    let managed_receipt_before = authority_tree(&managed_receipt);
    let cleanup = RuntimeSmokeCleanup::new(
        handle.clone(),
        state.clone(),
        tmp.clone(),
        sandbox_port,
        proxy_port,
    );
    let _db_catalog_bypass = matches!(
        failure_edge.as_str(),
        "db-health-skipped"
            | "db-health-recovery"
            | "db-health-never-clears"
            | "db-health-stalled"
            | "db-health-io-errors"
            | "db-health-missing-kind"
            | "db-restart-no-listener"
            | "db-restart-launch-blocks"
    )
    .then(|| sandbox_session::test_arm_gateway_catalog_bypass(proxy_port));

    let mut science_spawn_failure_seam =
        (failure_edge == "spawn-error").then(science::test_arm_science_launch_spawn_failure);
    port_reservations.release_one_click_ports();
    let operation_started = Instant::now();
    let failed: Result<serde_json::Value, String> = if codex_gateway_oracle {
        invoke_json(
            &webview,
            "one_click_login",
            serde_json::json!({"runtimeChoice": null}),
        )
        .and_then(|value| {
            if value["status"] == "error" {
                Err(value["message"]
                    .as_str()
                    .unwrap_or("typed one-click IPC failure")
                    .to_string())
            } else {
                Ok(value)
            }
        })
    } else {
        sandbox_session::one_click_login(
            handle.clone(),
            state.clone(),
            lifecycle.as_ref(),
            None,
            None,
        )
        .map_err(|error| error.to_string())
    };
    let failure_surface = failed
        .as_ref()
        .err()
        .map(String::as_str)
        .unwrap_or_default();
    if failure_edge != "db-health-recovery" {
        let environment_was_exposed = !matches!(
            failure_edge.as_str(),
            "foreign-stub" | "spawn-error" | "host-proof-drift"
        );
        assert_eq!(
                failure_surface.contains("environment_uncertain"),
                environment_was_exposed,
                "the failure surface must distinguish pre-spawn rollback from a launch whose Science-owned environment may have changed: edge={failure_edge}, failure={failure_surface}"
            );
    }
    if failure_edge == "serve-mutates-then-exits" {
        assert!(
                failure_surface.contains("recovery_status=cleanup_required")
                    && failure_surface.contains("compensation_restore_blocked_science_candidate"),
                "a failed Science invocation without an exact managed identity must preserve the recovery snapshot and block protected-state restore: {failed:?}"
            );
        assert!(
            failure_marker.is_file()
                && fs::read_to_string(&inplace_authority)
                    .is_ok_and(|bytes| bytes == "mutated-by-failing-science\n"),
            "the oracle must prove Science changed a protected org authority before failing"
        );
        assert_ne!(
            science_authority_projection(&science_data),
            science_authority_before,
            "protected authority must not be restored underneath an unproven candidate"
        );
        assert!(
            failure_surface.contains(".one-click-rollback-"),
            "manual recovery must retain a credential-free snapshot locator"
        );
        fs::remove_file(&failure_control).unwrap();
        cleanup.finish().expect("unproven candidate oracle cleanup");
        return;
    }
    if failure_edge == "db-health-recovery" {
        assert!(
            failed
                .as_ref()
                .is_ok_and(|value| value["action"] == "started"),
            "stateful 0.1.25 oracle must close in one operation: {failed:?}"
        );
        assert_eq!(
            fs::read_to_string(science_data.join("fake-science/serve-count")).unwrap(),
            "2",
            "recovery must use the initial boot plus exactly one managed restart"
        );
        assert!(
            !science_data.join("fake-db-damage-verdict").exists(),
            "the Science-owned cleared verdict must survive the managed restart"
        );
        env::remove_var("CSSWITCH_FAKE_SCIENCE_DB_HEALTH");
        cleanup.finish().expect("DB recovery oracle cleanup");
        return;
    }
    if failure_edge == "db-health-skipped" {
        assert!(
            failed
                .as_ref()
                .is_err_and(|error| { error.contains("第二次启动未达到 clear/clear") }),
            "a second boot that still reports migrations_skipped must fail closed: {failed:?}"
        );
        assert_eq!(
                fs::read_to_string(&science_call_log)
                    .unwrap_or_default()
                    .lines()
                    .filter(|line| *line == "serve")
                    .count(),
                2,
                "second-boot failure must consume the sole managed restart and never launch a third candidate"
            );
        assert_eq!(
                science_authority_projection(&science_data),
                science_authority_before,
                "second-boot failure must restore the complete pre-operation ScienceData tree, including OAuth, route/config, and the persisted verdict"
            );
        assert!(TcpStream::connect(("127.0.0.1", sandbox_port)).is_err());
        assert!(!config_dir.join("science-managed-launch.v1.json").exists());
        env::remove_var("CSSWITCH_FAKE_SCIENCE_DB_HEALTH");
        cleanup.finish().expect("DB second-boot failure cleanup");
        return;
    }
    if failure_edge == "db-health-never-clears" {
        assert!(
            failed
                .as_ref()
                .is_err_and(|error| error.contains("science_db_reverify_timeout")),
            "a verdict that never clears must stop at the bounded reverify deadline: {failed:?}"
        );
        assert_eq!(
            fs::read_to_string(&science_call_log)
                .unwrap_or_default()
                .lines()
                .filter(|line| *line == "serve")
                .count(),
            1,
            "reverify timeout must not consume the managed restart"
        );
        assert_eq!(
            science_authority_projection(&science_data),
            science_authority_before
        );
        assert!(TcpStream::connect(("127.0.0.1", sandbox_port)).is_err());
        assert!(!config_dir.join("science-managed-launch.v1.json").exists());
        env::remove_var("CSSWITCH_FAKE_SCIENCE_DB_HEALTH");
        env::remove_var("CSSWITCH_TEST_DB_REVERIFY_BUDGET_MS");
        cleanup.finish().expect("DB reverify-timeout cleanup");
        return;
    }
    if failure_edge == "db-health-stalled" {
        assert!(
                failed
                    .as_ref()
                    .is_err_and(|error| error.contains("science_db_reverify_timeout")),
                "a stalled authenticated health response must stop at the absolute reverify deadline: {failed:?}"
            );
        assert!(
                operation_started.elapsed() < Duration::from_millis(3_500),
                "the 250ms reverify budget must cap a stalled 2s health response in addition to the fixed launch/rollback work"
            );
        assert_eq!(
            fs::read_to_string(&science_call_log)
                .unwrap_or_default()
                .lines()
                .filter(|line| *line == "serve")
                .count(),
            1,
            "stalled health must not consume the managed restart"
        );
        assert_eq!(
            science_authority_projection(&science_data),
            science_authority_before
        );
        assert!(TcpStream::connect(("127.0.0.1", sandbox_port)).is_err());
        assert!(!config_dir.join("science-managed-launch.v1.json").exists());
        env::remove_var("CSSWITCH_FAKE_SCIENCE_DB_HEALTH");
        env::remove_var("CSSWITCH_TEST_DB_REVERIFY_BUDGET_MS");
        cleanup.finish().expect("DB stalled-health cleanup");
        return;
    }
    if failure_edge == "db-health-io-errors" {
        assert!(
                failed.as_ref().is_err_and(|error| {
                    error.contains("第二次启动未达到 clear/clear")
                }),
                "0.1.25 io_errors is a THIS-run wedge and must consume the sole restart immediately: {failed:?}"
            );
        assert_eq!(
            fs::read_to_string(&science_call_log)
                .unwrap_or_default()
                .lines()
                .filter(|line| *line == "serve")
                .count(),
            2,
            "io_errors must not enter the 305s damage reverify lane"
        );
        assert_eq!(
            science_authority_projection(&science_data),
            science_authority_before
        );
        assert!(TcpStream::connect(("127.0.0.1", sandbox_port)).is_err());
        assert!(!config_dir.join("science-managed-launch.v1.json").exists());
        env::remove_var("CSSWITCH_FAKE_SCIENCE_DB_HEALTH");
        cleanup.finish().expect("DB io_errors cleanup");
        return;
    }
    if failure_edge == "db-health-missing-kind" {
        assert!(
                failed.as_ref().is_err_and(
                    |error| error.contains("science_api_health_missing_db_corruption_kind")
                ),
                "flagged health without the 0.1.25 kind discriminant must fail closed: {failed:?}"
            );
        assert_eq!(
            fs::read_to_string(&science_call_log)
                .unwrap_or_default()
                .lines()
                .filter(|line| *line == "serve")
                .count(),
            1,
            "malformed health must not guess a recovery lane or consume the restart"
        );
        assert_eq!(
            science_authority_projection(&science_data),
            science_authority_before
        );
        assert!(TcpStream::connect(("127.0.0.1", sandbox_port)).is_err());
        assert!(!config_dir.join("science-managed-launch.v1.json").exists());
        env::remove_var("CSSWITCH_FAKE_SCIENCE_DB_HEALTH");
        cleanup.finish().expect("DB missing-kind cleanup");
        return;
    }
    if matches!(
        failure_edge.as_str(),
        "db-restart-no-listener" | "db-restart-launch-blocks"
    ) {
        assert!(
                failed.as_ref().is_err_and(|error| {
                    error.contains("code=science_candidate_stop_unproven")
                        && error.contains("compensation_restore_blocked_science_candidate")
                        && error.contains("recovery_status=cleanup_required")
                }),
                "a detached post-spawn/pre-receipt candidate must block authority restore with typed recovery: {failed:?}"
        );
        if failure_edge == "db-restart-launch-blocks" {
            assert!(
                    operation_started.elapsed() < std::time::Duration::from_secs(5),
                    "the recovery deadline must include the launch script, not begin after blocking status"
                );
        }
        let unbound_pid = fs::read_to_string(&unbound_candidate_pid)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        assert!(unbound_pid > 1);
        assert!(
            process_start_identity_if_alive(unbound_pid).is_some(),
            "old-product red requires the unbound second candidate to remain alive"
        );
        let expected_mutation = if failure_edge == "db-restart-launch-blocks" {
            "mutated-by-blocked-second-candidate\n"
        } else {
            "mutated-by-unbound-second-candidate\n"
        };
        assert_eq!(
            fs::read_to_string(&inplace_authority).unwrap(),
            expected_mutation,
            "authority must not be restored underneath an unproven live candidate"
        );
        assert_ne!(
            science_authority_projection(&science_data),
            science_authority_before,
            "blocked restore must preserve the current authority plus the recovery snapshot"
        );
        assert!(TcpStream::connect(("127.0.0.1", sandbox_port)).is_err());
        assert!(!config_dir.join("science-managed-launch.v1.json").exists());
        unsafe {
            libc::kill(unbound_pid as i32, libc::SIGTERM);
        }
        for _ in 0..50 {
            if process_start_identity_if_alive(unbound_pid).is_none() {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(20));
        }
        if process_start_identity_if_alive(unbound_pid).is_some() {
            unsafe {
                libc::kill(unbound_pid as i32, libc::SIGKILL);
            }
        }
        assert!(
            process_start_identity_if_alive(unbound_pid).is_none(),
            "test must clean the intentionally unowned candidate"
        );
        env::remove_var("CSSWITCH_FAKE_SCIENCE_DB_HEALTH");
        env::remove_var("CSSWITCH_FAKE_SCIENCE_SECOND_BOOT_NO_LISTENER");
        env::remove_var("CSSWITCH_FAKE_SCIENCE_SECOND_BOOT_BLOCKS");
        env::remove_var("CSSWITCH_FAKE_SCIENCE_UNBOUND_PID");
        env::remove_var("CSSWITCH_FAKE_SCIENCE_UNBOUND_MUTATION");
        env::remove_var("CSSWITCH_TEST_DB_RECOVERY_RESTART_BUDGET_MS");
        cleanup.finish().expect("DB unbound-candidate cleanup");
        return;
    }
    let operation_auth_preflight_count = fs::read_to_string(&auth_call_log)
        .unwrap_or_default()
        .lines()
        .filter(|line| *line == "codex-auth status")
        .count();
    let ipc_ui_sinks = if codex_gateway_oracle {
        ["status", "boot_snapshot"]
            .into_iter()
            .map(|command| {
                invoke_json(&webview, command, serde_json::json!({}))
                    .map(|value| value.to_string())
                    .unwrap_or_else(|_| "typed IPC error".into())
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let config_after_failure = config::load_from(&config_dir).unwrap();
    let app_authority_after_failure = app_authority_projection(&state);
    let (tracked_gateway_pid_after_failure, tracked_gateway_running_after_failure) = {
        let mut authority = lock(&state);
        match authority.proxy.as_mut() {
            Some(child) => (Some(child.id()), child.try_wait().unwrap().is_none()),
            None => (None, false),
        }
    };
    let gateway_health_after_failure = crate::proc::http_gateway_health(
        app_authority_after_failure.proxy_port,
        Some(&app_authority_after_failure.secret),
        crate::runtime::operation::LOCAL_HEALTH_TIMEOUT_MS,
    );
    let gateway_publish_raw = fs::read_to_string(&gateway_publish_log).unwrap_or_default();
    let gateway_publishes = gateway_publish_raw
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_ascii_whitespace();
            Some((
                fields.next()?.parse::<u32>().ok()?,
                fields.next()?.to_string(),
                fields.next()?.parse::<u64>().ok()?,
                fields.next()?.parse::<u16>().ok()?,
            ))
        })
        .collect::<Vec<_>>();
    let gateway_publish_adapters = gateway_publish_raw
        .lines()
        .filter_map(|line| line.split_ascii_whitespace().nth(4).map(str::to_string))
        .collect::<Vec<_>>();
    let launch_context_matches_prior = if codex_gateway_oracle {
        let authority = lock(&state);
        authority
            .gateway_launch_context
            .as_ref()
            .is_some_and(|context| {
                prior_codex_profile
                    .as_ref()
                    .is_some_and(|profile| context.profile == *profile)
                    && context.science_runtime.is_none()
            })
    } else {
        true
    };
    let key_fingerprint_lineage_exact = if codex_gateway_oracle {
        gateway_publishes.len() == 3
            && gateway_publishes[2].2 == gateway_publishes[0].2
            && gateway_publishes[2].2 != gateway_publishes[1].2
    } else {
        true
    };
    let system_ssh_after_failure = authority_tree(&home.join(".ssh"));
    let science_authority_after_failure = science_authority_projection(&science_data);
    let private_state_after_failure = authority_tree(&sandbox_home.parent().unwrap().join("state"));
    let installer_key_after_failure = authority_tree(&config_dir.join("runtime"));
    let managed_receipt_after_failure = authority_tree(&managed_receipt);
    let science_calls_after_failure =
        fs::read_to_string(&science_call_log).unwrap_or_else(|_| "<missing>".into());
    let failure_marker_reached = failure_marker.is_file();
    let oauth_restored = science_authority_after_failure == science_authority_before
        && private_state_after_failure == private_state_before;
    let profile_restored = config_after_failure == config_before;
    let journal_restored =
        config_after_failure.runtime_transaction == config_before.runtime_transaction;
    let stable_app_authority_restored = app_authority_after_failure.proxy_port
        == app_authority_before.proxy_port
        && app_authority_after_failure.secret == app_authority_before.secret
        && app_authority_after_failure.provider == app_authority_before.provider
        && app_authority_after_failure.gateway_kind == app_authority_before.gateway_kind
        && app_authority_after_failure.shim_mode == app_authority_before.shim_mode
        && app_authority_after_failure.key_fp == app_authority_before.key_fp
        && app_authority_after_failure.sandbox_present == app_authority_before.sandbox_present
        && app_authority_after_failure.sandbox_port == app_authority_before.sandbox_port
        && app_authority_after_failure.sandbox_url == app_authority_before.sandbox_url
        && app_authority_after_failure.science_runtime == app_authority_before.science_runtime
        && app_authority_after_failure.science_confirmed_stopped
            == app_authority_before.science_confirmed_stopped
        && app_authority_after_failure.history_recovery_present
            == app_authority_before.history_recovery_present;
    let gateway_restored = if owned_gateway_oracle {
        let prior_health = prior_gateway_health.as_ref();
        let restored_health = gateway_health_after_failure.as_ref();
        let prior_publish = gateway_publishes.first();
        let candidate = gateway_publishes.get(1);
        let restored_publish = gateway_publishes.get(2);
        let expected_installer_token = proxy_lifecycle::test_skill_install_bridge_token(
            &app_authority_after_failure.secret,
            &app_authority_after_failure.launch_id,
        )
        .ok()
        .map(|token| format!("{token}\n").into_bytes());
        stable_app_authority_restored
            && app_authority_after_failure.proxy_present
            && !app_authority_after_failure.launch_id.is_empty()
            && tracked_gateway_running_after_failure
            && tracked_gateway_pid_after_failure == restored_publish.map(|publish| publish.0)
            && listener_pid_if_unique(proxy_port) == tracked_gateway_pid_after_failure
            && gateway_publishes.len() == 3
            && prior_publish.is_some_and(|prior| {
                Some(prior.0) == prior_gateway_pid
                    && prior_health.is_some_and(|health| health.launch_id == prior.1)
                    && prior.3 == proxy_port
            })
            && candidate.is_some_and(|candidate| {
                Some(candidate.0) != tracked_gateway_pid_after_failure
                    && Some(candidate.0) != prior_gateway_pid
                    && candidate.1 != app_authority_after_failure.launch_id
                    && candidate.3 == proxy_port
                    && process_start_identity_if_alive(candidate.0).is_none()
            })
            && restored_publish.is_some_and(|restored| {
                Some(restored.0) != prior_gateway_pid
                    && restored.1 == app_authority_after_failure.launch_id
                    && restored.2 == app_authority_after_failure.key_fp
                    && restored.3 == app_authority_after_failure.proxy_port
            })
            && prior_gateway_pid.is_some()
            && prior_health
                .zip(restored_health)
                .is_some_and(|(prior, restored)| {
                    restored.gateway == prior.gateway
                        && restored.provider == prior.provider
                        && restored.shim == prior.shim
                        && restored.provider_contract_id == prior.provider_contract_id
                        && restored.provider_contract_digest == prior.provider_contract_digest
                        && restored.catalog_fp == prior.catalog_fp
                        && restored.intent == prior.intent
                        && restored.launch_id == app_authority_after_failure.launch_id
                })
            && installer_key_after_failure
                .get(&PathBuf::from("skill-install-bridge.key"))
                .is_some_and(|entry| {
                    entry.kind == "file"
                        && entry.mode == 0o600
                        && Some(&entry.bytes) == expected_installer_token.as_ref()
                })
    } else {
        app_authority_after_failure == app_authority_before
            && TcpStream::connect(("127.0.0.1", proxy_port)).is_err()
            && installer_key_after_failure == installer_key_before
    };
    let science_restored = stable_app_authority_restored
        && TcpStream::connect(("127.0.0.1", sandbox_port)).is_err()
        && managed_receipt_after_failure == managed_receipt_before;
    let bridge_restored = science_authority_after_failure == science_authority_before;
    let foreign_stub_authority = fs::symlink_metadata(&foreign_stub)
        .ok()
        .filter(|metadata| metadata.file_type().is_file())
        .map(|metadata| {
            (
                metadata.permissions().mode() & 0o777,
                fs::read(&foreign_stub).unwrap(),
            )
        });
    let sandbox_ssh_after_failure = authority_tree(&sandbox_home.join(".ssh"));
    let expected_sandbox_ssh = BTreeMap::from([
        (
            PathBuf::new(),
            AuthorityTreeEntry {
                kind: "dir",
                mode: 0o700,
                bytes: Vec::new(),
            },
        ),
        (
            PathBuf::from("config"),
            AuthorityTreeEntry {
                kind: "file",
                mode: 0o600,
                bytes: b"foreign-test-stub-must-survive\n".to_vec(),
            },
        ),
    ]);

    if failure_edge == "foreign-stub" {
        env::remove_var("CSSWITCH_TEST_SSH_LATE_FOREIGN_STUB");
        fs::remove_file(&foreign_stub).unwrap();
    } else if failure_edge == "host-proof-drift" {
        env::remove_var("CSSWITCH_TEST_SSH_HOSTS_AFTER_CAPTURE");
    } else if failure_edge == "db-health-skipped" {
        env::remove_var("CSSWITCH_FAKE_SCIENCE_DB_HEALTH");
    } else if failure_edge == "spawn-error" {
        drop(science_spawn_failure_seam.take());
    } else if failure_edge == "serve-mutates-then-exits" {
        fs::remove_file(&failure_control).unwrap();
    } else {
        env::remove_var("CSSWITCH_TEST_MANAGED_LAUNCH_COMMIT_FAILURE");
    }
    let retry: Result<serde_json::Value, String> = if codex_gateway_oracle {
        invoke_json(
            &webview,
            "one_click_login",
            serde_json::json!({"runtimeChoice": null}),
        )
        .and_then(|value| {
            if value["status"] == "error" {
                Err(value["message"]
                    .as_str()
                    .unwrap_or("typed one-click IPC failure")
                    .to_string())
            } else {
                Ok(value)
            }
        })
    } else {
        sandbox_session::one_click_login(handle, state.clone(), lifecycle.as_ref(), None, None)
            .map_err(|error| error.to_string())
    };
    let retry_started_once = retry
        .as_ref()
        .is_ok_and(|value| value["action"] == "started")
        && fs::read_to_string(sandbox_home.join(".claude-science/fake-science/serve-count"))
            .ok()
            .as_deref()
            == Some(if failure_edge == "post-status-receipt" {
                "2"
            } else {
                "1"
            });
    let science_calls_after_retry =
        fs::read_to_string(&science_call_log).unwrap_or_else(|_| "<missing>".into());
    let serve_calls_after_failure = science_calls_after_failure
        .lines()
        .filter(|line| *line == "serve")
        .count();
    let serve_calls_after_retry = science_calls_after_retry
        .lines()
        .filter(|line| *line == "serve")
        .count();
    cleanup
        .finish()
        .expect("late-failure and retry fixtures must leave no process residue");

    if failure_edge == "foreign-stub" {
        assert!(
            failed.as_ref().is_err_and(|error| {
                error.contains("起沙箱脚本失败")
                    && error.contains("隔离 SSH config 不是 CSSwitch 管理的安全入口")
            }),
            "late injection must reach the exact SSH launch-script foreign-stub refusal"
        );
        assert_eq!(
                foreign_stub_authority,
                Some((0o600, b"foreign-test-stub-must-survive\n".to_vec())),
                "late-failure compensation must preserve foreign SSH file kind, private mode, and exact bytes"
            );
        assert_eq!(
                sandbox_ssh_after_failure, expected_sandbox_ssh,
                "late-failure compensation must leave no extra file, symlink, or directory in the sandbox SSH authority"
            );
    } else if failure_edge == "host-proof-drift" {
        assert!(
            failed.as_ref().is_err_and(|error| {
                error.contains("code=ssh_authority_changed_retry")
                    && !error.contains("起沙箱脚本失败")
            }),
            "host proof drift must fail before packaged zsh with a typed retry"
        );
        assert!(
            sandbox_ssh_after_failure.is_empty(),
            "host proof drift must not create or rewrite the sandbox SSH stub"
        );
        assert_eq!(
            serve_calls_after_failure, 0,
            "host proof drift must fail before fake Science serve"
        );
    } else if failure_edge == "db-health-skipped" {
        assert!(
            failed.as_ref().is_err_and(|error| {
                error.contains("science_db_migrations_skipped_restart_required")
            }),
            "semantic DB health must reject a coarse /health 200"
        );
        assert_eq!(
            serve_calls_after_failure, 1,
            "fake Science must start before semantic DB health rejects readiness"
        );
        assert!(
            sandbox_ssh_after_failure.is_empty(),
            "DB readiness compensation must remove the managed sandbox SSH stub"
        );
    } else if failure_edge == "spawn-error" {
        let spawn_error_reached = failure_surface.contains("起沙箱失败");
        assert!(
                spawn_error_reached,
                "spawn-error fixture must reach the post-authority zsh spawn edge: spawn_error_reached={spawn_error_reached}"
            );
        assert!(
            sandbox_ssh_after_failure.is_empty(),
            "a shell spawn error must leave no sandbox SSH authority"
        );
    } else if failure_edge == "serve-mutates-then-exits" {
        let serve_failure_reached = failure_surface.contains("起沙箱脚本失败");
        assert!(
                serve_failure_reached,
                "failing Science must propagate the launch-script failure: serve_failure_reached={serve_failure_reached}"
            );
        assert!(
                failure_marker_reached,
                "fake Science must prove it mutated live authority before exiting: failure_marker_reached={failure_marker_reached}, science_serve_observed={}",
                science_calls_after_failure.lines().any(|line| line == "serve")
            );
        assert!(
            sandbox_ssh_after_failure.is_empty(),
            "failed Science cleanup must remove only the managed sandbox SSH stub"
        );
    } else {
        let post_status_failure_reached = failure_surface.contains("受管启动身份无法安全提交")
            && failure_surface.contains("test-only managed launch commit failure");
        assert!(
                post_status_failure_reached,
                "post-status fixture must fail only after healthy listener identity and before receipt commit: post_status_failure_reached={post_status_failure_reached}"
            );
        assert!(
            sandbox_ssh_after_failure.is_empty(),
            "post-status compensation must remove the managed sandbox SSH stub"
        );
    }
    if failure_edge == "host-proof-drift" {
        assert_ne!(
            system_ssh_after_failure, system_ssh_before,
            "the external test authority drift must remain external to CSSwitch rollback"
        );
        assert_eq!(
            system_ssh_after_failure
                .get(&PathBuf::from("config"))
                .map(|entry| entry.bytes.as_slice()),
            Some(b"Host drifted-test-host\n".as_slice())
        );
    } else {
        assert_eq!(
            system_ssh_after_failure, system_ssh_before,
            "late-failure compensation must not change the authorized system SSH source tree"
        );
    }
    if oracle_first == "gateway" {
        assert!(
            gateway_restored,
            "late SSH failure must restore Gateway authority"
        );
    } else if oracle_first == "journal" {
        assert!(
            journal_restored,
            "late SSH failure must restore the exact prior transaction journal"
        );
    } else if oracle_first == "bridge" {
        assert!(
            bridge_restored,
            "late SSH failure must restore managed bridge state and Science config"
        );
    } else {
        assert!(
            oauth_restored,
            "late SSH failure must restore the exact prior virtual OAuth authority"
        );
    }
    assert!(
        profile_restored,
        "late SSH failure must preserve the active profile"
    );
    if owned_gateway_oracle {
        let ownership_error_absent = !failure_surface.contains("无法恢复先前 Gateway child 所有权");
        assert!(
                ownership_error_absent,
                "rollback must restore a real prior Gateway instead of reporting lost child ownership: ownership_error_absent={ownership_error_absent}"
            );
    }
    if codex_gateway_oracle {
        let log_authority = authority_tree(&config_dir.join("logs"));
        let mut forbidden = vec![
            operation_api_canary.clone(),
            format!("Bearer {operation_api_canary}"),
            "csswitch:codex:default".to_string(),
            installer_key_canary.clone(),
            config_before.secret.clone(),
            app_authority_before.secret.clone(),
            config_after_failure.secret.clone(),
            app_authority_after_failure.secret.clone(),
        ];
        for secret in [
            config_before.secret.as_str(),
            app_authority_before.secret.as_str(),
            config_after_failure.secret.as_str(),
            app_authority_after_failure.secret.as_str(),
        ] {
            if !secret.is_empty() {
                forbidden.push(format!("Bearer {secret}"));
                forbidden.push(format!("http://127.0.0.1:{proxy_port}/{secret}"));
            }
        }
        for authority in [&installer_key_before, &installer_key_after_failure] {
            forbidden.extend(authority.values().filter_map(|entry| {
                (entry.kind == "file")
                    .then(|| String::from_utf8(entry.bytes.clone()).ok())
                    .flatten()
                    .map(|value| value.trim().to_string())
            }));
        }
        forbidden.retain(|needle| !needle.is_empty());
        forbidden.sort();
        forbidden.dedup();
        let failure_surface_credential_free = forbidden
            .iter()
            .all(|needle| !failure_surface.contains(needle.as_str()));
        let restore_auth_rejection = failure_surface.contains("rollback=")
            && failure_surface.contains("CODEX_AUTH_UNAVAILABLE")
            && failure_surface.contains("缺少本次 Codex 操作的认证 proof");
        let credential_log_free = forbidden.iter().all(|needle| {
            !gateway_publish_raw.contains(needle.as_str())
                && log_authority
                    .values()
                    .all(|entry| !String::from_utf8_lossy(&entry.bytes).contains(needle.as_str()))
        });
        let retry_surface = match &retry {
            Ok(value) => value.to_string(),
            Err(_) => "typed retry error".into(),
        };
        let opener_sink = fs::read_to_string(&open_log).unwrap_or_default();
        let emitted_event_sinks = event_sinks
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let ipc_and_event_sinks_free = forbidden.iter().all(|needle| {
            !retry_surface.contains(needle.as_str())
                && !opener_sink.contains(needle.as_str())
                && ipc_ui_sinks
                    .iter()
                    .all(|sink| !sink.contains(needle.as_str()))
                && emitted_event_sinks
                    .iter()
                    .all(|sink| !sink.contains(needle.as_str()))
        });
        assert!(
                gateway_restored
                    && launch_context_matches_prior
                    && key_fingerprint_lineage_exact
                    && operation_auth_preflight_count == 1
                    && failure_surface_credential_free
                    && gateway_publish_adapters
                    == vec!["codex".to_string(), "relay".to_string(), "codex".to_string()]
                    && prior_gateway_health
                        .as_ref()
                        .is_some_and(|health| health.provider == "codex")
                    && gateway_health_after_failure
                        .as_ref()
                        .is_some_and(|health| health.provider == "codex")
                    && credential_log_free
                    && ipc_and_event_sinks_free,
                "prior fake/local Codex Gateway must transition to a non-Codex candidate, then restore exact Codex child/context/health and ownership with candidate gone and no credential-bearing sinks: restore_auth_rejection={restore_auth_rejection}, union_preflight_once={}, failure_surface_credential_free={failure_surface_credential_free}, gateway_restored={gateway_restored}, launch_context_matches_prior={launch_context_matches_prior}, key_fingerprint_lineage_exact={key_fingerprint_lineage_exact}, adapters_exact={}, prior_health_codex={}, restored_health_codex={}, operation_sinks_free={credential_log_free}, ipc_event_sinks_free={ipc_and_event_sinks_free}",
                operation_auth_preflight_count == 1,
                gateway_publish_adapters
                    == vec!["codex".to_string(), "relay".to_string(), "codex".to_string()],
                prior_gateway_health
                    .as_ref()
                    .is_some_and(|health| health.provider == "codex"),
                gateway_health_after_failure
                    .as_ref()
                    .is_some_and(|health| health.provider == "codex")
            );
    }
    assert!(
        gateway_restored,
        "late SSH failure must restore Gateway authority"
    );
    assert!(
        science_restored,
        "late SSH failure must restore Science authority"
    );
    assert!(
        bridge_restored,
        "late SSH failure must restore managed bridge state and Science config"
    );
    assert!(
        journal_restored,
        "late SSH failure must restore the exact prior transaction journal"
    );
    assert!(
        retry_started_once,
        "retry after exact compensation must be idempotent and start one Science"
    );
    assert_eq!(
            serve_calls_after_failure,
            usize::from(matches!(
                failure_edge.as_str(),
                "serve-mutates-then-exits" | "db-health-skipped" | "post-status-receipt"
            )),
            "authority-external call log must identify whether the failed attempt reached Science serve"
        );
    assert_eq!(
        serve_calls_after_retry,
        if matches!(
            failure_edge.as_str(),
            "serve-mutates-then-exits" | "db-health-skipped" | "post-status-receipt"
        ) {
            2
        } else {
            1
        },
        "authority-external call log must count the failed attempt and one clean retry"
    );
}

#[test]
#[ignore = "explicit Acceptance-boundary serialized Codex proof recheck; temp HOME, fake auth, fake Science, and loopback only"]
fn isolated_real_ipc_rechecks_union_proof_after_serializer_wait() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let tmp = tmpdir("codex-serialized-recheck");
    let home = tmp.join("home");
    let bin_dir = tmp.join("bin");
    fs::create_dir_all(&home).unwrap();
    let fake_science = write_test_bins(&bin_dir).canonicalize().unwrap();
    let mock_upstream = start_mock_upstream();
    let (proxy_port, sandbox_port) = ssh_fixture_ports();
    let auth_log = tmp.join("codex-auth-call.log");
    let publish_log = tmp.join("gateway-publish.log");
    let open_log = tmp.join("open.log");
    fs::write(&publish_log, b"").unwrap();
    let mut env_guard = EnvGuard::new();
    env_guard.set("HOME", &home);
    env_guard.set("CSSWITCH_REPO", &root);
    env_guard.set("SCIENCE_BIN", &fake_science);
    env_guard.set("CSSWITCH_TEST_OPEN_BIN", bin_dir.join("open"));
    env_guard.set("CSSWITCH_FAKE_OPEN_LOG", &open_log);
    env_guard.set("CSSWITCH_TEST_FAKE_SCIENCE_IDENTITY", "1");
    env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
    env_guard.set("CSSWITCH_TEST_GATEWAY_PUBLISH_LOG", &publish_log);
    env_guard.set(
        "PATH",
        format!(
            "{}:/usr/bin:/bin:/usr/sbin:/sbin",
            bin_dir.to_string_lossy()
        ),
    );
    let config_dir = config::default_dir();
    let mut cfg = ssh_fixture_config(mock_upstream.port, proxy_port, sandbox_port);
    let api_canary = format!("serialized-recheck-key-{}", config::new_id());
    cfg.profile_by_id_mut("ssh-transaction").unwrap().api_key = api_canary.clone();
    cfg.secret = config::new_id();
    cfg.experimental_codex_enabled = true;
    cfg.profiles.push(Profile {
        id: "prior-codex-serialized".into(),
        name: "Prior serialized Codex".into(),
        template_id: "codex".into(),
        category: "experimental".into(),
        api_format: "openai_responses".into(),
        credential_source: crate::provider_contracts::CredentialSource::CsswitchOauth,
        credential_ref: Some("csswitch:codex:default".into()),
        model_policy: crate::provider_contracts::ModelPolicy::DynamicCatalog,
        ..Default::default()
    });
    cfg.active_id = "prior-codex-serialized".into();
    config::save_to(&config_dir, &cfg).unwrap();
    let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
    let lifecycle = Arc::new(lifecycle::Lifecycle::new());
    let supervisor = Arc::new(crate::codex_auth_supervisor::CodexAuthSupervisor::default());
    let app = tauri::test::mock_builder()
        .manage(state.clone())
        .manage(lifecycle.clone())
        .manage(supervisor)
        .invoke_handler(tauri::generate_handler![
            super::one_click_login,
            super::status,
            super::boot_snapshot
        ])
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    let handle = app.handle().clone();
    let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .unwrap();
    let real_gateway = proxy_lifecycle::gateway_bin_path(&handle).unwrap();
    let gateway_wrapper = bin_dir.join("csswitch-gateway-serialized-wrapper");
    write_executable(
        &gateway_wrapper,
        &format!(
            r#"#!/bin/sh
printf '%s %s\n' "$1" "$2" >> '{}'
if [ "$1" = "codex-auth" ] && [ "$2" = "status" ]; then
  printf '%s\n' '{{"schema_version":3,"ok":true,"command":"status","status":{{"authenticated":true,"reason":"ready","account_hash":"abababababababababababababababab","expiry_state":"valid","expires_at":2000000000,"auth_epoch":"cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd","auth_generation":1}}}}'
  exit 0
fi
exec '{}' "$@"
"#,
            auth_log.display(),
            real_gateway.display()
        ),
    );
    env_guard.set("CSSWITCH_GATEWAY_BIN", &gateway_wrapper);
    let prior_profile = config::load_from(&config_dir)
        .unwrap()
        .active_profile()
        .unwrap()
        .clone();
    let started = start_internal_gateway(&handle, &state, lifecycle.as_ref(), &prior_profile)
        .expect("internal Gateway core must establish prior Codex");
    assert_complete_gateway_receipt(&started, &prior_profile, None);
    assert!(started.catalog.expected_fingerprint.is_none());
    assert!(started.catalog.accepted_fingerprint.is_empty());
    assert_eq!(started.port, proxy_port);
    let prior = app_authority_projection(&state);
    let prior_pid = lock(&state).proxy.as_ref().map(std::process::Child::id);
    let prior_context = lock(&state).gateway_launch_context.clone();
    cfg.active_id = "ssh-transaction".into();
    config::save_to(&config_dir, &cfg).unwrap();
    fs::write(&auth_log, b"").unwrap();
    fs::write(&publish_log, b"").unwrap();
    let cleanup = RuntimeSmokeCleanup::new(
        handle.clone(),
        state.clone(),
        tmp.clone(),
        sandbox_port,
        proxy_port,
    );
    let worker_webview = webview.clone();
    let auth_log_wait = auth_log.clone();
    let config_dir_drift = config_dir.clone();
    let drift_proxy_port = loop {
        let port = free_port();
        if port != proxy_port && port != sandbox_port && port != mock_upstream.port && port != 8765
        {
            break port;
        }
    };
    let drift_port_closed_before = TcpStream::connect(("127.0.0.1", drift_proxy_port)).is_err();
    let (worker, preflight_seen) = lifecycle.with_serialized(|| {
        let worker = thread::spawn(move || {
            invoke_json(
                &worker_webview,
                "one_click_login",
                serde_json::json!({"runtimeChoice": null}),
            )
        });
        let mut seen = false;
        for _ in 0..200 {
            let count = fs::read_to_string(&auth_log_wait)
                .unwrap_or_default()
                .lines()
                .filter(|line| *line == "codex-auth status")
                .count();
            if count == 1 {
                seen = true;
                break;
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
        let mut drifted = config::load_from(&config_dir_drift).unwrap();
        drifted.proxy_port = drift_proxy_port;
        config::save_to(&config_dir_drift, &drifted).unwrap();
        (worker, seen)
    });
    let response = worker.join().expect("real IPC worker must join");
    let surface = match &response {
        Ok(value) => value.to_string(),
        Err(error) => error.clone(),
    };
    let preflight_count = fs::read_to_string(&auth_log)
        .unwrap_or_default()
        .lines()
        .filter(|line| *line == "codex-auth status")
        .count();
    let after = app_authority_projection(&state);
    let prior_child_still_running = {
        let mut authority = lock(&state);
        authority
            .proxy
            .as_mut()
            .is_some_and(|child| child.try_wait().unwrap().is_none())
    };
    let launch_context_unchanged = {
        let current = lock(&state).gateway_launch_context.clone();
        current.zip(prior_context).is_some_and(|(current, prior)| {
            current.profile == prior.profile && current.science_runtime == prior.science_runtime
        })
    };
    let child_context_unchanged = after == prior
        && lock(&state).proxy.as_ref().map(std::process::Child::id) == prior_pid
        && launch_context_unchanged
        && prior_child_still_running;
    let no_candidate_publish = fs::read_to_string(&publish_log)
        .unwrap_or_default()
        .is_empty();
    let opener_sink = fs::read_to_string(&open_log).unwrap_or_default();
    let sinks = [surface.as_str(), opener_sink.as_str()].join("\n");
    let canary_free = !sinks.contains(&api_canary)
        && !sinks.contains(&cfg.secret)
        && !sinks.contains("csswitch:codex:default");
    let rejected_typed = surface.contains("config_changed_retry");
    let drift_port_closed_after = TcpStream::connect(("127.0.0.1", drift_proxy_port)).is_err();
    cleanup
        .finish()
        .expect("serialized recheck fixture must leave no process residue");
    assert!(
            preflight_seen
                && preflight_count == 1
                && rejected_typed
                && child_context_unchanged
                && no_candidate_publish
                && canary_free
                && drift_port_closed_before
                && drift_port_closed_after,
            "after exactly one union preflight blocks on the real lifecycle serializer, a distinct free proxy_port snapshot drift must return typed config_changed_retry before Gateway/Science/authority mutation while preserving the exact prior owned child/context, keeping the drift port listener-free, and keeping sinks credential-free: preflight_seen={preflight_seen}, preflight_count={preflight_count}, rejected_typed={rejected_typed}, child_context_unchanged={child_context_unchanged}, no_candidate_publish={no_candidate_publish}, canary_free={canary_free}, drift_port_closed_before={drift_port_closed_before}, drift_port_closed_after={drift_port_closed_after}"
        );
}

const R0_PRE_SNAPSHOT_CHILD: &str = "CSSWITCH_TEST_R0_PRE_SNAPSHOT_CHILD";
const R0_PRE_SNAPSHOT_ROOT: &str = "CSSWITCH_TEST_R0_PRE_SNAPSHOT_ROOT";

fn run_r0_pre_snapshot_child(oracle: &str, tmp: &Path) {
    assert!(
        matches!(oracle, "prior-stop-error" | "snapshot-journal-exit"),
        "unknown R0 pre-snapshot oracle: {oracle}"
    );
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let home = tmp.join("home");
    let bin_dir = tmp.join("bin");
    fs::create_dir_all(&home).unwrap();
    let fake_science = write_test_bins(&bin_dir).canonicalize().unwrap();
    let mock_upstream = start_mock_upstream();
    let mut port_reservations = reserve_ssh_fixture_ports();
    let proxy_port = port_reservations.proxy_port;
    let sandbox_port = port_reservations.sandbox_port;
    let observation = tmp.join("snapshot-observation.log");
    let science_call_log = tmp.join("science-call.log");
    fs::write(tmp.join("sandbox-port"), sandbox_port.to_string()).unwrap();

    let mut env_guard = EnvGuard::new();
    env_guard.set("HOME", &home);
    env_guard.set("CSSWITCH_REPO", &root);
    env_guard.set("SCIENCE_BIN", &fake_science);
    env_guard.set("CSSWITCH_TEST_OPEN_BIN", bin_dir.join("open"));
    env_guard.set("CSSWITCH_TEST_FAKE_SCIENCE_IDENTITY", "1");
    env_guard.set("CSSWITCH_FAKE_SCIENCE_CALL_LOG", &science_call_log);
    env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
    env_guard.set(
        "PATH",
        format!(
            "{}:/usr/bin:/bin:/usr/sbin:/sbin",
            bin_dir.to_string_lossy()
        ),
    );

    let config_dir = config::default_dir();
    let mut cfg = ssh_fixture_config(mock_upstream.port, proxy_port, sandbox_port);
    cfg.reuse_system_ssh = false;
    cfg.runtime_binding = None;
    config::save_to(&config_dir, &cfg).unwrap();
    let sandbox_home = home
        .join(config::CONFIG_DIR_NAME)
        .join("sandbox")
        .join("home");
    let science_data = sandbox_home.join(".claude-science");
    fs::create_dir_all(&science_data).unwrap();
    crate::oauth_forge::ensure_virtual_login(
        &science_data,
        "virtual@localhost.invalid",
        &sandbox_home,
    )
    .unwrap();
    let stable_authority = science_data.join("orgs/csswitch-test/prior-authority.db");
    fs::create_dir_all(stable_authority.parent().unwrap()).unwrap();
    fs::write(&stable_authority, b"prior-authority-before-r0\n").unwrap();
    fs::set_permissions(&stable_authority, fs::Permissions::from_mode(0o600)).unwrap();
    let prior_runtime =
        science::select_science_runtime_cached(None, &science::ScienceVersionCache::default())
            .unwrap();
    port_reservations.release_sandbox();
    let prior_pid = start_managed_fake_science(
        &fake_science,
        &sandbox_home,
        &science_data,
        sandbox_port,
        &prior_runtime,
    );
    fs::write(tmp.join("prior.pid"), prior_pid.to_string()).unwrap();
    let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
    {
        let mut authority = lock(&state);
        authority.sandbox_port = sandbox_port;
        authority.sandbox_url = Some(format!("http://127.0.0.1:{sandbox_port}/prior"));
        authority.science_runtime = Some(prior_runtime.clone());
        authority.science_confirmed_stopped = None;
    }
    let lifecycle = Arc::new(lifecycle::Lifecycle::new());
    let app = tauri::test::mock_builder()
        .manage(state.clone())
        .manage(lifecycle.clone())
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    let handle = app.handle().clone();
    let receipt_path = config_dir.join("science-managed-launch.v1.json");

    if oracle == "prior-stop-error" {
        let _stop_seam = science::test_arm_post_stop_result_failure(config_dir.clone());
        port_reservations.release_one_click_ports();
        let result = super::one_click_login_cmd(handle, state.clone(), lifecycle.clone(), None);
        let config_after = config::load_from(&config_dir).unwrap();
        let app_after = app_authority_projection(&state);
        let snapshot_count = fs::read_dir(config_dir.join("sandbox"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".one-click-rollback-")
            })
            .count();
        let no_restart = call_count(&science_call_log, "serve") == 1;
        let exact_stopped = process_start_identity_if_alive(prior_pid).is_none()
            && TcpStream::connect(("127.0.0.1", sandbox_port)).is_err()
            && !receipt_path.exists();
        let durable_stop_outcome = config_after
            .runtime_transaction
            .as_ref()
            .and_then(config::RuntimeTransactionRecord::as_v2)
            .is_some_and(|record| {
                record.snapshot_ticket.is_none()
                    && matches!(
                        record.prior_stop,
                        config::RuntimePriorStopState::Outcome {
                            outcome: config::RuntimePriorStopOutcome::ExactStopped,
                            ..
                        }
                    )
            })
            && config::read_pending_authority_cleanup_manifest(&config_dir)
                .unwrap()
                .is_none()
            && snapshot_count == 0;
        force_cleanup_isolated_fixture(&state, tmp, sandbox_port, proxy_port);
        assert!(
            result.as_ref().is_ok_and(|value| {
                value["status"] == "error"
                    && value["recovery_status"] == "manual_recovery_required"
                    && value["message"].as_str().is_some_and(|message| {
                        message.contains(
                            "test-only post-stop failure after exact process and receipt cleanup",
                        )
                    })
            }),
            "fixture must return the injected post-stop error: {result:?}"
        );
        assert!(
            exact_stopped
                && no_restart
                && durable_stop_outcome
                && app_after.science_runtime.is_none()
                && app_after.science_confirmed_stopped == Some(prior_runtime),
            "a stop helper error after real stop/receipt cleanup must retain a durable exact-stopped outcome without snapshot creation or coordinator restart: exact_stopped={exact_stopped}, no_restart={no_restart}, durable_stop_outcome={durable_stop_outcome}, app={app_after:?}"
        );
        return;
    }

    let _capture_seam = sandbox_session::test_arm_one_click_snapshot_capture(
        config_dir.clone(),
        observation,
        false,
        prior_pid,
        receipt_path,
    );
    let _exit_seam =
        sandbox_session::test_arm_one_click_exit_after_snapshot_capture(config_dir.clone());
    port_reservations.release_one_click_ports();
    let unexpected = super::one_click_login_cmd(handle, state, lifecycle, None);
    panic!("snapshot-to-journal exit seam did not terminate the child: {unexpected:?}");
}

fn run_r0_pre_snapshot_process(test_name: &str, oracle: &str, tmp: &Path) -> std::process::Output {
    let _characterization_guard = IGNORED_RUNTIME_CHARACTERIZATION_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .env(R0_PRE_SNAPSHOT_CHILD, oracle)
        .env(R0_PRE_SNAPSHOT_ROOT, tmp)
        .output()
        .unwrap()
}

#[test]
fn r0_one_click_prior_stop_failure_has_no_pre_snapshot_restart() {
    if env::var(R0_PRE_SNAPSHOT_CHILD).ok().as_deref() == Some("prior-stop-error") {
        let tmp = PathBuf::from(env::var_os(R0_PRE_SNAPSHOT_ROOT).unwrap());
        run_r0_pre_snapshot_child("prior-stop-error", &tmp);
        return;
    }
    let tmp = tmpdir("r0-prior-stop-error");
    let output = run_r0_pre_snapshot_process(
        "commands::runtime::tests::r0_one_click_prior_stop_failure_has_no_pre_snapshot_restart",
        "prior-stop-error",
        &tmp,
    );
    let exact_test_completed = exact_child_test_completed(
        "commands::runtime::tests::r0_one_click_prior_stop_failure_has_no_pre_snapshot_restart",
        &output,
    );
    let _ = fs::remove_dir_all(&tmp);
    assert!(
        output.status.success() && exact_test_completed,
        "prior-stop characterization child failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn r0_one_click_snapshot_to_journal_boundary_is_frozen() {
    if env::var(R0_PRE_SNAPSHOT_CHILD).ok().as_deref() == Some("snapshot-journal-exit") {
        let tmp = PathBuf::from(env::var_os(R0_PRE_SNAPSHOT_ROOT).unwrap());
        run_r0_pre_snapshot_child("snapshot-journal-exit", &tmp);
        return;
    }
    let tmp = tmpdir("r0-snapshot-journal-exit");
    let output = run_r0_pre_snapshot_process(
        "commands::runtime::tests::r0_one_click_snapshot_to_journal_boundary_is_frozen",
        "snapshot-journal-exit",
        &tmp,
    );
    let config_dir = tmp.join("home").join(config::CONFIG_DIR_NAME);
    let config_after = config::load_from(&config_dir).unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(
        &config::read_pending_authority_cleanup_manifest(&config_dir)
            .unwrap()
            .expect("captured snapshot must durably register recovery"),
    )
    .unwrap();
    let entry = &manifest["entries"][0];
    let snapshot_root = PathBuf::from(entry["path"].as_str().unwrap());
    let snapshot_metadata = fs::symlink_metadata(&snapshot_root).unwrap();
    let prior_pid = fs::read_to_string(tmp.join("prior.pid"))
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    let sandbox_port = fs::read_to_string(tmp.join("sandbox-port"))
        .unwrap()
        .trim()
        .parse::<u16>()
        .unwrap();
    let observation = fs::read_to_string(tmp.join("snapshot-observation.log")).unwrap();
    let receipt_path = config_dir.join("science-managed-launch.v1.json");
    let durable_prior_stop = config_after
        .runtime_transaction
        .as_ref()
        .and_then(config::RuntimeTransactionRecord::as_v2)
        .is_some_and(|record| {
            record.snapshot_ticket.is_none()
                && matches!(
                    record.prior_stop,
                    config::RuntimePriorStopState::Outcome {
                        outcome: config::RuntimePriorStopOutcome::ExactStopped,
                        ..
                    }
                )
        });
    let boundary_exact = output.status.code() == Some(86)
        && durable_prior_stop
        && manifest["disposition"] == "active_recovery"
        && manifest["entries"]
            .as_array()
            .is_some_and(|entries| entries.len() == 1)
        && snapshot_root.parent() == Some(config_dir.join("sandbox").as_path())
        && snapshot_metadata.is_dir()
        && !snapshot_metadata.file_type().is_symlink()
        && snapshot_metadata.permissions().mode() & 0o777 == 0o700
        && process_start_identity_if_alive(prior_pid).is_none()
        && TcpStream::connect(("127.0.0.1", sandbox_port)).is_err()
        && !receipt_path.exists()
        && observation.contains("listener=stopped")
        && observation.contains("prior_process=absent")
        && observation.contains("prior_receipt=absent");
    let _ = fs::remove_dir_all(&tmp);
    assert!(
        boundary_exact,
        "exit after durable snapshot capture and before the first post-snapshot checkpoint must leave prior Science stopped, its durable exact outcome, and an ActiveRecovery snapshot: status={:?}, config_transaction_present={}, manifest={}, observation={observation:?}, stdout={}, stderr={}",
        output.status.code(),
        config_after.runtime_transaction.is_some(),
        manifest,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn r0_one_click_snapshot_capture_failure_restarts_prior_runtime() {
    run_exact_ignored_runtime_characterization(
        "commands::runtime::tests::isolated_snapshot_failure_occurs_after_verified_stop_and_restarts_prior_science",
        &[],
    );
}

#[test]
fn r2_b_one_click_pre_journal_abort_uses_registered_ticket_compensation() {
    run_exact_ignored_runtime_characterization(
        "commands::runtime::tests::isolated_snapshot_failure_occurs_after_verified_stop_and_restarts_prior_science",
        &[("CSSWITCH_TEST_R2_B_PRE_JOURNAL_ABORT", "1")],
    );
}

#[test]
fn r0_one_click_db_restart_unproven_candidate_blocks_restore() {
    run_exact_ignored_runtime_characterization(
        "commands::runtime::tests::isolated_ssh_late_failure_compensates_every_authority_and_retry_is_idempotent",
        &[(
            "CSSWITCH_TEST_SSH_LATE_FAILURE_EDGE",
            "db-restart-no-listener",
        )],
    );
}

#[test]
fn r0_one_click_post_receipt_failure_restores_prior_runtime() {
    run_exact_ignored_runtime_characterization(
        "commands::runtime::tests::isolated_late_failure_restarts_prior_managed_science_with_fresh_receipt",
        &[],
    );
}

#[test]
fn r0_one_click_cold_start_commits_runtime_and_receipts() {
    run_exact_ignored_runtime_characterization(
        "commands::runtime::tests::isolated_one_click_reuse_status_smoke_with_fake_science",
        &[("CSSWITCH_TEST_SCIENCE_ADOPTION_HEALTHY_DEFER_ONLY", "1")],
    );
    run_exact_ignored_runtime_characterization(
        "commands::runtime::tests::isolated_one_click_reuse_status_smoke_with_fake_science",
        &[("CSSWITCH_TEST_SCIENCE_ADOPTION_UNPROVEN_PENDING_ONLY", "1")],
    );
}

#[test]
fn o0_gateway_terminal_handoff_reaches_production_one_click() {
    run_exact_ignored_runtime_characterization(
        "commands::runtime::tests::isolated_one_click_reuse_status_smoke_with_fake_science",
        &[
            ("CSSWITCH_TEST_GATEWAY_TERMINAL_HANDOFF", "1"),
            ("CSSWITCH_TEST_R0_COLD_START_ONLY", "1"),
        ],
    );
}

#[test]
fn h3_finalize_failures_preserve_replayable_intent() {
    for fault in [
        "CSSWITCH_TEST_FINALIZE_PREPARE_FAILURE",
        "CSSWITCH_TEST_FINALIZE_COMPLETION_FAILURE",
    ] {
        run_exact_ignored_runtime_characterization(
            "commands::runtime::tests::isolated_one_click_reuse_status_smoke_with_fake_science",
            &[(fault, "1"), ("CSSWITCH_TEST_R0_COLD_START_ONLY", "1")],
        );
        run_exact_ignored_runtime_characterization(
            "commands::runtime::tests::isolated_r0_one_click_history_attention",
            &[(fault, "1")],
        );
    }
    for provenance_fault in ["absent", "v1", "wrong-attempt", "runtime-drift"] {
        run_exact_ignored_runtime_characterization(
            "commands::runtime::tests::isolated_one_click_reuse_status_smoke_with_fake_science",
            &[
                ("CSSWITCH_TEST_FINALIZE_COMPLETION_FAILURE", "1"),
                ("CSSWITCH_TEST_R0_COLD_START_ONLY", "1"),
                (
                    "CSSWITCH_TEST_FINALIZE_REPLAY_PROVENANCE_FAULT",
                    provenance_fault,
                ),
            ],
        );
    }
}

#[test]
#[ignore = "explicit Acceptance-boundary prior Science rollback; temp HOME, managed fake Science, and loopback only"]
fn isolated_late_failure_restarts_prior_managed_science_with_fresh_receipt() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let tmp = tmpdir("prior-science-rollback");
    let home = tmp.join("home");
    let bin_dir = tmp.join("bin");
    fs::create_dir_all(&home).unwrap();
    let fake_science = write_test_bins(&bin_dir).canonicalize().unwrap();
    let mock_upstream = start_mock_upstream();
    let mut port_reservations = reserve_ssh_fixture_ports();
    let proxy_port = port_reservations.proxy_port;
    let sandbox_port = port_reservations.sandbox_port;
    let candidate_pid_log = tmp.join("candidate-science.pid");
    let snapshot_observation = tmp.join("snapshot-observation.log");

    let mut env_guard = EnvGuard::new();
    env_guard.set("HOME", &home);
    env_guard.set("CSSWITCH_REPO", &root);
    env_guard.set("SCIENCE_BIN", &fake_science);
    env_guard.set("CSSWITCH_TEST_OPEN_BIN", bin_dir.join("open"));
    env_guard.set("CSSWITCH_TEST_FAKE_SCIENCE_IDENTITY", "1");
    env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
    env_guard.set(
        "PATH",
        format!(
            "{}:/usr/bin:/bin:/usr/sbin:/sbin",
            bin_dir.to_string_lossy()
        ),
    );

    let config_dir = config::default_dir();
    let mut cfg = ssh_fixture_config(mock_upstream.port, proxy_port, sandbox_port);
    cfg.reuse_system_ssh = false;
    cfg.runtime_binding = None;
    config::save_to(&config_dir, &cfg).unwrap();
    let sandbox_home = home
        .join(config::CONFIG_DIR_NAME)
        .join("sandbox")
        .join("home");
    let science_data = sandbox_home.join(".claude-science");
    fs::create_dir_all(&science_data).unwrap();
    crate::oauth_forge::ensure_virtual_login(
        &science_data,
        "virtual@localhost.invalid",
        &sandbox_home,
    )
    .unwrap();
    let prior_runtime =
        science::select_science_runtime_cached(None, &science::ScienceVersionCache::default())
            .unwrap();
    port_reservations.release_sandbox();
    let prior_pid = start_managed_fake_science(
        &fake_science,
        &sandbox_home,
        &science_data,
        sandbox_port,
        &prior_runtime,
    );
    let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
    {
        let mut authority = lock(&state);
        authority.sandbox_port = sandbox_port;
        authority.sandbox_url = Some(format!("http://127.0.0.1:{sandbox_port}/prior"));
        authority.science_runtime = Some(prior_runtime.clone());
        authority.science_confirmed_stopped = None;
    }
    let lifecycle = Arc::new(lifecycle::Lifecycle::new());
    let app = tauri::test::mock_builder()
        .manage(state.clone())
        .manage(lifecycle.clone())
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    let handle = app.handle().clone();

    science::test_reset_managed_launch_commit_failure_once();
    env_guard.set("CSSWITCH_TEST_MANAGED_LAUNCH_COMMIT_FAILURE_ONCE", "1");
    env_guard.set(
        "CSSWITCH_TEST_MANAGED_LAUNCH_FAILURE_PID_LOG",
        &candidate_pid_log,
    );
    let receipt_path = config_dir.join("science-managed-launch.v1.json");
    let _snapshot_seam = sandbox_session::test_arm_one_click_snapshot_capture(
        config_dir.clone(),
        snapshot_observation.clone(),
        false,
        prior_pid,
        receipt_path.clone(),
    );
    port_reservations.release_one_click_ports();
    let failed = sandbox_session::one_click_login(
        handle.clone(),
        state.clone(),
        lifecycle.as_ref(),
        None,
        None,
    );
    let candidate_pid = fs::read_to_string(&candidate_pid_log)
        .ok()
        .and_then(|pid| pid.trim().parse::<u32>().ok());
    let capture_observation = fs::read_to_string(&snapshot_observation).unwrap_or_default();
    let expected_capture_observation = format!(
            "expected_prior_pid={prior_pid}\nexpected_receipt={}\nlistener=stopped\nprior_process=absent\nprior_receipt=absent\n",
            receipt_path.display()
        );
    let restored_pid = listener_pid_if_unique(sandbox_port);
    let receipt = fs::read(&receipt_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    let receipt_pid = receipt
        .as_ref()
        .and_then(|value| value["listener_pid"].as_u64())
        .map(|pid| pid as u32);
    let receipt_port = receipt
        .as_ref()
        .and_then(|value| value["port"].as_u64())
        .map(|port| port as u16);
    let app_after = app_authority_projection(&state);
    let candidate_gone =
        candidate_pid.is_some_and(|pid| process_start_identity_if_alive(pid).is_none());
    let prior_pid_gone = process_start_identity_if_alive(prior_pid).is_none();
    let restored_healthy = restored_pid.is_some()
        && restored_pid != Some(prior_pid)
        && TcpStream::connect(("127.0.0.1", sandbox_port)).is_ok();
    let fresh_receipt = receipt_pid == restored_pid
        && receipt_port == Some(sandbox_port)
        && receipt_pid != Some(prior_pid);
    let app_consistent = app_after.science_runtime == Some(prior_runtime.clone())
        && app_after.science_confirmed_stopped.is_none()
        && app_after.sandbox_port == sandbox_port
        && app_after.sandbox_url.is_some();

    let safe_stop = {
        let mut authority = lock(&state);
        let runtime = authority.science_runtime.clone();
        let AppState {
            sandbox,
            sandbox_url,
            ..
        } = &mut *authority;
        science::stop_sandbox(
            &handle,
            sandbox,
            sandbox_url,
            science::ScienceStopRequest::recover(runtime.as_ref()),
        )
    };
    let stopped_cleanly = safe_stop.is_ok()
        && TcpStream::connect(("127.0.0.1", sandbox_port)).is_err()
        && !receipt_path.exists();
    force_cleanup_isolated_fixture(&state, &tmp, sandbox_port, proxy_port);

    assert!(
        failed.as_ref().is_err_and(|error| {
            error
                .to_string()
                .contains("test-only managed launch commit failure")
        }),
        "fixture must reach the post-status managed-receipt failure: {failed:?}"
    );
    assert!(
            restored_healthy
                && fresh_receipt
                && app_consistent
                && candidate_gone
                && prior_pid_gone
                && capture_observation == expected_capture_observation
                && stopped_cleanly,
            "late failure after stop-old must quiesce the exact prior process before capture, restart prior Science with a fresh receipt and exact ownership, and leave no orphan daemon: capture={capture_observation:?}, prior_pid={prior_pid}, candidate_pid={candidate_pid:?}, restored_pid={restored_pid:?}, receipt_pid={receipt_pid:?}, prior_gone={prior_pid_gone}, app={app_after:?}, safe_stop={safe_stop:?}"
        );
}

fn run_prior_restart_failure_oracle(oracle: &str) {
    assert!(
        matches!(oracle, "cleanup-degraded" | "post-spawn-failure"),
        "unknown prior restart failure oracle: {oracle}"
    );
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let tmp = tmpdir(&format!("prior-restart-{oracle}"));
    let home = tmp.join("home");
    let bin_dir = tmp.join("bin");
    fs::create_dir_all(&home).unwrap();
    let fake_science = write_test_bins(&bin_dir).canonicalize().unwrap();
    let mock_upstream = start_mock_upstream();
    let (proxy_port, sandbox_port) = ssh_fixture_ports();
    let candidate_pid_log = tmp.join("candidate-science.pid");
    let snapshot_observation = tmp.join("snapshot-observation.log");
    let cleanup_log = tmp.join("snapshot-cleanup.log");

    let mut env_guard = EnvGuard::new();
    env_guard.set("HOME", &home);
    env_guard.set("CSSWITCH_REPO", &root);
    env_guard.set("SCIENCE_BIN", &fake_science);
    env_guard.set("CSSWITCH_TEST_OPEN_BIN", bin_dir.join("open"));
    env_guard.set("CSSWITCH_TEST_FAKE_SCIENCE_IDENTITY", "1");
    env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
    env_guard.set(
        "PATH",
        format!(
            "{}:/usr/bin:/bin:/usr/sbin:/sbin",
            bin_dir.to_string_lossy()
        ),
    );

    let config_dir = config::default_dir();
    let mut cfg = ssh_fixture_config(mock_upstream.port, proxy_port, sandbox_port);
    cfg.reuse_system_ssh = false;
    cfg.runtime_binding = None;
    config::save_to(&config_dir, &cfg).unwrap();
    let sandbox_home = home
        .join(config::CONFIG_DIR_NAME)
        .join("sandbox")
        .join("home");
    let science_data = sandbox_home.join(".claude-science");
    fs::create_dir_all(&science_data).unwrap();
    crate::oauth_forge::ensure_virtual_login(
        &science_data,
        "virtual@localhost.invalid",
        &sandbox_home,
    )
    .unwrap();
    let stable_authority = science_data.join("orgs/csswitch-test/stable-authority.db");
    fs::create_dir_all(stable_authority.parent().unwrap()).unwrap();
    fs::write(&stable_authority, b"stable-private-authority\n").unwrap();
    fs::set_permissions(&stable_authority, fs::Permissions::from_mode(0o600)).unwrap();
    let stable_before = fs::read(&stable_authority).unwrap();
    let prior_runtime =
        science::select_science_runtime_cached(None, &science::ScienceVersionCache::default())
            .unwrap();
    let prior_pid = start_managed_fake_science(
        &fake_science,
        &sandbox_home,
        &science_data,
        sandbox_port,
        &prior_runtime,
    );
    let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
    {
        let mut authority = lock(&state);
        authority.sandbox_port = sandbox_port;
        authority.sandbox_url = Some(format!("http://127.0.0.1:{sandbox_port}/prior"));
        authority.science_runtime = Some(prior_runtime.clone());
        authority.science_confirmed_stopped = None;
    }
    let lifecycle = Arc::new(lifecycle::Lifecycle::new());
    let app = tauri::test::mock_builder()
        .manage(state.clone())
        .manage(lifecycle.clone())
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    let handle = app.handle().clone();
    let prior_profile = cfg.active_profile().unwrap().clone();
    let prior_gateway = proxy_lifecycle::GatewayController::start_for(
        &handle,
        &state,
        lifecycle.as_ref(),
        &prior_profile,
        Some(&prior_runtime),
        None,
        None,
    )
    .unwrap();
    let prior_gateway_action = prior_gateway.action;
    assert!(matches!(
        prior_gateway_action,
        crate::runtime::proxy::ProxyAction::Restarted
    ));
    let prior_gateway_pid = lock(&state).proxy.as_ref().map(std::process::Child::id);
    let prior_gateway_health = crate::proc::http_gateway_health(
        proxy_port,
        Some(&lock(&state).secret),
        crate::runtime::operation::LOCAL_HEALTH_TIMEOUT_MS,
    )
    .expect("prior Gateway must be healthy before the late failure");
    let config_before = config::load_from(&config_dir).unwrap();

    science::test_reset_managed_launch_commit_failure_once();
    env_guard.set("CSSWITCH_TEST_MANAGED_LAUNCH_COMMIT_FAILURE_ONCE", "1");
    env_guard.set(
        "CSSWITCH_TEST_MANAGED_LAUNCH_FAILURE_PID_LOG",
        &candidate_pid_log,
    );
    let receipt_path = config_dir.join("science-managed-launch.v1.json");
    let _snapshot_seam = sandbox_session::test_arm_one_click_snapshot_capture(
        config_dir.clone(),
        snapshot_observation.clone(),
        false,
        prior_pid,
        receipt_path.clone(),
    );
    let _cleanup_seam = (oracle == "cleanup-degraded").then(|| {
        sandbox_session::test_arm_authority_snapshot_cleanup_fault(
            tmp.clone(),
            "persistent",
            cleanup_log.clone(),
        )
    });
    let _restart_seam = (oracle == "post-spawn-failure")
        .then(|| sandbox_session::test_arm_prior_restart_post_spawn_failure(sandbox_port));

    let failed = sandbox_session::one_click_login(
        handle.clone(),
        state.clone(),
        lifecycle.as_ref(),
        None,
        None,
    );
    let candidate_pid = fs::read_to_string(&candidate_pid_log)
        .ok()
        .and_then(|pid| pid.trim().parse::<u32>().ok());
    let capture_observation = fs::read_to_string(&snapshot_observation).unwrap_or_default();
    let expected_capture_observation = format!(
            "expected_prior_pid={prior_pid}\nexpected_receipt={}\nlistener=stopped\nprior_process=absent\nprior_receipt=absent\n",
            receipt_path.display()
        );
    let listener_after_failure = listener_pid_if_unique(sandbox_port);
    let verified_restart_identity = sandbox_session::test_prior_restart_post_spawn_identity();
    let verified_restart_identity_absent =
        verified_restart_identity
            .as_ref()
            .is_some_and(|(pid, process_start)| {
                listener_after_failure != Some(*pid)
                    && science::test_process_start_identity_for_pid(*pid).as_ref()
                        != Some(process_start)
            });
    let receipt = fs::read(&receipt_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    let receipt_pid = receipt
        .as_ref()
        .and_then(|value| value["listener_pid"].as_u64())
        .map(|pid| pid as u32);
    let app_after = app_authority_projection(&state);
    let candidate_gone =
        candidate_pid.is_some_and(|pid| process_start_identity_if_alive(pid).is_none());
    let prior_gone = process_start_identity_if_alive(prior_pid).is_none();
    let stable_file_restored = fs::read(&stable_authority)
        .is_ok_and(|bytes| bytes == stable_before)
        && fs::metadata(&stable_authority)
            .is_ok_and(|metadata| metadata.permissions().mode() & 0o777 == 0o600);
    let config_after = config::load_from(&config_dir).ok();
    let expected_failed_step = if oracle == "cleanup-degraded" {
        config::RuntimeCompensationStep::SnapshotCleanup
    } else {
        config::RuntimeCompensationStep::PriorScienceRestart
    };
    let durable_compensation_record = config_after
        .as_ref()
        .and_then(|current| current.runtime_compensation.as_ref())
        .is_some_and(|record| {
            record.state
                == config::RuntimeCompensationState::Incomplete {
                    failed_steps: vec![expected_failed_step],
                }
        });
    let mut config_after_without_journal = config_after.clone();
    if let Some(current) = config_after_without_journal.as_mut() {
        current.runtime_compensation = None;
    }
    let stable_authority_restored = stable_file_restored
        && durable_compensation_record
        && config_after_without_journal.as_ref() == Some(&config_before);
    let gateway_after = {
        let authority = lock(&state);
        crate::proc::http_gateway_health(
            authority.proxy_port,
            Some(&authority.secret),
            crate::runtime::operation::LOCAL_HEALTH_TIMEOUT_MS,
        )
    };
    let gateway_restored = lock(&state).proxy.as_ref().map(std::process::Child::id)
        == prior_gateway_pid
        && gateway_after.as_ref().is_some_and(|health| {
            health.gateway == prior_gateway_health.gateway
                && health.provider == prior_gateway_health.provider
                && health.launch_id == prior_gateway_health.launch_id
        });
    let cleanup_line = fs::read_to_string(&cleanup_log)
        .unwrap_or_default()
        .lines()
        .last()
        .unwrap_or_default()
        .to_string();
    let cleanup_residual = cleanup_line
        .split('\t')
        .nth(2)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    let cleanup_residual_private = cleanup_residual.as_ref().is_some_and(|path| {
        fs::symlink_metadata(path).is_ok_and(|metadata| {
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.permissions().mode() & 0o777 == 0o700
        })
    });
    let cleanup_tracked = cleanup_residual.as_ref().is_some_and(|path| {
        lock(&state)
            .pending_authority_cleanup
            .iter()
            .any(|pending| pending == path)
    });
    let result_surface = match &failed {
        Ok(value) => value.to_string(),
        Err(error) => error.to_string(),
    };

    let safe_stop = {
        let mut authority = lock(&state);
        let runtime = authority.science_runtime.clone();
        let AppState {
            sandbox,
            sandbox_url,
            ..
        } = &mut *authority;
        science::stop_sandbox(
            &handle,
            sandbox,
            sandbox_url,
            science::ScienceStopRequest::recover(runtime.as_ref()),
        )
    };
    let listener_closed_after_safe_stop = TcpStream::connect(("127.0.0.1", sandbox_port)).is_err();
    force_cleanup_isolated_fixture(&state, &tmp, sandbox_port, proxy_port);

    assert!(
        failed.as_ref().is_err_and(|error| {
            error
                .to_string()
                .contains("test-only managed launch commit failure")
        }),
        "fixture must reach the post-status managed-receipt failure: {failed:?}"
    );
    assert_eq!(
        capture_observation, expected_capture_observation,
        "prior Science must be exactly quiesced before the authority snapshot"
    );
    assert!(
            stable_authority_restored && gateway_restored && candidate_gone && prior_gone,
            "authority and prior Gateway must restore before the prior Science recovery decision while retaining only the durable compensation journal: stable={stable_authority_restored}, stable_file={stable_file_restored}, config_equal_without_journal={}, durable_compensation={durable_compensation_record}, gateway={gateway_restored}, candidate_gone={candidate_gone}, prior_gone={prior_gone}, gateway_health_present={}",
            config_after_without_journal.as_ref() == Some(&config_before),
            gateway_after.is_some()
        );
    if oracle == "cleanup-degraded" {
        let prior_restarted = listener_after_failure.is_some()
            && listener_after_failure != Some(prior_pid)
            && receipt_pid == listener_after_failure
            && app_after.science_runtime.as_ref() == Some(&prior_runtime)
            && app_after.science_confirmed_stopped.is_none()
            && safe_stop.is_ok()
            && listener_closed_after_safe_stop;
        let degraded_surface =
            result_surface.contains("degraded") && result_surface.contains("cleanup_required");
        let cleanup_path_reported = cleanup_residual
            .as_ref()
            .is_some_and(|path| result_surface.contains(&path.to_string_lossy().to_string()));
        assert!(
                prior_restarted
                    && cleanup_residual_private
                    && cleanup_tracked
                    && degraded_surface
                    && cleanup_path_reported,
                "persistent snapshot cleanup failure after successful authority/Gateway restore must still restart prior Science with a fresh receipt and exact AppState, while returning explicit degraded/cleanup_required with one tracked private residual: listener_present={}, receipt_matches_listener={}, app_runtime_restored={}, app_confirmed_stopped={}, degraded_surface={degraded_surface}, cleanup_path_reported={cleanup_path_reported}, private={cleanup_residual_private}, tracked={cleanup_tracked}, safe_stop={safe_stop:?}",
                listener_after_failure.is_some(),
                receipt_pid == listener_after_failure,
                app_after.science_runtime.as_ref() == Some(&prior_runtime),
                app_after.science_confirmed_stopped.is_some()
            );
    } else {
        let honest_stopped = listener_after_failure.is_none()
            && !receipt_path.exists()
            && app_after.science_runtime.is_none()
            && app_after.science_confirmed_stopped.as_ref() == Some(&prior_runtime);
        assert!(
                failed.as_ref().is_err_and(|error| {
                    error
                        .to_string()
                        .contains("test-only prior Science post-spawn validation failure")
                }) && verified_restart_identity_absent
                    && honest_stopped,
                "a post-spawn prior restart validation failure must clean the exact verified candidate PID/process-start identity and port, leave no receipt, and keep AppState honestly stopped: result={failed:?}, verified_identity_recorded={}, verified_identity_absent={verified_restart_identity_absent}, listener_present={}, receipt_present={}, app_runtime_present={}, app_confirmed_stopped={}",
                verified_restart_identity.is_some(),
                listener_after_failure.is_some(),
                receipt.is_some(),
                app_after.science_runtime.is_some(),
                app_after.science_confirmed_stopped.is_some()
            );
    }
}

#[test]
#[ignore = "explicit Acceptance-boundary prior Science cleanup degradation; temp HOME, managed fake Science, real local Gateway, and loopback only"]
fn isolated_prior_science_restart_survives_snapshot_cleanup_degradation() {
    run_prior_restart_failure_oracle("cleanup-degraded");
}

#[test]
#[ignore = "explicit Acceptance-boundary prior Science restart cleanup; temp HOME, managed fake Science, and loopback only"]
fn isolated_prior_science_post_spawn_failure_cleans_candidate() {
    run_prior_restart_failure_oracle("post-spawn-failure");
}

#[test]
#[ignore = "explicit Acceptance-boundary snapshot quiesce rollback; temp HOME, managed fake Science, and loopback only"]
fn isolated_snapshot_failure_occurs_after_verified_stop_and_restarts_prior_science() {
    let pre_journal_abort = env::var("CSSWITCH_TEST_R2_B_PRE_JOURNAL_ABORT")
        .ok()
        .as_deref()
        == Some("1");
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let tmp = tmpdir("snapshot-quiesce-rollback");
    let home = tmp.join("home");
    let bin_dir = tmp.join("bin");
    fs::create_dir_all(&home).unwrap();
    let fake_science = write_test_bins(&bin_dir).canonicalize().unwrap();
    let mock_upstream = start_mock_upstream();
    let mut port_reservations = reserve_ssh_fixture_ports();
    let proxy_port = port_reservations.proxy_port;
    let sandbox_port = port_reservations.sandbox_port;
    let snapshot_observation = tmp.join("snapshot-observation.log");

    let mut env_guard = EnvGuard::new();
    env_guard.set("HOME", &home);
    env_guard.set("CSSWITCH_REPO", &root);
    env_guard.set("SCIENCE_BIN", &fake_science);
    env_guard.set("CSSWITCH_TEST_OPEN_BIN", bin_dir.join("open"));
    env_guard.set("CSSWITCH_TEST_FAKE_SCIENCE_IDENTITY", "1");
    env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
    env_guard.set(
        "PATH",
        format!(
            "{}:/usr/bin:/bin:/usr/sbin:/sbin",
            bin_dir.to_string_lossy()
        ),
    );

    let config_dir = config::default_dir();
    let mut cfg = ssh_fixture_config(mock_upstream.port, proxy_port, sandbox_port);
    cfg.reuse_system_ssh = false;
    cfg.runtime_binding = None;
    config::save_to(&config_dir, &cfg).unwrap();
    let sandbox_home = home
        .join(config::CONFIG_DIR_NAME)
        .join("sandbox")
        .join("home");
    let science_data = sandbox_home.join(".claude-science");
    fs::create_dir_all(&science_data).unwrap();
    crate::oauth_forge::ensure_virtual_login(
        &science_data,
        "virtual@localhost.invalid",
        &sandbox_home,
    )
    .unwrap();
    let stable_authority = science_data.join("orgs/csswitch-test/prior-authority.db");
    fs::create_dir_all(stable_authority.parent().unwrap()).unwrap();
    fs::write(&stable_authority, b"prior-authority-bytes\n").unwrap();
    fs::set_permissions(&stable_authority, fs::Permissions::from_mode(0o600)).unwrap();
    let stable_bytes_before = fs::read(&stable_authority).unwrap();
    let stable_mode_before = fs::metadata(&stable_authority)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    let config_before = config::load_from(&config_dir).unwrap();
    let prior_runtime =
        science::select_science_runtime_cached(None, &science::ScienceVersionCache::default())
            .unwrap();
    port_reservations.release_sandbox();
    let prior_pid = start_managed_fake_science(
        &fake_science,
        &sandbox_home,
        &science_data,
        sandbox_port,
        &prior_runtime,
    );
    let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
    {
        let mut authority = lock(&state);
        authority.sandbox_port = sandbox_port;
        authority.sandbox_url = Some(format!("http://127.0.0.1:{sandbox_port}/prior"));
        authority.science_runtime = Some(prior_runtime.clone());
        authority.science_confirmed_stopped = None;
    }
    let lifecycle = Arc::new(lifecycle::Lifecycle::new());
    let app = tauri::test::mock_builder()
        .manage(state.clone())
        .manage(lifecycle.clone())
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    let handle = app.handle().clone();
    let receipt_path = config_dir.join("science-managed-launch.v1.json");
    let _snapshot_seam = sandbox_session::test_arm_one_click_snapshot_capture(
        config_dir.clone(),
        snapshot_observation.clone(),
        !pre_journal_abort,
        prior_pid,
        receipt_path.clone(),
    );
    let _first_journal_seam = pre_journal_abort
        .then(|| sandbox_session::test_arm_one_click_first_journal_failure(config_dir.clone()));

    port_reservations.release_one_click_ports();
    let failed = sandbox_session::one_click_login(
        handle.clone(),
        state.clone(),
        lifecycle.as_ref(),
        None,
        None,
    );
    let observation = fs::read_to_string(&snapshot_observation).unwrap_or_default();
    let expected_observation = format!(
            "expected_prior_pid={prior_pid}\nexpected_receipt={}\nlistener=stopped\nprior_process=absent\nprior_receipt=absent\n",
            receipt_path.display()
        );
    let restored_pid = listener_pid_if_unique(sandbox_port);
    let receipt_pid = fs::read(&receipt_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| value["listener_pid"].as_u64())
        .map(|pid| pid as u32);
    let app_after = app_authority_projection(&state);
    let stable_authority_unchanged = fs::read(&stable_authority).unwrap() == stable_bytes_before
        && fs::metadata(&stable_authority)
            .unwrap()
            .permissions()
            .mode()
            & 0o777
            == stable_mode_before
        && config::load_from(&config_dir).unwrap() == config_before;
    let restarted_after_quiesce = observation == expected_observation
        && restored_pid.is_some()
        && restored_pid != Some(prior_pid)
        && process_start_identity_if_alive(prior_pid).is_none()
        && receipt_pid == restored_pid
        && app_after.science_runtime == Some(prior_runtime.clone())
        && app_after.science_confirmed_stopped.is_none()
        && TcpStream::connect(("127.0.0.1", sandbox_port)).is_ok();
    let safe_stop = {
        let mut authority = lock(&state);
        let runtime = authority.science_runtime.clone();
        let AppState {
            sandbox,
            sandbox_url,
            ..
        } = &mut *authority;
        science::stop_sandbox(
            &handle,
            sandbox,
            sandbox_url,
            science::ScienceStopRequest::recover(runtime.as_ref()),
        )
    };
    let stopped_cleanly = safe_stop.is_ok()
        && TcpStream::connect(("127.0.0.1", sandbox_port)).is_err()
        && !receipt_path.exists();
    let cleanup_manifest_empty = config::read_pending_authority_cleanup_manifest(&config_dir)
        .unwrap()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| value["entries"].as_array().map(Vec::is_empty))
        .unwrap_or(false);
    let cleanup_manifest_contract = !pre_journal_abort || cleanup_manifest_empty;
    force_cleanup_isolated_fixture(&state, &tmp, sandbox_port, proxy_port);

    let expected_failure = if pre_journal_abort {
        "test-only one-click first journal write failure"
    } else {
        "test-only one-click authority snapshot capture failure"
    };
    assert!(
        failed
            .as_ref()
            .is_err_and(|error| error.to_string().contains(expected_failure)),
        "fixture must reach the injected snapshot or first-journal failure: mode_pre_journal={pre_journal_abort}, result={failed:?}"
    );
    assert!(
            restarted_after_quiesce
                && stable_authority_unchanged
                && stopped_cleanly
                && cleanup_manifest_contract,
            "snapshot capture failure or same-process PreJournalAbort must occur only after verified stop, preserve stable authority, use the registered ticket to clean the snapshot, restart prior Science with fresh ownership, leave no orphan prior PID, and remain safely stoppable: mode_pre_journal={pre_journal_abort}, observation={observation:?}, prior_pid={prior_pid}, restored_pid={restored_pid:?}, receipt_pid={receipt_pid:?}, app={app_after:?}, safe_stop={safe_stop:?}, cleanup_manifest_empty={cleanup_manifest_empty}"
        );
}

#[test]
fn r0_history_restore_pre_stop_rejection_preserves_runtime() {
    run_exact_ignored_runtime_characterization(
        "commands::runtime::tests::isolated_r0_history_restore_command_contract",
        &[(
            "CSSWITCH_TEST_R0_HISTORY_RESTORE_ORACLE",
            "pre-stop-rejection",
        )],
    );
}

#[test]
fn r0_history_restore_post_stop_config_drift_leaves_science_stopped() {
    run_exact_ignored_runtime_characterization(
        "commands::runtime::tests::isolated_r0_history_restore_command_contract",
        &[(
            "CSSWITCH_TEST_R0_HISTORY_RESTORE_ORACLE",
            "post-stop-config-drift",
        )],
    );
}

#[test]
fn r0_history_restore_post_stop_candidate_and_credential_failures_leave_science_stopped() {
    for oracle in ["candidate-revalidation-failure", "credential-write-failure"] {
        run_exact_ignored_runtime_characterization(
            "commands::runtime::tests::isolated_r0_history_restore_command_contract",
            &[("CSSWITCH_TEST_R0_HISTORY_RESTORE_ORACLE", oracle)],
        );
    }
}

#[test]
fn r0_history_restore_rotates_all_references_only_after_success() {
    run_exact_ignored_runtime_characterization(
        "commands::runtime::tests::isolated_r0_history_restore_command_contract",
        &[("CSSWITCH_TEST_R0_HISTORY_RESTORE_ORACLE", "success")],
    );
}

#[test]
fn f1_a_history_restore_and_resume_is_one_backend_operation() {
    run_exact_ignored_runtime_characterization(
        "commands::runtime::tests::isolated_r0_history_restore_command_contract",
        &[("CSSWITCH_TEST_R0_HISTORY_RESTORE_ORACLE", "resume-success")],
    );
}

#[test]
fn api_only_legacy_history_recovery_precedes_account_rejection() {
    run_exact_ignored_runtime_characterization(
        "commands::runtime::tests::isolated_r0_history_restore_command_contract",
        &[(
            "CSSWITCH_TEST_R0_HISTORY_RESTORE_ORACLE",
            "credential-crash-replay",
        )],
    );
}

#[test]
fn f1_a_history_crash_and_concurrent_config_recovery_is_owned() {
    for oracle in [
        "post-snapshot-config-drift",
        "credential-crash-replay",
        "credential-crash-replay-config-drift",
        "credential-crash-replay-config-race",
        "credential-preauth-race-replay",
        "cleanup-prepare-failure-resume",
        "finalize-completion-failure-restore-only",
        "finalize-completion-failure-resume",
    ] {
        run_exact_ignored_runtime_characterization(
            "commands::runtime::tests::isolated_r0_history_restore_command_contract",
            &[("CSSWITCH_TEST_R0_HISTORY_RESTORE_ORACLE", oracle)],
        );
    }
}

#[test]
fn o1_e4_history_full_snapshot_replay_resumes_from_durable_restore_intent() {
    run_exact_ignored_runtime_characterization(
        "commands::runtime::tests::isolated_r0_history_restore_command_contract",
        &[(
            "CSSWITCH_TEST_R0_HISTORY_RESTORE_ORACLE",
            "credential-crash-replay-restore-interrupt",
        )],
    );
}

#[test]
fn r0_one_click_history_attention_commits_choice_session_without_starting_science() {
    run_exact_ignored_runtime_characterization(
        "commands::runtime::tests::isolated_r0_one_click_history_attention",
        &[],
    );
}

#[test]
#[ignore = "explicit Acceptance-boundary one-click history attention; temp HOME, production one-click IPC, fake Science identity, and dynamic loopback only"]
fn isolated_r0_one_click_history_attention() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let tmp = tmpdir("r0-one-click-history-attention");
    let home = tmp.join("home");
    let bin_dir = tmp.join("bin");
    fs::create_dir_all(&home).unwrap();
    let fake_science = write_test_bins(&bin_dir).canonicalize().unwrap();
    let mock_upstream = start_mock_upstream();
    let mut port_reservations = reserve_ssh_fixture_ports();
    let proxy_port = port_reservations.proxy_port;
    let sandbox_port = port_reservations.sandbox_port;
    let science_call_log = tmp.join("science-call.log");
    let finalize_prepare_failure = env::var_os("CSSWITCH_TEST_FINALIZE_PREPARE_FAILURE").is_some();
    let finalize_completion_failure =
        env::var_os("CSSWITCH_TEST_FINALIZE_COMPLETION_FAILURE").is_some();
    let mut env_guard = EnvGuard::new();
    env_guard.set("HOME", &home);
    env_guard.set("CSSWITCH_REPO", &root);
    env_guard.set("SCIENCE_BIN", &fake_science);
    env_guard.set("CSSWITCH_FAKE_SCIENCE_CALL_LOG", &science_call_log);
    env_guard.set("CSSWITCH_TEST_FAKE_SCIENCE_IDENTITY", "1");
    env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
    env_guard.set(
        "PATH",
        format!(
            "{}:/usr/bin:/bin:/usr/sbin:/sbin",
            bin_dir.to_string_lossy()
        ),
    );

    let config_dir = config::default_dir();
    let mut cfg = ssh_fixture_config(mock_upstream.port, proxy_port, sandbox_port);
    cfg.reuse_system_ssh = false;
    cfg.runtime_binding = None;
    cfg.runtime_transaction = None;
    config::save_to(&config_dir, &cfg).unwrap();
    let sandbox_home = config_dir.join("sandbox").join("home");
    let science_data = sandbox_home.join(".claude-science");
    fs::create_dir_all(&science_data).unwrap();
    let (forged, _) = crate::oauth_forge::ensure_virtual_login(
        &science_data,
        "virtual@localhost.invalid",
        &sandbox_home,
    )
    .unwrap();
    for org in [
        "33333333-3333-4333-8333-333333333333",
        "44444444-4444-4444-8444-444444444444",
    ] {
        fs::create_dir_all(science_data.join("orgs").join(org)).unwrap();
    }
    fs::remove_file(&forged.enc_file).unwrap();
    fs::remove_file(science_data.join("active-org.json")).unwrap();
    let marker = config_dir
        .join("sandbox")
        .join("state")
        .join("virtual-org.v1.json");
    fs::remove_file(&marker).unwrap();

    let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
    let lifecycle = Arc::new(lifecycle::Lifecycle::new());
    let app = tauri::test::mock_builder()
        .manage(state.clone())
        .manage(lifecycle)
        .invoke_handler(tauri::generate_handler![
            super::one_click_login,
            super::restore_history_choice
        ])
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .unwrap();
    let prepare_failure_guard = finalize_prepare_failure.then(|| {
        config::test_arm_pending_cleanup_lifecycle(Some(
            config::PendingCleanupPublishFault::Prepare,
        ))
    });
    let completion_failure_guard = finalize_completion_failure.then(|| {
        sandbox_session::test_arm_one_click_finalize_completion_failure(config_dir.clone())
    });
    port_reservations.release_one_click_ports();
    let attention = invoke_json(
        &webview,
        "one_click_login",
        serde_json::json!({"runtimeChoice": null}),
    )
    .unwrap();
    if finalize_prepare_failure || finalize_completion_failure {
        assert_eq!(attention["status"], "degraded");
        assert_eq!(attention["action"], "history_choice_required");
        assert_eq!(attention["recovery_status"], "manual_recovery_required");
        let pending = config::load_from(&config_dir).unwrap();
        let pending_record = pending
            .runtime_transaction
            .as_ref()
            .and_then(config::RuntimeTransactionRecord::as_v2)
            .expect("history finalize failure must retain the exact V2 record");
        assert_eq!(pending_record.target_profile_id, cfg.active_id);
        assert!(matches!(
            pending_record.finalize,
            config::RuntimeFinalizeState::Intent {
                action: config::RuntimeFinalizeAction::ClearJournal
            }
        ));
        assert!(lock(&state).history_recovery.is_some());
        let manifest = config::read_pending_authority_cleanup_manifest(&config_dir)
            .unwrap()
            .unwrap();
        let manifest: serde_json::Value = serde_json::from_slice(&manifest).unwrap();
        if finalize_prepare_failure {
            assert_eq!(manifest["disposition"], "active_recovery");
            assert_eq!(manifest["entries"].as_array().map(Vec::len), Some(1));
        } else {
            assert_eq!(manifest["entries"].as_array().map(Vec::len), Some(0));
        }
        drop(prepare_failure_guard);
        drop(completion_failure_guard);
        let replayed = invoke_json(
            &webview,
            "one_click_login",
            serde_json::json!({"runtimeChoice": null}),
        )
        .expect("next production history command must replay finalize first");
        assert_eq!(replayed["status"], "attention");
        let finalized = config::load_from(&config_dir).unwrap();
        assert!(finalized.runtime_transaction.is_none());
        assert!(finalized.runtime_binding.is_none());
        assert!(lock(&state).history_recovery.is_some());
        fs::remove_dir_all(&tmp).unwrap();
        return;
    }
    let returned_references = attention["choices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|choice| choice["reference"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    let (session_references, stopped_runtime, runtime_absent, child_absent) = {
        let authority = lock(&state);
        (
            authority
                .history_recovery
                .as_ref()
                .expect("production one-click must commit the history choice session")
                .choices
                .iter()
                .map(|choice| choice.reference.clone())
                .collect::<Vec<_>>(),
            authority.science_confirmed_stopped.clone(),
            authority.science_runtime.is_none(),
            authority.sandbox.is_none(),
        )
    };
    let after = config::load_from(&config_dir).unwrap();
    assert_eq!(attention["status"], "attention");
    assert_eq!(attention["action"], "history_choice_required");
    assert_eq!(attention["stage"], "history_recovery");
    assert_eq!(returned_references.len(), 2);
    assert_eq!(session_references, returned_references);
    assert!(
        stopped_runtime.is_none(),
        "history attention without a prior verified stop must not invent confirmed authority"
    );
    assert!(runtime_absent && child_absent);
    assert!(lock(&state).proxy.is_none());
    assert!(listener_pid_if_unique(sandbox_port).is_none());
    assert!(!config_dir.join("science-managed-launch.v1.json").exists());
    assert!(after.runtime_binding.is_none());
    assert!(after.runtime_transaction.is_none());
    assert!(
        !fs::read_to_string(&science_call_log)
            .unwrap_or_default()
            .lines()
            .any(|line| line == "serve"),
        "history attention must stop before Science launch"
    );
    assert!(!marker.exists());
    let restored = invoke_json(
        &webview,
        "restore_history_choice",
        serde_json::json!({"reference": returned_references[0]}),
    )
    .expect("typed no-managed-runtime proof must authorize the attention-to-restore continuation");
    assert_eq!(restored["status"], "ok");
    assert_eq!(restored["action"], "history_choice_restored");
    let second_reference = restored["choices"][0]["reference"]
        .as_str()
        .unwrap()
        .to_string();
    let runtime =
        science::select_science_runtime_cached(None, &science::ScienceVersionCache::default())
            .unwrap();
    let drifted_pid = start_managed_fake_science(
        &fake_science,
        &sandbox_home,
        &science_data,
        sandbox_port,
        &runtime,
    );
    {
        let mut authority = lock(&state);
        authority.science_runtime = Some(runtime.clone());
        authority.sandbox_port = sandbox_port;
        authority.sandbox_url = Some(format!("http://127.0.0.1:{sandbox_port}/history-drift"));
    }
    let restored_after_live_drift = invoke_json(
        &webview,
        "restore_history_choice",
        serde_json::json!({"reference": second_reference}),
    )
    .expect("a newly live managed runtime must be exact-stopped before history restore");
    assert_eq!(restored_after_live_drift["status"], "ok");
    assert!(process_start_identity_if_alive(drifted_pid).is_none());
    {
        let authority = lock(&state);
        assert_eq!(authority.science_confirmed_stopped.as_ref(), Some(&runtime));
        assert!(matches!(
            authority
                .history_recovery
                .as_ref()
                .map(|session| &session.science_quiescence),
            Some(crate::HistoryRecoveryScienceQuiescence::ExactStopped(expected))
                if expected == &runtime
        ));
    }
    let third_reference = restored_after_live_drift["choices"][0]["reference"]
        .as_str()
        .unwrap()
        .to_string();
    let restored_from_advanced_proof = invoke_json(
        &webview,
        "restore_history_choice",
        serde_json::json!({"reference": third_reference}),
    )
    .expect("rotated references must reuse the advanced exact-stopped session proof");
    assert_eq!(restored_from_advanced_proof["status"], "ok");
    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
#[ignore = "explicit Acceptance-boundary history restore command contract; temp HOME, managed fake Science, and loopback only"]
fn isolated_r0_history_restore_command_contract() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let oracle =
        env::var("CSSWITCH_TEST_R0_HISTORY_RESTORE_ORACLE").unwrap_or_else(|_| "success".into());
    assert!(matches!(
        oracle.as_str(),
        "pre-stop-rejection"
            | "post-stop-config-drift"
            | "candidate-revalidation-failure"
            | "credential-write-failure"
            | "post-snapshot-config-drift"
            | "credential-crash-replay"
            | "credential-crash-replay-restore-interrupt"
            | "credential-crash-replay-config-drift"
            | "credential-crash-replay-config-race"
            | "credential-preauth-race-replay"
            | "cleanup-prepare-failure-resume"
            | "finalize-completion-failure-restore-only"
            | "finalize-completion-failure-resume"
            | "success"
            | "resume-success"
    ));
    let tmp = tmpdir("r0-history-restore");
    let home = tmp.join("home");
    let bin_dir = tmp.join("bin");
    fs::create_dir_all(&home).unwrap();
    let fake_science = write_test_bins(&bin_dir).canonicalize().unwrap();
    let mock_upstream = start_mock_upstream();
    let (proxy_port, sandbox_port) = ssh_fixture_ports();

    let mut env_guard = EnvGuard::new();
    env_guard.set("HOME", &home);
    env_guard.set("CSSWITCH_REPO", &root);
    env_guard.set("SCIENCE_BIN", &fake_science);
    env_guard.set("CSSWITCH_TEST_FAKE_SCIENCE_IDENTITY", "1");
    env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
    env_guard.set(
        "PATH",
        format!(
            "{}:/usr/bin:/bin:/usr/sbin:/sbin",
            bin_dir.to_string_lossy()
        ),
    );

    let config_dir = config::default_dir();
    let mut cfg = ssh_fixture_config(mock_upstream.port, proxy_port, sandbox_port);
    cfg.reuse_system_ssh = false;
    if matches!(
        oracle.as_str(),
        "credential-crash-replay"
            | "credential-crash-replay-restore-interrupt"
            | "credential-crash-replay-config-drift"
            | "credential-crash-replay-config-race"
            | "credential-preauth-race-replay"
    ) {
        cfg.experimental_codex_enabled = true;
        cfg.profiles.push(Profile {
            id: "history-replay-codex".into(),
            name: "History replay unavailable Codex auth".into(),
            template_id: "codex".into(),
            category: "experimental".into(),
            api_format: "openai_responses".into(),
            credential_source: crate::provider_contracts::CredentialSource::CsswitchOauth,
            credential_ref: Some("csswitch:codex:default".into()),
            model_policy: crate::provider_contracts::ModelPolicy::DynamicCatalog,
            ..Default::default()
        });
        cfg.active_id = "history-replay-codex".into();
    }
    config::save_to(&config_dir, &cfg).unwrap();
    let sandbox_home = config_dir.join("sandbox").join("home");
    let science_data = sandbox_home.join(".claude-science");
    fs::create_dir_all(&science_data).unwrap();
    let (forged, _) = crate::oauth_forge::ensure_virtual_login(
        &science_data,
        "virtual@localhost.invalid",
        &sandbox_home,
    )
    .unwrap();
    let history_orgs = [
        "11111111-1111-4111-8111-111111111111",
        "22222222-2222-4222-8222-222222222222",
    ];
    for org in history_orgs {
        fs::create_dir_all(science_data.join("orgs").join(org)).unwrap();
    }
    fs::remove_file(&forged.enc_file).unwrap();
    fs::remove_file(science_data.join("active-org.json")).unwrap();
    let marker = config_dir
        .join("sandbox")
        .join("state")
        .join("virtual-org.v1.json");
    fs::remove_file(&marker).unwrap();
    let candidates = match crate::oauth_forge::ensure_virtual_login(
        &science_data,
        "virtual@localhost.invalid",
        &sandbox_home,
    )
    .unwrap_err()
    {
        crate::oauth_forge::EnsureVirtualLoginError::HistoryChoiceRequired(candidates) => {
            candidates
        }
        other => panic!("expected history choice fixture, got {other}"),
    };
    assert_eq!(candidates.len(), 2);
    let choices = candidates
        .into_iter()
        .enumerate()
        .map(|(index, candidate)| crate::HistoryRecoveryChoice {
            reference: format!("history-reference-{index}"),
            candidate,
        })
        .collect::<Vec<_>>();
    let old_references = choices
        .iter()
        .map(|choice| choice.reference.clone())
        .collect::<Vec<_>>();
    let selected_reference = old_references[0].clone();
    let active_profile_id = cfg.active_profile().unwrap().id.clone();
    let prior_runtime =
        science::select_science_runtime_cached(None, &science::ScienceVersionCache::default())
            .unwrap();
    let prior_science_pid = start_managed_fake_science(
        &fake_science,
        &sandbox_home,
        &science_data,
        sandbox_port,
        &prior_runtime,
    );
    let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
    {
        let mut authority = lock(&state);
        authority.sandbox_port = sandbox_port;
        authority.sandbox_url = Some(format!("http://127.0.0.1:{sandbox_port}/history"));
        authority.science_runtime = Some(prior_runtime.clone());
        authority.science_confirmed_stopped = None;
        authority.history_recovery = Some(crate::HistoryRecoverySession {
            active_profile_id,
            sandbox_port,
            auth_dir: science_data.clone(),
            sandbox_root: sandbox_home.clone(),
            science_quiescence: crate::HistoryRecoveryScienceQuiescence::ExactStopped(
                prior_runtime.clone(),
            ),
            choices,
        });
        authority.boot.transition(
            crate::BootState::Attention,
            Some(serde_json::json!({"status": "history"})),
        );
    }
    let lifecycle = Arc::new(lifecycle::Lifecycle::new());
    let supervisor = Arc::new(crate::codex_auth_supervisor::CodexAuthSupervisor::default());
    let app = tauri::test::mock_builder()
        .manage(state.clone())
        .manage(lifecycle.clone())
        .manage(supervisor)
        .invoke_handler(tauri::generate_handler![super::restore_history_choice])
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .unwrap();

    let _config_drift = (oracle == "post-stop-config-drift")
        .then(super::one_click::test_arm_history_restore_post_stop_config_drift);
    let _post_snapshot_config_drift = (oracle == "post-snapshot-config-drift")
        .then(super::one_click::test_arm_history_restore_post_snapshot_config_drift);
    let _credential_interrupt = matches!(
        oracle.as_str(),
        "credential-crash-replay"
            | "credential-crash-replay-restore-interrupt"
            | "credential-crash-replay-config-drift"
            | "credential-crash-replay-config-race"
            | "credential-preauth-race-replay"
    )
    .then(super::one_click::test_arm_history_restore_credential_interrupt);
    let _cleanup_prepare_failure = (oracle == "cleanup-prepare-failure-resume").then(|| {
        config::test_arm_pending_cleanup_lifecycle(Some(
            config::PendingCleanupPublishFault::Prepare,
        ))
    });
    let _finalize_completion_failure = matches!(
        oracle.as_str(),
        "finalize-completion-failure-restore-only" | "finalize-completion-failure-resume"
    )
    .then(sandbox_session::test_arm_history_finalize_completion_failure);
    if oracle == "pre-stop-rejection" {
        cfg.mode = "official".into();
        config::save_to(&config_dir, &cfg).unwrap();
    } else if oracle == "candidate-revalidation-failure" {
        let chosen = science_data.join("orgs").join(history_orgs[0]);
        fs::remove_dir(&chosen).unwrap();
        fs::create_dir(&chosen).unwrap();
    } else if oracle == "credential-write-failure" {
        let key = science_data.join("encryption.key");
        let outside = tmp.join("immutable-test-key");
        fs::write(&outside, fs::read(&key).unwrap()).unwrap();
        fs::remove_file(&key).unwrap();
        symlink(&outside, &key).unwrap();
    }

    let result = invoke_json(
        &webview,
        "restore_history_choice",
        serde_json::json!({
            "reference": selected_reference,
            "resume": matches!(oracle.as_str(), "resume-success" | "cleanup-prepare-failure-resume" | "finalize-completion-failure-resume")
        }),
    );
    let interrupted_journal = matches!(
        oracle.as_str(),
        "credential-crash-replay"
            | "credential-crash-replay-restore-interrupt"
            | "credential-crash-replay-config-drift"
            | "credential-crash-replay-config-race"
            | "credential-preauth-race-replay"
    )
    .then(|| config::load_from(&config_dir).unwrap().runtime_transaction)
    .flatten();
    let pending_manifest_before_drift = (oracle == "credential-crash-replay-config-drift")
        .then(|| config::read_pending_authority_cleanup_manifest(&config_dir).unwrap());
    let drifted_replay_result = if oracle == "credential-crash-replay-config-drift" {
        let mut current = config::load_from(&config_dir).unwrap();
        current.sandbox_port = sandbox_port
            .checked_add(1)
            .filter(|port| *port != 8765)
            .unwrap_or(sandbox_port.saturating_sub(1));
        config::test_save_to_without_history_authority_guard(&config_dir, &current).unwrap();
        Some(sandbox_session::replay_interrupted_history_recovery_before_auth(&state))
    } else {
        None
    };
    let _replay_config_writer = (oracle == "credential-crash-replay-config-race")
        .then(sandbox_session::test_arm_history_replay_sibling_config_writer);
    let raced_replay_result = (oracle == "credential-crash-replay-config-race")
        .then(|| sandbox_session::replay_interrupted_history_recovery_before_auth(&state));
    let _restore_interrupt = (oracle == "credential-crash-replay-restore-interrupt")
        .then(super::one_click::test_arm_history_replay_interrupt_after_first_restore);
    let interrupted_restore_result = (oracle == "credential-crash-replay-restore-interrupt")
        .then(|| sandbox_session::replay_interrupted_history_recovery_before_auth(&state));
    let journal_after_interrupted_restore = (oracle == "credential-crash-replay-restore-interrupt")
        .then(|| config::load_from(&config_dir).unwrap().runtime_transaction)
        .flatten();
    let selected_org_after_interrupted_restore = (oracle
        == "credential-crash-replay-restore-interrupt")
        .then(|| {
            fs::read_to_string(science_data.join("active-org.json"))
                .ok()
                .and_then(|bytes| serde_json::from_str::<serde_json::Value>(&bytes).ok())
                .and_then(|value| value["org_uuid"].as_str().map(str::to_string))
        })
        .flatten();
    let completed_restore_result = (oracle == "credential-crash-replay-restore-interrupt")
        .then(|| sandbox_session::replay_interrupted_history_recovery_before_auth(&state));
    let (blocked_replay_result, journal_while_blocked, selected_org_while_blocked, replay_result) =
        if oracle == "credential-crash-replay" {
            let replay_listener = TcpListener::bind(("127.0.0.1", sandbox_port)).unwrap();
            let blocked = super::one_click::one_click_login_cmd(
                app.handle().clone(),
                state.clone(),
                lifecycle.clone(),
                None,
            );
            let journal_while_blocked = config::load_from(&config_dir)
                .unwrap()
                .runtime_transaction
                .is_some();
            let selected_org_while_blocked =
                fs::read_to_string(science_data.join("active-org.json"))
                    .ok()
                    .and_then(|bytes| serde_json::from_str::<serde_json::Value>(&bytes).ok())
                    .and_then(|value| value["org_uuid"].as_str().map(str::to_string));
            drop(replay_listener);
            let replay = super::one_click::one_click_login_cmd(
                app.handle().clone(),
                state.clone(),
                lifecycle.clone(),
                None,
            );
            (
                Some(blocked),
                journal_while_blocked,
                selected_org_while_blocked,
                Some(replay),
            )
        } else if oracle == "credential-preauth-race-replay" {
            let pending = interrupted_journal
                .clone()
                .expect("race fixture must preserve the pending history record");
            config::update(&config_dir, |current| current.runtime_transaction = None).unwrap();
            let _race = super::one_click::test_arm_pre_auth_history_race(pending);
            let replay = super::one_click::one_click_login_cmd(
                app.handle().clone(),
                state.clone(),
                lifecycle.clone(),
                None,
            );
            (None, false, None, Some(replay))
        } else {
            (None, false, None, None)
        };
    let (current_references, boot_attention_present, runtime_present, confirmed_stopped) = {
        let authority = lock(&state);
        (
            authority
                .history_recovery
                .as_ref()
                .map(|session| {
                    session
                        .choices
                        .iter()
                        .map(|choice| choice.reference.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
            authority.boot.state == crate::BootState::Attention,
            authority.science_runtime.is_some(),
            authority.science_confirmed_stopped.clone(),
        )
    };
    let listener_after = listener_pid_if_unique(sandbox_port);
    let serve_count =
        fs::read_to_string(science_data.join("fake-science/serve-count")).unwrap_or_default();
    let receipt_exists = config_dir.join("science-managed-launch.v1.json").exists();
    let exact_stopped = process_start_identity_if_alive(prior_science_pid).is_none()
        && listener_after.is_none()
        && !receipt_exists
        && !runtime_present
        && confirmed_stopped.as_ref() == Some(&prior_runtime);
    let selected_org_after = fs::read_to_string(science_data.join("active-org.json"))
        .ok()
        .and_then(|bytes| serde_json::from_str::<serde_json::Value>(&bytes).ok())
        .and_then(|value| value["org_uuid"].as_str().map(str::to_string));
    let marker_present_after = marker.is_file();
    let journal_present_after = config::load_from(&config_dir)
        .unwrap()
        .runtime_transaction
        .is_some();
    let pending_manifest_after_drift = (oracle == "credential-crash-replay-config-drift")
        .then(|| config::read_pending_authority_cleanup_manifest(&config_dir).unwrap());
    let config_after = config::load_from(&config_dir).unwrap();
    let reuse_system_ssh_after = config_after.reuse_system_ssh;
    let sandbox_port_after = config_after.sandbox_port;

    if oracle == "pre-stop-rejection" {
        let safe_stop = {
            let mut authority = lock(&state);
            let runtime = authority.science_runtime.clone();
            let AppState {
                sandbox,
                sandbox_url,
                ..
            } = &mut *authority;
            science::stop_sandbox(
                &app.handle().clone(),
                sandbox,
                sandbox_url,
                science::ScienceStopRequest::recover(runtime.as_ref()),
            )
        };
        assert!(safe_stop.is_ok());
    }
    force_cleanup_isolated_fixture(&state, &tmp, sandbox_port, proxy_port);

    match oracle.as_str() {
        "pre-stop-rejection" => {
            assert!(
                result
                    .as_ref()
                    .is_err_and(|error| error.contains("第三方模型模式")),
                "pre-stop config rejection must be explicit: {result:?}"
            );
            assert!(
                listener_after == Some(prior_science_pid)
                    && runtime_present
                    && confirmed_stopped.is_none()
                    && current_references == old_references
                    && boot_attention_present,
                "pre-stop rejection must preserve exact managed Science and all history references"
            );
        }
        "post-stop-config-drift" => {
            assert!(
                result
                    .as_ref()
                    .is_err_and(|error| error.contains("运行配置或事务在恢复前已变化")),
                "post-stop config drift must reject before credential mutation: {result:?}"
            );
            assert!(
                exact_stopped && current_references == old_references && boot_attention_present
            );
        }
        "candidate-revalidation-failure" => {
            assert!(
                result
                    .as_ref()
                    .is_err_and(|error| error.contains("历史记录候选已变化")),
                "candidate identity replacement must fail after exact stop: {result:?}"
            );
            assert!(
                exact_stopped && current_references == old_references && boot_attention_present
            );
        }
        "credential-write-failure" => {
            assert!(
                result.as_ref().is_err_and(|error| {
                    error.contains("符号链接")
                        && !error.contains(&tmp.to_string_lossy().to_string())
                }),
                "credential publication guard must fail after exact stop: {result:?}"
            );
            assert!(
                exact_stopped && current_references == old_references && boot_attention_present
            );
        }
        "post-snapshot-config-drift" => {
            assert!(
                result.as_ref().is_err_and(|error| {
                    error.contains("concurrent config writer")
                        && error.contains("drifted config authority")
                }),
                "snapshot compensation must reject a stale full-config restore: {result:?}"
            );
            assert!(
                exact_stopped
                    && current_references == old_references
                    && boot_attention_present
                    && reuse_system_ssh_after
                    && journal_present_after,
                "concurrent config must survive and exact recovery evidence must remain: reuse_system_ssh={reuse_system_ssh_after}, journal={journal_present_after}, result={result:?}"
            );
        }
        "credential-crash-replay" => {
            assert!(
                result.as_ref().is_err_and(|error| {
                    error.contains("interrupted history credential publication")
                }),
                "fixture must stop after credential files but before durable commit: {result:?}"
            );
            assert!(matches!(
                interrupted_journal,
                Some(config::RuntimeTransactionRecord::V2(
                    config::RuntimeTransactionV2 {
                        operation: config::RuntimeTransactionOperation::HistoryRecovery,
                        phase: config::RuntimeTransactionPhase::HistoryCredentialWritePending,
                        ..
                    }
                ))
            ));
            assert!(
                blocked_replay_result.as_ref().is_some_and(|replay| {
                    replay.as_ref().is_ok_and(|value| {
                        value["status"] == "error"
                            && value["action"] == "failed"
                            && value["recovery_status"] == "manual_recovery_required"
                    })
                }) && journal_while_blocked
                    && selected_org_while_blocked.as_deref() == Some(history_orgs[0])
                    && replay_result.as_ref().is_some_and(|replay| {
                        matches!(replay, Err(crate::commands::codex::RuntimeCommandError::Message(message)) if message.contains("仅支持第三方 API"))
                    })
                    && !journal_present_after
                    && selected_org_after.is_none()
                    && !marker_present_after,
                "replay must fail closed while Science is live, then restore the exact before-image and clear the journal before unavailable Codex auth is checked: blocked={blocked_replay_result:?}, blocked_journal={journal_while_blocked}, blocked_org={selected_org_while_blocked:?}, replay={replay_result:?}, journal={journal_present_after}, selected_org={selected_org_after:?}, marker={marker_present_after}"
            );
        }
        "credential-crash-replay-restore-interrupt" => {
            assert!(
                result.as_ref().is_err_and(|error| {
                    error.contains("interrupted history credential publication")
                })
                    && interrupted_restore_result
                        .as_ref()
                        .is_some_and(|replay| replay.as_ref().is_err_and(|error| {
                            error.contains("interrupted history authority restore")
                        }))
                    && matches!(
                        journal_after_interrupted_restore,
                        Some(config::RuntimeTransactionRecord::V2(
                            config::RuntimeTransactionV2 {
                                operation: config::RuntimeTransactionOperation::HistoryRecovery,
                                phase: config::RuntimeTransactionPhase::HistoryAuthorityRestorePending,
                                ..
                            }
                        ))
                    )
                    && selected_org_after_interrupted_restore.as_deref()
                        == Some(history_orgs[0])
                    && completed_restore_result
                        .as_ref()
                        .is_some_and(|replay| replay.as_ref().is_ok_and(|replayed| *replayed))
                    && !journal_present_after
                    && selected_org_after.is_none()
                    && !marker_present_after,
                "a crash after the first full-snapshot restore entry must preserve durable restore intent; the next sole owner must replay the idempotent full snapshot and clear only after a durable outcome: initial={result:?}, interrupted={interrupted_restore_result:?}, journal={journal_after_interrupted_restore:?}, interrupted_org={selected_org_after_interrupted_restore:?}, completed={completed_restore_result:?}, final_journal={journal_present_after}, final_org={selected_org_after:?}, marker={marker_present_after}"
            );
        }
        "credential-crash-replay-config-drift" => {
            assert!(
                result.as_ref().is_err_and(|error| {
                    error.contains("interrupted history credential publication")
                })
                    && drifted_replay_result.as_ref().is_some_and(|replay| {
                        replay.as_ref().is_err_and(|error| {
                            error.contains("config authority drifted")
                        })
                    })
                    && journal_present_after
                    && selected_org_after.as_deref() == Some(history_orgs[0])
                    && marker_present_after
                    && pending_manifest_after_drift == pending_manifest_before_drift,
                "restart replay must reject sibling Config drift before probing, credential restore, manifest transition, or journal clear: result={result:?}, replay={drifted_replay_result:?}, journal={journal_present_after}, selected_org={selected_org_after:?}, marker={marker_present_after}, manifest_before={pending_manifest_before_drift:?}, manifest_after={pending_manifest_after_drift:?}"
            );
        }
        "credential-crash-replay-config-race" => {
            assert!(
                result.as_ref().is_err_and(|error| {
                    error.contains("interrupted history credential publication")
                })
                    && raced_replay_result
                        .as_ref()
                        .is_some_and(|replay| replay.as_ref().is_ok_and(|replayed| *replayed))
                    && !journal_present_after
                    && selected_org_after.is_none()
                    && !marker_present_after
                    && sandbox_port_after == sandbox_port,
                "a sibling writer inserted after replay validation must be rejected by the central Config authority fence before any drift commits, while replay converges normally: result={result:?}, replay={raced_replay_result:?}, journal={journal_present_after}, selected_org={selected_org_after:?}, marker={marker_present_after}"
            );
        }
        "credential-preauth-race-replay" => {
            assert!(
                result.as_ref().is_err_and(|error| {
                    error.contains("interrupted history credential publication")
                })
                    && replay_result.as_ref().is_some_and(|replay| {
                        matches!(replay, Err(crate::commands::codex::RuntimeCommandError::Message(message)) if message.contains("仅支持第三方 API"))
                    })
                    && !journal_present_after
                    && selected_org_after.is_none()
                    && !marker_present_after,
                "a pending history journal inserted after the first check must replay before provider auth and still return the later auth failure: result={result:?}, replay={replay_result:?}, journal={journal_present_after}, selected_org={selected_org_after:?}, marker={marker_present_after}"
            );
        }
        "cleanup-prepare-failure-resume" => {
            let returned_references = result
                .as_ref()
                .ok()
                .and_then(|value| value["history_recovery"]["choices"].as_array())
                .map(|choices| {
                    choices
                        .iter()
                        .filter_map(|choice| choice["reference"].as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            assert!(
                result.as_ref().is_ok_and(|value| {
                    value["status"] == "degraded"
                        && value["recovery_status"] == "manual_recovery_required"
                        && value["history_recovery"]["status"] == "restored"
                }) && exact_stopped
                    && journal_present_after
                    && returned_references == current_references
                    && current_references.iter().all(|reference| !old_references.contains(reference))
                    && selected_org_after.as_deref() == Some(history_orgs[0])
                    && marker_present_after,
                "cleanup preparation failure must preserve the rotated references and exact recovery journal in a degraded DTO: result={result:?}, journal={journal_present_after}, old={old_references:?}, current={current_references:?}, returned={returned_references:?}"
            );
        }
        "finalize-completion-failure-restore-only" | "finalize-completion-failure-resume" => {
            let returned_references = result
                .as_ref()
                .ok()
                .and_then(|value| value["history_recovery"]["choices"].as_array())
                .map(|choices| {
                    choices
                        .iter()
                        .filter_map(|choice| choice["reference"].as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            assert!(
                result.as_ref().is_ok_and(|value| {
                    value["status"] == "degraded"
                        && value["recovery_status"] == "manual_recovery_required"
                        && value["history_recovery"]["status"] == "restored"
                }) && exact_stopped
                    && journal_present_after
                    && returned_references == current_references
                    && current_references.iter().all(|reference| !old_references.contains(reference))
                    && selected_org_after.as_deref() == Some(history_orgs[0])
                    && marker_present_after,
                "finalize completion failure must preserve nested rotated references for restore-only and resume while retaining exact recovery evidence: result={result:?}, journal={journal_present_after}, old={old_references:?}, current={current_references:?}, returned={returned_references:?}"
            );
        }
        "success" => {
            let returned_references = result
                .as_ref()
                .ok()
                .and_then(|value| value["choices"].as_array())
                .map(|choices| {
                    choices
                        .iter()
                        .filter_map(|choice| choice["reference"].as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            assert!(
                result.as_ref().is_ok_and(|value| {
                    value["status"] == "ok" && value["action"] == "history_choice_restored"
                }),
                "history restore must return its narrow success DTO: {result:?}"
            );
            assert!(
                exact_stopped
                    && serve_count.trim() == "1"
                    && current_references.len() == old_references.len()
                    && current_references
                        .iter()
                        .zip(&old_references)
                        .all(|(current, old)| current != old)
                    && current_references
                        .iter()
                        .all(|current| !old_references.contains(current))
                    && returned_references == current_references
                    && !boot_attention_present
                    && selected_org_after.as_deref() == Some(history_orgs[0])
                    && marker_present_after,
                "successful history restore must rotate every reference, select only the chosen org, and leave Science stopped for the frontend's separate one-click: stopped={exact_stopped}, serve_count={serve_count:?}, old={old_references:?}, current={current_references:?}, returned={returned_references:?}, boot_attention_present={boot_attention_present}, selected_org={selected_org_after:?}, marker_present={marker_present_after}"
            );
        }
        "resume-success" => {
            let returned_references = result
                .as_ref()
                .ok()
                .and_then(|value| value["history_recovery"]["choices"].as_array())
                .map(|choices| {
                    choices
                        .iter()
                        .filter_map(|choice| choice["reference"].as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            assert!(
                result.as_ref().is_ok_and(|value| {
                    value["status"] == "ok"
                        && value["action"] == "started"
                        && value["history_recovery"]["status"] == "restored"
                }),
                "combined history restore-and-resume must return the one-click result from one backend command: {result:?}"
            );
            assert!(
                runtime_present
                    && listener_after.is_some()
                    && receipt_exists
                    && confirmed_stopped.is_none()
                    && serve_count.trim() == "2"
                    && current_references.len() == old_references.len()
                    && current_references
                        .iter()
                        .all(|current| !old_references.contains(current))
                    && returned_references == current_references
                    && !boot_attention_present
                    && !journal_present_after
                    && selected_org_after.as_deref() == Some(history_orgs[0])
                    && marker_present_after,
                "combined history restore-and-resume must commit credentials, rotate references, consume the exact durable handoff, and publish a fresh managed runtime: runtime_present={runtime_present}, listener={listener_after:?}, receipt={receipt_exists}, confirmed_stopped={confirmed_stopped:?}, serve_count={serve_count:?}, journal={journal_present_after}, old={old_references:?}, current={current_references:?}, returned={returned_references:?}, selected_org={selected_org_after:?}"
            );
        }
        _ => unreachable!(),
    }
}

#[test]
fn r0_healthy_reopen_existing_marker_is_read_only() {
    run_exact_ignored_runtime_characterization(
        "commands::runtime::tests::isolated_healthy_reopen_catalog_failure_restores_prior_owned_gateway",
        &[("CSSWITCH_TEST_R0_HEALTHY_REOPEN_ORACLE", "existing-marker-success")],
    );
}

#[test]
fn r0_healthy_reopen_missing_marker_bootstraps_without_credential_mutation() {
    run_exact_ignored_runtime_characterization(
        "commands::runtime::tests::isolated_healthy_reopen_catalog_failure_restores_prior_owned_gateway",
        &[("CSSWITCH_TEST_R0_HEALTHY_REOPEN_ORACLE", "marker-bootstrap-success")],
    );
}

#[test]
fn r0_healthy_reopen_marker_bootstrap_failure_preserves_credentials() {
    run_exact_ignored_runtime_characterization(
        "commands::runtime::tests::isolated_healthy_reopen_catalog_failure_restores_prior_owned_gateway",
        &[("CSSWITCH_TEST_R0_HEALTHY_REOPEN_ORACLE", "marker-bootstrap-failure")],
    );
}

#[test]
fn r0_healthy_reopen_bootstrap_marker_survives_gateway_rollback() {
    run_exact_ignored_runtime_characterization(
        "commands::runtime::tests::isolated_healthy_reopen_catalog_failure_restores_prior_owned_gateway",
        &[("CSSWITCH_TEST_R0_HEALTHY_REOPEN_ORACLE", "marker-late-failure")],
    );
}

#[test]
fn r0_start_gateway_only_keeps_binding_and_science_but_records_secret_effect() {
    run_exact_ignored_runtime_characterization(
        "commands::runtime::tests::isolated_r0_start_gateway_only_secret_effect",
        &[],
    );
}

#[test]
fn r0_start_gateway_only_rechecks_non_codex_credential_after_serializer_wait() {
    run_exact_ignored_runtime_characterization(
        "commands::runtime::tests::isolated_real_ipc_rechecks_non_codex_credential_after_serializer_wait",
        &[],
    );
}

#[test]
fn r0_start_gateway_only_success_matrix_freezes_role_science_and_action() {
    run_exact_ignored_runtime_characterization(
        "commands::runtime::tests::isolated_r0_start_gateway_only_success_matrix",
        &[],
    );
}

#[test]
fn r0_start_gateway_only_failure_matrix_preserves_current_partial_effects() {
    for oracle in ["spawn-error", "health-failure"] {
        run_exact_ignored_runtime_characterization(
            "commands::runtime::tests::isolated_r0_start_gateway_only_failure_matrix",
            &[("CSSWITCH_TEST_R0_START_GATEWAY_FAILURE", oracle)],
        );
    }
}

#[test]
fn gateway_spawn_production_race_preserves_replacement_and_candidate_side_effects() {
    run_exact_ignored_runtime_characterization(
        "commands::runtime::tests::isolated_gateway_spawn_production_race_preserves_replacement",
        &[],
    );
}

#[test]
fn s6_registered_start_proxy_is_absent_from_invoke_surface() {
    let app_source = include_str!("../../lib.rs");
    let registration = app_source
        .split_once(".invoke_handler(tauri::generate_handler![")
        .map(|(_, suffix)| suffix)
        .expect("invoke handler must exist")
        .split("])\n")
        .next()
        .unwrap();
    assert!(!registration.contains("commands::runtime::start_proxy"));
}

#[test]
fn single_instance_plugin_is_registered_first() {
    let app_source = include_str!("../../lib.rs");
    let builder = app_source
        .split_once("let app = tauri::Builder::default()")
        .map(|(_, suffix)| suffix)
        .expect("production Builder chain must exist")
        .split(".manage(")
        .next()
        .unwrap();
    let single_instance = builder
        .find(".plugin(tauri_plugin_single_instance::init(")
        .expect("single-instance plugin must remain registered");
    assert_eq!(
        builder.find(".plugin("),
        Some(single_instance),
        "single-instance must be the first registered plugin so a second process exits before any other plugin setup"
    );
}

#[test]
fn s6_gateway_receipt_preserves_remembered_science_context_on_cold_restore() {
    run_exact_ignored_runtime_characterization(
        "commands::runtime::tests::isolated_s6_gateway_receipt_preserves_remembered_context",
        &[],
    );
}

#[test]
#[ignore = "explicit Acceptance-boundary start-gateway-only failure characterization; temp HOME, fake Gateway, retained fake Science process, and loopback only"]
fn isolated_r0_start_gateway_only_secret_effect() {
    let tmp = tmpdir("r0-start-gateway-only");
    let home = tmp.join("home");
    let bin_dir = tmp.join("bin");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&bin_dir).unwrap();
    let gateway = bin_dir.join("csswitch-gateway-exit");
    write_executable(
        &gateway,
        r#"#!/bin/sh
exit 23
"#,
    );
    let (proxy_port, sandbox_port) = ssh_fixture_ports();
    let mut env_guard = EnvGuard::new();
    env_guard.set("HOME", &home);
    env_guard.set("CSSWITCH_GATEWAY_BIN", &gateway);
    env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");

    let config_dir = config::default_dir();
    let mut cfg = ssh_fixture_config(9, proxy_port, sandbox_port);
    cfg.reuse_system_ssh = false;
    cfg.secret.clear();
    let binding = config::RuntimeBindingCommit {
        profile_id: cfg.active_id.clone(),
        route_fp: "committed-route".into(),
        catalog_fp: "committed-catalog".into(),
        binding_fp: "committed-binding".into(),
        science_adoption_attempt_id: None,
    };
    let journal = config::RuntimeTransactionJournal {
        transaction_id: "preexisting-transaction".into(),
        target_profile_id: cfg.active_id.clone(),
        stage: "start_gateway".into(),
        previous_binding: Some(binding.clone()),
        previous_gateway: None,
    };
    cfg.runtime_binding = Some(binding.clone());
    cfg.runtime_transaction = Some(journal.clone().into());
    config::save_to(&config_dir, &cfg).unwrap();

    let science_child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .expect("controlled fake Science process should start");
    let science_pid = science_child.id();
    let mut app_state = AppState::default();
    app_state.sandbox = Some(science_child);
    app_state.sandbox_port = sandbox_port;
    app_state.sandbox_url = Some(format!("http://127.0.0.1:{sandbox_port}"));
    let state: SharedAppState = Arc::new(Mutex::new(app_state));
    let lifecycle = Arc::new(lifecycle::Lifecycle::new());
    let app = tauri::test::mock_builder()
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();

    let profile = config::load_from(&config_dir)
        .unwrap()
        .active_profile()
        .unwrap()
        .clone();
    let result = start_internal_gateway(app.handle(), &state, lifecycle.as_ref(), &profile);
    assert!(
        result.is_err(),
        "the fake Gateway must fail before publication"
    );

    let after = config::load_from(&config_dir).unwrap();
    assert_eq!(after.runtime_binding, Some(binding));
    assert_eq!(after.runtime_transaction, Some(journal.into()));
    assert!(!after.secret.is_empty(), "path secret must remain durable");
    assert!(
        config_dir
            .join("runtime/skill-install-bridge.key")
            .is_file(),
        "bridge-key publication precedes Gateway health failure"
    );
    {
        let mut current = lock(&state);
        assert!(current.proxy.is_none());
        let science = current
            .sandbox
            .as_mut()
            .expect("start-gateway-only must retain the Science child");
        assert_eq!(science.id(), science_pid);
        assert!(science.try_wait().unwrap().is_none());
        science.kill().unwrap();
        science.wait().unwrap();
        current.sandbox = None;
    }
    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
#[ignore = "explicit Acceptance-boundary internal Gateway receipt success matrix; temp HOME, managed fake Science, real local Gateway, and dynamic loopback only"]
fn isolated_r0_start_gateway_only_success_matrix() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let tmp = tmpdir("r0-start-gateway-success-matrix");
    let home = tmp.join("home");
    let bin_dir = tmp.join("bin");
    fs::create_dir_all(&home).unwrap();
    let fake_science = write_test_bins(&bin_dir).canonicalize().unwrap();
    let mock_upstream = start_mock_upstream();
    let (proxy_port, sandbox_port) = ssh_fixture_ports();
    let publish_log = tmp.join("gateway-publish.log");
    fs::write(&publish_log, b"").unwrap();

    let mut env_guard = EnvGuard::new();
    env_guard.set("HOME", &home);
    env_guard.set("CSSWITCH_REPO", &root);
    env_guard.set("SCIENCE_BIN", &fake_science);
    env_guard.set("CSSWITCH_TEST_FAKE_SCIENCE_IDENTITY", "1");
    env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
    env_guard.set("CSSWITCH_TEST_GATEWAY_PUBLISH_LOG", &publish_log);
    env_guard.set(
        "PATH",
        format!(
            "{}:/usr/bin:/bin:/usr/sbin:/sbin",
            bin_dir.to_string_lossy()
        ),
    );

    let config_dir = config::default_dir();
    let mut cfg = ssh_fixture_config(mock_upstream.port, proxy_port, sandbox_port);
    cfg.reuse_system_ssh = false;
    cfg.secret = config::new_id();
    let applied_profile_id = cfg.active_id.clone();
    let applied_profile = cfg.active_profile().unwrap().clone();
    let applied_adapter = crate::runtime::provider::resolve_launch_plan(&applied_profile)
        .unwrap()
        .adapter;
    let mut selected_profile = applied_profile.clone();
    selected_profile.id = "r0-selected-profile".into();
    selected_profile.name = "R0 selected profile".into();
    selected_profile.api_key = "r0-selected-profile-fake-key-never-log".into();
    cfg.profiles.push(selected_profile.clone());
    let binding = config::RuntimeBindingCommit {
        profile_id: applied_profile_id.clone(),
        route_fp: "r0-applied-route".into(),
        catalog_fp: "r0-applied-catalog".into(),
        binding_fp: "r0-applied-binding".into(),
        science_adoption_attempt_id: None,
    };
    let journal = config::RuntimeTransactionJournal {
        transaction_id: "r0-start-gateway-success-prior-journal".into(),
        target_profile_id: applied_profile_id.clone(),
        stage: "start_gateway".into(),
        previous_binding: Some(binding.clone()),
        previous_gateway: None,
    };
    cfg.runtime_binding = Some(binding.clone());
    cfg.runtime_transaction = Some(journal.clone().into());
    config::save_to(&config_dir, &cfg).unwrap();

    let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
    let lifecycle = Arc::new(lifecycle::Lifecycle::new());
    let app = tauri::test::mock_builder()
        .manage(state.clone())
        .manage(lifecycle.clone())
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    let handle = app.handle().clone();
    let cleanup = RuntimeSmokeCleanup::new(
        handle.clone(),
        state.clone(),
        tmp.clone(),
        sandbox_port,
        proxy_port,
    );

    let first = start_internal_active_gateway(&handle, &state, lifecycle.as_ref()).unwrap();
    assert_complete_gateway_receipt(&first, &applied_profile, None);
    assert_eq!(first.port, proxy_port);
    assert!(matches!(
        first.action,
        crate::runtime::proxy::ProxyAction::Restarted
    ));
    assert_eq!(first.health.gateway_kind, "rust");
    assert_eq!(first.health.provider, applied_adapter);
    assert_eq!(first.health.shim_mode, "off");
    assert_eq!(first.health.intent, "formal");
    assert!(!first.health.launch_id.is_empty());
    assert!(!first.health.provider_contract_id.is_empty());
    assert!(!first.health.provider_contract_digest.is_empty());
    assert_eq!(
        first.catalog.expected_fingerprint.as_deref(),
        Some(first.catalog.accepted_fingerprint.as_str())
    );
    assert_eq!(first.recipe.profile.id, applied_profile_id);
    assert!(first.recipe.science_runtime.is_none());
    wait_http_health(proxy_port);
    let first_pid = lock(&state).proxy.as_ref().unwrap().id();
    let first_launch_id = lock(&state).launch_id.clone();
    let first_bridge_key = fs::read(config_dir.join("runtime/skill-install-bridge.key")).unwrap();
    let first_publish_count = fs::read_to_string(&publish_log).unwrap().lines().count();
    let after_first = config::load_from(&config_dir).unwrap();
    assert_eq!(after_first.runtime_binding, Some(binding.clone()));
    assert_eq!(
        after_first.runtime_transaction,
        Some(journal.clone().into())
    );

    let reused = start_internal_active_gateway(&handle, &state, lifecycle.as_ref()).unwrap();
    assert_complete_gateway_receipt(&reused, &applied_profile, None);
    assert_eq!(reused.port, proxy_port);
    assert!(matches!(
        reused.action,
        crate::runtime::proxy::ProxyAction::Reused
    ));
    assert_eq!(reused.health.launch_id, first_launch_id);
    assert_eq!(
        reused.health.provider_contract_id,
        first.health.provider_contract_id
    );
    assert_eq!(
        reused.health.provider_contract_digest,
        first.health.provider_contract_digest
    );
    assert_eq!(
        reused.catalog.accepted_fingerprint,
        first.catalog.accepted_fingerprint
    );
    assert_eq!(reused.recipe.profile.id, applied_profile_id);
    assert!(reused.recipe.science_runtime.is_none());
    let reused_pid = lock(&state).proxy.as_ref().unwrap().id();
    assert_eq!(
        reused_pid, first_pid,
        "healthy matching Gateway must be reused"
    );
    assert_eq!(lock(&state).launch_id, first_launch_id);
    assert_eq!(
        fs::read(config_dir.join("runtime/skill-install-bridge.key")).unwrap(),
        first_bridge_key,
        "reuse must not rotate the bridge key"
    );
    assert_eq!(
        fs::read_to_string(&publish_log).unwrap().lines().count(),
        first_publish_count,
        "reuse must not publish a second child"
    );

    let sandbox_home = home
        .join(config::CONFIG_DIR_NAME)
        .join("sandbox")
        .join("home");
    let science_data = sandbox_home.join(".claude-science");
    fs::create_dir_all(&science_data).unwrap();
    let prior_runtime =
        science::select_science_runtime_cached(None, &science::ScienceVersionCache::default())
            .unwrap();
    let science_pid = start_managed_fake_science(
        &fake_science,
        &sandbox_home,
        &science_data,
        sandbox_port,
        &prior_runtime,
    );
    wait_http_health(sandbox_port);
    {
        let mut authority = lock(&state);
        authority.sandbox_port = sandbox_port;
        authority.sandbox_url = Some(format!("http://127.0.0.1:{sandbox_port}/prior"));
        authority.science_runtime = Some(prior_runtime.clone());
        authority.science_confirmed_stopped = None;
    }

    let restarted = start_internal_active_gateway(&handle, &state, lifecycle.as_ref()).unwrap();
    assert_complete_gateway_receipt(&restarted, &applied_profile, Some(&prior_runtime));
    assert_eq!(restarted.port, proxy_port);
    assert!(matches!(
        restarted.action,
        crate::runtime::proxy::ProxyAction::Restarted
    ));
    assert_eq!(restarted.health.provider, applied_adapter);
    assert_eq!(restarted.health.intent, "formal");
    assert_eq!(
        restarted.catalog.expected_fingerprint.as_deref(),
        Some(restarted.catalog.accepted_fingerprint.as_str())
    );
    assert_eq!(restarted.recipe.profile.id, applied_profile_id);
    assert_eq!(
        restarted.recipe.science_runtime.as_ref(),
        Some(&prior_runtime)
    );
    wait_http_health(proxy_port);
    let running_science_pid = listener_pid_if_unique(sandbox_port);
    let restarted_pid = lock(&state).proxy.as_ref().unwrap().id();
    let restarted_bridge_key =
        fs::read(config_dir.join("runtime/skill-install-bridge.key")).unwrap();
    assert_ne!(
        restarted_pid, first_pid,
        "effective Science context must restart Gateway"
    );
    assert_ne!(
        restarted_bridge_key, first_bridge_key,
        "restart must rotate bridge key"
    );
    assert_eq!(running_science_pid, Some(science_pid));
    assert_eq!(lock(&state).science_runtime, Some(prior_runtime.clone()));
    assert!(
        lock(&state)
            .gateway_launch_context
            .as_ref()
            .is_some_and(|context| context.profile.id == applied_profile_id
                && context.science_runtime.as_ref() == Some(&prior_runtime)),
        "internal Gateway receipt must retain the effective Science runtime in its exact recipe"
    );

    config::update(&config_dir, |current| {
        current.active_id = selected_profile.id.clone();
    })
    .unwrap();
    let before_selected_mismatch = config::load_from(&config_dir).unwrap();
    let mismatch = start_internal_active_gateway(&handle, &state, lifecycle.as_ref()).unwrap();
    assert_complete_gateway_receipt(&mismatch, &selected_profile, Some(&prior_runtime));
    assert_eq!(mismatch.port, proxy_port);
    assert!(matches!(
        mismatch.action,
        crate::runtime::proxy::ProxyAction::Restarted
    ));
    assert_eq!(mismatch.health.provider, applied_adapter);
    assert_eq!(mismatch.health.intent, "formal");
    assert_eq!(mismatch.recipe.profile.id, "r0-selected-profile");
    assert_eq!(
        mismatch.recipe.science_runtime.as_ref(),
        Some(&prior_runtime)
    );
    wait_http_health(proxy_port);
    let mismatch_pid = lock(&state).proxy.as_ref().unwrap().id();
    let after_selected_mismatch = config::load_from(&config_dir).unwrap();
    assert_ne!(
        mismatch_pid, restarted_pid,
        "selected!=applied must replace the live Gateway"
    );
    assert_eq!(after_selected_mismatch.active_id, selected_profile.id);
    assert_eq!(
        after_selected_mismatch.runtime_binding,
        before_selected_mismatch.runtime_binding
    );
    assert_eq!(
        after_selected_mismatch.runtime_transaction,
        before_selected_mismatch.runtime_transaction
    );
    assert_eq!(listener_pid_if_unique(sandbox_port), Some(science_pid));
    assert_eq!(lock(&state).science_runtime, Some(prior_runtime.clone()));
    assert!(lock(&state)
        .gateway_launch_context
        .as_ref()
        .is_some_and(|context| context.profile.id == "r0-selected-profile"
            && context.science_runtime.as_ref() == Some(&prior_runtime)));
    assert_eq!(
        fs::read_to_string(&publish_log).unwrap().lines().count(),
        first_publish_count + 2,
        "matrix must publish initial spawn plus Science-context and selected-profile restarts"
    );

    cleanup
        .finish()
        .expect("success matrix fixtures must cleanly stop");
}

#[test]
#[ignore = "explicit Acceptance-boundary internal Gateway core failure matrix; temp HOME, controlled fake Gateway, retained fake Science child, and dynamic loopback only"]
fn isolated_r0_start_gateway_only_failure_matrix() {
    let oracle = env::var("CSSWITCH_TEST_R0_START_GATEWAY_FAILURE").unwrap();
    assert!(matches!(oracle.as_str(), "spawn-error" | "health-failure"));
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let tmp = tmpdir(&format!("r0-start-gateway-{oracle}"));
    let home = tmp.join("home");
    let bin_dir = tmp.join("bin");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&bin_dir).unwrap();
    let failing_gateway = bin_dir.join(format!("csswitch-gateway-{oracle}"));
    if oracle == "spawn-error" {
        fs::write(&failing_gateway, b"not an executable image\n").unwrap();
        fs::set_permissions(&failing_gateway, fs::Permissions::from_mode(0o700)).unwrap();
    } else {
        write_executable(
            &failing_gateway,
            r#"#!/bin/sh
exit 23
"#,
        );
    }
    let mock_upstream = start_mock_upstream();
    let (proxy_port, sandbox_port) = ssh_fixture_ports();
    let publish_log = tmp.join("gateway-publish.log");
    fs::write(&publish_log, b"").unwrap();

    let mut env_guard = EnvGuard::new();
    env_guard.set("HOME", &home);
    env_guard.set("CSSWITCH_REPO", &root);
    env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
    env_guard.set("CSSWITCH_TEST_GATEWAY_PUBLISH_LOG", &publish_log);

    let config_dir = config::default_dir();
    let mut cfg = ssh_fixture_config(mock_upstream.port, proxy_port, sandbox_port);
    cfg.reuse_system_ssh = false;
    let applied_profile_id = cfg.active_id.clone();
    let mut selected_profile = cfg.active_profile().unwrap().clone();
    selected_profile.id = "r0-failure-selected".into();
    selected_profile.name = "R0 failure selected".into();
    selected_profile.api_key = "r0-failure-selected-fake-key-never-log".into();
    cfg.profiles.push(selected_profile.clone());
    let binding = config::RuntimeBindingCommit {
        profile_id: applied_profile_id.clone(),
        route_fp: "failure-applied-route".into(),
        catalog_fp: "failure-applied-catalog".into(),
        binding_fp: "failure-applied-binding".into(),
        science_adoption_attempt_id: None,
    };
    let journal = config::RuntimeTransactionJournal {
        transaction_id: format!("r0-start-gateway-{oracle}-journal"),
        target_profile_id: applied_profile_id,
        stage: "start_gateway".into(),
        previous_binding: Some(binding.clone()),
        previous_gateway: None,
    };
    cfg.runtime_binding = Some(binding.clone());
    cfg.runtime_transaction = Some(journal.clone().into());
    if oracle == "spawn-error" {
        cfg.secret.clear();
    } else {
        cfg.secret = config::new_id();
    }
    config::save_to(&config_dir, &cfg).unwrap();

    let science_child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let science_pid = science_child.id();
    let mut authority = AppState::default();
    authority.sandbox = Some(science_child);
    authority.sandbox_port = sandbox_port;
    authority.sandbox_url = Some(format!("http://127.0.0.1:{sandbox_port}"));
    let state: SharedAppState = Arc::new(Mutex::new(authority));
    let lifecycle = Arc::new(lifecycle::Lifecycle::new());
    let app = tauri::test::mock_builder()
        .manage(state.clone())
        .manage(lifecycle.clone())
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();

    let mut prior_pid = None;
    let mut prior_key = None;
    if oracle == "health-failure" {
        let real_gateway = proxy_lifecycle::gateway_bin_path(app.handle()).unwrap();
        env_guard.set("CSSWITCH_GATEWAY_BIN", &real_gateway);
        start_internal_active_gateway(app.handle(), &state, lifecycle.as_ref()).unwrap();
        wait_http_health(proxy_port);
        prior_pid = lock(&state).proxy.as_ref().map(std::process::Child::id);
        prior_key = Some(fs::read(config_dir.join("runtime/skill-install-bridge.key")).unwrap());
        config::update(&config_dir, |current| {
            current.active_id = selected_profile.id.clone();
        })
        .unwrap();
    }
    env_guard.set("CSSWITCH_GATEWAY_BIN", &failing_gateway);
    let before_failure = config::load_from(&config_dir).unwrap();
    let failure = start_internal_active_gateway(app.handle(), &state, lifecycle.as_ref());
    assert!(
        failure.is_err(),
        "{oracle} must surface an internal Gateway error"
    );

    let after_failure = config::load_from(&config_dir).unwrap();
    let bridge_key = fs::read(config_dir.join("runtime/skill-install-bridge.key")).unwrap();
    assert_eq!(
        after_failure.runtime_binding,
        before_failure.runtime_binding
    );
    assert_eq!(
        after_failure.runtime_transaction,
        before_failure.runtime_transaction
    );
    assert!(lock(&state).proxy.is_none());
    {
        let mut current = lock(&state);
        let child = current
            .sandbox
            .as_mut()
            .expect("Science child must be retained");
        assert_eq!(child.id(), science_pid);
        assert!(child.try_wait().unwrap().is_none());
        child.kill().unwrap();
        child.wait().unwrap();
        current.sandbox = None;
    }
    if oracle == "spawn-error" {
        assert!(before_failure.secret.is_empty());
        assert!(
            !after_failure.secret.is_empty(),
            "empty secret must remain durably generated"
        );
        assert!(
            !bridge_key.is_empty(),
            "bridge key first write must precede spawn error"
        );
        assert_eq!(fs::read_to_string(&publish_log).unwrap().lines().count(), 0);
    } else {
        assert_eq!(after_failure.secret, before_failure.secret);
        assert_ne!(
            Some(bridge_key),
            prior_key,
            "failed replacement must leave rotated bridge key"
        );
        assert!(prior_pid.is_some_and(|pid| process_start_identity_if_alive(pid).is_none()));
        assert_eq!(
            fs::read_to_string(&publish_log).unwrap().lines().count(),
            1,
            "only the prior healthy Gateway may have been published"
        );
    }
    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
#[ignore = "isolated production Gateway spawn race; temp HOME, loopback Gateway/upstream, and controlled replacement child only"]
fn isolated_gateway_spawn_production_race_preserves_replacement() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let tmp = tmpdir("gateway-spawn-production-race");
    let home = tmp.join("home");
    let bin_dir = tmp.join("bin");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&bin_dir).unwrap();
    let ready = tmp.join("spawn-ready");
    let release = tmp.join("spawn-release");
    let wrapper = bin_dir.join("csswitch-gateway-spawn-barrier");
    let mock_upstream = start_mock_upstream();
    let mut port_reservations = reserve_ssh_fixture_ports();
    let proxy_port = port_reservations.proxy_port;
    let sandbox_port = port_reservations.sandbox_port;

    let mut env_guard = EnvGuard::new();
    env_guard.set("HOME", &home);
    env_guard.set("CSSWITCH_REPO", &root);
    env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
    let config_dir = config::default_dir();
    let mut cfg = ssh_fixture_config(mock_upstream.port, proxy_port, sandbox_port);
    cfg.reuse_system_ssh = false;
    cfg.secret = config::new_id();
    let profile = cfg.active_profile().unwrap().clone();
    config::save_to(&config_dir, &cfg).unwrap();

    let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
    let lifecycle = Arc::new(lifecycle::Lifecycle::new());
    let app = tauri::test::mock_builder()
        .manage(state.clone())
        .manage(lifecycle.clone())
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    let real_gateway = proxy_lifecycle::gateway_bin_path(app.handle()).unwrap();
    write_executable(
        &wrapper,
        &format!(
            "#!/bin/sh\n: > '{}'\nwhile [ ! -f '{}' ]; do /bin/sleep 0.01; done\nexec '{}' \"$@\"\n",
            ready.display(),
            release.display(),
            real_gateway.display()
        ),
    );
    env_guard.set("CSSWITCH_GATEWAY_BIN", &wrapper);
    port_reservations.release_proxy();

    let worker_handle = app.handle().clone();
    let worker_state = state.clone();
    let worker_lifecycle = lifecycle.clone();
    let worker = thread::spawn(move || {
        start_internal_active_gateway(&worker_handle, &worker_state, worker_lifecycle.as_ref())
    });
    for _ in 0..300 {
        if ready.is_file() {
            break;
        }
        assert!(
            !worker.is_finished(),
            "Gateway start exited before spawn barrier"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        ready.is_file(),
        "production Gateway spawn did not reach barrier"
    );

    let replacement = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let replacement_pid = replacement.id();
    lifecycle.bump_generation();
    {
        let mut current = state
            .try_lock()
            .expect("AppState must be available while candidate command is outside the lock");
        current.proxy = Some(replacement);
        current.proxy_port = proxy_port;
        current.secret = "replacement-secret-0123456789abcdef".into();
        current.provider = "replacement-provider".into();
        current.gateway_kind = "rust".into();
        current.shim_mode = "off".into();
        current.launch_id = "replacement-launch-0123456789abcdef".into();
        current.key_fp = 0x5eed;
        current.gateway_launch_context = Some(proxy_lifecycle::GatewayLaunchRecipe {
            profile: profile.clone(),
            science_runtime: None,
        });
    }
    let canonical_log = crate::runtime::system::log_path("proxy.log");
    fs::create_dir_all(canonical_log.parent().unwrap()).unwrap();
    fs::write(&canonical_log, b"replacement-log\n").unwrap();
    let canonical_key = config_dir.join("runtime/skill-install-bridge.key");
    fs::create_dir_all(canonical_key.parent().unwrap()).unwrap();
    fs::write(&canonical_key, b"replacement-key\n").unwrap();
    fs::write(&release, b"release\n").unwrap();

    let error = match worker.join().unwrap() {
        Ok(_) => panic!("generation/replacement drift must reject the spawned candidate"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("owner 已变化"), "{error}");
    {
        let mut current = lock(&state);
        let replacement = current
            .proxy
            .as_mut()
            .expect("replacement child must survive");
        assert_eq!(replacement.id(), replacement_pid);
        assert!(replacement.try_wait().unwrap().is_none());
        assert_eq!(current.provider, "replacement-provider");
        assert_eq!(current.launch_id, "replacement-launch-0123456789abcdef");
        let _ = current.stop_proxy();
    }
    assert!(lock(&state).rejected_gateway_candidates.is_idle());
    assert_eq!(fs::read(&canonical_log).unwrap(), b"replacement-log\n");
    assert_eq!(fs::read(&canonical_key).unwrap(), b"replacement-key\n");
    for _ in 0..200 {
        if !crate::proc::loopback_port_in_use(
            proxy_port,
            crate::runtime::operation::LOCAL_HEALTH_TIMEOUT_MS,
        ) {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!crate::proc::loopback_port_in_use(
        proxy_port,
        crate::runtime::operation::LOCAL_HEALTH_TIMEOUT_MS,
    ));
    drop(env_guard);
    drop(app);
    fs::remove_dir_all(tmp).unwrap();
}

#[test]
#[ignore = "explicit Acceptance-boundary typed Gateway receipt remembered Science context rollback; temp HOME, managed fake Science, real local Gateway, and dynamic loopback only"]
fn isolated_s6_gateway_receipt_preserves_remembered_context() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let tmp = tmpdir("r0-start-gateway-remembered-context-loss");
    let home = tmp.join("home");
    let bin_dir = tmp.join("bin");
    fs::create_dir_all(&home).unwrap();
    let fake_science = write_test_bins(&bin_dir).canonicalize().unwrap();
    let mock_upstream = start_mock_upstream();
    let mut port_reservations = reserve_ssh_fixture_ports();
    let proxy_port = port_reservations.proxy_port;
    let sandbox_port = port_reservations.sandbox_port;
    let gateway_context_log = tmp.join("gateway-science-context.log");
    let candidate_pid_log = tmp.join("candidate-science.pid");
    fs::write(&gateway_context_log, b"").unwrap();

    let mut env_guard = EnvGuard::new();
    env_guard.set("HOME", &home);
    env_guard.set("CSSWITCH_REPO", &root);
    env_guard.set("SCIENCE_BIN", &fake_science);
    env_guard.set("CSSWITCH_TEST_FAKE_SCIENCE_IDENTITY", "1");
    env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
    env_guard.set(
        "PATH",
        format!(
            "{}:/usr/bin:/bin:/usr/sbin:/sbin",
            bin_dir.to_string_lossy()
        ),
    );

    let config_dir = config::default_dir();
    let mut cfg = ssh_fixture_config(mock_upstream.port, proxy_port, sandbox_port);
    cfg.reuse_system_ssh = false;
    cfg.secret = config::new_id();
    cfg.runtime_binding = None;
    cfg.runtime_transaction = None;
    config::save_to(&config_dir, &cfg).unwrap();
    let config_before = config::load_from(&config_dir).unwrap();
    let sandbox_home = home
        .join(config::CONFIG_DIR_NAME)
        .join("sandbox")
        .join("home");
    let science_data = sandbox_home.join(".claude-science");
    fs::create_dir_all(&science_data).unwrap();
    crate::oauth_forge::ensure_virtual_login(
        &science_data,
        "virtual@localhost.invalid",
        &sandbox_home,
    )
    .unwrap();
    let prior_runtime =
        science::select_science_runtime_cached(None, &science::ScienceVersionCache::default())
            .unwrap();
    port_reservations.release_sandbox();
    let prior_science_pid = start_managed_fake_science(
        &fake_science,
        &sandbox_home,
        &science_data,
        sandbox_port,
        &prior_runtime,
    );
    wait_http_health(sandbox_port);

    let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
    {
        let mut authority = lock(&state);
        authority.sandbox_port = sandbox_port;
        authority.sandbox_url = Some(format!("http://127.0.0.1:{sandbox_port}/prior"));
        authority.science_runtime = Some(prior_runtime.clone());
        authority.science_confirmed_stopped = None;
    }
    let lifecycle = Arc::new(lifecycle::Lifecycle::new());
    let app = tauri::test::mock_builder()
        .manage(state.clone())
        .manage(lifecycle.clone())
        .invoke_handler(tauri::generate_handler![super::one_click_login])
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    let handle = app.handle().clone();
    let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .unwrap();
    let real_gateway = proxy_lifecycle::gateway_bin_path(&handle).unwrap();
    let gateway_wrapper = bin_dir.join("csswitch-gateway-context-wrapper");
    write_executable(
        &gateway_wrapper,
        &format!(
            r#"#!/bin/sh
if [ -n "${{CSSWITCH_SCIENCE_HOST_CONTEXT:-}}" ]; then
  printf '%s\n' present >> '{}'
else
  printf '%s\n' absent >> '{}'
fi
exec '{}' "$@"
"#,
            gateway_context_log.display(),
            gateway_context_log.display(),
            real_gateway.display()
        ),
    );
    env_guard.set("CSSWITCH_GATEWAY_BIN", &gateway_wrapper);
    port_reservations.release_proxy();

    let started = start_internal_active_gateway(&handle, &state, lifecycle.as_ref()).unwrap();
    assert_eq!(started.port, proxy_port);
    wait_http_health(proxy_port);
    let prior_gateway_pid = lock(&state).proxy.as_ref().unwrap().id();
    let prior_gateway_health = crate::proc::http_gateway_health(
        proxy_port,
        Some(&lock(&state).secret),
        crate::runtime::operation::LOCAL_HEALTH_TIMEOUT_MS,
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(&gateway_context_log)
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        vec!["present"],
        "internal Gateway core must derive host context from healthy remembered Science"
    );
    assert!(
        lock(&state)
            .gateway_launch_context
            .as_ref()
            .is_some_and(|context| context.science_runtime.as_ref() == Some(&prior_runtime)),
        "typed Gateway receipt must persist the effective Science runtime recipe"
    );

    science::test_reset_managed_launch_commit_failure_once();
    env_guard.set("CSSWITCH_TEST_MANAGED_LAUNCH_COMMIT_FAILURE_ONCE", "1");
    env_guard.set(
        "CSSWITCH_TEST_MANAGED_LAUNCH_FAILURE_PID_LOG",
        &candidate_pid_log,
    );
    port_reservations.release_one_click_ports();
    let failed = invoke_json(
        &webview,
        "one_click_login",
        serde_json::json!({"runtimeChoice": null}),
    )
    .unwrap();
    let candidate_science_pid = fs::read_to_string(&candidate_pid_log)
        .ok()
        .and_then(|pid| pid.trim().parse::<u32>().ok());
    let contexts = fs::read_to_string(&gateway_context_log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let restored_gateway_pid = lock(&state).proxy.as_ref().map(std::process::Child::id);
    let restored_health = crate::proc::http_gateway_health(
        proxy_port,
        Some(&lock(&state).secret),
        crate::runtime::operation::LOCAL_HEALTH_TIMEOUT_MS,
    );
    let config_after = config::load_from(&config_dir).unwrap();
    assert_eq!(
        failed["status"], "error",
        "cold one-click must reach the injected failure"
    );
    assert!(
        failed["message"]
            .as_str()
            .is_some_and(|message| message.contains("test-only managed launch commit failure")),
        "fixture must fail after candidate Science listener identity: {failed:?}"
    );
    assert!(process_start_identity_if_alive(prior_science_pid).is_none());
    assert!(candidate_science_pid.is_some_and(|pid| process_start_identity_if_alive(pid).is_none()));
    assert!(restored_gateway_pid.is_some());
    assert!(
        !contexts.is_empty() && contexts.iter().all(|value| value == "present"),
        "a reused or restarted compensated Gateway must retain the exact effective host context: prior_pid={prior_gateway_pid}, restored_pid={restored_gateway_pid:?}, contexts={contexts:?}"
    );
    assert!(lock(&state)
        .gateway_launch_context
        .as_ref()
        .is_some_and(|context| context.science_runtime.as_ref() == Some(&prior_runtime)));
    assert!(restored_health.as_ref().is_some_and(|health| {
        health.provider == prior_gateway_health.provider
            && health.provider_contract_id == prior_gateway_health.provider_contract_id
            && health.provider_contract_digest == prior_gateway_health.provider_contract_digest
            && health.catalog_fp == prior_gateway_health.catalog_fp
            && health.intent == prior_gateway_health.intent
    }));
    assert_eq!(config_after.runtime_binding, config_before.runtime_binding);
    assert_eq!(
        config_after.runtime_transaction,
        config_before.runtime_transaction
    );
    assert_eq!(lock(&state).science_runtime, Some(prior_runtime));

    force_cleanup_isolated_fixture(&state, &tmp, sandbox_port, proxy_port);
}

#[test]
#[ignore = "explicit Acceptance-boundary healthy reopen Gateway rollback; temp HOME, managed fake Science, real local Gateway, and loopback only"]
fn isolated_healthy_reopen_catalog_failure_restores_prior_owned_gateway() {
    let oracle = env::var("CSSWITCH_TEST_R0_HEALTHY_REOPEN_ORACLE")
        .unwrap_or_else(|_| "marker-late-failure".into());
    assert!(matches!(
        oracle.as_str(),
        "existing-marker-success"
            | "marker-bootstrap-success"
            | "marker-bootstrap-failure"
            | "marker-late-failure"
    ));
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let tmp = tmpdir("healthy-reopen-gateway-rollback");
    let home = tmp.join("home");
    let bin_dir = tmp.join("bin");
    fs::create_dir_all(&home).unwrap();
    let fake_science = write_test_bins(&bin_dir).canonicalize().unwrap();
    let mock_upstream = start_mock_upstream();
    let (proxy_port, sandbox_port) = ssh_fixture_ports();
    let gateway_publish_log = tmp.join("gateway-publish.log");
    fs::write(&gateway_publish_log, b"").unwrap();
    fs::set_permissions(&gateway_publish_log, fs::Permissions::from_mode(0o600)).unwrap();

    let mut env_guard = EnvGuard::new();
    env_guard.set("HOME", &home);
    env_guard.set("CSSWITCH_REPO", &root);
    env_guard.set("SCIENCE_BIN", &fake_science);
    env_guard.set("CSSWITCH_TEST_OPEN_BIN", bin_dir.join("open"));
    env_guard.set("CSSWITCH_TEST_FAKE_SCIENCE_IDENTITY", "1");
    env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
    env_guard.set("CSSWITCH_TEST_GATEWAY_PUBLISH_LOG", &gateway_publish_log);
    env_guard.set(
        "PATH",
        format!(
            "{}:/usr/bin:/bin:/usr/sbin:/sbin",
            bin_dir.to_string_lossy()
        ),
    );

    let config_dir = config::default_dir();
    let mut cfg = ssh_fixture_config(mock_upstream.port, proxy_port, sandbox_port);
    cfg.reuse_system_ssh = false;
    cfg.secret = "22".repeat(32);
    config::save_to(&config_dir, &cfg).unwrap();
    let sandbox_home = home
        .join(config::CONFIG_DIR_NAME)
        .join("sandbox")
        .join("home");
    let science_data = sandbox_home.join(".claude-science");
    fs::create_dir_all(&science_data).unwrap();
    let (forged, _) = crate::oauth_forge::ensure_virtual_login(
        &science_data,
        "virtual@localhost.invalid",
        &sandbox_home,
    )
    .unwrap();
    let marker = config_dir
        .join("sandbox")
        .join("state")
        .join("virtual-org.v1.json");
    let credential_snapshot = [
        (
            science_data.join("encryption.key"),
            fs::read(science_data.join("encryption.key")).unwrap(),
        ),
        (forged.enc_file.clone(), fs::read(&forged.enc_file).unwrap()),
        (
            science_data.join("active-org.json"),
            fs::read(science_data.join("active-org.json")).unwrap(),
        ),
    ];
    let marker_before = fs::read(&marker).unwrap();
    let marker_metadata_before = fs::metadata(&marker).unwrap();
    let marker_identity_before = (
        marker_metadata_before.dev(),
        marker_metadata_before.ino(),
        marker_metadata_before.permissions().mode() & 0o777,
    );
    let prior_runtime =
        science::select_science_runtime_cached(None, &science::ScienceVersionCache::default())
            .unwrap();
    let committed_binding = crate::runtime::provider::desired_runtime_binding(
        &cfg,
        cfg.active_profile().unwrap(),
        &prior_runtime,
    )
    .unwrap();
    cfg.runtime_binding = Some(committed_binding);
    config::save_to(&config_dir, &cfg).unwrap();
    let prior_science_pid = start_managed_fake_science(
        &fake_science,
        &sandbox_home,
        &science_data,
        sandbox_port,
        &prior_runtime,
    );

    let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
    {
        let mut authority = lock(&state);
        authority.sandbox_port = sandbox_port;
        authority.sandbox_url = Some(format!("http://127.0.0.1:{sandbox_port}/prior"));
        authority.science_runtime = Some(prior_runtime.clone());
        authority.science_confirmed_stopped = None;
    }
    let lifecycle = Arc::new(lifecycle::Lifecycle::new());
    let app = tauri::test::mock_builder()
        .manage(state.clone())
        .manage(lifecycle.clone())
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    let handle = app.handle().clone();
    let mut prior_profile = cfg.active_profile().unwrap().clone();
    prior_profile.api_key = "prior-gateway-different-fake-key".into();
    let prior_gateway = proxy_lifecycle::GatewayController::start_for(
        &handle,
        &state,
        lifecycle.as_ref(),
        &prior_profile,
        Some(&prior_runtime),
        None,
        None,
    )
    .unwrap();
    let prior_action = prior_gateway.action;
    assert!(matches!(
        prior_action,
        crate::runtime::proxy::ProxyAction::Restarted
    ));
    let prior_gateway_pid = lock(&state).proxy.as_ref().map(std::process::Child::id);
    let prior_gateway_health = crate::proc::http_gateway_health(
        proxy_port,
        Some(&lock(&state).secret),
        crate::runtime::operation::LOCAL_HEALTH_TIMEOUT_MS,
    )
    .expect("prior Gateway must be healthy");
    let sandbox_dir = sandbox_home.parent().unwrap().to_path_buf();
    if oracle != "existing-marker-success" {
        fs::remove_file(&marker).unwrap();
    }
    if oracle == "marker-bootstrap-failure" {
        fs::remove_dir(marker.parent().unwrap()).unwrap();
        fs::set_permissions(&sandbox_dir, fs::Permissions::from_mode(0o500)).unwrap();
    }
    let _catalog_seam = (oracle == "marker-late-failure")
        .then(|| sandbox_session::test_arm_healthy_reopen_catalog_failure(proxy_port));

    let result = sandbox_session::one_click_login(
        handle.clone(),
        state.clone(),
        lifecycle.as_ref(),
        None,
        None,
    );
    let publishes = fs::read_to_string(&gateway_publish_log)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_ascii_whitespace();
            Some((
                fields.next()?.parse::<u32>().ok()?,
                fields.next()?.to_string(),
                fields.next()?.parse::<u64>().ok()?,
                fields.next()?.parse::<u16>().ok()?,
            ))
        })
        .collect::<Vec<_>>();
    let (
        tracked_gateway_pid,
        tracked_gateway_running,
        secret,
        launch_id,
        key_fp,
        launch_context_matches_prior,
    ) = {
        let mut authority = lock(&state);
        let running = authority
            .proxy
            .as_mut()
            .is_some_and(|child| child.try_wait().unwrap().is_none());
        let context_matches = authority
            .gateway_launch_context
            .as_ref()
            .is_some_and(|context| {
                context.profile == prior_profile
                    && context.science_runtime.as_ref() == Some(&prior_runtime)
            });
        (
            authority.proxy.as_ref().map(std::process::Child::id),
            running,
            authority.secret.clone(),
            authority.launch_id.clone(),
            authority.key_fp,
            context_matches,
        )
    };
    let restored_gateway_health = crate::proc::http_gateway_health(
        proxy_port,
        Some(&secret),
        crate::runtime::operation::LOCAL_HEALTH_TIMEOUT_MS,
    );
    let candidate_pid = publishes.get(1).map(|publish| publish.0);
    let restored_publish = publishes.get(2);
    let gateway_restored = publishes.len() == 3
        && prior_gateway_pid == publishes.first().map(|publish| publish.0)
        && candidate_pid.is_some_and(|pid| process_start_identity_if_alive(pid).is_none())
        && tracked_gateway_running
        && tracked_gateway_pid == restored_publish.map(|publish| publish.0)
        && restored_publish.is_some_and(|publish| {
            publish.1 == launch_id
                && publish.2 == key_fp
                && publish.3 == proxy_port
                && publishes.first().is_some_and(|prior| publish.2 == prior.2)
                && publishes
                    .get(1)
                    .is_some_and(|candidate| publish.2 != candidate.2)
        })
        && launch_context_matches_prior
        && restored_gateway_health.as_ref().is_some_and(|restored| {
            restored.gateway == prior_gateway_health.gateway
                && restored.provider == prior_gateway_health.provider
                && restored.shim == prior_gateway_health.shim
                && restored.provider_contract_id == prior_gateway_health.provider_contract_id
                && restored.provider_contract_digest
                    == prior_gateway_health.provider_contract_digest
                && restored.catalog_fp == prior_gateway_health.catalog_fp
                && restored.intent == prior_gateway_health.intent
                && restored.launch_id == launch_id
        });
    let science_untouched = listener_pid_if_unique(sandbox_port) == Some(prior_science_pid)
        && TcpStream::connect(("127.0.0.1", sandbox_port)).is_ok();
    if oracle == "marker-bootstrap-failure" {
        fs::set_permissions(&sandbox_dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let credentials_unchanged = credential_snapshot
        .iter()
        .all(|(path, expected)| fs::read(path).is_ok_and(|actual| actual == *expected));
    let marker_after = fs::read(&marker).ok();
    let marker_identity_after = fs::metadata(&marker).ok().map(|metadata| {
        (
            metadata.dev(),
            metadata.ino(),
            metadata.permissions().mode() & 0o777,
        )
    });
    let prior_gateway_still_owned = {
        let mut authority = lock(&state);
        authority.proxy.as_mut().is_some_and(|child| {
            child.id() == prior_gateway_pid.unwrap() && child.try_wait().unwrap().is_none()
        })
    };
    let safe_science_stop = {
        let mut authority = lock(&state);
        let runtime = authority.science_runtime.clone();
        let AppState {
            sandbox,
            sandbox_url,
            ..
        } = &mut *authority;
        science::stop_sandbox(
            &handle,
            sandbox,
            sandbox_url,
            science::ScienceStopRequest::recover(runtime.as_ref()),
        )
    };
    force_cleanup_isolated_fixture(&state, &tmp, sandbox_port, proxy_port);

    match oracle.as_str() {
        "existing-marker-success" => {
            assert!(
                result.as_ref().is_ok_and(|value| {
                    value["status"] == "ok" && value["action"] == "reopened"
                }),
                "healthy reopen with an existing marker must succeed: {result:?}"
            );
            assert!(
                credentials_unchanged
                    && marker_after.as_deref() == Some(marker_before.as_slice())
                    && marker_identity_after == Some(marker_identity_before)
                    && science_untouched
                    && safe_science_stop.is_ok(),
                "existing-marker healthy reopen must reuse marker and credentials byte-for-byte without restarting Science: marker_present={}, credentials_unchanged={credentials_unchanged}, science_pid={prior_science_pid}, result={result:?}",
                marker_after.is_some()
            );
        }
        "marker-bootstrap-success" => {
            assert!(
                result.as_ref().is_ok_and(|value| {
                    value["status"] == "ok" && value["action"] == "reopened"
                }),
                "healthy reopen must succeed after bootstrapping a missing marker: {result:?}"
            );
            assert!(
                credentials_unchanged
                    && marker_after.is_some()
                    && tracked_gateway_running
                    && tracked_gateway_pid != prior_gateway_pid
                    && restored_gateway_health.is_some()
                    && science_untouched
                    && safe_science_stop.is_ok(),
                "missing-marker healthy reopen must bootstrap a new marker, preserve credential bytes and Science, and publish the selected healthy Gateway: marker_present={}, credentials_unchanged={credentials_unchanged}, prior_gateway_pid={prior_gateway_pid:?}, tracked_gateway_pid={tracked_gateway_pid:?}, health={restored_gateway_health:?}, science_pid={prior_science_pid}, result={result:?}",
                marker_after.is_some()
            );
        }
        "marker-bootstrap-failure" => {
            assert!(
                result
                    .as_ref()
                    .is_err_and(|error| error.to_string().contains("CSSwitch 私有状态目录")),
                "fixture must fail while publishing the missing marker: {result:?}"
            );
            assert!(
                credentials_unchanged
                    && marker_after.is_none()
                    && prior_gateway_still_owned
                    && science_untouched
                    && safe_science_stop.is_ok(),
                "marker bootstrap failure must preserve credentials, prior Gateway, and healthy Science without creating a marker: credentials_unchanged={credentials_unchanged}, marker_present={}, prior_gateway_still_owned={prior_gateway_still_owned}, science_pid={prior_science_pid}, result={result:?}",
                marker_after.is_some()
            );
        }
        "marker-late-failure" => {
            assert!(
                result.as_ref().is_err_and(|error| {
                    error
                        .to_string()
                        .contains("test-only healthy reopen catalog failure")
                }),
                "fixture must fail only after the active Gateway restart: {result:?}"
            );
            assert!(
                credentials_unchanged
                    && marker_after.is_some()
                    && gateway_restored
                    && science_untouched
                    && safe_science_stop.is_ok(),
                "healthy reopen must retain its newly bootstrapped marker while restoring the exact prior Gateway and leaving credentials/Science untouched: marker_present={}, credentials_unchanged={credentials_unchanged}, publishes={publishes:?}, prior_gateway_pid={prior_gateway_pid:?}, tracked={tracked_gateway_pid:?}, candidate={candidate_pid:?}, context_restored={launch_context_matches_prior}, health={restored_gateway_health:?}, science_pid={prior_science_pid}, safe_science_stop={safe_science_stop:?}",
                marker_after.is_some()
            );
        }
        _ => unreachable!(),
    }
}

#[test]
#[ignore = "explicit Acceptance-boundary optional SSH feature-off smoke; temp HOME, fake Science, and loopback only"]
fn isolated_ssh_feature_off_ignores_missing_optional_wrapper_and_system_config() {
    let cleanup_oracle = env::var("CSSWITCH_TEST_COMMIT_CLEANUP_ORACLE").unwrap_or_default();
    assert!(
        matches!(cleanup_oracle.as_str(), "" | "once" | "persistent"),
        "unknown commit cleanup oracle: {cleanup_oracle}"
    );
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let tmp = tmpdir("ssh-feature-off");
    let home = tmp.join("home");
    let bin_dir = tmp.join("bin");
    fs::create_dir_all(&home).unwrap();
    let fake_science = write_test_bins(&bin_dir).canonicalize().unwrap();
    let mock_upstream = start_mock_upstream();
    let (proxy_port, sandbox_port) = ssh_fixture_ports();

    let mut env_guard = EnvGuard::new();
    let cleanup_log = tmp.join("snapshot-cleanup.log");
    let _cleanup_seam = (!cleanup_oracle.is_empty()).then(|| {
        sandbox_session::test_arm_authority_snapshot_cleanup_fault(
            tmp.clone(),
            &cleanup_oracle,
            cleanup_log.clone(),
        )
    });
    env_guard.set("HOME", &home);
    env_guard.set("CSSWITCH_REPO", &root);
    env_guard.set("SCIENCE_BIN", &fake_science);
    env_guard.set("CSSWITCH_TEST_OPEN_BIN", bin_dir.join("open"));
    env_guard.set("CSSWITCH_TEST_FAKE_SCIENCE_IDENTITY", "1");
    env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
    env_guard.set(
        "CSSWITCH_TEST_SSH_WRAPPER_OVERRIDE",
        tmp.join("missing-optional-wrapper"),
    );
    env_guard.set(
        "PATH",
        format!(
            "{}:/usr/bin:/bin:/usr/sbin:/sbin",
            bin_dir.to_string_lossy()
        ),
    );

    let config_dir = config::default_dir();
    let mut cfg = ssh_fixture_config(mock_upstream.port, proxy_port, sandbox_port);
    cfg.reuse_system_ssh = false;
    config::save_to(&config_dir, &cfg).unwrap();
    let sandbox_home = home
        .join(config::CONFIG_DIR_NAME)
        .join("sandbox")
        .join("home");
    let sandbox_ssh = sandbox_home.join(".ssh");
    fs::create_dir_all(&sandbox_ssh).unwrap();
    fs::set_permissions(&sandbox_ssh, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(
        sandbox_ssh.join("known_hosts"),
        b"foreign-known-host-authority\n",
    )
    .unwrap();
    fs::set_permissions(
        sandbox_ssh.join("known_hosts"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let sandbox_ssh_before = authority_tree(&sandbox_ssh);
    let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
    let lifecycle = Arc::new(lifecycle::Lifecycle::new());
    let app = tauri::test::mock_builder()
        .manage(state.clone())
        .manage(lifecycle.clone())
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    let handle = app.handle().clone();
    let cleanup = RuntimeSmokeCleanup::new(
        handle.clone(),
        state.clone(),
        tmp.clone(),
        sandbox_port,
        proxy_port,
    );

    let started =
        sandbox_session::one_click_login(handle, state.clone(), lifecycle.as_ref(), None, None);
    let serve_count =
        fs::read_to_string(sandbox_home.join(".claude-science/fake-science/serve-count"));
    let committed = config::load_from(&config_dir).unwrap();
    let sandbox_ssh_after = authority_tree(&sandbox_ssh);
    let system_ssh_after = authority_tree(&home.join(".ssh"));
    let rollback_residue = fs::read_dir(sandbox_home.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().starts_with(".one-click-rollback-"))
        .collect::<Vec<_>>();
    let cleanup_observation = fs::read_to_string(&cleanup_log).unwrap_or_default();
    let tracked_residual = cleanup_observation
        .lines()
        .last()
        .and_then(|line| line.split('\t').nth(2))
        .unwrap_or_default()
        .to_string();
    cleanup
        .finish()
        .expect("feature-off fixture must leave no process residue");

    if cleanup_oracle == "persistent" {
        assert!(
                started.as_ref().is_ok_and(|value| {
                    value["status"] == "degraded"
                        && value["recovery_status"] == "cleanup_required"
                        && !tracked_residual.is_empty()
                        && value.to_string().contains(&tracked_residual)
                }) && rollback_residue.len() == 1,
                "persistent commit cleanup failure must return explicit degraded/cleanup_required status and track the exact residual: result={started:?}, residual={tracked_residual:?}, entries={rollback_residue:?}"
            );
        return;
    }
    assert!(
            started
                .as_ref()
                .is_ok_and(|value| value["action"] == "started"),
            "disabled optional SSH must not require HOME/.ssh/config or a packaged wrapper: {started:?}"
        );
    assert!(
        system_ssh_after.is_empty(),
        "feature-off flow must not create a system SSH authority"
    );
    assert_eq!(
        sandbox_ssh_after, sandbox_ssh_before,
        "feature-off flow must preserve foreign sandbox SSH files exactly"
    );
    assert_eq!(serve_count.unwrap(), "1");
    assert!(!committed.reuse_system_ssh);
    assert!(
        committed.runtime_transaction.is_none(),
        "successful feature-off start must commit and clear the journal"
    );
    assert!(
        rollback_residue.is_empty(),
        "successful feature-off start must not retain rollback residue"
    );
    if cleanup_oracle == "once" {
        assert!(
                cleanup_observation.lines().count() >= 2,
                "one-shot commit cleanup fault must be retried before ordinary success: {cleanup_observation:?}"
            );
    }
}

#[test]
#[ignore = "isolated V1 upgrade: temp HOME, fake Science/security and loopback only"]
fn isolated_ssh_legacy_v1_upgrade_and_rollback() {
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    for fail_commit in [false, true] {
        let tmp = tmpdir("ssh-v1-upgrade");
        let home = tmp.join("home");
        let bins = tmp.join("bin");
        fs::create_dir_all(home.join(".ssh")).unwrap();
        let system_config = home.join(".ssh/config");
        fs::write(&system_config, b"Host isolated-test-host\n").unwrap();
        fs::set_permissions(&system_config, fs::Permissions::from_mode(0o600)).unwrap();
        let system_before = authority_tree(&home.join(".ssh"));
        let fake_science = write_test_bins(&bins).canonicalize().unwrap();
        let upstream = start_mock_upstream();
        let mut ports = reserve_ssh_fixture_ports();
        let mut env_guard = EnvGuard::new();
        env_guard.set("HOME", &home);
        env_guard.set("CSSWITCH_REPO", &repository);
        env_guard.set("SCIENCE_BIN", &fake_science);
        env_guard.set("CSSWITCH_TEST_OPEN_BIN", bins.join("open"));
        env_guard.set("CSSWITCH_TEST_FAKE_SCIENCE_IDENTITY", "1");
        env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
        env_guard.set(
            "PATH",
            format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", bins.display()),
        );
        let dir = config::default_dir();
        let cfg = ssh_fixture_config(upstream.port, ports.proxy_port, ports.sandbox_port);
        config::save_to(&dir, &cfg).unwrap();
        let sandbox_home = dir.join("sandbox/home");
        let ssh_dir = sandbox_home.join(".ssh");
        fs::create_dir_all(&ssh_dir).unwrap();
        fs::set_permissions(&ssh_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let legacy = format!(
            "# CSSwitch managed system SSH config bridge v1\nInclude \"{}\"\n",
            system_config.display()
        );
        let stub = ssh_dir.join("config");
        fs::write(&stub, legacy.as_bytes()).unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(ssh_dir.join("known_hosts"), b"unrelated-test-entry\n").unwrap();
        let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
        let lifecycle = Arc::new(lifecycle::Lifecycle::new());
        let app = tauri::test::mock_builder()
            .manage(state.clone())
            .manage(lifecycle.clone())
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        let handle = app.handle().clone();
        let cleanup = RuntimeSmokeCleanup::new(
            handle.clone(),
            state.clone(),
            tmp.clone(),
            ports.sandbox_port,
            ports.proxy_port,
        );
        if fail_commit {
            env_guard.set("CSSWITCH_TEST_MANAGED_LAUNCH_COMMIT_FAILURE", "1");
        }
        ports.release_one_click_ports();
        let first = sandbox_session::one_click_login(
            handle.clone(),
            state.clone(),
            lifecycle.as_ref(),
            None,
            None,
        );
        if fail_commit {
            assert!(
                first.is_err(),
                "injected commit failure must not report success"
            );
            assert_eq!(
                fs::read(&stub).unwrap(),
                legacy.as_bytes(),
                "rollback must restore V1, not strand a V2 migration"
            );
            assert!(config::load_from(&dir)
                .unwrap()
                .runtime_transaction
                .is_none());
            env::remove_var("CSSWITCH_TEST_MANAGED_LAUNCH_COMMIT_FAILURE");
            let retry = sandbox_session::one_click_login(
                handle,
                state.clone(),
                lifecycle.as_ref(),
                None,
                None,
            );
            assert!(
                retry.as_ref().is_ok_and(|v| v["action"] == "started"),
                "retry must start: {retry:?}"
            );
        } else {
            assert!(
                first.as_ref().is_ok_and(|v| v["action"] == "started"),
                "legacy upgrade must start: {first:?}"
            );
        }
        crate::runtime::settings::validate_managed_sandbox_ssh_stub(
            &sandbox_home,
            &["isolated-test-host".into()],
        )
        .unwrap();
        assert!(lock(&state).proxy.is_some());
        assert!(crate::proc::http_health(ports.sandbox_port, None, 1000));
        assert_eq!(authority_tree(&home.join(".ssh")), system_before);
        assert_eq!(
            fs::read(ssh_dir.join("known_hosts")).unwrap(),
            b"unrelated-test-entry\n"
        );
        cleanup
            .finish()
            .expect("legacy SSH fixture must leave no processes");
    }
}

fn run_cleanup_recovery_oracle(oracle: &str) {
    assert!(
        matches!(
            oracle,
            "pending-notfound" | "durable-present" | "durable-missing"
        ),
        "unknown cleanup recovery oracle: {oracle}"
    );
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let tmp = tmpdir(&format!("cleanup-recovery-{oracle}"));
    let home = tmp.join("home");
    let bin_dir = tmp.join("bin");
    fs::create_dir_all(&home).unwrap();
    let fake_science = write_test_bins(&bin_dir).canonicalize().unwrap();
    let mock_upstream = start_mock_upstream();
    let (proxy_port, sandbox_port) = ssh_fixture_ports();
    let cleanup_log = tmp.join("snapshot-cleanup.log");

    let mut env_guard = EnvGuard::new();
    env_guard.set("HOME", &home);
    env_guard.set("CSSWITCH_REPO", &root);
    env_guard.set("SCIENCE_BIN", &fake_science);
    env_guard.set("CSSWITCH_TEST_OPEN_BIN", bin_dir.join("open"));
    env_guard.set("CSSWITCH_TEST_FAKE_SCIENCE_IDENTITY", "1");
    env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
    env_guard.set(
        "CSSWITCH_TEST_SSH_WRAPPER_OVERRIDE",
        tmp.join("missing-optional-wrapper"),
    );
    env_guard.set(
        "PATH",
        format!(
            "{}:/usr/bin:/bin:/usr/sbin:/sbin",
            bin_dir.to_string_lossy()
        ),
    );

    let config_dir = config::default_dir();
    let mut cfg = ssh_fixture_config(mock_upstream.port, proxy_port, sandbox_port);
    cfg.reuse_system_ssh = false;
    config::save_to(&config_dir, &cfg).unwrap();
    let sandbox_home = home
        .join(config::CONFIG_DIR_NAME)
        .join("sandbox")
        .join("home");
    let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
    let lifecycle = Arc::new(lifecycle::Lifecycle::new());
    let app = tauri::test::mock_builder()
        .manage(state.clone())
        .manage(lifecycle.clone())
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    let handle = app.handle().clone();
    let mut cleanup = RuntimeSmokeCleanup::new(
        handle.clone(),
        state.clone(),
        tmp.clone(),
        sandbox_port,
        proxy_port,
    );
    let cleanup_seam = sandbox_session::test_arm_authority_snapshot_cleanup_fault(
        tmp.clone(),
        "persistent",
        cleanup_log.clone(),
    );
    let first = sandbox_session::one_click_login(
        handle.clone(),
        state.clone(),
        lifecycle.as_ref(),
        None,
        None,
    );
    let cleanup_line = fs::read_to_string(&cleanup_log)
        .unwrap_or_default()
        .lines()
        .last()
        .unwrap_or_default()
        .to_string();
    let residual = PathBuf::from(
        cleanup_line
            .split('\t')
            .nth(2)
            .expect("persistent cleanup observation must expose the exact residual"),
    );
    assert!(
            first.as_ref().is_ok_and(|value| {
                value["status"] == "degraded"
                    && value["recovery_status"] == "cleanup_required"
                    && value.to_string().contains(&residual.to_string_lossy().to_string())
            }) && residual.is_dir()
                && lock(&state)
                    .pending_authority_cleanup
                    .iter()
                    .any(|pending| pending == &residual),
            "fixture must establish one explicit tracked persistent cleanup residual: first={first:?}, residual={}",
            residual.display()
        );

    let sandbox_parent = sandbox_home.parent().unwrap();
    let neighbor = sandbox_parent.join(".one-click-rollback-neighbor-must-survive");
    fs::create_dir(&neighbor).unwrap();
    fs::set_permissions(&neighbor, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(neighbor.join("sentinel"), b"unrelated-neighbor\n").unwrap();
    fs::set_permissions(neighbor.join("sentinel"), fs::Permissions::from_mode(0o600)).unwrap();
    let neighbor_before = authority_tree(&neighbor);

    let manifest_path = config_dir.join(config::PENDING_AUTHORITY_CLEANUP_MANIFEST_FILE);
    let residual_metadata = fs::symlink_metadata(&residual).unwrap();
    let managed_id = residual
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_string();
    let managed_suffix = managed_id
        .strip_prefix(".one-click-rollback-")
        .unwrap_or_default();
    let managed_id_valid = managed_suffix.len() == 32
        && managed_suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'));
    let manifest_raw = fs::read(&manifest_path).ok();
    let manifest_private_exact = manifest_raw.as_ref().is_some_and(|bytes| {
        let metadata = fs::symlink_metadata(&manifest_path).ok();
        let parent_metadata = manifest_path
            .parent()
            .and_then(|parent| fs::symlink_metadata(parent).ok());
        let parsed = serde_json::from_slice::<serde_json::Value>(bytes).ok();
        metadata.is_some_and(|metadata| {
            metadata.is_file()
                && !metadata.file_type().is_symlink()
                && metadata.permissions().mode() & 0o777 == 0o600
        }) && parent_metadata.is_some_and(|metadata| {
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.permissions().mode() & 0o777 == 0o700
        }) && parsed.is_some_and(|value| {
            value.as_object().is_some_and(|object| object.len() == 3)
                && value["schema_version"] == 2
                && value["disposition"] == "cleanup_only"
                && value["entries"]
                    == serde_json::json!([{
                        "managed_id": managed_id,
                        "path": residual.to_string_lossy().to_string(),
                        "device": residual_metadata.dev(),
                        "inode": residual_metadata.ino(),
                        "marker": managed_id
                    }])
        }) && fs::read(residual.join(".csswitch-one-click-rollback.marker"))
            .is_ok_and(|bytes| bytes == format!("{managed_id}\n").as_bytes())
            && {
                let raw = String::from_utf8_lossy(bytes);
                raw.contains(&residual.to_string_lossy().to_string())
                    && !raw.contains("foreign-known-host-authority")
                    && !raw.contains("ssh-transaction-fake-key-never-log")
                    && (cfg.secret.is_empty() || !raw.contains(&cfg.secret))
            }
    });
    let lifecycle_identity = config::PendingCleanupIdentity {
        managed_id: managed_id.clone(),
        path: residual.clone(),
        device: residual_metadata.dev(),
        inode: residual_metadata.ino(),
        marker: managed_id.clone(),
    };
    if matches!(oracle, "pending-notfound" | "durable-missing") {
        fs::remove_dir_all(&residual).unwrap();
    }
    drop(cleanup_seam);
    let _lifecycle_observation = oracle
        .starts_with("durable-")
        .then(|| config::test_arm_pending_cleanup_lifecycle(None));

    let second_state = if oracle.starts_with("durable-") {
        let stop_result = {
            let mut authority = lock(&state);
            let runtime = authority.science_runtime.clone();
            let AppState {
                sandbox,
                sandbox_url,
                ..
            } = &mut *authority;
            let result = science::stop_sandbox(
                &handle,
                sandbox,
                sandbox_url,
                science::ScienceStopRequest::recover(runtime.as_ref()),
            );
            let _ = authority.stop_proxy();
            result
        };
        assert!(
                stop_result.is_ok()
                    && TcpStream::connect(("127.0.0.1", sandbox_port)).is_err()
                    && TcpStream::connect(("127.0.0.1", proxy_port)).is_err(),
                "fixture must quiesce old AppState-owned processes before simulating restart: {stop_result:?}"
            );
        let restarted: SharedAppState = Arc::new(Mutex::new(AppState::default()));
        cleanup.state = restarted.clone();
        restarted
    } else {
        state.clone()
    };

    let second = sandbox_session::one_click_login(
        handle,
        second_state.clone(),
        lifecycle.as_ref(),
        None,
        None,
    );
    let pending_cleared = lock(&second_state).pending_authority_cleanup.is_empty();
    let residual_removed = !residual.exists();
    let neighbor_untouched = authority_tree(&neighbor) == neighbor_before;
    let manifest_cleared = match fs::read(&manifest_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|value| value["entries"].as_array().cloned())
            .is_some_and(|entries| entries.is_empty()),
        _ => false,
    };
    let lifecycle_observation = config::test_pending_cleanup_lifecycle_observation();
    let lifecycle_order_exact = !oracle.starts_with("durable-")
        || lifecycle_observation.events
            == vec![
                config::PendingCleanupLifecycleEvent::Register(lifecycle_identity.clone()),
                config::PendingCleanupLifecycleEvent::Remove {
                    identity: lifecycle_identity,
                    not_found: oracle == "durable-missing",
                },
                config::PendingCleanupLifecycleEvent::Clear,
            ];
    let durable_boundaries_exact = !oracle.starts_with("durable-")
        || (lifecycle_observation.validated_loader_count == 1
            && lifecycle_observation.initial_ticket_count == 1
            && lifecycle_observation.race_hook_count == 1
            && lifecycle_observation.completion_count == 1
            && lifecycle_observation.causal_mismatch_count == 0
            && lifecycle_observation.delete_attempt_count
                == usize::from(oracle == "durable-present"));
    cleanup
        .finish()
        .expect("cleanup recovery fixture must leave no process or temp residue");

    if oracle == "pending-notfound" {
        assert!(
                second.as_ref().is_ok()
                    && pending_cleared
                    && residual_removed
                    && neighbor_untouched,
                "an exact tracked cleanup root that is already NotFound must be removed from pending and one-click must continue without deleting a similarly named neighbor: second_ok={}, pending_cleared={pending_cleared}, residual_removed={residual_removed}, neighbor_untouched={neighbor_untouched}",
                second.is_ok()
            );
    } else {
        assert!(
                manifest_private_exact
                    && managed_id_valid
                    && second.as_ref().is_ok()
                    && pending_cleared
                    && residual_removed
                    && neighbor_untouched
                    && manifest_cleared
                    && lifecycle_order_exact
                    && durable_boundaries_exact,
                "durable cleanup manifest must be strictly validated by the fresh AppState loader, publish an explicit Present/Removed or Missing/AlreadyAbsent completion with final NotFound, preserve marker-bearing identity, and clear only after the passive lifecycle boundaries are reached: oracle={oracle}, managed_id_valid={managed_id_valid}, manifest_private_exact={manifest_private_exact}, second_ok={}, pending_cleared={pending_cleared}, residual_removed={residual_removed}, neighbor_untouched={neighbor_untouched}, manifest_cleared={manifest_cleared}, lifecycle_order_exact={lifecycle_order_exact}, durable_boundaries_exact={durable_boundaries_exact}, validated_loader_count={}, initial_ticket_count={}, race_hook_count={}, delete_attempt_count={}, completion_count={}, causal_mismatch_count={}",
                second.is_ok(),
                lifecycle_observation.validated_loader_count,
                lifecycle_observation.initial_ticket_count,
                lifecycle_observation.race_hook_count,
                lifecycle_observation.delete_attempt_count,
                lifecycle_observation.completion_count,
                lifecycle_observation.causal_mismatch_count
            );
    }
}

#[test]
#[ignore = "explicit Acceptance-boundary pending cleanup idempotence; temp HOME, fake Science, and loopback only"]
fn isolated_pending_cleanup_not_found_is_idempotent_and_neighbor_safe() {
    run_cleanup_recovery_oracle("pending-notfound");
}

#[test]
#[ignore = "explicit Acceptance-boundary durable cleanup manifest; temp HOME, fake Science, and loopback only"]
fn isolated_cleanup_manifest_survives_app_state_restart() {
    let case = env::var("CSSWITCH_TEST_DURABLE_CLEANUP_CASE").unwrap_or_else(|_| "present".into());
    assert!(
        matches!(case.as_str(), "present" | "missing"),
        "unknown durable cleanup case: {case}"
    );
    run_cleanup_recovery_oracle(&format!("durable-{case}"));
}

#[test]
#[ignore = "explicit Acceptance-boundary cleanup manifest race revalidation; temp HOME, fake Science, and loopback only"]
fn isolated_cleanup_manifest_race_revalidates_before_delete() {
    let case =
        env::var("CSSWITCH_TEST_DURABLE_RACE_CASE").unwrap_or_else(|_| "missing-recreated".into());
    assert!(
        matches!(case.as_str(), "missing-recreated" | "present-deleted"),
        "unknown durable cleanup race case: {case}"
    );
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let tmp = tmpdir(&format!("cleanup-manifest-race-{case}"));
    let home = tmp.join("home");
    let bin_dir = tmp.join("bin");
    fs::create_dir_all(&home).unwrap();
    let fake_science = write_test_bins(&bin_dir).canonicalize().unwrap();
    let mock_upstream = start_mock_upstream();
    let (proxy_port, sandbox_port) = ssh_fixture_ports();
    let mut env_guard = EnvGuard::new();
    env_guard.set("HOME", &home);
    env_guard.set("CSSWITCH_REPO", &root);
    env_guard.set("SCIENCE_BIN", &fake_science);
    env_guard.set("CSSWITCH_TEST_OPEN_BIN", bin_dir.join("open"));
    env_guard.set("CSSWITCH_TEST_FAKE_SCIENCE_IDENTITY", "1");
    env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
    env_guard.set(
        "PATH",
        format!(
            "{}:/usr/bin:/bin:/usr/sbin:/sbin",
            bin_dir.to_string_lossy()
        ),
    );

    let config_dir = config::default_dir();
    let mut cfg = ssh_fixture_config(mock_upstream.port, proxy_port, sandbox_port);
    cfg.reuse_system_ssh = false;
    config::save_to(&config_dir, &cfg).unwrap();
    let sandbox_parent = home.join(config::CONFIG_DIR_NAME).join("sandbox");
    fs::create_dir_all(&sandbox_parent).unwrap();
    let managed_id = ".one-click-rollback-abcdefabcdefabcdefabcdefabcdefab";
    let replacement_marker = "race-replacement-object-must-survive";
    let managed = sandbox_parent.join(managed_id);
    fs::create_dir(&managed).unwrap();
    fs::set_permissions(&managed, fs::Permissions::from_mode(0o700)).unwrap();
    let marker_path = managed.join(".csswitch-one-click-rollback.marker");
    fs::write(&marker_path, format!("{managed_id}\n")).unwrap();
    fs::set_permissions(&marker_path, fs::Permissions::from_mode(0o600)).unwrap();
    let metadata = fs::symlink_metadata(&managed).unwrap();
    let manifest_path = config_dir.join(config::PENDING_AUTHORITY_CLEANUP_MANIFEST_FILE);
    let manifest_bytes = serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "entries": [{
            "managed_id": managed_id,
            "path": managed.to_string_lossy().to_string(),
            "device": metadata.dev(),
            "inode": metadata.ino(),
            "marker": managed_id
        }]
    }))
    .unwrap();
    fs::write(&manifest_path, &manifest_bytes).unwrap();
    fs::set_permissions(&manifest_path, fs::Permissions::from_mode(0o600)).unwrap();
    if case == "missing-recreated" {
        fs::remove_file(&marker_path).unwrap();
        fs::remove_dir(&managed).unwrap();
    }
    let neighbor = sandbox_parent.join(".one-click-rollback-race-neighbor");
    fs::create_dir(&neighbor).unwrap();
    fs::write(neighbor.join("sentinel"), b"race-neighbor\n").unwrap();
    let neighbor_before = authority_tree(&neighbor);

    let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
    let lifecycle = Arc::new(lifecycle::Lifecycle::new());
    let app = tauri::test::mock_builder()
        .manage(state.clone())
        .manage(lifecycle.clone())
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    let handle = app.handle().clone();
    let cleanup = RuntimeSmokeCleanup::new(
        handle.clone(),
        state.clone(),
        tmp.clone(),
        sandbox_port,
        proxy_port,
    );
    let _lifecycle_observation = config::test_arm_pending_cleanup_lifecycle(None);
    config::test_configure_pending_cleanup_race(if case == "missing-recreated" {
        config::PendingCleanupRaceAction::Recreate {
            path: managed.clone(),
            marker: replacement_marker.to_string(),
        }
    } else {
        config::PendingCleanupRaceAction::Delete {
            path: managed.clone(),
        }
    });

    let result =
        sandbox_session::one_click_login(handle, state.clone(), lifecycle.as_ref(), None, None);
    let observation = config::test_pending_cleanup_lifecycle_observation();
    let zero_remove = observation
        .events
        .iter()
        .all(|event| !matches!(event, config::PendingCleanupLifecycleEvent::Remove { .. }));
    let boundary_exact = observation.validated_loader_count == 1
        && observation.initial_ticket_count == 1
        && observation.race_hook_count == 1
        && observation.delete_attempt_count == 0
        && observation.completion_count == 1
        && observation.causal_mismatch_count == 1;
    let fail_closed_before_runtime = result.is_err()
        && lock(&state).proxy.is_none()
        && lock(&state).sandbox.is_none()
        && TcpStream::connect(("127.0.0.1", proxy_port)).is_err()
        && TcpStream::connect(("127.0.0.1", sandbox_port)).is_err();
    let race_effect_preserved = if case == "missing-recreated" {
        observation
            .race_identity
            .as_ref()
            .is_some_and(|race_identity| {
                race_identity.path == managed
                    && race_identity.marker == replacement_marker
                    && race_identity.device == metadata.dev()
                    && (race_identity.inode != metadata.ino() || race_identity.marker != managed_id)
                    && fs::symlink_metadata(&managed).is_ok_and(|current| {
                        current.dev() == race_identity.device
                            && current.ino() == race_identity.inode
                    })
                    && fs::read(&marker_path)
                        .is_ok_and(|bytes| bytes == format!("{replacement_marker}\n").as_bytes())
            })
    } else {
        !managed.exists()
    };
    let manifest_untouched = fs::read(&manifest_path).is_ok_and(|bytes| bytes == manifest_bytes);
    let neighbor_untouched = authority_tree(&neighbor) == neighbor_before;
    cleanup
        .finish()
        .expect("durable cleanup race fixture must leave no process or temp residue");

    assert!(
            boundary_exact
                && zero_remove
                && fail_closed_before_runtime
                && race_effect_preserved
                && manifest_untouched
                && neighbor_untouched,
            "fresh-AppState durable cleanup must reach the validated-loader/ticket/race/completion boundaries, revalidate after the race, never call path delete for a causal mismatch, emit zero Remove, preserve the replacement or hook-deleted state, and fail closed: case={case}, result_failed={}, validated_loader_count={}, initial_ticket_count={}, race_hook_count={}, delete_attempt_count={}, completion_count={}, causal_mismatch_count={}, events={}, race_identity_present={}, race_effect_preserved={race_effect_preserved}, manifest_untouched={manifest_untouched}, neighbor_untouched={neighbor_untouched}",
            result.is_err(),
            observation.validated_loader_count,
            observation.initial_ticket_count,
            observation.race_hook_count,
            observation.delete_attempt_count,
            observation.completion_count,
            observation.causal_mismatch_count,
            observation.events.len(),
            observation.race_identity.is_some()
        );
}

#[test]
#[ignore = "explicit Acceptance-boundary manifest pre-rename crash consistency; temp HOME, fake Science, and loopback only"]
fn isolated_cleanup_manifest_pre_rename_fault_is_crash_consistent() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let tmp = tmpdir("cleanup-manifest-pre-rename");
    let home = tmp.join("home");
    let bin_dir = tmp.join("bin");
    fs::create_dir_all(&home).unwrap();
    let fake_science = write_test_bins(&bin_dir).canonicalize().unwrap();
    let mock_upstream = start_mock_upstream();
    let (proxy_port, sandbox_port) = ssh_fixture_ports();

    let mut env_guard = EnvGuard::new();
    env_guard.set("HOME", &home);
    env_guard.set("CSSWITCH_REPO", &root);
    env_guard.set("SCIENCE_BIN", &fake_science);
    env_guard.set("CSSWITCH_TEST_OPEN_BIN", bin_dir.join("open"));
    env_guard.set("CSSWITCH_TEST_FAKE_SCIENCE_IDENTITY", "1");
    env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
    env_guard.set(
        "PATH",
        format!(
            "{}:/usr/bin:/bin:/usr/sbin:/sbin",
            bin_dir.to_string_lossy()
        ),
    );

    let config_dir = config::default_dir();
    let mut cfg = ssh_fixture_config(mock_upstream.port, proxy_port, sandbox_port);
    cfg.reuse_system_ssh = false;
    config::save_to(&config_dir, &cfg).unwrap();
    let sandbox_parent = home.join(config::CONFIG_DIR_NAME).join("sandbox");
    fs::create_dir_all(&sandbox_parent).unwrap();
    fs::set_permissions(&sandbox_parent, fs::Permissions::from_mode(0o700)).unwrap();
    let managed_id = ".one-click-rollback-0123456789abcdef0123456789abcdef";
    let residual = sandbox_parent.join(managed_id);
    fs::create_dir(&residual).unwrap();
    fs::set_permissions(&residual, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(
        residual.join(".csswitch-one-click-rollback.marker"),
        format!("{managed_id}\n"),
    )
    .unwrap();
    fs::set_permissions(
        residual.join(".csswitch-one-click-rollback.marker"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let residual_metadata = fs::symlink_metadata(&residual).unwrap();
    let lifecycle_identity = config::PendingCleanupIdentity {
        managed_id: managed_id.to_string(),
        path: residual.clone(),
        device: residual_metadata.dev(),
        inode: residual_metadata.ino(),
        marker: managed_id.to_string(),
    };
    let manifest_path = config_dir.join(config::PENDING_AUTHORITY_CLEANUP_MANIFEST_FILE);
    let manifest_bytes = serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "entries": [{
            "managed_id": managed_id,
            "path": residual.to_string_lossy().to_string(),
            "device": residual_metadata.dev(),
            "inode": residual_metadata.ino(),
            "marker": managed_id
        }]
    }))
    .unwrap();
    fs::write(&manifest_path, &manifest_bytes).unwrap();
    fs::set_permissions(&manifest_path, fs::Permissions::from_mode(0o600)).unwrap();
    let manifest_before = fs::symlink_metadata(&manifest_path).unwrap();
    let lookalike_temp =
        config_dir.join(".pending-authority-cleanup.v1.json.tmp-lookalike-sentinel");
    let lookalike_temp_bytes = b"foreign-lookalike-temp-must-survive\n".to_vec();
    fs::write(&lookalike_temp, &lookalike_temp_bytes).unwrap();
    fs::set_permissions(&lookalike_temp, fs::Permissions::from_mode(0o600)).unwrap();
    let lookalike_temp_before = fs::symlink_metadata(&lookalike_temp).unwrap();
    fs::remove_file(residual.join(".csswitch-one-click-rollback.marker")).unwrap();
    fs::remove_dir(&residual).unwrap();

    let neighbor = sandbox_parent.join(".one-click-rollback-lookalike");
    fs::create_dir(&neighbor).unwrap();
    fs::write(neighbor.join("foreign-marker"), b"foreign-neighbor\n").unwrap();
    let neighbor_before = authority_tree(&neighbor);

    let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
    let lifecycle = Arc::new(lifecycle::Lifecycle::new());
    let app = tauri::test::mock_builder()
        .manage(state.clone())
        .manage(lifecycle.clone())
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    let handle = app.handle().clone();
    let mut cleanup = RuntimeSmokeCleanup::new(
        handle.clone(),
        state.clone(),
        tmp.clone(),
        sandbox_port,
        proxy_port,
    );
    let (failpoint, observation) =
        config::test_arm_pending_manifest_pre_rename_failure(&config_dir).unwrap();
    let _lifecycle_observation =
        config::test_arm_pending_cleanup_lifecycle(Some(config::PendingCleanupPublishFault::Clear));
    let first = sandbox_session::one_click_login(
        handle.clone(),
        state.clone(),
        lifecycle.as_ref(),
        None,
        None,
    );
    let observed = observation
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let manifest_after_fault = fs::read(&manifest_path).ok();
    let manifest_metadata_after_fault = fs::symlink_metadata(&manifest_path).ok();
    let old_manifest_exact = manifest_after_fault.as_deref() == Some(manifest_bytes.as_slice())
        && manifest_metadata_after_fault
            .as_ref()
            .is_some_and(|metadata| {
                metadata.permissions().mode() & 0o777 == 0o600
                    && metadata.dev() == manifest_before.dev()
                    && metadata.ino() == manifest_before.ino()
            });
    let owned_temp_observed_and_cleaned = observed.as_ref().is_some_and(|observation| {
        observation.target_path == manifest_path
            && observation.config_access_held
            && observation.temp_path.parent() == Some(config_dir.as_path())
            && observation
                .temp_path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".pending-authority-cleanup.v1.json.tmp-"))
            && observation.temp_device == manifest_before.dev()
            && observation.temp_inode > 0
            && !observation.temp_path.exists()
    });
    let no_temp_residue = fs::read_dir(&config_dir).unwrap().all(|entry| {
        entry
            .ok()
            .and_then(|entry| entry.file_name().into_string().ok())
            .is_none_or(|name| {
                name == ".pending-authority-cleanup.v1.json.tmp-lookalike-sentinel"
                    || !name.contains("pending-authority-cleanup.v1.json.tmp-")
            })
    });
    let lookalike_temp_unchanged = fs::read(&lookalike_temp)
        .is_ok_and(|bytes| bytes == lookalike_temp_bytes)
        && fs::symlink_metadata(&lookalike_temp).is_ok_and(|metadata| {
            metadata.permissions().mode() & 0o777 == 0o600
                && metadata.dev() == lookalike_temp_before.dev()
                && metadata.ino() == lookalike_temp_before.ino()
        });
    let first_stopped_before_runtime = lock(&state).proxy.is_none()
        && lock(&state).sandbox.is_none()
        && TcpStream::connect(("127.0.0.1", proxy_port)).is_err()
        && TcpStream::connect(("127.0.0.1", sandbox_port)).is_err();
    let neighbor_after_fault = authority_tree(&neighbor) == neighbor_before;

    {
        let mut authority = lock(&state);
        let runtime = authority.science_runtime.clone();
        let AppState {
            sandbox,
            sandbox_url,
            ..
        } = &mut *authority;
        let _ = science::stop_sandbox(
            &handle,
            sandbox,
            sandbox_url,
            science::ScienceStopRequest::recover(runtime.as_ref()),
        );
        let _ = authority.stop_proxy();
    }
    drop(failpoint);
    let fresh_state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
    cleanup.state = fresh_state.clone();
    let second = sandbox_session::one_click_login(
        handle,
        fresh_state.clone(),
        lifecycle.as_ref(),
        None,
        None,
    );
    let manifest_cleared = match fs::read(&manifest_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|value| value["entries"].as_array().cloned())
            .is_some_and(|entries| entries.is_empty()),
        _ => false,
    };
    let fresh_retry_continued = second
        .as_ref()
        .is_ok_and(|value| matches!(value["action"].as_str(), Some("started" | "reopened")));
    let neighbor_untouched = authority_tree(&neighbor) == neighbor_before;
    let lifecycle_observation = config::test_pending_cleanup_lifecycle_observation();
    let lifecycle_order_exact = lifecycle_observation.events
        == vec![
            config::PendingCleanupLifecycleEvent::Register(lifecycle_identity.clone()),
            config::PendingCleanupLifecycleEvent::Remove {
                identity: lifecycle_identity,
                not_found: true,
            },
            config::PendingCleanupLifecycleEvent::Clear,
        ];
    let lifecycle_boundaries_exact = lifecycle_observation.validated_loader_count == 1
        && lifecycle_observation.initial_ticket_count == 1
        && lifecycle_observation.race_hook_count == 1
        && lifecycle_observation.delete_attempt_count == 0
        && lifecycle_observation.completion_count == 1
        && lifecycle_observation.causal_mismatch_count == 0;
    cleanup
        .finish()
        .expect("manifest pre-rename fixture must leave no process or temp residue");

    assert!(
            first.is_err()
                && observed.is_some()
                && old_manifest_exact
                && owned_temp_observed_and_cleaned
                && no_temp_residue
                && lookalike_temp_unchanged
                && first_stopped_before_runtime
                && neighbor_after_fault
                && fresh_retry_continued
                && manifest_cleared
                && neighbor_untouched
                && lifecycle_order_exact
                && lifecycle_boundaries_exact,
            "pre-rename CLEAR fault must occur only after the fresh loader and explicit Missing/AlreadyAbsent completion boundaries, preserve the old private target byte/mode/inode-exactly, remove only its owned temp, and retain the lookalike: first_failed={}, failpoint_reached={}, old_manifest_exact={old_manifest_exact}, owned_temp_observed_and_cleaned={owned_temp_observed_and_cleaned}, no_temp_residue={no_temp_residue}, lookalike_temp_unchanged={lookalike_temp_unchanged}, first_stopped_before_runtime={first_stopped_before_runtime}, neighbor_after_fault={neighbor_after_fault}, fresh_retry_continued={fresh_retry_continued}, manifest_cleared={manifest_cleared}, neighbor_untouched={neighbor_untouched}, lifecycle_order_exact={lifecycle_order_exact}, lifecycle_boundaries_exact={lifecycle_boundaries_exact}, validated_loader_count={}, initial_ticket_count={}, race_hook_count={}, delete_attempt_count={}, completion_count={}, causal_mismatch_count={}",
            first.is_err(),
            observed.is_some(),
            lifecycle_observation.validated_loader_count,
            lifecycle_observation.initial_ticket_count,
            lifecycle_observation.race_hook_count,
            lifecycle_observation.delete_attempt_count,
            lifecycle_observation.completion_count,
            lifecycle_observation.causal_mismatch_count
        );
}

#[test]
#[ignore = "explicit Acceptance-boundary durable REGISTER failure; temp HOME, fake Science, and loopback only"]
fn isolated_cleanup_register_failure_never_enters_remove() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let tmp = tmpdir("cleanup-register-failure");
    let home = tmp.join("home");
    let bin_dir = tmp.join("bin");
    fs::create_dir_all(&home).unwrap();
    let fake_science = write_test_bins(&bin_dir).canonicalize().unwrap();
    let mock_upstream = start_mock_upstream();
    let (proxy_port, sandbox_port) = ssh_fixture_ports();
    let cleanup_log = tmp.join("remove-attempt.log");
    let mut env_guard = EnvGuard::new();
    env_guard.set("HOME", &home);
    env_guard.set("CSSWITCH_REPO", &root);
    env_guard.set("SCIENCE_BIN", &fake_science);
    env_guard.set("CSSWITCH_TEST_OPEN_BIN", bin_dir.join("open"));
    env_guard.set("CSSWITCH_TEST_FAKE_SCIENCE_IDENTITY", "1");
    env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
    env_guard.set(
        "PATH",
        format!(
            "{}:/usr/bin:/bin:/usr/sbin:/sbin",
            bin_dir.to_string_lossy()
        ),
    );
    let config_dir = config::default_dir();
    let mut cfg = ssh_fixture_config(mock_upstream.port, proxy_port, sandbox_port);
    cfg.reuse_system_ssh = false;
    config::save_to(&config_dir, &cfg).unwrap();
    let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
    let lifecycle = Arc::new(lifecycle::Lifecycle::new());
    let app = tauri::test::mock_builder()
        .manage(state.clone())
        .manage(lifecycle.clone())
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    let handle = app.handle().clone();
    let cleanup = RuntimeSmokeCleanup::new(
        handle.clone(),
        state.clone(),
        tmp.clone(),
        sandbox_port,
        proxy_port,
    );
    let _remove_seam = sandbox_session::test_arm_authority_snapshot_cleanup_fault(
        tmp.clone(),
        "persistent",
        cleanup_log.clone(),
    );
    let _lifecycle_seam = config::test_arm_pending_cleanup_lifecycle(Some(
        config::PendingCleanupPublishFault::Register,
    ));
    let result =
        sandbox_session::one_click_login(handle, state.clone(), lifecycle.as_ref(), None, None);
    let lifecycle_observation = config::test_pending_cleanup_lifecycle_observation();
    let residual = lifecycle_observation
        .attempted_register
        .as_ref()
        .map(|identity| identity.path.clone());
    let remove_log_empty = fs::read_to_string(&cleanup_log)
        .unwrap_or_default()
        .is_empty();
    let residual_private = residual.as_ref().is_some_and(|path| path.is_dir());
    let register_failed_before_remove = result.is_err()
        && lifecycle_observation.attempted_register.is_some()
        && lifecycle_observation.events.is_empty()
        && lifecycle_observation.initial_ticket_count == 0
        && lifecycle_observation.completion_count == 0
        && remove_log_empty
        && residual_private
        && !config_dir
            .join(config::PENDING_AUTHORITY_CLEANUP_MANIFEST_FILE)
            .exists();
    cleanup
        .finish()
        .expect("REGISTER failure fixture must leave no process residue");
    assert!(
            register_failed_before_remove,
            "a product-owned durable REGISTER attempt must fail at its explicit publish boundary, preserve the still-valuable private snapshot, and never enter ticket/completion/Remove/Clear: result_failed={}, attempted_register={}, durable_events={}, initial_ticket_count={}, completion_count={}, remove_log_empty={}, residual_private={}",
            result.is_err(),
            lifecycle_observation.attempted_register.is_some(),
            lifecycle_observation.events.len(),
            lifecycle_observation.initial_ticket_count,
            lifecycle_observation.completion_count,
            remove_log_empty,
            residual_private
        );
}

#[test]
#[ignore = "explicit Acceptance-boundary unsafe cleanup manifest matrix; temp HOME, fake Science, and loopback only"]
fn isolated_cleanup_manifest_rejects_unsafe_entries_before_runtime() {
    let case = env::var("CSSWITCH_TEST_UNSAFE_MANIFEST_CASE")
        .unwrap_or_else(|_| "invalid-managed-id".into());
    assert!(
        matches!(
            case.as_str(),
            "invalid-managed-id"
                | "path-mismatch"
                | "device-mismatch"
                | "inode-mismatch"
                | "marker-mismatch"
                | "manifest-symlink"
                | "extra-field"
        ),
        "unknown unsafe manifest case"
    );
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let tmp = tmpdir(&format!("unsafe-cleanup-manifest-{case}"));
    let home = tmp.join("home");
    let bin_dir = tmp.join("bin");
    fs::create_dir_all(&home).unwrap();
    let fake_science = write_test_bins(&bin_dir).canonicalize().unwrap();
    let mock_upstream = start_mock_upstream();
    let (proxy_port, sandbox_port) = ssh_fixture_ports();
    let mut env_guard = EnvGuard::new();
    env_guard.set("HOME", &home);
    env_guard.set("CSSWITCH_REPO", &root);
    env_guard.set("SCIENCE_BIN", &fake_science);
    env_guard.set("CSSWITCH_TEST_OPEN_BIN", bin_dir.join("open"));
    env_guard.set("CSSWITCH_TEST_FAKE_SCIENCE_IDENTITY", "1");
    env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
    env_guard.set(
        "PATH",
        format!(
            "{}:/usr/bin:/bin:/usr/sbin:/sbin",
            bin_dir.to_string_lossy()
        ),
    );
    let config_dir = config::default_dir();
    let mut cfg = ssh_fixture_config(mock_upstream.port, proxy_port, sandbox_port);
    cfg.reuse_system_ssh = false;
    config::save_to(&config_dir, &cfg).unwrap();
    let config_before = config::load_from(&config_dir).unwrap();
    let sandbox_parent = home.join(config::CONFIG_DIR_NAME).join("sandbox");
    fs::create_dir_all(&sandbox_parent).unwrap();
    let managed_id = ".one-click-rollback-fedcba9876543210fedcba9876543210";
    let managed = sandbox_parent.join(managed_id);
    fs::create_dir(&managed).unwrap();
    let marker = managed.join(".csswitch-one-click-rollback.marker");
    fs::write(&marker, format!("{managed_id}\n")).unwrap();
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
    let metadata = fs::symlink_metadata(&managed).unwrap();
    let neighbor = sandbox_parent.join(".one-click-rollback-11111111111111111111111111111111");
    fs::create_dir(&neighbor).unwrap();
    fs::write(neighbor.join("sentinel"), b"neighbor\n").unwrap();
    let manifest_path = config_dir.join(config::PENDING_AUTHORITY_CLEANUP_MANIFEST_FILE);
    let mut manifest = serde_json::json!({
        "schema_version": 1,
        "entries": [{
            "managed_id": managed_id,
            "path": managed.to_string_lossy().to_string(),
            "device": metadata.dev(),
            "inode": metadata.ino(),
            "marker": managed_id
        }]
    });
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    fs::set_permissions(&manifest_path, fs::Permissions::from_mode(0o600)).unwrap();
    let predicate_vector = |path: &Path, target: &Path, expected_raw_path: &str| {
        let lstat = fs::symlink_metadata(path).ok();
        let parsed = fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
        let entry = parsed
            .as_ref()
            .and_then(|value| value["entries"].as_array())
            .and_then(|entries| entries.first());
        let target_metadata = fs::symlink_metadata(target).ok();
        let id = entry
            .and_then(|entry| entry["managed_id"].as_str())
            .unwrap_or_default();
        let marker_ticket = entry
            .and_then(|entry| entry["marker"].as_str())
            .unwrap_or_default();
        let suffix = id.strip_prefix(".one-click-rollback-").unwrap_or_default();
        [
            lstat.is_some_and(|metadata| {
                metadata.is_file()
                    && !metadata.file_type().is_symlink()
                    && metadata.permissions().mode() & 0o777 == 0o600
            }),
            parsed.as_ref().is_some_and(|value| {
                value.as_object().is_some_and(|object| {
                    object.len() == 2
                        && object.contains_key("schema_version")
                        && object.contains_key("entries")
                }) && value["schema_version"] == 1
                    && value["entries"].as_array().is_some_and(|entries| {
                        entries.len() == 1
                            && entries[0].as_object().is_some_and(|object| {
                                object.len() == 5
                                    && ["managed_id", "path", "device", "inode", "marker"]
                                        .iter()
                                        .all(|key| object.contains_key(*key))
                                    && entries[0]["managed_id"].is_string()
                                    && entries[0]["path"].is_string()
                                    && entries[0]["device"].as_u64().is_some()
                                    && entries[0]["inode"].as_u64().is_some()
                                    && entries[0]["marker"].is_string()
                            })
                    })
            }),
            suffix.len() == 32
                && suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
            entry.and_then(|entry| entry["path"].as_str()) == Some(expected_raw_path),
            entry.and_then(|entry| entry["device"].as_u64())
                == target_metadata.as_ref().map(MetadataExt::dev),
            entry.and_then(|entry| entry["inode"].as_u64())
                == target_metadata.as_ref().map(MetadataExt::ino),
            target.file_name().and_then(|name| name.to_str()) == Some(id)
                && marker_ticket == id
                && fs::read(target.join(".csswitch-one-click-rollback.marker"))
                    .is_ok_and(|bytes| bytes == format!("{marker_ticket}\n").as_bytes()),
        ]
    };
    let baseline_vector =
        predicate_vector(&manifest_path, &managed, managed.to_string_lossy().as_ref());
    assert_eq!(
        baseline_vector, [true; 7],
        "unsafe matrix baseline E1/E2/P1/P2/P3/P4/P5 must all be true"
    );
    let mut predicate_target = managed.clone();
    let mut expected_raw_path = managed.to_string_lossy().into_owned();
    if case == "invalid-managed-id" {
        let invalid_id = ".one-click-rollback-not-32-lower-hex";
        predicate_target = sandbox_parent.join(invalid_id);
        fs::create_dir(&predicate_target).unwrap();
        fs::write(
            predicate_target.join(".csswitch-one-click-rollback.marker"),
            format!("{invalid_id}\n"),
        )
        .unwrap();
        let invalid_metadata = fs::symlink_metadata(&predicate_target).unwrap();
        expected_raw_path = predicate_target.to_string_lossy().into_owned();
        manifest["entries"][0] = serde_json::json!({
            "managed_id": invalid_id,
            "path": expected_raw_path,
            "device": invalid_metadata.dev(),
            "inode": invalid_metadata.ino(),
            "marker": invalid_id
        });
    } else if case == "path-mismatch" {
        manifest["entries"][0]["path"] =
            serde_json::json!(format!("{}/./{}", sandbox_parent.display(), managed_id));
    } else if case == "device-mismatch" {
        manifest["entries"][0]["device"] = serde_json::json!(metadata.dev() + 1);
    } else if case == "inode-mismatch" {
        manifest["entries"][0]["inode"] = serde_json::json!(metadata.ino() + 1);
    } else if case == "marker-mismatch" {
        fs::write(&marker, b"foreign-marker-authority\n").unwrap();
    } else if case == "extra-field" {
        manifest["unexpected"] = serde_json::json!(true);
    }
    let target_before = authority_tree(&predicate_target);
    let neighbor_before = authority_tree(&neighbor);
    let authority_before = authority_tree(&sandbox_parent);
    let mut symlink_target = None;
    if case == "manifest-symlink" {
        fs::remove_file(&manifest_path).unwrap();
        let foreign = tmp.join("foreign-manifest-target");
        fs::write(&foreign, serde_json::to_vec(&manifest).unwrap()).unwrap();
        fs::set_permissions(&foreign, fs::Permissions::from_mode(0o600)).unwrap();
        let foreign_metadata = fs::symlink_metadata(&foreign).unwrap();
        symlink_target = Some((
            foreign.clone(),
            authority_tree(&foreign),
            (
                foreign_metadata.dev(),
                foreign_metadata.ino(),
                foreign_metadata.permissions().mode() & 0o777,
            ),
        ));
        symlink(&foreign, &manifest_path).unwrap();
    } else {
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        fs::set_permissions(&manifest_path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let post_vector = predicate_vector(&manifest_path, &predicate_target, &expected_raw_path);
    let expected_false = match case.as_str() {
        "manifest-symlink" => 0,
        "extra-field" => 1,
        "invalid-managed-id" => 2,
        "path-mismatch" => 3,
        "device-mismatch" => 4,
        "inode-mismatch" => 5,
        "marker-mismatch" => 6,
        _ => unreachable!(),
    };
    assert_eq!(
        post_vector
            .iter()
            .enumerate()
            .filter_map(|(index, valid)| (!valid).then_some(index))
            .collect::<Vec<_>>(),
        vec![expected_false],
        "each unsafe selector must make exactly one of E1/E2/P1/P2/P3/P4/P5 false"
    );
    let manifest_before = authority_tree(&manifest_path);
    let manifest_metadata_before = fs::symlink_metadata(&manifest_path).unwrap();
    let manifest_identity_before = (
        manifest_metadata_before.dev(),
        manifest_metadata_before.ino(),
        manifest_metadata_before.permissions().mode() & 0o777,
        manifest_metadata_before.file_type().is_symlink(),
    );

    let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
    let lifecycle = Arc::new(lifecycle::Lifecycle::new());
    let app = tauri::test::mock_builder()
        .manage(state.clone())
        .manage(lifecycle.clone())
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    let handle = app.handle().clone();
    let cleanup = RuntimeSmokeCleanup::new(
        handle.clone(),
        state.clone(),
        tmp.clone(),
        sandbox_port,
        proxy_port,
    );
    let result =
        sandbox_session::one_click_login(handle, state.clone(), lifecycle.as_ref(), None, None);
    let rejected_before_runtime = result.is_err()
        && lock(&state).proxy.is_none()
        && lock(&state).sandbox.is_none()
        && TcpStream::connect(("127.0.0.1", proxy_port)).is_err()
        && TcpStream::connect(("127.0.0.1", sandbox_port)).is_err();
    let config_unchanged =
        config::load_from(&config_dir).is_ok_and(|current| current == config_before);
    let authority_untouched = authority_tree(&sandbox_parent) == authority_before;
    let target_untouched = authority_tree(&predicate_target) == target_before;
    let neighbor_untouched = authority_tree(&neighbor) == neighbor_before;
    let manifest_untouched = authority_tree(&manifest_path) == manifest_before
        && fs::symlink_metadata(&manifest_path).is_ok_and(|metadata| {
            (
                metadata.dev(),
                metadata.ino(),
                metadata.permissions().mode() & 0o777,
                metadata.file_type().is_symlink(),
            ) == manifest_identity_before
        });
    let symlink_target_untouched =
        symlink_target
            .as_ref()
            .is_none_or(|(path, before, identity)| {
                authority_tree(path) == *before
                    && fs::symlink_metadata(path).is_ok_and(|metadata| {
                        (
                            metadata.dev(),
                            metadata.ino(),
                            metadata.permissions().mode() & 0o777,
                        ) == *identity
                    })
            });
    cleanup
        .finish()
        .expect("unsafe manifest fixture must leave no process or temp residue");
    assert!(
            rejected_before_runtime
                && config_unchanged
                && authority_untouched
                && target_untouched
                && neighbor_untouched
                && manifest_untouched
                && symlink_target_untouched,
            "each unsafe manifest case must vary only its named invalid dimension, fail closed before Gateway/Science, and preserve target, neighbor, config, manifest bytes/mode/device/inode or symlink authority, and symlink target exactly: case={case}, baseline_vector={baseline_vector:?}, post_vector={post_vector:?}, rejected_before_runtime={rejected_before_runtime}, config_unchanged={config_unchanged}, authority_untouched={authority_untouched}, target_untouched={target_untouched}, neighbor_untouched={neighbor_untouched}, manifest_untouched={manifest_untouched}, symlink_target_untouched={symlink_target_untouched}"
        );
}

#[test]
#[ignore = "explicit Acceptance-boundary compensation diagnostic redaction; temp HOME, fake Science, and loopback only"]
fn isolated_compensation_diagnostics_are_credential_free() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let tmp = tmpdir("compensation-diagnostic-redaction");
    let home = tmp.join("home");
    let bin_dir = tmp.join("bin");
    fs::create_dir_all(&home).unwrap();
    let fake_science = write_test_bins(&bin_dir).canonicalize().unwrap();
    let mock_upstream = start_mock_upstream();
    let (proxy_port, sandbox_port) = ssh_fixture_ports();
    let mut env_guard = EnvGuard::new();
    env_guard.set("HOME", &home);
    env_guard.set("CSSWITCH_REPO", &root);
    env_guard.set("SCIENCE_BIN", &fake_science);
    env_guard.set("CSSWITCH_TEST_OPEN_BIN", bin_dir.join("open"));
    env_guard.set("CSSWITCH_TEST_FAKE_SCIENCE_IDENTITY", "1");
    env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
    env_guard.set("CSSWITCH_TEST_MANAGED_LAUNCH_COMMIT_FAILURE_ONCE", "1");
    env_guard.set(
        "PATH",
        format!(
            "{}:/usr/bin:/bin:/usr/sbin:/sbin",
            bin_dir.to_string_lossy()
        ),
    );
    let config_dir = config::default_dir();
    let mut cfg = ssh_fixture_config(mock_upstream.port, proxy_port, sandbox_port);
    cfg.reuse_system_ssh = false;
    config::save_to(&config_dir, &cfg).unwrap();
    let sandbox_home = home
        .join(config::CONFIG_DIR_NAME)
        .join("sandbox")
        .join("home");
    fs::create_dir_all(sandbox_home.join(".claude-science")).unwrap();
    crate::oauth_forge::ensure_virtual_login(
        &sandbox_home.join(".claude-science"),
        "virtual@localhost.invalid",
        &sandbox_home,
    )
    .unwrap();
    let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
    let lifecycle = Arc::new(lifecycle::Lifecycle::new());
    let app = tauri::test::mock_builder()
        .manage(state.clone())
        .manage(lifecycle.clone())
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    let handle = app.handle().clone();
    let cleanup = RuntimeSmokeCleanup::new(
        handle.clone(),
        state.clone(),
        tmp.clone(),
        sandbox_port,
        proxy_port,
    );
    science::test_reset_managed_launch_commit_failure_once();
    let canary = format!("rollback-raw-canary-credential-{}", config::new_id());
    let _rollback_seam = sandbox_session::test_arm_rollback_diagnostic_canary(&canary);
    let _cleanup_lifecycle = config::test_arm_pending_cleanup_lifecycle(None);
    let result =
        sandbox_session::one_click_login(handle, state.clone(), lifecycle.as_ref(), None, None);
    let surface = match &result {
        Ok(value) => value.to_string(),
        Err(error) => error.to_string(),
    };
    let target_failure_reached = surface.contains("test-only managed launch commit failure");
    let diagnostic_credential_free = !surface.contains(&canary);
    let valuable_snapshot = sandbox_session::test_rollback_diagnostic_snapshot();
    let cleanup_observation = config::test_pending_cleanup_lifecycle_observation();
    let failed_restore_preserved_registered_snapshot =
        valuable_snapshot.as_ref().is_some_and(|path| {
            path.is_dir()
                && cleanup_observation
                    .attempted_register
                    .as_ref()
                    .is_some_and(|identity| identity.path == *path)
        }) && cleanup_observation.events.is_empty()
            && cleanup_observation.initial_ticket_count == 0
            && cleanup_observation.completion_count == 0;
    cleanup
        .finish()
        .expect("diagnostic redaction fixture must leave no process or temp residue");
    assert!(
            target_failure_reached
                && diagnostic_credential_free
                && failed_restore_preserved_registered_snapshot,
            "compensation diagnostics may expose only a typed safe rollback code, never a raw nested error; a failed restore must preserve the initial ActiveRecovery snapshot without entering REMOVE/CLEAR or completion: target_failure_reached={target_failure_reached}, diagnostic_credential_free={diagnostic_credential_free}, valuable_snapshot_present={}, cleanup_events={}, initial_registration_matches_snapshot={}, initial_ticket_count={}, completion_count={}",
            valuable_snapshot.is_some(),
            cleanup_observation.events.len(),
            valuable_snapshot.as_ref().is_some_and(|path| {
                cleanup_observation
                    .attempted_register
                    .as_ref()
                    .is_some_and(|identity| identity.path == *path)
            }),
            cleanup_observation.initial_ticket_count,
            cleanup_observation.completion_count
        );
}

struct TestChild(std::process::Child);

impl Drop for TestChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn stop_test_sandbox<R: tauri::Runtime>(
    handle: &tauri::AppHandle<R>,
    state: &SharedAppState,
    sandbox_port: u16,
) {
    {
        let mut st = lock(state);
        let AppState {
            sandbox,
            sandbox_url,
            science_runtime,
            science_confirmed_stopped,
            ..
        } = &mut *st;
        let runtime = science_runtime.clone();
        assert!(science::stop_sandbox(
            handle,
            sandbox,
            sandbox_url,
            science::ScienceStopRequest::recover(runtime.as_ref()),
        )
        .is_ok());
        *science_confirmed_stopped = runtime;
        *science_runtime = None;
    }
    wait_http_unreachable(sandbox_port);
}

fn kill_tracked_proxy(state: &SharedAppState, proxy_port: u16) {
    let mut proxy_child = {
        let mut st = lock(state);
        assert_eq!(st.proxy_port, proxy_port);
        assert!(!st.secret.is_empty());
        st.proxy.take().expect("proxy child should be tracked")
    };
    let _ = proxy_child.kill();
    let _ = proxy_child.wait();
    wait_http_unreachable(proxy_port);
}

#[test]
#[ignore = "explicit Acceptance-boundary runtime smoke; uses fake Science and local loopback ports"]
fn isolated_one_click_reuse_status_smoke_with_fake_science() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let tmp = tmpdir("isolated-runtime-smoke");
    let home = tmp.join("home");
    let bin_dir = tmp.join("bin");
    fs::create_dir_all(&home).unwrap();
    let fake_science = write_test_bins(&bin_dir).canonicalize().unwrap();
    let open_log = tmp.join("open.log");
    let science_call_log = tmp.join("science-call.log");
    let route_config_log = tmp.join("route-config.log");
    let mock_upstream = start_mock_upstream();
    let mock_upstream_port = mock_upstream.port;
    let proxy_port = free_port();
    let sandbox_port = free_port();
    assert_ne!(proxy_port, sandbox_port);

    let mut env_guard = EnvGuard::new();
    env_guard.set("HOME", &home);
    env_guard.set("CSSWITCH_REPO", &root);
    env_guard.set("SCIENCE_BIN", &fake_science);
    env_guard.set("CSSWITCH_TEST_OPEN_BIN", bin_dir.join("open"));
    env_guard.set("CSSWITCH_TEST_FAKE_SCIENCE_IDENTITY", "1");
    env_guard.set("CSSWITCH_FAKE_OPEN_LOG", &open_log);
    env_guard.set("CSSWITCH_FAKE_SCIENCE_CALL_LOG", &science_call_log);
    env_guard.set("CSSWITCH_TEST_THIRD_PARTY_CONFIG_LOG", &route_config_log);
    env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
    env_guard.set(
        "PATH",
        format!(
            "{}:/usr/bin:/bin:/usr/sbin:/sbin",
            bin_dir.to_string_lossy()
        ),
    );
    let adoption_update_fixture = env::var_os("CSSWITCH_TEST_SCIENCE_ADOPTION_HEALTHY_DEFER_ONLY")
        .is_some()
        || env::var_os("CSSWITCH_TEST_SCIENCE_ADOPTION_UNPROVEN_PENDING_ONLY").is_some();
    let adoption_updater_bin = adoption_update_fixture.then(|| {
        let updater_bin = home.join(".claude-science/bin/claude-science");
        let initial_updater = fs::read_to_string(&fake_science).unwrap();
        write_padded_test_updater(&updater_bin, &initial_updater);
        env::remove_var("SCIENCE_BIN");
        updater_bin
    });
    let _updater_identity_guard = adoption_updater_bin
        .as_ref()
        .map(|_| science::test_arm_fake_science_updater_identity());

    let fake_key = "csswitch-isolated-fake-key-never-log";
    let profile = Profile {
        id: "mock-relay".into(),
        name: "Mock Relay".into(),
        template_id: "custom".into(),
        category: "custom".into(),
        api_format: "anthropic".into(),
        base_url: format!("http://127.0.0.1:{mock_upstream_port}/anthropic"),
        api_key: fake_key.into(),
        model: "mock-model".into(),
        model_catalog: vec![crate::model_catalog::ModelRoute {
            selector_id: "claude-csswitch-relay-mock-model-0123456789ab".into(),
            display_name: "Mock model".into(),
            upstream_model: "mock-model".into(),
            supports_tools: Some(true),
            ..Default::default()
        }],
        default_model_route_id: "claude-csswitch-relay-mock-model-0123456789ab".into(),
        role_bindings: crate::model_catalog::RoleBindings {
            sonnet: "claude-csswitch-relay-mock-model-0123456789ab".into(),
            opus: "claude-csswitch-relay-mock-model-0123456789ab".into(),
            haiku: "claude-csswitch-relay-mock-model-0123456789ab".into(),
            fable: "claude-csswitch-relay-mock-model-0123456789ab".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let terminal_handoff = env::var_os("CSSWITCH_TEST_GATEWAY_TERMINAL_HANDOFF").is_some();
    let finalize_prepare_failure = env::var_os("CSSWITCH_TEST_FINALIZE_PREPARE_FAILURE").is_some();
    let finalize_completion_failure =
        env::var_os("CSSWITCH_TEST_FINALIZE_COMPLETION_FAILURE").is_some();
    let finalize_replay_provenance_fault =
        env::var("CSSWITCH_TEST_FINALIZE_REPLAY_PROVENANCE_FAULT").ok();
    let mut cfg = Config {
        profiles: vec![profile],
        active_id: "mock-relay".into(),
        proxy_port,
        sandbox_port,
        ..Default::default()
    };
    if terminal_handoff {
        cfg.runtime_transaction = Some(config::RuntimeTransactionRecord::V2(
            config::RuntimeTransactionV2 {
                schema_version: config::RUNTIME_TRANSACTION_SCHEMA_VERSION_V2,
                transaction_id: "gateway-terminal-production-handoff".into(),
                operation: config::RuntimeTransactionOperation::ProfileSwitch,
                target_profile_id: cfg.active_id.clone(),
                phase: config::RuntimeTransactionPhase::RecoverInterruptedGateway,
                runtime_fingerprint: None,
                environment_exposure: config::RuntimeEnvironmentExposure::NotExposed,
                snapshot_ticket: None,
                previous_binding: None,
                previous_gateway: None,
                compensation: config::RuntimeCompensationState::NotStarted,
                gateway_stop_outcome: config::RuntimeGatewayStopOutcome::AbsentAfterAttempt,
                prior_stop: config::RuntimePriorStopState::NotRequired,
                finalize: config::RuntimeFinalizeState::NotStarted,
            },
        ));
    }
    let config_dir = config::default_dir();
    config::save_to(&config_dir, &cfg).unwrap();

    let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
    let lifecycle = Arc::new(lifecycle::Lifecycle::new());
    let app = tauri::test::mock_builder()
        .manage(state.clone())
        .manage(lifecycle.clone())
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    let handle = app.handle().clone();

    let cleanup = RuntimeSmokeCleanup::new(
        handle.clone(),
        state.clone(),
        tmp.clone(),
        sandbox_port,
        proxy_port,
    );
    let prepare_failure_guard = finalize_prepare_failure.then(|| {
        config::test_arm_pending_cleanup_lifecycle(Some(
            config::PendingCleanupPublishFault::Prepare,
        ))
    });
    let completion_failure_guard = finalize_completion_failure.then(|| {
        sandbox_session::test_arm_one_click_finalize_completion_failure(config_dir.clone())
    });
    let first = if terminal_handoff || finalize_prepare_failure || finalize_completion_failure {
        super::one_click_login_cmd(handle.clone(), state.clone(), lifecycle.clone(), None)
            .expect("production command must not return an IPC error")
    } else {
        sandbox_session::one_click_login(
            handle.clone(),
            state.clone(),
            lifecycle.as_ref(),
            None,
            None,
        )
        .expect("first one-click should start proxy and sandbox")
    };
    if finalize_prepare_failure || finalize_completion_failure {
        assert_eq!(first["status"], "degraded");
        assert_eq!(first["recovery_status"], "manual_recovery_required");
        let pending = config::load_from(&config_dir).unwrap();
        assert!(pending.runtime_binding.is_none());
        let pending_record = pending
            .runtime_transaction
            .as_ref()
            .and_then(config::RuntimeTransactionRecord::as_v2)
            .expect("finalize failure must retain the exact V2 record");
        assert_eq!(
            pending_record.operation,
            config::RuntimeTransactionOperation::OneClick
        );
        assert_eq!(pending_record.target_profile_id, "mock-relay");
        let (pending_binding, pending_adoption_attempt_id) = match &pending_record.finalize {
            config::RuntimeFinalizeState::Intent {
                action:
                    config::RuntimeFinalizeAction::CommitBinding {
                        binding,
                        science_adoption_attempt_id: Some(attempt_id),
                    },
            } if !attempt_id.is_empty() => (binding, attempt_id),
            other => panic!("finalize failure retained the wrong action: {other:?}"),
        };
        assert_eq!(pending_binding.profile_id, "mock-relay");
        assert_eq!(pending_adoption_attempt_id.len(), 32);
        assert_eq!(
            pending_binding.science_adoption_attempt_id.as_deref(),
            Some(pending_adoption_attempt_id.as_str()),
            "binding and finalize intent must preserve the same exact adoption attempt"
        );
        {
            let runtime = lock(&state);
            assert!(runtime.proxy.is_some());
            assert!(runtime.science_runtime.is_some());
            assert_eq!(runtime.sandbox_port, sandbox_port);
            assert!(runtime.sandbox_url.is_some());
        }
        let manifest = config::read_pending_authority_cleanup_manifest(&config_dir)
            .unwrap()
            .unwrap();
        let manifest: serde_json::Value = serde_json::from_slice(&manifest).unwrap();
        if finalize_prepare_failure {
            assert_eq!(manifest["disposition"], "active_recovery");
            assert_eq!(manifest["entries"].as_array().map(Vec::len), Some(1));
            let snapshot = PathBuf::from(manifest["entries"][0]["path"].as_str().unwrap());
            assert!(snapshot.is_dir());
        } else {
            assert_eq!(manifest["entries"].as_array().map(Vec::len), Some(0));
        }
        drop(prepare_failure_guard);
        drop(completion_failure_guard);
        let replacement = config::RuntimeBindingCommit {
            profile_id: "replacement".into(),
            route_fp: "replacement-route".into(),
            catalog_fp: "replacement-catalog".into(),
            binding_fp: "replacement-binding".into(),
            science_adoption_attempt_id: None,
        };
        config::update(&config_dir, |current| {
            current.runtime_binding = Some(replacement.clone());
        })
        .unwrap();
        let drifted = fs::read(config_dir.join("config.json")).unwrap();
        let rejected =
            super::one_click_login_cmd(handle.clone(), state.clone(), lifecycle.clone(), None)
                .expect("fresh finalize authority drift must use the frozen IPC failure DTO");
        assert_eq!(rejected["status"], "error");
        assert_eq!(rejected["recovery_status"], "manual_recovery_required");
        assert!(
            rejected["message"]
                .as_str()
                .is_some_and(|message| message.contains("success finalize")),
            "{rejected}"
        );
        assert_eq!(fs::read(config_dir.join("config.json")).unwrap(), drifted);
        config::update(&config_dir, |current| {
            current.runtime_binding = None;
        })
        .unwrap();
        if let Some(provenance_fault) = finalize_replay_provenance_fault.as_deref() {
            let receipt_path = config_dir.join("science-managed-launch.v1.json");
            let original_receipt = fs::read(&receipt_path)
                .expect("pending finalize must retain its exact managed Science V2 receipt");
            match provenance_fault {
                "absent" => fs::remove_file(&receipt_path).unwrap(),
                "v1" | "wrong-attempt" | "runtime-drift" => {
                    let mut receipt: serde_json::Value =
                        serde_json::from_slice(&original_receipt).unwrap();
                    match provenance_fault {
                        "v1" => {
                            receipt["schema_version"] = serde_json::json!(1);
                            let object = receipt.as_object_mut().unwrap();
                            object.remove("runtime_source");
                            object.remove("runtime_version");
                            object.remove("adoption_attempt_id");
                        }
                        "wrong-attempt" => {
                            receipt["adoption_attempt_id"] = serde_json::json!("f".repeat(32));
                        }
                        "runtime-drift" => {
                            receipt["runtime_sha256"] = serde_json::json!("f".repeat(64));
                        }
                        _ => unreachable!(),
                    }
                    fs::write(&receipt_path, serde_json::to_vec(&receipt).unwrap()).unwrap();
                    fs::set_permissions(&receipt_path, fs::Permissions::from_mode(0o600)).unwrap();
                }
                other => panic!("unknown finalize replay provenance fault: {other}"),
            }
            let before_rejected_replay = fs::read(config_dir.join("config.json")).unwrap();
            let rejected =
                super::one_click_login_cmd(handle.clone(), state.clone(), lifecycle.clone(), None)
                    .expect("provenance rejection must use the frozen IPC failure DTO");
            assert_eq!(rejected["status"], "error", "{rejected}");
            assert_eq!(
                rejected["recovery_status"], "manual_recovery_required",
                "{rejected}"
            );
            assert_eq!(
                fs::read(config_dir.join("config.json")).unwrap(),
                before_rejected_replay,
                "{provenance_fault} receipt must preserve the finalize journal and binding"
            );
            fs::write(&receipt_path, original_receipt).unwrap();
            fs::set_permissions(&receipt_path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let replayed =
            super::one_click_login_cmd(handle.clone(), state.clone(), lifecycle.clone(), None)
                .expect("next production command must replay finalize before healthy reopen");
        assert_ne!(replayed["status"], "failed");
        let finalized = config::load_from(&config_dir).unwrap();
        assert_eq!(
            finalized
                .runtime_binding
                .as_ref()
                .map(|binding| binding.profile_id.as_str()),
            Some("mock-relay")
        );
        assert_eq!(
            finalized
                .runtime_binding
                .as_ref()
                .and_then(|binding| binding.science_adoption_attempt_id.as_deref()),
            Some(pending_adoption_attempt_id.as_str())
        );
        assert!(finalized.runtime_transaction.is_none());
        let manifest = config::read_pending_authority_cleanup_manifest(&config_dir)
            .unwrap()
            .unwrap();
        let manifest: serde_json::Value = serde_json::from_slice(&manifest).unwrap();
        assert_eq!(manifest["entries"].as_array().map(Vec::len), Some(0));
        cleanup
            .finish()
            .expect("prepare-failure replay fixture must leave no process residue");
        return;
    }
    assert_ne!(first["status"], "failed", "first one-click must succeed");
    assert_eq!(first["action"], "started");
    assert!(
        first.get("url").is_none(),
        "one-time URL must stay backend-only"
    );
    wait_http_health(sandbox_port);
    let fake_state_dir = home
        .join(config::CONFIG_DIR_NAME)
        .join("sandbox")
        .join("home")
        .join(".claude-science")
        .join("fake-science");
    let first_pid = fs::read_to_string(fake_state_dir.join("pid")).unwrap();
    assert_eq!(
        fs::read_to_string(fake_state_dir.join("serve-count")).unwrap(),
        "1"
    );
    let committed = config::load_from(&config_dir).unwrap();
    let running_runtime = lock(&state)
        .science_runtime
        .clone()
        .expect("cold start must publish the selected Science runtime");
    let expected_binding = crate::runtime::provider::desired_runtime_binding(
        &committed,
        committed.active_profile().unwrap(),
        &running_runtime,
    )
    .unwrap();
    assert_eq!(committed.runtime_binding, Some(expected_binding));
    assert!(
        committed.runtime_transaction.is_none(),
        "successful cold start must clear its transaction journal"
    );
    let (gateway_pid, gateway_secret, gateway_launch_id, gateway_context_committed) = {
        let mut authority = lock(&state);
        let gateway = authority
            .proxy
            .as_mut()
            .expect("cold start must publish an owned Gateway child");
        assert!(gateway.try_wait().unwrap().is_none());
        (
            gateway.id(),
            authority.secret.clone(),
            authority.launch_id.clone(),
            authority
                .gateway_launch_context
                .as_ref()
                .is_some_and(|context| {
                    context.profile.id == "mock-relay"
                        && context.science_runtime.as_ref() == Some(&running_runtime)
                }),
        )
    };
    assert!(gateway_context_committed);
    let gateway_health = crate::proc::http_gateway_health(
        proxy_port,
        Some(&gateway_secret),
        crate::runtime::operation::LOCAL_HEALTH_TIMEOUT_MS,
    )
    .expect("cold start Gateway receipt must resolve to managed health");
    assert_eq!(gateway_health.launch_id, gateway_launch_id);
    assert_eq!(listener_pid_if_unique(proxy_port), Some(gateway_pid));
    assert!(config_dir
        .join("runtime/skill-install-bridge.key")
        .is_file());

    let managed_launch_path = config_dir.join("science-managed-launch.v1.json");
    let managed_launch: serde_json::Value =
        serde_json::from_slice(&fs::read(&managed_launch_path).unwrap()).unwrap();
    assert_eq!(managed_launch["port"], sandbox_port);
    assert_eq!(
        managed_launch["listener_pid"],
        first_pid.trim().parse::<u32>().unwrap()
    );
    let managed_launch_bytes = fs::read(&managed_launch_path).unwrap();
    let proof = science::ScienceHostAdapter::prove_managed_healthy(sandbox_port, &running_runtime)
        .expect("fresh exact managed receipt and healthy listener must produce a typed proof");
    let replaced_receipt = managed_launch_path.with_extension("replacement");
    fs::write(&replaced_receipt, b"{}").unwrap();
    fs::set_permissions(&replaced_receipt, fs::Permissions::from_mode(0o600)).unwrap();
    fs::rename(&replaced_receipt, &managed_launch_path).unwrap();
    assert!(
        !science::ScienceHostAdapter::revalidate_managed_healthy(&proof),
        "receipt replacement after proof must fail closed"
    );
    let restored_receipt = managed_launch_path.with_extension("restored");
    fs::write(&restored_receipt, &managed_launch_bytes).unwrap();
    fs::set_permissions(&restored_receipt, fs::Permissions::from_mode(0o600)).unwrap();
    fs::rename(&restored_receipt, &managed_launch_path).unwrap();
    assert!(
        !science::ScienceHostAdapter::revalidate_managed_healthy(&proof),
        "restoring equal receipt bytes must not revive the replaced token identity"
    );
    assert!(
        science::ScienceHostAdapter::prove_managed_healthy(sandbox_port, &running_runtime)
            .is_some(),
        "restored exact receipt must be reproved instead of reviving the stale token"
    );
    if adoption_update_fixture {
        let updater_bin = adoption_updater_bin
            .as_ref()
            .expect("healthy adoption fixture must seed the fixed updater path");
        let updater_script = fs::read_to_string(&fake_science)
            .unwrap()
            .replace("0.0.0-csswitch-test", "0.0.1-csswitch-test");
        write_padded_test_updater(updater_bin, &updater_script);
        let (version_cache, confirmed_stopped, running_before) = {
            let authority = lock(&state);
            (
                authority.science_version_cache.clone(),
                authority.science_confirmed_stopped.clone(),
                authority.science_runtime.clone(),
            )
        };
        let preflight =
            science::science_runtime_preflight(&version_cache, confirmed_stopped.as_ref())
                .expect("healthy production preflight must reuse the fixed active runtime");
        assert_eq!(preflight["status"], "installed_ready");
        assert_eq!(preflight["adoption_record_status"], "recorded");
        let before_update = science::science_runtime_update_status()
            .expect("startup preflight must leave update status readable");
        assert!(
            before_update["pending_update"].is_null(),
            "startup preflight must not discover a pending update"
        );
        let before_ledger = science::science_adoption_ledger_json_for_test()
            .expect("startup preflight must leave the adoption ledger readable");
        assert!(
            before_ledger["attempts"]
                .as_array()
                .is_some_and(|attempts| attempts
                    .iter()
                    .all(|attempt| attempt["decision"] != "deferred_healthy")),
            "startup preflight must not probe or defer an update candidate"
        );
        let unproven =
            env::var_os("CSSWITCH_TEST_SCIENCE_ADOPTION_UNPROVEN_PENDING_ONLY").is_some();
        if unproven {
            fs::write(&managed_launch_path, b"{}").unwrap();
            fs::set_permissions(&managed_launch_path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let update = crate::run_science_runtime_update_once(&handle)
            .expect("due background check must publish the changed runtime as pending");
        assert_eq!(update["check_status"], "checked");
        assert_eq!(
            update["adoption_record_status"],
            if unproven { "degraded" } else { "recorded" }
        );
        assert_eq!(
            update["pending_update"]["version"],
            "claude-science 0.0.1-csswitch-test"
        );
        if unproven {
            fs::write(&managed_launch_path, &managed_launch_bytes).unwrap();
            fs::set_permissions(&managed_launch_path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let repeated = crate::run_science_runtime_update_once(&handle)
            .expect("repeated scheduler pass must preserve the pending user choice");
        assert_eq!(repeated["check_status"], "pending_choice_waiting");
        assert_eq!(repeated["adoption_record_status"], "not_needed");
        assert_eq!(
            lock(&state).science_runtime.as_ref(),
            running_before.as_ref(),
            "preflight and background update check must not replace the running Science owner"
        );
        assert_eq!(
            fs::read_to_string(fake_state_dir.join("pid")).unwrap(),
            first_pid,
            "healthy preflight must not replace the running Science process"
        );
        assert_eq!(
            fs::read_to_string(fake_state_dir.join("serve-count")).unwrap(),
            "1",
            "healthy preflight must not restart Science"
        );
        let ledger = science::science_adoption_ledger_json_for_test()
            .expect("background update check must publish the adoption ledger");
        let deferred = ledger["attempts"].as_array().and_then(|attempts| {
            attempts
                .iter()
                .rev()
                .find(|attempt| attempt["decision"] == "deferred_healthy")
        });
        if unproven {
            assert!(
                deferred.is_none(),
                "an unproven pending candidate must not inherit a running predecessor"
            );
            cleanup
                .finish()
                .expect("unproven pending fixture must cleanly stop after receipt restoration");
            return;
        }
        let deferred = deferred.expect("background update check must persist deferred_healthy");
        assert_eq!(
            deferred["predecessor"]["version"],
            "claude-science 0.0.0-csswitch-test"
        );
        assert_eq!(
            deferred["candidate"]["version"],
            "claude-science 0.0.1-csswitch-test"
        );
        cleanup
            .finish()
            .expect("healthy background update fixture must cleanly stop");
        return;
    }
    if env::var_os("CSSWITCH_TEST_R0_COLD_START_ONLY").is_some() {
        cleanup
            .finish()
            .expect("focused cold-start fixture must cleanly stop");
        return;
    }
    assert_eq!(call_count(&science_call_log, "--version"), 1);
    assert_eq!(call_count(&science_call_log, "status"), 1);
    assert_eq!(call_count(&science_call_log, "url"), 2);
    assert_eq!(call_count(&route_config_log, "configure-third-party"), 1);

    let pending_cleanup_manifest = config_dir.join(config::PENDING_AUTHORITY_CLEANUP_MANIFEST_FILE);
    let malformed_pending_cleanup = b"malformed-pending-cleanup-must-not-block-healthy-reopen\n";
    fs::write(&pending_cleanup_manifest, malformed_pending_cleanup).unwrap();
    fs::set_permissions(&pending_cleanup_manifest, fs::Permissions::from_mode(0o600)).unwrap();
    let second = sandbox_session::one_click_login(
        handle.clone(),
        state.clone(),
        lifecycle.as_ref(),
        None,
        None,
    )
    .expect("second one-click should reuse running sandbox");
    assert_eq!(second["action"], "reopened");
    assert_eq!(
        fs::read(&pending_cleanup_manifest).unwrap(),
        malformed_pending_cleanup,
        "healthy reopen must decide before and bypass cold/recovery pending cleanup"
    );
    fs::remove_file(&pending_cleanup_manifest).unwrap();
    assert!(
        second.get("url").is_none(),
        "one-time URL must stay backend-only"
    );
    assert_eq!(
        fs::read_to_string(fake_state_dir.join("pid")).unwrap(),
        first_pid
    );
    assert_eq!(
        fs::read_to_string(fake_state_dir.join("serve-count")).unwrap(),
        "1"
    );
    assert_eq!(call_count(&science_call_log, "--version"), 1);
    assert_eq!(call_count(&science_call_log, "status"), 2);
    assert_eq!(call_count(&science_call_log, "url"), 3);
    assert_eq!(call_count(&route_config_log, "configure-third-party"), 1);
    if env::var_os("CSSWITCH_TEST_O1A_HEALTHY_ONLY").is_some() {
        cleanup
            .finish()
            .expect("focused O1-A healthy-reopen fixture must cleanly stop");
        return;
    }

    let managed_launch_metadata = managed_launch_path
        .symlink_metadata()
        .expect("managed launch receipt should be committed after a verified start");
    assert!(managed_launch_metadata.file_type().is_file());
    assert_eq!(managed_launch_metadata.permissions().mode() & 0o077, 0);
    assert!(managed_launch_metadata.len() > 0);
    assert!(managed_launch_metadata.len() <= 16 * 1024);

    let version_before_fresh_probe = call_count(&science_call_log, "--version");
    let fresh_probe = science::probe_sandbox_runtime_cached(
        sandbox_port,
        &science::ScienceVersionCache::default(),
    )
    .expect("fresh Science probe should inspect only the isolated runtime candidates");
    assert_eq!(fresh_probe.0, science::SandboxScienceState::RunningHealthy);
    let reattached_runtime = fresh_probe
        .1
        .expect("managed receipt should recover the exact Science runtime");
    assert_eq!(reattached_runtime.path, fake_science);
    assert!(science::runtime_identity_is_current(&reattached_runtime));
    assert_eq!(
        fs::read_to_string(fake_state_dir.join("pid")).unwrap(),
        first_pid
    );
    assert_eq!(
        fs::read_to_string(fake_state_dir.join("serve-count")).unwrap(),
        "1"
    );
    assert_eq!(
        call_count(&science_call_log, "--version"),
        version_before_fresh_probe + 1,
        "fresh Science cache must revalidate the executable version exactly once"
    );
    assert_eq!(call_count(&science_call_log, "--version"), 2);
    assert_eq!(call_count(&science_call_log, "status"), 3);
    assert_eq!(call_count(&science_call_log, "url"), 3);
    assert_eq!(call_count(&route_config_log, "configure-third-party"), 1);

    super::open_url_inner(&state)
        .expect("first manual open should refresh the one-time Science URL");
    super::open_url_inner(&state)
        .expect("second manual open should refresh the one-time Science URL again");
    assert_eq!(call_count(&science_call_log, "url"), 5);
    let opened_urls: Vec<_> = fs::read_to_string(&open_log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert!(opened_urls.len() >= 4);
    assert!(opened_urls[opened_urls.len() - 2].ends_with("/?nonce=4"));
    assert!(opened_urls[opened_urls.len() - 1].ends_with("/?nonce=5"));
    assert_ne!(
        opened_urls[opened_urls.len() - 2],
        opened_urls[opened_urls.len() - 1]
    );

    let fail_once = tmp.join("open-failed-once");
    env_guard.set("CSSWITCH_FAKE_OPEN_FAIL_ONCE_FILE", &fail_once);
    let failed_open = super::open_url_inner(&state)
        .expect("manual opener failure should be a structured UI result");
    assert_eq!(failed_open["status"], "error");
    assert!(failed_open["fallback_url"]
        .as_str()
        .unwrap()
        .ends_with("/?nonce=6"));
    let retried_open = super::open_url_inner(&state)
        .expect("retry should fetch and submit another fresh Science URL");
    assert_eq!(retried_open["status"], "ok");
    assert!(retried_open["fallback_url"].is_null());
    assert_eq!(call_count(&science_call_log, "url"), 7);

    let route_check =
        lifecycle.with_serialized(|| sandbox_session::force_third_party_reconcile(&handle, &state));
    let route_check = route_check.unwrap();
    assert_eq!(
        route_check.status,
        sandbox_session::SkillRouteRepairStatus::Synchronized
    );
    assert_eq!(route_check.message, "Skill 路由已强制核验并同步。");
    assert_eq!(call_count(&science_call_log, "--version"), 3);
    assert_eq!(call_count(&science_call_log, "status"), 4);
    assert_eq!(call_count(&science_call_log, "url"), 8);
    assert_eq!(call_count(&route_config_log, "configure-third-party"), 2);

    stop_test_sandbox(&handle, &state, sandbox_port);
    let mut cold_start_ms = Vec::new();
    for cycle in 0..5 {
        let status_before_preflight = call_count(&science_call_log, "status");
        let (version_cache, confirmed_stopped) = {
            let st = lock(&state);
            (
                st.science_version_cache.clone(),
                st.science_confirmed_stopped.clone(),
            )
        };
        let preflight =
            science::science_runtime_preflight(&version_cache, confirmed_stopped.as_ref())
                .expect("explicit preflight should refresh the stopped runtime selection");
        assert_eq!(preflight["status"], "installed_ready");
        assert_eq!(
            call_count(&science_call_log, "status"),
            status_before_preflight + 1,
            "each explicit preflight should perform one bounded status probe"
        );
        let status_before_restart = call_count(&science_call_log, "status");
        let started_at = Instant::now();
        let restarted = sandbox_session::one_click_login(
            handle.clone(),
            state.clone(),
            lifecycle.as_ref(),
            None,
            None,
        )
        .expect("normal cold start should not re-probe or reconfigure");
        cold_start_ms.push(started_at.elapsed().as_millis());
        assert_eq!(restarted["action"], "started");
        assert_eq!(
            call_count(&science_call_log, "status"),
            status_before_restart,
            "confirmed-stop one-click should not repeat the explicit preflight probe"
        );
        if cycle < 4 {
            stop_test_sandbox(&handle, &state, sandbox_port);
        }
    }
    let mut sorted_cold_start_ms = cold_start_ms.clone();
    sorted_cold_start_ms.sort_unstable();
    eprintln!(
        "focused cold starts ms={cold_start_ms:?} median_ms={}",
        sorted_cold_start_ms[2]
    );
    assert_eq!(call_count(&science_call_log, "--version"), 3);
    assert_eq!(call_count(&science_call_log, "status"), 9);
    assert_eq!(call_count(&science_call_log, "url"), 13);
    assert_eq!(call_count(&route_config_log, "configure-third-party"), 2);
    assert_eq!(
        fs::read_to_string(fake_state_dir.join("serve-count")).unwrap(),
        "6"
    );

    stop_test_sandbox(&handle, &state, sandbox_port);
    let upgraded_script = fs::read_to_string(&fake_science)
        .unwrap()
        .replace("0.0.0-csswitch-test", "0.0.1-csswitch-test");
    let upgraded_candidate = bin_dir.join("claude-science-upgraded");
    write_executable(&upgraded_candidate, &upgraded_script);
    fs::rename(&upgraded_candidate, &fake_science).unwrap();
    let (version_cache, confirmed_stopped) = {
        let st = lock(&state);
        (
            st.science_version_cache.clone(),
            st.science_confirmed_stopped.clone(),
        )
    };
    let status_before_upgrade_preflight = call_count(&science_call_log, "status");
    assert_eq!(
        science::science_runtime_preflight(&version_cache, confirmed_stopped.as_ref()).unwrap()
            ["status"],
        "installed_ready"
    );
    assert_eq!(
        call_count(&science_call_log, "status"),
        status_before_upgrade_preflight + 1,
        "binary replacement preflight should perform one bounded status probe"
    );
    let status_before_upgraded_start = call_count(&science_call_log, "status");
    let upgraded = sandbox_session::one_click_login(
        handle.clone(),
        state.clone(),
        lifecycle.as_ref(),
        None,
        None,
    )
    .expect("binary replacement should re-probe and reconcile once");
    assert_eq!(upgraded["action"], "started");
    assert_eq!(
        call_count(&science_call_log, "status"),
        status_before_upgraded_start,
        "confirmed-stop upgraded start should not repeat the explicit preflight probe"
    );
    assert_eq!(call_count(&science_call_log, "--version"), 4);
    assert_eq!(call_count(&science_call_log, "status"), 10);
    assert_eq!(call_count(&science_call_log, "url"), 15);
    assert_eq!(call_count(&route_config_log, "configure-third-party"), 3);
    assert_eq!(
        fs::read_to_string(fake_state_dir.join("serve-count")).unwrap(),
        "7"
    );

    let status = super::status(app.state::<SharedAppState>());
    assert_eq!(status["proxy"], "green");
    assert_eq!(status["sandbox"], "green");
    assert_eq!(status["upstream"], "green");
    assert_eq!(status["active_profile"]["id"], "mock-relay");
    assert_eq!(status["science"]["sandbox"]["port"], sandbox_port);
    assert_eq!(status["science"]["schema_version"], 1);
    assert!(status["last_error"].is_null());

    let doctor = std::process::Command::new(root.join("scripts/doctor.sh"))
        .env("HOME", &home)
        .env("SCIENCE_BIN", &fake_science)
        .env("CSSWITCH_CONFIG", config_dir.join("config.json"))
        .env("CSSWITCH_PROXY_PORT", proxy_port.to_string())
        .env("CSSWITCH_SANDBOX_PORT", sandbox_port.to_string())
        .output()
        .expect("doctor should run");
    assert!(doctor.status.success());
    let doctor_out = String::from_utf8_lossy(&doctor.stdout);
    assert!(doctor_out.contains("真实 HOME 检查默认跳过"));
    assert!(!doctor_out.contains(&format!("{}/.claude-science", home.display())));

    let cfg_after = config::load_from(&config_dir).unwrap();
    let secret = cfg_after.secret;
    assert!(!secret.is_empty());
    let doctor_err = String::from_utf8_lossy(&doctor.stderr);
    assert!(!doctor_out.contains(fake_key));
    assert!(!doctor_out.contains(&secret));
    assert!(!doctor_err.contains(fake_key));
    assert!(!doctor_err.contains(&secret));
    assert!(!first.to_string().contains(fake_key));
    assert!(!first.to_string().contains(&secret));
    assert!(!second.to_string().contains(fake_key));
    assert!(!second.to_string().contains(&secret));
    let opened = fs::read_to_string(&open_log).unwrap_or_default();
    assert!(!opened.contains(fake_key));
    assert!(!opened.contains(&secret));
    for name in ["proxy.log", "sandbox.log", "operation.log"] {
        let body = fs::read_to_string(config_dir.join("logs").join(name))
            .unwrap_or_else(|e| panic!("expected {name} to exist: {e}"));
        assert!(!body.contains(fake_key), "{name} leaked fake key");
        assert!(!body.contains(&secret), "{name} leaked path secret");
    }

    let valid_receipt_bytes = fs::read(&managed_launch_path).unwrap();
    let valid_receipt: serde_json::Value = serde_json::from_slice(&valid_receipt_bytes).unwrap();
    let valid_receipt_metadata = managed_launch_path.symlink_metadata().unwrap();
    assert_eq!(
        valid_receipt_metadata.uid(),
        unsafe { libc::geteuid() },
        "managed launch receipt must be owned by the current user"
    );
    let listener_pid_before_rejection = fs::read_to_string(fake_state_dir.join("pid")).unwrap();
    let actual_listener_pid = unique_listener_pid(sandbox_port);
    assert_eq!(
        valid_receipt["listener_pid"].as_u64(),
        Some(actual_listener_pid as u64)
    );
    let actual_process_start = process_start_identity(actual_listener_pid);
    assert_eq!(
        valid_receipt["process_start"].as_str(),
        Some(actual_process_start.as_str())
    );
    let helper_process = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .expect("controlled non-listener helper should start");
    let helper_pid = helper_process.id();
    let helper_process_start = process_start_identity(helper_pid);
    let _helper_process = TestChild(helper_process);
    let assert_receipt_rejected = |case: &str| {
        let version_before = call_count(&science_call_log, "--version");
        let status_before = call_count(&science_call_log, "status");
        let observed = science::probe_sandbox_runtime_cached(
            sandbox_port,
            &science::ScienceVersionCache::default(),
        )
        .unwrap_or_else(|error| panic!("{case} probe should classify safely: {error}"));
        assert_eq!(
            observed,
            (science::SandboxScienceState::Unknown, None),
            "{case} must reject the managed launch receipt"
        );
        assert_eq!(
            call_count(&science_call_log, "--version"),
            version_before + 1,
            "{case} must use a fresh executable identity"
        );
        assert_eq!(
            call_count(&science_call_log, "status"),
            status_before + 1,
            "{case} must use one bounded status probe"
        );
        assert_eq!(
            fs::read_to_string(fake_state_dir.join("pid")).unwrap(),
            listener_pid_before_rejection,
            "{case} must not signal the listener"
        );
        assert!(
            TcpStream::connect(("127.0.0.1", sandbox_port)).is_ok(),
            "{case} must leave the listener reachable"
        );
        assert_eq!(
            unique_listener_pid(sandbox_port),
            actual_listener_pid,
            "{case} must leave the same kernel listener PID"
        );
        assert_eq!(
            process_start_identity(actual_listener_pid),
            actual_process_start,
            "{case} must not replace or recycle the listener process"
        );
    };

    let mut receipt_mutants = Vec::new();
    for field in [
        "schema_version",
        "port",
        "runtime_device",
        "runtime_inode",
        "runtime_size",
        "runtime_modified_seconds",
        "runtime_modified_nanoseconds",
        "runtime_mode",
        "data_dir_device",
        "data_dir_inode",
    ] {
        let mut mutant = valid_receipt.clone();
        let value = mutant[field]
            .as_u64()
            .unwrap_or_else(|| panic!("{field} must be numeric"));
        let changed = if value == u64::MAX {
            value - 1
        } else {
            value + 1
        };
        mutant[field] = serde_json::Value::from(changed);
        receipt_mutants.push((field, mutant));
    }
    for field in ["runtime_path", "data_dir"] {
        let mut mutant = valid_receipt.clone();
        let value = mutant[field]
            .as_str()
            .unwrap_or_else(|| panic!("{field} must be a path string"));
        mutant[field] = serde_json::Value::from(format!("{value}.tampered"));
        receipt_mutants.push((field, mutant));
    }
    let mut listener_pid_mutant = valid_receipt.clone();
    listener_pid_mutant["listener_pid"] = serde_json::Value::from(helper_pid);
    listener_pid_mutant["process_start"] = serde_json::Value::from(helper_process_start);
    receipt_mutants.push(("non_listener_pid", listener_pid_mutant));
    let mut process_start_mutant = valid_receipt.clone();
    process_start_mutant["process_start"] = serde_json::Value::from(format!(
        "{} tampered",
        valid_receipt["process_start"].as_str().unwrap()
    ));
    receipt_mutants.push(("process_start", process_start_mutant));
    let mut fingerprint_mutant = valid_receipt.clone();
    fingerprint_mutant["runtime_sha256"] = serde_json::Value::from("00".repeat(32));
    receipt_mutants.push(("runtime_sha256", fingerprint_mutant));
    let mut unknown_field_mutant = valid_receipt.clone();
    unknown_field_mutant
        .as_object_mut()
        .unwrap()
        .insert("unexpected_field".into(), serde_json::Value::Bool(true));
    receipt_mutants.push(("unknown_field", unknown_field_mutant));

    for (case, mutant) in receipt_mutants {
        fs::write(&managed_launch_path, serde_json::to_vec(&mutant).unwrap()).unwrap();
        fs::set_permissions(&managed_launch_path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_receipt_rejected(case);
        fs::write(&managed_launch_path, &valid_receipt_bytes).unwrap();
    }

    fs::set_permissions(&managed_launch_path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_receipt_rejected("widened_mode");
    fs::set_permissions(&managed_launch_path, fs::Permissions::from_mode(0o600)).unwrap();

    let mut oversized_receipt = valid_receipt_bytes.clone();
    oversized_receipt.resize(16 * 1024 + 1, b' ');
    assert!(
        serde_json::from_slice::<serde_json::Value>(&oversized_receipt).is_ok(),
        "oversized receipt mutant must remain valid JSON"
    );
    fs::write(&managed_launch_path, oversized_receipt).unwrap();
    assert_receipt_rejected("oversized_receipt");
    fs::write(&managed_launch_path, &valid_receipt_bytes).unwrap();

    let symlink_target = config_dir.join("science-managed-launch-target.json");
    fs::write(&symlink_target, &valid_receipt_bytes).unwrap();
    fs::set_permissions(&symlink_target, fs::Permissions::from_mode(0o600)).unwrap();
    fs::remove_file(&managed_launch_path).unwrap();
    symlink(&symlink_target, &managed_launch_path).unwrap();
    assert_receipt_rejected("symlink_receipt");
    fs::remove_file(&managed_launch_path).unwrap();
    fs::remove_file(&symlink_target).unwrap();
    fs::write(&managed_launch_path, &valid_receipt_bytes).unwrap();
    fs::set_permissions(&managed_launch_path, fs::Permissions::from_mode(0o600)).unwrap();

    fs::remove_file(&managed_launch_path)
        .expect("isolated managed launch receipt should be removable for rejection injection");
    let version_before_rejected_reattach = call_count(&science_call_log, "--version");
    let status_before_rejected_reattach = call_count(&science_call_log, "status");
    let rejected = science::probe_sandbox_runtime_cached(
        sandbox_port,
        &science::ScienceVersionCache::default(),
    )
    .expect("missing receipt should be a classified Science state");
    assert_eq!(
        rejected,
        (science::SandboxScienceState::Unknown, None),
        "fresh Science probe must reject a listener after its receipt is removed"
    );
    assert_eq!(
        call_count(&science_call_log, "--version"),
        version_before_rejected_reattach + 1,
        "receipt rejection must still revalidate the executable through a fresh cache"
    );
    assert_eq!(
        call_count(&science_call_log, "status"),
        status_before_rejected_reattach + 1
    );
    assert_eq!(
        fs::read_to_string(fake_state_dir.join("pid")).unwrap(),
        listener_pid_before_rejection,
        "rejected adoption must not signal the unproven listener"
    );
    assert!(
        TcpStream::connect(("127.0.0.1", sandbox_port)).is_ok(),
        "rejected adoption must leave the unproven listener untouched"
    );
    assert_eq!(
        unique_listener_pid(sandbox_port),
        actual_listener_pid,
        "missing receipt rejection must leave the same kernel listener PID"
    );
    assert_eq!(
        process_start_identity(actual_listener_pid),
        actual_process_start,
        "missing receipt rejection must not recycle the listener process"
    );
    fs::write(&managed_launch_path, &valid_receipt_bytes).unwrap();
    fs::set_permissions(&managed_launch_path, fs::Permissions::from_mode(0o600)).unwrap();

    let drift_marker = tmp.join("process-start-drift");
    env_guard.set(
        "CSSWITCH_TEST_PROCESS_START_DRIFT_PID",
        actual_listener_pid.to_string(),
    );
    env_guard.set("CSSWITCH_TEST_PROCESS_START_DRIFT_MARKER", &drift_marker);
    let drift_stop = {
        let mut st = lock(&state);
        super::stop_sandbox_state(&handle, &mut st)
    };
    let listener_after_drift_stop = listener_pid_if_unique(sandbox_port);
    let receipt_preserved_after_drift = managed_launch_path.is_file();
    env::remove_var("CSSWITCH_TEST_PROCESS_START_DRIFT_PID");
    env::remove_var("CSSWITCH_TEST_PROCESS_START_DRIFT_MARKER");
    let process_start_after_drift_stop = process_start_identity_if_alive(actual_listener_pid);

    let (runtime_before_stop_oracles, url_before_stop_oracles) = {
        let st = lock(&state);
        (
            st.science_runtime
                .clone()
                .expect("failed drift stop must retain the exact runtime"),
            st.sandbox_url.clone(),
        )
    };
    let port_sequence = tmp.join("false-then-true-port-observation");
    fs::create_dir_all(&port_sequence).unwrap();
    let mut concurrent_receipt = valid_receipt.clone();
    concurrent_receipt["launch_id"] =
        serde_json::Value::from("concurrent-managed-launch-receipt-0001");
    let concurrent_receipt_bytes = serde_json::to_vec(&concurrent_receipt).unwrap();
    assert_ne!(
        concurrent_receipt_bytes, valid_receipt_bytes,
        "false-to-true oracle replacement receipt must have a distinct identity"
    );
    let concurrent_receipt_source = config_dir.join(".concurrent-managed-launch-receipt.json");
    fs::write(&concurrent_receipt_source, &concurrent_receipt_bytes).unwrap();
    fs::set_permissions(
        &concurrent_receipt_source,
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    env_guard.set(
        "CSSWITCH_TEST_PORT_OBSERVATION_TARGET",
        sandbox_port.to_string(),
    );
    env_guard.set(
        "CSSWITCH_TEST_PORT_OBSERVATION_SEQUENCE_DIR",
        &port_sequence,
    );
    env_guard.set(
        "CSSWITCH_TEST_PORT_OBSERVATION_REPLACEMENT_RECEIPT",
        &concurrent_receipt_source,
    );
    env_guard.set(
        "CSSWITCH_TEST_PORT_OBSERVATION_RECEIPT",
        &managed_launch_path,
    );
    let false_then_true_stop = {
        let mut st = lock(&state);
        super::stop_sandbox_state(&handle, &mut st)
    };
    let (
        false_then_true_confirmed_stopped,
        false_then_true_runtime,
        false_then_true_receipt,
        false_then_true_listener,
        false_then_true_process_start,
    ) = {
        let st = lock(&state);
        (
            st.science_confirmed_stopped.clone(),
            st.science_runtime.clone(),
            fs::read(&managed_launch_path).ok(),
            listener_pid_if_unique(sandbox_port),
            process_start_identity_if_alive(actual_listener_pid),
        )
    };
    env::remove_var("CSSWITCH_TEST_PORT_OBSERVATION_TARGET");
    env::remove_var("CSSWITCH_TEST_PORT_OBSERVATION_SEQUENCE_DIR");
    env::remove_var("CSSWITCH_TEST_PORT_OBSERVATION_REPLACEMENT_RECEIPT");
    env::remove_var("CSSWITCH_TEST_PORT_OBSERVATION_RECEIPT");
    let complete_port_sequence = port_sequence.join("observed-false").is_file()
        && port_sequence.join("receipt-replaced").is_file()
        && port_sequence.join("observed-true").is_file()
        && !port_sequence.join("receipt-replacement-failed").is_file();
    {
        let mut st = lock(&state);
        st.science_confirmed_stopped = None;
        st.science_runtime = Some(runtime_before_stop_oracles.clone());
        st.sandbox_url = url_before_stop_oracles.clone();
    }

    let sandbox_data_dir = fake_state_dir
        .parent()
        .expect("fake Science state must have an isolated data-dir parent")
        .to_path_buf();
    let moved_data_dir = sandbox_data_dir.with_extension("missing-for-stop-oracle");
    fs::rename(&sandbox_data_dir, &moved_data_dir)
        .expect("isolated data-dir must be movable for the missing-dir stop oracle");
    let missing_data_dir_stop = {
        let mut st = lock(&state);
        super::stop_sandbox_state(&handle, &mut st)
    };
    let (missing_data_dir_confirmed_stopped, missing_data_dir_runtime) = {
        let st = lock(&state);
        (
            st.science_confirmed_stopped.clone(),
            st.science_runtime.clone(),
        )
    };
    let missing_data_dir_receipt = fs::read(&managed_launch_path).ok();
    let missing_data_dir_listener = listener_pid_if_unique(sandbox_port);
    let missing_data_dir_process_start = process_start_identity_if_alive(actual_listener_pid);
    fs::rename(&moved_data_dir, &sandbox_data_dir)
        .expect("isolated data-dir must be restored for bounded cleanup");
    {
        let mut st = lock(&state);
        st.science_confirmed_stopped = None;
        st.science_runtime = Some(runtime_before_stop_oracles.clone());
        st.sandbox_url = url_before_stop_oracles;
    }
    cleanup.finish().expect(
        "all isolated Science and Gateway fixtures must cleanly stop before oracle assertions",
    );

    assert!(
            complete_port_sequence,
            "false-to-true stop oracle must inject observed-false, replace the managed receipt, and then inject observed-true"
        );
    assert_eq!(
            false_then_true_receipt.as_deref(),
            Some(concurrent_receipt_bytes.as_slice()),
            "false-to-true stop oracle must observe the concurrently committed receipt before evaluating stop success"
        );
    if env::var("CSSWITCH_TEST_STOP_ORACLE_FIRST").ok().as_deref() == Some("missing-data-dir") {
        assert!(
                missing_data_dir_stop.is_err(),
                "stop must fail closed when the data-dir disappears while the exact managed listener remains"
            );
    }
    assert!(
            false_then_true_stop.is_err(),
            "stop must fail closed when configured-port observations change from false to true after the stop CLI"
        );
    assert!(
        false_then_true_confirmed_stopped.is_none()
            && false_then_true_runtime.as_ref() == Some(&runtime_before_stop_oracles),
        "false-to-true port observations must not publish a confirmed-stopped transition"
    );
    assert_eq!(
        false_then_true_listener,
        Some(actual_listener_pid),
        "false-to-true port observations must not signal or replace the managed listener"
    );
    assert_eq!(
        false_then_true_process_start,
        Some(actual_process_start.clone()),
        "false-to-true port observations must preserve the exact managed process identity"
    );

    assert!(
            missing_data_dir_stop.is_err(),
            "stop must fail closed when the data-dir disappears while the exact managed listener remains"
        );
    assert!(
            missing_data_dir_confirmed_stopped.is_none()
                && missing_data_dir_runtime.as_ref() == Some(&runtime_before_stop_oracles),
            "a missing data-dir with a live managed listener must not publish a confirmed-stopped transition"
        );
    assert_eq!(
        missing_data_dir_receipt.as_deref(),
        Some(concurrent_receipt_bytes.as_slice()),
        "a missing data-dir with a live managed listener must preserve the managed receipt"
    );
    assert_eq!(
        missing_data_dir_listener,
        Some(actual_listener_pid),
        "a missing data-dir stop must leave the exact managed listener untouched"
    );
    assert_eq!(
        missing_data_dir_process_start,
        Some(actual_process_start.clone()),
        "a missing data-dir stop must preserve the exact managed process identity"
    );

    assert!(
        drift_stop.is_err(),
        "stop must fail closed when process-start changes after the stop CLI"
    );
    assert_eq!(
        listener_after_drift_stop,
        Some(actual_listener_pid),
        "process-start drift must not signal or replace the listener"
    );
    assert_eq!(
        process_start_after_drift_stop,
        Some(actual_process_start),
        "process-start drift must leave the original listener alive"
    );
    assert!(
        receipt_preserved_after_drift,
        "failed stop must preserve the managed launch receipt for safe retry"
    );
}

#[test]
#[ignore = "explicit Acceptance-boundary recovery proof; uses fake Science and local loopback ports"]
fn isolated_manual_actions_recover_dead_proxy_with_fake_science() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let tmp = tmpdir("isolated-recovery-proof");
    let home = tmp.join("home");
    let bin_dir = tmp.join("bin");
    fs::create_dir_all(&home).unwrap();
    let fake_science = write_test_bins(&bin_dir).canonicalize().unwrap();
    let open_log = tmp.join("open.log");
    let mock_upstream = start_mock_upstream();
    let mock_upstream_port = mock_upstream.port;
    let proxy_port = free_port();
    let sandbox_port = free_port();
    assert_ne!(proxy_port, sandbox_port);

    let mut env_guard = EnvGuard::new();
    env_guard.set("HOME", &home);
    env_guard.set("CSSWITCH_REPO", &root);
    env_guard.set("SCIENCE_BIN", &fake_science);
    env_guard.set("CSSWITCH_TEST_OPEN_BIN", bin_dir.join("open"));
    env_guard.set("CSSWITCH_TEST_FAKE_SCIENCE_IDENTITY", "1");
    env_guard.set("CSSWITCH_FAKE_OPEN_LOG", &open_log);
    env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
    env_guard.set(
        "PATH",
        format!(
            "{}:/usr/bin:/bin:/usr/sbin:/sbin",
            bin_dir.to_string_lossy()
        ),
    );

    let fake_key = "csswitch-isolated-fake-key-never-log";
    let profile = Profile {
        id: "mock-relay".into(),
        name: "Mock Relay".into(),
        template_id: "custom".into(),
        category: "custom".into(),
        api_format: "anthropic".into(),
        base_url: format!("http://127.0.0.1:{mock_upstream_port}/anthropic"),
        api_key: fake_key.into(),
        model: "mock-model".into(),
        model_catalog: vec![crate::model_catalog::ModelRoute {
            selector_id: "claude-csswitch-relay-mock-model-0123456789ab".into(),
            display_name: "Mock model".into(),
            upstream_model: "mock-model".into(),
            supports_tools: Some(true),
            ..Default::default()
        }],
        default_model_route_id: "claude-csswitch-relay-mock-model-0123456789ab".into(),
        role_bindings: crate::model_catalog::RoleBindings {
            sonnet: "claude-csswitch-relay-mock-model-0123456789ab".into(),
            opus: "claude-csswitch-relay-mock-model-0123456789ab".into(),
            haiku: "claude-csswitch-relay-mock-model-0123456789ab".into(),
            fable: "claude-csswitch-relay-mock-model-0123456789ab".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let cfg = Config {
        profiles: vec![profile],
        active_id: "mock-relay".into(),
        proxy_port,
        sandbox_port,
        ..Default::default()
    };
    let config_dir = config::default_dir();
    config::save_to(&config_dir, &cfg).unwrap();

    let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
    let lifecycle = Arc::new(lifecycle::Lifecycle::new());
    let app = tauri::test::mock_builder()
        .manage(state.clone())
        .manage(lifecycle.clone())
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    let handle = app.handle().clone();
    let cleanup = RuntimeSmokeCleanup::new(
        handle.clone(),
        state.clone(),
        tmp.clone(),
        sandbox_port,
        proxy_port,
    );

    let first = sandbox_session::one_click_login(
        handle.clone(),
        state.clone(),
        lifecycle.as_ref(),
        None,
        None,
    )
    .expect("first one-click should start proxy and sandbox");
    assert_eq!(first["action"], "started");
    assert!(
        first.get("url").is_none(),
        "one-time URL must stay backend-only"
    );
    wait_http_health(proxy_port);
    wait_http_health(sandbox_port);
    let fake_state_dir = home
        .join(config::CONFIG_DIR_NAME)
        .join("sandbox")
        .join("home")
        .join(".claude-science")
        .join("fake-science");
    let first_pid = fs::read_to_string(fake_state_dir.join("pid")).unwrap();

    kill_tracked_proxy(&state, proxy_port);

    let down_status = super::status(app.state::<SharedAppState>());
    assert_eq!(down_status["proxy"], "amber");
    assert_eq!(down_status["sandbox"], "green");
    assert_eq!(down_status["last_error"]["type"], "proxy_unhealthy");
    assert_eq!(
        down_status["last_error"]["message"],
        "代理进程不可达或已退出，请点击「一键开始」恢复。"
    );
    assert_eq!(down_status["last_error"]["port"], proxy_port);

    let start_proxy_recovered = start_internal_active_gateway(&handle, &state, lifecycle.as_ref())
        .expect("internal Gateway core should manually recover a dead proxy");
    assert_eq!(start_proxy_recovered.port, proxy_port);
    wait_http_health(proxy_port);

    let start_proxy_status = super::status(app.state::<SharedAppState>());
    assert_eq!(start_proxy_status["proxy"], "green");
    assert_eq!(start_proxy_status["sandbox"], "green");
    assert_eq!(start_proxy_status["upstream"], "green");
    assert!(start_proxy_status["last_error"].is_null());

    kill_tracked_proxy(&state, proxy_port);
    let down_again_status = super::status(app.state::<SharedAppState>());
    assert_eq!(down_again_status["proxy"], "amber");
    assert_eq!(down_again_status["sandbox"], "green");
    assert_eq!(down_again_status["last_error"]["type"], "proxy_unhealthy");

    let recovered = sandbox_session::one_click_login(
        handle.clone(),
        state.clone(),
        lifecycle.as_ref(),
        None,
        None,
    )
    .expect("one-click should manually recover a dead proxy");
    assert_eq!(recovered["action"], "reopened");
    assert_eq!(recovered["stage"], "complete");
    assert_eq!(recovered["status"], "ok");
    assert_eq!(recovered["recovery_status"], "not_needed");
    assert_eq!(recovered["external_skill_installer"]["status"], "WARNING");
    assert!(
        recovered.get("url").is_none(),
        "one-time URL must stay backend-only"
    );
    wait_http_health(proxy_port);
    assert_eq!(
        fs::read_to_string(fake_state_dir.join("pid")).unwrap(),
        first_pid
    );
    assert_eq!(
        fs::read_to_string(fake_state_dir.join("serve-count")).unwrap(),
        "1"
    );

    let recovered_status = super::status(app.state::<SharedAppState>());
    assert_eq!(recovered_status["proxy"], "green");
    assert_eq!(recovered_status["sandbox"], "green");
    assert_eq!(recovered_status["upstream"], "green");
    assert!(recovered_status["last_error"].is_null());

    let cfg_after = config::load_from(&config_dir).unwrap();
    let secret = cfg_after.secret;
    assert!(!secret.is_empty());
    assert!(!down_status.to_string().contains(fake_key));
    assert!(!down_status.to_string().contains(&secret));
    assert!(!recovered.to_string().contains(fake_key));
    assert!(!recovered.to_string().contains(&secret));
    assert!(!recovered_status.to_string().contains(fake_key));
    assert!(!recovered_status.to_string().contains(&secret));
    for name in ["proxy.log", "sandbox.log", "operation.log"] {
        let body = fs::read_to_string(config_dir.join("logs").join(name))
            .unwrap_or_else(|e| panic!("expected {name} to exist: {e}"));
        assert!(!body.contains(fake_key), "{name} leaked fake key");
        assert!(!body.contains(&secret), "{name} leaked path secret");
    }

    cleanup
        .finish()
        .expect("isolated recovery fixtures must cleanly stop");
}

#[test]
#[ignore = "explicit Acceptance-boundary serialized non-Codex credential recheck; temp HOME, fake auth, fake Science, and loopback only"]
fn isolated_real_ipc_rechecks_non_codex_credential_after_serializer_wait() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let tmp = tmpdir("non-codex-credential-serialized-recheck");
    let home = tmp.join("home");
    let bin_dir = tmp.join("bin");
    fs::create_dir_all(home.join(".ssh")).unwrap();
    fs::write(home.join(".ssh/config"), "Host isolated-test-host\n").unwrap();
    fs::set_permissions(home.join(".ssh/config"), fs::Permissions::from_mode(0o600)).unwrap();
    let fake_science = write_test_bins(&bin_dir).canonicalize().unwrap();
    let mock_upstream = start_mock_upstream();
    let (proxy_port, sandbox_port) = ssh_fixture_ports();
    let auth_log = tmp.join("codex-auth-call.log");
    let publish_log = tmp.join("gateway-publish.log");
    let science_call_log = tmp.join("science-call.log");
    let open_log = tmp.join("open.log");
    fs::write(&publish_log, b"").unwrap();

    let mut env_guard = EnvGuard::new();
    env_guard.set("HOME", &home);
    env_guard.set("CSSWITCH_REPO", &root);
    env_guard.set("SCIENCE_BIN", &fake_science);
    env_guard.set("CSSWITCH_TEST_OPEN_BIN", bin_dir.join("open"));
    env_guard.set("CSSWITCH_FAKE_OPEN_LOG", &open_log);
    env_guard.set("CSSWITCH_FAKE_SCIENCE_CALL_LOG", &science_call_log);
    env_guard.set("CSSWITCH_TEST_FAKE_SCIENCE_IDENTITY", "1");
    env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
    env_guard.set("CSSWITCH_TEST_GATEWAY_PUBLISH_LOG", &publish_log);
    env_guard.set(
        "PATH",
        format!(
            "{}:/usr/bin:/bin:/usr/sbin:/sbin",
            bin_dir.to_string_lossy()
        ),
    );

    let config_dir = config::default_dir();
    let mut cfg = ssh_fixture_config(mock_upstream.port, proxy_port, sandbox_port);
    let original_api_canary = format!("serialized-candidate-original-{}", config::new_id());
    let drifted_api_canary = format!("serialized-candidate-drifted-{}", config::new_id());
    cfg.profile_by_id_mut("ssh-transaction").unwrap().api_key = original_api_canary.clone();
    cfg.secret = config::new_id();
    cfg.experimental_codex_enabled = true;
    cfg.runtime_transaction = Some(
        config::RuntimeTransactionJournal {
            transaction_id: "serialized-prior-journal".into(),
            target_profile_id: "ssh-transaction".into(),
            stage: "start_gateway".into(),
            previous_binding: cfg.runtime_binding.clone(),
            previous_gateway: None,
        }
        .into(),
    );
    cfg.profiles.push(Profile {
        id: "prior-codex-credential-recheck".into(),
        name: "Prior Codex credential recheck".into(),
        template_id: "codex".into(),
        category: "experimental".into(),
        api_format: "openai_responses".into(),
        credential_source: crate::provider_contracts::CredentialSource::CsswitchOauth,
        credential_ref: Some("csswitch:codex:default".into()),
        model_policy: crate::provider_contracts::ModelPolicy::DynamicCatalog,
        ..Default::default()
    });
    cfg.active_id = "prior-codex-credential-recheck".into();
    config::save_to(&config_dir, &cfg).unwrap();

    let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
    let lifecycle = Arc::new(lifecycle::Lifecycle::new());
    let supervisor = Arc::new(crate::codex_auth_supervisor::CodexAuthSupervisor::default());
    let app = tauri::test::mock_builder()
        .manage(state.clone())
        .manage(lifecycle.clone())
        .manage(supervisor)
        .invoke_handler(tauri::generate_handler![
            super::one_click_login,
            super::status,
            super::boot_snapshot
        ])
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    let handle = app.handle().clone();
    let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .unwrap();
    let event_sinks = Arc::new(Mutex::new(Vec::<String>::new()));
    for event_name in ["codex-auth://operation", "boot://publication"] {
        let observed = event_sinks.clone();
        handle.listen_any(event_name, move |event| {
            observed
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(event.payload().to_string());
        });
    }
    let real_gateway = proxy_lifecycle::gateway_bin_path(&handle).unwrap();
    let gateway_wrapper = bin_dir.join("csswitch-gateway-credential-recheck-wrapper");
    write_executable(
        &gateway_wrapper,
        &format!(
            r#"#!/bin/sh
printf '%s %s\n' "$1" "$2" >> '{}'
if [ "$1" = "codex-auth" ] && [ "$2" = "status" ]; then
  printf '%s\n' '{{"schema_version":3,"ok":true,"command":"status","status":{{"authenticated":true,"reason":"ready","account_hash":"abababababababababababababababab","expiry_state":"valid","expires_at":2000000000,"auth_epoch":"cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd","auth_generation":1}}}}'
  exit 0
fi
exec '{}' "$@"
"#,
            auth_log.display(),
            real_gateway.display()
        ),
    );
    env_guard.set("CSSWITCH_GATEWAY_BIN", &gateway_wrapper);
    let prior_profile = config::load_from(&config_dir)
        .unwrap()
        .active_profile()
        .unwrap()
        .clone();
    let started = start_internal_gateway(&handle, &state, lifecycle.as_ref(), &prior_profile)
        .expect("internal Gateway core must establish the prior Codex Gateway");
    assert_eq!(started.port, proxy_port);

    let prior_authority = app_authority_projection(&state);
    let prior_pid = lock(&state).proxy.as_ref().map(std::process::Child::id);
    let prior_context = lock(&state)
        .gateway_launch_context
        .clone()
        .expect("prior Codex Gateway must retain its full launch context");
    let prior_health = crate::proc::http_gateway_health(
        prior_authority.proxy_port,
        Some(&prior_authority.secret),
        crate::runtime::operation::LOCAL_HEALTH_TIMEOUT_MS,
    )
    .expect("prior Codex Gateway must expose exact managed health");

    let mut candidate_config = config::load_from(&config_dir).unwrap();
    candidate_config.active_id = "ssh-transaction".into();
    config::save_to(&config_dir, &candidate_config).unwrap();
    let config_before_drift = config::load_from(&config_dir).unwrap();
    fs::write(&auth_log, b"").unwrap();
    fs::write(&publish_log, b"").unwrap();

    let sandbox_home = home
        .join(config::CONFIG_DIR_NAME)
        .join("sandbox")
        .join("home");
    let science_data = sandbox_home.join(".claude-science");
    let private_state = sandbox_home.parent().unwrap().join("state");
    let managed_receipt = config_dir.join("science-managed-launch.v1.json");
    let system_ssh_before = authority_tree(&home.join(".ssh"));
    let science_before = science_authority_projection(&science_data);
    let private_state_before = authority_tree(&private_state);
    let runtime_before = authority_tree(&config_dir.join("runtime"));
    let receipt_before = authority_tree(&managed_receipt);
    let stub_before = authority_tree(&sandbox_home.join(".ssh"));
    let cleanup = RuntimeSmokeCleanup::new(
        handle.clone(),
        state.clone(),
        tmp.clone(),
        sandbox_port,
        proxy_port,
    );

    let worker_webview = webview.clone();
    let auth_log_wait = auth_log.clone();
    let config_dir_drift = config_dir.clone();
    let drifted_api_for_worker = drifted_api_canary.clone();
    let (worker, preflight_seen) = lifecycle.with_serialized(|| {
        let worker = thread::spawn(move || {
            invoke_json(
                &worker_webview,
                "one_click_login",
                serde_json::json!({"runtimeChoice": null}),
            )
        });
        let mut seen = false;
        for _ in 0..200 {
            let count = fs::read_to_string(&auth_log_wait)
                .unwrap_or_default()
                .lines()
                .filter(|line| *line == "codex-auth status")
                .count();
            if count == 1 {
                seen = true;
                break;
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
        let mut drifted = config::load_from(&config_dir_drift).unwrap();
        drifted
            .profile_by_id_mut("ssh-transaction")
            .unwrap()
            .api_key = drifted_api_for_worker;
        config::save_to(&config_dir_drift, &drifted).unwrap();
        (worker, seen)
    });
    let response = worker.join().expect("real IPC worker must join");
    let surface = response
        .as_ref()
        .map(serde_json::Value::to_string)
        .unwrap_or_else(|error| error.clone());
    let rejected_typed_first = response.as_ref().is_ok_and(|value| {
        value["status"] == "error"
            && value["message"]
                .as_str()
                .is_some_and(|message| message.starts_with("config_changed_retry"))
    });
    let preflight_count = fs::read_to_string(&auth_log)
        .unwrap_or_default()
        .lines()
        .filter(|line| *line == "codex-auth status")
        .count();

    let mut expected_drift = config_before_drift.clone();
    expected_drift
        .profile_by_id_mut("ssh-transaction")
        .unwrap()
        .api_key = drifted_api_canary.clone();
    let config_after = config::load_from(&config_dir).unwrap();
    let candidate_api_only_drift = config_after == expected_drift
        && config_after.active_id == config_before_drift.active_id
        && config_after.runtime_transaction == config_before_drift.runtime_transaction;
    let after_authority = app_authority_projection(&state);
    let (after_pid, prior_child_alive) = {
        let mut authority = lock(&state);
        match authority.proxy.as_mut() {
            Some(child) => (Some(child.id()), child.try_wait().unwrap().is_none()),
            None => (None, false),
        }
    };
    let after_context = lock(&state).gateway_launch_context.clone();
    let after_health = crate::proc::http_gateway_health(
        prior_authority.proxy_port,
        Some(&prior_authority.secret),
        crate::runtime::operation::LOCAL_HEALTH_TIMEOUT_MS,
    );
    let exact_prior_gateway = after_authority == prior_authority
        && after_pid == prior_pid
        && prior_child_alive
        && after_context.as_ref() == Some(&prior_context)
        && after_authority.key_fp == prior_authority.key_fp
        && after_health.as_ref() == Some(&prior_health);
    let no_candidate_publish = fs::read_to_string(&publish_log)
        .unwrap_or_default()
        .is_empty();
    let no_authority_mutation = authority_tree(&home.join(".ssh")) == system_ssh_before
        && science_authority_projection(&science_data) == science_before
        && authority_tree(&private_state) == private_state_before
        && authority_tree(&config_dir.join("runtime")) == runtime_before
        && authority_tree(&managed_receipt) == receipt_before
        && authority_tree(&sandbox_home.join(".ssh")) == stub_before
        && TcpStream::connect(("127.0.0.1", sandbox_port)).is_err()
        && !fs::read_to_string(&science_call_log)
            .unwrap_or_default()
            .lines()
            .any(|line| line == "serve");

    let ipc_ui_sinks = ["status", "boot_snapshot"]
        .into_iter()
        .map(|command| {
            invoke_json(&webview, command, serde_json::json!({}))
                .map(|value| value.to_string())
                .unwrap_or_else(|_| "typed IPC error".into())
        })
        .collect::<Vec<_>>();
    let event_payloads = event_sinks
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let log_authority = authority_tree(&config_dir.join("logs"));
    let opener_sink = fs::read_to_string(&open_log).unwrap_or_default();
    let auth_sink = fs::read_to_string(&auth_log).unwrap_or_default();
    let publish_sink = fs::read_to_string(&publish_log).unwrap_or_default();
    let canaries = [
        original_api_canary.as_str(),
        drifted_api_canary.as_str(),
        config_before_drift.secret.as_str(),
        "csswitch:codex:default",
    ];
    let canaries_absent = canaries.iter().all(|canary| {
        !canary.is_empty()
            && !surface.contains(*canary)
            && !opener_sink.contains(*canary)
            && !auth_sink.contains(*canary)
            && !publish_sink.contains(*canary)
            && ipc_ui_sinks.iter().all(|sink| !sink.contains(*canary))
            && event_payloads
                .iter()
                .all(|payload| !payload.contains(*canary))
            && log_authority
                .values()
                .all(|entry| !String::from_utf8_lossy(&entry.bytes).contains(*canary))
    });
    cleanup
        .finish()
        .expect("non-Codex credential recheck fixture must leave no process residue");

    assert!(
            preflight_seen
                && preflight_count == 1
                && rejected_typed_first
                && candidate_api_only_drift
                && no_candidate_publish
                && exact_prior_gateway
                && no_authority_mutation
                && canaries_absent,
            "after exactly one Codex preflight waits on the real serializer, changing only the active non-Codex candidate api_key must return first typed config_changed_retry before any candidate publish or OAuth/Science/journal/stub mutation, preserve the exact prior owned Codex child/full launch context/key fingerprint/health, and keep both credential canaries absent from UI/trace/events/logs: preflight_seen={preflight_seen}, preflight_count={preflight_count}, rejected_typed_first={rejected_typed_first}, candidate_api_only_drift={candidate_api_only_drift}, no_candidate_publish={no_candidate_publish}, exact_prior_gateway={exact_prior_gateway}, no_authority_mutation={no_authority_mutation}, canaries_absent={canaries_absent}"
        );
}

#[test]
#[ignore = "explicit Acceptance-boundary preexisting managed SSH stub rollback; temp HOME, fake OAuth, fake Science, and loopback only"]
fn isolated_late_failure_preserves_preexisting_managed_stub_when_science_stopped() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let tmp = tmpdir("preexisting-managed-stub-late-failure");
    let home = tmp.join("home");
    let bin_dir = tmp.join("bin");
    let system_ssh = home.join(".ssh");
    let system_config = system_ssh.join("config");
    fs::create_dir_all(&system_ssh).unwrap();
    fs::write(&system_config, "Host isolated-test-host\n").unwrap();
    fs::set_permissions(&system_config, fs::Permissions::from_mode(0o600)).unwrap();
    let fake_science = write_test_bins(&bin_dir).canonicalize().unwrap();
    let mock_upstream = start_mock_upstream();
    let (proxy_port, sandbox_port) = ssh_fixture_ports();
    let science_call_log = tmp.join("science-call.log");
    let open_log = tmp.join("open.log");

    let mut env_guard = EnvGuard::new();
    env_guard.set("HOME", &home);
    env_guard.set("CSSWITCH_REPO", &root);
    env_guard.set("SCIENCE_BIN", &fake_science);
    env_guard.set("CSSWITCH_TEST_OPEN_BIN", bin_dir.join("open"));
    env_guard.set("CSSWITCH_FAKE_OPEN_LOG", &open_log);
    env_guard.set("CSSWITCH_FAKE_SCIENCE_CALL_LOG", &science_call_log);
    env_guard.set("CSSWITCH_TEST_FAKE_SCIENCE_IDENTITY", "1");
    env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
    env_guard.set(
        "PATH",
        format!(
            "{}:/usr/bin:/bin:/usr/sbin:/sbin",
            bin_dir.to_string_lossy()
        ),
    );

    let config_dir = config::default_dir();
    let mut cfg = ssh_fixture_config(mock_upstream.port, proxy_port, sandbox_port);
    let api_canary = format!("managed-stub-api-{}", config::new_id());
    cfg.profile_by_id_mut("ssh-transaction").unwrap().api_key = api_canary.clone();
    cfg.secret = config::new_id();
    cfg.runtime_transaction = Some(
        config::RuntimeTransactionJournal {
            transaction_id: "preexisting-stub-prior-journal".into(),
            target_profile_id: "ssh-transaction".into(),
            stage: "start_gateway".into(),
            previous_binding: cfg.runtime_binding.clone(),
            previous_gateway: None,
        }
        .into(),
    );
    config::save_to(&config_dir, &cfg).unwrap();

    let sandbox_home = home
        .join(config::CONFIG_DIR_NAME)
        .join("sandbox")
        .join("home");
    let sandbox_ssh = sandbox_home.join(".ssh");
    let sandbox_stub = sandbox_ssh.join("config");
    let foreign_neighbor = sandbox_ssh.join("known_hosts");
    fs::create_dir_all(&sandbox_ssh).unwrap();
    fs::set_permissions(&sandbox_ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let stub_bytes = format!(
        "# CSSwitch managed system SSH config bridge v2\nHost isolated-test-host\nInclude \"{}\"\n",
        system_config.display()
    )
    .into_bytes();
    fs::write(&sandbox_stub, &stub_bytes).unwrap();
    fs::set_permissions(&sandbox_stub, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&foreign_neighbor, b"foreign-neighbor-must-survive\n").unwrap();
    fs::set_permissions(&foreign_neighbor, fs::Permissions::from_mode(0o600)).unwrap();

    let science_data = sandbox_home.join(".claude-science");
    fs::create_dir_all(&science_data).unwrap();
    crate::oauth_forge::ensure_virtual_login(
        &science_data,
        "virtual@localhost.invalid",
        &sandbox_home,
    )
    .unwrap();
    fs::remove_file(science_data.join("active-org.json")).unwrap();
    fs::write(
        science_data.join("config.toml"),
        "quiet_logs = true\nssh_hosts = [\"prior-user-host\"]\n",
    )
    .unwrap();
    fs::set_permissions(
        science_data.join("config.toml"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let inplace_authority = science_data.join("orgs/csswitch-test/prior-inplace.db");
    fs::create_dir_all(inplace_authority.parent().unwrap()).unwrap();
    fs::write(&inplace_authority, b"prior-inplace-authority\n").unwrap();
    fs::set_permissions(&inplace_authority, fs::Permissions::from_mode(0o600)).unwrap();
    let failure_marker = tmp.join("serve-mutation-reached");
    let failure_control = science_data.join("fake-science/fail-after-inplace-mutation");
    fs::create_dir_all(failure_control.parent().unwrap()).unwrap();
    fs::write(
        &failure_control,
        format!(
            "{}\n{}\n",
            inplace_authority.display(),
            failure_marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&failure_control, fs::Permissions::from_mode(0o600)).unwrap();

    let prior_runtime =
        science::select_science_runtime_cached(None, &science::ScienceVersionCache::default())
            .unwrap();
    let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
    {
        let mut authority = lock(&state);
        authority.sandbox_port = sandbox_port;
        authority.sandbox_url = None;
        authority.science_runtime = None;
        authority.science_confirmed_stopped = Some(prior_runtime.clone());
    }
    let lifecycle = Arc::new(lifecycle::Lifecycle::new());
    let app = tauri::test::mock_builder()
        .manage(state.clone())
        .manage(lifecycle.clone())
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    let handle = app.handle().clone();
    let prior_explicitly_stopped = lock(&state).sandbox.is_none()
        && lock(&state).science_runtime.is_none()
        && lock(&state).science_confirmed_stopped.as_ref() == Some(&prior_runtime)
        && TcpStream::connect(("127.0.0.1", sandbox_port)).is_err();

    let config_before = config::load_from(&config_dir).unwrap();
    let app_before = app_authority_projection(&state);
    let system_ssh_before = authority_tree(&system_ssh);
    let science_before = science_authority_projection(&science_data);
    let private_state = sandbox_home.parent().unwrap().join("state");
    let private_state_before = authority_tree(&private_state);
    let runtime_before = authority_tree(&config_dir.join("runtime"));
    let receipt = config_dir.join("science-managed-launch.v1.json");
    let receipt_before = authority_tree(&receipt);
    let stub_tree_before = authority_tree(&sandbox_ssh);
    let neighbor_before = authority_tree(&foreign_neighbor);
    let cleanup = RuntimeSmokeCleanup::new(
        handle.clone(),
        state.clone(),
        tmp.clone(),
        sandbox_port,
        proxy_port,
    );

    let failed = sandbox_session::one_click_login(
        handle.clone(),
        state.clone(),
        lifecycle.as_ref(),
        None,
        None,
    );
    let failure_surface = failed
        .as_ref()
        .err()
        .map(|error| error.as_ref())
        .unwrap_or_default()
        .to_string();
    let late_edge_reached = failure_marker.is_file()
        && failure_surface.contains("起沙箱脚本失败")
        && fs::read_to_string(&science_call_log)
            .unwrap_or_default()
            .lines()
            .any(|line| line == "serve");
    let stub_after_failure = authority_tree(&sandbox_ssh);
    let neighbor_after_failure = authority_tree(&foreign_neighbor);
    let config_after_failure = config::load_from(&config_dir).unwrap();
    let app_after_failure = app_authority_projection(&state);
    let all_authorities_restored = config_after_failure == config_before
        && config_after_failure.runtime_transaction == config_before.runtime_transaction
        && app_after_failure == app_before
        && authority_tree(&system_ssh) == system_ssh_before
        && science_authority_projection(&science_data) == science_before
        && authority_tree(&private_state) == private_state_before
        && authority_tree(&config_dir.join("runtime")) == runtime_before
        && authority_tree(&receipt) == receipt_before
        && TcpStream::connect(("127.0.0.1", proxy_port)).is_err()
        && TcpStream::connect(("127.0.0.1", sandbox_port)).is_err();
    let exact_stub_preserved = stub_after_failure == stub_tree_before
        && fs::read(&sandbox_stub).ok().as_deref() == Some(stub_bytes.as_slice())
        && fs::symlink_metadata(&sandbox_stub)
            .is_ok_and(|metadata| metadata.permissions().mode() & 0o777 == 0o600)
        && neighbor_after_failure == neighbor_before;

    fs::remove_file(&failure_control).unwrap();
    let retry =
        sandbox_session::one_click_login(handle, state.clone(), lifecycle.as_ref(), None, None);
    let retry_surface = retry
        .as_ref()
        .map(serde_json::Value::to_string)
        .unwrap_or_else(|error| error.to_string());
    let retry_idempotent = retry.as_ref().is_ok_and(|value| {
        value["action"] == "started" && value["stage"] == "complete" && value["status"] == "ok"
    }) && fs::read_to_string(science_data.join("fake-science/serve-count"))
        .ok()
        .as_deref()
        == Some("1")
        && fs::read_to_string(&science_call_log)
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == "serve")
            .count()
            == 2
        && authority_tree(&sandbox_ssh) == stub_tree_before
        && authority_tree(&foreign_neighbor) == neighbor_before;
    let sinks_credential_free = [
        failure_surface.as_str(),
        retry_surface.as_str(),
        fs::read_to_string(&open_log).unwrap_or_default().as_str(),
    ]
    .iter()
    .all(|sink| !sink.contains(&api_canary) && !sink.contains(&config_before.secret));
    cleanup
        .finish()
        .expect("preexisting managed stub fixture must leave no process residue");

    assert!(
            prior_explicitly_stopped
                && late_edge_reached
                && all_authorities_restored
                && exact_stub_preserved
                && retry_idempotent
                && sinks_credential_free,
            "with prior Science explicitly stopped and an exact valid private V2 CSSwitch-managed SSH stub plus a foreign neighbor already present, a post-OAuth real Science late failure must preserve the exact stub bytes/mode/tree and neighbor, restore every profile/OAuth/Gateway/Science/journal authority, and allow one idempotent retry: prior_explicitly_stopped={prior_explicitly_stopped}, late_edge_reached={late_edge_reached}, all_authorities_restored={all_authorities_restored}, exact_stub_preserved={exact_stub_preserved}, retry_idempotent={retry_idempotent}, sinks_credential_free={sinks_credential_free}"
        );
}

#[test]
fn r0_set_mode_config_failure_leaves_runtime_stopped() {
    run_exact_ignored_runtime_characterization(
        "commands::runtime::tests::isolated_r0_d_lifecycle_command_contract",
        &[("CSSWITCH_TEST_R0_D_CASE", "set-mode")],
    );
}

fn assert_p2b_attention_preserves_config_authority(config_dir: &Path, before: &Config, case: &str) {
    let current = config::load_from(config_dir).unwrap();
    assert_eq!(
        current.without_config_mutation_operation_fence(),
        before.without_config_mutation_operation_fence(),
        "{case}"
    );
    assert!(
        current.config_mutation_operation_fence().unwrap().is_some(),
        "{case}"
    );
    assert!(
        config::read_config_mutation_operation_receipt(config_dir)
            .unwrap()
            .is_some(),
        "{case}"
    );
}

#[test]
#[allow(clippy::result_large_err)]
fn set_mode_rejects_config_commit_when_gateway_stop_is_uncertain() {
    let root = tmpdir("set-mode-gateway-stop-uncertain");
    let config_dir = root.join("config");
    fs::create_dir_all(&config_dir).unwrap();
    config::save_to(
        &config_dir,
        &Config {
            mode: "proxy".into(),
            proxy_port: 18000,
            sandbox_port: 18765,
            ..Default::default()
        },
    )
    .unwrap();
    let config_before = fs::read(config_dir.join("config.json")).unwrap();
    let config_authority_before = config::load_from(&config_dir).unwrap();
    let child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let child_pid = child.id();
    let mut authority = AppState::default();
    authority.proxy = Some(child);
    let state: SharedAppState = Arc::new(Mutex::new(authority));
    let lifecycle = Arc::new(lifecycle::Lifecycle::new());
    let app = tauri::test::mock_builder()
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();

    let result = super::lifecycle::set_mode_inner_with(
        app.handle().clone(),
        state.clone(),
        lifecycle,
        "official".into(),
        config_dir.clone(),
        |runtime| Ok(science::ScienceStopRequest::recover(runtime)),
        |_, _| {
            (
                Ok(science::VerifiedScienceStop {
                    runtime: None,
                    ownership_was_proven: false,
                }),
                false,
            )
        },
        |current| current.stop_proxy_with(|_| Err("injected active stop uncertainty".into())),
    );
    assert!(result
        .as_ref()
        .is_err_and(|error| error.contains("未切换到官方模式")));
    let current = config::load_from(&config_dir).unwrap();
    assert_eq!(
        current.without_config_mutation_operation_fence(),
        config_authority_before
    );
    assert!(current.config_mutation_operation_fence().unwrap().is_some());
    assert!(config::read_config_mutation_operation_receipt(&config_dir)
        .unwrap()
        .is_some());
    assert_ne!(
        fs::read(config_dir.join("config.json")).unwrap(),
        config_before
    );
    let cleanup = lock(&state).rejected_gateway_candidates.clone();
    assert_eq!(cleanup.owned_pids(), vec![child_pid]);
    cleanup
        .retry_with(crate::runtime::system::stop_child_confirmed)
        .unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn p2a_codex_disable_fence_blocks_mode_and_settings_before_effects() {
    let root = tmpdir("p2a-codex-disable-entry-fence");
    let config_dir = root.join("config");
    let sandbox_home = root.join("sandbox");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&sandbox_home).unwrap();
    let cfg = Config {
        mode: "proxy".into(),
        experimental_codex_enabled: true,
        proxy_port: 18000,
        sandbox_port: 18765,
        ..Default::default()
    };
    config::save_to(&config_dir, &cfg).unwrap();
    let mut after = cfg.clone();
    after.experimental_codex_enabled = false;
    let fence = config::CodexDisableOperationFence::new(
        "11".repeat(16),
        "22".repeat(32),
        config::codex_disable_config_fingerprint(&cfg).unwrap(),
        config::codex_disable_config_fingerprint(&after).unwrap(),
    );
    let receipt = br#"{"schema_version":1,"test":"p2a-entry-fence"}"#;
    config::begin_codex_disable_operation(&config_dir, &cfg, &fence, receipt).unwrap();
    let fenced_bytes = fs::read(config_dir.join("config.json")).unwrap();
    let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
    let lifecycle = Arc::new(lifecycle::Lifecycle::new());
    let app = tauri::test::mock_builder()
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    let effect_called = Arc::new(AtomicBool::new(false));

    let mode_effect = effect_called.clone();
    let mode_result = super::lifecycle::set_mode_inner_with(
        app.handle().clone(),
        state.clone(),
        lifecycle.clone(),
        "official".into(),
        config_dir.clone(),
        move |runtime| {
            mode_effect.store(true, Ordering::SeqCst);
            Ok(science::ScienceStopRequest::recover(runtime))
        },
        |_, _| unreachable!("mode stop effect must be fenced"),
        |_| unreachable!("mode gateway stop must be fenced"),
    );
    assert!(mode_result
        .as_ref()
        .is_err_and(|error| error.contains("codex_disable_operation_in_progress")));

    let settings_effect = effect_called.clone();
    let settings_result = super::lifecycle::set_settings_inner_with(
        app.handle().clone(),
        state,
        lifecycle,
        super::lifecycle::UiSettings {
            proxy_port: 18001,
            sandbox_port: 18766,
            reuse_system_ssh: false,
        },
        super::lifecycle::SetSettingsPaths {
            config_dir: config_dir.clone(),
            sandbox_home,
        },
        move |runtime| {
            settings_effect.store(true, Ordering::SeqCst);
            Ok(science::ScienceStopRequest::recover(runtime))
        },
        |_, _| unreachable!("settings stop effect must be fenced"),
        |_| unreachable!("settings gateway stop must be fenced"),
    );
    assert!(settings_result
        .as_ref()
        .is_err_and(|error| error.contains("codex_disable_operation_in_progress")));
    assert!(!effect_called.load(Ordering::SeqCst));
    assert_eq!(
        fs::read(config_dir.join("config.json")).unwrap(),
        fenced_bytes
    );

    config::clear_codex_disable_operation_receipt(
        &config_dir,
        receipt,
        &fence,
        config::CodexDisableTerminalConfigImage::Before,
    )
    .unwrap();
    config::clear_codex_disable_operation_fence(
        &config_dir,
        &fence,
        config::CodexDisableTerminalConfigImage::Before,
    )
    .unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn p2b_settings_ssh_cleanup_only_attention_preserves_runtime_truth() {
    assert_eq!(super::lifecycle::settings_runtime_state(false), "preserved");
    assert_eq!(super::lifecycle::settings_runtime_state(true), "stopped");
    let source = include_str!("lifecycle.rs");
    let settings = source
        .split("pub(super) fn set_settings_inner_with")
        .nth(1)
        .and_then(|tail| tail.split("pub(super) async fn stop_all_command").next())
        .expect("set_settings implementation must remain discoverable");
    assert!(
        settings
            .matches("settings_runtime_state(teardown)")
            .count()
            >= 7,
        "every SSH/config checkpoint and terminal projection must derive stopped versus preserved from the actual teardown action"
    );
}

#[test]
#[allow(clippy::result_large_err)]
fn r1_set_mode_wait_releases_read_model_and_stale_result_preserves_replacement() {
    let root = tmpdir("r1-set-mode-owner-cas");
    let prior_binary = root.join("prior-science");
    let replacement_binary = root.join("replacement-science");
    fs::write(&prior_binary, b"#!/bin/sh\nexit 0\n").unwrap();
    fs::write(&replacement_binary, b"#!/bin/sh\nexit 0\n# replacement\n").unwrap();
    fs::set_permissions(&prior_binary, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&replacement_binary, fs::Permissions::from_mode(0o700)).unwrap();
    let prior = science::test_runtime_identity(prior_binary);
    let replacement = science::test_runtime_identity(replacement_binary);

    for (case, replace_identity, bump_generation) in [
        ("generation-only", false, true),
        ("identity-only", true, false),
    ] {
        let config_dir = root.join(format!("config-{case}"));
        fs::create_dir_all(&config_dir).unwrap();
        fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700)).unwrap();
        config::save_to(
            &config_dir,
            &Config {
                mode: "proxy".into(),
                sandbox_port: 18765,
                proxy_port: 18000,
                ..Default::default()
            },
        )
        .unwrap();
        let config_authority_before = config::load_from(&config_dir).unwrap();

        let (state, proxy_pid) = r0_d_proxy_state();
        {
            let mut authority = lock(&state);
            authority.science_runtime = Some(prior.clone());
            authority.sandbox_port = 18765;
            authority.sandbox_url = Some("http://127.0.0.1:18765/prior".into());
        }
        let lifecycle = Arc::new(lifecycle::Lifecycle::new());
        let app = tauri::test::mock_builder()
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        let handle = app.handle().clone();

        let (stop_started_tx, stop_started_rx) = std::sync::mpsc::channel();
        let (release_stop_tx, release_stop_rx) = std::sync::mpsc::channel();
        let worker_state = state.clone();
        let worker_lifecycle = lifecycle.clone();
        let worker_prior = prior.clone();
        let worker = thread::spawn(move || {
            let claim_prior = worker_prior.clone();
            super::lifecycle::set_mode_inner_with(
                handle,
                worker_state,
                worker_lifecycle,
                "official".into(),
                config_dir,
                |runtime| {
                    assert_eq!(runtime, Some(&claim_prior));
                    Ok(science::ScienceStopRequest::recover(runtime))
                },
                move |_, _| {
                    stop_started_tx.send(()).unwrap();
                    release_stop_rx.recv().unwrap();
                    (
                        Ok(science::VerifiedScienceStop {
                            runtime: Some(worker_prior),
                            ownership_was_proven: true,
                        }),
                        true,
                    )
                },
                AppState::stop_proxy,
            )
        });

        stop_started_rx.recv().unwrap();
        {
            let mut read_model = state
                .try_lock()
                .expect("set_mode Science stop wait must not retain the AppState mutex");
            assert_eq!(read_model.science_runtime.as_ref(), Some(&prior), "{case}");
            if replace_identity {
                read_model.science_runtime = Some(replacement.clone());
                read_model.sandbox_url = Some("http://127.0.0.1:18765/replacement".into());
            }
        }
        if bump_generation {
            lifecycle.bump_generation();
        }
        release_stop_tx.send(()).unwrap();

        let switched = worker.join().unwrap();
        assert!(
            switched
                .as_ref()
                .is_err_and(|error| error.contains("process-local owner 已变化")),
            "{case}: {switched:?}"
        );
        let case_config_dir = root.join(format!("config-{case}"));
        assert_p2b_attention_preserves_config_authority(
            &case_config_dir,
            &config_authority_before,
            case,
        );
        let current = lock(&state);
        let expected_runtime = if replace_identity {
            &replacement
        } else {
            &prior
        };
        let expected_url = if replace_identity {
            "http://127.0.0.1:18765/replacement"
        } else {
            "http://127.0.0.1:18765/prior"
        };
        assert_eq!(
            current.science_runtime.as_ref(),
            Some(expected_runtime),
            "{case}"
        );
        assert!(current.science_confirmed_stopped.is_none(), "{case}");
        assert_eq!(current.sandbox_url.as_deref(), Some(expected_url), "{case}");
        assert!(current.proxy.is_some(), "{case}");
        assert!(r0_d_process_is_running(proxy_pid), "{case}");
        drop(current);
        let _ = lock(&state).stop_proxy();
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn r0_set_settings_failure_points_preserve_stop_before_commit() {
    run_exact_ignored_runtime_characterization(
        "commands::runtime::tests::isolated_r0_d_lifecycle_command_contract",
        &[("CSSWITCH_TEST_R0_D_CASE", "set-settings")],
    );
}

#[test]
#[allow(clippy::result_large_err)]
fn set_settings_rejects_config_commit_when_gateway_stop_is_uncertain() {
    let root = tmpdir("set-settings-gateway-stop-uncertain");
    let config_dir = root.join("config");
    let sandbox_home = root.join("sandbox");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&sandbox_home).unwrap();
    config::save_to(
        &config_dir,
        &Config {
            mode: "proxy".into(),
            proxy_port: 18000,
            sandbox_port: 18765,
            ..Default::default()
        },
    )
    .unwrap();
    let config_before = fs::read(config_dir.join("config.json")).unwrap();
    let config_authority_before = config::load_from(&config_dir).unwrap();
    let child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let child_pid = child.id();
    let mut authority = AppState::default();
    authority.proxy = Some(child);
    let state: SharedAppState = Arc::new(Mutex::new(authority));
    let lifecycle = Arc::new(lifecycle::Lifecycle::new());
    let app = tauri::test::mock_builder()
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();

    let result = super::lifecycle::set_settings_inner_with(
        app.handle().clone(),
        state.clone(),
        lifecycle,
        super::lifecycle::UiSettings {
            proxy_port: 18001,
            sandbox_port: 18766,
            reuse_system_ssh: false,
        },
        super::lifecycle::SetSettingsPaths {
            config_dir: config_dir.clone(),
            sandbox_home,
        },
        |runtime| Ok(science::ScienceStopRequest::recover(runtime)),
        |_, _| {
            (
                Ok(science::VerifiedScienceStop {
                    runtime: None,
                    ownership_was_proven: false,
                }),
                false,
            )
        },
        |current| current.stop_proxy_with(|_| Err("injected active stop uncertainty".into())),
    );
    assert!(result
        .as_ref()
        .is_err_and(|error| error.contains("设置未更改")));
    let current = config::load_from(&config_dir).unwrap();
    assert_eq!(
        current.without_config_mutation_operation_fence(),
        config_authority_before
    );
    assert!(current.config_mutation_operation_fence().unwrap().is_some());
    assert!(config::read_config_mutation_operation_receipt(&config_dir)
        .unwrap()
        .is_some());
    assert_ne!(
        fs::read(config_dir.join("config.json")).unwrap(),
        config_before
    );
    let cleanup = lock(&state).rejected_gateway_candidates.clone();
    assert_eq!(cleanup.owned_pids(), vec![child_pid]);
    cleanup
        .retry_with(crate::runtime::system::stop_child_confirmed)
        .unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn r0_set_settings_stub_remove_failure_preserves_stop_before_commit() {
    run_exact_ignored_runtime_characterization(
        "commands::runtime::tests::isolated_r0_d_lifecycle_command_contract",
        &[("CSSWITCH_TEST_R0_D_CASE", "set-settings-stub-remove")],
    );
}

#[test]
#[allow(clippy::result_large_err)]
fn r2_set_settings_wait_releases_read_model_and_stale_result_preserves_replacement() {
    let root = tmpdir("r2-set-settings-owner-cas");
    let prior_binary = root.join("prior-science");
    let replacement_binary = root.join("replacement-science");
    fs::write(&prior_binary, b"#!/bin/sh\nexit 0\n").unwrap();
    fs::write(&replacement_binary, b"#!/bin/sh\nexit 0\n# replacement\n").unwrap();
    fs::set_permissions(&prior_binary, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&replacement_binary, fs::Permissions::from_mode(0o700)).unwrap();
    let prior = science::test_runtime_identity(prior_binary);
    let replacement = science::test_runtime_identity(replacement_binary);

    for (case, replace_identity, bump_generation) in [
        ("generation-only", false, true),
        ("identity-only", true, false),
    ] {
        let config_dir = root.join(format!("config-{case}"));
        let sandbox_home = root.join(format!("sandbox-{case}"));
        fs::create_dir_all(&config_dir).unwrap();
        fs::create_dir_all(&sandbox_home).unwrap();
        fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&sandbox_home, fs::Permissions::from_mode(0o700)).unwrap();
        config::save_to(
            &config_dir,
            &Config {
                mode: "proxy".into(),
                sandbox_port: 18765,
                proxy_port: 18000,
                ..Default::default()
            },
        )
        .unwrap();
        let config_authority_before = config::load_from(&config_dir).unwrap();

        let (state, proxy_pid) = r0_d_proxy_state();
        {
            let mut authority = lock(&state);
            authority.science_runtime = Some(prior.clone());
            authority.sandbox_port = 18765;
            authority.sandbox_url = Some("http://127.0.0.1:18765/prior".into());
        }
        let lifecycle = Arc::new(lifecycle::Lifecycle::new());
        let generation_before = lifecycle.current_generation();
        let app = tauri::test::mock_builder()
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        let handle = app.handle().clone();

        let (stop_started_tx, stop_started_rx) = std::sync::mpsc::channel();
        let (release_stop_tx, release_stop_rx) = std::sync::mpsc::channel();
        let worker_state = state.clone();
        let worker_lifecycle = lifecycle.clone();
        let worker_prior = prior.clone();
        let worker = thread::spawn(move || {
            let claim_prior = worker_prior.clone();
            super::lifecycle::set_settings_inner_with(
                handle,
                worker_state,
                worker_lifecycle,
                super::lifecycle::UiSettings {
                    proxy_port: 18001,
                    sandbox_port: 18766,
                    reuse_system_ssh: false,
                },
                super::lifecycle::SetSettingsPaths {
                    config_dir,
                    sandbox_home,
                },
                |runtime| {
                    assert_eq!(runtime, Some(&claim_prior));
                    Ok(science::ScienceStopRequest::recover(runtime))
                },
                move |_, _| {
                    stop_started_tx.send(()).unwrap();
                    release_stop_rx.recv().unwrap();
                    (
                        Ok(science::VerifiedScienceStop {
                            runtime: Some(worker_prior),
                            ownership_was_proven: true,
                        }),
                        true,
                    )
                },
                AppState::stop_proxy,
            )
        });

        stop_started_rx.recv().unwrap();
        {
            let mut read_model = state
                .try_lock()
                .expect("set_settings Science stop wait must not retain the AppState mutex");
            assert_eq!(read_model.science_runtime.as_ref(), Some(&prior), "{case}");
            if replace_identity {
                read_model.science_runtime = Some(replacement.clone());
                read_model.sandbox_url = Some("http://127.0.0.1:18765/replacement".into());
            }
        }
        if bump_generation {
            lifecycle.bump_generation();
        }
        release_stop_tx.send(()).unwrap();

        let changed = worker.join().unwrap();
        assert!(
            changed
                .as_ref()
                .is_err_and(|error| error.contains("process-local owner 已变化")),
            "{case}: {changed:?}"
        );
        let case_config_dir = root.join(format!("config-{case}"));
        assert_p2b_attention_preserves_config_authority(
            &case_config_dir,
            &config_authority_before,
            case,
        );
        assert_eq!(
            lifecycle.current_generation(),
            generation_before + u64::from(bump_generation),
            "{case}"
        );
        let current = lock(&state);
        let expected_runtime = if replace_identity {
            &replacement
        } else {
            &prior
        };
        let expected_url = if replace_identity {
            "http://127.0.0.1:18765/replacement"
        } else {
            "http://127.0.0.1:18765/prior"
        };
        assert_eq!(
            current.science_runtime.as_ref(),
            Some(expected_runtime),
            "{case}"
        );
        assert!(current.science_confirmed_stopped.is_none(), "{case}");
        assert_eq!(current.sandbox_url.as_deref(), Some(expected_url), "{case}");
        assert!(current.proxy.is_some(), "{case}");
        assert!(r0_d_process_is_running(proxy_pid), "{case}");
        drop(current);
        let _ = lock(&state).stop_proxy();
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn r0_stop_all_stops_gateway_even_when_science_stop_fails() {
    run_exact_ignored_runtime_characterization(
        "commands::runtime::tests::isolated_r0_d_lifecycle_command_contract",
        &[("CSSWITCH_TEST_R0_D_CASE", "stop-all")],
    );
}

#[test]
#[allow(clippy::result_large_err)]
fn s2_stop_all_wait_releases_read_model_and_stale_result_preserves_replacement() {
    let root = tmpdir("s2-stop-all-owner-cas");
    let prior_binary = root.join("prior-science");
    let replacement_binary = root.join("replacement-science");
    fs::write(&prior_binary, b"#!/bin/sh\nexit 0\n").unwrap();
    fs::write(&replacement_binary, b"#!/bin/sh\nexit 0\n# replacement\n").unwrap();
    fs::set_permissions(&prior_binary, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&replacement_binary, fs::Permissions::from_mode(0o700)).unwrap();
    let prior = science::test_runtime_identity(prior_binary);
    let replacement = science::test_runtime_identity(replacement_binary);

    for (case, replace_identity, bump_generation) in [
        ("generation-only", false, true),
        ("identity-only", true, false),
    ] {
        let mut authority = AppState::default();
        authority.science_runtime = Some(prior.clone());
        authority.sandbox_port = 18765;
        authority.sandbox_url = Some("http://127.0.0.1:18765/prior".into());
        let state: SharedAppState = Arc::new(Mutex::new(authority));
        let lifecycle = Arc::new(lifecycle::Lifecycle::new());
        let app = tauri::test::mock_builder()
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        let handle = app.handle().clone();

        let (stop_started_tx, stop_started_rx) = std::sync::mpsc::channel();
        let (release_stop_tx, release_stop_rx) = std::sync::mpsc::channel();
        let worker_state = state.clone();
        let worker_lifecycle = lifecycle.clone();
        let worker_prior = prior.clone();
        let worker = thread::spawn(move || {
            super::lifecycle::stop_all_inner_with(
                handle,
                worker_state,
                worker_lifecycle,
                crate::lifecycle::RuntimeMutationDomain::Destructive,
                |runtime| Ok(science::ScienceStopRequest::recover(runtime)),
                move |_, _| {
                    stop_started_tx.send(()).unwrap();
                    release_stop_rx.recv().unwrap();
                    (
                        Ok(science::VerifiedScienceStop {
                            runtime: Some(worker_prior),
                            ownership_was_proven: true,
                        }),
                        true,
                    )
                },
            )
        });

        stop_started_rx.recv().unwrap();
        {
            let mut read_model = state
                .try_lock()
                .expect("Science stop wait must not retain the AppState mutex");
            assert_eq!(read_model.science_runtime.as_ref(), Some(&prior), "{case}");
            if replace_identity {
                read_model.science_runtime = Some(replacement.clone());
                read_model.sandbox_url = Some("http://127.0.0.1:18765/replacement".into());
            }
        }
        if bump_generation {
            lifecycle.bump_generation();
        }
        release_stop_tx.send(()).unwrap();

        let stopped = worker.join().unwrap();
        assert!(
            stopped
                .as_ref()
                .is_err_and(|error| error.contains("process-local owner 已变化")),
            "{case}: {stopped:?}"
        );
        let current = lock(&state);
        let expected_runtime = if replace_identity {
            &replacement
        } else {
            &prior
        };
        let expected_url = if replace_identity {
            "http://127.0.0.1:18765/replacement"
        } else {
            "http://127.0.0.1:18765/prior"
        };
        assert_eq!(
            current.science_runtime.as_ref(),
            Some(expected_runtime),
            "{case}"
        );
        assert!(current.science_confirmed_stopped.is_none(), "{case}");
        assert_eq!(current.sandbox_url.as_deref(), Some(expected_url), "{case}");
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn r0_quit_command_does_not_exit_after_science_stop_error() {
    run_exact_ignored_runtime_characterization(
        "commands::runtime::tests::isolated_r0_d_lifecycle_command_contract",
        &[("CSSWITCH_TEST_R0_D_CASE", "quit")],
    );
}

fn r0_d_process_is_running(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn r0_d_proxy_state() -> (SharedAppState, u32) {
    let child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .expect("spawn isolated tracked Gateway fixture");
    let pid = child.id();
    let mut app_state = AppState::default();
    app_state.proxy = Some(child);
    (Arc::new(Mutex::new(app_state)), pid)
}

fn r0_d_config(home: &Path, sandbox_port: u16, proxy_port: u16) -> PathBuf {
    env::set_var("HOME", home);
    let config_dir = config::default_dir();
    fs::create_dir_all(&config_dir).unwrap();
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let cfg = Config {
        mode: "proxy".into(),
        sandbox_port,
        proxy_port,
        reuse_system_ssh: false,
        ..Default::default()
    };
    config::save_to(&config_dir, &cfg).unwrap();
    config_dir
}

fn o1_e1_in_progress_compensation() -> config::RuntimeCompensationJournal {
    config::RuntimeCompensationJournal {
        schema_version: config::RUNTIME_COMPENSATION_SCHEMA_VERSION_V1,
        compensation_id: "o1-e1-mutation-guard".into(),
        target_profile_id: "guarded-profile".into(),
        runtime_fingerprint: "a".repeat(64),
        snapshot_ticket: config::RuntimeSnapshotTicket::verified(
            ".one-click-rollback-0123456789abcdef0123456789abcdef".into(),
        )
        .unwrap(),
        state: config::RuntimeCompensationState::InProgress,
        steps: Vec::new(),
        science_adoption_attempt_ids: Vec::new(),
    }
}

#[test]
#[ignore = "source-gate parents execute exact isolated R0-D lifecycle cases with temp HOME, fake processes, and dynamic loopback ports"]
fn isolated_r0_d_lifecycle_command_contract() {
    let requested = env::var("CSSWITCH_TEST_R0_D_CASE").unwrap_or_default();
    assert!(
        matches!(
            requested.as_str(),
            "set-mode" | "set-settings" | "set-settings-stub-remove" | "stop-all" | "quit"
        ),
        "unknown isolated R0-D case"
    );
    let root = tmpdir(&format!("r0-d-{requested}"));
    let mut env_guard = EnvGuard::new();
    env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
    let fake_science = root.join("fake-science-status");
    write_executable(
        &fake_science,
        "#!/bin/sh\nif [ \"${1:-}\" = status ]; then echo '{\"running\":false}'; exit 1; fi\nexit 0\n",
    );
    env_guard.set("SCIENCE_BIN", &fake_science);
    let app = tauri::test::mock_builder()
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    let handle = app.handle().clone();

    if requested == "set-mode" {
        let guard_home = root.join("mode-journal-guard-home");
        fs::create_dir_all(&guard_home).unwrap();
        let config_dir = r0_d_config(&guard_home, free_port(), free_port());
        config::update(&config_dir, |current| {
            current.runtime_compensation = Some(o1_e1_in_progress_compensation())
        })
        .unwrap();
        let before = fs::read(config_dir.join("config.json")).unwrap();
        let (state, proxy_pid) = r0_d_proxy_state();
        let lifecycle = Arc::new(lifecycle::Lifecycle::new());
        let generation = lifecycle.current_generation();
        let failed = super::lifecycle::set_mode_inner(
            handle.clone(),
            state.clone(),
            lifecycle.clone(),
            "official".into(),
        )
        .unwrap_err();
        assert!(
            failed.contains("runtime_transaction_in_progress"),
            "{failed}"
        );
        assert_eq!(fs::read(config_dir.join("config.json")).unwrap(), before);
        assert_eq!(lifecycle.current_generation(), generation);
        assert!(lock(&state).proxy.is_some());
        assert!(r0_d_process_is_running(proxy_pid));
        let _ = lock(&state).stop_proxy();

        let stop_home = root.join("mode-stop-home");
        fs::create_dir_all(&stop_home).unwrap();
        let sandbox_listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let sandbox_port = sandbox_listener.local_addr().unwrap().port();
        assert_ne!(sandbox_port, 8765);
        let config_dir = r0_d_config(&stop_home, sandbox_port, free_port());
        let config_authority_before = config::load_from(&config_dir).unwrap();
        let (state, proxy_pid) = r0_d_proxy_state();
        let lifecycle = Arc::new(lifecycle::Lifecycle::new());
        let generation = lifecycle.current_generation();
        let failed = super::lifecycle::set_mode_inner(
            handle.clone(),
            state.clone(),
            lifecycle.clone(),
            "official".into(),
        )
        .unwrap_err();
        assert!(failed.contains("停止沙箱失败"), "{failed}");
        assert_p2b_attention_preserves_config_authority(
            &config_dir,
            &config_authority_before,
            "set-mode-stop",
        );
        assert_eq!(lifecycle.current_generation(), generation + 1);
        assert!(lock(&state).proxy.is_some());
        assert!(r0_d_process_is_running(proxy_pid));
        let _ = lock(&state).stop_proxy();

        let commit_home = root.join("mode-commit-home");
        fs::create_dir_all(&commit_home).unwrap();
        let config_dir = r0_d_config(&commit_home, free_port(), free_port());
        let config_authority_before = config::load_from(&config_dir).unwrap();
        let (state, proxy_pid) = r0_d_proxy_state();
        let lifecycle = Arc::new(lifecycle::Lifecycle::new());
        let fault = config::test_arm_update_commit_failure(config_dir.clone());
        let failed = super::lifecycle::set_mode_inner(
            handle.clone(),
            state.clone(),
            lifecycle,
            "official".into(),
        )
        .unwrap_err();
        drop(fault);
        assert!(failed.contains("test-only config update commit failure"));
        assert_p2b_attention_preserves_config_authority(
            &config_dir,
            &config_authority_before,
            "set-mode-commit",
        );
        assert!(lock(&state).proxy.is_none());
        assert!(!r0_d_process_is_running(proxy_pid));
    }

    if requested == "set-settings" {
        let guard_home = root.join("settings-journal-guard-home");
        fs::create_dir_all(&guard_home).unwrap();
        let config_dir = r0_d_config(&guard_home, free_port(), free_port());
        config::update(&config_dir, |current| {
            current.runtime_compensation = Some(o1_e1_in_progress_compensation())
        })
        .unwrap();
        let before = fs::read(config_dir.join("config.json")).unwrap();
        let (state, proxy_pid) = r0_d_proxy_state();
        let lifecycle = Arc::new(lifecycle::Lifecycle::new());
        let generation = lifecycle.current_generation();
        let failed = super::lifecycle::set_settings_inner(
            handle.clone(),
            state.clone(),
            lifecycle.clone(),
            super::lifecycle::UiSettings {
                proxy_port: free_port(),
                sandbox_port: free_port(),
                reuse_system_ssh: false,
            },
        )
        .unwrap_err();
        assert!(
            failed.contains("runtime_transaction_in_progress"),
            "{failed}"
        );
        assert_eq!(fs::read(config_dir.join("config.json")).unwrap(), before);
        assert_eq!(lifecycle.current_generation(), generation);
        assert!(lock(&state).proxy.is_some());
        assert!(r0_d_process_is_running(proxy_pid));
        let _ = lock(&state).stop_proxy();

        let stop_home = root.join("settings-stop-home");
        fs::create_dir_all(&stop_home).unwrap();
        let sandbox_listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let sandbox_port = sandbox_listener.local_addr().unwrap().port();
        assert_ne!(sandbox_port, 8765);
        let config_dir = r0_d_config(&stop_home, sandbox_port, free_port());
        let config_authority_before = config::load_from(&config_dir).unwrap();
        let (state, proxy_pid) = r0_d_proxy_state();
        let lifecycle = Arc::new(lifecycle::Lifecycle::new());
        let generation = lifecycle.current_generation();
        let failed = super::lifecycle::set_settings_inner(
            handle.clone(),
            state.clone(),
            lifecycle.clone(),
            super::lifecycle::UiSettings {
                proxy_port: free_port(),
                sandbox_port: free_port(),
                reuse_system_ssh: false,
            },
        )
        .unwrap_err();
        assert!(failed.contains("设置未更改"), "{failed}");
        assert_p2b_attention_preserves_config_authority(
            &config_dir,
            &config_authority_before,
            "set-settings-stop",
        );
        assert_eq!(lifecycle.current_generation(), generation);
        assert!(lock(&state).proxy.is_some());
        assert!(r0_d_process_is_running(proxy_pid));
        let _ = lock(&state).stop_proxy();

        let revoke_home = root.join("settings-revoke-home");
        fs::create_dir_all(&revoke_home).unwrap();
        let config_dir = r0_d_config(&revoke_home, free_port(), free_port());
        let before = fs::read(config_dir.join("config.json")).unwrap();
        let science_data = science::sandbox_home().join(".claude-science");
        fs::create_dir_all(&science_data).unwrap();
        let foreign = root.join("foreign-config.toml");
        fs::write(&foreign, b"quiet_logs = true\n").unwrap();
        symlink(&foreign, science_data.join("config.toml")).unwrap();
        let (state, proxy_pid) = r0_d_proxy_state();
        let lifecycle = Arc::new(lifecycle::Lifecycle::new());
        let generation = lifecycle.current_generation();
        let failed = super::lifecycle::set_settings_inner(
            handle.clone(),
            state.clone(),
            lifecycle.clone(),
            super::lifecycle::UiSettings {
                proxy_port: free_port(),
                sandbox_port: free_port(),
                reuse_system_ssh: false,
            },
        )
        .unwrap_err();
        assert!(failed.contains("符号链接"), "{failed}");
        assert_eq!(fs::read(config_dir.join("config.json")).unwrap(), before);
        assert_eq!(lifecycle.current_generation(), generation);
        assert!(lock(&state).proxy.is_some());
        assert!(r0_d_process_is_running(proxy_pid));
        let _ = lock(&state).stop_proxy();

        let commit_home = root.join("settings-commit-home");
        fs::create_dir_all(&commit_home).unwrap();
        let config_dir = r0_d_config(&commit_home, free_port(), free_port());
        let config_authority_before = config::load_from(&config_dir).unwrap();
        let (state, proxy_pid) = r0_d_proxy_state();
        let lifecycle = Arc::new(lifecycle::Lifecycle::new());
        let fault = config::test_arm_update_commit_failure(config_dir.clone());
        let failed = super::lifecycle::set_settings_inner(
            handle.clone(),
            state.clone(),
            lifecycle,
            super::lifecycle::UiSettings {
                proxy_port: free_port(),
                sandbox_port: free_port(),
                reuse_system_ssh: false,
            },
        )
        .unwrap_err();
        drop(fault);
        assert!(failed.contains("test-only config update commit failure"));
        assert_p2b_attention_preserves_config_authority(
            &config_dir,
            &config_authority_before,
            "set-settings-commit",
        );
        assert!(lock(&state).proxy.is_none());
        assert!(!r0_d_process_is_running(proxy_pid));
    }

    if requested == "set-settings-stub-remove" {
        let home = root.join("settings-stub-remove-home");
        fs::create_dir_all(home.join(".ssh")).unwrap();
        let system_config = home.join(".ssh/config");
        fs::write(&system_config, b"Host isolated-managed-host\n").unwrap();
        fs::set_permissions(&system_config, fs::Permissions::from_mode(0o600)).unwrap();
        let config_dir = r0_d_config(&home, free_port(), free_port());
        let config_authority_before = config::load_from(&config_dir).unwrap();
        let sandbox_home = science::sandbox_home();
        let science_data = sandbox_home.join(".claude-science");
        fs::create_dir_all(&science_data).unwrap();
        let science_config = science_data.join("config.toml");
        let science_config_before = b"quiet_logs = true\n";
        fs::write(&science_config, science_config_before).unwrap();
        fs::set_permissions(&science_config, fs::Permissions::from_mode(0o600)).unwrap();
        crate::runtime::ssh_bridge::prepare_science_ssh_bridge_for(&sandbox_home, &home).unwrap();
        let bridge_state = science_data.join("csswitch-ssh-bridge.v1.json");
        assert!(bridge_state.is_file());

        let sandbox_ssh = sandbox_home.join(".ssh");
        fs::create_dir_all(&sandbox_ssh).unwrap();
        let sandbox_stub = sandbox_ssh.join("config");
        let stub_bytes = format!(
            "# CSSwitch managed system SSH config bridge v1\nInclude \"{}\"\n",
            system_config.display()
        )
        .into_bytes();
        fs::write(&sandbox_stub, &stub_bytes).unwrap();
        fs::set_permissions(&sandbox_stub, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&sandbox_ssh, fs::Permissions::from_mode(0o500)).unwrap();

        let (state, proxy_pid) = r0_d_proxy_state();
        let lifecycle = Arc::new(lifecycle::Lifecycle::new());
        let generation = lifecycle.current_generation();
        let failed = super::lifecycle::set_settings_inner(
            handle.clone(),
            state.clone(),
            lifecycle.clone(),
            super::lifecycle::UiSettings {
                proxy_port: free_port(),
                sandbox_port: free_port(),
                reuse_system_ssh: false,
            },
        )
        .unwrap_err();
        fs::set_permissions(&sandbox_ssh, fs::Permissions::from_mode(0o700)).unwrap();

        assert!(failed.contains("撤销隔离 SSH config 失败"), "{failed}");
        assert_p2b_attention_preserves_config_authority(
            &config_dir,
            &config_authority_before,
            "set-settings-stub-remove",
        );
        assert_eq!(lifecycle.current_generation(), generation + 1);
        assert!(lock(&state).proxy.is_none());
        assert!(!r0_d_process_is_running(proxy_pid));
        assert!(
            !bridge_state.exists(),
            "SSH bridge revoke must complete first"
        );
        assert_eq!(fs::read(&science_config).unwrap(), science_config_before);
        assert_eq!(fs::read(&sandbox_stub).unwrap(), stub_bytes);
    }

    if requested == "stop-all" || requested == "quit" {
        let home = root.join("terminal-home");
        fs::create_dir_all(&home).unwrap();
        let sandbox_listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let sandbox_port = sandbox_listener.local_addr().unwrap().port();
        assert_ne!(sandbox_port, 8765);
        r0_d_config(&home, sandbox_port, free_port());
        let (state, proxy_pid) = r0_d_proxy_state();
        let lifecycle = Arc::new(lifecycle::Lifecycle::new());
        let generation = lifecycle.current_generation();
        let stopped =
            super::lifecycle::stop_all_inner_cmd(handle, state.clone(), lifecycle.clone());
        assert!(
            stopped
                .as_ref()
                .is_err_and(|error| error.contains("代理已停")),
            "{stopped:?}"
        );
        assert_eq!(lifecycle.current_generation(), generation + 1);
        assert!(lock(&state).proxy.is_none());
        assert!(!r0_d_process_is_running(proxy_pid));
        if requested == "quit" {
            let exited = AtomicBool::new(false);
            let result = super::lifecycle::exit_after_stop_success(stopped, || {
                exited.store(true, Ordering::SeqCst)
            });
            assert!(result.is_err());
            assert!(!exited.load(Ordering::SeqCst));
        }
    }

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn p2b_boot_dual_receipt_conflict_fails_closed() {
    let dir = tmpdir("p2b-boot-dual-receipt");
    let mut cfg = Config {
        experimental_codex_enabled: true,
        ..Default::default()
    };
    let before = config::codex_disable_config_fingerprint(&cfg).unwrap();
    let mut after = cfg.clone();
    after.experimental_codex_enabled = false;
    let after_fingerprint = config::codex_disable_config_fingerprint(&after).unwrap();
    cfg.extra.insert(
        "codex_disable_operation".into(),
        serde_json::json!({
            "schema_version": 1,
            "operation_id": "11".repeat(16),
            "intent_digest": "22".repeat(32),
            "before_config_fingerprint": before,
            "after_config_fingerprint": after_fingerprint,
        }),
    );
    config::save_to(&dir, &cfg).unwrap();
    fs::write(dir.join("codex-disable-operation.v1.json"), b"p2a").unwrap();
    fs::write(
        dir.join(config::CONFIG_MUTATION_OPERATION_RECEIPT_FILE),
        b"p2b",
    )
    .unwrap();
    let attention = super::config_mutation::boot_recover(&dir).unwrap();
    assert_eq!(attention.unwrap()["cause"], "p2a_p2b_mutual_exclusion");
    let _ = fs::remove_dir_all(&dir);
}

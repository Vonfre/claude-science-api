#!/usr/bin/env bash
# Verifies launch-virtual-sandbox.sh starts Science from env -i allowlist.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SCRIPT="$ROOT/scripts/launch-virtual-sandbox.sh"
TMP="$(mktemp -d /private/tmp/csswitch-science-env-allowlist.XXXXXX)"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

STUB_BIN="$TMP/fake-science"
cat > "$STUB_BIN" <<'STUB'
#!/bin/zsh
set -euo pipefail
if [[ "${1:-}" == "serve" ]]; then
  /usr/bin/env | /usr/bin/sort > "${HOME:?}/science-env-dump.txt"
  printf '%s\n' "$@" > "${HOME:?}/science-args.txt"
  exit 0
fi
if [[ "${1:-}" == "--version" ]]; then
  echo "fake-science 0.0.0-test"
  exit 0
fi
exit 2
STUB
chmod 755 "$STUB_BIN"

SANDBOX_HOME="$TMP/sandbox-home"
HOST_HOME="$TMP/host-home"
mkdir -p "$SANDBOX_HOME/.claude-science" "$SANDBOX_HOME/Library/Keychains" "$HOST_HOME"
ENV_OUT="$SANDBOX_HOME/science-env-dump.txt"

mkdir -p "$TMP/bin"
printf '#!/bin/sh\nexit 44\n' > "$TMP/bin/security"
chmod 700 "$TMP/bin/security"
export HOME="$HOST_HOME"

export CSSWITCH_TEST_SENTINEL_SECRET="parent-sentinel-must-not-leak"
export OPENAI_API_KEY="sk-parent-must-not-leak"
export AWS_SECRET_ACCESS_KEY="aws-parent-must-not-leak"
export SSH_AUTH_SOCK="/private/tmp/fake-ssh-agent.sock"
export DEEPSEEK_API_KEY="ds-parent-must-not-leak"

PORT=$((41000 + RANDOM % 2000))
OPAQUE="conda=absent;runtime=absent;seed-assets=absent;r-libs=absent;sbx-bind-src=absent"

set +e
out="$(
  CSSWITCH_HOST_HOME="$HOST_HOME" \
  SANDBOX_HOME="$SANDBOX_HOME" \
  SCIENCE_BIN="$STUB_BIN" \
  CSSWITCH_RUNTIME_VERSION_PRECHECKED=1 \
  CSSWITCH_SCIENCE_OPAQUE_BINDINGS="$OPAQUE" \
  CSSWITCH_PROXY_URL="http://127.0.0.1:18991/deadbeefdeadbeefdeadbeefdeadbeef" \
  CSSWITCH_REUSE_SYSTEM_SSH=0 \
  CSSWITCH_SYSTEM_SSH_HOSTS= \
  PATH="$TMP/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
  zsh "$SCRIPT" --port "$PORT" --skip-oauth-forge 2>&1
)"
status=$?
set -e

if [[ ! -f "$ENV_OUT" ]]; then
  echo "FAIL: stub did not write env dump (script status=$status)"
  echo "$out"
  exit 1
fi

for needle in \
  CSSWITCH_TEST_SENTINEL_SECRET \
  parent-sentinel-must-not-leak \
  OPENAI_API_KEY \
  sk-parent-must-not-leak \
  AWS_SECRET_ACCESS_KEY \
  aws-parent-must-not-leak \
  SSH_AUTH_SOCK \
  DEEPSEEK_API_KEY \
  ds-parent-must-not-leak \
  CSSWITCH_PROXY_URL \
  CSSWITCH_HOST_HOME \
  SCIENCE_BIN \
  CSSWITCH_SCIENCE_OPAQUE_BINDINGS
do
  if grep -F -q -- "$needle" "$ENV_OUT"; then
    echo "FAIL: Science child env leaked or retained forbidden '$needle'"
    cat "$ENV_OUT"
    exit 1
  fi
done

grep -F -q "HOME=$SANDBOX_HOME" "$ENV_OUT"
grep -E -q '^ANTHROPIC_BASE_URL=http://127\.0\.0\.1:18991/' "$ENV_OUT"
grep -E -q '^(HTTPS_PROXY|https_proxy)=http://127\.0\.0\.1:18991$' "$ENV_OUT"
grep -E -q '^(NO_PROXY|no_proxy)=.*127\.0\.0\.1' "$ENV_OUT"
grep -E -q '^PATH=/usr/bin:/bin:/usr/sbin:/sbin$' "$ENV_OUT"

echo "PASS: Science serve allowlist blocks ambient secrets and keeps required vars"

# Production mode: host browsing without falling back to host auth/config.
mkdir -p "$SANDBOX_HOME/.csswitch-science-tools"
CONFIG="$SANDBOX_HOME/.claude-science/config.toml"
TOOL="$SANDBOX_HOME/.csswitch-science-tools/security"
printf '[paths]\nauth_dir = "%s"\nconda_home = "%s"\n' "$SANDBOX_HOME/.claude-science" "$SANDBOX_HOME/.claude-science/conda" > "$CONFIG"
python3 - "$ROOT" "$TOOL" <<'PYTOOL'
import pathlib, re, sys
source = (pathlib.Path(sys.argv[1]) / 'desktop/src-tauri/src/runtime/science/home_layout.rs').read_text()
pathlib.Path(sys.argv[2]).write_text(re.search(r'SCIENCE_HOST_SECURITY_WRAPPER: &str = r#"(.*?)"#;', source, re.S).group(1))
PYTOOL
chmod 600 "$CONFIG"
chmod 500 "$TOOL"
CONFIG_HASH="$(shasum -a 256 "$CONFIG" | awk '{print $1}')"
TOOL_HASH="$(shasum -a 256 "$TOOL" | awk '{print $1}')"
host_launch() {
  CSSWITCH_HOST_HOME="$HOST_HOME" \
  SANDBOX_HOME="$SANDBOX_HOME" \
  SCIENCE_BIN="$STUB_BIN" \
  CSSWITCH_RUNTIME_VERSION_PRECHECKED=1 \
  CSSWITCH_SCIENCE_OPAQUE_BINDINGS="$OPAQUE" \
  CSSWITCH_PROXY_URL="http://127.0.0.1:18991/deadbeefdeadbeefdeadbeefdeadbeef" \
  CSSWITCH_REUSE_SYSTEM_SSH=0 \
  CSSWITCH_SYSTEM_SSH_HOSTS= \
  CSSWITCH_SCIENCE_USE_HOST_HOME=1 \
  CSSWITCH_SCIENCE_CONFIG_SHA256="$CONFIG_HASH" \
  CSSWITCH_SCIENCE_SECURITY_SHA256="$TOOL_HASH" \
  PATH="$TMP/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
  zsh "$SCRIPT" --port "$PORT" --skip-oauth-forge > "$TMP/launch.log" 2>&1
}
host_launch
grep -Fxq "HOME=$HOST_HOME" "$HOST_HOME/science-env-dump.txt"
grep -Fxq "PATH=$SANDBOX_HOME/.csswitch-science-tools:/usr/bin:/bin:/usr/sbin:/sbin" "$HOST_HOME/science-env-dump.txt"
python3 - "$HOST_HOME/science-args.txt" "$CONFIG" "$SANDBOX_HOME/.claude-science" <<'PYARGS'
import pathlib, sys
args = pathlib.Path(sys.argv[1]).read_text().splitlines()
assert args[args.index('--config') + 1] == sys.argv[2]
assert args[args.index('--data-dir') + 1] == sys.argv[3]
assert '--dangerously-no-sandbox' not in args
assert '--dangerously-skip-approvals' not in args
PYARGS
! grep -Eq 'CSSWITCH_SCIENCE_|SENTINEL|OPENAI_API_KEY|AWS_SECRET|SSH_AUTH_SOCK' "$HOST_HOME/science-env-dump.txt"
echo "PASS: host HOME browsing has explicit isolated config/data and no ambient secrets"
rm "$HOST_HOME/science-env-dump.txt"
printf '# changed\n' >> "$CONFIG"
if host_launch; then echo "FAIL: drifted config was accepted"; exit 1; fi
[[ ! -e "$HOST_HOME/science-env-dump.txt" ]]
echo "PASS: config drift is rejected before Science starts"
CONFIG_HASH="$(shasum -a 256 "$CONFIG" | awk '{print $1}')"
TOOL_HASH=invalid
if host_launch; then echo "FAIL: drifted private security tool was accepted"; exit 1; fi
[[ ! -e "$HOST_HOME/science-env-dump.txt" ]]
echo "PASS: security tool drift is rejected before Science starts"

# Exercise the exact shim with only its terminal security target replaced by a
# fake executable. A hostile synthetic .zshenv must never run before HOME resets.
python3 - "$TOOL" "$HOST_HOME" "$SANDBOX_HOME" "$TMP" <<'PYSHIM'
import json, os, pathlib, subprocess, sys
source, host, sandbox, root = map(pathlib.Path, sys.argv[1:])
(host / '.zshenv').write_text('print -r -- unexpected-host-startup\nexit 73\n')
fake = root / 'fake-security'
fake.write_text('#!/bin/sh\nprintf "%s\\n" "$HOME" "$@"\n')
fake.chmod(0o700)
probe = source.with_name('security-probe')
text = source.read_text()
assert text.count('exec /usr/bin/security "$@"') == 1
probe.write_text(text.replace('exec /usr/bin/security "$@"', f'exec "{fake}" "$@"'))
probe.chmod(0o500)
args = ['find-generic-password', 'argument with spaces', '*literal*', '']
result = subprocess.run([str(probe), *args], env={'HOME': str(host), 'PATH': '/usr/bin:/bin'}, capture_output=True, text=True)
assert result.returncode == 0, 'host startup interrupted shim'
assert result.stderr == ''
assert result.stdout.splitlines() == [str(sandbox), *args], 'HOME or arguments changed'
print('PASS: security shim skips host startup, restores isolated HOME and preserves arguments')
PYSHIM

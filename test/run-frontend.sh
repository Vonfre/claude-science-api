#!/usr/bin/env bash
# S0 frontend 层：node --check 语法检查（无框架、无构建）。无 node → env-blocked。
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"
if ! command -v node >/dev/null 2>&1; then
  echo "S0_LAYER frontend env-blocked (no node)"; exit 0
fi
fail=0
if ! node --test test/app_update.test.mjs test/directory_access.test.mjs; then fail=1; fi
for f in desktop/src/directory-access.js desktop/src/app-update-controller.js scripts/write-app-update-manifest.mjs test/app_update.test.mjs test/directory_access.test.mjs test/sciport_ui.test.mjs desktop/src/profile-batch.js desktop/src/runtime-error.js test/profile_batch.test.mjs desktop/src/main.js desktop/src/preview-adapter.js desktop/src/ipc-client.js desktop/src/codex-controller.js desktop/src/profile-controller.js desktop/src/runtime-controller.js desktop/src/codex-auth-protocol.js desktop/src/codex-disable-protocol.js desktop/src/runtime-mutation-protocol.js desktop/src/runtime-status-state.js desktop/src/model-catalog-state.js test/codex_auth_ui.test.mjs test/codex_disable_ui.test.mjs test/runtime_mutation_protocol.test.mjs test/runtime_controller_doctor.test.mjs test/runtime_controller_history_restore.test.mjs test/runtime_status_state.test.mjs test/model_catalog_state.test.mjs test/profile_preview_mock.test.mjs desktop/src/skill-page.js desktop/src/skill-page-state.js test/skill_page_state.test.mjs; do
  if node --check "$f"; then echo "ok - node --check $f"; else echo "NOT ok - $f"; fail=1; fi
done
if node --test test/codex_auth_ui.test.mjs; then
  echo "ok - Codex auth UI structured error tests"
else
  echo "NOT ok - Codex auth UI structured error tests"
  fail=1
fi
if node --test test/codex_disable_ui.test.mjs; then
  echo "ok - Codex disable UI structured error tests"
else
  echo "NOT ok - Codex disable UI structured error tests"
  fail=1
fi
if node --test test/runtime_mutation_protocol.test.mjs; then
  echo "ok - runtime mutation structured protocol tests"
else
  echo "NOT ok - runtime mutation structured protocol tests"
  fail=1
fi
if node --test test/skill_page_state.test.mjs; then
  echo "ok - node --test real Skill page state"
else
  echo "NOT ok - real Skill page state tests"; fail=1
fi
if node --test test/runtime_status_state.test.mjs; then
  echo "ok - node --test runtime status state"
else
  echo "NOT ok - runtime status state tests"; fail=1
fi
if node --test test/model_catalog_state.test.mjs; then
  echo "ok - node --test provider model catalog state"
else
  echo "NOT ok - provider model catalog state tests"; fail=1
fi
if node --test test/profile_preview_mock.test.mjs; then
  echo "ok - node --test profile preview selection/applied state"
else
  echo "NOT ok - profile preview selection/applied state tests"; fail=1
fi
if node --test test/runtime_controller_history_restore.test.mjs; then
  echo "ok - node --test runtime controller history restore sequencing"
else
  echo "NOT ok - runtime controller history restore sequencing tests"; fail=1
fi
if node --test test/runtime_controller_doctor.test.mjs; then
  echo "ok - node --test Doctor intent split"
else
  echo "NOT ok - Doctor intent split tests"; fail=1
fi
if node --test test/profile_batch.test.mjs; then
  echo "ok - API-only profile batch management"
else
  echo "NOT ok - API-only profile batch management"; fail=1
fi
if node --test test/sciport_ui.test.mjs; then
  echo "ok - SciPort UI and removed localization boundaries"
else
  echo "NOT ok - SciPort UI and removed localization boundaries"; fail=1
fi
if [ "$fail" -eq 0 ]; then echo "S0_LAYER frontend pass"; exit 0; else echo "S0_LAYER frontend fail"; exit 1; fi

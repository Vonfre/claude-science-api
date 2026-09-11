import pathlib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]


def sandbox_session_source():
    module_dir = ROOT / "desktop/src-tauri/src/runtime/sandbox_session"
    sources = sorted(module_dir.rglob("*.rs"))
    return "\n".join(path.read_text() for path in sources)


class ProfilePinContractTests(unittest.TestCase):
    def test_backend_pin_is_local_and_one_click_remains_the_apply_boundary(self):
        profiles = (ROOT / "desktop/src-tauri/src/commands/profiles.rs").read_text()
        pin = profiles.split("fn pin_active_profile_in_dir(", 1)[1].split(
            "\n#[cfg(test)]", 1
        )[0]

        self.assertIn("config::update_result", pin)
        self.assertIn("cfg.active_id = id.to_string()", pin)
        self.assertIn("config::require_no_runtime_transaction(cfg)?", pin)
        config_source = (ROOT / "desktop/src-tauri/src/config.rs").read_text()
        self.assertIn("code=runtime_transaction_in_progress", config_source)
        self.assertIn("resolve_launch_plan(profile)?", pin)
        self.assertIn('"apply_state".into()', pin)
        self.assertIn('applied_profile_id.as_deref() == Some(id)', pin)
        self.assertIn('serde_json::Value::String(apply_state.into())', pin)
        for forbidden in (
            "prepare_provider_auth",
            "scratch_validate_candidate",
            "set_active_profile_txn",
            "start_proxy",
            "stop_proxy",
            "probe_",
            "runtime_binding =",
            "runtime_transaction =",
            "history_recovery",
            "boot_attention",
        ):
            self.assertNotIn(forbidden, pin)

        one_click = (
            ROOT / "desktop/src-tauri/src/runtime/sandbox_session/one_click/cold.rs"
        ).read_text()
        self.assertRegex(
            one_click,
            r"begin_one_click_finalize\(\s*&dir,\s*&transaction_identity,\s*"
            r"&mut journal_progress,\s*"
            r"config::RuntimeFinalizeAction::CommitBinding\s*\{\s*"
            r"binding:\s*committed,\s*"
            r"science_adoption_attempt_id:\s*Some\(adoption_attempt_id\),\s*\},\s*\)"
            r"[\s\S]*?complete_one_click_finalize\(\s*&dir,\s*&mut journal_progress\s*\)",
        )
        self.assertRegex(
            one_click,
            r"let adoption_attempt_id\s*=\s*match\s*"
            r"committed\.science_adoption_attempt_id\.clone\(\)\s*\{\s*"
            r"Some\(attempt_id\)\s*if\s*launch_runtime\.adoption_attempt_id\(\)\s*"
            r"==\s*Some\(attempt_id\.as_str\(\)\)\s*=>\s*\{\s*attempt_id\s*\}\s*"
            r"_\s*=>\s*\{\s*return\s*Err\(rollback_context\.failure\(\s*"
            r'"Science managed receipt、runtime 与 binding adoption provenance 不一致"\s*'
            r",?\s*\)\);\s*\}\s*\};",
        )
        handler = (ROOT / "desktop/src-tauri/src/lib.rs").read_text().split(
            "tauri::generate_handler![", 1
        )[1].split(
            "])\n", 1
        )[0]
        profile_runtime = (ROOT / "desktop/src-tauri/src/runtime/profile.rs").read_text()
        preview_mock = (ROOT / "desktop/src/preview-adapter.js").read_text()
        for retired_command in (
            "preview_profile_preset_sync",
            "apply_profile_preset_sync",
        ):
            self.assertNotIn(retired_command, handler)
            self.assertNotIn(retired_command, profiles)
            self.assertNotIn(retired_command, profile_runtime)
            self.assertNotIn(retired_command, preview_mock)
        connection = profiles.split("fn update_profile_connection_inner_cmd", 1)[1].split(
            "/// 只把 profile", 1
        )[0]
        connection_flow = profiles.split("fn update_profile_connection_with", 1)[1].split(
            "fn commit_profile_connection_in_dir", 1
        )[0]
        connection_commit = profiles.split("fn commit_profile_connection_in_dir", 1)[
            1
        ].split("/// 只把 profile", 1)[0]
        self.assertNotIn("set_active_profile_txn", connection)
        self.assertNotIn("cfg.active_id == id", connection)
        self.assertLess(
            connection_flow.index("load_without_runtime_transaction(dir)"),
            connection_flow.index("let prepared = prepare("),
        )
        self.assertLess(
            connection_flow.index("let prepared = prepare("),
            connection_flow.index(".with_mutation("),
        )
        self.assertRegex(
            connection_flow,
            r"\.with_mutation\(\s*lifecycle::RuntimeMutationDomain::Intent\s*,",
        )
        self.assertIn("verify(&prepared, dir)?", connection_flow)
        self.assertLess(
            connection_commit.index("load_without_runtime_transaction(dir)"),
            connection_commit.index("let validated = validate(&candidate)?"),
        )
        self.assertIn("config::require_no_runtime_transaction(cfg)?", profiles)

    def test_ui_activation_has_no_skip_path_and_reports_pending_selection(self):
        main = (ROOT / "desktop/src/profile-controller.js").read_text()
        runtime = (ROOT / "desktop/src/runtime-controller.js").read_text()
        js = main + runtime
        activate = main.split("async function activate(id)", 1)[1].split(
            "\n  return {", 1
        )[0]
        boundary = runtime.split("async function checkOneClickBoundary()", 1)[1].split(
            "\nasync function runOneClick", 1
        )[0]

        self.assertIn('call("set_active_profile", { id })', activate)
        self.assertIn("当前选择", activate)
        self.assertIn("待启动 Science核验并应用", activate)
        self.assertNotIn("skipVerify", js)
        self.assertNotIn("can_skip", js)
        self.assertNotIn("pendingSkipActivateId", js)
        codex_controller = (ROOT / "desktop/src/codex-controller.js").read_text()
        codex_snapshot_parser = codex_controller.split(
            "function parseCodexOperationSnapshot(value)", 1
        )[1].split("\nfunction codexOperationActive", 1)[0]
        codex_snapshot_accept = codex_controller.split(
            "function acceptCodexOperationSnapshot(raw, allowReplacement)", 1
        )[1].split("\nasync function registerCodexAuthEvents", 1)[0]
        self.assertIn(
            'value.state !== "starting" && value.config_mutation_operation_id == null',
            codex_snapshot_parser,
        )
        self.assertIn(
            "codexOperationSnapshotTransitionAccepted(codexAuthOperation, next, allowReplacement)",
            codex_snapshot_accept,
        )
        active_intent = js.split("function isExactActiveProfileIntent", 1)[1].split(
            "\nasync function switchMode", 1
        )[0]
        self.assertIn('["committed", "no_change"].includes(outcome.disposition)', active_intent)
        self.assertIn('outcome.selected_profile_id === selectedProfileId', active_intent)
        self.assertIn('outcome.validation === "accepted"', active_intent)
        self.assertIn('typeof outcome.science_running === "boolean"', active_intent)
        self.assertIn('raw.apply_state === expectedApplyState', active_intent)
        self.assertIn('outcome.applied_profile_id === selectedProfileId', active_intent)
        self.assertIn('raw.applied_profile_id === outcome.applied_profile_id', active_intent)
        self.assertIn('isExactActiveProfileIntent(r, intent, id)', activate)
        self.assertNotIn("r.hint", activate)
        self.assertIn("当前选择", boundary)
        self.assertIn('pending ? "待应用 · 启动后生效" : "上次已应用"', js)
        self.assertIn('applied ? "上次应用" : ""', js)
        command = (ROOT / "desktop/src-tauri/src/commands/profiles.rs").read_text().split(
            "pub(crate) async fn set_active_profile(", 1
        )[1].split("\n}", 1)[0]
        self.assertNotIn("skip_verify", command)
        html = (ROOT / "desktop/src/index.html").read_text()
        self.assertNotIn("skipActivateBtn", html)

    def test_mode_switch_accepts_only_exact_typed_terminal_outcomes(self):
        main = (ROOT / "desktop/src/profile-controller.js").read_text()
        exact_intent = main.split("function isExactConfigIntent", 1)[1].split(
            "\nfunction isExactCompletedConfigMutation", 1
        )[0]
        exact_terminal = main.split("function isExactCompletedConfigMutation", 1)[1].split(
            "\nasync function switchMode", 1
        )[0]
        switch_mode = main.split("async function switchMode(m)", 1)[1].split(
            "\nasync function openOfficial", 1
        )[0]
        settings = main.split("async function persistRuntimeSettings()", 1)[1].split(
            "\n// ── 模型候选", 1
        )[0]
        clear_key = main.split("async function doClearKey(id)", 1)[1].split(
            "\n// ── C4", 1
        )[0]
        delete = main.split("async function doDelete(id)", 1)[1].split(
            "\n// 设为当前", 1
        )[0]

        self.assertIn("dispositions.includes(outcome.disposition)", exact_intent)
        self.assertIn('outcome.config_state === "committed"', exact_intent)
        self.assertIn('typeof outcome.intent_id === "string"', exact_intent)
        self.assertIn('outcome.validation === "not_run"', exact_intent)
        self.assertIn("outcome.science_running === false", exact_intent)
        self.assertIn('outcome.disposition === "completed"', exact_terminal)
        self.assertIn('outcome.config_state === "after"', exact_terminal)
        self.assertIn("runtimeStates.includes(outcome.runtime_state)", exact_terminal)
        self.assertIn('outcome.recovery_state === "not_needed"', exact_terminal)
        self.assertIn('typeof outcome.operation_id === "string"', exact_terminal)

        self.assertIn('isExactConfigIntent(outcome, "set_mode", ["committed"])', switch_mode)
        self.assertIn('m === "official"', switch_mode)
        for source, intent, destructive in (
            (settings, "set_settings", "set_settings_destructive"),
            (clear_key, "clear_profile_key", "clear_applied_profile_key"),
            (delete, "delete_profile", "delete_applied_profile"),
        ):
            self.assertIn("isExactConfigIntent(", source)
            self.assertIn(f'"{intent}"', source)
            self.assertIn(f'"{destructive}"', source)
            self.assertIn("isExactCompletedConfigMutation(", source)
        self.assertIn('"set_mode_official"', switch_mode)

    def test_success_keeps_single_finally_busy_ownership_through_refresh(self):
        js = (ROOT / "desktop/src/runtime-controller.js").read_text()
        run_one_click = js.split("async function runOneClick(runtimeChoice)", 1)[1].split(
            "\nasync function importLocalSkill", 1
        )[0]
        success = run_one_click.split("// 透传后端据实回传的 msg", 1)[1].split(
            "} catch (e)", 1
        )[0]

        self.assertNotIn("setBusy(false)", success)
        finally_block = run_one_click.split("} finally {", 1)[1]
        self.assertLess(finally_block.index("setBusy(false)"), finally_block.index("refreshIfLoaded"))
        self.assertIn("await getSkillPage()?.refreshIfLoaded()", run_one_click)

    def test_committed_profile_mutations_distinguish_refresh_failure(self):
        js = (ROOT / "desktop/src/profile-controller.js").read_text()
        helper = js.split("async function loadConfigAfterCommit()", 1)[1].split(
            "// 列表优先展示", 1
        )[0]
        self.assertIn("loadConfig({ throwOnError: true })", helper)
        self.assertIn("error.configCommitted = true", helper)
        self.assertIn("已提交，但界面刷新失败", helper)
        self.assertEqual(js.count("await loadConfigAfterCommit()"), 6)
        for action in (
            "创建配置", "连接配置", "清除 key", "配置元数据", "删除配置", "当前选择",
        ):
            self.assertIn(f'committedRefreshMessage("{action}"', js)

    def test_skill_and_browser_warnings_preserve_runtime_success(self):
        session = sandbox_session_source()
        self.assertIn("configure_third_party_best_effort", session)
        self.assertIn("RegistrationStatus::Warning", session)
        self.assertIn("服务已就绪；自动打开失败。", session)
        self.assertIn('"status": "ok"', session)
        self.assertIn('"fallback_url": fallback_url', session)


if __name__ == "__main__":
    unittest.main()

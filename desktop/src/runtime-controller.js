import { call } from "./ipc-client.js";
import { RUNTIME_STATUS_LABELS, aggregateRuntimeStatus, normalizeRuntimeLight } from "./runtime-status-state.js";

export function createRuntimeController({
  els,
  getConfigState,
  getSkillPage,
  isBusy,
  isActivationInFlight,
  getMode,
  getOfficialRuntimeState,
  setBusy,
  setMsg,
  setBrowserFallback,
  startOneClickFeedback,
  startDoctorFeedback,
  isCodexSource,
  renderList,
  runtimeCommandErrorText,
  syncOpenBrowserControl,
  setLight,
  setStatusText,
  setStatusRecoveryMsg,
  proxyRecoveryMessage,
}) {
  let browserOpenInFlight = false;
  let doctorIntentInFlight = false;
  let runtimeChoiceActiveId = null;
  let runtimePendingSha256 = null;
  let scienceRuntimeUpdateRefreshPending = false;
  let scienceRuntimeUpdateRefreshEpoch = 0;
  let scienceRuntimeUpdateRefreshInFlight = 0;

function hideRuntimeChoice() {
  els.runtimeChoiceSec.hidden = true;
  els.runtimeChoiceText.textContent = "";
  runtimeChoiceActiveId = null;
  runtimePendingSha256 = null;
}

function showRuntimeChoice(preflight) {
  const pending = preflight && preflight.pending_update;
  const canActivateUpdate = !!(pending && pending.sha256 && pending.version);
  els.runtimeActivateUpdateBtn.hidden = !canActivateUpdate;
  els.runtimeKeepActiveBtn.hidden = !canActivateUpdate;
  if (canActivateUpdate) {
    els.runtimeUseCacheBtn.hidden = true;
    els.runtimeDownloadBtn.hidden = true;
    els.runtimeChoiceCancelBtn.hidden = true;
    els.runtimeChoiceText.textContent =
      "后台检测到 Claude Science " + pending.version + "。当前运行不会被打断；你可以固定它供下次冷启动使用，或继续当前 active 版本。";
    els.runtimeChoiceSec.hidden = false;
    runtimePendingSha256 = pending.sha256;
    runtimeChoiceActiveId = getConfigState().active_id || null;
    return;
  }
  const cachedVersion = preflight && preflight.cached_version;
  const canUseCache = preflight && preflight.status === "cached_choice_required" && !!cachedVersion;
  els.runtimeActivateUpdateBtn.hidden = true;
  els.runtimeKeepActiveBtn.hidden = true;
  els.runtimeUseCacheBtn.hidden = !canUseCache;
  els.runtimeDownloadBtn.hidden = false;
  els.runtimeChoiceCancelBtn.hidden = false;
  els.runtimeChoiceText.textContent = canUseCache
    ? "未找到通过安全预检的 Claude Science App。发现可确认版本的历史缓存：" + cachedVersion + "。你可以仅本次使用它，或前往官方页面安装 / 更新 Science。此选择不会保存。"
    : "未找到通过安全预检的 Claude Science App，历史缓存也无法确认版本。请先从官方页面安装 / 更新 Science。";
  els.runtimeChoiceSec.hidden = false;
  runtimeChoiceActiveId = getConfigState().active_id || null;
}

async function refreshScienceRuntimeUpdate(status = null) {
  const refreshEpoch = ++scienceRuntimeUpdateRefreshEpoch;
  if (isBusy()) {
    scienceRuntimeUpdateRefreshPending = true;
    return;
  }
  const readsDurableStatus = !status;
  if (readsDurableStatus) scienceRuntimeUpdateRefreshInFlight += 1;
  try {
    const current = status || await call("science_runtime_update_status");
    if (refreshEpoch !== scienceRuntimeUpdateRefreshEpoch) return;
    if (isBusy()) {
      scienceRuntimeUpdateRefreshPending = true;
      return;
    }
    if (current && current.pending_update) showRuntimeChoice(current);
    else if (runtimePendingSha256) hideRuntimeChoice();
    scienceRuntimeUpdateRefreshPending = false;
  } catch (_) {
    if (refreshEpoch === scienceRuntimeUpdateRefreshEpoch) {
      scienceRuntimeUpdateRefreshPending = true;
    }
  } finally {
    if (readsDurableStatus) scienceRuntimeUpdateRefreshInFlight -= 1;
  }
}

async function applyScienceRuntimeUpdate(action) {
  if (isBusy() || !runtimePendingSha256) return;
  const expectedSha256 = runtimePendingSha256;
  let refreshAfterFailure = false;
  scienceRuntimeUpdateRefreshEpoch += 1;
  setBusy(true, { kind: "scienceRuntimeUpdate" });
  try {
    await call("science_runtime_update_action", { action, expectedSha256 });
    hideRuntimeChoice();
    setMsg(action === "activate_pending"
      ? "已固定新的 Science active runtime；当前健康进程不会重启，下次冷启动生效。"
      : "已继续使用当前 Science active runtime；同一候选不会重复提示。", "ok");
  } catch (e) {
    setMsg("Science runtime 更新选择失败：" + runtimeCommandErrorText(e), "err");
    refreshAfterFailure = true;
  } finally {
    setBusy(false);
  }
  if (refreshAfterFailure) await refreshScienceRuntimeUpdate();
}

function hideHistoryRecovery() {
  els.historyRecoverySec.hidden = true;
  els.historyRecoveryText.textContent = "";
  els.historyRecoveryChoices.replaceChildren();
}

function showHistoryRecovery(result) {
  const choices = Array.isArray(result && result.choices) ? result.choices : [];
  els.historyRecoveryChoices.replaceChildren();
  choices.forEach((choice) => {
    if (!choice || typeof choice.reference !== "string" || !choice.reference) return;
    const label = typeof choice.label === "string" && choice.label
      ? choice.label
      : "历史记录";
    const restoreButton = document.createElement("button");
    restoreButton.type = "button";
    restoreButton.className = "btn";
    restoreButton.dataset.historyReference = choice.reference;
    restoreButton.dataset.historyResume = "false";
    restoreButton.textContent = label;
    els.historyRecoveryChoices.appendChild(restoreButton);
    const resumeButton = document.createElement("button");
    resumeButton.type = "button";
    resumeButton.className = "btn primary";
    resumeButton.dataset.historyReference = choice.reference;
    resumeButton.dataset.historyResume = "true";
    resumeButton.textContent = label + "并启动";
    els.historyRecoveryChoices.appendChild(resumeButton);
  });
  els.historyRecoveryText.textContent =
    "检测到多份 v0.8.0 遗留历史。请选择要恢复的一份；CSSwitch 不会读取对话内容，也不会删除其他记录。若打开后发现选错，可在本次应用运行期间返回这里改选。";
  els.historyRecoverySec.hidden = false;
}

function publishFinalizeUnknown() {
  getConfigState().selection_pending = true;
  getConfigState().applied_profile_id = null;
  renderList();
}

async function restoreHistoryChoice(reference, resume = false) {
  if (!reference || isBusy()) return;
  setBusy(true, { kind: "historyRecovery" });
  setMsg("正在重新核验并恢复所选历史记录…");
  try {
    const result = await call("restore_history_choice", { reference, resume });
    if (resume) {
      if (result && result.history_recovery && Array.isArray(result.history_recovery.choices)) {
        showHistoryRecovery({ choices: result.history_recovery.choices });
      }
      let consumer;
      try {
        consumer = await call("finalize_consumer_state", { outcome: result });
      } catch (_) {
        publishFinalizeUnknown();
        setMsg("历史记录已恢复，但无法回读继续启动后的最终配置状态；不会误报为已应用。", "err");
        await refreshStatus();
        return;
      }
      getConfigState().selection_pending = !!consumer.selection_pending;
      getConfigState().applied_profile_id = consumer.applied_profile_id || null;
      renderList();
      if (consumer.disposition !== "ready") {
        setMsg(((result && (result.message || result.msg)) || "历史恢复后的继续启动未完成") + "；请检查后重试。", "err");
        await refreshStatus();
        return;
      }
      setMsg((result && result.msg) || "已恢复所选历史记录并启动。", "ok");
      setBrowserFallback(result && result.fallback_url);
      await refreshStatus();
      return;
    }
    if (result && result.history_recovery && Array.isArray(result.history_recovery.choices)) {
      showHistoryRecovery({ choices: result.history_recovery.choices });
    } else if (result && Array.isArray(result.choices)) {
      showHistoryRecovery(result);
    }
    let consumer;
    try {
      consumer = await call("finalize_consumer_state", { outcome: result });
    } catch (_) {
      publishFinalizeUnknown();
      setMsg("历史记录恢复结果无法完成权威回读；不会误报为已完成。", "err");
      await refreshStatus();
      return;
    }
    getConfigState().selection_pending = !!consumer.selection_pending;
    getConfigState().applied_profile_id = consumer.applied_profile_id || null;
    renderList();
    const restoredMessage = result && result.message || "已恢复所选历史记录。";
    if (!result || result.status !== "ok" || consumer.disposition !== "attention") {
      setMsg(restoredMessage + " 请先完成恢复清理再重试。", "err");
      await refreshStatus();
      return;
    }
    setMsg(restoredMessage + " 当前保持停止；请再次点击「一键开始」。", "ok");
  } catch (e) {
    setMsg((resume ? "恢复并启动失败：" : "恢复历史记录失败：") + runtimeCommandErrorText(e), "err");
  } finally {
    setBusy(false);
  }
}

async function checkOneClickBoundary() {
  if (isActivationInFlight()) {
    setMsg("当前选择仍在保存。请等待完成后再一键开始。", "err");
    return false;
  }
  // API admission is backend-owned, after any interrupted local recovery.
  return true;
}

async function runOneClick(runtimeChoice) {
  if (runtimeChoice) {
    if (!runtimeChoiceActiveId || runtimeChoiceActiveId !== getConfigState().active_id) {
      hideRuntimeChoice();
      setMsg("当前选择已变化，本次缓存运行选择已作废。请重新点击「一键开始」。", "err");
      return;
    }
  }
  if (!(await checkOneClickBoundary())) return;
  getSkillPage()?.invalidate();
  if (runtimePendingSha256 || scienceRuntimeUpdateRefreshInFlight > 0) {
    scienceRuntimeUpdateRefreshPending = true;
  }
  scienceRuntimeUpdateRefreshEpoch += 1;
  hideRuntimeChoice();
  setBusy(true, { kind: "oneClick" });
  setBrowserFallback("");
  startOneClickFeedback();
  try {
    const r = await call("one_click_login", { runtimeChoice: runtimeChoice || null });
    let consumer;
    try {
      consumer = await call("finalize_consumer_state", { outcome: r });
    } catch (readbackError) {
      hideHistoryRecovery();
      publishFinalizeUnknown();
      setMsg("一键开始已返回，但无法回读最终配置状态；不会把当前选择误报为已应用。请检查配置后重试。", "err");
      setBrowserFallback(r && r.fallback_url);
      await refreshStatus();
      return;
    }
    getConfigState().selection_pending = !!consumer.selection_pending;
    getConfigState().applied_profile_id = consumer.applied_profile_id || null;
    renderList();
    if (consumer.disposition === "attention" && r && r.action === "history_choice_required") {
      showHistoryRecovery(r);
      const cleanupWarning = consumer.cleanup_required ? "；另有私有事务快照等待安全清理" : "";
      setMsg((r.msg || "请选择要恢复的历史记录。") + cleanupWarning, "err");
      await refreshStatus();
      return;
    }
    if (consumer.disposition !== "ready") {
      hideHistoryRecovery();
      const recovery = consumer.journal_disposition === "open"
        ? "；安全事务仍待下次显式一键操作重放"
        : consumer.cleanup_required
          ? "；最终状态已回读，但仍有私有事务快照等待安全清理"
          : "；最终应用状态无法确认，需要人工检查";
      setMsg(((r && (r.message || r.msg)) || "一键开始未完成") + recovery + "（阶段：" + ((r && r.stage) || "unknown") + "）", "err");
      setBrowserFallback(r && r.fallback_url);
      await refreshStatus();
      return;
    }
    // 透传后端据实回传的 msg（已重开 / 已用新配置重启 / 沿用原对话 / 已启动 / 打开失败请手动打开）。
    const active = (getConfigState().profiles || []).find((p) => p.id === getConfigState().active_id);
    const message = r.msg || "已就绪，正在打开面板…";
    if (!els.historyRecoverySec.hidden) {
      els.historyRecoveryText.textContent =
        "已打开所选历史。如果内容不对，可在本次应用运行期间选择另一份；切换前 CSSwitch 会先安全停止隔离 Science。";
    }
    const recoveryWarning = consumer.cleanup_required
      ? " 私有事务快照仍待安全清理。"
      : r.recovery_status === "manual_recovery_required"
        ? " 最终应用状态已通过只读回读确认。"
        : "";
    setMsg(message + recoveryWarning + (isCodexSource(active)
      ? " 请在 Science 的 More models 中选择 Codex / … 后再发第一条消息；默认 Claude 壳会被明确拒绝。"
      : ""), "ok");
    setBrowserFallback(r.fallback_url);
    await refreshStatus();
  } catch (e) {
    setMsg("一键开始失败：" + runtimeCommandErrorText(e), "err");
  } finally {
    setBusy(false);
    if (scienceRuntimeUpdateRefreshPending) await refreshScienceRuntimeUpdate();
    await getSkillPage()?.refreshIfLoaded();
  }
}

async function importLocalSkill() {
  if (isBusy()) return;
  setBusy(true, { kind: "importSkill" });
  setMsg("正在选择并校验 Skill 包…");
  try {
    const result = await call("install_local_skill_package");
    if (result.status === "CANCELLED") {
      setMsg("已取消导入 Skill 包。");
    } else if (result.status === "INSTALLED_ATTACHED_VERIFY_REQUIRED") {
      setMsg("文件已安装并绑定 OPERON。请在 Science 中让 Agent 调用 skill(" + result.skill_name + ") 验证当前会话加载。", "ok");
    } else if (result.status === "BUNDLE_INSTALLED_ATTACHED") {
      const names = Array.isArray(result.skill_names) ? result.skill_names : [];
      const summary = names.slice(0, 4).join("、") + (names.length > 4 ? " 等" : "");
      setMsg("已安装并绑定 " + names.length + " 个 Skill" + (summary ? "：" + summary : "") + "。", "ok");
    } else {
      setMsg((result.message || "Skill 包导入未完成") + " [" + result.status + "]", "err");
    }
    if (result && result.directory_commit === true && getSkillPage()) {
      await getSkillPage().refresh();
    }
  } catch (e) {
    setMsg("导入 Skill 包失败：" + e, "err");
    await getSkillPage()?.refreshIfLoaded();
  } finally {
    setBusy(false);
  }
}

// ── 一键开始：先确认本次实际 Science runtime，再进入原启动链路。──
async function oneClick() {
  if (!(await checkOneClickBoundary())) return;
  setBusy(true, { kind: "oneClick" });
  setMsg("正在确认本次使用的 Claude Science…");
  try {
    const preflight = await call("science_runtime_preflight");
    if (preflight && preflight.status === "installed_ready") {
      setBusy(false);
      await runOneClick(null);
      return;
    }
    showRuntimeChoice(preflight || { status: "missing" });
    setMsg(preflight && preflight.status === "cached_choice_required"
      ? "Claude Science App 不可用或未通过预检。请选择是否仅本次使用已确认版本的缓存。"
      : "Claude Science App 不可用或未通过预检，且没有可安全启动的缓存版本。", "err");
  } catch (e) {
    setMsg("Science 运行环境检查失败：" + e, "err");
  } finally {
    setBusy(false);
  }
}

async function openScienceDownload() {
  try {
    await call("open_science_download_page");
    setMsg("已打开 Claude 官方下载页。安装完成后请再次点击「一键开始」。");
  } catch (e) {
    setMsg("打开 Claude 官方下载页失败：" + e, "err");
  }
}

function cancelRuntimeChoice() {
  hideRuntimeChoice();
  setMsg("已取消，本次没有启动 Claude Science。");
}

async function stopAll() {
  getSkillPage()?.invalidate();
  setBusy(true);
  setMsg("停止中…");
  try {
    await call("stop_all");
    setMsg("已停止代理与沙箱。", "ok");
    await refreshStatus();
  } catch (e) {
    setMsg("停止失败：" + e, "err");
  } finally {
    setBusy(false);
    await getSkillPage()?.refreshIfLoaded();
  }
}

async function openBrowser() {
  if (isBusy() || browserOpenInFlight) return;
  browserOpenInFlight = true;
  syncOpenBrowserControl();
  setMsg("正在获取新的 Science 地址并交给默认浏览器打开…");
  try {
    const result = await call("open_url");
    if (result && result.status === "error") {
      setBrowserFallback(result.fallback_url);
      setMsg(result.message || "打开浏览器失败；请复制 URL 手动打开。", "err");
    } else {
      setBrowserFallback("");
      setMsg((result && result.message) || "已向默认浏览器发出打开 Science 的请求；若窗口没有切到前台，请从 Dock 或其他桌面切回默认浏览器。", "ok");
    }
  } catch (e) {
    setMsg("打开浏览器失败：" + e, "err");
  } finally {
    browserOpenInFlight = false;
    syncOpenBrowserControl();
  }
}

function renderDoctorIntentResult(result, expectedIntent) {
  if (!result || result.schema_version !== 1 || result.intent !== expectedIntent || typeof result.status !== "string" || typeof result.message !== "string") {
    throw new Error("后端返回了无法识别的 Doctor intent 结果");
  }
  const completed = result.status === "passed" || result.status === "synchronized" || result.status === "not_required";
  setMsg(result.message, completed ? "ok" : "err");
}

async function runDoctorReadOnly() {
  if (doctorIntentInFlight || isBusy()) return;
  doctorIntentInFlight = true;
  setBusy(true, { kind: "doctorReadOnly" });
  startDoctorFeedback();
  try {
    const result = await call("run_doctor_read_only");
    renderDoctorIntentResult(result, "read_only_diagnostics");
  } catch (e) {
    setMsg("只读自检失败：" + e, "err");
  } finally {
    doctorIntentInFlight = false;
    setBusy(false);
  }
}

async function repairSkillRoute() {
  if (doctorIntentInFlight || isBusy()) return;
  doctorIntentInFlight = true;
  setBusy(true, { kind: "repairSkillRoute" });
  setMsg("正在核验并修复 CSSwitch 管理的 Skill 路由…");
  try {
    const result = await call("repair_skill_route");
    renderDoctorIntentResult(result, "repair_skill_route");
  } catch (e) {
    setMsg("Skill 路由修复失败：" + e, "err");
  } finally {
    doctorIntentInFlight = false;
    setBusy(false);
  }
}

// 简单 semver 比较：a 是否比 b 新。
function isNewer(a, b) {
  const pa = String(a).split(".").map((n) => parseInt(n, 10) || 0);
  const pb = String(b).split(".").map((n) => parseInt(n, 10) || 0);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const x = pa[i] || 0, y = pb[i] || 0;
    if (x !== y) return x > y;
  }
  return false;
}

async function checkUpdate() {
  setMsg("检查更新中…");
  let cur = "";
  try { cur = await call("app_version"); } catch (e) {}
  try {
    const resp = await fetch(
      "https://api.github.com/repos/SuperJJ007/CSSwitch/releases/latest",
      { headers: { Accept: "application/vnd.github+json" } }
    );
    if (!resp.ok) throw new Error("HTTP " + resp.status);
    const data = await resp.json();
    const latest = (data.tag_name || "").replace(/^v/, "");
    if (!latest) throw new Error("无版本信息");
    if (isNewer(latest, cur)) {
      setMsg("发现新版本 v" + latest + "（当前 v" + cur + "）。正在打开下载页…", "ok");
      try { await call("open_release_page"); } catch (_) {}
    } else {
      setMsg("已是最新版本（v" + cur + "）。", "ok");
    }
  } catch (e) {
    setMsg("无法自动检查更新（多为网络或代理限制）。已打开 Releases 页，请手动查看。", "err");
    try { await call("open_release_page"); } catch (_) {}
  }
}

async function refreshStatus() {
  try {
    const s = await call("status");
    setLight(els.ltProxy, s.proxy);
    setLight(els.ltSandbox, s.sandbox);
    setLight(els.ltUpstream, s.upstream);
    setStatusText("proxyStateText", s.proxy);
    setStatusText("sandboxStateText", s.sandbox);
    setStatusText("upstreamStateText", s.upstream);
    els.brandDot.className = "dot " + aggregateRuntimeStatus(s, {
      mode: getMode(),
      officialState: getOfficialRuntimeState(),
    });
    setStatusRecoveryMsg(proxyRecoveryMessage(s));
  } catch (e) {
    [els.ltProxy, els.ltSandbox, els.ltUpstream].forEach((l) => setLight(l, "unknown"));
    ["proxyStateText", "sandboxStateText", "upstreamStateText"].forEach((id) => setStatusText(id, "unknown"));
    els.brandDot.className = "dot gray";
  }
  if (scienceRuntimeUpdateRefreshPending && !isBusy()) {
    await refreshScienceRuntimeUpdate();
  }
}

  return {
    isBrowserOpenInFlight: () => browserOpenInFlight,
    isDoctorInFlight: () => doctorIntentInFlight,
    hideRuntimeChoice,
    showRuntimeChoice,
    refreshScienceRuntimeUpdate,
    applyScienceRuntimeUpdate,
    hideHistoryRecovery,
    showHistoryRecovery,
    publishFinalizeUnknown,
    restoreHistoryChoice,
    runOneClick,
    importLocalSkill,
    oneClick,
    openScienceDownload,
    cancelRuntimeChoice,
    stopAll,
    openBrowser,
    runDoctorReadOnly,
    repairSkillRoute,
    checkUpdate,
    refreshStatus,
  };
}

import {
  PREVIEW,
  PROFILE_INTERACTIVE_PREVIEW,
  mockStore,
} from "./preview-adapter.js";
import { runtimeCommandErrorText } from "./runtime-error.js";
import { call, configureDesktopWindow, listen } from "./ipc-client.js";
import { createProfileController } from "./profile-controller.js";
import { createRuntimeController } from "./runtime-controller.js";
import { runtimeStatusLabel, normalizeRuntimeLight } from "./runtime-status-state.js";

// CSSwitch 桌面面板前端。只调用后端 Tauri command，绝不碰任何密钥落盘逻辑。
// 后端只把 key 的【掩码】回显给这里；完整 key 永不进前端。
//
// ── Tauri 参数键约定（务必遵守）──────────────────────────────────────────────
// 本项目所有命令都是裸 `#[tauri::command]`（无 rename_all）。tauri-macros 默认
// `ArgumentCase::Camel`，会把 Rust 蛇形【顶层参数名】转成 lowerCamelCase 交给 JS：
//   template_id→templateId、base_url→baseUrl、api_format→apiFormat。
// 所以 invoke 顶层 args 用【小驼峰】。而 serde 结构体入参（`req`=FetchModelsReq、
// `cfg`=UiSettings）内部字段按结构体字段名（蛇形）：proxy_port/sandbox_port、
// template_id/base_url/key/profile_id。核对表见任务报告。
//
const $ = (id) => document.getElementById(id);
const els = {};
let statusTimer = null;
let profileController = null;
let runtimeController = null;
let busy = false;
let busyOp = null;
let activationInFlight = false;
let activationOp = null;
let busyMsgTimers = [];
let statusRecoveryMsg = "";
let skillPage = null;
let mode = "proxy"; // "proxy" 第三方 | "official" 官方
let officialRuntimeState = "gray";
// 当前配置快照（get_config 结果）。全 key 绝不在此，只有掩码。
let configState = { profiles: [], templates: [], active_id: "", applied_profile_id: null, selection_pending: false, proxy_port: 18991, sandbox_port: 8990, reuse_system_ssh: false, experimental_codex_enabled: false, codex_network: { mode: "auto", proxy_url: "" }, codex_network_resolved: { source: "direct", proxy_scheme: null } };
let lastBootSequence = -1;
let pendingConfirm = null;          // 危险操作（清 key / 删除）的「再点一次确认」态

const PAGE_META = {
  switch: ["工作台", "研究，从这里出发。", "连接你信赖的模型，把专注留给下一次发现。"],
  settings: ["偏好设置", "让工作空间，合你心意。", "管理本地端口与运行权限，保留简单、可控的使用方式。"],
};

const THEME_STORAGE_KEY = "csswitch-theme";

function savedTheme() {
  try {
    return window.localStorage?.getItem(THEME_STORAGE_KEY) || "";
  } catch (_) {
    return "";
  }
}

function applyTheme(theme, { persist = true } = {}) {
  const value = theme === "dark" ? "dark" : "light";
  document.documentElement.dataset.theme = value;
  if (els.themeBtn) {
    const label = value === "dark" ? "切换浅色主题" : "切换深色主题";
    els.themeBtn.setAttribute("aria-label", label);
    els.themeBtn.title = label;
    $("themeLabel").textContent = value === "dark" ? "浅色" : "深色";
  }
  if (persist) {
    try { window.localStorage?.setItem(THEME_STORAGE_KEY, value); } catch (_) {}
  }
}

function setPage(page) {
  if (!["switch", "settings"].includes(page)) page = "switch";
  const meta = PAGE_META[page] || PAGE_META.switch;
  document.querySelectorAll("[data-page]").forEach((node) => node.classList.toggle("active", node.dataset.page === page));
  document.querySelectorAll("[data-page-target]").forEach((node) => node.classList.toggle("active", node.dataset.pageTarget === page));
  document.querySelectorAll("[data-page-target]").forEach((node) => node.setAttribute("aria-current", node.dataset.pageTarget === page ? "page" : "false"));
  if (els.pageEyebrow) els.pageEyebrow.textContent = meta[0];
  if (els.pageTitle) els.pageTitle.textContent = meta[1];
  if (els.pageSubtitle) els.pageSubtitle.textContent = meta[2];
}

/** Normalize auto-boot failure payload: plain string (legacy) or one-click DTO object. */
function formatBootFailure(payload) {
  if (payload == null || payload === "") return "未知原因";
  if (typeof payload === "string") return payload;
  if (typeof payload === "object") {
    const message = payload.message || payload.msg || "未知原因";
    const stage = payload.stage ? "（阶段：" + payload.stage + "）" : "";
    const recovery = payload.recovery_status === "degraded"
      ? "；恢复也未完全成功"
      : payload.recovery_status === "environment_uncertain"
        ? "；环境状态不确定"
        : payload.recovery_status === "cleanup_required"
          ? "；私有事务快照仍待安全清理"
          : payload.recovery_status === "manual_recovery_required"
            ? "；最终状态需要按配置回读或再次显式重试"
            : "";
    return message + recovery + stage;
  }
  return String(payload);
}

function setMsg(text, kind) {
  // 去掉常驻「就绪。」：空消息或纯 idle 时整条反馈栏不占位，有真实反馈（结果/错误/自检）才冒出来。
  const t = text && text !== "就绪。" ? text : "";
  els.msg.textContent = t;
  els.msg.className = "msg" + (kind ? " " + kind : "");
  els.msg.parentElement.hidden = !t && (!els.browserFallback || els.browserFallback.hidden);
}

// Dismiss presentation only; do not cancel a task, clear its result, or mutate runtime state.
function dismissFeedback() {
  const panel = els.msg.parentElement;
  const focusInside = panel.contains(document.activeElement);
  panel.hidden = true;
  if (focusInside) {
    const target = els.doctorBtn && !els.doctorBtn.disabled
      ? els.doctorBtn : document.querySelector('[data-page-target="switch"]');
    target?.focus();
  }
}

function wireFeedbackDismissal() {
  $("feedbackCloseBtn").addEventListener("click", dismissFeedback);
  document.addEventListener("keydown", (event) => {
    // An open confirmation dialog owns Escape; never swallow its cancel gesture.
    if (event.key !== "Escape" || event.defaultPrevented || event.isComposing
        || document.querySelector("dialog[open]") || els.msg.parentElement.hidden) return;
    event.preventDefault();
    dismissFeedback();
  });
}

function setBrowserFallback(url) {
  const value = typeof url === "string" ? url.trim() : "";
  els.browserFallbackUrl.value = value;
  els.browserFallback.hidden = !value;
  els.msg.parentElement.hidden = !value && !els.msg.textContent;
}

function setLight(el, s) {
  const cls = { green: "g", amber: "a", red: "r", gray: "", unknown: "" }[normalizeRuntimeLight(s)];
  el.className = "lt" + (cls ? " " + cls : "");
  document.querySelectorAll('[data-mirror-light="' + el.id + '"]').forEach((node) => {
    node.className = "lt" + (cls ? " " + cls : "");
  });
}

function setStatusText(id, status) {
  const label = runtimeStatusLabel(id, status);
  const node = $(id);
  if (node) node.textContent = label;
  document.querySelectorAll('[data-mirror-text="' + id + '"]').forEach((mirror) => {
    mirror.textContent = label;
  });
}

const PROXY_UNHEALTHY_MSG = "代理进程不可达或已退出，请点击「启动 Science」恢复。";

function proxyRecoveryMessage(status) {
  const err = status && status.last_error;
  if (err && err.type === "proxy_unhealthy") return err.message || PROXY_UNHEALTHY_MSG;
  return "";
}

function setStatusRecoveryMsg(text) {
  if (busy) return;
  const current = els.msg.textContent || "";
  if (text) {
    if (!current || current === statusRecoveryMsg) {
      setMsg(text, "err");
      statusRecoveryMsg = text;
    }
    return;
  }
  if (statusRecoveryMsg && current === statusRecoveryMsg) {
    setMsg("");
  }
  statusRecoveryMsg = "";
}

function clearBusyMsgTimers() {
  busyMsgTimers.forEach((t) => clearTimeout(t));
  busyMsgTimers = [];
}

function sameOp(a, b) {
  return !!(a && b && a.kind === b.kind && (a.id || "") === (b.id || ""));
}

function scheduleBusyMsg(ms, op, text) {
  const timer = setTimeout(() => {
    if (busy && sameOp(busyOp, op)) setMsg(text);
  }, ms);
  busyMsgTimers.push(timer);
}

function startFetchModelsFeedback(id, codex = false) {
  clearBusyMsgTimers();
  if (!codex) {
    setMsg("正在用候选地址和 Key 做隔离模型探测；不会修改当前配置或正式代理…");
    scheduleBusyMsg(12000, { kind: "fetchModels", id }, "仍在等待上游模型列表。探测结果只会成为可选项，不会自动写入配置。");
    scheduleBusyMsg(30000, { kind: "fetchModels", id }, "模型探测接近等待上限。失败后仍可在已选 transport 下手工填写精确模型 ID。");
    return;
  }
  setMsg("正在读取 CSSwitch Codex 账号模型目录；首次授权刷新可能需要约 2 分钟…");
  scheduleBusyMsg(12000, { kind: "fetchModels", id }, "仍在等待 Codex 账号模型目录。不会修改模型选择、OAuth 或当前配置。");
  scheduleBusyMsg(60000, { kind: "fetchModels", id }, "仍在等待官方目录响应；CSSwitch 不会把任意模型当作默认模型。");
  scheduleBusyMsg(120000, { kind: "fetchModels", id }, "模型目录接近等待上限。若官方暂时不可达，可能返回带年龄标记的安全缓存。");
}

function startActivateFeedback(id) {
  const name = profileController.profileName(id);
  setMsg("正在把「" + name + "」设为当前选择；不会启动或切换正在运行的服务。");
}

function startSaveConnectionFeedback(id, selected) {
  clearBusyMsgTimers();
  if (selected) {
    setMsg("正在保存当前选择的连接；当前运行链保持不变，下次启动 Science时应用…");
    scheduleBusyMsg(4500, { kind: "saveConnection", id }, "仍在等待候选连接上游校验。当前运行链保持不变。");
    scheduleBusyMsg(18000, { kind: "saveConnection", id }, "上游校验接近等待上限。验证后只保存候选连接。");
    return;
  }
  setMsg("保存连接中：正在做候选上游校验；无法确认时会保存但标记为未校验…");
  scheduleBusyMsg(4500, { kind: "saveConnection", id }, "仍在等待候选连接校验。不会影响当前正在运行的代理。");
}

function startOneClickFeedback() {
  clearBusyMsgTimers();
  setMsg("启动 Science：检查代理 → 保护凭据与历史 → 准备虚拟登录 → 启动/复用沙箱 → 探活…");
  scheduleBusyMsg(3500, { kind: "oneClick" }, "正在建立启动事务并保护凭据、组织历史和插件配置；不会复制 Science 的 Conda 或 Runtime 环境。");
  scheduleBusyMsg(9000, { kind: "oneClick" }, "仍在准备受保护状态或启动沙箱。完成后会自动打开 Science；失败会显示日志摘要。");
}

function startSwitchModeFeedback(targetMode) {
  clearBusyMsgTimers();
  const toOfficial = targetMode === "official";
  setMsg(toOfficial
    ? "正在切到官方模式：停止第三方代理/沙箱并保存模式…"
    : "正在切到第三方模式：保存模式，完成后可选择配置并启动 Science…");
  scheduleBusyMsg(3500, { kind: "switchMode", id: targetMode }, toOfficial
    ? "仍在停止第三方链路。真实 Claude Science 实例不会被触碰。"
    : "仍在保存模式切换。当前不会自动启动第三方代理。");
}

function startPortSaveFeedback(changed) {
  clearBusyMsgTimers();
  if (changed) {
    setMsg("正在保存端口设置：端口变化会先重置当前代理/沙箱链路…");
    scheduleBusyMsg(3500, { kind: "ports" }, "仍在应用端口设置。若旧沙箱无法停止，端口会保持原值并显示错误。");
    return;
  }
  setMsg("正在保存端口设置…");
}

function startDoctorFeedback() {
  clearBusyMsgTimers();
  setMsg("只读自检中：正在运行本地诊断脚本…");
  scheduleBusyMsg(3500, { kind: "doctorReadOnly" }, "只读自检仍在运行。它会检查本地依赖、端口和脱敏配置摘要；不会修复 Skill 路由、启动 Science 或 Gateway，也不会读取真实 Science HOME 或展示完整 key。");
}

function setBusy(on, op) {
  busy = on;
  busyOp = on ? (op || { kind: "global" }) : null;
  if (!on) clearBusyMsgTimers();
  [
    els.oneClickBtn, els.stopBtn, els.importSkillBtn, els.newBtn,
    els.doctorBtn, els.repairSkillRouteBtn,
    els.runtimeActivateUpdateBtn, els.runtimeKeepActiveBtn,
    els.runtimeUseCacheBtn, els.runtimeDownloadBtn, els.runtimeChoiceCancelBtn,
    els.wizSaveBtn, els.wizFetchBtn, els.wizCancelBtn,
    els.connSaveBtn, els.connFetchBtn, els.connClearBtn, els.connCancelBtn,
    els.wizModel, els.wizRoleQuality, els.wizRoleFast, els.wizRoleFable,
    els.connModel, els.connRoleQuality, els.connRoleFast, els.connRoleFable,
    els.metaSaveBtn, els.metaCancelBtn, $("saveSettingsBtn"),
    // 端口输入也纳入忙碌禁用：忙碌中改端口会与在途操作竞态（修 P1-c 前端侧）。
    els.proxyPort, els.sandboxPort, els.reuseSystemSsh,
  ].forEach((b) => b && (b.disabled = on));
  syncOpenBrowserControl();
  if (skillPage) skillPage.setGlobalBusy(on);
  // 模式切换按钮同样禁用：忙碌中切官方会与「启动 Science」竞态（修 P1-b 前端侧）。
  if (els.modeSeg) els.modeSeg.querySelectorAll(".seg-btn").forEach((b) => (b.disabled = on));
  profileController.syncProfileBusyState();
  // 松开忙碌时，把模型必填保存门控交回门（避免 setBusy(false) 覆盖门控）。
  if (!on) { profileController.refreshWizGate(); profileController.refreshConnGate(); }
  syncActivationControls();
}

function syncOpenBrowserControl() {
  if (!els.openBrowserBtn) return;
  els.openBrowserBtn.disabled = busy || runtimeController.isBrowserOpenInFlight();
  els.openBrowserBtn.textContent = runtimeController.isBrowserOpenInFlight() ? "打开中…" : "打开 Science";
}

function syncActivationControls() {
  const writeLocked = busy;
  [
    els.newBtn, els.proxyPort, els.sandboxPort, els.reuseSystemSsh,
    els.connClearBtn, els.metaSaveBtn,
  ].forEach((b) => b && (b.disabled = writeLocked));
  if (els.modeSeg) els.modeSeg.querySelectorAll(".seg-btn").forEach((b) => (b.disabled = writeLocked));
  if (els.oneClickBtn) els.oneClickBtn.disabled = busy || activationInFlight;
  if (els.runtimeUseCacheBtn) els.runtimeUseCacheBtn.disabled = busy || activationInFlight;
  if (els.reuseSystemSsh) els.reuseSystemSsh.disabled = busy || activationInFlight;
  profileController.syncProfileBusyState();
  profileController.refreshWizGate();
  profileController.refreshConnGate();
}

function setActivationInFlight(on, op) {
  activationInFlight = on;
  activationOp = on ? op : null;
  syncActivationControls();
}

function escapeHtml(s) {
  return String(s == null ? "" : s).replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c])
  );
}

// ── 视图切换：列表 / 新建向导 / 连接编辑 / 改名。一次只显示一个表单（列表隐去减少高度）。──
function showView(v) {
  els.connectionOverview.hidden = v !== "list";
  els.listSec.hidden = v !== "list";
  els.wizSec.hidden = v !== "wizard";
  els.connSec.hidden = v !== "conn";
  els.metaSec.hidden = v !== "meta";
  els.panel.classList.toggle("view-form", v !== "list");
  if (v !== "list") setPage("switch");
}
function cancelForm() { showView("list"); setPage("switch"); setMsg("就绪。"); }

// 危险操作「再点一次确认」（避免依赖 window.confirm，Tauri webview 里不可靠）。
function confirmAction(token, promptText, fn) {
  if (pendingConfirm && pendingConfirm.token === token) {
    clearTimeout(pendingConfirm.timer);
    pendingConfirm = null;
    fn();
    return;
  }
  if (pendingConfirm) clearTimeout(pendingConfirm.timer);
  pendingConfirm = {
    token,
    timer: setTimeout(() => { pendingConfirm = null; setMsg("已取消。"); }, 4000),
  };
  setMsg(promptText + " —— 再点一次同一按钮确认（4 秒内）。", "err");
}

profileController = createProfileController({
  els,
  getConfigState: () => configState,
  getSkillPage: () => skillPage,
  getMode: () => mode,
  setMode: (value) => { mode = value; },
  setOfficialRuntimeState: (value) => { officialRuntimeState = value; },
  isBusy: () => busy,
  getBusyOp: () => busyOp,
  isActivationInFlight: () => activationInFlight,
  getActivationOp: () => activationOp,
  setActivationInFlight,
  setBusy,
  setMsg,
  setPage,
  showView,
  confirmAction,
  escapeHtml,
  startFetchModelsFeedback,
  startActivateFeedback,
  startSaveConnectionFeedback,
  startSwitchModeFeedback,
  startPortSaveFeedback,
  runtime: {
    hideHistoryRecovery: (...args) => runtimeController.hideHistoryRecovery(...args),
    refreshStatus: (...args) => runtimeController.refreshStatus(...args),
    oneClick: (...args) => runtimeController.oneClick(...args),
  },
});

runtimeController = createRuntimeController({
  els,
  getConfigState: () => configState,
  getSkillPage: () => skillPage,
  isBusy: () => busy,
  isActivationInFlight: () => activationInFlight,
  getMode: () => mode,
  getOfficialRuntimeState: () => officialRuntimeState,
  setBusy,
  setMsg,
  setBrowserFallback,
  startOneClickFeedback,
  startDoctorFeedback,
  isCodexSource: (...args) => profileController.isCodexSource(...args),
  renderList: (...args) => profileController.renderList(...args),
  runtimeCommandErrorText: runtimeCommandErrorText,
  syncOpenBrowserControl,
  setLight,
  setStatusText,
  setStatusRecoveryMsg,
  proxyRecoveryMessage,
});

function wire() {
  [
    "oneClickBtn", "stopBtn", "importSkillBtn", "refreshSkillsBtn", "ltProxy", "ltSandbox", "ltUpstream",
    "runtimeChoiceSec", "runtimeChoiceText", "runtimeActivateUpdateBtn", "runtimeKeepActiveBtn", "runtimeUseCacheBtn", "runtimeDownloadBtn", "runtimeChoiceCancelBtn",
    "historyRecoverySec", "historyRecoveryText", "historyRecoveryChoices", "historyRecoveryCancelBtn",
    "msg", "browserFallback", "browserFallbackUrl", "browserFallbackCopyBtn", "browserFallbackRetryBtn", "brandDot", "openBrowserBtn", "doctorBtn", "repairSkillRouteBtn", "updateBtn", "verLabel",
    "reportBtn", "logsBtn", "quitBtn", "modeSeg", "proxyPort", "sandboxPort", "reuseSystemSsh", "advSec",
    "connectionOverview", "listSec", "profileList", "newBtn",
    "wizSec", "wizTemplate", "wizTemplateChips", "wizTplLabel", "wizTplHint", "wizName", "wizBaseGroup", "wizBase", "wizBaseHint",
    "wizModelGroup", "wizModelLabel", "wizFetchBtn", "wizModelInfo", "wizModel", "wizModelHint", "wizCodexCatalog", "wizCodexCatalogMeta", "wizCodexCatalogList", "wizStaticCatalog", "wizRoleQuality", "wizRoleFast", "wizRoleFable", "wizCatalogWarning", "wizKeyGroup", "wizKey", "wizSaveBtn", "wizCancelBtn",
    "connSec", "connTitle", "connBaseGroup", "connBase", "connBaseHint", "connFetchBtn",
    "connModelGroup", "connModelLabel", "connModelInfo", "connModel", "connModelHint", "connCodexCatalog", "connCodexCatalogMeta", "connCodexCatalogList", "connStaticCatalog", "connRoleQuality", "connRoleFast", "connRoleFable", "connCatalogWarning", "connKeyGroup", "connKey", "connSaveBtn", "connClearBtn", "connCancelBtn",
    "metaSec", "metaName", "metaNotes", "metaSaveBtn", "metaCancelBtn",
    "themeBtn", "pageEyebrow", "pageTitle", "pageSubtitle",
    "proxyStateText", "sandboxStateText", "upstreamStateText",
  ].forEach((id) => (els[id] = $(id)));
  els.panel = document.querySelector(".panel");
  wireFeedbackDismissal();

  document.querySelectorAll("[data-page-target]").forEach((button) => {
    button.addEventListener("click", () => {
      if (button.dataset.pageTarget === "switch") showView("list");
      setPage(button.dataset.pageTarget);
    });
  });
  applyTheme(savedTheme() || document.documentElement.dataset.theme || "light", { persist: false });
  els.themeBtn.addEventListener("click", () => applyTheme(document.documentElement.dataset.theme === "dark" ? "light" : "dark"));


  $("saveSettingsBtn").addEventListener("click", profileController.persistRuntimeSettings);
  $("profileSearch").addEventListener("input", profileController.renderList);
  $("batchManageBtn").addEventListener("click", profileController.toggleBatchMode);
  $("batchDeletePhrase").addEventListener("input", profileController.syncBatchConfirmation);
  $("batchClearSelection").addEventListener("click", profileController.clearSelection);
  $("batchClearKeys").addEventListener("click", () => profileController.requestBatch("clearkey"));
  $("batchDelete").addEventListener("click", () => profileController.requestBatch("delete"));
  $("batchConfirm").addEventListener("click", profileController.confirmBatch);
  $("batchCancel").addEventListener("click", profileController.cancelBatch);
  $("batchDialog").addEventListener("cancel", e => { e.preventDefault(); profileController.cancelBatch(); });


  // 列表行内操作（事件委托；忙碌时忽略）。
  els.profileList.addEventListener("click", (e) => {
    if (busy) return;
    const btn = e.target.closest("[data-act]");
    const row = e.target.closest("[data-id]");
    if (!btn || !row || btn.disabled || btn.dataset.permanentlyDisabled === "true") return;
    const id = row.getAttribute("data-id");
    const act = btn.getAttribute("data-act");
    if (act === "activate") profileController.activate(id);
    else if (act === "editconn") profileController.openConn(id);
    else if (act === "editmeta") profileController.openMeta(id);
    else if (act === "clearkey") profileController.clearKey(id);
    else if (act === "delete") profileController.del(id);
  });

  // 浏览器交互预览可单独启用模型下拉；真实 App 始终使用后端已保存模型。
  els.profileList.addEventListener("change", (e) => {
    const checkbox = e.target.closest("[data-select-profile]");
    if (checkbox) { profileController.changeSelection(checkbox.dataset.selectProfile, checkbox.checked); return; }
    const select = e.target.closest("[data-profile-model]");
    if (!select || !PROFILE_INTERACTIVE_PREVIEW) return;
    const id = select.getAttribute("data-profile-model");
    const model = select.value;
    const profile = (configState.profiles || []).find((item) => item.id === id);
    const stored = (mockStore.profiles || []).find((item) => item.id === id);
    if (!profile || !stored) return;
    profile.model = model;
    stored.model = model;
    profileController.renderList();
    setMsg(`预览：已将「${profile.name}」的模型设为 ${model}。`, "ok");
  });

  els.newBtn.addEventListener("click", profileController.openWizard);
  els.wizTemplateChips.addEventListener("click", (e) => {
    if (busy) return;
    const chip = e.target.closest(".chip");
    if (chip) profileController.selectWizTemplate(chip.getAttribute("data-tid"));
  });
  [els.wizModel, els.wizRoleQuality, els.wizRoleFast, els.wizRoleFable]
    .forEach((input) => input.addEventListener("input", () => profileController.catalogRolesChanged("wizard")));
  els.wizFetchBtn.addEventListener("click", profileController.wizFetch);
  els.wizSaveBtn.addEventListener("click", profileController.wizSave);
  els.wizCancelBtn.addEventListener("click", cancelForm);

  [els.connModel, els.connRoleQuality, els.connRoleFast, els.connRoleFable]
    .forEach((input) => input.addEventListener("input", () => profileController.catalogRolesChanged("connection")));
  els.connFetchBtn.addEventListener("click", profileController.connFetch);
  els.connSaveBtn.addEventListener("click", profileController.connSave);
  els.connClearBtn.addEventListener("click", () => profileController.clearKey(els.connSec.dataset.id));
  els.connCancelBtn.addEventListener("click", cancelForm);

  els.metaSaveBtn.addEventListener("click", profileController.metaSave);
  els.metaCancelBtn.addEventListener("click", cancelForm);

  els.oneClickBtn.addEventListener("click", profileController.heroClick);
  els.runtimeActivateUpdateBtn.addEventListener("click", () => runtimeController.applyScienceRuntimeUpdate("activate_pending"));
  els.runtimeKeepActiveBtn.addEventListener("click", () => runtimeController.applyScienceRuntimeUpdate("keep_active"));
  els.runtimeUseCacheBtn.addEventListener("click", () => runtimeController.runOneClick("cached_once"));
  els.runtimeDownloadBtn.addEventListener("click", runtimeController.openScienceDownload);
  els.runtimeChoiceCancelBtn.addEventListener("click", runtimeController.cancelRuntimeChoice);
  els.historyRecoveryChoices.addEventListener("click", (event) => {
    const button = event.target.closest("[data-history-reference]");
    if (button) runtimeController.restoreHistoryChoice(
      button.dataset.historyReference,
      button.dataset.historyResume === "true",
    );
  });
  els.historyRecoveryCancelBtn.addEventListener("click", runtimeController.hideHistoryRecovery);
  els.stopBtn.addEventListener("click", runtimeController.stopAll);
  els.openBrowserBtn.addEventListener("click", runtimeController.openBrowser);
  els.browserFallbackRetryBtn.addEventListener("click", runtimeController.openBrowser);
  els.browserFallbackCopyBtn.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(els.browserFallbackUrl.value);
      setMsg("Science URL 已复制。", "ok");
    } catch (_) {
      els.browserFallbackUrl.select();
      setMsg("无法自动写入剪贴板；已选中 URL，请手动复制。", "err");
    }
  });
  els.doctorBtn.addEventListener("click", runtimeController.runDoctorReadOnly);
  els.logsBtn.addEventListener("click", () =>
    call("open_logs").catch((e) => setMsg("打开日志失败：" + e, "err"))
  );
  els.quitBtn.addEventListener("click", () => {
    setBusy(true);
    setMsg("正在停止代理与隔离 Science…");
    call("quit_app").catch((e) => {
      setBusy(false);
      setMsg("退出失败：" + e + "请先使用“全部停止”重试。", "err");
    });
  });
}

window.addEventListener("DOMContentLoaded", async () => {
  wire();
  await configureDesktopWindow();
  setPage("switch");
  $("previewNotice").hidden = !PREVIEW;
  await profileController.loadConfig();
  const applyBootPublication = (publication) => {
    if (!publication || !Number.isSafeInteger(publication.sequence) || publication.sequence < 0) return;
    if (publication.sequence <= lastBootSequence) return;
    if (!["idle", "starting", "ready", "failed", "attention"].includes(publication.state)) return;
    lastBootSequence = publication.sequence;
    const payload = publication.payload;
    if (publication.state === "failed" && payload) {
      runtimeController.publishFinalizeUnknown();
      setMsg("自动启动未成功：" + formatBootFailure(payload) + "\n可检查配置后点「启动 Science」重试。", "err");
      runtimeController.refreshStatus();
    } else if (publication.state === "attention" && payload) {
      runtimeController.publishFinalizeUnknown();
      if (payload.action === "history_choice_required") {
        runtimeController.showHistoryRecovery(payload);
        setMsg(payload.msg || "自动启动需要先选择要恢复的历史记录。", "err");
      }
      runtimeController.refreshStatus();
    }
  };
  try {
    await listen("boot://publication", (event) => applyBootPublication(event.payload));
  } catch (e) {
    setMsg("无法订阅自动启动状态：" + e, "err");
  }
  try {
    await listen("science-runtime://update", () => runtimeController.refreshScienceRuntimeUpdate());
  } catch (e) {}
  try {
    applyBootPublication(await call("boot_snapshot"));
  } catch (e) {}
  try { els.verLabel.textContent = "v" + (await call("app_version")); } catch (e) {}
  await runtimeController.refreshScienceRuntimeUpdate();
  await runtimeController.refreshStatus();
  if (!PREVIEW) statusTimer = setInterval(() => runtimeController.refreshStatus(), 2500);
});

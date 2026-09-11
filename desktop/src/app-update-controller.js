export function createAppUpdateController({ call, isBusy, setBusy, status, checkButton,
  installButton, notice, confirm, showSettings, schedule = setInterval }) {
  let pending = null;
  let checking = false;
  let installing = false;
  function render() {
    checkButton.disabled = checking || installing;
    installButton.disabled = checking || installing;
    installButton.hidden = !pending;
    notice.hidden = !pending;
    notice.textContent = pending ? `发现研舟 v${pending.version} · 查看更新` : "";
  }
  async function check() {
    if (checking || installing || isBusy()) return;
    checking = true;
    status.textContent = "正在检查 GitHub 稳定版…";
    render();
    try {
      const result = await call("check_app_update");
      pending = result.status === "available" ? result : null;
      status.textContent = pending ? `当前 v${result.current_version}，可更新至 v${result.version}。`
        : result.status === "not_configured" ? "此构建尚未配置更新签名公钥；请使用维护者发布的正式安装包。"
        : `当前 v${result.current_version}，已是最新稳定版。`;
    } catch (_) {
      // Never expose provider/backend/network error text in the update UI.
      status.textContent = "暂时无法检查更新（网络不可用或发布尚未提供更新清单）。可稍后重试。";
    } finally { checking = false; render(); }
  }
  async function install() {
    if (!pending || checking || installing || isBusy()) return;
    installing = true;
    setBusy(true, { kind: "appUpdate" });
    render();
    try {
      if (!(await confirm())) return;
      status.textContent = "正在下载并校验更新；成功后会停止服务、安装并重启…";
      await call("install_app_update", { expectedVersion: pending.version });
      status.textContent = "更新已安装，正在重启研舟…";
    } catch (_) {
      status.textContent = "更新未完成。请检查网络、签名及应用目录写入权限后重试；若服务已停止，请手动启动 Science。";
    } finally {
      installing = false;
      setBusy(false);
      render();
    }
  }
  return { check, install, start({ automatic = true } = {}) {
    checkButton.addEventListener("click", check);
    installButton.addEventListener("click", install);
    notice.addEventListener("click", showSettings);
    render();
    if (automatic) { void check(); return schedule(() => void check(), 6 * 60 * 60 * 1000); }
    return null;
  } };
}

use super::*;

pub(super) fn open_official_inner() -> Result<(), String> {
    let app_path = "/Applications/Claude Science.app";
    let mut cmd = Command::new("open");
    if Path::new(app_path).is_dir() {
        cmd.arg(app_path);
    } else {
        cmd.arg("-a").arg("Claude Science");
    }
    cmd.env_remove("ANTHROPIC_BASE_URL")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN");
    match cmd.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(_) => Err("未能打开 Claude Science。请确认已安装官方 Claude Science。".into()),
        Err(e) => Err(format!("打开官方 Claude Science 失败：{e}")),
    }
}

pub(super) fn open_science_download_page_inner() -> Result<(), String> {
    open_in_browser(SCIENCE_DOWNLOAD_URL)
}

pub(super) fn manual_open_result(url: String, result: Result<(), String>) -> serde_json::Value {
    match result {
        Ok(()) => json!({
            "status": "ok",
            "message": "已向默认浏览器发出打开 Science 的请求。",
            "fallback_url": null,
        }),
        Err(error) => json!({
            "status": "error",
            "message": format!("打开浏览器失败：{error}"),
            "fallback_url": url,
        }),
    }
}

#[cfg(test)]
pub(super) fn open_url_inner(state: &SharedAppState) -> Result<serde_json::Value, String> {
    open_url_with(state, |url| open_in_browser(url).map(|_| "browser"))
}

fn open_url_with(
    state: &SharedAppState,
    open: impl FnOnce(&str) -> Result<&'static str, String>,
) -> Result<serde_json::Value, String> {
    let (sandbox_port, runtime) = {
        let st = lock(state);
        let runtime = st
            .science_runtime
            .clone()
            .ok_or("隔离 Science 尚未运行，请先「一键开始」。")?;
        (st.sandbox_port, runtime)
    };
    if sandbox_port == 0 || !ScienceHostAdapter::listener_matches(sandbox_port, &runtime) {
        return Err("隔离 Science 尚未就绪，请重新点击「一键开始」。".into());
    }
    // Science 的控制地址可能是短期、一次性的。每次手动打开都重新获取，
    // 不复用 one-click 已消费的内存 URL。成功时不返回 URL；只有系统
    // opener 失败时才把同一次新 URL 交给 UI，供用户复制或再次打开。
    let url = ScienceHostAdapter::url(sandbox_port, &runtime);
    match open(&url) {
        Ok(_) => Ok(manual_open_result(url, Ok(()))),
        Err(error) => Ok(json!({"status":"error", "message":error, "fallback_url":url})),
    }
}

pub(super) async fn open_url_command(
    _app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
) -> Result<serde_json::Value, String> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking(move || {
        lifecycle.with_observed_context(|| {
            open_url_with(&state, |url| open_in_browser(url).map(|_| "browser"))
        })
    })
    .await
}

use super::*;

#[derive(Deserialize)]
pub(crate) struct FetchModelsReq {
    /// 模板 id（决定 builtin / base_url 可编辑性 / 默认 base_url）。
    template_id: String,
    /// 编辑已存 profile 时的实际 api_format；为空则按模板默认值。
    #[serde(default)]
    api_format: Option<String>,
    /// 自定义模板时用户填的 base_url（不可编辑模板忽略）。
    #[serde(default)]
    base_url: String,
    /// 用户新填的 key；为空表示沿用 profile_id 已存的 key（后端不回传完整 key）。
    #[serde(default)]
    key: String,
    /// 编辑已存 profile 时传其 id（用于沿用已存 key）。
    #[serde(default)]
    profile_id: Option<String>,
}

/// 「获取可用模型」——纯 scratch 探测：只用临时代理探候选 base_url/key 的 /v1/models，
/// 绝不写 config、不改 AppState、不碰正在服务 Science 的正式代理。
pub(super) async fn fetch_models_command(
    app: tauri::AppHandle,
    lifecycle: State<'_, SharedLifecycle>,
    req: FetchModelsReq,
) -> Result<serde_json::Value, crate::commands::codex::RuntimeCommandError> {
    crate::api_only::require_api_template(&req.template_id)?;
    if let Some(id) = req.profile_id.as_deref() {
        crate::api_only::require_api_profile(&config::default_dir(), Some(id))?;
    }
    let lifecycle = lifecycle.inner().clone();
    run_blocking_typed(
        move || -> Result<_, crate::commands::codex::RuntimeCommandError> {
            let request = crate::runtime::model_discovery::ModelDiscoveryRequest {
                template_id: req.template_id,
                api_format: req.api_format,
                base_url: req.base_url,
                key: req.key,
                profile_id: req.profile_id,
            };
            let adapter = crate::runtime::model_discovery::request_adapter(&request)?;
            let target = request.profile_id.as_ref().map_or(
                crate::commands::codex::CodexPreflightTarget::NoProfile,
                |id| crate::commands::codex::CodexPreflightTarget::Profile(id.clone()),
            );
            let prepared = crate::commands::codex::prepare_provider_auth(&app, &adapter, target)?;
            lifecycle
                .with_observed_context(|| -> Result<(), String> {
                    if let Some(prepared) = prepared.as_ref() {
                        prepared.verify_unchanged()?;
                    }
                    Ok(())
                })
                .map_err(crate::commands::codex::RuntimeCommandError::from)?;
            Ok(crate::runtime::model_discovery::fetch_models(
                app,
                request,
                prepared.as_ref().map(|prepared| prepared.proof()),
            )?)
        },
    )
    .await
}

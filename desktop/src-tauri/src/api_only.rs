//! Public product boundary. Legacy storage/recovery stays intact; account-only
//! profiles are neither exposed nor admitted by this edition's command surface.
use crate::config;
use serde_json::Value;

pub(crate) fn require_api_template(template_id: &str) -> Result<(), String> {
    if template_id == "codex" {
        return Err("此版本仅支持第三方 API，不提供 Codex 账号连接。原有账号数据未被删除。".into());
    }
    Ok(())
}

pub(crate) fn require_api_profile(dir: &std::path::Path, id: Option<&str>) -> Result<(), String> {
    let cfg = config::load_current_from_read_only(dir).map_err(|e| e.to_string())?;
    let profile = match id {
        Some(id) => cfg.profile_by_id(id),
        None => cfg.active_profile(),
    }
    .ok_or("请先添加 API，并将一条配置设为当前。")?;
    require_api_template(&profile.template_id)
}

// Missing IDs retain the mutation owner's existing no-op/error contract.
pub(crate) fn require_api_mutation_target(dir: &std::path::Path, id: &str) -> Result<(), String> {
    let cfg = config::load_current_from_read_only(dir).map_err(|e| e.to_string())?;
    check_api_mutation_target(&cfg, id)
}

fn check_api_mutation_target(cfg: &config::Config, id: &str) -> Result<(), String> {
    if let Some(profile) = cfg.profile_by_id(id) {
        require_api_template(&profile.template_id)?;
    }
    Ok(())
}

pub(crate) fn project_config(mut value: Value) -> Value {
    for key in ["profiles", "templates"] {
        if let Some(items) = value.get_mut(key).and_then(Value::as_array_mut) {
            items.retain(|item| {
                item.get("template_id")
                    .or_else(|| item.get("id"))
                    .and_then(Value::as_str)
                    != Some("codex")
            });
        }
    }
    let active_visible = value["profiles"]
        .as_array()
        .is_some_and(|profiles| profiles.iter().any(|p| p["id"] == value["active_id"]));
    if !active_visible {
        value["active_id"] = Value::String(String::new());
        value["selection_pending"] = Value::Bool(true);
    }
    if let Some(object) = value.as_object_mut() {
        for field in [
            "experimental_codex_enabled",
            "codex_network",
            "codex_network_resolved",
        ] {
            object.remove(field);
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn api_only_projection_preserves_api_and_does_not_select_a_replacement() {
        let input = json!({"active_id":"legacy", "profiles":[
            {"id":"legacy", "template_id":"codex"}, {"id":"api", "template_id":"custom-openai"}],
            "templates":[{"id":"codex"},{"id":"custom-openai"}],
            "experimental_codex_enabled":true,"codex_network":{"mode":"auto"}});
        let projected = project_config(input.clone());
        assert_eq!(projected["profiles"].as_array().unwrap().len(), 1);
        assert_eq!(projected["templates"].as_array().unwrap().len(), 1);
        assert_eq!(projected["active_id"], "");
        assert!(projected.get("codex_network").is_none());
        assert_eq!(input["profiles"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn api_only_rejects_account_templates_but_keeps_openai_api() {
        assert!(require_api_template("codex").is_err());
        for id in [
            "custom",
            "custom-openai",
            "custom-openai-responses",
            "deepseek",
        ] {
            assert!(require_api_template(id).is_ok());
        }
    }

    #[test]
    fn api_only_mutation_targets_reject_hidden_accounts_and_preserve_missing_ids() {
        let mut cfg = config::Config::default();
        cfg.profiles = vec![
            config::Profile {
                id: "legacy".into(),
                template_id: "codex".into(),
                ..Default::default()
            },
            config::Profile {
                id: "api".into(),
                template_id: "custom".into(),
                ..Default::default()
            },
        ];
        assert!(check_api_mutation_target(&cfg, "legacy").is_err());
        assert!(check_api_mutation_target(&cfg, "api").is_ok());
        assert!(check_api_mutation_target(&cfg, "missing").is_ok());
    }

    #[test]
    fn api_only_projection_preserves_selected_api() {
        let projected = project_config(json!({"profiles":[{"id":"api","template_id":"custom"}],
            "active_id":"api","selection_pending":false,"templates":[]}));
        assert_eq!(projected["active_id"], "api");
        assert_eq!(projected["selection_pending"], false);
    }
}

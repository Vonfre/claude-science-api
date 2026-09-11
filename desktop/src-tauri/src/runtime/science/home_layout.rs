// The daemon browses the host HOME, but its config, auth and Conda roots remain
// CSSwitch-owned. Control probes keep the isolated HOME and never need this mode.
const SCIENCE_HOST_SECURITY_WRAPPER: &str = r#"#!/bin/zsh -f
set -eu
# Keep the existing CSSwitch key store when the daemon uses the host HOME.
export HOME="${0:A:h:h}"
exec /usr/bin/security "$@"
"#;

struct ScienceHostHomeLaunch {
    config_sha256: String,
    security_sha256: String,
}

// Called by the common host adapter for cold start, recovery and auto start.
// Without an explicit saved opt-in, do not even prepare the host-home config.
fn configure_science_home_access(
    command: &mut Command,
    sandbox: &Path,
    accepted: bool,
) -> Result<(), String> {
    if !accepted {
        return Ok(());
    }
    let home = prepare_science_host_home(sandbox)?;
    super::launch_env::configure_science_host_home(
        command,
        &home.config_sha256,
        &home.security_sha256,
    );
    Ok(())
}

fn prepare_science_host_home(sandbox: &Path) -> Result<ScienceHostHomeLaunch, String> {
    use super::ssh_bridge::{atomic_write, checked_file_bytes, reject_symlink_components};
    use toml_edit::{value, DocumentMut, Table};

    reject_symlink_components(sandbox)?;
    let data = sandbox.join(".claude-science");
    let config = data.join("config.toml");
    reject_symlink_components(&config)?;
    let prior = checked_file_bytes(&config, 1024 * 1024, "Science 启动配置")?;
    let mut document = match prior.as_deref() {
        Some(bytes) => std::str::from_utf8(bytes)
            .map_err(|_| "Science 启动配置不是 UTF-8")?
            .parse::<DocumentMut>()
            .map_err(|_| "Science 启动配置不是有效 TOML")?,
        None => DocumentMut::new(),
    };
    if document.get("paths").is_none() {
        document["paths"] = toml_edit::Item::Table(Table::new());
    }
    if !document["paths"].is_table_like() {
        return Err("Science 配置中的 paths 必须是表".into());
    }
    // Explicit paths prevent --data-dir from leaving auth/Conda at host defaults.
    for (key, expected) in [
        ("auth_dir", data.clone()),
        ("conda_home", data.join("conda")),
    ] {
        if let Some(existing) = document["paths"].get(key) {
            let raw = existing.as_str().ok_or("Science 隔离路径必须是字符串")?;
            let expanded = raw
                .strip_prefix("~/")
                .map(|rest| sandbox.join(rest))
                .unwrap_or_else(|| PathBuf::from(raw));
            if expanded != expected {
                return Err("Science 配置的认证或运行环境路径不属于切换器；未覆盖原配置".into());
            }
        }
        document["paths"][key] = value(expected.to_str().ok_or("Science 隔离路径不是 UTF-8")?);
    }
    document["data_dir"] = value(data.to_str().ok_or("Science 数据路径不是 UTF-8")?);
    let bytes = document.to_string().into_bytes();
    // Avoid clobbering a configuration edit made during preparation.
    if checked_file_bytes(&config, 1024 * 1024, "Science 启动配置")? != prior {
        return Err("Science 启动配置在准备期间被修改，请重试".into());
    }
    let config_mode = fs::symlink_metadata(&config)
        .map(|metadata| metadata.mode() & 0o7777)
        .unwrap_or(0);
    if prior.as_deref() != Some(bytes.as_slice()) || config_mode != 0o600 {
        atomic_write(&config, &bytes)?;
    }
    let tools = sandbox.join(".csswitch-science-tools");
    reject_symlink_components(&tools)?;
    fs::create_dir_all(&tools).map_err(|_| "无法准备 Science 私有工具目录")?;
    fs::set_permissions(&tools, fs::Permissions::from_mode(0o700))
        .map_err(|_| "无法保护 Science 私有工具目录")?;
    let security = tools.join("security");
    reject_symlink_components(&security)?;
    atomic_write(&security, SCIENCE_HOST_SECURITY_WRAPPER.as_bytes())?;
    fs::set_permissions(&security, fs::Permissions::from_mode(0o500))
        .map_err(|_| "无法保护 Science 私有工具")?;
    Ok(ScienceHostHomeLaunch {
        config_sha256: format!("{:x}", Sha256::digest(&bytes)),
        security_sha256: format!(
            "{:x}",
            Sha256::digest(SCIENCE_HOST_SECURITY_WRAPPER.as_bytes())
        ),
    })
}

#[cfg(test)]
mod home_layout_tests {
    use super::*;

    #[test]
    fn home_access_requires_explicit_opt_in_and_revocation_is_non_mutating() {
        let root = fixture();
        let config = write_config(root.path(), "[ui]\ntheme = 'dark'\n");
        let original = fs::read(&config).unwrap();
        let mut isolated = Command::new("test-science");
        isolated.env_clear();
        configure_science_home_access(&mut isolated, root.path(), false).unwrap();
        assert_eq!(fs::read(&config).unwrap(), original);
        assert!(!root.path().join(".csswitch-science-tools").exists());
        assert_eq!(isolated.get_envs().count(), 0);

        let mut opted_in = Command::new("test-science");
        opted_in.env_clear();
        configure_science_home_access(&mut opted_in, root.path(), true).unwrap();
        assert!(opted_in.get_envs().any(|(key, value)| {
            key == "CSSWITCH_SCIENCE_USE_HOST_HOME" && value == Some(std::ffi::OsStr::new("1"))
        }));
        let prepared = fs::read(&config).unwrap();
        let mut revoked = Command::new("test-science");
        revoked.env_clear();
        configure_science_home_access(&mut revoked, root.path(), false).unwrap();
        assert_eq!(fs::read(&config).unwrap(), prepared);
        assert_eq!(revoked.get_envs().count(), 0);
    }

    struct Fixture(PathBuf);
    impl Fixture {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn fixture() -> Fixture {
        let root = Fixture(
            PathBuf::from("/private/tmp")
                .join(format!("csswitch-home-layout-{}", crate::config::new_id())),
        );
        fs::create_dir(root.path()).unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(root.path().join(".claude-science")).unwrap();
        root
    }
    fn write_config(root: &Path, text: &str) -> PathBuf {
        let path = root.join(".claude-science/config.toml");
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        path
    }
    #[test]
    fn home_layout_pins_state_and_preserves_preferences() {
        let root = fixture();
        let config = write_config(root.path(), "# user preference\nquiet_logs = true\nssh_hosts = ['lab']\n[paths]\nauth_dir = '~/.claude-science'\n");
        let prepared = prepare_science_host_home(root.path()).unwrap();
        let bytes = fs::read(&config).unwrap();
        assert_eq!(
            prepared.config_sha256,
            format!("{:x}", Sha256::digest(&bytes))
        );
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.contains("# user preference"));
        let doc = text.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(doc["quiet_logs"].as_bool(), Some(true));
        assert_eq!(doc["ssh_hosts"][0].as_str(), Some("lab"));
        assert_eq!(
            doc["paths"]["auth_dir"].as_str(),
            root.path().join(".claude-science").to_str()
        );
        assert_eq!(
            doc["paths"]["conda_home"].as_str(),
            root.path().join(".claude-science/conda").to_str()
        );
        prepare_science_host_home(root.path()).unwrap();
        assert_eq!(fs::read(config).unwrap(), bytes);
        assert_eq!(
            fs::metadata(root.path().join(".csswitch-science-tools/security"))
                .unwrap()
                .mode()
                & 0o777,
            0o500
        );
    }
    #[test]
    fn home_layout_rejects_invalid_and_foreign_state_without_overwriting() {
        for text in [
            "invalid = [",
            "paths = 3",
            "[paths]\nauth_dir = '/other/auth'",
            "[paths]\nconda_home = '/other/conda'",
        ] {
            let root = fixture();
            let config = write_config(root.path(), text);
            assert!(prepare_science_host_home(root.path()).is_err());
            assert_eq!(fs::read_to_string(config).unwrap(), text);
        }
    }
    #[test]
    fn home_layout_normalizes_unchanged_read_only_config() {
        let root = fixture();
        prepare_science_host_home(root.path()).unwrap();
        let config = root.path().join(".claude-science/config.toml");
        let before = fs::read(&config).unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o400)).unwrap();
        prepare_science_host_home(root.path()).unwrap();
        assert_eq!(fs::read(&config).unwrap(), before);
        assert_eq!(fs::metadata(&config).unwrap().mode() & 0o7777, 0o600);
    }

    #[test]
    fn home_layout_rejects_symlink_config() {
        let root = fixture();
        let other = fixture();
        std::os::unix::fs::symlink(
            other.path(),
            root.path().join(".claude-science/config.toml"),
        )
        .unwrap();
        assert!(prepare_science_host_home(root.path()).is_err());
    }
    #[test]
    fn home_layout_can_initialize_an_empty_isolated_root() {
        let root = fixture();
        assert!(prepare_science_host_home(root.path()).is_ok());
    }
}

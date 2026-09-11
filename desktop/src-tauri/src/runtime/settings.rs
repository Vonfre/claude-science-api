use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const SSH_STUB_MARKER: &str = "# CSSwitch managed system SSH config bridge v1";
const SSH_STUB_MARKER_V2: &str = "# CSSwitch managed system SSH config bridge v2";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ManagedSshStubSnapshot {
    bytes: Vec<u8>,
    device: u64,
    inode: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum ManagedSshStubBefore {
    Absent,
    Present(ManagedSshStubSnapshot),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManagedSshStubTransaction {
    before: ManagedSshStubBefore,
    candidate: Option<ManagedSshStubSnapshot>,
    expected_system_config: PathBuf,
    expected_hosts: Vec<String>,
}

fn managed_ssh_stub_text(text: &str, expected_system_config: &Path) -> bool {
    let escaped = expected_system_config
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    if text == format!("{SSH_STUB_MARKER}\nInclude \"{escaped}\"\n") {
        return true;
    }
    let lines = text.lines().collect::<Vec<_>>();
    lines.len() == 3
        && lines[0] == SSH_STUB_MARKER_V2
        && lines[1].strip_prefix("Host ").is_some_and(|hosts| {
            let aliases = hosts.split_ascii_whitespace().collect::<Vec<_>>();
            !aliases.is_empty()
                && aliases
                    .iter()
                    .all(|alias| crate::runtime::ssh_bridge::is_concrete_alias(alias))
        })
        && lines[2] == format!("Include \"{escaped}\"")
}

fn read_exact_v2_managed_stub(
    sandbox_home: &Path,
    expected_system_config: &Path,
    expected_hosts: &[String],
) -> Result<Option<ManagedSshStubSnapshot>, String> {
    read_managed_stub(sandbox_home, expected_system_config, expected_hosts, false)
}

// Only transaction admission/rollback may accept the exact legacy V1 entry.
// Running health and candidate ownership must still prove the expected V2 hosts.
fn read_managed_stub(
    sandbox_home: &Path,
    expected_system_config: &Path,
    expected_hosts: &[String],
    allow_legacy: bool,
) -> Result<Option<ManagedSshStubSnapshot>, String> {
    let ssh_dir = sandbox_home.join(".ssh");
    let dir_metadata = match std::fs::symlink_metadata(&ssh_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("隔离 SSH 配置目录状态无法安全确认".into()),
    };
    // SAFETY: geteuid has no preconditions and does not dereference pointers.
    let uid = unsafe { libc::geteuid() };
    if dir_metadata.file_type().is_symlink()
        || !dir_metadata.file_type().is_dir()
        || dir_metadata.uid() != uid
        || dir_metadata.mode() & 0o022 != 0
    {
        return Err("隔离 SSH 配置目录状态无法安全确认".into());
    }
    let config = ssh_dir.join("config");
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&config)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("隔离 SSH config 状态无法安全确认".into()),
    };
    let metadata = file
        .metadata()
        .map_err(|_| "隔离 SSH config 状态无法安全确认")?;
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.nlink() != 1
        || metadata.mode() & 0o7777 != 0o600
        || metadata.len() > 128 * 1024
    {
        return Err("隔离 SSH config 不是私有的 CSSwitch V2 普通文件".into());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)
        .map_err(|_| "隔离 SSH config 状态无法安全确认")?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| "隔离 SSH config 不是私有的 CSSwitch V2 普通文件")?;
    let expected_host_line = format!("Host {}", expected_hosts.join(" "));
    let lines = text.lines().collect::<Vec<_>>();
    if expected_hosts.is_empty()
        || !expected_hosts
            .iter()
            .all(|host| crate::runtime::ssh_bridge::is_concrete_alias(host))
        || !managed_ssh_stub_text(text, expected_system_config)
        || !((allow_legacy && lines.first().copied() == Some(SSH_STUB_MARKER))
            || (lines.first().copied() == Some(SSH_STUB_MARKER_V2)
                && lines.get(1).copied() == Some(expected_host_line.as_str())))
    {
        return Err("隔离 SSH config 不是当前操作的精确 CSSwitch V2 文件".into());
    }
    let named =
        std::fs::symlink_metadata(&config).map_err(|_| "隔离 SSH config 状态无法安全确认")?;
    if named.file_type().is_symlink()
        || named.dev() != metadata.dev()
        || named.ino() != metadata.ino()
    {
        return Err("隔离 SSH config 在检查期间发生变化".into());
    }
    Ok(Some(ManagedSshStubSnapshot {
        bytes,
        device: metadata.dev(),
        inode: metadata.ino(),
    }))
}

// Pin the directory for the whole recovery operation; never follow a replaced
// .ssh symlink when publishing or retaining an object during compensation.
fn ssh_recovery_dir(home: &Path) -> Result<std::fs::File, String> {
    let dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(home.join(".ssh"))
        .map_err(|_| "SSH 恢复目录无法安全打开")?;
    let meta = dir.metadata().map_err(|_| "SSH 恢复目录无法核验")?;
    // SAFETY: geteuid has no preconditions.
    if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o022 != 0 {
        return Err("SSH 恢复目录权限不安全".into());
    }
    Ok(dir)
}

fn ssh_recovery_open(
    dir: &std::fs::File,
    name: &str,
    flags: i32,
) -> Result<std::fs::File, std::io::Error> {
    let name = std::ffi::CString::new(name).unwrap();
    // SAFETY: directory fd is live, name is a NUL-terminated single component.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: openat returned a new owned descriptor.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

fn ssh_recovery_snapshot(
    dir: &std::fs::File,
    name: &str,
) -> Result<Option<ManagedSshStubSnapshot>, String> {
    let mut file = match ssh_recovery_open(dir, name, libc::O_RDONLY) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("SSH 恢复对象无法安全核验，已保留供人工检查".into()),
    };
    let meta = file.metadata().map_err(|_| "SSH 恢复对象无法核验")?;
    // SAFETY: geteuid has no preconditions.
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.nlink() != 1
        || meta.mode() & 0o7777 != 0o600
        || meta.len() > 128 * 1024
    {
        return Err("SSH 恢复对象不安全，已保留供人工检查".into());
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(128 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "SSH 恢复对象无法读取")?;
    Ok(Some(ManagedSshStubSnapshot {
        bytes,
        device: meta.dev(),
        inode: meta.ino(),
    }))
}

fn ssh_recovery_move(dir: &std::fs::File, from: &str, to: &str) -> Result<(), String> {
    let from = std::ffi::CString::new(from).unwrap();
    let to = std::ffi::CString::new(to).unwrap();
    #[cfg(target_os = "macos")]
    let result = {
        extern "C" {
            fn renameatx_np(
                a: i32,
                b: *const libc::c_char,
                c: i32,
                d: *const libc::c_char,
                flags: u32,
            ) -> i32;
        }
        // SAFETY: both names and the pinned directory fd remain live. EXCL
        // forbids overwriting a concurrently published destination.
        unsafe {
            renameatx_np(
                dir.as_raw_fd(),
                from.as_ptr(),
                dir.as_raw_fd(),
                to.as_ptr(),
                4,
            )
        }
    };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            dir.as_raw_fd(),
            from.as_ptr(),
            dir.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let result = -1;
    if result == 0 {
        Ok(())
    } else {
        Err("SSH 恢复入口发生竞争或无法提交；未覆盖现存文件".into())
    }
}

#[cfg(test)]
thread_local! {
    static SSH_RECOVERY_TEST_HOOK: std::cell::RefCell<Option<Box<dyn FnMut(&str) -> Result<(), String>>>> = std::cell::RefCell::new(None);
}

fn ssh_recovery_checkpoint(_phase: &str) -> Result<(), String> {
    #[cfg(test)]
    SSH_RECOVERY_TEST_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook(_phase)
        } else {
            Ok(())
        }
    })?;
    Ok(())
}

fn ssh_recovery_sync(dir: &std::fs::File) -> Result<(), String> {
    ssh_recovery_checkpoint("sync")?;
    dir.sync_all()
        .map_err(|_| "旧版 SSH 入口恢复持久化未确认".into())
}

fn ssh_recovery_finish(
    home: &Path,
    dir: &std::fs::File,
    before: &ManagedSshStubSnapshot,
) -> Result<(), String> {
    let named_dir = ssh_recovery_dir(home)?;
    let expected = dir.metadata().map_err(|_| "SSH 恢复目录无法核验")?;
    let named = named_dir.metadata().map_err(|_| "SSH 恢复目录无法核验")?;
    if expected.dev() != named.dev() || expected.ino() != named.ino() {
        return Err("SSH 恢复目录已变化，恢复状态未确认".into());
    }
    if !ssh_recovery_snapshot(dir, "config")?.is_some_and(|current| current.bytes == before.bytes) {
        return Err("SSH 恢复入口已变化，恢复状态未确认".into());
    }
    ssh_recovery_open(dir, "config", libc::O_RDONLY)
        .and_then(|file| file.sync_all())
        .map_err(|_| "SSH 恢复文件持久化未确认")?;
    ssh_recovery_sync(dir)
}

impl ManagedSshStubTransaction {
    pub(crate) fn capture(sandbox_home: &Path, expected_hosts: &[String]) -> Result<Self, String> {
        let expected_system_config = system_ssh_config_path()?;
        let before =
            match read_managed_stub(sandbox_home, &expected_system_config, expected_hosts, true)? {
                Some(snapshot) => ManagedSshStubBefore::Present(snapshot),
                None => ManagedSshStubBefore::Absent,
            };
        Ok(Self {
            before,
            candidate: None,
            expected_system_config,
            expected_hosts: expected_hosts.to_vec(),
        })
    }

    pub(crate) fn validate_prepared_hosts(&self, prepared_hosts: &[String]) -> Result<(), String> {
        if self.expected_hosts != prepared_hosts {
            return Err(
                "系统 SSH Host authority 在预检后发生变化；请重试（code=ssh_authority_changed_retry）"
                    .into(),
            );
        }
        Ok(())
    }

    pub(crate) fn observe_after_launch(&mut self, sandbox_home: &Path) {
        self.candidate = read_exact_v2_managed_stub(
            sandbox_home,
            &self.expected_system_config,
            &self.expected_hosts,
        )
        .ok()
        .flatten();
    }

    pub(crate) fn compensate_with_authority_bypass(
        &self,
        sandbox_home: &Path,
        bypass: &crate::config::AuthorityWriterBypass<'_>,
    ) -> Result<(), String> {
        let _authority_guard = crate::config::authority_writer_guard_from_bypass(bypass);
        self.compensate_unfenced(sandbox_home)
    }

    fn compensate_unfenced(&self, sandbox_home: &Path) -> Result<(), String> {
        // A retained object is checked on every replay, including the already
        // restored branch. Unknown objects never silently clear the journal.
        if let Some(candidate) = &self.candidate {
            if let ManagedSshStubBefore::Present(before) = &self.before {
                if before.bytes.starts_with(SSH_STUB_MARKER.as_bytes()) {
                    let dir = ssh_recovery_dir(sandbox_home)?;
                    let retained = format!(
                        ".csswitch-ssh-recovery-{}-{}",
                        candidate.device, candidate.inode
                    );
                    if let Some(snapshot) = ssh_recovery_snapshot(&dir, &retained)? {
                        if &snapshot != candidate {
                            return Err("SSH 恢复对象归属变化，已保留供人工检查".into());
                        }
                    }
                }
            }
        }
        let current = read_managed_stub(
            sandbox_home,
            &self.expected_system_config,
            &self.expected_hosts,
            true,
        );
        match &self.before {
            ManagedSshStubBefore::Absent => {
                let Some(candidate) = self.candidate.as_ref() else {
                    return Ok(());
                };
                let current = current?;
                let Some(current) = current else {
                    return Ok(());
                };
                if current.device != candidate.device
                    || current.inode != candidate.inode
                    || current.bytes != candidate.bytes
                {
                    return Err("隔离 SSH config 已不是本次事务创建的精确文件，拒绝删除".into());
                }
                std::fs::remove_file(sandbox_home.join(".ssh/config"))
                    .map_err(|error| format!("撤销本次事务创建的隔离 SSH config 失败：{error}"))?;
                let _ = std::fs::remove_dir(sandbox_home.join(".ssh"));
                Ok(())
            }
            ManagedSshStubBefore::Present(before) => match current? {
                Some(current) if current.bytes == before.bytes => {
                    ssh_recovery_finish(sandbox_home, &ssh_recovery_dir(sandbox_home)?, before)
                }
                Some(current) => {
                    // The launch script upgrades V1 to V2. Restore only the exact
                    // V2 candidate observed by this transaction, never a foreign file.
                    if self.candidate.as_ref() != Some(&current) {
                        return Err("隔离 SSH config 已发生外部变化，拒绝覆盖".into());
                    }
                    self.restore_legacy_before(sandbox_home, before, &current)
                }
                None => {
                    let dir = ssh_recovery_dir(sandbox_home)?;
                    let mut file = ssh_recovery_open(
                        &dir,
                        "config",
                        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                    )
                    .map_err(|_| "隔离 SSH config 缺失且无法安全恢复")?;
                    file.write_all(&before.bytes)
                        .and_then(|_| file.sync_all())
                        .map_err(|_| "隔离 SSH config 缺失且无法安全恢复")?;
                    ssh_recovery_finish(sandbox_home, &dir, before)
                }
            },
        }
    }

    fn restore_legacy_before(
        &self,
        sandbox_home: &Path,
        before: &ManagedSshStubSnapshot,
        candidate: &ManagedSshStubSnapshot,
    ) -> Result<(), String> {
        let text = std::str::from_utf8(&before.bytes).map_err(|_| "旧版 SSH 入口快照无效")?;
        if text.lines().next() != Some(SSH_STUB_MARKER)
            || !managed_ssh_stub_text(text, &self.expected_system_config)
        {
            return Err("隔离 SSH config 已发生外部变化，拒绝覆盖".into());
        }
        let dir = ssh_recovery_dir(sandbox_home)?;
        let retained = format!(
            ".csswitch-ssh-recovery-{}-{}",
            candidate.device, candidate.inode
        );
        let temporary = format!(".csswitch-restore-{}", crate::config::new_id());
        let mut file = ssh_recovery_open(
            &dir,
            &temporary,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        )
        .map_err(|_| "无法准备旧版 SSH 入口恢复文件")?;
        file.write_all(&before.bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| "无法写入旧版 SSH 入口恢复文件")?;
        if ssh_recovery_snapshot(&dir, "config")?.as_ref() != Some(candidate) {
            return Err("隔离 SSH config 已发生外部变化，拒绝覆盖".into());
        }
        ssh_recovery_checkpoint("before_move")?;
        // Keep the moved object, even after successful recovery. It can have
        // been replaced/edited after our check; never unlink an unknown object.
        ssh_recovery_move(&dir, "config", &retained)?;
        ssh_recovery_sync(&dir)?;
        if ssh_recovery_snapshot(&dir, &retained)?.as_ref() != Some(candidate) {
            // Best effort restore the foreign object to its original name, but
            // never overwrite a new config. Either location preserves it.
            let _ = ssh_recovery_move(&dir, &retained, "config");
            ssh_recovery_sync(&dir)?;
            return Err("隔离 SSH config 在恢复时发生外部变化，原文件已保留".into());
        }
        ssh_recovery_checkpoint("before_publish")?;
        ssh_recovery_move(&dir, &temporary, "config")?;
        ssh_recovery_checkpoint("after_publish")?;
        ssh_recovery_finish(sandbox_home, &dir, before)
    }

    pub(crate) fn compensate_durable_with_authority_bypass(
        &self,
        sandbox_home: &Path,
        bypass: &crate::config::AuthorityWriterBypass<'_>,
    ) -> Result<(), String> {
        let _authority_guard = crate::config::authority_writer_guard_from_bypass(bypass);
        self.compensate_durable_unfenced(sandbox_home)
    }

    fn compensate_durable_unfenced(&self, sandbox_home: &Path) -> Result<(), String> {
        if self.expected_system_config != system_ssh_config_path()?
            || self.expected_hosts.is_empty()
            || !self
                .expected_hosts
                .iter()
                .all(|host| crate::runtime::ssh_bridge::is_concrete_alias(host))
        {
            return Err("durable SSH stub transaction authority drifted or retargeted".into());
        }
        self.compensate_unfenced(sandbox_home)
    }
}

fn system_ssh_config_path_for_home(home: &Path) -> Result<PathBuf, String> {
    if !home.is_absolute() {
        return Err("无法确认系统 HOME，不能启用系统 SSH 配置。".into());
    }
    let config = home.join(".ssh").join("config");
    if !config.is_file() {
        return Err("未找到系统 ~/.ssh/config，不能启用系统 SSH 配置。".into());
    }
    Ok(config)
}

pub(crate) fn system_ssh_config_path() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("无法确认系统 HOME，不能启用系统 SSH 配置。")?;
    system_ssh_config_path_for_home(&home)
}

pub(crate) fn managed_sandbox_ssh_stub_path(sandbox_home: &Path) -> PathBuf {
    sandbox_home.join(".ssh/config")
}

pub(crate) fn remove_managed_sandbox_ssh_stub_exact(
    sandbox_home: &Path,
    expected: &crate::commands::runtime::config_mutation::AssetIdentity,
) -> Result<(), String> {
    let _authority_guard = crate::config::acquire_authority_writer_guard()
        .map_err(|error| format!("authority writer fence failed: {error}"))?;
    remove_managed_sandbox_ssh_stub_unfenced(sandbox_home, Some(expected))
}

pub(crate) fn remove_managed_sandbox_ssh_stub_with_authority_bypass(
    sandbox_home: &Path,
    bypass: &crate::config::AuthorityWriterBypass<'_>,
) -> Result<(), String> {
    let _authority_guard = crate::config::authority_writer_guard_from_bypass(bypass);
    remove_managed_sandbox_ssh_stub_unfenced(sandbox_home, None)
}

fn remove_managed_sandbox_ssh_stub_unfenced(
    sandbox_home: &Path,
    expected: Option<&crate::commands::runtime::config_mutation::AssetIdentity>,
) -> Result<(), String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("无法确认系统 HOME，不能撤销系统 SSH 配置。")?;
    if !home.is_absolute() {
        return Err("无法确认系统 HOME，不能撤销系统 SSH 配置。".into());
    }
    remove_managed_sandbox_ssh_stub_for_config(sandbox_home, &home.join(".ssh/config"), expected)
}

pub(crate) fn validate_managed_sandbox_ssh_stub(
    sandbox_home: &Path,
    expected_hosts: &[String],
) -> Result<(), String> {
    let expected_system_config = system_ssh_config_path()?;
    validate_managed_sandbox_ssh_stub_for_config(
        sandbox_home,
        &expected_system_config,
        expected_hosts,
    )
}

pub(crate) fn prevalidate_sandbox_ssh_stub(
    sandbox_home: &Path,
    expected_hosts: &[String],
    enabled: bool,
) -> Result<(), String> {
    let ssh_dir = sandbox_home.join(".ssh");
    let dir_metadata = match std::fs::symlink_metadata(&ssh_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("隔离 SSH config 不是 CSSwitch 管理的安全入口".into()),
    };
    // SAFETY: geteuid has no preconditions and does not dereference pointers.
    let uid = unsafe { libc::geteuid() };
    if dir_metadata.file_type().is_symlink()
        || !dir_metadata.file_type().is_dir()
        || dir_metadata.uid() != uid
        || dir_metadata.mode() & 0o022 != 0
    {
        return Err("隔离 SSH config 不是 CSSwitch 管理的安全入口".into());
    }
    let config = ssh_dir.join("config");
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&config)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("隔离 SSH config 不是 CSSwitch 管理的安全入口".into()),
    };
    let metadata = file
        .metadata()
        .map_err(|_| "隔离 SSH config 不是 CSSwitch 管理的安全入口")?;
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.mode() & 0o077 != 0
        || metadata.len() > 128 * 1024
    {
        return Err("隔离 SSH config 不是 CSSwitch 管理的安全入口".into());
    }
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|_| "隔离 SSH config 不是 CSSwitch 管理的安全入口")?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("无法确认系统 HOME，不能预检现有隔离 SSH config。")?;
    if !home.is_absolute() {
        return Err("无法确认系统 HOME，不能预检现有隔离 SSH config。".into());
    }
    let expected_system_config = home.join(".ssh/config");
    if !managed_ssh_stub_text(&text, &expected_system_config) {
        return Err("隔离 SSH config 不是 CSSwitch 管理的安全入口".into());
    }
    if enabled {
        if expected_hosts.is_empty()
            || !expected_hosts
                .iter()
                .all(|host| crate::runtime::ssh_bridge::is_concrete_alias(host))
        {
            return Err("没有可供 Science 校验的安全 SSH Host alias".into());
        }
        let expected_host_line = format!("Host {}", expected_hosts.join(" "));
        let lines = text.lines().collect::<Vec<_>>();
        if lines.first().copied() != Some(SSH_STUB_MARKER)
            && (lines.first().copied() != Some(SSH_STUB_MARKER_V2)
                || lines.get(1).copied() != Some(expected_host_line.as_str()))
        {
            return Err("隔离 SSH config 不是 CSSwitch 管理的安全入口".into());
        }
    }
    Ok(())
}

/// Read-only P2-B admission for removing the isolated managed stub.  The
/// caller gets a boolean only after the exact CSSwitch marker and private
/// regular-file checks have passed; foreign, malformed or ambiguous files
/// are never treated as absent.
pub(crate) fn preflight_managed_sandbox_ssh_stub_cleanup(
    sandbox_home: &Path,
) -> Result<bool, String> {
    prevalidate_sandbox_ssh_stub(sandbox_home, &[], false)?;
    let config = sandbox_home.join(".ssh/config");
    match std::fs::symlink_metadata(&config) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => Err("隔离 SSH config 不是安全普通文件".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("无法确认隔离 SSH config 状态".into()),
    }
}

fn validate_managed_sandbox_ssh_stub_for_config(
    sandbox_home: &Path,
    expected_system_config: &Path,
    expected_hosts: &[String],
) -> Result<(), String> {
    if expected_hosts.is_empty()
        || !expected_hosts
            .iter()
            .all(|host| crate::runtime::ssh_bridge::is_concrete_alias(host))
    {
        return Err("没有可供 Science 校验的安全 SSH Host alias".into());
    }
    let ssh_dir = sandbox_home.join(".ssh");
    let dir_metadata = std::fs::symlink_metadata(&ssh_dir)
        .map_err(|_| "隔离 SSH 配置目录缺失，拒绝复用运行中的 Science")?;
    // SAFETY: geteuid has no preconditions and does not dereference pointers.
    let uid = unsafe { libc::geteuid() };
    if !dir_metadata.file_type().is_dir()
        || dir_metadata.file_type().is_symlink()
        || dir_metadata.uid() != uid
        || dir_metadata.mode() & 0o022 != 0
    {
        return Err("隔离 SSH 配置目录不安全，拒绝复用运行中的 Science".into());
    }
    let config = ssh_dir.join("config");
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&config)
        .map_err(|_| "隔离 SSH config 缺失或不安全，拒绝复用运行中的 Science")?;
    let metadata = file.metadata().map_err(|_| "无法检查隔离 SSH config")?;
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.mode() & 0o077 != 0
        || metadata.len() > 128 * 1024
    {
        return Err("隔离 SSH config 不是安全的 CSSwitch 管理文件".into());
    }
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|_| "无法读取隔离 SSH config")?;
    let expected_host_line = format!("Host {}", expected_hosts.join(" "));
    let lines = text.lines().collect::<Vec<_>>();
    if !managed_ssh_stub_text(&text, expected_system_config)
        || lines.first().copied() != Some(SSH_STUB_MARKER_V2)
        || lines.get(1).copied() != Some(expected_host_line.as_str())
    {
        return Err("隔离 SSH config 与当前 CSSwitch SSH bridge 不一致".into());
    }
    Ok(())
}

fn remove_managed_sandbox_ssh_stub_for_config(
    sandbox_home: &Path,
    expected_system_config: &Path,
    expected_identity: Option<&crate::commands::runtime::config_mutation::AssetIdentity>,
) -> Result<(), String> {
    let mut ancestor = Some(sandbox_home);
    for _ in 0..3 {
        let Some(path) = ancestor else { break };
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("隔离 SSH 配置路径包含符号链接，拒绝撤销授权".into())
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("检查隔离 SSH 配置路径失败：{error}")),
        }
        ancestor = path.parent();
    }
    let ssh_dir = sandbox_home.join(".ssh");
    let dir_metadata = match std::fs::symlink_metadata(&ssh_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return if expected_identity.is_some() {
                Err("隔离 SSH stub parent 在 exact 撤销前消失".into())
            } else {
                Ok(())
            }
        }
        Err(error) => return Err(format!("检查隔离 SSH 配置目录失败：{error}")),
    };
    // SAFETY: geteuid has no preconditions and does not dereference pointers.
    let uid = unsafe { libc::geteuid() };
    if !dir_metadata.file_type().is_dir() || dir_metadata.uid() != uid {
        return Err("隔离 SSH 配置目录不安全，拒绝撤销授权".into());
    }
    let config = ssh_dir.join("config");
    if let Some(expected) = expected_identity {
        if crate::commands::runtime::config_mutation::capture_asset_identity(&config)?.as_ref()
            != Some(expected)
        {
            return Err("隔离 SSH stub identity 在撤销前变化".into());
        }
    }
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&config)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let _ = std::fs::remove_dir(&ssh_dir);
            return if expected_identity.is_some() {
                Err("隔离 SSH stub 在 exact 撤销前消失".into())
            } else {
                Ok(())
            };
        }
        Err(error) => return Err(format!("检查隔离 SSH config 失败：{error}")),
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("检查隔离 SSH config 失败：{error}"))?;
    if !metadata.is_file() || metadata.uid() != uid || metadata.len() > 128 * 1024 {
        return Err("隔离 SSH config 不是 CSSwitch 管理的安全普通文件".into());
    }
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|error| format!("读取隔离 SSH config 失败：{error}"))?;
    if !managed_ssh_stub_text(&text, expected_system_config) {
        return Err("隔离 SSH config 不是 CSSwitch 管理的入口，拒绝删除".into());
    }
    if let Some(expected) = expected_identity {
        if crate::commands::runtime::config_mutation::capture_asset_identity(&config)?.as_ref()
            != Some(expected)
        {
            return Err("隔离 SSH stub identity 在 unlink 前变化".into());
        }
    }
    std::fs::remove_file(&config).map_err(|error| format!("撤销隔离 SSH config 失败：{error}"))?;
    #[cfg(test)]
    if test_take_stub_directory_sync_fault() {
        return Err("test-only managed SSH stub directory sync failure".into());
    }
    let ssh_dir_handle = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&ssh_dir)
        .map_err(|error| format!("打开隔离 SSH 目录进行同步失败：{error}"))?;
    ssh_dir_handle
        .sync_all()
        .map_err(|error| format!("同步隔离 SSH 目录失败：{error}"))?;
    crate::commands::runtime::config_mutation::require_asset_absent(&config)?;
    if std::fs::remove_dir(&ssh_dir).is_ok() {
        let sandbox_handle = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(sandbox_home)
            .map_err(|error| format!("打开 sandbox HOME 进行同步失败：{error}"))?;
        sandbox_handle
            .sync_all()
            .map_err(|error| format!("同步 sandbox HOME 失败：{error}"))?;
    }
    Ok(())
}

#[cfg(test)]
fn test_stub_directory_sync_fault() -> &'static std::sync::Mutex<Option<std::thread::ThreadId>> {
    static FAULT: std::sync::OnceLock<std::sync::Mutex<Option<std::thread::ThreadId>>> =
        std::sync::OnceLock::new();
    FAULT.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(test)]
fn test_arm_stub_directory_sync_failure() {
    *test_stub_directory_sync_fault()
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(std::thread::current().id());
}

#[cfg(test)]
fn test_take_stub_directory_sync_fault() -> bool {
    let mut fault = test_stub_directory_sync_fault()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if fault.as_ref() == Some(&std::thread::current().id()) {
        *fault = None;
        true
    } else {
        false
    }
}

pub(crate) fn validate_runtime_ports(proxy_port: u16, sandbox_port: u16) -> Result<(), String> {
    crate::config::validate_runtime_ports(proxy_port, sandbox_port)?;
    let preview_port = sandbox_port
        .checked_add(1)
        .ok_or("沙箱端口必须小于 65535，才能分配隔离预览端口。")?;
    if preview_port == 8765 {
        return Err("沙箱预览端口会命中真实 Science 保留端口 8765。".into());
    }
    if preview_port == proxy_port {
        return Err("代理端口不能与沙箱预览端口相同。".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    use super::{
        remove_managed_sandbox_ssh_stub_for_config, system_ssh_config_path_for_home,
        test_arm_stub_directory_sync_failure, validate_managed_sandbox_ssh_stub_for_config,
        validate_runtime_ports, ManagedSshStubBefore, ManagedSshStubTransaction, SSH_STUB_MARKER,
        SSH_STUB_MARKER_V2,
    };

    #[test]
    fn validate_runtime_ports_rejects_reserved_real_science_port() {
        assert!(validate_runtime_ports(8765, 18991).is_err());
        assert!(validate_runtime_ports(18991, 8765).is_err());
    }

    #[test]
    fn validate_runtime_ports_rejects_zero_and_same_port() {
        assert!(validate_runtime_ports(0, 18991).is_err());
        assert!(validate_runtime_ports(18991, 0).is_err());
        assert!(validate_runtime_ports(18991, 18991).is_err());
        assert!(validate_runtime_ports(8991, 8990).is_err());
        assert!(validate_runtime_ports(18991, 8764).is_err());
        assert!(validate_runtime_ports(18991, u16::MAX).is_err());
        assert!(
            crate::config::validate_runtime_ports(8991, 8990).is_ok(),
            "legacy config must remain readable so the UI can repair it"
        );
    }

    #[test]
    fn validate_runtime_ports_accepts_distinct_nonreserved_ports() {
        assert!(validate_runtime_ports(18991, 18992).is_ok());
    }

    #[test]
    fn system_ssh_config_requires_an_absolute_home_and_regular_target() {
        assert!(system_ssh_config_path_for_home(std::path::Path::new("relative-home")).is_err());
        let home = std::env::temp_dir().join(format!(
            "csswitch-system-ssh-config-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        assert!(system_ssh_config_path_for_home(&home).is_err());
        std::fs::write(home.join(".ssh/config"), "Host test\n").unwrap();
        assert_eq!(
            system_ssh_config_path_for_home(&home).unwrap(),
            home.join(".ssh/config")
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn managed_sandbox_ssh_stub_is_revoked_without_touching_foreign_files() {
        let home = std::env::temp_dir().join(format!(
            "csswitch-system-ssh-stub-test-{}",
            crate::config::new_id()
        ));
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let config = home.join(".ssh/config");
        let expected_system_config = home.join("real-home/.ssh/config");
        std::fs::write(
            &config,
            format!(
                "{SSH_STUB_MARKER}\nInclude \"{}\"\n",
                expected_system_config.display()
            ),
        )
        .unwrap();
        remove_managed_sandbox_ssh_stub_for_config(&home, &expected_system_config, None).unwrap();
        assert!(!config.exists());

        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(
            &config,
            format!("{SSH_STUB_MARKER}\nInclude \"/different/config\"\n\nHost foreign\n"),
        )
        .unwrap();
        assert!(
            remove_managed_sandbox_ssh_stub_for_config(&home, &expected_system_config, None)
                .is_err()
        );
        assert!(std::fs::read_to_string(&config)
            .unwrap()
            .contains("Host foreign"));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn p2b_exact_stub_revoke_propagates_directory_sync_failure() {
        let home = std::env::temp_dir().join(format!(
            "csswitch-p2b-ssh-stub-sync-test-{}",
            crate::config::new_id()
        ));
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let config = home.join(".ssh/config");
        let expected_system_config = home.join("real-home/.ssh/config");
        std::fs::write(
            &config,
            format!(
                "{SSH_STUB_MARKER}\nInclude \"{}\"\n",
                expected_system_config.display()
            ),
        )
        .unwrap();
        let identity = crate::commands::runtime::config_mutation::capture_asset_identity(&config)
            .unwrap()
            .unwrap();
        test_arm_stub_directory_sync_failure();
        let error = remove_managed_sandbox_ssh_stub_for_config(
            &home,
            &expected_system_config,
            Some(&identity),
        )
        .unwrap_err();
        assert!(error.contains("test-only managed SSH stub directory sync failure"));
        assert!(!config.exists());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn running_stub_validation_requires_exact_v2_aliases_and_private_file() {
        let home = std::env::temp_dir().join(format!(
            "csswitch-system-ssh-running-stub-test-{}",
            crate::config::new_id()
        ));
        let ssh_dir = home.join(".ssh");
        std::fs::create_dir_all(&ssh_dir).unwrap();
        let expected_system_config = home.join("real-home/.ssh/config");
        let config = ssh_dir.join("config");
        std::fs::write(
            &config,
            format!(
                "{SSH_STUB_MARKER_V2}\nHost alpha beta\nInclude \"{}\"\n",
                expected_system_config.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
        let expected = vec!["alpha".to_string(), "beta".to_string()];
        assert!(validate_managed_sandbox_ssh_stub_for_config(
            &home,
            &expected_system_config,
            &expected
        )
        .is_ok());
        assert!(validate_managed_sandbox_ssh_stub_for_config(
            &home,
            &expected_system_config,
            &["alpha".to_string()]
        )
        .is_err());
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(validate_managed_sandbox_ssh_stub_for_config(
            &home,
            &expected_system_config,
            &expected
        )
        .is_err());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn ssh_stub_transaction_rejects_host_proof_drift_before_launch() {
        let home = std::env::temp_dir().join(format!(
            "csswitch-system-ssh-transaction-drift-test-{}",
            crate::config::new_id()
        ));
        let ssh_dir = home.join(".ssh");
        std::fs::create_dir_all(&ssh_dir).unwrap();
        std::fs::set_permissions(&ssh_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let expected_system_config = home.join("real-home/.ssh/config");
        let config = ssh_dir.join("config");
        std::fs::write(
            &config,
            format!(
                "{SSH_STUB_MARKER_V2}\nHost beta\nInclude \"{}\"\n",
                expected_system_config.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();

        let transaction = ManagedSshStubTransaction {
            before: ManagedSshStubBefore::Absent,
            candidate: None,
            expected_system_config,
            expected_hosts: vec!["alpha".to_string()],
        };

        assert!(
            transaction
                .validate_prepared_hosts(&["beta".to_string()])
                .is_err(),
            "a beta host proof must be rejected before an alpha transaction launches"
        );
        assert!(transaction
            .validate_prepared_hosts(&["alpha".to_string()])
            .is_ok());
        assert!(config.is_file(), "the proof check must not mutate the stub");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn legacy_ssh_admission_and_rollback_preserve_foreign_files() {
        let home =
            std::env::temp_dir().join(format!("csswitch-v1-stub-{}", crate::config::new_id()));
        let dir = home.join(".ssh");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = dir.join("config");
        let expected = home.join("system/.ssh/config");
        let hosts = vec!["alpha".to_string()];
        let legacy = format!("{SSH_STUB_MARKER}\nInclude \"{}\"\n", expected.display());
        let current = format!(
            "{SSH_STUB_MARKER_V2}\nHost alpha\nInclude \"{}\"\n",
            expected.display()
        );
        let write = |text: &str| {
            std::fs::write(&config, text).unwrap();
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
        };
        write(&legacy);
        assert!(super::read_exact_v2_managed_stub(&home, &expected, &hosts).is_err());
        let before = super::read_managed_stub(&home, &expected, &hosts, true)
            .unwrap()
            .unwrap();
        let mut tx = ManagedSshStubTransaction {
            before: ManagedSshStubBefore::Present(before),
            candidate: None,
            expected_system_config: expected.clone(),
            expected_hosts: hosts.clone(),
        };
        // No launch mutation: compensation must be an idempotent no-op.
        tx.compensate_unfenced(&home).unwrap();
        write(&current);
        tx.observe_after_launch(&home);
        let durable: ManagedSshStubTransaction =
            serde_json::from_str(&serde_json::to_string(&tx).unwrap()).unwrap();
        durable.compensate_unfenced(&home).unwrap();
        assert_eq!(std::fs::read(&config).unwrap(), legacy.as_bytes());
        durable.compensate_unfenced(&home).unwrap();
        write(&current);
        tx.observe_after_launch(&home);
        // Replacing the file with identical bytes still loses candidate ownership.
        std::fs::rename(&config, dir.join("previous-candidate")).unwrap();
        write(&current);
        assert!(tx.compensate_unfenced(&home).is_err());
        assert_eq!(std::fs::read(&config).unwrap(), current.as_bytes());
        for foreign in [
            "Host private\n",
            &format!("{legacy}ProxyCommand false\n"),
            &legacy.replace("system/.ssh/config", "foreign/.ssh/config"),
        ] {
            write(foreign);
            assert!(super::read_managed_stub(&home, &expected, &hosts, true).is_err());
            assert!(tx.compensate_unfenced(&home).is_err());
            assert_eq!(std::fs::read(&config).unwrap(), foreign.as_bytes());
        }
        std::fs::remove_file(&config).unwrap();
        std::os::unix::fs::symlink(dir.join("previous-candidate"), &config).unwrap();
        assert!(super::read_managed_stub(&home, &expected, &hosts, true).is_err());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn legacy_ssh_recovery_races_and_interrupted_sync() {
        for phase in ["before_move", "before_publish", "after_publish"] {
            let home =
                std::env::temp_dir().join(format!("csswitch-v1-race-{}", crate::config::new_id()));
            let dir = home.join(".ssh");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            let config = dir.join("config");
            let expected = home.join("system/.ssh/config");
            let hosts = vec!["alpha".to_string()];
            let legacy = format!("{SSH_STUB_MARKER}\nInclude \"{}\"\n", expected.display());
            let v2 = format!(
                "{SSH_STUB_MARKER_V2}\nHost alpha\nInclude \"{}\"\n",
                expected.display()
            );
            std::fs::write(&config, &legacy).unwrap();
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
            let before = super::read_managed_stub(&home, &expected, &hosts, true)
                .unwrap()
                .unwrap();
            let mut tx = ManagedSshStubTransaction {
                before: ManagedSshStubBefore::Present(before),
                candidate: None,
                expected_system_config: expected,
                expected_hosts: hosts,
            };
            std::fs::write(&config, &v2).unwrap();
            tx.observe_after_launch(&home);
            let target = config.clone();
            super::SSH_RECOVERY_TEST_HOOK.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move |point| {
                    if point != phase {
                        return Ok(());
                    }
                    if phase == "after_publish" {
                        return Err("injected interruption before directory sync".into());
                    }
                    let foreign = target.with_file_name("incoming-foreign");
                    std::fs::write(&foreign, b"foreign preserved\n").unwrap();
                    std::fs::set_permissions(&foreign, std::fs::Permissions::from_mode(0o600))
                        .unwrap();
                    std::fs::rename(&foreign, &target).unwrap();
                    Ok(())
                }))
            });
            let result = tx.compensate_unfenced(&home);
            super::SSH_RECOVERY_TEST_HOOK.with(|hook| *hook.borrow_mut() = None);
            assert!(result.is_err(), "{phase} must not claim recovery success");
            let replay: ManagedSshStubTransaction =
                serde_json::from_str(&serde_json::to_string(&tx).unwrap()).unwrap();
            if phase == "after_publish" {
                assert_eq!(std::fs::read(&config).unwrap(), legacy.as_bytes());
                super::SSH_RECOVERY_TEST_HOOK.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(|point| {
                        if point == "sync" {
                            Err("injected directory sync failure".into())
                        } else {
                            Ok(())
                        }
                    }))
                });
                let failed_sync = replay.compensate_unfenced(&home);
                super::SSH_RECOVERY_TEST_HOOK.with(|hook| *hook.borrow_mut() = None);
                assert!(
                    failed_sync.is_err(),
                    "replay must not bypass directory durability"
                );
                replay.compensate_unfenced(&home).unwrap();
            } else {
                assert_eq!(std::fs::read(&config).unwrap(), b"foreign preserved\n");
                assert!(replay.compensate_unfenced(&home).is_err());
                assert_eq!(std::fs::read(&config).unwrap(), b"foreign preserved\n");
            }
            std::fs::remove_dir_all(home).unwrap();
        }
    }

    #[test]
    fn sandbox_ssh_revocation_rejects_fifo_and_symlinked_home_without_blocking() {
        let base = std::env::temp_dir().join(format!(
            "csswitch-system-ssh-special-test-{}",
            crate::config::new_id()
        ));
        let home = base.join("home");
        let expected = base.join("real/.ssh/config");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let fifo = home.join(".ssh/config");
        let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: fifo_c is a valid NUL-terminated path and mode is conventional.
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        assert!(remove_managed_sandbox_ssh_stub_for_config(&home, &expected, None).is_err());
        assert!(std::fs::symlink_metadata(&fifo)
            .unwrap()
            .file_type()
            .is_fifo());

        let outside = base.join("outside");
        std::fs::create_dir_all(outside.join(".ssh")).unwrap();
        std::fs::write(
            outside.join(".ssh/config"),
            format!("{SSH_STUB_MARKER}\nInclude \"{}\"\n", expected.display()),
        )
        .unwrap();
        let linked_home = base.join("linked-home");
        std::os::unix::fs::symlink(&outside, &linked_home).unwrap();
        assert!(remove_managed_sandbox_ssh_stub_for_config(&linked_home, &expected, None).is_err());
        assert!(outside.join(".ssh/config").is_file());
        let _ = std::fs::remove_dir_all(base);
    }
}

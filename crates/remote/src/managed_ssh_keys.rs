use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use gpui::{AppContext as _, AsyncApp};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use util::command::Stdio;
use uuid::Uuid;

use crate::SshConnectionOptions;

const MANIFEST_VERSION: u32 = 1;
const KEY_DIRECTORY_NAME: &str = "ssh/keys";
const MANIFEST_FILE_NAME: &str = "manifest.json";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedSshKey {
    pub key_id: String,
    pub host: String,
    pub port: u16,
    pub requested_username: Option<String>,
    #[serde(default)]
    pub connection_args: Vec<String>,
    #[serde(default)]
    pub ssh_destination: String,
    pub remote_username: String,
    pub private_key_file: String,
    pub public_key: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub deployment_state: ManagedSshKeyDeploymentState,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedSshKeyDeploymentState {
    Pending,
    Verified,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ManagedSshKeyManifest {
    version: u32,
    client_id: String,
    client_name: String,
    keys: Vec<ManagedSshKey>,
}

pub struct GeneratedManagedSshKey {
    pub record: ManagedSshKey,
    pub private_key_path: PathBuf,
}

pub fn managed_ssh_key_directory() -> PathBuf {
    paths::data_dir().join(KEY_DIRECTORY_NAME)
}

pub fn list_managed_ssh_keys() -> Result<Vec<ManagedSshKey>> {
    Ok(load_or_create_manifest()?.keys)
}

/// Adds the stored Zed identity for this target to `options`, if one exists.
///
/// The manifest lookup touches the filesystem, so it runs on the background
/// executor. This is the first thing every SSH connection does, and a blocking
/// read on GPUI's foreground executor would delay the connection for a whole
/// round trip.
pub async fn apply_managed_identity(
    options: &mut SshConnectionOptions,
    cx: &AsyncApp,
) -> Result<Option<ManagedSshKey>> {
    let host = normalize_host(&options.host.to_string());
    let port = options.port.unwrap_or(22);
    let username = options.username.clone();
    let identity = cx
        .background_spawn(async move {
            load_managed_identity(
                &managed_ssh_key_directory(),
                &host,
                port,
                username.as_deref(),
            )
        })
        .await?;
    let Some((key, private_key_path)) = identity else {
        return Ok(None);
    };

    let arguments = options.args.get_or_insert_default();
    if !contains_identity_file(arguments, &private_key_path) {
        arguments.extend([
            "-o".to_string(),
            "IdentitiesOnly=yes".to_string(),
            "-i".to_string(),
            private_key_path.to_string_lossy().into_owned(),
        ]);
    }
    Ok(Some(key))
}

fn load_managed_identity(
    directory: &Path,
    host: &str,
    port: u16,
    username: Option<&str>,
) -> Result<Option<(ManagedSshKey, PathBuf)>> {
    let manifest = load_or_create_manifest_in(directory)?;
    let Some(key) = manifest.keys.into_iter().find(|key| {
        key.deployment_state == ManagedSshKeyDeploymentState::Verified
            && key.host == host
            && key.port == port
            && key.requested_username.as_deref() == username
    }) else {
        return Ok(None);
    };

    let private_key_path = directory.join(&key.private_key_file);
    if !private_key_path.is_file() {
        return Ok(None);
    }
    Ok(Some((key, private_key_path)))
}

/// Generates and stores a new identity for this target.
///
/// Key generation spawns `ssh-keygen` and rewrites the manifest, so the whole
/// body runs on the background executor.
pub async fn generate_managed_ssh_key(
    options: &SshConnectionOptions,
    remote_username: String,
    cx: &AsyncApp,
) -> Result<GeneratedManagedSshKey> {
    let options = options.clone();
    cx.background_spawn(
        async move { generate_managed_ssh_key_blocking(options, remote_username).await },
    )
    .await
}

async fn generate_managed_ssh_key_blocking(
    options: SshConnectionOptions,
    remote_username: String,
) -> Result<GeneratedManagedSshKey> {
    let directory = managed_ssh_key_directory();
    let mut manifest = load_or_create_manifest_in(&directory)?;
    let host = normalize_host(&options.host.to_string());
    let port = options.port.unwrap_or(22);

    if let Some(key) = manifest.keys.iter().find(|key| {
        key.host == host
            && key.port == port
            && key.requested_username == options.username
            && directory.join(&key.private_key_file).is_file()
    }) {
        return Ok(GeneratedManagedSshKey {
            private_key_path: directory.join(&key.private_key_file),
            record: key.clone(),
        });
    }

    fs::create_dir_all(&directory)
        .with_context(|| format!("创建 Zed SSH 密钥目录失败：{}", directory.display()))?;
    enforce_restricted_permissions(&directory, RestrictedPath::Directory)?;

    let target_hash = short_hash(&format!(
        "{}\0{}\0{}",
        options.username.as_deref().unwrap_or(""),
        host,
        port
    ));
    let readable_host = sanitize_file_component(&host);
    let readable_user = sanitize_file_component(&remote_username);
    let private_key_file = format!(
        "zed-remote-ed25519-{}_{}-{}-{}",
        readable_user, readable_host, port, target_hash
    );
    let private_key_path = directory.join(&private_key_file);
    let temporary_path = directory.join(format!(".{private_key_file}-{}.tmp", Uuid::new_v4()));
    let created_at = utc_timestamp();
    let comment = format!(
        "zed-created;client={};client_id={};target={}@{}:{};created={};key_id={}",
        sanitize_comment_component(&manifest.client_name),
        manifest.client_id,
        sanitize_comment_component(&remote_username),
        sanitize_comment_component(&host),
        port,
        created_at,
        target_hash
    );

    let output = util::command::new_command("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", &comment, "-f"])
        .arg(&temporary_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("无法启动 ssh-keygen；请先安装 OpenSSH 客户端")?;
    if !output.status.success() {
        cleanup_key_pair(&temporary_path);
        anyhow::bail!(
            "创建 Zed SSH 密钥失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let temporary_public_path = public_key_path(&temporary_path);
    let public_key = fs::read_to_string(&temporary_public_path)
        .context("读取新建的 Zed SSH 公钥失败")?
        .trim()
        .to_string();
    let key_id = public_key_identity(&public_key)?;
    fs::rename(&temporary_path, &private_key_path).context("保存 Zed SSH 私钥失败")?;
    if let Err(error) = fs::rename(&temporary_public_path, public_key_path(&private_key_path))
        .context("保存 Zed SSH 公钥失败")
    {
        cleanup_key_pair(&private_key_path);
        return Err(error);
    }
    enforce_restricted_permissions(&private_key_path, RestrictedPath::File)?;

    let record = ManagedSshKey {
        key_id,
        host,
        port,
        requested_username: options.username.clone(),
        connection_args: options.additional_args_without_port_forwards(),
        ssh_destination: options.ssh_destination(),
        remote_username,
        private_key_file,
        public_key,
        created_at,
        last_used_at: None,
        deployment_state: ManagedSshKeyDeploymentState::Pending,
    };
    manifest.keys.push(record.clone());
    save_manifest_in(&directory, &manifest)?;

    Ok(GeneratedManagedSshKey {
        record,
        private_key_path,
    })
}

pub async fn mark_managed_ssh_key_verified(key_id: &str, cx: &AsyncApp) -> Result<()> {
    let key_id = key_id.to_owned();
    cx.background_spawn(async move {
        let directory = managed_ssh_key_directory();
        let mut manifest = load_or_create_manifest_in(&directory)?;
        let Some(key) = manifest.keys.iter_mut().find(|key| key.key_id == key_id) else {
            anyhow::bail!("找不到刚刚创建的 Zed SSH 密钥记录");
        };
        key.deployment_state = ManagedSshKeyDeploymentState::Verified;
        key.last_used_at = Some(utc_timestamp());
        save_manifest_in(&directory, &manifest)
    })
    .await
}

pub async fn mark_managed_ssh_key_used(key_id: &str, cx: &AsyncApp) -> Result<()> {
    let key_id = key_id.to_owned();
    cx.background_spawn(async move {
        let directory = managed_ssh_key_directory();
        let mut manifest = load_or_create_manifest_in(&directory)?;
        if let Some(key) = manifest.keys.iter_mut().find(|key| key.key_id == key_id) {
            key.last_used_at = Some(utc_timestamp());
            save_manifest_in(&directory, &manifest)?;
        }
        Ok(())
    })
    .await
}

pub async fn revoke_and_delete_managed_ssh_key(key_id: &str, cx: &AsyncApp) -> Result<()> {
    let key_id = key_id.to_owned();
    cx.background_spawn(async move { revoke_and_delete_managed_ssh_key_blocking(&key_id).await })
        .await
}

async fn revoke_and_delete_managed_ssh_key_blocking(key_id: &str) -> Result<()> {
    let manifest = load_or_create_manifest()?;
    let key = manifest
        .keys
        .iter()
        .find(|key| key.key_id == key_id)
        .cloned()
        .context("找不到要撤销的 Zed SSH 密钥")?;
    let private_key_path = managed_ssh_key_directory().join(&key.private_key_file);
    if !private_key_path.is_file() {
        anyhow::bail!("本地私钥已不存在，无法安全登录远程主机撤销公钥");
    }

    let destination = if key.ssh_destination.is_empty() {
        let mut destination = String::new();
        if let Some(username) = &key.requested_username {
            destination.push_str(username);
            destination.push('@');
        }
        destination.push_str(&key.host);
        destination
    } else {
        key.ssh_destination.clone()
    };
    let script = "umask 077; file=\"$HOME/.ssh/authorized_keys\"; [ -f \"$file\" ] || exit 0; lock=\"$HOME/.ssh/.zed-authorized-keys.lock\"; count=0; while ! mkdir \"$lock\" 2>/dev/null; do count=$((count+1)); [ \"$count\" -ge 100 ] && exit 73; sleep 0.1; done; trap 'rm -f \"$file.zed-tmp.$$\"; rmdir \"$lock\"' EXIT HUP INT TERM; set -- $1; type=$1; blob=$2; awk -v type=\"$type\" -v blob=\"$blob\" '!( $1 == type && $2 == blob )' \"$file\" > \"$file.zed-tmp.$$\"; chmod 600 \"$file.zed-tmp.$$\"; mv \"$file.zed-tmp.$$\" \"$file\"";
    let output = util::command::new_command("ssh")
        .args(&key.connection_args)
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "ConnectTimeout=15",
            "-i",
        ])
        .arg(&private_key_path)
        .arg(destination)
        .args(["sh", "-c", script, "zed-revoke-key", &key.public_key])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("无法启动 SSH 撤销命令")?;
    if !output.status.success() {
        anyhow::bail!(
            "远程撤销失败，本地密钥已保留：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    delete_local_managed_ssh_key_blocking(key_id)
}

pub async fn delete_local_managed_ssh_key(key_id: &str, cx: &AsyncApp) -> Result<()> {
    let key_id = key_id.to_owned();
    cx.background_spawn(async move { delete_local_managed_ssh_key_blocking(&key_id) })
        .await
}

fn delete_local_managed_ssh_key_blocking(key_id: &str) -> Result<()> {
    let directory = managed_ssh_key_directory();
    let mut manifest = load_or_create_manifest_in(&directory)?;
    let Some(index) = manifest.keys.iter().position(|key| key.key_id == key_id) else {
        return Ok(());
    };
    let key = manifest.keys.remove(index);
    let private_key_path = directory.join(key.private_key_file);
    remove_file_if_present(&private_key_path)?;
    remove_file_if_present(&public_key_path(&private_key_path))?;
    save_manifest_in(&directory, &manifest)
}

fn load_or_create_manifest() -> Result<ManagedSshKeyManifest> {
    load_or_create_manifest_in(&managed_ssh_key_directory())
}

/// Reads `directory`'s key manifest, creating it when it does not exist yet.
///
/// Reading must not enforce permissions again: on Windows that would run a
/// blocking `icacls` process, and this runs on every SSH connection. Existing
/// key material is only repaired where that is free, see [`repair_read_permissions`].
fn load_or_create_manifest_in(directory: &Path) -> Result<ManagedSshKeyManifest> {
    let path = directory.join(MANIFEST_FILE_NAME);
    if path.is_file() {
        let manifest: ManagedSshKeyManifest = serde_json::from_slice(
            &fs::read(&path).with_context(|| format!("读取 {} 失败", path.display()))?,
        )
        .with_context(|| format!("解析 {} 失败", path.display()))?;
        if manifest.version != MANIFEST_VERSION {
            anyhow::bail!("不支持的 Zed SSH 密钥清单版本：{}", manifest.version);
        }
        repair_read_permissions(directory, &path, &manifest.keys)?;
        return Ok(manifest);
    }

    fs::create_dir_all(directory).with_context(|| format!("创建 {} 失败", directory.display()))?;
    enforce_restricted_permissions(directory, RestrictedPath::Directory)?;
    let manifest = ManagedSshKeyManifest {
        version: MANIFEST_VERSION,
        client_id: Uuid::new_v4().to_string(),
        client_name: local_client_name(),
        keys: Vec::new(),
    };
    save_manifest_in(directory, &manifest)?;
    Ok(manifest)
}

fn save_manifest_in(directory: &Path, manifest: &ManagedSshKeyManifest) -> Result<()> {
    fs::create_dir_all(directory)?;
    enforce_restricted_permissions(directory, RestrictedPath::Directory)?;
    let path = directory.join(MANIFEST_FILE_NAME);
    let temporary_path = directory.join(format!(".{MANIFEST_FILE_NAME}-{}.tmp", Uuid::new_v4()));
    let bytes = serde_json::to_vec_pretty(manifest)?;
    fs::write(&temporary_path, bytes)
        .with_context(|| format!("写入 {} 失败", temporary_path.display()))?;
    enforce_restricted_permissions(&temporary_path, RestrictedPath::File)?;
    replace_manifest_file(&temporary_path, &path)
        .with_context(|| format!("替换 {} 失败", path.display()))?;
    Ok(())
}

#[cfg(not(windows))]
fn replace_manifest_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_manifest_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;

    use windows::{
        Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MoveFileExW},
        core::PCWSTR,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    unsafe {
        MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING,
        )
    }
    .map_err(std::io::Error::other)
}

fn contains_identity_file(arguments: &[String], private_key_path: &Path) -> bool {
    let expected = private_key_path.to_string_lossy();
    arguments.windows(2).any(|pair| {
        pair.first().is_some_and(|argument| argument == "-i")
            && pair.get(1).is_some_and(|argument| argument == &expected)
    }) || arguments.iter().any(|argument| {
        argument
            .strip_prefix("-i")
            .is_some_and(|path| path == expected)
    })
}

fn public_key_path(private_key_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.pub", private_key_path.to_string_lossy()))
}

fn cleanup_key_pair(private_key_path: &Path) {
    if let Err(error) = remove_file_if_present(private_key_path) {
        log::warn!("清理未完成的 Zed SSH 私钥失败：{error:#}");
    }
    if let Err(error) = remove_file_if_present(&public_key_path(private_key_path)) {
        log::warn!("清理未完成的 Zed SSH 公钥失败：{error:#}");
    }
}

fn remove_file_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("删除 {} 失败", path.display())),
    }
}

fn public_key_identity(public_key: &str) -> Result<String> {
    let mut fields = public_key.split_whitespace();
    let key_type = fields.next().context("公钥缺少类型")?;
    let key_blob = fields.next().context("公钥缺少内容")?;
    Ok(format!("{}:{}", key_type, short_hash(key_blob)))
}

fn short_hash(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn normalize_host(host: &str) -> String {
    host.trim().to_lowercase()
}

fn sanitize_file_component(value: &str) -> String {
    let value = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let value = value.trim_matches(['.', '_']);
    if value.is_empty() {
        "unknown".to_string()
    } else {
        value.chars().take(48).collect()
    }
}

fn sanitize_comment_component(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '@') {
                character
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

fn local_client_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "unknown-client".to_string())
}

fn utc_timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[derive(Clone, Copy)]
enum RestrictedPath {
    Directory,
    File,
}

/// Applies the permissions required for Zed-owned key material.
///
/// Only call this while creating or rewriting the key directory, the manifest
/// or a private key. On Windows it runs `icacls`, a blocking process spawn, so
/// it must stay off GPUI's foreground executor.
fn enforce_restricted_permissions(path: &Path, kind: RestrictedPath) -> Result<()> {
    #[cfg(test)]
    TEST_PERMISSION_ENFORCEMENTS.with(|count| count.set(count.get() + 1));
    match kind {
        RestrictedPath::Directory => restrict_directory_permissions(path),
        RestrictedPath::File => restrict_private_key_permissions(path),
    }
}

#[cfg(test)]
std::thread_local! {
    /// Counts permission enforcement on the current thread, so the regression
    /// test below can prove that reading the manifest does not enforce again.
    static TEST_PERMISSION_ENFORCEMENTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn permission_enforcement_count() -> usize {
    TEST_PERMISSION_ENFORCEMENTS.with(std::cell::Cell::get)
}

/// Restores the permissions of existing key material without spawning a process.
///
/// Unix re-applies the mode only when it drifted, which costs one `stat` per
/// path. Verifying a Windows ACL would need another `icacls` process, so Windows
/// permissions are enforced where the key material is created or rewritten, in
/// [`enforce_restricted_permissions`].
#[cfg(unix)]
fn repair_read_permissions(
    directory: &Path,
    manifest_path: &Path,
    keys: &[ManagedSshKey],
) -> Result<()> {
    repair_unix_mode(directory, 0o700)?;
    repair_unix_mode(manifest_path, 0o600)?;
    for key in keys {
        let private_key_path = directory.join(&key.private_key_file);
        if private_key_path.is_file() {
            repair_unix_mode(&private_key_path, 0o600)?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn repair_read_permissions(
    _directory: &Path,
    _manifest_path: &Path,
    _keys: &[ManagedSshKey],
) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn repair_unix_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("读取 {} 权限失败", path.display()));
        }
    };
    if metadata.permissions().mode() & 0o777 != mode {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .with_context(|| format!("设置 {} 权限失败", path.display()))?;
    }
    Ok(())
}

#[cfg(unix)]
fn restrict_directory_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("设置 {} 权限失败", path.display()))
}

#[cfg(windows)]
fn restrict_directory_permissions(path: &Path) -> Result<()> {
    restrict_windows_permissions(path)
}

#[cfg(not(any(unix, windows)))]
fn restrict_directory_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn restrict_private_key_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("设置 {} 权限失败", path.display()))
}

#[cfg(windows)]
fn restrict_private_key_permissions(path: &Path) -> Result<()> {
    restrict_windows_permissions(path)
}

#[cfg(not(any(unix, windows)))]
fn restrict_private_key_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(windows)]
fn restrict_windows_permissions(path: &Path) -> Result<()> {
    let username = std::env::var("USERNAME").context("无法确定当前 Windows 用户")?;
    let output = std::process::Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("{username}:(F)"))
        .output()
        .with_context(|| format!("无法设置 {} 的 Windows ACL", path.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "设置 {} 的 Windows ACL 失败：{}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_managed_key_file_components() {
        assert_eq!(sanitize_file_component("root/a:b"), "root_a_b");
        assert_eq!(sanitize_file_component("..."), "unknown");
    }

    #[test]
    fn identity_arguments_are_not_added_twice() {
        let key = Path::new("/tmp/zed-key");
        assert!(contains_identity_file(
            &["-i".to_string(), key.to_string_lossy().into_owned()],
            key
        ));
        assert!(contains_identity_file(
            &[format!("-i{}", key.to_string_lossy())],
            key
        ));
    }

    #[test]
    fn public_key_identity_ignores_comment() {
        assert_eq!(
            public_key_identity("ssh-ed25519 AAAA first").unwrap(),
            public_key_identity("ssh-ed25519 AAAA second").unwrap()
        );
    }

    fn test_key(key_id: &str, deployment_state: ManagedSshKeyDeploymentState) -> ManagedSshKey {
        ManagedSshKey {
            key_id: key_id.to_string(),
            host: "example.com".to_string(),
            port: 22,
            requested_username: Some("dev".to_string()),
            connection_args: Vec::new(),
            ssh_destination: String::new(),
            remote_username: "dev".to_string(),
            private_key_file: "zed-key".to_string(),
            public_key: "ssh-ed25519 AAAA".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            last_used_at: None,
            deployment_state,
        }
    }

    #[test]
    fn reading_the_manifest_does_not_enforce_permissions_again() {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path();
        let created = load_or_create_manifest_in(directory).unwrap();

        let enforced = permission_enforcement_count();
        let reread = load_or_create_manifest_in(directory).unwrap();
        assert_eq!(permission_enforcement_count(), enforced);
        assert_eq!(reread.client_id, created.client_id);
    }

    #[test]
    fn only_verified_keys_with_an_existing_private_key_are_reused() {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path();
        let mut manifest = load_or_create_manifest_in(directory).unwrap();
        manifest
            .keys
            .push(test_key("pending", ManagedSshKeyDeploymentState::Pending));
        manifest
            .keys
            .push(test_key("verified", ManagedSshKeyDeploymentState::Verified));
        save_manifest_in(directory, &manifest).unwrap();

        assert!(
            load_managed_identity(directory, "example.com", 22, Some("dev"))
                .unwrap()
                .is_none()
        );

        fs::write(directory.join("zed-key"), b"private key").unwrap();
        let (key, private_key_path) =
            load_managed_identity(directory, "example.com", 22, Some("dev"))
                .unwrap()
                .unwrap();
        assert_eq!(key.key_id, "verified");
        assert_eq!(private_key_path, directory.join("zed-key"));
        assert!(
            load_managed_identity(directory, "example.com", 2222, Some("dev"))
                .unwrap()
                .is_none()
        );
        assert!(
            load_managed_identity(directory, "example.com", 22, Some("other"))
                .unwrap()
                .is_none()
        );
        assert!(
            load_managed_identity(directory, "other.example", 22, Some("dev"))
                .unwrap()
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn reading_a_loose_manifest_restores_restrictive_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path();
        load_or_create_manifest_in(directory).unwrap();
        let manifest_path = directory.join(MANIFEST_FILE_NAME);
        fs::set_permissions(directory, fs::Permissions::from_mode(0o777)).unwrap();
        fs::set_permissions(&manifest_path, fs::Permissions::from_mode(0o644)).unwrap();

        load_or_create_manifest_in(directory).unwrap();
        assert_eq!(
            fs::metadata(directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&manifest_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

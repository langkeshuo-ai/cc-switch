//! Transport-agnostic sync protocol layer.
//!
//! Shared by WebDAV, S3, and future transports. Artifact set: `db.sql` + `skills.zip`.

use std::collections::BTreeMap;
use std::fs;
use std::future::Future;
use std::process::Command;
use std::sync::OnceLock;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::tempdir;

use crate::error::AppError;
use crate::services::skill::{skill_state_read_guard, skill_state_write_guard};
use crate::services::sync_crypto;
use crate::settings::WebDavSyncStatus;

// Re-export archive functions for use by transport layers.
pub(crate) use super::webdav_sync::archive::{
    backup_current_skills, restore_skills_from_backup, restore_skills_zip, zip_skills_ssot,
};

// ─── Protocol constants ──────────────────────────────────────

/// Wire-format identifier stored in remote manifests.
/// Retains historic "webdav" naming for backward compatibility with existing remotes.
pub(crate) const PROTOCOL_FORMAT: &str = "cc-switch-webdav-sync";
pub(crate) const PROTOCOL_VERSION: u32 = 2;
pub(crate) const DB_COMPAT_VERSION: u32 = 6;
pub(crate) const LEGACY_DB_COMPAT_VERSION: u32 = 5;
pub(crate) const REMOTE_DB_SQL: &str = "db.sql";
pub(crate) const REMOTE_SKILLS_ZIP: &str = "skills.zip";
pub(crate) const REMOTE_MANIFEST: &str = "manifest.json";
pub(crate) const MAX_DEVICE_NAME_LEN: usize = 64;
pub(crate) const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_SYNC_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;

// ─── Sync operation lock ────────────────────────────────────

/// Serialize every snapshot upload/download across all transports.
///
/// WebDAV and S3 used to own separate mutexes, which allowed two transports to
/// restore the database and Skills SSOT concurrently. Keep the lock in this
/// transport-agnostic layer so future transports automatically share it too.
pub(crate) fn sync_mutex() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

pub(crate) async fn run_with_sync_lock<T, Fut>(operation: Fut) -> Result<T, AppError>
where
    Fut: Future<Output = Result<T, AppError>>,
{
    let _guard = sync_mutex().lock().await;
    operation.await
}

/// Tables whose changes make the remote configuration snapshot stale.
///
/// Keep this transport-agnostic so WebDAV and S3 cannot silently drift apart.
/// `model_pricing` is intentionally excluded while its local JSON sidecar is
/// the user-owned SSOT.
pub(crate) fn should_trigger_auto_sync_for_table(table: &str) -> bool {
    let normalized = table.trim().to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "providers"
            | "provider_endpoints"
            | "mcp_servers"
            | "prompts"
            | "skills"
            | "skill_repos"
            | "profiles"
            | "settings"
            | "proxy_config"
    )
}

// ─── Error helpers ───────────────────────────────────────────

pub(crate) fn localized(
    key: &'static str,
    zh: impl Into<String>,
    en: impl Into<String>,
) -> AppError {
    AppError::localized(key, zh, en)
}

pub(crate) fn io_context_localized(
    _key: &'static str,
    zh: impl Into<String>,
    en: impl Into<String>,
    source: std::io::Error,
) -> AppError {
    let zh_msg = zh.into();
    let en_msg = en.into();
    AppError::IoContext {
        context: format!("{zh_msg} ({en_msg})"),
        source,
    }
}

// ─── Transport security policy ───────────────────────────────

/// Hosts that may use plaintext HTTP without an explicit exemption: loopback,
/// RFC1918 / link-local private ranges, and `.local` / `localhost` mDNS names.
fn is_trusted_plaintext_host(host: &str) -> bool {
    let host = host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        Ok(std::net::IpAddr::V6(v6)) => {
            v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local()
        }
        Err(_) => false,
    }
}

/// True when `raw_url` is plaintext HTTP to a host outside the trusted set.
pub(crate) fn is_plaintext_http_to_untrusted_host(raw_url: &str) -> bool {
    let Ok(url) = url::Url::parse(raw_url) else {
        return false;
    };
    if url.scheme() != "http" {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    !is_trusted_plaintext_host(host)
}

/// Enforce the secure-transport policy for uploads/downloads.
///
/// Plaintext HTTP to a non-loopback, non-LAN host is rejected unless the user
/// explicitly opted into the "allow plaintext HTTP (self-hosted LAN)"
/// exemption. Loopback and private-network hosts are always allowed.
pub(crate) fn enforce_secure_transport(
    raw_url: &str,
    allow_plaintext_http: bool,
) -> Result<(), AppError> {
    if allow_plaintext_http || !is_plaintext_http_to_untrusted_host(raw_url) {
        return Ok(());
    }
    Err(localized(
        "sync.plaintext_http.blocked",
        "已阻止明文 HTTP 同步：为避免账号密码与同步数据（含供应商 API Key）被网络中间人截获，请改用 HTTPS，或在同步设置中显式勾选“允许明文 HTTP（自建内网）”后重试。",
        "Plaintext HTTP sync blocked: to prevent credentials and sync data (including provider API keys) from being intercepted, use HTTPS or explicitly enable \"Allow plaintext HTTP (self-hosted LAN)\" in sync settings.",
    ))
}

/// Security knobs shared by WebDAV and S3 settings for the snapshot flows.
pub(crate) trait SyncSecuritySettings {
    /// Whether the user accepted plaintext HTTP for this transport.
    fn allow_plaintext_http(&self) -> bool;
    /// Resolve the E2E password for this transport:
    ///
    /// - encryption disabled → `Ok(None)` (plaintext snapshot),
    /// - enabled with a non-empty password → `Ok(Some(password))`,
    /// - enabled with an empty password → `Err` (never silently upload/download
    ///   in plaintext).
    ///
    /// This is the single choke point for the "enabled but empty password"
    /// state, so settings that reach it through `update_settings` /
    /// `merge_settings_for_save` (which do not run [`validate`]) cannot bypass
    /// the check and fall back to plaintext.
    ///
    /// [`validate`]: crate::settings::WebDavSyncSettings::validate
    fn encryption_password(&self) -> Result<Option<&str>, AppError>;
}

// ─── Types ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SyncManifest {
    pub format: String,
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db_compat_version: Option<u32>,
    pub device_name: String,
    pub created_at: String,
    pub artifacts: BTreeMap<String, ArtifactMeta>,
    pub snapshot_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ArtifactMeta {
    pub sha256: String,
    pub size: u64,
}

pub(crate) struct LocalSnapshot {
    pub db_sql: Vec<u8>,
    pub skills_zip: Vec<u8>,
    pub manifest_bytes: Vec<u8>,
    pub manifest_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteLayout {
    Current,
    Legacy,
}

impl RemoteLayout {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Legacy => "legacy",
        }
    }
}

// ─── Snapshot building ───────────────────────────────────────

pub(crate) fn build_local_snapshot(
    db: &crate::database::Database,
) -> Result<LocalSnapshot, AppError> {
    build_local_snapshot_with_crypto(db, None)
}

/// Build a snapshot, optionally encrypting each artifact **before** hashing so
/// the manifest describes exactly the bytes that get uploaded.
///
/// Encryption is implemented once in [`sync_crypto`] and applied here in the
/// shared protocol layer; WebDAV and S3 never touch the cipher directly.
pub(crate) fn build_local_snapshot_with_crypto(
    db: &crate::database::Database,
    encryption_password: Option<&str>,
) -> Result<LocalSnapshot, AppError> {
    // Keep the DB's skill rows and the filesystem SSOT at one logical point in
    // time. Skill writers take the matching write guard around both mutations.
    let _skill_state_guard = skill_state_read_guard();

    // Export database to SQL string
    let sql_string = db.export_sql_string_for_sync()?;
    let db_sql = sql_string.into_bytes();

    // Pack skills into deterministic ZIP. Note: the SSOT zipper only writes to
    // a path, so the plaintext skills archive briefly lands in the system temp
    // dir here and is read back into memory below; it is encrypted (if opted
    // in) only afterwards. The DB export above never touches disk. The temp
    // directory is removed when `tmp` drops at the end of this function.
    let tmp = tempdir().map_err(|e| {
        io_context_localized(
            "sync.snapshot_tmpdir_failed",
            "创建快照临时目录失败",
            "Failed to create temporary directory for snapshot",
            e,
        )
    })?;
    let skills_zip_path = tmp.path().join(REMOTE_SKILLS_ZIP);
    zip_skills_ssot(&skills_zip_path)?;
    let skills_zip = fs::read(&skills_zip_path).map_err(|e| AppError::io(&skills_zip_path, e))?;

    // Encrypt in memory only when opted in; the remote only ever sees these
    // bytes (ciphertext when a password is configured, plaintext otherwise).
    let (db_sql, skills_zip) = match encryption_password {
        Some(password) => (
            sync_crypto::encrypt_blob(&db_sql, password)?,
            sync_crypto::encrypt_blob(&skills_zip, password)?,
        ),
        None => (db_sql, skills_zip),
    };

    finalize_snapshot(
        detect_system_device_name().unwrap_or_else(|| "Unknown Device".to_string()),
        Utc::now().to_rfc3339(),
        db_sql,
        skills_zip,
    )
}

/// Assemble a self-verifying manifest around already-serialized artifacts.
///
/// The manifest hashes and `snapshot_id` are derived from the exact artifact
/// bytes passed in, which for encrypted snapshots are the ciphertext.
fn finalize_snapshot(
    device_name: String,
    created_at: String,
    db_sql: Vec<u8>,
    skills_zip: Vec<u8>,
) -> Result<LocalSnapshot, AppError> {
    let mut artifacts = BTreeMap::new();
    artifacts.insert(
        REMOTE_DB_SQL.to_string(),
        ArtifactMeta {
            sha256: sha256_hex(&db_sql),
            size: db_sql.len() as u64,
        },
    );
    artifacts.insert(
        REMOTE_SKILLS_ZIP.to_string(),
        ArtifactMeta {
            sha256: sha256_hex(&skills_zip),
            size: skills_zip.len() as u64,
        },
    );

    let snapshot_id = compute_snapshot_id(&artifacts);
    let manifest = SyncManifest {
        format: PROTOCOL_FORMAT.to_string(),
        version: PROTOCOL_VERSION,
        db_compat_version: Some(DB_COMPAT_VERSION),
        device_name,
        created_at,
        artifacts,
        snapshot_id,
    };
    let manifest_bytes =
        serde_json::to_vec_pretty(&manifest).map_err(|e| AppError::JsonSerialize { source: e })?;
    let manifest_hash = sha256_hex(&manifest_bytes);

    Ok(LocalSnapshot {
        db_sql,
        skills_zip,
        manifest_bytes,
        manifest_hash,
    })
}

// ─── Manifest handling ───────────────────────────────────────

/// Compute a deterministic snapshot identity from artifact hashes.
///
/// BTreeMap iteration order is sorted by key, ensuring stability.
pub(crate) fn compute_snapshot_id(artifacts: &BTreeMap<String, ArtifactMeta>) -> String {
    let parts: Vec<String> = artifacts
        .iter()
        .map(|(name, meta)| format!("{}:{}", name, meta.sha256))
        .collect();
    sha256_hex(parts.join("|").as_bytes())
}

pub(crate) fn effective_db_compat_version(
    manifest: &SyncManifest,
    layout: RemoteLayout,
) -> Option<u32> {
    manifest
        .db_compat_version
        .or_else(|| (layout == RemoteLayout::Legacy).then_some(LEGACY_DB_COMPAT_VERSION))
}

pub(crate) fn validate_manifest_compat(
    manifest: &SyncManifest,
    layout: RemoteLayout,
) -> Result<(), AppError> {
    if manifest.format != PROTOCOL_FORMAT {
        return Err(localized(
            "sync.manifest_format_incompatible",
            format!("远端 manifest 格式不兼容: {}", manifest.format),
            format!(
                "Remote manifest format is incompatible: {}",
                manifest.format
            ),
        ));
    }
    if manifest.version != PROTOCOL_VERSION {
        return Err(localized(
            "sync.manifest_version_incompatible",
            format!(
                "远端 manifest 协议版本不兼容: v{} (本地 v{PROTOCOL_VERSION})",
                manifest.version
            ),
            format!(
                "Remote manifest protocol version is incompatible: v{} (local v{PROTOCOL_VERSION})",
                manifest.version
            ),
        ));
    }
    let Some(db_compat_version) = effective_db_compat_version(manifest, layout) else {
        return Err(localized(
            "sync.manifest_db_version_missing",
            "远端 manifest 缺少数据库兼容版本",
            "Remote manifest is missing the database compatibility version.",
        ));
    };
    match layout {
        RemoteLayout::Current if db_compat_version != DB_COMPAT_VERSION => {
            return Err(localized(
                "sync.manifest_db_version_incompatible",
                format!(
                    "远端数据库快照版本不兼容: db-v{db_compat_version} (本地 db-v{DB_COMPAT_VERSION})"
                ),
                format!(
                    "Remote database snapshot version is incompatible: db-v{db_compat_version} (local db-v{DB_COMPAT_VERSION})"
                ),
            ));
        }
        RemoteLayout::Legacy if db_compat_version > DB_COMPAT_VERSION => {
            return Err(localized(
                "sync.manifest_db_version_incompatible",
                format!(
                    "远端数据库快照版本不兼容: db-v{db_compat_version} (本地最高支持 db-v{DB_COMPAT_VERSION})"
                ),
                format!(
                    "Remote database snapshot version is incompatible: db-v{db_compat_version} (local supports up to db-v{DB_COMPAT_VERSION})"
                ),
            ));
        }
        _ => {}
    }
    Ok(())
}

// ─── Artifact verification ───────────────────────────────────

pub(crate) fn validate_artifact_size_limit(artifact_name: &str, size: u64) -> Result<(), AppError> {
    if size > MAX_SYNC_ARTIFACT_BYTES {
        let max_mb = MAX_SYNC_ARTIFACT_BYTES / 1024 / 1024;
        return Err(localized(
            "sync.artifact_too_large",
            format!("artifact {artifact_name} 超过下载上限（{} MB）", max_mb),
            format!(
                "Artifact {artifact_name} exceeds download limit ({} MB)",
                max_mb
            ),
        ));
    }
    Ok(())
}

/// Verify that downloaded artifact bytes match the expected size and SHA-256 hash.
pub(crate) fn verify_artifact(
    bytes: &[u8],
    artifact_name: &str,
    meta: &ArtifactMeta,
) -> Result<(), AppError> {
    // Quick size check before expensive hash
    if bytes.len() as u64 != meta.size {
        return Err(localized(
            "sync.artifact_size_mismatch",
            format!(
                "artifact {artifact_name} 大小不匹配 (expected: {}, got: {})",
                meta.size,
                bytes.len(),
            ),
            format!(
                "Artifact {artifact_name} size mismatch (expected: {}, got: {})",
                meta.size,
                bytes.len(),
            ),
        ));
    }

    let actual_hash = sha256_hex(bytes);
    if actual_hash != meta.sha256 {
        return Err(localized(
            "sync.artifact_hash_mismatch",
            format!(
                "artifact {artifact_name} SHA256 校验失败 (expected: {}..., got: {}...)",
                meta.sha256.get(..8).unwrap_or(&meta.sha256),
                actual_hash.get(..8).unwrap_or(&actual_hash),
            ),
            format!(
                "Artifact {artifact_name} SHA256 verification failed (expected: {}..., got: {}...)",
                meta.sha256.get(..8).unwrap_or(&meta.sha256),
                actual_hash.get(..8).unwrap_or(&actual_hash),
            ),
        ));
    }
    Ok(())
}

// ─── Snapshot application ────────────────────────────────────

pub(crate) fn apply_snapshot(
    db: &crate::database::Database,
    db_sql: &[u8],
    skills_zip: &[u8],
) -> Result<(), AppError> {
    let sql_str = std::str::from_utf8(db_sql).map_err(|e| {
        localized(
            "sync.sql_not_utf8",
            format!("SQL 非 UTF-8: {e}"),
            format!("SQL is not valid UTF-8: {e}"),
        )
    })?;
    // Exclude installs, uninstalls, updates, and local projection while Skills
    // are backed up/replaced and the corresponding database snapshot is applied.
    let _skill_state_guard = skill_state_write_guard();
    let skills_backup = backup_current_skills()?;

    // Replace skills first, then import database; roll back skills on DB failure.
    restore_skills_zip(skills_zip)?;

    if let Err(db_err) = db.import_sql_string_for_sync(sql_str) {
        if let Err(rollback_err) = restore_skills_from_backup(&skills_backup) {
            return Err(localized(
                "sync.db_import_and_rollback_failed",
                format!("导入数据库失败: {db_err}; 同时回滚 Skills 失败: {rollback_err}"),
                format!(
                    "Database import failed: {db_err}; skills rollback also failed: {rollback_err}"
                ),
            ));
        }
        return Err(db_err);
    }

    Ok(())
}

// ─── Verified apply (hash check before any local mutation) ───

/// Recompute the manifest identity from its artifact hashes and compare it with
/// the recorded `snapshot_id`. Detects a manifest whose contents were edited
/// without updating its identity.
///
/// # Threat model (what this does *not* cover)
///
/// This check — together with [`verify_manifest_artifact`] — only defends
/// against **naive corruption/manipulation**: transit damage, or content that
/// was changed without also updating the hash. It does **not** provide
/// **authenticity**: the manifest is neither signed nor covered by the AEAD
/// tag, so an actor who controls the remote can recompute a wholly consistent
/// manifest + artifacts, or replay an older valid snapshot to force a silent
/// rollback. Authenticity currently relies on transport-layer security; a
/// future iteration plans to derive a MAC key from the E2E password and HMAC
/// the manifest (explicitly deferred for now).
pub(crate) fn verify_manifest_integrity(manifest: &SyncManifest) -> Result<(), AppError> {
    let expected = compute_snapshot_id(&manifest.artifacts);
    if expected != manifest.snapshot_id {
        return Err(localized(
            "sync.manifest_integrity_failed",
            "远端 manifest 自校验失败（snapshot_id 与 artifact 哈希不一致），已拒绝应用。",
            "Remote manifest self-check failed (snapshot_id does not match the artifact hashes); refusing to apply.",
        ));
    }
    Ok(())
}

fn verify_manifest_artifact(
    manifest: &SyncManifest,
    artifact_name: &str,
    bytes: &[u8],
) -> Result<(), AppError> {
    let meta = manifest.artifacts.get(artifact_name).ok_or_else(|| {
        localized(
            "sync.manifest_missing_artifact",
            format!("manifest 中缺少 artifact: {artifact_name}"),
            format!("Manifest missing artifact: {artifact_name}"),
        )
    })?;
    verify_artifact(bytes, artifact_name, meta)
}

/// Decrypt one downloaded artifact according to the encryption state of both
/// sides. The four combinations are deliberate:
///
/// | remote magic | local password | result                       |
/// |--------------|----------------|------------------------------|
/// | yes          | `Some`         | decrypt                      |
/// | yes          | `None`         | reject (`password_required`) |
/// | no           | `None`         | pass through (legacy)        |
/// | no           | `Some`         | reject (downgrade guard)     |
///
/// A remote with the magic is either decrypted or explicitly rejected, so an
/// encrypted artifact is never applied as garbage. A plaintext remote is only
/// tolerated while the user has *not* enabled E2E: once encryption is on, a
/// magic-less artifact means an attacker (or a compromised remote) may have
/// stripped encryption and injected self-consistent plaintext. Silently
/// accepting it would break the integrity guarantee E2E promises, so it is
/// rejected with an actionable migration hint.
fn decrypt_artifact(
    artifact_name: &str,
    bytes: &[u8],
    password: Option<&str>,
) -> Result<Vec<u8>, AppError> {
    if !sync_crypto::is_encrypted_blob(bytes) {
        if password.is_some() {
            return Err(localized(
                "sync.encryption.plaintext_remote_rejected",
                format!(
                    "远端快照 {artifact_name} 未加密，但本地已启用端到端加密。为避免静默降级（攻击者可剥离加密注入内容），已拒绝应用。如需迁移该旧版明文快照，请临时关闭端到端加密后重试；任一端重新上传加密快照后此提示将消失。"
                ),
                format!(
                    "Remote artifact {artifact_name} is not encrypted, but local end-to-end encryption is enabled. Refusing to apply it to prevent a silent downgrade (an attacker could strip encryption and inject content). To migrate this legacy plaintext snapshot, temporarily disable end-to-end encryption and retry; this warning disappears once either device uploads an encrypted snapshot again."
                ),
            ));
        }
        return Ok(bytes.to_vec());
    }
    let Some(password) = password else {
        return Err(localized(
            "sync.encryption.password_required",
            format!(
                "远端快照 {artifact_name} 已加密，请在同步设置中启用端到端加密并输入正确口令。"
            ),
            format!(
                "Remote artifact {artifact_name} is encrypted; enable end-to-end encryption and enter the correct password in sync settings."
            ),
        ));
    };
    sync_crypto::decrypt_blob(bytes, password)
}

/// Verify a downloaded snapshot against its manifest, decrypt it, and apply it.
///
/// The manifest identity and both artifact hashes/sizes are checked against the
/// **stored** bytes before any local mutation, so a mismatched or corrupted
/// snapshot is rejected outright. Decryption happens only after verification,
/// and `apply_snapshot` still rolls Skills back if the DB import fails.
pub(crate) fn apply_remote_snapshot(
    db: &crate::database::Database,
    manifest: &SyncManifest,
    stored_db_sql: &[u8],
    stored_skills_zip: &[u8],
    password: Option<&str>,
) -> Result<(), AppError> {
    verify_manifest_integrity(manifest)?;
    verify_manifest_artifact(manifest, REMOTE_DB_SQL, stored_db_sql)?;
    verify_manifest_artifact(manifest, REMOTE_SKILLS_ZIP, stored_skills_zip)?;

    let db_sql = decrypt_artifact(REMOTE_DB_SQL, stored_db_sql, password)?;
    let skills_zip = decrypt_artifact(REMOTE_SKILLS_ZIP, stored_skills_zip, password)?;

    apply_snapshot(db, &db_sql, &skills_zip)
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

pub(crate) fn detect_system_device_name() -> Option<String> {
    let env_name = ["CC_SWITCH_DEVICE_NAME", "COMPUTERNAME", "HOSTNAME"]
        .iter()
        .filter_map(|key| std::env::var(key).ok())
        .find_map(|value| normalize_device_name(&value));

    if env_name.is_some() {
        return env_name;
    }

    let output = Command::new("hostname").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let hostname = String::from_utf8(output.stdout).ok()?;
    normalize_device_name(&hostname)
}

pub(crate) fn normalize_device_name(raw: &str) -> Option<String> {
    let compact = raw
        .chars()
        .fold(String::with_capacity(raw.len()), |mut acc, ch| {
            if ch.is_whitespace() {
                acc.push(' ');
            } else if !ch.is_control() {
                acc.push(ch);
            }
            acc
        });
    let normalized = compact.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = normalized.trim();
    if trimmed.is_empty() {
        return None;
    }

    let limited = trimmed
        .chars()
        .take(MAX_DEVICE_NAME_LEN)
        .collect::<String>();
    if limited.is_empty() {
        None
    } else {
        Some(limited)
    }
}

// ─── Sync status persistence ─────────────────────────────────

pub(crate) fn persist_sync_success_best_effort<S, F>(
    settings: &mut S,
    manifest_hash: String,
    etag: Option<String>,
    persist_fn: F,
) -> bool
where
    F: FnOnce(&mut S, String, Option<String>) -> Result<(), AppError>,
{
    match persist_fn(settings, manifest_hash, etag) {
        Ok(()) => true,
        Err(err) => {
            log::warn!("[Sync] Persist sync status failed, keep operation success: {err}");
            false
        }
    }
}

// ─── Sync status slot access ─────────────────────────────────

/// Uniform access to the shared [`WebDavSyncStatus`] slot kept on both the
/// WebDAV and S3 settings structs.
pub(crate) trait SyncStatusHolder {
    fn sync_status_mut(&mut self) -> &mut WebDavSyncStatus;
}

impl SyncStatusHolder for crate::settings::WebDavSyncSettings {
    fn sync_status_mut(&mut self) -> &mut WebDavSyncStatus {
        &mut self.status
    }
}

impl SyncStatusHolder for crate::settings::S3SyncSettings {
    fn sync_status_mut(&mut self) -> &mut WebDavSyncStatus {
        &mut self.status
    }
}

impl SyncSecuritySettings for crate::settings::WebDavSyncSettings {
    fn allow_plaintext_http(&self) -> bool {
        self.allow_plaintext_http
    }

    fn encryption_password(&self) -> Result<Option<&str>, AppError> {
        resolve_encryption_password(self.encryption_enabled, self.encryption_password.as_str())
    }
}

impl SyncSecuritySettings for crate::settings::S3SyncSettings {
    fn allow_plaintext_http(&self) -> bool {
        self.allow_plaintext_http
    }

    fn encryption_password(&self) -> Result<Option<&str>, AppError> {
        resolve_encryption_password(self.encryption_enabled, self.encryption_password.as_str())
    }
}

/// Shared resolver for [`SyncSecuritySettings::encryption_password`].
///
/// Kept as one function so WebDAV and S3 cannot drift on the empty-password
/// policy.
fn resolve_encryption_password(
    encryption_enabled: bool,
    encryption_password: &str,
) -> Result<Option<&str>, AppError> {
    if !encryption_enabled {
        return Ok(None);
    }
    if encryption_password.is_empty() {
        return Err(localized(
            "sync.encryption.password_empty",
            "已启用端到端加密但口令为空，请重新输入口令或关闭加密后再同步。",
            "End-to-end encryption is enabled but the password is empty; re-enter the password or disable encryption before syncing.",
        ));
    }
    Ok(Some(encryption_password))
}

/// Record a sync failure on the transport's status slot and persist it.
pub(crate) fn persist_sync_error<S, F>(settings: &mut S, error: &AppError, source: &str, update: F)
where
    S: SyncStatusHolder,
    F: FnOnce(WebDavSyncStatus) -> Result<(), AppError>,
{
    let status = settings.sync_status_mut();
    status.last_error = Some(error.to_string());
    status.last_error_source = Some(source.to_string());
    let _ = update(status.clone());
}

/// Record a successful sync on the transport's status slot and persist it.
pub(crate) fn persist_sync_success_with<S, F>(
    settings: &mut S,
    manifest_hash: String,
    etag: Option<String>,
    update: F,
) -> Result<(), AppError>
where
    S: SyncStatusHolder,
    F: FnOnce(WebDavSyncStatus) -> Result<(), AppError>,
{
    let status = WebDavSyncStatus {
        last_sync_at: Some(Utc::now().timestamp()),
        last_error: None,
        last_error_source: None,
        last_local_manifest_hash: Some(manifest_hash.clone()),
        last_remote_manifest_hash: Some(manifest_hash),
        last_remote_etag: etag,
    };
    *settings.sync_status_mut() = status.clone();
    update(status)
}

// ─── Tests ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Provider;
    use serial_test::serial;

    /// Isolate the Skills SSOT (and app config dir) under a temp home so
    /// snapshot build/apply tests never touch the real user directory.
    struct TestHomeGuard {
        previous: Option<std::ffi::OsString>,
        _dir: tempfile::TempDir,
    }

    impl TestHomeGuard {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("create isolated test home");
            let previous = std::env::var_os("CC_SWITCH_TEST_HOME");
            std::env::set_var("CC_SWITCH_TEST_HOME", dir.path());
            Self {
                previous,
                _dir: dir,
            }
        }
    }

    impl Drop for TestHomeGuard {
        fn drop(&mut self) {
            match self.previous.as_ref() {
                Some(previous) => std::env::set_var("CC_SWITCH_TEST_HOME", previous),
                None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
            }
        }
    }

    fn save_baseline_provider(db: &crate::database::Database) {
        db.save_provider(
            "claude",
            &Provider::with_id(
                "baseline-provider".to_string(),
                "Baseline Provider".to_string(),
                serde_json::json!({ "claude_base_url": "https://example.invalid" }),
                None,
            ),
        )
        .expect("save baseline provider");
    }

    #[tokio::test]
    async fn webdav_and_s3_operations_share_one_sync_mutex() {
        let webdav_lock = crate::services::webdav_sync::sync_mutex();
        let s3_lock = crate::services::s3_sync::sync_mutex();
        assert!(
            std::ptr::eq(webdav_lock, s3_lock),
            "every transport must expose the same global sync lock"
        );

        let guard = webdav_lock.lock().await;
        assert!(s3_lock.try_lock().is_err());
        drop(guard);
        assert!(s3_lock.try_lock().is_ok());
    }

    fn artifact(sha256: &str, size: u64) -> ArtifactMeta {
        ArtifactMeta {
            sha256: sha256.to_string(),
            size,
        }
    }

    #[test]
    fn auto_sync_table_filter_covers_shared_configuration() {
        for table in [
            "providers",
            "provider_endpoints",
            "mcp_servers",
            "prompts",
            "skills",
            "skill_repos",
            "profiles",
            "settings",
            "proxy_config",
        ] {
            assert!(
                should_trigger_auto_sync_for_table(table),
                "{table} should trigger an automatic snapshot upload"
            );
        }

        assert!(should_trigger_auto_sync_for_table("  PROFILES  "));
        for table in [
            "proxy_request_logs",
            "provider_health",
            "session_log_sync",
            "model_pricing",
        ] {
            assert!(
                !should_trigger_auto_sync_for_table(table),
                "{table} should not trigger automatic snapshot upload"
            );
        }
    }

    #[test]
    fn snapshot_id_is_stable() {
        let mut artifacts = BTreeMap::new();
        artifacts.insert("db.sql".to_string(), artifact("abc123", 100));
        artifacts.insert("skills.zip".to_string(), artifact("def456", 200));

        let id1 = compute_snapshot_id(&artifacts);
        let id2 = compute_snapshot_id(&artifacts);
        assert_eq!(id1, id2);
    }

    #[test]
    fn snapshot_id_changes_with_artifacts() {
        let mut a1 = BTreeMap::new();
        a1.insert("db.sql".to_string(), artifact("hash-a", 1));

        let mut a2 = BTreeMap::new();
        a2.insert("db.sql".to_string(), artifact("hash-b", 1));

        assert_ne!(compute_snapshot_id(&a1), compute_snapshot_id(&a2));
    }

    #[test]
    fn sha256_hex_is_correct() {
        let hash = sha256_hex(b"hello");
        assert_eq!(
            hash,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn persist_best_effort_returns_true_on_success() {
        let mut dummy = ();
        let ok = persist_sync_success_best_effort(
            &mut dummy,
            "hash".to_string(),
            Some("etag".to_string()),
            |_settings, _hash, _etag| Ok(()),
        );
        assert!(ok);
    }

    #[test]
    fn persist_best_effort_returns_false_on_error() {
        let mut dummy = ();
        let ok = persist_sync_success_best_effort(
            &mut dummy,
            "hash".to_string(),
            None,
            |_settings, _hash, _etag| Err(AppError::Config("boom".to_string())),
        );
        assert!(!ok);
    }

    fn manifest_with(format: &str, version: u32, db_compat_version: Option<u32>) -> SyncManifest {
        let mut artifacts = BTreeMap::new();
        artifacts.insert("db.sql".to_string(), artifact("abc", 1));
        artifacts.insert("skills.zip".to_string(), artifact("def", 2));
        SyncManifest {
            format: format.to_string(),
            version,
            db_compat_version,
            device_name: "My MacBook".to_string(),
            created_at: "2026-02-12T00:00:00Z".to_string(),
            artifacts,
            snapshot_id: "snap-1".to_string(),
        }
    }

    #[test]
    fn validate_manifest_compat_accepts_supported_manifest() {
        let manifest = manifest_with(PROTOCOL_FORMAT, PROTOCOL_VERSION, Some(DB_COMPAT_VERSION));
        assert!(validate_manifest_compat(&manifest, RemoteLayout::Current).is_ok());
    }

    #[test]
    fn validate_manifest_compat_rejects_wrong_format() {
        let manifest = manifest_with("other-format", PROTOCOL_VERSION, Some(DB_COMPAT_VERSION));
        assert!(validate_manifest_compat(&manifest, RemoteLayout::Current).is_err());
    }

    #[test]
    fn validate_manifest_compat_rejects_wrong_version() {
        let manifest = manifest_with(
            PROTOCOL_FORMAT,
            PROTOCOL_VERSION + 1,
            Some(DB_COMPAT_VERSION),
        );
        assert!(validate_manifest_compat(&manifest, RemoteLayout::Current).is_err());
    }

    #[test]
    fn validate_manifest_compat_accepts_legacy_manifest_without_db_compat() {
        let manifest = manifest_with(PROTOCOL_FORMAT, PROTOCOL_VERSION, None);
        assert!(validate_manifest_compat(&manifest, RemoteLayout::Legacy).is_ok());
    }

    #[test]
    fn validate_manifest_compat_rejects_current_manifest_with_wrong_db_compat() {
        let manifest = manifest_with(
            PROTOCOL_FORMAT,
            PROTOCOL_VERSION,
            Some(LEGACY_DB_COMPAT_VERSION),
        );
        assert!(validate_manifest_compat(&manifest, RemoteLayout::Current).is_err());
    }

    #[test]
    fn validate_manifest_compat_rejects_legacy_manifest_from_newer_db_generation() {
        let manifest = manifest_with(
            PROTOCOL_FORMAT,
            PROTOCOL_VERSION,
            Some(DB_COMPAT_VERSION + 1),
        );
        assert!(validate_manifest_compat(&manifest, RemoteLayout::Legacy).is_err());
    }

    #[test]
    fn effective_db_compat_version_defaults_legacy_layout_to_v5() {
        let manifest = manifest_with(PROTOCOL_FORMAT, PROTOCOL_VERSION, None);
        assert_eq!(
            effective_db_compat_version(&manifest, RemoteLayout::Legacy),
            Some(LEGACY_DB_COMPAT_VERSION)
        );
        assert_eq!(
            effective_db_compat_version(&manifest, RemoteLayout::Current),
            None
        );
    }

    #[test]
    fn normalize_device_name_returns_none_for_blank_input() {
        assert_eq!(normalize_device_name("   \n\t  "), None);
    }

    #[test]
    fn normalize_device_name_collapses_whitespace_and_drops_control_chars() {
        assert_eq!(
            normalize_device_name("  Mac\tBook \n Pro\u{0007} "),
            Some("Mac Book Pro".to_string())
        );
    }

    #[test]
    fn normalize_device_name_truncates_to_max_len() {
        let long = "a".repeat(80);
        assert_eq!(normalize_device_name(&long).map(|s| s.len()), Some(64));
    }

    #[test]
    fn manifest_serialization_uses_device_name_only() {
        let manifest = manifest_with(PROTOCOL_FORMAT, PROTOCOL_VERSION, Some(DB_COMPAT_VERSION));
        let value = serde_json::to_value(&manifest).expect("serialize manifest");
        assert!(
            value.get("deviceName").is_some(),
            "manifest should contain deviceName"
        );
        assert_eq!(
            value.get("dbCompatVersion").and_then(|v| v.as_u64()),
            Some(DB_COMPAT_VERSION as u64)
        );
        assert!(
            value.get("deviceId").is_none(),
            "manifest should not contain deviceId"
        );
    }

    #[test]
    fn validate_artifact_size_limit_rejects_oversized_artifacts() {
        let err = validate_artifact_size_limit("skills.zip", MAX_SYNC_ARTIFACT_BYTES + 1)
            .expect_err("artifact larger than limit should be rejected");
        assert!(
            err.to_string().contains("too large") || err.to_string().contains("超过"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_artifact_size_limit_accepts_limit_boundary() {
        assert!(validate_artifact_size_limit("skills.zip", MAX_SYNC_ARTIFACT_BYTES).is_ok());
    }

    #[test]
    fn verify_artifact_rejects_size_mismatch() {
        let meta = artifact("abc123", 100);
        let bytes = vec![0u8; 50];
        let err = verify_artifact(&bytes, "test.bin", &meta)
            .expect_err("size mismatch should be rejected");
        assert!(
            err.to_string().contains("mismatch") || err.to_string().contains("不匹配"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn verify_artifact_rejects_hash_mismatch() {
        let meta = ArtifactMeta {
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            size: 5,
        };
        let bytes = b"hello";
        let err = verify_artifact(bytes, "test.bin", &meta)
            .expect_err("hash mismatch should be rejected");
        assert!(
            err.to_string().contains("verification failed") || err.to_string().contains("校验失败"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn verify_artifact_accepts_matching_data() {
        let data = b"hello";
        let meta = ArtifactMeta {
            sha256: sha256_hex(data),
            size: data.len() as u64,
        };
        assert!(verify_artifact(data, "test.bin", &meta).is_ok());
    }

    #[test]
    #[serial]
    fn build_local_snapshot_produces_self_verifying_manifest() {
        let _home = TestHomeGuard::new();
        let db = crate::database::Database::memory().expect("create memory db");

        let snapshot = build_local_snapshot(&db).expect("build local snapshot");

        let manifest: SyncManifest = serde_json::from_slice(&snapshot.manifest_bytes)
            .expect("manifest bytes deserialize back into SyncManifest");
        assert_eq!(manifest.format, PROTOCOL_FORMAT);
        assert_eq!(manifest.version, PROTOCOL_VERSION);
        assert_eq!(manifest.db_compat_version, Some(DB_COMPAT_VERSION));
        assert_eq!(
            manifest.snapshot_id,
            compute_snapshot_id(&manifest.artifacts),
            "snapshot_id must derive from the artifact hashes"
        );
        assert_eq!(snapshot.manifest_hash, sha256_hex(&snapshot.manifest_bytes));

        let db_meta = manifest
            .artifacts
            .get(REMOTE_DB_SQL)
            .expect("db.sql artifact present");
        assert_eq!(db_meta.size, snapshot.db_sql.len() as u64);
        assert_eq!(db_meta.sha256, sha256_hex(&snapshot.db_sql));

        let zip_meta = manifest
            .artifacts
            .get(REMOTE_SKILLS_ZIP)
            .expect("skills.zip artifact present");
        assert_eq!(zip_meta.size, snapshot.skills_zip.len() as u64);
        assert_eq!(zip_meta.sha256, sha256_hex(&snapshot.skills_zip));
    }

    #[test]
    #[serial]
    fn build_local_snapshot_changes_identity_when_providers_change() {
        let _home = TestHomeGuard::new();
        let db = crate::database::Database::memory().expect("create memory db");

        let before = build_local_snapshot(&db).expect("build snapshot before provider change");
        save_baseline_provider(&db);
        let after = build_local_snapshot(&db).expect("build snapshot after provider change");

        assert_ne!(
            sha256_hex(&before.db_sql),
            sha256_hex(&after.db_sql),
            "a provider insert must change the exported db.sql"
        );

        let before_manifest: SyncManifest =
            serde_json::from_slice(&before.manifest_bytes).expect("deserialize before manifest");
        let after_manifest: SyncManifest =
            serde_json::from_slice(&after.manifest_bytes).expect("deserialize after manifest");
        assert_ne!(
            before_manifest.snapshot_id, after_manifest.snapshot_id,
            "a provider insert must produce a new snapshot identity"
        );
    }

    #[test]
    #[serial]
    fn apply_snapshot_restores_database_and_skills() {
        let _home = TestHomeGuard::new();
        let source = crate::database::Database::memory().expect("create source db");
        save_baseline_provider(&source);

        let ssot = crate::services::skill::SkillService::get_ssot_dir().expect("resolve ssot dir");
        std::fs::create_dir_all(ssot.join("baseline-skill")).expect("create skill dir");
        std::fs::write(ssot.join("baseline-skill").join("SKILL.md"), "hello")
            .expect("write skill file");

        let snapshot = build_local_snapshot(&source).expect("build local snapshot");

        let target = crate::database::Database::memory().expect("create target db");
        apply_snapshot(&target, &snapshot.db_sql, &snapshot.skills_zip)
            .expect("apply snapshot onto empty database");

        assert!(
            ssot.join("baseline-skill").join("SKILL.md").is_file(),
            "skills.zip contents must be restored into the SSOT dir"
        );
        let sql = target
            .export_sql_string_for_sync()
            .expect("export applied database");
        assert!(
            sql.contains("baseline-provider"),
            "applied database must contain the provider rows from the snapshot"
        );
    }

    #[test]
    #[serial]
    fn apply_snapshot_rejects_foreign_sql_and_rolls_back_skills() {
        let _home = TestHomeGuard::new();
        let db = crate::database::Database::memory().expect("create memory db");
        let snapshot = build_local_snapshot(&db).expect("build local snapshot");

        let ssot = crate::services::skill::SkillService::get_ssot_dir().expect("resolve ssot dir");
        std::fs::create_dir_all(ssot.join("keep-me")).expect("create skill dir");
        std::fs::write(ssot.join("keep-me").join("SKILL.md"), "keep").expect("write skill file");

        apply_snapshot(
            &db,
            b"definitely not a cc-switch export",
            &snapshot.skills_zip,
        )
        .expect_err("non cc-switch SQL must be rejected");

        assert!(
            ssot.join("keep-me").join("SKILL.md").is_file(),
            "a failed database import must roll skills back to their pre-apply state"
        );
    }

    #[tokio::test]
    async fn run_with_sync_lock_propagates_operation_results() {
        let ok = run_with_sync_lock(async { Ok::<u32, AppError>(7) }).await;
        assert_eq!(ok.expect("success value passes through"), 7);

        let err =
            run_with_sync_lock(async { Err::<u32, _>(AppError::Config("boom".to_string())) }).await;
        assert!(err.is_err(), "operation error passes through");
    }
}

// ─── Security & integrity tests (added by cloud-sync trust hardening) ───
//
// Kept in a separate module so the behavior-baseline `tests` block above stays
// byte-for-byte untouched.
#[cfg(test)]
mod security_tests {
    use super::*;
    use crate::provider::Provider;
    use serial_test::serial;

    struct TestHomeGuard {
        previous: Option<std::ffi::OsString>,
        _dir: tempfile::TempDir,
    }

    impl TestHomeGuard {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("create isolated test home");
            let previous = std::env::var_os("CC_SWITCH_TEST_HOME");
            std::env::set_var("CC_SWITCH_TEST_HOME", dir.path());
            Self {
                previous,
                _dir: dir,
            }
        }
    }

    impl Drop for TestHomeGuard {
        fn drop(&mut self) {
            match self.previous.as_ref() {
                Some(previous) => std::env::set_var("CC_SWITCH_TEST_HOME", previous),
                None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
            }
        }
    }

    fn manifest_with(payloads: &[(&str, &[u8])]) -> SyncManifest {
        let mut artifacts = BTreeMap::new();
        for (name, bytes) in payloads {
            artifacts.insert(
                (*name).to_string(),
                ArtifactMeta {
                    sha256: sha256_hex(bytes),
                    size: bytes.len() as u64,
                },
            );
        }
        SyncManifest {
            format: PROTOCOL_FORMAT.to_string(),
            version: PROTOCOL_VERSION,
            db_compat_version: Some(DB_COMPAT_VERSION),
            device_name: "device".to_string(),
            created_at: "2026-02-12T00:00:00Z".to_string(),
            snapshot_id: compute_snapshot_id(&artifacts),
            artifacts,
        }
    }

    #[test]
    fn plaintext_http_policy_blocks_public_but_allows_trusted_hosts() {
        // Secure or trusted endpoints always pass.
        for url in [
            "https://dav.example.com",
            "http://localhost:5005",
            "http://127.0.0.1:5005",
            "http://[::1]:5005",
            "http://192.168.1.10:5005",
            "http://10.0.0.5:9000",
            "http://172.16.4.2",
        ] {
            assert!(
                enforce_secure_transport(url, false).is_ok(),
                "{url} should be allowed without an exemption"
            );
        }

        // Public plaintext HTTP is blocked by default.
        let err = enforce_secure_transport("http://nas.example.com:5005", false)
            .expect_err("public plaintext HTTP must be blocked");
        assert!(
            err.to_string().contains("明文") || err.to_string().contains("Plaintext"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn plaintext_http_policy_honours_explicit_exemption() {
        assert!(enforce_secure_transport("http://nas.example.com:5005", true).is_ok());
    }

    #[test]
    fn plaintext_http_policy_ignores_non_http_and_unparsable_input() {
        assert!(enforce_secure_transport("", false).is_ok());
        assert!(enforce_secure_transport("not a url", false).is_ok());
        assert!(enforce_secure_transport("ftp://nas.example.com", false).is_ok());
    }

    #[test]
    fn manifest_integrity_rejects_tampered_snapshot_id() {
        let manifest = manifest_with(&[(REMOTE_DB_SQL, b"db"), (REMOTE_SKILLS_ZIP, b"zip")]);
        assert!(verify_manifest_integrity(&manifest).is_ok());

        let mut tampered = manifest.clone();
        tampered.snapshot_id = "deadbeef".to_string();
        assert!(
            verify_manifest_integrity(&tampered).is_err(),
            "a snapshot_id that no longer matches the artifact hashes must be rejected"
        );
    }

    #[test]
    #[serial]
    fn apply_remote_snapshot_rejects_hash_mismatch_before_applying() {
        let _home = TestHomeGuard::new();
        let db = crate::database::Database::memory().expect("create memory db");
        let snapshot = build_local_snapshot(&db).expect("build local snapshot");
        let manifest: SyncManifest =
            serde_json::from_slice(&snapshot.manifest_bytes).expect("deserialize manifest");

        // Tamper with the stored db payload: size/hash no longer matches.
        let mut tampered_db = snapshot.db_sql.clone();
        tampered_db.push(b'x');
        apply_remote_snapshot(&db, &manifest, &tampered_db, &snapshot.skills_zip, None)
            .expect_err("hash mismatch must be rejected");

        // The intact payload still applies cleanly.
        apply_remote_snapshot(&db, &manifest, &snapshot.db_sql, &snapshot.skills_zip, None)
            .expect("verified snapshot applies");
    }

    #[test]
    #[serial]
    fn encrypted_snapshot_round_trips_through_protocol_layer() {
        let _home = TestHomeGuard::new();
        let source = crate::database::Database::memory().expect("create source db");
        source
            .save_provider(
                "claude",
                &Provider::with_id(
                    "encrypted-provider".to_string(),
                    "Encrypted Provider".to_string(),
                    serde_json::json!({ "claude_base_url": "https://example.invalid" }),
                    None,
                ),
            )
            .expect("save provider");

        let snapshot =
            build_local_snapshot_with_crypto(&source, Some("correct horse")).expect("encrypt");
        assert!(
            sync_crypto::is_encrypted_blob(&snapshot.db_sql),
            "opt-in encryption must encrypt the db artifact before upload"
        );
        assert!(
            sync_crypto::is_encrypted_blob(&snapshot.skills_zip),
            "opt-in encryption must encrypt the skills artifact before upload"
        );

        let manifest: SyncManifest =
            serde_json::from_slice(&snapshot.manifest_bytes).expect("deserialize manifest");

        let target = crate::database::Database::memory().expect("create target db");
        // Wrong password is rejected without mutating the target.
        apply_remote_snapshot(
            &target,
            &manifest,
            &snapshot.db_sql,
            &snapshot.skills_zip,
            Some("wrong password"),
        )
        .expect_err("wrong password must be rejected");

        // Correct password decrypts and restores.
        apply_remote_snapshot(
            &target,
            &manifest,
            &snapshot.db_sql,
            &snapshot.skills_zip,
            Some("correct horse"),
        )
        .expect("correct password restores the snapshot");
        let sql = target
            .export_sql_string_for_sync()
            .expect("export applied database");
        assert!(
            sql.contains("encrypted-provider"),
            "decrypted snapshot must restore the provider rows"
        );
    }

    #[test]
    #[serial]
    fn encrypted_snapshot_without_password_is_rejected() {
        let _home = TestHomeGuard::new();
        let db = crate::database::Database::memory().expect("create memory db");
        let snapshot = build_local_snapshot_with_crypto(&db, Some("pw")).expect("encrypt");
        let manifest: SyncManifest =
            serde_json::from_slice(&snapshot.manifest_bytes).expect("deserialize manifest");
        let target = crate::database::Database::memory().expect("create target db");

        let err = apply_remote_snapshot(
            &target,
            &manifest,
            &snapshot.db_sql,
            &snapshot.skills_zip,
            None,
        )
        .expect_err("encrypted snapshot without a password must be rejected");
        assert!(
            err.to_string().contains("加密") || err.to_string().contains("encrypted"),
            "unexpected error: {err}"
        );
    }

    /// Regression guard for the encryption-downgrade path: once the user has
    /// enabled E2E, a magic-less (plaintext) remote must be refused rather than
    /// silently applied, and the local database must stay untouched.
    ///
    /// The `password=None` plaintext path is still exercised by
    /// [`apply_remote_snapshot_rejects_hash_mismatch_before_applying`], which
    /// applies an intact plaintext snapshot successfully.
    #[test]
    #[serial]
    fn plaintext_remote_snapshot_is_rejected_when_encryption_enabled() {
        let _home = TestHomeGuard::new();
        let source = crate::database::Database::memory().expect("create source db");
        // Plaintext snapshot: exactly what a legacy (pre-E2E) remote serves.
        let snapshot = build_local_snapshot(&source).expect("build plaintext snapshot");
        let manifest: SyncManifest =
            serde_json::from_slice(&snapshot.manifest_bytes).expect("deserialize manifest");

        // Seed a local marker row that must survive the rejected apply.
        let target = crate::database::Database::memory().expect("create target db");
        target
            .save_provider(
                "claude",
                &Provider::with_id(
                    "local-marker".to_string(),
                    "Local Marker".to_string(),
                    serde_json::json!({ "claude_base_url": "https://example.invalid" }),
                    None,
                ),
            )
            .expect("seed local marker");

        let err = apply_remote_snapshot(
            &target,
            &manifest,
            &snapshot.db_sql,
            &snapshot.skills_zip,
            Some("correct horse"),
        )
        .expect_err("plaintext remote must be rejected once E2E is enabled");
        assert!(
            err.to_string().contains("未加密") || err.to_string().contains("not encrypted"),
            "unexpected error: {err}"
        );

        let sql = target
            .export_sql_string_for_sync()
            .expect("export target database");
        assert!(
            sql.contains("local-marker"),
            "a rejected plaintext snapshot must leave the local database untouched"
        );
    }

    /// The "encryption enabled but password empty" state (reachable by hand-
    /// editing settings.json) must surface an error from the single resolver
    /// instead of resolving to `None` and silently uploading plaintext.
    #[test]
    fn encryption_password_resolver_enforces_empty_password_policy() {
        use crate::settings::{S3SyncSettings, WebDavSyncSettings};

        // Disabled → plaintext, regardless of any stored password.
        let disabled = WebDavSyncSettings::default();
        assert!(matches!(disabled.encryption_password(), Ok(None)));

        // Enabled + non-empty → the password.
        let enabled = WebDavSyncSettings {
            encryption_enabled: true,
            encryption_password: "correct horse".to_string(),
            ..WebDavSyncSettings::default()
        };
        assert_eq!(
            enabled.encryption_password().expect("non-empty password"),
            Some("correct horse")
        );

        // Enabled + empty → Err (the silent-plaintext bypass is closed).
        let empty = WebDavSyncSettings {
            encryption_enabled: true,
            ..WebDavSyncSettings::default()
        };
        let err = empty
            .encryption_password()
            .expect_err("enabled encryption with an empty password must error");
        assert!(
            err.to_string().contains("口令") || err.to_string().contains("password"),
            "unexpected error: {err}"
        );

        // S3 shares the exact same policy.
        let s3_empty = S3SyncSettings {
            encryption_enabled: true,
            ..S3SyncSettings::default()
        };
        assert!(s3_empty.encryption_password().is_err());
    }
}

//! WebDAV v2 sync protocol layer with DB compatibility subdirectories.
//!
//! Implements manifest-based synchronization on top of the HTTP transport
//! primitives in [`super::webdav`]. The shared snapshot flows live in
//! [`super::sync_transport`]; this module provides the WebDAV adapter.
//! Artifact set: `db.sql` + `skills.zip`.

use serde_json::{json, Value};

use crate::error::AppError;
use crate::services::sync_protocol::{
    persist_sync_success_with, RemoteLayout, SyncSecuritySettings, DB_COMPAT_VERSION,
    PROTOCOL_VERSION,
};
use crate::services::sync_transport::{self, SyncTransport};
use crate::services::webdav::{
    auth_from_credentials, build_remote_url, ensure_remote_directories, get_bytes, head_etag,
    path_segments, put_bytes, test_connection, WebDavAuth,
};
use crate::settings::{update_webdav_sync_status, WebDavSyncSettings};

pub(crate) use super::sync_protocol::run_with_sync_lock;

#[cfg(test)]
pub(crate) fn sync_mutex() -> &'static tokio::sync::Mutex<()> {
    super::sync_protocol::sync_mutex()
}

pub(crate) mod archive;

// ─── Public API ──────────────────────────────────────────────

/// Check WebDAV connectivity and ensure remote directory structure.
pub async fn check_connection(settings: &WebDavSyncSettings) -> Result<(), AppError> {
    settings.validate()?;
    sync_transport::check_connection(
        &WebDavTransport::new(settings),
        settings.allow_plaintext_http(),
    )
    .await
}

/// Upload local snapshot (db + skills) to remote.
pub async fn upload(
    db: &crate::database::Database,
    settings: &mut WebDavSyncSettings,
) -> Result<Value, AppError> {
    settings.validate()?;
    let transport = WebDavTransport::new(settings);
    sync_transport::upload_snapshot(&transport, db, settings, persist_sync_success).await
}

/// Download remote snapshot and apply to local database + skills.
pub async fn download(
    db: &crate::database::Database,
    settings: &mut WebDavSyncSettings,
) -> Result<Value, AppError> {
    settings.validate()?;
    let transport = WebDavTransport::new(settings);
    sync_transport::download_snapshot(&transport, db, settings, persist_sync_success).await
}

/// Fetch remote manifest info without downloading artifacts.
pub async fn fetch_remote_info(settings: &WebDavSyncSettings) -> Result<Option<Value>, AppError> {
    settings.validate()?;
    sync_transport::fetch_remote_info_payload(
        &WebDavTransport::new(settings),
        settings.allow_plaintext_http(),
    )
    .await
}

// ─── Sync status persistence ─────────────────────────────────

fn persist_sync_success(
    settings: &mut WebDavSyncSettings,
    manifest_hash: String,
    etag: Option<String>,
) -> Result<(), AppError> {
    persist_sync_success_with(settings, manifest_hash, etag, update_webdav_sync_status)
}

// ─── Remote path helpers ─────────────────────────────────────

fn remote_dir_segments(settings: &WebDavSyncSettings, layout: RemoteLayout) -> Vec<String> {
    let mut segs = Vec::new();
    segs.extend(path_segments(&settings.remote_root).map(str::to_string));
    segs.push(format!("v{PROTOCOL_VERSION}"));
    if layout == RemoteLayout::Current {
        segs.push(format!("db-v{DB_COMPAT_VERSION}"));
    }
    segs.extend(path_segments(&settings.profile).map(str::to_string));
    segs
}

fn remote_file_url(
    settings: &WebDavSyncSettings,
    layout: RemoteLayout,
    file_name: &str,
) -> Result<String, AppError> {
    let mut segs = remote_dir_segments(settings, layout);
    segs.extend(path_segments(file_name).map(str::to_string));
    build_remote_url(&settings.base_url, &segs)
}

fn remote_dir_display(settings: &WebDavSyncSettings, layout: RemoteLayout) -> String {
    let segs = remote_dir_segments(settings, layout);
    format!("/{}", segs.join("/"))
}

// ─── WebDAV transport adapter ────────────────────────────────

/// [`SyncTransport`] adapter over the WebDAV HTTP primitives.
struct WebDavTransport {
    settings: WebDavSyncSettings,
    auth: WebDavAuth,
}

impl WebDavTransport {
    fn new(settings: &WebDavSyncSettings) -> Self {
        Self {
            settings: settings.clone(),
            auth: auth_from_credentials(&settings.username, &settings.password),
        }
    }
}

impl SyncTransport for WebDavTransport {
    fn log_tag(&self) -> &'static str {
        "WebDAV"
    }

    async fn probe(&self) -> Result<(), AppError> {
        test_connection(&self.settings.base_url, &self.auth).await
    }

    async fn ensure_layout(&self, layout: RemoteLayout) -> Result<(), AppError> {
        let dir_segs = remote_dir_segments(&self.settings, layout);
        ensure_remote_directories(&self.settings.base_url, &dir_segs, &self.auth).await
    }

    async fn put_artifact(
        &self,
        layout: RemoteLayout,
        name: &str,
        bytes: &[u8],
        content_type: &str,
    ) -> Result<(), AppError> {
        let url = remote_file_url(&self.settings, layout, name)?;
        put_bytes(&url, &self.auth, bytes.to_vec(), content_type).await
    }

    async fn get_artifact(
        &self,
        layout: RemoteLayout,
        name: &str,
        max_bytes: usize,
    ) -> Result<Option<sync_transport::ArtifactBytes>, AppError> {
        let url = remote_file_url(&self.settings, layout, name)?;
        get_bytes(&url, &self.auth, max_bytes).await
    }

    async fn head_artifact(
        &self,
        layout: RemoteLayout,
        name: &str,
    ) -> Result<Option<String>, AppError> {
        let url = remote_file_url(&self.settings, layout, name)?;
        head_etag(&url, &self.auth).await
    }

    fn supports_legacy_layout(&self) -> bool {
        true
    }

    fn display_path(&self, layout: RemoteLayout) -> String {
        remote_dir_display(&self.settings, layout)
    }

    fn remote_endpoint(&self) -> &str {
        &self.settings.base_url
    }

    fn key_remote_empty(&self) -> &'static str {
        "webdav.sync.remote_empty"
    }

    fn key_manifest_missing_artifact(&self) -> &'static str {
        "webdav.sync.manifest_missing_artifact"
    }

    fn key_remote_missing_artifact(&self) -> &'static str {
        "webdav.sync.remote_missing_artifact"
    }

    fn download_extras(&self, layout: RemoteLayout) -> Vec<(&'static str, Value)> {
        vec![
            ("sourceLayout", json!(layout.as_str())),
            (
                "sourcePath",
                json!(remote_dir_display(&self.settings, layout)),
            ),
        ]
    }
}

// ─── Tests ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_dir_segments_uses_current_layout() {
        let settings = WebDavSyncSettings {
            remote_root: "cc-switch-sync".to_string(),
            profile: "default".to_string(),
            ..WebDavSyncSettings::default()
        };
        let segs = remote_dir_segments(&settings, RemoteLayout::Current);
        assert_eq!(segs, vec!["cc-switch-sync", "v2", "db-v6", "default"]);
    }

    #[test]
    fn remote_dir_segments_uses_legacy_layout() {
        let settings = WebDavSyncSettings {
            remote_root: "cc-switch-sync".to_string(),
            profile: "default".to_string(),
            ..WebDavSyncSettings::default()
        };
        let segs = remote_dir_segments(&settings, RemoteLayout::Legacy);
        assert_eq!(segs, vec!["cc-switch-sync", "v2", "default"]);
    }
}

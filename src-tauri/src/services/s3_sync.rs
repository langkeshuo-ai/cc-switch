//! S3 v2 sync protocol layer.
//!
//! Implements manifest-based synchronization on top of the S3 transport
//! primitives in [`super::s3`]. The shared snapshot flows live in
//! [`super::sync_transport`]; this module provides the S3 adapter.
//! Artifact set: `db.sql` + `skills.zip`.

use serde_json::Value;

use crate::error::AppError;
use crate::services::s3::{self, S3Credentials};
use crate::services::sync_protocol::{
    persist_sync_success_with, RemoteLayout, DB_COMPAT_VERSION, PROTOCOL_VERSION,
};
use crate::services::sync_transport::{self, SyncTransport};
use crate::settings::{update_s3_sync_status, S3SyncSettings};

pub(crate) use super::sync_protocol::run_with_sync_lock;

#[cfg(test)]
pub(crate) fn sync_mutex() -> &'static tokio::sync::Mutex<()> {
    super::sync_protocol::sync_mutex()
}

// ─── Public API ──────────────────────────────────────────────

/// Check S3 connectivity by issuing a HEAD request against the bucket.
pub async fn check_connection(settings: &S3SyncSettings) -> Result<(), AppError> {
    settings.validate()?;
    sync_transport::check_connection(&S3Transport::new(settings)).await
}

/// Upload local snapshot (db + skills) to remote S3.
pub async fn upload(
    db: &crate::database::Database,
    settings: &mut S3SyncSettings,
) -> Result<Value, AppError> {
    settings.validate()?;
    let transport = S3Transport::new(settings);
    sync_transport::upload_snapshot(&transport, db, settings, persist_sync_success).await
}

/// Download remote snapshot and apply to local database + skills.
pub async fn download(
    db: &crate::database::Database,
    settings: &mut S3SyncSettings,
) -> Result<Value, AppError> {
    settings.validate()?;
    let transport = S3Transport::new(settings);
    sync_transport::download_snapshot(&transport, db, settings, persist_sync_success).await
}

/// Fetch remote manifest info without downloading artifacts.
pub async fn fetch_remote_info(settings: &S3SyncSettings) -> Result<Option<Value>, AppError> {
    settings.validate()?;
    sync_transport::fetch_remote_info_payload(&S3Transport::new(settings)).await
}

// ─── Sync status persistence ─────────────────────────────────

fn persist_sync_success(
    settings: &mut S3SyncSettings,
    manifest_hash: String,
    etag: Option<String>,
) -> Result<(), AppError> {
    persist_sync_success_with(settings, manifest_hash, etag, update_s3_sync_status)
}

// ─── S3 key helpers ──────────────────────────────────────────

/// Build the S3 object key for a given artifact.
///
/// Format: `{remote_root}/v{PROTOCOL_VERSION}/db-v{DB_COMPAT_VERSION}/{profile}/{artifact}`
/// Example: `cc-switch-sync/v2/db-v6/default/manifest.json`
fn s3_key(settings: &S3SyncSettings, artifact: &str) -> String {
    format!(
        "{}/v{}/db-v{}/{}/{}",
        settings.remote_root, PROTOCOL_VERSION, DB_COMPAT_VERSION, settings.profile, artifact
    )
}

fn s3_dir_display(settings: &S3SyncSettings) -> String {
    format!(
        "{}/v{}/db-v{}/{}",
        settings.remote_root, PROTOCOL_VERSION, DB_COMPAT_VERSION, settings.profile
    )
}

fn creds_for(settings: &S3SyncSettings) -> S3Credentials {
    S3Credentials {
        access_key_id: settings.access_key_id.clone(),
        secret_access_key: settings.secret_access_key.clone(),
        region: settings.region.clone(),
        bucket: settings.bucket.clone(),
        endpoint: settings.endpoint.clone(),
    }
}

// ─── S3 transport adapter ────────────────────────────────────

/// [`SyncTransport`] adapter over the S3 SigV4 primitives.
struct S3Transport {
    settings: S3SyncSettings,
}

impl S3Transport {
    fn new(settings: &S3SyncSettings) -> Self {
        Self {
            settings: settings.clone(),
        }
    }
}

impl SyncTransport for S3Transport {
    fn log_tag(&self) -> &'static str {
        "S3"
    }

    async fn probe(&self) -> Result<(), AppError> {
        s3::test_connection(&creds_for(&self.settings)).await
    }

    async fn put_artifact(
        &self,
        layout: RemoteLayout,
        name: &str,
        bytes: &[u8],
        content_type: &str,
    ) -> Result<(), AppError> {
        let _ = layout;
        let key = s3_key(&self.settings, name);
        s3::put_object(
            &creds_for(&self.settings),
            &key,
            bytes.to_vec(),
            content_type,
        )
        .await
    }

    async fn get_artifact(
        &self,
        layout: RemoteLayout,
        name: &str,
        max_bytes: usize,
    ) -> Result<Option<sync_transport::ArtifactBytes>, AppError> {
        let _ = layout;
        let key = s3_key(&self.settings, name);
        s3::get_object(&creds_for(&self.settings), &key, max_bytes).await
    }

    async fn head_artifact(
        &self,
        layout: RemoteLayout,
        name: &str,
    ) -> Result<Option<String>, AppError> {
        let _ = layout;
        let key = s3_key(&self.settings, name);
        s3::head_object(&creds_for(&self.settings), &key).await
    }

    fn display_path(&self, layout: RemoteLayout) -> String {
        let _ = layout;
        s3_dir_display(&self.settings)
    }

    fn key_remote_empty(&self) -> &'static str {
        "s3.sync.remote_empty"
    }

    fn key_manifest_missing_artifact(&self) -> &'static str {
        "s3.sync.manifest_missing_artifact"
    }

    fn key_remote_missing_artifact(&self) -> &'static str {
        "s3.sync.remote_missing_artifact"
    }
}

// ─── Tests ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_settings() -> S3SyncSettings {
        S3SyncSettings {
            remote_root: "cc-switch-sync".to_string(),
            profile: "default".to_string(),
            ..S3SyncSettings::default()
        }
    }

    #[test]
    fn s3_key_uses_v2_and_correct_format() {
        let settings = test_settings();
        let key = s3_key(&settings, "manifest.json");
        assert_eq!(key, "cc-switch-sync/v2/db-v6/default/manifest.json");
    }

    #[test]
    fn s3_key_with_custom_profile() {
        let settings = S3SyncSettings {
            remote_root: "my-root".to_string(),
            profile: "work".to_string(),
            ..S3SyncSettings::default()
        };
        assert_eq!(s3_key(&settings, "db.sql"), "my-root/v2/db-v6/work/db.sql");
    }

    #[test]
    fn s3_key_matches_expected_pattern() {
        let settings = test_settings();
        let key = s3_key(&settings, "skills.zip");
        // Should follow {remote_root}/v{version}/db-v{db}/{profile}/{artifact}
        let parts: Vec<&str> = key.splitn(5, '/').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[0], "cc-switch-sync");
        assert_eq!(parts[1], "v2");
        assert_eq!(parts[2], "db-v6");
        assert_eq!(parts[3], "default");
        assert_eq!(parts[4], "skills.zip");
    }

    #[test]
    fn sync_mutex_is_singleton() {
        let m1 = sync_mutex();
        let m2 = sync_mutex();
        assert!(
            std::ptr::eq(m1, m2),
            "sync_mutex must return the same instance"
        );
    }

    #[test]
    fn creds_for_maps_all_fields() {
        let settings = S3SyncSettings {
            access_key_id: "AKID".to_string(),
            secret_access_key: "SECRET".to_string(),
            region: "us-west-2".to_string(),
            bucket: "my-bucket".to_string(),
            endpoint: "minio.local:9000".to_string(),
            ..S3SyncSettings::default()
        };
        let creds = creds_for(&settings);
        assert_eq!(creds.access_key_id, "AKID");
        assert_eq!(creds.secret_access_key, "SECRET");
        assert_eq!(creds.region, "us-west-2");
        assert_eq!(creds.bucket, "my-bucket");
        assert_eq!(creds.endpoint, "minio.local:9000");
    }
}

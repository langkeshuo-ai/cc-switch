//! Transport abstraction for the v2 sync protocol.
//!
//! [`SyncTransport`] captures the per-transport primitives (WebDAV HTTP verbs,
//! S3 SigV4 object storage) so the snapshot upload/download flows in this
//! module are written once for all transports. Polymorphic points that stay
//! per-transport: URL/key construction, directory creation, the legacy layout
//! fallback (WebDAV only), and the user-facing error keys.

use std::collections::BTreeMap;
use std::future::Future;

use serde_json::{json, Value};

use crate::database::Database;
use crate::error::AppError;
use crate::services::sync_protocol::{
    apply_remote_snapshot, build_local_snapshot, build_local_snapshot_with_crypto,
    effective_db_compat_version, enforce_secure_transport, localized,
    persist_sync_success_best_effort, sha256_hex, validate_artifact_size_limit,
    validate_manifest_compat, ArtifactMeta, RemoteLayout, SyncManifest, SyncSecuritySettings,
    MAX_MANIFEST_BYTES, MAX_SYNC_ARTIFACT_BYTES, REMOTE_DB_SQL, REMOTE_MANIFEST, REMOTE_SKILLS_ZIP,
};

// ─── Types ───────────────────────────────────────────────────

/// Downloaded artifact bytes paired with the optional ETag from the response.
pub(crate) type ArtifactBytes = (Vec<u8>, Option<String>);

pub(crate) struct RemoteSnapshot {
    pub(crate) layout: RemoteLayout,
    pub(crate) manifest: SyncManifest,
    pub(crate) manifest_bytes: Vec<u8>,
    pub(crate) manifest_etag: Option<String>,
}

// ─── Transport contract ──────────────────────────────────────

pub(crate) trait SyncTransport {
    /// Log prefix used by flow-level diagnostics (e.g. "WebDAV", "S3").
    fn log_tag(&self) -> &'static str;

    /// Probe remote connectivity (transport-level test).
    fn probe(&self) -> impl Future<Output = Result<(), AppError>> + Send;

    /// Upload one artifact. Artifacts first, manifest last.
    fn put_artifact(
        &self,
        layout: RemoteLayout,
        name: &str,
        bytes: &[u8],
        content_type: &str,
    ) -> impl Future<Output = Result<(), AppError>> + Send;

    /// Download one artifact, returning `None` when it does not exist (404).
    fn get_artifact(
        &self,
        layout: RemoteLayout,
        name: &str,
        max_bytes: usize,
    ) -> impl Future<Output = Result<Option<ArtifactBytes>, AppError>> + Send;

    /// Retrieve the ETag of one artifact via HEAD. Returns `None` on 404.
    fn head_artifact(
        &self,
        layout: RemoteLayout,
        name: &str,
    ) -> impl Future<Output = Result<Option<String>, AppError>> + Send;

    /// Create the remote directory structure for `layout`.
    /// No-op for flat object stores.
    fn ensure_layout(
        &self,
        layout: RemoteLayout,
    ) -> impl Future<Output = Result<(), AppError>> + Send {
        let _ = layout;
        std::future::ready(Ok(()))
    }

    /// Whether this transport also serves the legacy manifest layout.
    fn supports_legacy_layout(&self) -> bool {
        false
    }

    /// Human-readable remote directory for UI payloads.
    fn display_path(&self, layout: RemoteLayout) -> String;

    /// Remote endpoint URL subject to the plaintext-HTTP policy. Returning an
    /// empty string (the default) means "no transport-level endpoint to check".
    fn remote_endpoint(&self) -> &str {
        ""
    }

    /// Localization keys for flow-level errors (per-transport `webdav.sync.*`
    /// / `s3.sync.*` namespaces).
    fn key_remote_empty(&self) -> &'static str;
    fn key_manifest_missing_artifact(&self) -> &'static str;
    fn key_remote_missing_artifact(&self) -> &'static str;

    /// Extra fields merged into the download result payload (e.g. WebDAV
    /// `sourceLayout` / `sourcePath` hints for the frontend).
    fn download_extras(&self, layout: RemoteLayout) -> Vec<(&'static str, Value)> {
        let _ = layout;
        Vec::new()
    }
}

// ─── Shared flows ────────────────────────────────────────────

/// Probe connectivity and prepare the current remote layout.
///
/// The plaintext-HTTP policy is enforced first so a blocked endpoint fails
/// before any credentials are sent over the wire.
pub(crate) async fn check_connection<T: SyncTransport>(
    t: &T,
    allow_plaintext_http: bool,
) -> Result<(), AppError> {
    enforce_secure_transport(t.remote_endpoint(), allow_plaintext_http)?;
    t.probe().await?;
    t.ensure_layout(RemoteLayout::Current).await
}

/// Upload local snapshot (db + skills) to the remote.
pub(crate) async fn upload_snapshot<T, S, F>(
    t: &T,
    db: &Database,
    settings: &mut S,
    persist_fn: F,
) -> Result<Value, AppError>
where
    T: SyncTransport,
    S: SyncSecuritySettings,
    F: FnOnce(&mut S, String, Option<String>) -> Result<(), AppError>,
{
    enforce_secure_transport(t.remote_endpoint(), settings.allow_plaintext_http())?;
    t.ensure_layout(RemoteLayout::Current).await?;
    let snapshot = match settings.encryption_password()? {
        Some(password) => build_local_snapshot_with_crypto(db, Some(password))?,
        None => build_local_snapshot(db)?,
    };

    // Upload order: artifacts first, manifest last (best-effort consistency)
    t.put_artifact(
        RemoteLayout::Current,
        REMOTE_DB_SQL,
        &snapshot.db_sql,
        "application/sql",
    )
    .await?;
    t.put_artifact(
        RemoteLayout::Current,
        REMOTE_SKILLS_ZIP,
        &snapshot.skills_zip,
        "application/zip",
    )
    .await?;
    t.put_artifact(
        RemoteLayout::Current,
        REMOTE_MANIFEST,
        &snapshot.manifest_bytes,
        "application/json",
    )
    .await?;

    // Fetch etag (best-effort, don't fail the upload)
    let etag = match t
        .head_artifact(RemoteLayout::Current, REMOTE_MANIFEST)
        .await
    {
        Ok(e) => e,
        Err(e) => {
            log::debug!("[{}] Failed to fetch ETag after upload: {e}", t.log_tag());
            None
        }
    };

    let _persisted =
        persist_sync_success_best_effort(settings, snapshot.manifest_hash, etag, persist_fn);
    Ok(json!({ "status": "uploaded" }))
}

/// Download the remote snapshot and apply it to the local database + skills.
pub(crate) async fn download_snapshot<T, S, F>(
    t: &T,
    db: &Database,
    settings: &mut S,
    persist_fn: F,
) -> Result<Value, AppError>
where
    T: SyncTransport,
    S: SyncSecuritySettings,
    F: FnOnce(&mut S, String, Option<String>) -> Result<(), AppError>,
{
    enforce_secure_transport(t.remote_endpoint(), settings.allow_plaintext_http())?;

    let snapshot = find_remote_snapshot(t).await?.ok_or_else(|| {
        localized(
            t.key_remote_empty(),
            "远端没有可下载的同步数据",
            "No downloadable sync data found on the remote.",
        )
    })?;

    validate_manifest_compat(&snapshot.manifest, snapshot.layout)?;

    // Download the raw (possibly encrypted) artifacts; verification + decryption
    // happen together in `apply_remote_snapshot` before any local mutation.
    let db_sql = download_artifact(
        t,
        snapshot.layout,
        REMOTE_DB_SQL,
        &snapshot.manifest.artifacts,
    )
    .await?;
    let skills_zip = download_artifact(
        t,
        snapshot.layout,
        REMOTE_SKILLS_ZIP,
        &snapshot.manifest.artifacts,
    )
    .await?;

    // Apply snapshot (verify manifest + artifact hashes, decrypt, then restore)
    apply_remote_snapshot(
        db,
        &snapshot.manifest,
        &db_sql,
        &skills_zip,
        settings.encryption_password()?,
    )?;

    let manifest_hash = sha256_hex(&snapshot.manifest_bytes);
    let _persisted = persist_sync_success_best_effort(
        settings,
        manifest_hash,
        snapshot.manifest_etag,
        persist_fn,
    );

    let mut payload = serde_json::Map::new();
    payload.insert(
        "status".to_string(),
        Value::String("downloaded".to_string()),
    );
    for (key, value) in t.download_extras(snapshot.layout) {
        payload.insert(key.to_string(), value);
    }
    Ok(Value::Object(payload))
}

/// Fetch remote manifest info without downloading artifacts.
///
/// This flow still transmits transport credentials (WebDAV Basic / S3 SigV4),
/// so it enforces the same plaintext-HTTP policy as upload/download *before*
/// issuing any request.
pub(crate) async fn fetch_remote_info_payload<T: SyncTransport>(
    t: &T,
    allow_plaintext_http: bool,
) -> Result<Option<Value>, AppError> {
    enforce_secure_transport(t.remote_endpoint(), allow_plaintext_http)?;
    let Some(snapshot) = find_remote_snapshot(t).await? else {
        return Ok(None);
    };
    let compatible = validate_manifest_compat(&snapshot.manifest, snapshot.layout).is_ok();
    let db_compat_version = effective_db_compat_version(&snapshot.manifest, snapshot.layout);

    let payload = json!({
        "deviceName": snapshot.manifest.device_name,
        "createdAt": snapshot.manifest.created_at,
        "snapshotId": snapshot.manifest.snapshot_id,
        "version": snapshot.manifest.version,
        "protocolVersion": snapshot.manifest.version,
        "dbCompatVersion": db_compat_version,
        "compatible": compatible,
        "artifacts": snapshot.manifest.artifacts.keys().collect::<Vec<_>>(),
        "layout": snapshot.layout.as_str(),
        "remotePath": t.display_path(snapshot.layout),
    });

    Ok(Some(payload))
}

// ─── Manifest lookup ─────────────────────────────────────────

/// Look up the remote snapshot: current layout first, then the legacy layout
/// for transports that still serve it (WebDAV).
async fn find_remote_snapshot<T: SyncTransport>(t: &T) -> Result<Option<RemoteSnapshot>, AppError> {
    if let Some(snapshot) = fetch_remote_snapshot(t, RemoteLayout::Current).await? {
        return Ok(Some(snapshot));
    }
    if t.supports_legacy_layout() {
        return fetch_remote_snapshot(t, RemoteLayout::Legacy).await;
    }
    Ok(None)
}

async fn fetch_remote_snapshot<T: SyncTransport>(
    t: &T,
    layout: RemoteLayout,
) -> Result<Option<RemoteSnapshot>, AppError> {
    let Some((manifest_bytes, manifest_etag)) = t
        .get_artifact(layout, REMOTE_MANIFEST, MAX_MANIFEST_BYTES)
        .await?
    else {
        return Ok(None);
    };

    let manifest: SyncManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|e| AppError::Json {
            path: REMOTE_MANIFEST.to_string(),
            source: e,
        })?;

    Ok(Some(RemoteSnapshot {
        layout,
        manifest,
        manifest_bytes,
        manifest_etag,
    }))
}

// ─── Download ────────────────────────────────────────────────

/// Download one artifact and enforce the manifest's size limit.
///
/// Hash verification is intentionally left to `apply_remote_snapshot` so the
/// check runs in the shared protocol layer immediately before applying.
async fn download_artifact<T: SyncTransport>(
    t: &T,
    layout: RemoteLayout,
    artifact_name: &str,
    artifacts: &BTreeMap<String, ArtifactMeta>,
) -> Result<Vec<u8>, AppError> {
    let meta = artifacts.get(artifact_name).ok_or_else(|| {
        localized(
            t.key_manifest_missing_artifact(),
            format!("manifest 中缺少 artifact: {artifact_name}"),
            format!("Manifest missing artifact: {artifact_name}"),
        )
    })?;
    validate_artifact_size_limit(artifact_name, meta.size)?;

    let (bytes, _) = t
        .get_artifact(layout, artifact_name, MAX_SYNC_ARTIFACT_BYTES as usize)
        .await?
        .ok_or_else(|| {
            localized(
                t.key_remote_missing_artifact(),
                format!("远端缺少 artifact 文件: {artifact_name}"),
                format!("Remote artifact file missing: {artifact_name}"),
            )
        })?;

    Ok(bytes)
}

// ─── Tests ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory transport stub: no network, no credentials. It exercises the
    /// transport-policy gate and the encryption-password gate, which both run
    /// before any real request would be issued.
    struct StubTransport {
        endpoint: String,
    }

    impl SyncTransport for StubTransport {
        fn log_tag(&self) -> &'static str {
            "Stub"
        }

        async fn probe(&self) -> Result<(), AppError> {
            Ok(())
        }

        async fn put_artifact(
            &self,
            _layout: RemoteLayout,
            _name: &str,
            _bytes: &[u8],
            _content_type: &str,
        ) -> Result<(), AppError> {
            Ok(())
        }

        async fn get_artifact(
            &self,
            _layout: RemoteLayout,
            _name: &str,
            _max_bytes: usize,
        ) -> Result<Option<ArtifactBytes>, AppError> {
            Ok(None)
        }

        async fn head_artifact(
            &self,
            _layout: RemoteLayout,
            _name: &str,
        ) -> Result<Option<String>, AppError> {
            Ok(None)
        }

        fn display_path(&self, _layout: RemoteLayout) -> String {
            String::new()
        }

        fn remote_endpoint(&self) -> &str {
            &self.endpoint
        }

        fn key_remote_empty(&self) -> &'static str {
            "stub.sync.remote_empty"
        }

        fn key_manifest_missing_artifact(&self) -> &'static str {
            "stub.sync.manifest_missing_artifact"
        }

        fn key_remote_missing_artifact(&self) -> &'static str {
            "stub.sync.remote_missing_artifact"
        }
    }

    /// `fetch_remote_info` sends transport credentials (WebDAV Basic / S3
    /// SigV4), so it must honour the same plaintext-HTTP policy as upload and
    /// download instead of bypassing it.
    #[tokio::test]
    async fn fetch_remote_info_enforces_plaintext_http_policy() {
        // Public plaintext HTTP without an exemption is blocked before the
        // manifest request (and thus any credentials) goes out.
        let blocked = StubTransport {
            endpoint: "http://nas.example.com:5005".to_string(),
        };
        assert!(
            fetch_remote_info_payload(&blocked, false).await.is_err(),
            "public plaintext HTTP must be blocked for fetch_remote_info"
        );
        // The same endpoint is allowed once the user grants the exemption; the
        // stub then reports there is no remote snapshot.
        assert!(
            fetch_remote_info_payload(&blocked, true)
                .await
                .expect("exempted plaintext HTTP is allowed")
                .is_none(),
            "an exempted endpoint with no manifest yields no info"
        );

        // Loopback / private-LAN hosts are always allowed without an exemption.
        for endpoint in ["http://192.168.1.5:5005", "http://127.0.0.1:5005"] {
            let trusted = StubTransport {
                endpoint: endpoint.to_string(),
            };
            assert!(
                fetch_remote_info_payload(&trusted, false).await.is_ok(),
                "{endpoint} should be allowed without an exemption"
            );
        }
    }

    /// Defense in depth: the shared upload flow rejects a settings object that
    /// enabled encryption but left the password empty, rather than silently
    /// uploading plaintext.
    #[tokio::test]
    async fn upload_rejects_enabled_encryption_with_empty_password() {
        let db = crate::database::Database::memory().expect("create memory db");
        let transport = StubTransport {
            endpoint: "https://dav.example.com".to_string(),
        };
        let mut settings = crate::settings::WebDavSyncSettings {
            encryption_enabled: true,
            ..crate::settings::WebDavSyncSettings::default()
        };

        let err = upload_snapshot(&transport, &db, &mut settings, |_settings, _hash, _etag| {
            Ok::<(), AppError>(())
        })
        .await
        .expect_err("enabled encryption with an empty password must be rejected");
        assert!(
            err.to_string().contains("口令") || err.to_string().contains("password"),
            "unexpected error: {err}"
        );
    }
}

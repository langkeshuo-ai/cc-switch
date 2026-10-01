//! Shared plumbing behind the WebDAV / S3 sync command shells.
//!
//! The two command files are thin mirrors over this module; only the
//! transport-specific settings fields and the Tauri command names differ.

use std::future::Future;

use crate::error::AppError;
use crate::services::sync_protocol::run_with_sync_lock;

/// 检测明文 HTTP 同步端点（非回环主机），返回面向用户的中英双语警示。
/// 共享给 WebDAV 与 S3 的保存设置命令：不强制 https（兼容局域网 NAS），
/// 但必须让用户知道凭据与同步数据将明文传输。
pub(crate) fn plaintext_http_warning(raw_url: &str) -> Option<String> {
    let url = url::Url::parse(raw_url).ok()?;
    if url.scheme() != "http" {
        return None;
    }
    let host = url.host_str()?;
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host.starts_with("127.")
        || host.eq("::1")
        || host.eq("[::1]");
    if loopback {
        return None;
    }
    Some(AppError::localized(
        "sync.plaintext_http.warning",
        "同步端点使用 HTTP 明文传输：账号密码与同步数据（含供应商 API Key）可能被网络中间人截获。局域网 NAS 等可信环境可忽略此警告。",
        "Sync endpoint uses plaintext HTTP: credentials and sync data (including provider API keys) may be intercepted on the network. Ignore this warning only for trusted LAN environments.",
    )
    .to_string())
}

pub(crate) fn map_sync_result<T, F>(result: Result<T, AppError>, on_error: F) -> Result<T, String>
where
    F: FnOnce(&AppError),
{
    match result {
        Ok(value) => Ok(value),
        Err(err) => {
            on_error(&err);
            Err(err.to_string())
        }
    }
}

/// Load the transport's settings and reject unconfigured / disabled configs.
pub(crate) fn require_enabled_settings<S>(
    get_settings: impl FnOnce() -> Option<S>,
    is_enabled: impl Fn(&S) -> bool,
    not_configured_error: impl FnOnce() -> String,
    disabled_error: impl FnOnce() -> String,
) -> Result<S, String> {
    let settings = get_settings().ok_or_else(not_configured_error)?;
    if !is_enabled(&settings) {
        return Err(disabled_error());
    }
    Ok(settings)
}

/// Preserve the stored secret when the incoming request leaves it empty.
pub(crate) fn resolve_secret_for_request<S>(
    mut incoming: S,
    existing: Option<S>,
    preserve_empty: bool,
    get_secret: impl Fn(&S) -> &str,
    set_secret: impl Fn(&mut S, String),
) -> S {
    if let Some(existing_settings) = existing.as_ref() {
        if preserve_empty && get_secret(&incoming).is_empty() {
            let secret = get_secret(existing_settings).to_string();
            set_secret(&mut incoming, secret);
        }
    }
    incoming
}

/// Run a download under the global sync lock with auto-sync suppression held
/// only while the snapshot itself is in flight; the post-download projection
/// runs after the guard is dropped.
pub(crate) async fn run_download_with_sync_lock<T, U, G, DownloadFut, Project, ProjectFut>(
    suppression: impl FnOnce() -> G,
    download: DownloadFut,
    project: Project,
) -> Result<U, AppError>
where
    G: Send,
    DownloadFut: Future<Output = Result<T, AppError>>,
    Project: FnOnce(T) -> ProjectFut,
    ProjectFut: Future<Output = Result<U, AppError>>,
{
    run_with_sync_lock(async {
        let result = {
            let _auto_sync_suppression = suppression();
            download.await?
        };
        project(result).await
    })
    .await
}

#[cfg(test)]
mod tests {
    #[test]
    fn sync_shared_plaintext_warning_accepts_public_http_endpoint() {
        assert!(super::plaintext_http_warning("http://nas.example.com:5005").is_some());
    }

    #[test]
    fn sync_shared_plaintext_warning_ignores_https_and_loopback() {
        assert!(super::plaintext_http_warning("https://dav.example.com").is_none());
        assert!(super::plaintext_http_warning("http://localhost:5005").is_none());
        assert!(super::plaintext_http_warning("not a url").is_none());
    }

    #[test]
    fn sync_shared_map_sync_result_invokes_error_handler() {
        let called = super::map_sync_result::<(), _>(
            Err(crate::error::AppError::Config("boom".to_string())),
            |err| assert!(err.to_string().contains("boom")),
        );
        assert!(called.is_err());
    }

    #[test]
    fn sync_shared_require_enabled_settings_rejects_missing() {
        let result: Result<u8, String> = super::require_enabled_settings(
            || None,
            |value| *value > 0,
            || "missing".to_string(),
            || "disabled".to_string(),
        );
        assert_eq!(result.expect_err("missing settings rejected"), "missing");
    }

    #[test]
    fn sync_shared_require_enabled_settings_rejects_disabled() {
        let result: Result<u8, String> = super::require_enabled_settings(
            || Some(0),
            |value| *value > 0,
            || "missing".to_string(),
            || "disabled".to_string(),
        );
        assert_eq!(result.expect_err("disabled rejected"), "disabled");
    }

    #[test]
    fn sync_shared_resolve_secret_preserves_and_overrides() {
        let resolved = super::resolve_secret_for_request(
            String::new(),
            Some("stored".to_string()),
            true,
            |value: &String| value.as_str(),
            |slot: &mut String, value: String| *slot = value,
        );
        assert_eq!(resolved, "stored");

        let explicit = super::resolve_secret_for_request(
            "explicit".to_string(),
            Some("stored".to_string()),
            false,
            |value: &String| value.as_str(),
            |slot: &mut String, value: String| *slot = value,
        );
        assert_eq!(explicit, "explicit");
    }
}

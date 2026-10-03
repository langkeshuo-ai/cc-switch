//! WebDAV binding for the shared auto-sync worker loop.
//!
//! Owns the WebDAV channel slot + suppression depth statics and binds the
//! WebDAV settings/upload service to [`crate::services::auto_sync`].

use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, OnceLock};

use crate::error::AppError;
use crate::services::auto_sync::{self, AutoSyncBackend};
use crate::services::sync_protocol::persist_sync_error;
use crate::services::webdav_sync as webdav_sync_service;
use crate::settings::{self, WebDavSyncSettings};

static DB_CHANGE_TX: OnceLock<tokio::sync::mpsc::Sender<String>> = OnceLock::new();
static AUTO_SYNC_SUPPRESS_DEPTH: AtomicUsize = AtomicUsize::new(0);

pub(crate) struct AutoSyncSuppressionGuard(#[allow(dead_code)] auto_sync::SuppressGuard);

impl AutoSyncSuppressionGuard {
    pub fn new() -> Self {
        Self(auto_sync::SuppressGuard::new(&AUTO_SYNC_SUPPRESS_DEPTH))
    }
}

/// Test-only: commands' download flow checks WebDAV suppression in tests.
#[cfg(test)]
pub(crate) fn is_auto_sync_suppressed() -> bool {
    auto_sync::is_auto_sync_suppressed(&AUTO_SYNC_SUPPRESS_DEPTH)
}

#[cfg(test)]
pub fn should_trigger_for_table(table: &str) -> bool {
    crate::services::sync_protocol::should_trigger_auto_sync_for_table(table)
}

fn should_run_auto_sync(settings: Option<&WebDavSyncSettings>) -> bool {
    let Some(sync) = settings else {
        return false;
    };
    sync.enabled && sync.auto_sync
}

fn persist_auto_sync_error(settings: &mut WebDavSyncSettings, error: &AppError) {
    persist_sync_error(settings, error, "auto", settings::update_webdav_sync_status);
}

pub fn notify_db_changed(table: &str) {
    auto_sync::notify_db_changed::<WebDavBackend>(&AUTO_SYNC_SUPPRESS_DEPTH, &DB_CHANGE_TX, table);
}

pub fn start_worker(db: Arc<crate::database::Database>, app: tauri::AppHandle) {
    auto_sync::start_worker::<WebDavBackend>(db, app, &DB_CHANGE_TX);
}

struct WebDavBackend;

impl AutoSyncBackend for WebDavBackend {
    type Settings = WebDavSyncSettings;

    fn get_settings() -> Option<WebDavSyncSettings> {
        settings::get_webdav_sync_settings()
    }

    fn should_run(settings: Option<&WebDavSyncSettings>) -> bool {
        should_run_auto_sync(settings)
    }

    fn persist_error(settings: &mut WebDavSyncSettings, error: &AppError) {
        persist_auto_sync_error(settings, error)
    }

    async fn upload(
        db: &crate::database::Database,
        settings: &mut WebDavSyncSettings,
    ) -> Result<serde_json::Value, AppError> {
        webdav_sync_service::upload(db, settings).await
    }

    fn event_name() -> &'static str {
        "webdav-sync-status-updated"
    }

    fn log_tag() -> &'static str {
        "WebDAV"
    }
}

#[cfg(test)]
pub(crate) use auto_sync::{auto_sync_wait_duration, enqueue_change_signal, MAX_AUTO_SYNC_WAIT_MS};

#[cfg(test)]
mod tests {
    use super::{
        auto_sync_wait_duration, enqueue_change_signal, is_auto_sync_suppressed,
        should_run_auto_sync, should_trigger_for_table, AutoSyncSuppressionGuard,
        MAX_AUTO_SYNC_WAIT_MS,
    };
    use crate::settings::WebDavSyncSettings;
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc::channel;

    #[test]
    fn should_trigger_sync_for_config_tables_only() {
        assert!(should_trigger_for_table("providers"));
        assert!(should_trigger_for_table("profiles"));
        assert!(should_trigger_for_table("settings"));
        assert!(!should_trigger_for_table("proxy_request_logs"));
        assert!(!should_trigger_for_table("provider_health"));
    }

    #[test]
    #[serial_test::serial(global_env)]
    fn suppression_guard_enables_and_restores_state() {
        assert!(!is_auto_sync_suppressed());
        {
            let _guard = AutoSyncSuppressionGuard::new();
            assert!(is_auto_sync_suppressed());
        }
        assert!(!is_auto_sync_suppressed());
    }

    #[test]
    #[serial_test::serial(global_env)]
    fn suppression_guard_supports_nesting() {
        assert!(!is_auto_sync_suppressed());
        let outer = AutoSyncSuppressionGuard::new();
        {
            let _inner = AutoSyncSuppressionGuard::new();
            assert!(is_auto_sync_suppressed());
            drop(outer);
            assert!(
                is_auto_sync_suppressed(),
                "an inner guard must keep suppression active"
            );
        }
        assert!(!is_auto_sync_suppressed());
    }

    #[test]
    fn auto_sync_wait_duration_debounces_then_clamps_to_remaining_max_wait() {
        let started = Instant::now();
        assert_eq!(
            auto_sync_wait_duration(started, started),
            Some(Duration::from_millis(1000)),
            "a fresh cycle waits the full debounce window"
        );

        let almost_max = started + Duration::from_millis(MAX_AUTO_SYNC_WAIT_MS - 100);
        assert_eq!(
            auto_sync_wait_duration(started, almost_max),
            Some(Duration::from_millis(100)),
            "the debounce wait is clamped to the remaining max wait budget"
        );
    }

    #[tokio::test]
    async fn enqueue_change_signal_fails_when_channel_closed() {
        let (tx, rx) = channel::<String>(1);
        drop(rx);
        assert!(!enqueue_change_signal(&tx, "providers"));
    }

    #[test]
    fn max_wait_caps_flush_latency_for_continuous_events() {
        let started = Instant::now();
        let later = started + Duration::from_millis(MAX_AUTO_SYNC_WAIT_MS + 1);
        assert!(auto_sync_wait_duration(started, later).is_none());
    }

    #[tokio::test]
    async fn enqueue_change_signal_drops_when_channel_is_full() {
        let (tx, _rx) = channel::<String>(1);
        assert!(enqueue_change_signal(&tx, "providers"));
        assert!(!enqueue_change_signal(&tx, "providers"));
    }

    #[test]
    fn should_run_auto_sync_requires_enabled_and_auto_sync_flag() {
        assert!(!should_run_auto_sync(None));

        let disabled = WebDavSyncSettings {
            enabled: false,
            auto_sync: true,
            ..WebDavSyncSettings::default()
        };
        assert!(!should_run_auto_sync(Some(&disabled)));

        let auto_sync_off = WebDavSyncSettings {
            enabled: true,
            auto_sync: false,
            ..WebDavSyncSettings::default()
        };
        assert!(!should_run_auto_sync(Some(&auto_sync_off)));

        let enabled = WebDavSyncSettings {
            enabled: true,
            auto_sync: true,
            ..WebDavSyncSettings::default()
        };
        assert!(should_run_auto_sync(Some(&enabled)));
    }

    #[test]
    fn service_layer_does_not_depend_on_commands_layer() {
        let source = include_str!("webdav_auto_sync.rs");
        let needle = ["crate", "commands", ""].join("::");
        assert!(
            !source.contains(&needle),
            "services layer should not depend on commands layer"
        );
    }
}

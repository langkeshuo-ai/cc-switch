//! Transport-agnostic auto-sync worker loop shared by WebDAV and S3.
//!
//! Each transport binds its own settings type, status event and upload
//! service through [`AutoSyncBackend`], and owns its own channel slot and
//! suppression depth static (see the `webdav_auto_sync` / `s3_auto_sync`
//! binding modules).

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde_json::json;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{channel, Receiver, Sender};

use crate::database::Database;
use crate::error::AppError;
use crate::services::sync_protocol::{run_with_sync_lock, should_trigger_auto_sync_for_table};

const AUTO_SYNC_DEBOUNCE_MS: u64 = 1000;
pub(crate) const MAX_AUTO_SYNC_WAIT_MS: u64 = 10_000;

// ─── Suppression ─────────────────────────────────────────────

/// RAII guard incrementing a transport-owned suppression depth counter.
pub(crate) struct SuppressGuard(&'static AtomicUsize);

impl SuppressGuard {
    pub fn new(depth: &'static AtomicUsize) -> Self {
        depth.fetch_add(1, Ordering::SeqCst);
        Self(depth)
    }
}

impl Drop for SuppressGuard {
    fn drop(&mut self) {
        let _ = self
            .0
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                Some(value.saturating_sub(1))
            });
    }
}

pub(crate) fn is_auto_sync_suppressed(depth: &'static AtomicUsize) -> bool {
    depth.load(Ordering::SeqCst) > 0
}

// ─── Change signaling ────────────────────────────────────────

pub(crate) fn should_trigger_for_table(table: &str) -> bool {
    should_trigger_auto_sync_for_table(table)
}

pub(crate) fn enqueue_change_signal(tx: &Sender<String>, table: &str) -> bool {
    match tx.try_send(table.to_string()) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) | Err(TrySendError::Closed(_)) => false,
    }
}

pub(crate) fn auto_sync_wait_duration(started_at: Instant, now: Instant) -> Option<Duration> {
    let max_wait = Duration::from_millis(MAX_AUTO_SYNC_WAIT_MS);
    let debounce = Duration::from_millis(AUTO_SYNC_DEBOUNCE_MS);
    let elapsed = now.saturating_duration_since(started_at);
    if elapsed >= max_wait {
        return None;
    }
    Some(debounce.min(max_wait - elapsed))
}

// ─── Backend binding ─────────────────────────────────────────

pub(crate) trait AutoSyncBackend: Send + Sync + 'static {
    type Settings: Send + 'static;

    fn get_settings() -> Option<Self::Settings>;
    fn should_run(settings: Option<&Self::Settings>) -> bool;
    fn persist_error(settings: &mut Self::Settings, error: &AppError);
    fn upload(
        db: &Database,
        settings: &mut Self::Settings,
    ) -> impl Future<Output = Result<serde_json::Value, AppError>> + Send;
    /// Tauri event emitted after each automatic sync attempt.
    fn event_name() -> &'static str;
    /// Log prefix (e.g. "WebDAV", "S3").
    fn log_tag() -> &'static str;
}

pub(crate) fn notify_db_changed<B: AutoSyncBackend>(
    depth: &'static AtomicUsize,
    tx_slot: &'static OnceLock<Sender<String>>,
    table: &str,
) {
    if is_auto_sync_suppressed(depth) {
        return;
    }
    if !should_trigger_for_table(table) {
        return;
    }
    let Some(tx) = tx_slot.get() else {
        return;
    };
    let _ = enqueue_change_signal(tx, table);
}

pub(crate) fn start_worker<B: AutoSyncBackend>(
    db: Arc<Database>,
    app: tauri::AppHandle,
    tx_slot: &'static OnceLock<Sender<String>>,
) {
    if tx_slot.get().is_some() {
        return;
    }

    // Buffer size 1 is enough: we only need "dirty" signals, not every event.
    let (tx, rx) = channel::<String>(1);
    if tx_slot.set(tx).is_err() {
        return;
    }

    tauri::async_runtime::spawn(async move {
        run_worker_loop::<B>(db, rx, app).await;
    });
}

// ─── Worker loop ─────────────────────────────────────────────

async fn run_worker_loop<B: AutoSyncBackend>(
    db: Arc<Database>,
    mut rx: Receiver<String>,
    app: AppHandle,
) {
    while let Some(first_table) = rx.recv().await {
        let started_at = Instant::now();
        let mut merged_count = 1usize;

        while let Some(wait_for) = auto_sync_wait_duration(started_at, Instant::now()) {
            let timeout = tokio::time::timeout(wait_for, rx.recv()).await;

            match timeout {
                Ok(Some(_)) => merged_count += 1,
                Ok(None) => return,
                Err(_) => break,
            }
        }

        log::debug!(
            "[{}][AutoSync] Triggered by table={first_table}, merged_changes={merged_count}",
            B::log_tag()
        );

        if let Err(err) = run_auto_sync_upload::<B>(&db, &app).await {
            log::warn!("[{}][AutoSync] Upload failed: {err}", B::log_tag());
        }
    }
}

async fn run_auto_sync_upload<B: AutoSyncBackend>(
    db: &Database,
    app: &AppHandle,
) -> Result<(), AppError> {
    let mut settings = B::get_settings();
    if !B::should_run(settings.as_ref()) {
        return Ok(());
    }

    let mut sync_settings = match settings.take() {
        Some(value) => value,
        None => return Ok(()),
    };

    let result = run_with_sync_lock(B::upload(db, &mut sync_settings)).await;
    match result {
        Ok(_) => {
            emit_auto_sync_status_updated::<B>(app, "success", None);
            Ok(())
        }
        Err(err) => {
            B::persist_error(&mut sync_settings, &err);
            emit_auto_sync_status_updated::<B>(app, "error", Some(&err.to_string()));
            Err(err)
        }
    }
}

fn emit_auto_sync_status_updated<B: AutoSyncBackend>(
    app: &AppHandle,
    status: &str,
    error: Option<&str>,
) {
    let payload = match error {
        Some(message) => json!({
            "source": "auto",
            "status": status,
            "error": message,
        }),
        None => json!({
            "source": "auto",
            "status": status,
        }),
    };

    if let Err(err) = app.emit(B::event_name(), payload) {
        log::debug!(
            "[{}] failed to emit sync status update event: {err}",
            B::log_tag()
        );
    }
}

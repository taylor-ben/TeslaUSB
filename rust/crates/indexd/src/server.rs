//! Unix-socket RPC server for `retentiond → indexd` archive registration.

use std::collections::HashSet;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use rusqlite::{Connection, params};

use crate::db::cloud::{
    CloudConfig, CloudQueuePk, CloudQueueRetryResolution, CloudQueueUpsertItem, cloud_candidates,
    cloud_config_get, cloud_config_put, cloud_history_load, cloud_queue_load, cloud_queue_retry,
    cloud_queue_upsert, cloud_stats_get, cloud_stats_reset, cloud_upload_commit, cloud_upload_fail,
    upload_lease_acquire, upload_lease_release, upload_lease_renew,
};
use crate::db::ingest::{
    ArchiveAngleRegistration, ArchiveRegistration, ArchiveUnitRegistration, register_archived_clip,
    register_quarantined_clip,
};
use crate::db::mutations::{
    BootContext, has_unexpired_lease, mark_deleted, mark_deleting, quarantine,
    release_delete_claim, set_pref,
};
use crate::db::reads::{list_eviction_candidates, list_recovery_rows};
use crate::db::{DbError, now_epoch_s};
use crate::model::FolderClass;
use crate::proto::{
    CloudCandidateWire, CloudConfigWire, CloudHistoryRowWire, CloudQueueRetryResolutionWire,
    CloudQueueRowWire, CloudQueueUpsertWire, EvictionCandidateWire, RecoveryRowWire,
    RegisterArchivedClip, Request, Response, read_request, write_response,
};

/// Start the indexd registration server thread.
///
/// # Errors
///
/// Returns an error if socket setup/bind fails.
pub fn spawn(
    conn: &Arc<Mutex<Connection>>,
    boot: &Arc<BootContext>,
    socket_path: &Path,
    io_timeout: Duration,
) -> io::Result<thread::JoinHandle<()>> {
    let listener = bind_listener(socket_path)?;
    let conn = Arc::clone(conn);
    let boot = Arc::clone(boot);
    thread::Builder::new()
        .name("indexd-rpc".to_owned())
        .spawn(move || serve(&listener, &conn, &boot, io_timeout))
}

fn bind_listener(socket_path: &Path) -> io::Result<UnixListener> {
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o750))?;
    }
    match std::fs::remove_file(socket_path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(socket_path)?;
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o660))?;
    Ok(listener)
}

fn serve(
    listener: &UnixListener,
    conn: &Arc<Mutex<Connection>>,
    boot: &Arc<BootContext>,
    io_timeout: Duration,
) {
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let _ = handle_connection(stream, conn, boot, io_timeout);
            }
            Err(_) => continue,
        }
    }
}

#[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
fn handle_connection(
    mut stream: UnixStream,
    conn: &Arc<Mutex<Connection>>,
    boot: &Arc<BootContext>,
    io_timeout: Duration,
) -> io::Result<()> {
    stream.set_read_timeout(Some(io_timeout))?;
    stream.set_write_timeout(Some(io_timeout))?;

    let response = match read_request(&mut stream) {
        Ok(request) => match request {
            Request::RegisterArchivedClip(payload) => {
                match handle_register_archived_clip(conn, &payload) {
                    Ok((clip_id, archive_item_id)) => Response::Ok {
                        clip_id,
                        archive_item_id,
                    },
                    Err(HandlerError::Rejected(message)) => Response::Rejected { message },
                    Err(HandlerError::Internal(message)) => Response::Error { message },
                }
            }
            Request::RegisterQuarantinedArchive(payload) => {
                match handle_register_quarantined_clip(conn, &payload) {
                    Ok((clip_id, archive_item_id)) => Response::Ok {
                        clip_id,
                        archive_item_id,
                    },
                    Err(HandlerError::Rejected(message)) => Response::Rejected { message },
                    Err(HandlerError::Internal(message)) => Response::Error { message },
                }
            }
            Request::SetPref { key, value } => match handle_set_pref(conn, &key, &value) {
                Ok(()) => Response::PrefSet { key },
                Err(message) => Response::Error { message },
            },
            Request::ClaimEvictionCandidate {
                id,
                recency_floor_epoch,
                allow_undurable,
            } => match handle_claim_eviction_candidate(
                conn,
                boot,
                id,
                recency_floor_epoch,
                allow_undurable,
            ) {
                Ok(response) => response,
                Err(message) => Response::Error { message },
            },
            Request::MarkArchiveDeleting { id } => match handle_mark_archive_deleting(conn, id) {
                Ok(()) => Response::Acked {},
                Err(message) => Response::Error { message },
            },
            Request::MarkArchiveDeleted { id, bytes_freed } => {
                match handle_mark_archive_deleted(conn, id, bytes_freed) {
                    Ok(response) => response,
                    Err(message) => Response::Error { message },
                }
            }
            Request::ReleaseArchiveDeleteClaim { id } => {
                match handle_release_archive_delete_claim(conn, id) {
                    Ok(()) => Response::Acked {},
                    Err(message) => Response::Error { message },
                }
            }
            Request::QuarantineArchiveItem { id, reason } => {
                match handle_quarantine_archive_item(conn, id, &reason) {
                    Ok(()) => Response::Acked {},
                    Err(message) => Response::Error { message },
                }
            }
            Request::ListEvictionCandidates {
                recency_floor_epoch,
                allow_undurable,
                limit,
            } => match handle_list_eviction_candidates(
                conn,
                recency_floor_epoch,
                allow_undurable,
                limit,
            ) {
                Ok(items) => Response::EvictionCandidates { items },
                Err(message) => Response::Error { message },
            },
            Request::ListRecoveryRows {} => match handle_list_recovery_rows(conn) {
                Ok(rows) => Response::RecoveryRows { rows },
                Err(message) => Response::Error { message },
            },
            Request::CloudCandidates {
                folders,
                after_cursor,
                limit,
            } => match handle_cloud_candidates(conn, &folders, after_cursor.as_deref(), limit) {
                Ok((items, next_cursor)) => Response::CloudCandidates { items, next_cursor },
                Err(HandlerError::Rejected(message)) => Response::Rejected { message },
                Err(HandlerError::Internal(message)) => Response::Error { message },
            },
            Request::CloudQueueLoad {
                after_cursor,
                limit,
            } => match handle_cloud_queue_load(conn, after_cursor.as_deref(), limit) {
                Ok((items, next_cursor)) => Response::CloudQueuePage { items, next_cursor },
                Err(HandlerError::Rejected(message)) => Response::Rejected { message },
                Err(HandlerError::Internal(message)) => Response::Error { message },
            },
            Request::CloudQueueUpsert { item } => match handle_cloud_queue_upsert(conn, &item) {
                Ok(state) => Response::CloudQueueState { state },
                Err(HandlerError::Rejected(message)) => Response::Rejected { message },
                Err(HandlerError::Internal(message)) => Response::Error { message },
            },
            Request::CloudQueueRetry {
                archive_item_id,
                child_key,
                resolution,
            } => match handle_cloud_queue_retry(
                conn,
                archive_item_id,
                child_key.as_deref(),
                &resolution,
            ) {
                Ok(state) => Response::CloudQueueState { state },
                Err(HandlerError::Rejected(message)) => Response::Rejected { message },
                Err(HandlerError::Internal(message)) => Response::Error { message },
            },
            Request::UploadLeaseAcquire {
                archive_item_id,
                ttl_ms,
            } => match handle_upload_lease_acquire(conn, boot, archive_item_id, ttl_ms) {
                Ok((granted, token, boot_id, expires_mono_ms)) => Response::UploadLeaseAcquired {
                    granted,
                    token,
                    boot_id,
                    expires_mono_ms,
                },
                Err(HandlerError::Rejected(message)) => Response::Rejected { message },
                Err(HandlerError::Internal(message)) => Response::Error { message },
            },
            Request::UploadLeaseRenew { token, ttl_ms } => {
                match handle_upload_lease_renew(conn, boot, &token, ttl_ms) {
                    Ok((ok, expires_mono_ms)) => Response::UploadLeaseRenewed {
                        ok,
                        expires_mono_ms,
                    },
                    Err(HandlerError::Rejected(message)) => Response::Rejected { message },
                    Err(HandlerError::Internal(message)) => Response::Error { message },
                }
            }
            Request::UploadLeaseRelease { token } => {
                match handle_upload_lease_release(conn, boot, &token) {
                    Ok(ok) => Response::UploadLeaseReleased { ok },
                    Err(HandlerError::Rejected(message)) => Response::Rejected { message },
                    Err(HandlerError::Internal(message)) => Response::Error { message },
                }
            }
            Request::CloudUploadCommit {
                queue_pk,
                attempt_id,
                hash,
                hash_alg,
                size,
            } => match handle_cloud_upload_commit(
                conn,
                &queue_pk.destination_id,
                &queue_pk.remote_key,
                &attempt_id,
                &hash,
                &hash_alg,
                size,
            ) {
                Ok((ok, durable_parent)) => Response::CloudUploadCommitted { ok, durable_parent },
                Err(HandlerError::Rejected(message)) => Response::Rejected { message },
                Err(HandlerError::Internal(message)) => Response::Error { message },
            },
            Request::CloudUploadFail {
                queue_pk,
                attempt_id,
                error_class,
                not_before,
                terminal,
            } => match handle_cloud_upload_fail(
                conn,
                &queue_pk.destination_id,
                &queue_pk.remote_key,
                &attempt_id,
                &error_class,
                not_before,
                terminal,
            ) {
                Ok((ok, state)) => Response::CloudUploadFailed { ok, state },
                Err(HandlerError::Rejected(message)) => Response::Rejected { message },
                Err(HandlerError::Internal(message)) => Response::Error { message },
            },
            Request::CloudStatsGet {} => match handle_cloud_stats_get(conn) {
                Ok((synced_count, synced_bytes, since_at)) => Response::CloudStats {
                    synced_count,
                    synced_bytes,
                    since_at,
                },
                Err(HandlerError::Rejected(message)) => Response::Rejected { message },
                Err(HandlerError::Internal(message)) => Response::Error { message },
            },
            Request::CloudStatsReset {} => match handle_cloud_stats_reset(conn) {
                Ok(baseline_seq) => Response::CloudStatsReset {
                    ok: true,
                    baseline_seq,
                },
                Err(HandlerError::Rejected(message)) => Response::Rejected { message },
                Err(HandlerError::Internal(message)) => Response::Error { message },
            },
            Request::CloudConfigGet {} => match handle_cloud_config_get(conn) {
                Ok(config) => Response::CloudConfig { config },
                Err(HandlerError::Rejected(message)) => Response::Rejected { message },
                Err(HandlerError::Internal(message)) => Response::Error { message },
            },
            Request::CloudConfigPut { config } => match handle_cloud_config_put(conn, &config) {
                Ok(config) => Response::CloudConfig { config },
                Err(HandlerError::Rejected(message)) => Response::Rejected { message },
                Err(HandlerError::Internal(message)) => Response::Error { message },
            },
            Request::CloudHistoryLoad {
                after_cursor,
                limit,
            } => match handle_cloud_history_load(conn, after_cursor.as_deref(), limit) {
                Ok((items, next_cursor)) => Response::CloudHistoryPage { items, next_cursor },
                Err(HandlerError::Rejected(message)) => Response::Rejected { message },
                Err(HandlerError::Internal(message)) => Response::Error { message },
            },
        },
        Err(e) => Response::Error {
            message: format!("invalid request: {e}"),
        },
    };
    let _ = write_response(&mut stream, &response);
    Ok(())
}

/// Classifies a register-handler failure for wire-response mapping.
enum HandlerError {
    /// Deterministic: payload invalid, retry is futile -> `Response::Rejected`.
    Rejected(String),
    /// Operational/transient: DB or lock failure -> `Response::Error`.
    Internal(String),
}

fn handle_register_archived_clip(
    conn: &Arc<Mutex<Connection>>,
    payload: &RegisterArchivedClip,
) -> Result<(i64, i64), HandlerError> {
    let registration = build_registration(payload).map_err(HandlerError::Rejected)?;
    let mut locked = conn
        .lock()
        .map_err(|_| HandlerError::Internal("index database mutex is poisoned".to_owned()))?;
    register_archived_clip(&mut locked, &registration)
        .map_err(|e| HandlerError::Internal(e.to_string()))
}

fn handle_register_quarantined_clip(
    conn: &Arc<Mutex<Connection>>,
    payload: &RegisterArchivedClip,
) -> Result<(i64, i64), HandlerError> {
    let registration = build_registration(payload).map_err(HandlerError::Rejected)?;
    let mut locked = conn
        .lock()
        .map_err(|_| HandlerError::Internal("index database mutex is poisoned".to_owned()))?;
    register_quarantined_clip(&mut locked, &registration)
        .map_err(|e| HandlerError::Internal(e.to_string()))
}

fn handle_set_pref(conn: &Arc<Mutex<Connection>>, key: &str, value: &str) -> Result<(), String> {
    let locked = conn
        .lock()
        .map_err(|_| "index database mutex is poisoned".to_owned())?;
    set_pref(&locked, key, value).map_err(|e| e.to_string())
}

fn handle_claim_eviction_candidate(
    conn: &Arc<Mutex<Connection>>,
    boot: &Arc<BootContext>,
    id: i64,
    recency_floor_epoch: i64,
    allow_undurable: bool,
) -> Result<Response, String> {
    let mut locked = conn
        .lock()
        .map_err(|_| "index database mutex is poisoned".to_owned())?;
    let exists: i64 = locked
        .query_row(
            "SELECT COUNT(*) FROM archive_items WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    if exists == 0 {
        return Ok(Response::NotFound {});
    }

    let claimed = boot
        .claim_eviction_candidate(&mut locked, id, recency_floor_epoch, allow_undurable)
        .map_err(|e| e.to_string())?;
    if claimed.is_some() {
        return Ok(Response::Claimed {});
    }

    let leased = has_unexpired_lease(&locked, boot.boot_id(), boot.mono_now_ms(), id)
        .map_err(|e| e.to_string())?;
    if leased {
        Ok(Response::ClaimDenied {
            reason: "unexpired lease".to_owned(),
        })
    } else {
        Ok(Response::ClaimDenied {
            reason: "ineligible".to_owned(),
        })
    }
}

fn handle_mark_archive_deleting(conn: &Arc<Mutex<Connection>>, id: i64) -> Result<(), String> {
    let locked = conn
        .lock()
        .map_err(|_| "index database mutex is poisoned".to_owned())?;
    mark_deleting(&locked, id).map_err(|e| e.to_string())
}

fn handle_mark_archive_deleted(
    conn: &Arc<Mutex<Connection>>,
    id: i64,
    bytes_freed: i64,
) -> Result<Response, String> {
    if bytes_freed < 0 {
        return Ok(Response::Rejected {
            message: "bytes_freed must be >= 0".to_owned(),
        });
    }
    let locked = conn
        .lock()
        .map_err(|_| "index database mutex is poisoned".to_owned())?;
    mark_deleted(&locked, id, bytes_freed).map_err(|e| e.to_string())?;
    Ok(Response::Acked {})
}

fn handle_release_archive_delete_claim(
    conn: &Arc<Mutex<Connection>>,
    id: i64,
) -> Result<(), String> {
    let locked = conn
        .lock()
        .map_err(|_| "index database mutex is poisoned".to_owned())?;
    release_delete_claim(&locked, id).map_err(|e| e.to_string())
}

fn handle_quarantine_archive_item(
    conn: &Arc<Mutex<Connection>>,
    id: i64,
    reason: &str,
) -> Result<(), String> {
    let locked = conn
        .lock()
        .map_err(|_| "index database mutex is poisoned".to_owned())?;
    quarantine(&locked, id, reason).map_err(|e| e.to_string())
}

fn handle_list_eviction_candidates(
    conn: &Arc<Mutex<Connection>>,
    recency_floor_epoch: i64,
    allow_undurable: bool,
    limit: u32,
) -> Result<Vec<EvictionCandidateWire>, String> {
    let now_epoch = now_epoch_s();
    let locked = conn
        .lock()
        .map_err(|_| "index database mutex is poisoned".to_owned())?;
    let rows = list_eviction_candidates(
        &locked,
        recency_floor_epoch,
        now_epoch,
        allow_undurable,
        limit,
    )
    .map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .map(|row| EvictionCandidateWire {
            id: row.id,
            path: row.path,
            size_bytes: row.size_bytes,
            archived_at: row.archived_at,
            folder_class: row.folder_class,
        })
        .collect())
}

fn handle_list_recovery_rows(
    conn: &Arc<Mutex<Connection>>,
) -> Result<Vec<RecoveryRowWire>, String> {
    let locked = conn
        .lock()
        .map_err(|_| "index database mutex is poisoned".to_owned())?;
    let rows = list_recovery_rows(&locked).map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .map(|row| RecoveryRowWire {
            id: row.id,
            delete_state: row.delete_state,
            path: row.path,
            size_bytes: row.size_bytes,
            delete_gen: row.delete_gen,
        })
        .collect())
}

fn map_db_error(error: DbError) -> HandlerError {
    match error {
        DbError::Sqlite(rusqlite::Error::InvalidParameterName(message)) => {
            HandlerError::Rejected(message)
        }
        other => HandlerError::Internal(other.to_string()),
    }
}

fn to_config_wire(config: &CloudConfig) -> CloudConfigWire {
    CloudConfigWire {
        sentry_enabled: config.sentry_enabled,
        saved_enabled: config.saved_enabled,
        recent_enabled: config.recent_enabled,
        sentry_priority: config.sentry_priority,
        saved_priority: config.saved_priority,
        recent_priority: config.recent_priority,
        reserve_gb: config.reserve_gb,
        max_attempts: config.max_attempts,
        base_backoff_secs: config.base_backoff_secs,
        keep_until_backed_up: config.keep_until_backed_up,
        auto_sync: config.auto_sync,
    }
}

fn from_config_wire(config: &CloudConfigWire) -> CloudConfig {
    CloudConfig {
        sentry_enabled: config.sentry_enabled,
        saved_enabled: config.saved_enabled,
        recent_enabled: config.recent_enabled,
        sentry_priority: config.sentry_priority,
        saved_priority: config.saved_priority,
        recent_priority: config.recent_priority,
        reserve_gb: config.reserve_gb,
        max_attempts: config.max_attempts,
        base_backoff_secs: config.base_backoff_secs,
        keep_until_backed_up: config.keep_until_backed_up,
        auto_sync: config.auto_sync,
    }
}

fn from_retry_resolution_wire(
    resolution: &CloudQueueRetryResolutionWire,
) -> CloudQueueRetryResolution {
    match resolution {
        CloudQueueRetryResolutionWire::KeepExisting => CloudQueueRetryResolution::KeepExisting,
        CloudQueueRetryResolutionWire::Rekey { remote_key } => CloudQueueRetryResolution::Rekey {
            remote_key: remote_key.clone(),
        },
        CloudQueueRetryResolutionWire::Replace => CloudQueueRetryResolution::Replace,
    }
}

fn handle_cloud_candidates(
    conn: &Arc<Mutex<Connection>>,
    folders: &[String],
    after_cursor: Option<&str>,
    limit: u32,
) -> Result<(Vec<CloudCandidateWire>, Option<String>), HandlerError> {
    let locked = conn
        .lock()
        .map_err(|_| HandlerError::Internal("index database mutex is poisoned".to_owned()))?;
    let page = cloud_candidates(&locked, folders, after_cursor, limit).map_err(map_db_error)?;
    Ok((
        page.items
            .into_iter()
            .map(|row| CloudCandidateWire {
                archive_item_id: row.archive_item_id,
                child_key: row.child_key,
                source_rel: row.source_rel,
                destination_id: row.destination_id,
                remote_key: row.remote_key,
                size_bytes: row.size_bytes,
                content_sha256: row.content_sha256,
                state: row.state,
                category: row.category,
                seq: row.seq,
            })
            .collect(),
        page.next_cursor,
    ))
}

fn handle_cloud_queue_load(
    conn: &Arc<Mutex<Connection>>,
    after_cursor: Option<&str>,
    limit: u32,
) -> Result<(Vec<CloudQueueRowWire>, Option<String>), HandlerError> {
    let locked = conn
        .lock()
        .map_err(|_| HandlerError::Internal("index database mutex is poisoned".to_owned()))?;
    let page = cloud_queue_load(&locked, after_cursor, limit).map_err(map_db_error)?;
    Ok((
        page.items
            .into_iter()
            .map(|row| CloudQueueRowWire {
                archive_item_id: row.archive_item_id,
                child_key: row.child_key,
                destination_id: row.destination_id,
                remote_key: row.remote_key,
                category: row.category,
                seq: row.seq,
                total_bytes: row.total_bytes,
                bytes_uploaded: row.bytes_uploaded,
                content_sha256: row.content_sha256,
                state: row.state,
                attempts: row.attempts,
                not_before: row.not_before,
                last_error: row.last_error,
            })
            .collect(),
        page.next_cursor,
    ))
}

fn handle_cloud_queue_upsert(
    conn: &Arc<Mutex<Connection>>,
    item: &CloudQueueUpsertWire,
) -> Result<String, HandlerError> {
    let locked = conn
        .lock()
        .map_err(|_| HandlerError::Internal("index database mutex is poisoned".to_owned()))?;
    cloud_queue_upsert(
        &locked,
        &CloudQueueUpsertItem {
            archive_item_id: item.archive_item_id,
            child_key: item.child_key.clone(),
            destination_id: item.destination_id.clone(),
            remote_key: item.remote_key.clone(),
            category: item.category.clone(),
            seq: item.seq,
            total_bytes: item.total_bytes,
            content_sha256: item.content_sha256.clone(),
            expected_hash: item.expected_hash.clone(),
            verify_alg: item.verify_alg.clone(),
        },
    )
    .map_err(map_db_error)
}

fn handle_cloud_queue_retry(
    conn: &Arc<Mutex<Connection>>,
    archive_item_id: i64,
    child_key: Option<&str>,
    resolution: &CloudQueueRetryResolutionWire,
) -> Result<String, HandlerError> {
    let locked = conn
        .lock()
        .map_err(|_| HandlerError::Internal("index database mutex is poisoned".to_owned()))?;
    cloud_queue_retry(
        &locked,
        archive_item_id,
        child_key,
        &from_retry_resolution_wire(resolution),
    )
    .map_err(map_db_error)
}

fn handle_upload_lease_acquire(
    conn: &Arc<Mutex<Connection>>,
    boot: &Arc<BootContext>,
    archive_item_id: i64,
    ttl_ms: u32,
) -> Result<UploadLeaseAcquireWire, HandlerError> {
    let locked = conn
        .lock()
        .map_err(|_| HandlerError::Internal("index database mutex is poisoned".to_owned()))?;
    let result =
        upload_lease_acquire(&locked, boot, archive_item_id, ttl_ms).map_err(map_db_error)?;
    Ok((
        result.granted,
        result.token,
        result.boot_id,
        result.expires_mono_ms,
    ))
}

type UploadLeaseAcquireWire = (bool, Option<String>, Option<String>, Option<i64>);

fn handle_upload_lease_renew(
    conn: &Arc<Mutex<Connection>>,
    boot: &Arc<BootContext>,
    token: &str,
    ttl_ms: u32,
) -> Result<(bool, Option<i64>), HandlerError> {
    let locked = conn
        .lock()
        .map_err(|_| HandlerError::Internal("index database mutex is poisoned".to_owned()))?;
    let result = upload_lease_renew(&locked, boot, token, ttl_ms).map_err(map_db_error)?;
    Ok((result.ok, result.expires_mono_ms))
}

fn handle_upload_lease_release(
    conn: &Arc<Mutex<Connection>>,
    boot: &Arc<BootContext>,
    token: &str,
) -> Result<bool, HandlerError> {
    let locked = conn
        .lock()
        .map_err(|_| HandlerError::Internal("index database mutex is poisoned".to_owned()))?;
    upload_lease_release(&locked, boot, token).map_err(map_db_error)
}

#[allow(clippy::too_many_arguments)]
fn handle_cloud_upload_commit(
    conn: &Arc<Mutex<Connection>>,
    destination_id: &str,
    remote_key: &str,
    attempt_id: &str,
    hash: &str,
    hash_alg: &str,
    size: i64,
) -> Result<(bool, bool), HandlerError> {
    let mut locked = conn
        .lock()
        .map_err(|_| HandlerError::Internal("index database mutex is poisoned".to_owned()))?;
    let result = cloud_upload_commit(
        &mut locked,
        &CloudQueuePk {
            destination_id: destination_id.to_owned(),
            remote_key: remote_key.to_owned(),
        },
        attempt_id,
        hash,
        hash_alg,
        size,
    )
    .map_err(map_db_error)?;
    Ok((result.ok, result.durable_parent))
}

#[allow(clippy::too_many_arguments)]
fn handle_cloud_upload_fail(
    conn: &Arc<Mutex<Connection>>,
    destination_id: &str,
    remote_key: &str,
    attempt_id: &str,
    error_class: &str,
    not_before: Option<i64>,
    terminal: bool,
) -> Result<(bool, String), HandlerError> {
    let mut locked = conn
        .lock()
        .map_err(|_| HandlerError::Internal("index database mutex is poisoned".to_owned()))?;
    let result = cloud_upload_fail(
        &mut locked,
        &CloudQueuePk {
            destination_id: destination_id.to_owned(),
            remote_key: remote_key.to_owned(),
        },
        attempt_id,
        error_class,
        not_before,
        terminal,
    )
    .map_err(map_db_error)?;
    Ok((result.ok, result.state))
}

fn handle_cloud_stats_get(conn: &Arc<Mutex<Connection>>) -> Result<(i64, i64, i64), HandlerError> {
    let locked = conn
        .lock()
        .map_err(|_| HandlerError::Internal("index database mutex is poisoned".to_owned()))?;
    let stats = cloud_stats_get(&locked).map_err(map_db_error)?;
    Ok((stats.synced_count, stats.synced_bytes, stats.since_at))
}

fn handle_cloud_stats_reset(conn: &Arc<Mutex<Connection>>) -> Result<i64, HandlerError> {
    let locked = conn
        .lock()
        .map_err(|_| HandlerError::Internal("index database mutex is poisoned".to_owned()))?;
    cloud_stats_reset(&locked).map_err(map_db_error)
}

fn handle_cloud_config_get(conn: &Arc<Mutex<Connection>>) -> Result<CloudConfigWire, HandlerError> {
    let locked = conn
        .lock()
        .map_err(|_| HandlerError::Internal("index database mutex is poisoned".to_owned()))?;
    let config = cloud_config_get(&locked).map_err(map_db_error)?;
    Ok(to_config_wire(&config))
}

fn handle_cloud_config_put(
    conn: &Arc<Mutex<Connection>>,
    config: &CloudConfigWire,
) -> Result<CloudConfigWire, HandlerError> {
    let locked = conn
        .lock()
        .map_err(|_| HandlerError::Internal("index database mutex is poisoned".to_owned()))?;
    let persisted = cloud_config_put(&locked, &from_config_wire(config)).map_err(map_db_error)?;
    Ok(to_config_wire(&persisted))
}

fn handle_cloud_history_load(
    conn: &Arc<Mutex<Connection>>,
    after_cursor: Option<&str>,
    limit: u32,
) -> Result<(Vec<CloudHistoryRowWire>, Option<String>), HandlerError> {
    let locked = conn
        .lock()
        .map_err(|_| HandlerError::Internal("index database mutex is poisoned".to_owned()))?;
    let page = cloud_history_load(&locked, after_cursor, limit).map_err(map_db_error)?;
    Ok((
        page.items
            .into_iter()
            .map(|row| CloudHistoryRowWire {
                id: row.id,
                completion_seq: row.completion_seq,
                archive_item_id: row.archive_item_id,
                child_key: row.child_key,
                destination_id: row.destination_id,
                outcome: row.outcome,
                size_bytes: row.size_bytes,
                at: row.at,
                error_class: row.error_class,
            })
            .collect(),
        page.next_cursor,
    ))
}

fn build_registration(payload: &RegisterArchivedClip) -> Result<ArchiveRegistration, String> {
    validate_payload(payload)?;
    let folder_class = parse_folder_class(&payload.folder_class)?;
    Ok(ArchiveRegistration {
        canonical_key: payload.canonical_key.clone(),
        folder_class,
        partition: payload.partition.clone(),
        started_at: payload.started_at,
        ended_at: payload.ended_at,
        duration_s: seconds_opt_to_f64(payload.duration_s, "duration_s")?,
        archive: ArchiveUnitRegistration {
            path: payload.archive.path.clone(),
            size_bytes: payload.archive.size_bytes,
            file_count: payload.archive.file_count,
            archived_at: payload.archive.archived_at,
        },
        angles: payload
            .angles
            .iter()
            .map(|a| {
                Ok(ArchiveAngleRegistration {
                    camera: a.camera.clone(),
                    file_ref: a.file_ref.clone(),
                    offset_ms: a.offset_ms,
                    duration_s: seconds_opt_to_f64(a.duration_s, "angles.duration_s")?,
                    size_bytes: a.size_bytes,
                })
            })
            .collect::<Result<Vec<_>, String>>()?,
    })
}

fn parse_folder_class(raw: &str) -> Result<FolderClass, String> {
    match raw {
        "RecentClips" => Ok(FolderClass::RecentClips),
        "SavedClips" => Ok(FolderClass::SavedClips),
        "SentryClips" => Ok(FolderClass::SentryClips),
        "TeslaTrackMode" => Ok(FolderClass::TeslaTrackMode),
        other => Err(format!("invalid folder_class: {other}")),
    }
}

fn validate_payload(payload: &RegisterArchivedClip) -> Result<(), String> {
    if payload.canonical_key.is_empty() {
        return Err("canonical_key must be non-empty".to_owned());
    }
    if payload.partition.is_empty() {
        return Err("partition must be non-empty".to_owned());
    }
    if let Some(duration_s) = payload.duration_s {
        if duration_s < 0 {
            return Err("duration_s must be >= 0".to_owned());
        }
    }
    validate_rel_path(&payload.archive.path, "archive.path")?;
    if payload.archive.size_bytes < 0 {
        return Err("archive.size_bytes must be >= 0".to_owned());
    }
    if payload.archive.file_count < 1 {
        return Err("archive.file_count must be >= 1".to_owned());
    }
    if payload.angles.is_empty() {
        return Err("register_archived_clip requires at least one angle".to_owned());
    }

    let mut seen_cameras: HashSet<&str> = HashSet::new();
    for angle in &payload.angles {
        if !is_allowed_camera(&angle.camera) {
            return Err(format!("invalid camera: {}", angle.camera));
        }
        if !seen_cameras.insert(angle.camera.as_str()) {
            return Err(format!("duplicate camera: {}", angle.camera));
        }
        validate_rel_path(&angle.file_ref, "angles.file_ref")?;
        if angle.offset_ms < 0 {
            return Err("angles.offset_ms must be >= 0".to_owned());
        }
        if angle.size_bytes < 0 {
            return Err("angles.size_bytes must be >= 0".to_owned());
        }
        if let Some(duration_s) = angle.duration_s {
            if duration_s < 0 {
                return Err("angles.duration_s must be >= 0".to_owned());
            }
        }
    }
    Ok(())
}

fn is_allowed_camera(camera: &str) -> bool {
    matches!(
        camera,
        "front" | "back" | "left_repeater" | "right_repeater" | "left_pillar" | "right_pillar"
    )
}

fn validate_rel_path(path: &str, field: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err(format!("{field} must be non-empty"));
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return Err(format!("{field} must be archive-root-relative"));
    }
    if path
        .split(['/', '\\'])
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(format!("{field} must not contain empty/dot path segments"));
    }
    Ok(())
}

fn seconds_opt_to_f64(value: Option<i64>, field: &str) -> Result<Option<f64>, String> {
    value
        .map(|seconds| seconds_to_f64(seconds, field))
        .transpose()
}

fn seconds_to_f64(value: i64, field: &str) -> Result<f64, String> {
    const MAX_EXACT_INT_IN_F64: u64 = 9_007_199_254_740_992;
    if value.unsigned_abs() > MAX_EXACT_INT_IN_F64 {
        return Err(format!("{field} exceeds exact f64 integer range"));
    }
    value
        .to_string()
        .parse::<f64>()
        .map_err(|e| format!("failed to convert {field}: {e}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use rusqlite::{Connection, params};

    use super::{parse_folder_class, spawn, validate_payload};
    use crate::db::mutations::BootContext;
    use crate::db::open_in_memory;
    use crate::proto::{
        ArchiveAngle, ArchiveUnit, CloudConfigWire, CloudQueuePkWire,
        CloudQueueRetryResolutionWire, CloudQueueUpsertWire, MAX_REQUEST_FRAME,
        RegisterArchivedClip, Request, Response, read_frame, write_frame,
    };

    fn payload() -> RegisterArchivedClip {
        RegisterArchivedClip {
            canonical_key: "slot0:TeslaCam/RecentClips/2026-06-19/clip-a".to_owned(),
            folder_class: "RecentClips".to_owned(),
            partition: "slot0".to_owned(),
            started_at: 1_718_805_600,
            ended_at: 1_718_805_660,
            duration_s: Some(60),
            archive: ArchiveUnit {
                path: "archive/2026-06-19/clip-a".to_owned(),
                size_bytes: 4096,
                file_count: 4,
                archived_at: 1_718_805_700,
            },
            angles: vec![ArchiveAngle {
                camera: "front".to_owned(),
                file_ref: "archive/2026-06-19/clip-a/front.mp4".to_owned(),
                offset_ms: 0,
                duration_s: Some(60),
                size_bytes: 1024,
            }],
        }
    }

    #[test]
    fn archived_clips_folder_class_is_rejected() {
        assert!(parse_folder_class("ArchivedClips").is_err());
    }

    #[test]
    fn validation_rejects_duplicate_camera() {
        let mut request = payload();
        let angle = request.angles.first().cloned().unwrap();
        request.angles.push(angle);
        assert!(validate_payload(&request).is_err());
    }

    #[test]
    fn validation_accepts_valid_payload() {
        assert!(validate_payload(&payload()).is_ok());
    }

    #[test]
    fn validation_accepts_real_six_camera_recent_clip_payload() {
        let mut request = payload();
        request.archive.file_count = 6;
        request.angles.push(ArchiveAngle {
            camera: "back".to_owned(),
            file_ref: "archive/2026-06-19/clip-a/back.mp4".to_owned(),
            offset_ms: 0,
            duration_s: Some(60),
            size_bytes: 1024,
        });
        request.angles.push(ArchiveAngle {
            camera: "left_repeater".to_owned(),
            file_ref: "archive/2026-06-19/clip-a/left_repeater.mp4".to_owned(),
            offset_ms: 0,
            duration_s: Some(60),
            size_bytes: 1024,
        });
        request.angles.push(ArchiveAngle {
            camera: "right_repeater".to_owned(),
            file_ref: "archive/2026-06-19/clip-a/right_repeater.mp4".to_owned(),
            offset_ms: 0,
            duration_s: Some(60),
            size_bytes: 1024,
        });
        request.angles.push(ArchiveAngle {
            camera: "left_pillar".to_owned(),
            file_ref: "archive/2026-06-19/clip-a/left_pillar.mp4".to_owned(),
            offset_ms: 0,
            duration_s: Some(60),
            size_bytes: 1024,
        });
        request.angles.push(ArchiveAngle {
            camera: "right_pillar".to_owned(),
            file_ref: "archive/2026-06-19/clip-a/right_pillar.mp4".to_owned(),
            offset_ms: 0,
            duration_s: Some(60),
            size_bytes: 1024,
        });

        assert!(validate_payload(&request).is_ok());
    }

    #[test]
    fn validation_accepts_left_pillar_single_angle_payload() {
        let mut request = payload();
        let angle = request.angles.first_mut().expect("payload has one angle");
        angle.camera = "left_pillar".to_owned();
        angle.file_ref = "archive/2026-06-19/clip-a/left_pillar.mp4".to_owned();
        assert!(validate_payload(&request).is_ok());
    }

    #[test]
    fn validation_rejects_bogus_left_camera_label() {
        let mut request = payload();
        let angle = request.angles.first_mut().expect("payload has one angle");
        angle.camera = "left".to_owned();
        angle.file_ref = "archive/2026-06-19/clip-a/left.mp4".to_owned();
        assert!(validate_payload(&request).is_err());
    }

    fn new_temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos();
        let name = format!("indexd-server-test-{}-{nanos}", std::process::id());
        let dir = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn insert_archive_item(
        conn: &Connection,
        path: &str,
        folder_class: &str,
        archived_at: i64,
        durable: i64,
        pinned: i64,
    ) -> i64 {
        conn.execute(
            "INSERT INTO archive_items
                (folder_class, path, size_bytes, file_count, archived_at, durable, pinned, created_at, updated_at)
             VALUES (?1, ?2, 4096, 1, ?3, ?4, ?5, 0, 0)",
            params![folder_class, path, archived_at, durable, pinned],
        )
        .expect("insert archive item");
        let archive_item_id = conn.last_insert_rowid();
        // The recency gate now keys on clips.started_at, so link a clip whose
        // recording instant mirrors archived_at to preserve the intended age.
        conn.execute(
            "INSERT INTO clips
                (canonical_key, started_at, partition, folder_class, created_at, updated_at)
             VALUES (?1, ?2, 'p', ?3, 0, 0)",
            params![path, archived_at, folder_class],
        )
        .expect("insert clip");
        let clip_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO archive_item_clips (archive_item_id, clip_id) VALUES (?1, ?2)",
            params![archive_item_id, clip_id],
        )
        .expect("link archive item to clip");
        archive_item_id
    }

    fn send(socket_path: &Path, request: &Request) -> Response {
        let mut stream = UnixStream::connect(socket_path).expect("connect indexd server");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set read timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .expect("set write timeout");
        let payload = serde_json::to_vec(request).expect("encode request");
        write_frame(&mut stream, &payload).expect("write request frame");
        let response_payload = read_frame(&mut stream, MAX_REQUEST_FRAME).expect("read response");
        serde_json::from_slice(&response_payload).expect("decode response")
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn socket_delete_lifecycle_and_lease_denial_roundtrip() {
        let conn = open_in_memory().expect("open db");
        let candidate_id =
            insert_archive_item(&conn, "archive/recent/old-1", "RecentClips", 100, 1, 0);
        let leased_id =
            insert_archive_item(&conn, "archive/recent/leased", "RecentClips", 90, 1, 0);
        let conn = Arc::new(Mutex::new(conn));
        let boot = Arc::new(BootContext::new());
        {
            let locked = conn.lock().expect("lock db");
            locked
                .execute(
                    "INSERT INTO leases
                        (archive_item_id, kind, holder, gen, boot_id, expires_mono_ms)
                     VALUES (?1, 'playback', 'webd:test', 'lease-gen', ?2, ?3)",
                    params![leased_id, boot.boot_id(), boot.mono_now_ms() + 120_000],
                )
                .expect("insert lease");
        }

        let dir = new_temp_dir();
        let socket_path = dir.join("indexd.sock");
        let _server =
            spawn(&conn, &boot, &socket_path, Duration::from_secs(2)).expect("spawn indexd server");
        let candidates = send(
            &socket_path,
            &Request::ListEvictionCandidates {
                recency_floor_epoch: 500,
                allow_undurable: false,
                limit: 8,
            },
        );
        assert!(matches!(candidates, Response::EvictionCandidates { .. }));
        let Response::EvictionCandidates { items } = candidates else {
            unreachable!();
        };
        assert!(items.iter().any(|item| item.id == candidate_id));
        assert_eq!(
            send(
                &socket_path,
                &Request::ClaimEvictionCandidate {
                    id: candidate_id,
                    recency_floor_epoch: 500,
                    allow_undurable: false,
                }
            ),
            Response::Claimed {}
        );
        assert_eq!(
            send(
                &socket_path,
                &Request::MarkArchiveDeleting { id: candidate_id }
            ),
            Response::Acked {}
        );
        assert_eq!(
            send(
                &socket_path,
                &Request::MarkArchiveDeleted {
                    id: candidate_id,
                    bytes_freed: 4096
                }
            ),
            Response::Acked {}
        );
        let second_claim = send(
            &socket_path,
            &Request::ClaimEvictionCandidate {
                id: candidate_id,
                recency_floor_epoch: 500,
                allow_undurable: false,
            },
        );
        assert!(matches!(
            second_claim,
            Response::ClaimDenied { .. } | Response::NotFound {}
        ));
        assert_eq!(
            send(
                &socket_path,
                &Request::ClaimEvictionCandidate {
                    id: leased_id,
                    recency_floor_epoch: 500,
                    allow_undurable: false,
                }
            ),
            Response::ClaimDenied {
                reason: "unexpired lease".to_owned()
            }
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn socket_undurable_opt_in_roundtrip() {
        let conn = open_in_memory().expect("open db");
        let undurable_id = insert_archive_item(
            &conn,
            "archive/recent/old-undurable",
            "RecentClips",
            80,
            0,
            0,
        );
        let conn = Arc::new(Mutex::new(conn));
        let boot = Arc::new(BootContext::new());
        let dir = new_temp_dir();
        let socket_path = dir.join("indexd.sock");
        let _server =
            spawn(&conn, &boot, &socket_path, Duration::from_secs(2)).expect("spawn indexd server");

        let denied_list = send(
            &socket_path,
            &Request::ListEvictionCandidates {
                recency_floor_epoch: 500,
                allow_undurable: false,
                limit: 8,
            },
        );
        let Response::EvictionCandidates { items } = denied_list else {
            unreachable!();
        };
        assert!(!items.iter().any(|item| item.id == undurable_id));

        let allowed_list = send(
            &socket_path,
            &Request::ListEvictionCandidates {
                recency_floor_epoch: 500,
                allow_undurable: true,
                limit: 8,
            },
        );
        let Response::EvictionCandidates { items } = allowed_list else {
            unreachable!();
        };
        assert!(items.iter().any(|item| item.id == undurable_id));

        assert_eq!(
            send(
                &socket_path,
                &Request::ClaimEvictionCandidate {
                    id: undurable_id,
                    recency_floor_epoch: 500,
                    allow_undurable: false,
                }
            ),
            Response::ClaimDenied {
                reason: "ineligible".to_owned()
            }
        );
        assert_eq!(
            send(
                &socket_path,
                &Request::ClaimEvictionCandidate {
                    id: undurable_id,
                    recency_floor_epoch: 500,
                    allow_undurable: true,
                }
            ),
            Response::Claimed {}
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    #[allow(clippy::too_many_lines, clippy::panic, clippy::single_match_else)]
    fn socket_cloud_rpc_roundtrip() {
        let conn = open_in_memory().expect("open db");
        conn.execute(
            "INSERT INTO archive_items
                (id, folder_class, path, size_bytes, file_count, archived_at, created_at, updated_at)
             VALUES (101, 'RecentClips', 'archive/cloud/101', 30, 2, 100, 0, 0)",
            [],
        )
        .expect("insert archive item");
        let conn = Arc::new(Mutex::new(conn));
        let boot = Arc::new(BootContext::new());
        let dir = new_temp_dir();
        let socket_path = dir.join("indexd.sock");
        let _server =
            spawn(&conn, &boot, &socket_path, Duration::from_secs(2)).expect("spawn indexd server");

        let hash_a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let hash_b = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

        let queued = send(
            &socket_path,
            &Request::CloudQueueUpsert {
                item: CloudQueueUpsertWire {
                    archive_item_id: 101,
                    child_key: "child-a".to_owned(),
                    destination_id: "dest".to_owned(),
                    remote_key: "rk/a".to_owned(),
                    category: "bulk".to_owned(),
                    seq: 1,
                    total_bytes: 10,
                    content_sha256: hash_a.to_owned(),
                    expected_hash: None,
                    verify_alg: "none".to_owned(),
                },
            },
        );
        assert_eq!(
            queued,
            Response::CloudQueueState {
                state: "queued".to_owned()
            }
        );
        let _ = send(
            &socket_path,
            &Request::CloudQueueUpsert {
                item: CloudQueueUpsertWire {
                    archive_item_id: 101,
                    child_key: "child-b".to_owned(),
                    destination_id: "dest".to_owned(),
                    remote_key: "rk/b".to_owned(),
                    category: "bulk".to_owned(),
                    seq: 2,
                    total_bytes: 20,
                    content_sha256: hash_b.to_owned(),
                    expected_hash: None,
                    verify_alg: "none".to_owned(),
                },
            },
        );

        let lease = send(
            &socket_path,
            &Request::UploadLeaseAcquire {
                archive_item_id: 101,
                ttl_ms: 1_000,
            },
        );
        let Response::UploadLeaseAcquired {
            granted: true,
            token: Some(token),
            boot_id: Some(_),
            expires_mono_ms: Some(_),
        } = lease
        else {
            panic!("unexpected lease response: {lease:?}");
        };
        let renewed = send(
            &socket_path,
            &Request::UploadLeaseRenew {
                token: token.clone(),
                ttl_ms: 2_000,
            },
        );
        assert!(matches!(
            renewed,
            Response::UploadLeaseRenewed {
                ok: true,
                expires_mono_ms: Some(_)
            }
        ));
        assert_eq!(
            send(
                &socket_path,
                &Request::UploadLeaseRelease {
                    token: token.clone()
                }
            ),
            Response::UploadLeaseReleased { ok: true }
        );

        let first_commit = send(
            &socket_path,
            &Request::CloudUploadCommit {
                queue_pk: CloudQueuePkWire {
                    destination_id: "dest".to_owned(),
                    remote_key: "rk/a".to_owned(),
                },
                attempt_id: "attempt-a".to_owned(),
                hash: hash_a.to_owned(),
                hash_alg: "sha256".to_owned(),
                size: 10,
            },
        );
        assert_eq!(
            first_commit,
            Response::CloudUploadCommitted {
                ok: true,
                durable_parent: false
            }
        );
        let second_commit = send(
            &socket_path,
            &Request::CloudUploadCommit {
                queue_pk: CloudQueuePkWire {
                    destination_id: "dest".to_owned(),
                    remote_key: "rk/b".to_owned(),
                },
                attempt_id: "attempt-b".to_owned(),
                hash: hash_b.to_owned(),
                hash_alg: "sha256".to_owned(),
                size: 20,
            },
        );
        assert_eq!(
            second_commit,
            Response::CloudUploadCommitted {
                ok: true,
                durable_parent: true
            }
        );

        let fail_state = send(
            &socket_path,
            &Request::CloudQueueUpsert {
                item: CloudQueueUpsertWire {
                    archive_item_id: 101,
                    child_key: "child-c".to_owned(),
                    destination_id: "dest".to_owned(),
                    remote_key: "rk/c".to_owned(),
                    category: "bulk".to_owned(),
                    seq: 3,
                    total_bytes: 5,
                    content_sha256:
                        "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                            .to_owned(),
                    expected_hash: None,
                    verify_alg: "none".to_owned(),
                },
            },
        );
        assert_eq!(
            fail_state,
            Response::CloudQueueState {
                state: "queued".to_owned()
            }
        );
        let failed = send(
            &socket_path,
            &Request::CloudUploadFail {
                queue_pk: CloudQueuePkWire {
                    destination_id: "dest".to_owned(),
                    remote_key: "rk/c".to_owned(),
                },
                attempt_id: "attempt-c".to_owned(),
                error_class: "timeout".to_owned(),
                not_before: Some(1234),
                terminal: false,
            },
        );
        assert_eq!(
            failed,
            Response::CloudUploadFailed {
                ok: true,
                state: "failed".to_owned()
            }
        );

        let candidates = send(
            &socket_path,
            &Request::CloudCandidates {
                folders: vec!["RecentClips".to_owned()],
                after_cursor: None,
                limit: 10,
            },
        );
        assert!(matches!(candidates, Response::CloudCandidates { .. }));
        let queue = send(
            &socket_path,
            &Request::CloudQueueLoad {
                after_cursor: None,
                limit: 10,
            },
        );
        assert!(matches!(queue, Response::CloudQueuePage { .. }));
        let history = send(
            &socket_path,
            &Request::CloudHistoryLoad {
                after_cursor: None,
                limit: 10,
            },
        );
        assert!(matches!(history, Response::CloudHistoryPage { .. }));
        let stats = send(&socket_path, &Request::CloudStatsGet {});
        assert!(matches!(stats, Response::CloudStats { .. }));
        let reset = send(&socket_path, &Request::CloudStatsReset {});
        assert!(matches!(reset, Response::CloudStatsReset { ok: true, .. }));
        let config = send(&socket_path, &Request::CloudConfigGet {});
        let Response::CloudConfig { config } = config else {
            panic!("expected cloud config response");
        };
        assert_eq!(config.max_attempts, 5);
        let updated = send(
            &socket_path,
            &Request::CloudConfigPut {
                config: CloudConfigWire {
                    auto_sync: false,
                    ..config
                },
            },
        );
        let Response::CloudConfig { config } = updated else {
            panic!("expected cloud config response");
        };
        assert!(!config.auto_sync);
        let retried = send(
            &socket_path,
            &Request::CloudQueueRetry {
                archive_item_id: 101,
                child_key: Some("child-c".to_owned()),
                resolution: CloudQueueRetryResolutionWire::Replace,
            },
        );
        assert_eq!(
            retried,
            Response::CloudQueueState {
                state: "queued".to_owned()
            }
        );

        let _ = std::fs::remove_dir_all(dir);
    }
}

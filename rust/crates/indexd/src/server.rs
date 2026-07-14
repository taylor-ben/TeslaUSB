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

use crate::db::ingest::{
    ArchiveAngleRegistration, ArchiveRegistration, ArchiveUnitRegistration, register_archived_clip,
    register_quarantined_clip,
};
use crate::db::mutations::{
    BootContext, has_unexpired_lease, mark_deleted, mark_deleting, quarantine, release_delete_claim,
    set_pref,
};
use crate::db::now_epoch_s;
use crate::db::reads::{list_eviction_candidates, list_recovery_rows};
use crate::model::FolderClass;
use crate::proto::{
    EvictionCandidateWire, RecoveryRowWire, RegisterArchivedClip, Request, Response, read_request,
    write_response,
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

fn handle_release_archive_delete_claim(conn: &Arc<Mutex<Connection>>, id: i64) -> Result<(), String> {
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

fn handle_list_recovery_rows(conn: &Arc<Mutex<Connection>>) -> Result<Vec<RecoveryRowWire>, String> {
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
        ArchiveAngle, ArchiveUnit, MAX_REQUEST_FRAME, RegisterArchivedClip, Request, Response,
        read_frame, write_frame,
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
        let candidate_id = insert_archive_item(
            &conn,
            "archive/recent/old-1",
            "RecentClips",
            100,
            1,
            0,
        );
        let leased_id = insert_archive_item(
            &conn,
            "archive/recent/leased",
            "RecentClips",
            90,
            1,
            0,
        );
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
        let _server = spawn(&conn, &boot, &socket_path, Duration::from_secs(2))
            .expect("spawn indexd server");
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
        let _server = spawn(&conn, &boot, &socket_path, Duration::from_secs(2))
            .expect("spawn indexd server");

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
}

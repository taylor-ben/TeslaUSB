//! The **consumer** half of the `scannerd → indexd` seam.
//!
//! [`apply`] takes a [`ScanBatch`] of facts — produced by `scannerd`'s
//! [`produce`](scannerd::produce::produce) over the untrusted raw image —
//! and runs the one-transaction DB cycle that was previously the tail of
//! `indexd`'s in-process `run_scan_pass`:
//!
//! ```text
//! validate batch → open tx → per record:
//!     front  → parse-state-driven clip/waypoint apply + upsert_angle +
//!              upsert_front_parse_attempt
//!     other  → ensure_clip + upsert_angle
//!   → prune vanished clips (only if `complete`) → derive → rebuild → commit
//! ```
//!
//! `indexd` is the **sole DB writer** and does **no raw parsing**
//! (`indexd.md` §1/§3). Every field in the batch is *untrusted data*: the
//! batch is validated against the wire caps, each record is validated
//! individually (a single malformed record is skipped + counted, never
//! aborting the batch), and the forge-prone `is_front` / `view_kind` are
//! **derived** here from the camera label and bucket rather than trusted
//! off the wire.
//!
//! ## Parity with the legacy in-process pass
//!
//! [`crate::scan::run_scan_pass`] is now `produce(...) + apply(...)`, so
//! the in-process path and the future cross-process (socket) path share
//! these exact two halves. The DB-outcome counters live here (only the
//! writer knows whether a row committed); the producer's diagnostic
//! counters (`unplaceable_*`) are merged back in `run_scan_pass` to
//! reproduce the legacy [`ScanReport`](crate::scan::ScanReport) exactly.

use std::collections::HashSet;

use rusqlite::Connection;
use scannerd::record::{ClipAngleRecord, ScanBatch, WireWaypoint};
use teslausb_core::sei::tesla::{AutopilotState, Gear};

use crate::db::DbError;
use crate::db::ingest::{
    AngleFacts, ClipEventFacts, ClipFacts, MediaFacts, ensure_clip, load_clip_events,
    load_derive_clips, prune_missing_clip_events, prune_missing_clips, prune_orphan_front_parse_attempts,
    prune_missing_media, rebuild_derived, replace_clip_waypoints, upsert_angle_scan_preserving, upsert_clip,
    upsert_clip_event, upsert_front_parse_attempt, upsert_media,
};
use crate::derive::{DeriveConfig, derive};
use crate::model::{DeriveWaypoint, FolderClass};
use crate::scan::ScanError;

/// DB-outcome counts from one [`apply`] call. These are the counters only
/// the writer can know (a row actually committed); the producer's
/// `unplaceable_*` diagnostics are merged with these in
/// [`run_scan_pass`](crate::scan::run_scan_pass) to form the legacy
/// `ScanReport`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ApplyReport {
    /// Clip angles written this pass (front + other) on full DB success.
    pub clips_written: usize,
    /// Front clips whose waypoint cache was replaced this pass.
    pub front_walked: usize,
    /// Cached waypoints written this pass.
    pub waypoints: usize,
    /// Records skipped: failed per-record validation or errored mid-write.
    pub record_errors: usize,
    /// Front records applied in the `parse_error` state.
    pub front_parse_errors: usize,
    /// Front records applied in the `read_error` state.
    pub front_read_errors: usize,
    /// Front records applied in the `no_waypoints` state.
    pub front_no_waypoints: usize,
    /// Clips pruned (present only when the batch was `complete`).
    pub pruned: usize,
    /// Media rows upserted this pass (p2 inventory).
    pub media_written: usize,
    /// Media rows pruned (present only when the producer inventoried media
    /// AND the batch was `complete`).
    pub media_pruned: usize,
    /// Clip-event sidecar rows upserted this pass.
    pub clip_events_written: usize,
    /// Clip-event sidecar rows pruned (present only when the producer
    /// inventoried clip events AND the batch was `complete`).
    pub clip_events_pruned: usize,
    /// Trips materialized after the rebuild.
    pub trips: usize,
    /// Events materialized after the rebuild (driving + sentry).
    pub events: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrontApplyState {
    ParsedWithWaypoints,
    NoWaypoints,
    ParseError,
    ReadError,
}

impl FrontApplyState {
    /// Map the wire `parse_state` to an apply outcome. The four known states
    /// map directly. Everything else — `None` (a pre-`parse_state` producer,
    /// though the `PROTOCOL_VERSION` gate already rejects an older batch
    /// wholesale, so this is defensive only) and any unrecognized/`legacy_unknown`
    /// string — collapses to `ParsedWithWaypoints`. That is safe because the
    /// apply path NEVER clears the waypoint cache for this state unless the
    /// record carries a non-empty list (see the data-integrity invariant in
    /// `apply_record`); an empty/unknown record is therefore non-destructive.
    fn from_wire(parse_state: Option<&str>) -> Self {
        match parse_state {
            Some("no_waypoints") => Self::NoWaypoints,
            Some("parse_error") => Self::ParseError,
            Some("read_error") => Self::ReadError,
            _ => Self::ParsedWithWaypoints,
        }
    }

    const fn as_wire(self) -> &'static str {
        match self {
            Self::ParsedWithWaypoints => "parsed_with_waypoints",
            Self::NoWaypoints => "no_waypoints",
            Self::ParseError => "parse_error",
            Self::ReadError => "read_error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ApplyRecordOutcome {
    waypoints_written: usize,
    front_state: Option<FrontApplyState>,
}

/// D1 `view_kind` for a freshly scanned car-volume clip, recomputed from
/// the bucket (never trusted off the wire). `ArchivedClips` are Pi-side
/// archive copies and carry `'archive'` (the durable/playable source);
/// every live car-volume bucket carries `'ro_usb'` (the read-only USB
/// view Tesla may rotate at any time — never retention-leasable / never an
/// upload source per `indexd-schema.md` §3.1 + `uploadd.md` §3).
fn view_kind_for(folder_class: FolderClass) -> &'static str {
    if matches!(folder_class, FolderClass::ArchivedClips) {
        "archive"
    } else {
        "ro_usb"
    }
}

/// Map a wire waypoint back to the internal derive-waypoint 1:1. The SEI
/// enums are decoded from their proto integers via `From<u32>`, which
/// round-trips the forward-compat `Unknown(n)` case losslessly.
fn map_waypoint(w: &WireWaypoint) -> DeriveWaypoint {
    DeriveWaypoint {
        frame_index: w.frame_index,
        offset_ms: w.offset_ms,
        absolute_utc: w.absolute_utc,
        lat: w.lat,
        lon: w.lon,
        speed: w.speed,
        heading: w.heading,
        accel_x: w.accel_x,
        accel_y: w.accel_y,
        accel_z: w.accel_z,
        autopilot_state: AutopilotState::from(w.autopilot_state),
        gear: Gear::from(w.gear),
        has_gps_fix: w.has_gps_fix,
    }
}

/// Build the [`ClipFacts`] for a record. Front clips carry the probed
/// `ended_at` / `duration_s`; non-front records always carry `None` for
/// both (the producer never fills them), so a single uniform construction
/// reproduces both legacy `process_front` and `process_other`.
fn clip_facts(record: &ClipAngleRecord, folder_class: FolderClass) -> ClipFacts {
    ClipFacts {
        canonical_key: record.canonical_key.clone(),
        started_at: record.started_at,
        ended_at: record.ended_at,
        partition: record.partition.clone(),
        folder_class,
        duration_s: record.duration_s,
    }
}

/// Build the [`AngleFacts`] for a record, recomputing `view_kind` from the
/// bucket so it cannot be forged independently off the wire.
fn angle_facts(record: &ClipAngleRecord, folder_class: FolderClass) -> AngleFacts {
    AngleFacts {
        camera: record.angle.camera.clone(),
        file_ref: record.angle.file_ref.clone(),
        view_kind: view_kind_for(folder_class).to_owned(),
        offset_ms: record.angle.offset_ms,
        duration_s: record.angle.duration_s,
        size_bytes: record.angle.size_bytes,
    }
}

/// Ingest one validated record. Front angles are parse-state driven:
/// `parsed_with_waypoints` / `no_waypoints` upsert+replace waypoints,
/// `parse_error` / `read_error` ensure-only (non-destructive, no waypoint
/// replacement), and all front paths upsert the parse-attempt row and
/// angle. Non-front angles only ensure the clip exists (never downgrading
/// a front-resolved instant) and upsert the angle.
///
/// Errors are surfaced to the caller, which counts the record and moves on.
/// There is **no per-record savepoint**: any earlier write for a record
/// that fails partway (e.g. a front `upsert_clip` + `replace_clip_waypoints`
/// that succeed before `upsert_angle` fails) remains in the open
/// transaction and commits with the rest — faithfully reproducing the
/// legacy in-process pass's partial-write-then-continue behavior.
fn apply_record(
    conn: &Connection,
    record: &ClipAngleRecord,
    is_front: bool,
) -> Result<ApplyRecordOutcome, DbError> {
    let folder_class = FolderClass::from_db_str(record.bucket.as_db_str());
    let facts = clip_facts(record, folder_class);
    let angle = angle_facts(record, folder_class);
    if is_front {
        let front_state = FrontApplyState::from_wire(record.parse_state.as_deref());
        let derived: Vec<DeriveWaypoint> = record.waypoints.iter().map(map_waypoint).collect();
        let clip_id = match front_state {
            FrontApplyState::ParseError | FrontApplyState::ReadError => ensure_clip(conn, &facts)?,
            FrontApplyState::ParsedWithWaypoints | FrontApplyState::NoWaypoints => {
                upsert_clip(conn, &facts)?
            }
        };
        // Data-integrity invariant: `replace_clip_waypoints` is a
        // DELETE-then-insert, so a destructive clear of prior good GPS may
        // ONLY happen for the explicit `no_waypoints` state. A
        // `parsed_with_waypoints` (or unknown/legacy state that `from_wire`
        // maps here) carrying an EMPTY list is incoherent — never let it
        // wipe the cache; leave the prior waypoints intact.
        let waypoints_written = match front_state {
            FrontApplyState::ParsedWithWaypoints => {
                if derived.is_empty() {
                    0
                } else {
                    replace_clip_waypoints(conn, clip_id, &derived)?;
                    derived.len()
                }
            }
            FrontApplyState::NoWaypoints => {
                replace_clip_waypoints(conn, clip_id, &[])?;
                0
            }
            FrontApplyState::ParseError | FrontApplyState::ReadError => 0,
        };
        upsert_angle_scan_preserving(conn, clip_id, &angle)?;
        upsert_front_parse_attempt(
            conn,
            &record.canonical_key,
            front_state.as_wire(),
            record.parse_fingerprint.as_deref(),
            record.parser_version,
        )?;
        Ok(ApplyRecordOutcome {
            waypoints_written,
            front_state: Some(front_state),
        })
    } else {
        let clip_id = ensure_clip(conn, &facts)?;
        upsert_angle_scan_preserving(conn, clip_id, &angle)?;
        Ok(ApplyRecordOutcome {
            waypoints_written: 0,
            front_state: None,
        })
    }
}

/// Apply one batch of scanner facts to the catalog in a single transaction.
///
/// The batch is validated at the batch level first (protocol version +
/// gross-size caps); a failure there is fatal (rejects the whole batch).
/// Each record is then validated and applied individually: a record that
/// fails validation, references a key absent from a `complete` batch's
/// present set, or errors mid-write is skipped and counted in
/// [`ApplyReport::record_errors`] — one bad record never aborts the batch
/// (matching the legacy tolerate-bad-clip behavior). The prune step runs
/// **only** when the batch is `complete` (the present set is trustworthy),
/// then the derivation is rebuilt and the transaction commits atomically.
///
/// # Errors
///
/// Returns [`ScanError::Batch`] if the batch fails batch-level validation,
/// or [`ScanError::Db`] if a transaction/prune/derive/commit step fails.
#[allow(clippy::too_many_lines)]
pub fn apply(
    conn: &mut Connection,
    batch: &ScanBatch,
    derive_cfg: DeriveConfig,
) -> Result<ApplyReport, ScanError> {
    batch.validate()?;

    let present: HashSet<&str> = batch.present_keys.iter().map(String::as_str).collect();
    let mut report = ApplyReport::default();

    let tx = conn.transaction().map_err(DbError::from)?;
    for record in &batch.records {
        if record.validate().is_err() {
            report.record_errors += 1;
            continue;
        }
        // A `complete` batch's present set is trustworthy and must contain
        // every emitted record; a record outside it is inconsistent (only
        // reachable over a forged wire — the in-process producer always
        // satisfies it) and is skipped rather than ingested.
        if batch.complete && !present.contains(record.canonical_key.as_str()) {
            report.record_errors += 1;
            continue;
        }
        let is_front = record.is_front();
        match apply_record(&tx, record, is_front) {
            Ok(outcome) => {
                report.clips_written += 1;
                if is_front {
                    report.front_walked += 1;
                    report.waypoints += outcome.waypoints_written;
                    match outcome.front_state {
                        Some(FrontApplyState::ParseError) => report.front_parse_errors += 1,
                        Some(FrontApplyState::ReadError) => report.front_read_errors += 1,
                        Some(FrontApplyState::NoWaypoints) => report.front_no_waypoints += 1,
                        _ => {}
                    }
                }
            }
            Err(_) => report.record_errors += 1,
        }
    }

    if batch.complete {
        let present_keys: HashSet<String> = batch.present_keys.iter().cloned().collect();
        report.pruned = prune_missing_clips(&tx, &present_keys)?;
        let _ = prune_orphan_front_parse_attempts(&tx)?;
    }

    // MEDIA (p2) inventory. Only a media-aware producer touches the catalog:
    // a batch from an older scannerd has `media_inventory == false`, so we
    // neither upsert nor prune and the existing rows are preserved.
    if batch.media_inventory {
        for media in &batch.media {
            if media.validate().is_err() {
                report.record_errors += 1;
                continue;
            }
            // A complete inventory's present set must contain every emitted
            // media path; an inconsistent record (only reachable over a
            // forged wire) is skipped rather than written.
            if batch.complete && !batch.media_present_paths.contains(&media.rel_path) {
                report.record_errors += 1;
                continue;
            }
            upsert_media(
                &tx,
                &MediaFacts {
                    partition: media.partition.clone(),
                    rel_path: media.rel_path.clone(),
                    name: media.name.clone(),
                    size_bytes: media.size_bytes,
                    modified: media.modified_local.clone(),
                },
            )?;
            report.media_written += 1;
        }
        // Prune only when the inventory is also a complete scan: a torn pass
        // could omit a present file and wrongly delete its row.
        if batch.complete {
            let present_paths: HashSet<String> =
                batch.media_present_paths.iter().cloned().collect();
            report.media_pruned = prune_missing_media(&tx, &present_paths)?;
        }
    }

    if batch.clip_events_inventory {
        for ev in &batch.clip_events {
            if ev.validate().is_err() {
                report.record_errors += 1;
                continue;
            }
            // Present-set = the event_dir_keys this batch emitted; on a
            // complete batch an inconsistent record is unreachable from the
            // in-process producer.
            upsert_clip_event(
                &tx,
                &ClipEventFacts {
                    event_dir_key: ev.event_dir_key.clone(),
                    bucket: ev.bucket.as_db_str().to_owned(),
                    primary_canonical_key: ev.primary_canonical_key.clone(),
                    timestamp_utc: ev.timestamp_utc,
                    timestamp_local_naive: ev.timestamp_local_naive,
                    timestamp_has_offset: ev.timestamp_has_offset,
                    est_lat: ev.est_lat,
                    est_lon: ev.est_lon,
                    reason: ev.reason.clone(),
                    city: ev.city.clone(),
                    camera: ev.camera.clone(),
                },
            )?;
            report.clip_events_written += 1;
        }
        if batch.complete {
            let present_event_keys: HashSet<String> = batch
                .clip_events
                .iter()
                .map(|e| e.event_dir_key.clone())
                .collect();
            report.clip_events_pruned = prune_missing_clip_events(&tx, &present_event_keys)?;
        }
    }

    let clips = load_derive_clips(&tx)?;
    let clip_events = load_clip_events(&tx)?;
    let derivation = derive(&clips, &clip_events, derive_cfg);
    rebuild_derived(&tx, &derivation)?;
    tx.commit().map_err(DbError::from)?;

    report.trips = derivation.trips.len();
    let trip_events: usize = derivation.trips.iter().map(|t| t.events.len()).sum();
    report.events = trip_events + derivation.sentry_events.len();

    Ok(report)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::float_cmp,
        clippy::indexing_slicing
    )]

    use super::{apply, map_waypoint, view_kind_for};
    use crate::db::open_in_memory;
    use crate::derive::{DeriveConfig, waypoint_from_walk};
    use crate::model::FolderClass;
    use rusqlite::Connection;
    use scannerd::produce::wire_waypoint_from_walk;
    use scannerd::record::{
        AngleRecord, Bucket, ClipAngleRecord, ClipEventRecord, MediaFileRecord, PROTOCOL_VERSION,
        PARSER_VERSION, ProducerStats, ScanBatch,
    };
    use scannerd::seiwalk::Waypoint;
    use teslausb_core::sei::tesla::{AutopilotState, Gear, SeiMessage};

    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    fn front_angle(camera: &str, dir: &str) -> AngleRecord {
        AngleRecord {
            camera: camera.to_owned(),
            file_ref: format!("{dir}/2026-06-01_20-10-04-{camera}.mp4"),
            offset_ms: 0,
            duration_s: None,
            size_bytes: Some(1024),
        }
    }

    fn waypoint(frame: u32, offset_ms: f64, lat: f64, lon: f64) -> Waypoint {
        Waypoint {
            frame_index: frame,
            timestamp_ms: offset_ms,
            message: SeiMessage {
                latitude_deg: lat,
                longitude_deg: lon,
                vehicle_speed_mps: 12.0,
                heading_deg: 90.0,
                linear_acceleration_mps2_x: 0.5,
                linear_acceleration_mps2_y: -0.25,
                linear_acceleration_mps2_z: 9.8,
                autopilot_state: AutopilotState::Autosteer,
                gear_state: Gear::Drive,
                ..SeiMessage::default()
            },
        }
    }

    /// Load-bearing parity check: the wire round-trip
    /// (`wire_waypoint_from_walk` → `map_waypoint`) must produce the exact
    /// `DeriveWaypoint` the legacy in-process `waypoint_from_walk` produced
    /// — that value is what feeds `replace_clip_waypoints` and therefore
    /// the entire derivation. Covers GPS / no-GPS and the forward-compat
    /// `Unknown(n)` enum cases.
    #[test]
    fn wire_waypoint_maps_back_to_legacy_derive_waypoint() {
        let started_at = 1_700_000_000;
        let mut cases = vec![
            waypoint(0, 0.0, 37.5, -122.3),
            waypoint(7, 1500.0, 0.0, 0.0), // no GPS fix
        ];
        // Forward-compat unknown enum codes must survive the int encoding.
        let mut unknown = waypoint(9, 3000.0, 1.0, 2.0);
        unknown.message.autopilot_state = AutopilotState::Unknown(42);
        unknown.message.gear_state = Gear::Unknown(7);
        cases.push(unknown);

        for w in &cases {
            let via_wire = map_waypoint(&wire_waypoint_from_walk(w, started_at));
            let legacy = waypoint_from_walk(w, started_at);
            assert_eq!(
                via_wire, legacy,
                "wire round-trip diverged from legacy derive-waypoint"
            );
        }
    }

    #[test]
    fn view_kind_maps_archive_vs_ro_usb() {
        assert_eq!(view_kind_for(FolderClass::ArchivedClips), "archive");
        assert_eq!(view_kind_for(FolderClass::SavedClips), "ro_usb");
        assert_eq!(view_kind_for(FolderClass::RecentClips), "ro_usb");
    }

    fn front_record(key: &str, dir: &str, started_at: i64) -> ClipAngleRecord {
        let started = started_at;
        let waypoints = vec![
            wire_waypoint_from_walk(&waypoint(0, 0.0, 37.5, -122.3), started),
            wire_waypoint_from_walk(&waypoint(1, 1000.0, 37.5005, -122.3005), started),
        ];
        front_record_with_state(
            key,
            dir,
            started_at,
            "parsed_with_waypoints",
            waypoints,
            Some(started + 1),
            Some(1.0),
        )
    }

    fn front_record_with_state(
        key: &str,
        dir: &str,
        started_at: i64,
        parse_state: &str,
        waypoints: Vec<scannerd::record::WireWaypoint>,
        ended_at: Option<i64>,
        duration_s: Option<f64>,
    ) -> ClipAngleRecord {
        ClipAngleRecord {
            canonical_key: key.to_owned(),
            started_at,
            ended_at,
            partition: "slot0".to_owned(),
            bucket: Bucket::SavedClips,
            duration_s,
            angle: front_angle("front", dir),
            parse_state: Some(parse_state.to_owned()),
            parse_fingerprint: Some("f00d".to_owned()),
            parser_version: Some(PARSER_VERSION),
            waypoints,
        }
    }

    fn other_record(key: &str, dir: &str, camera: &str, started_at: i64) -> ClipAngleRecord {
        ClipAngleRecord {
            canonical_key: key.to_owned(),
            started_at,
            ended_at: None,
            partition: "slot0".to_owned(),
            bucket: Bucket::SavedClips,
            duration_s: None,
            angle: front_angle(camera, dir),
            parse_state: None,
            parse_fingerprint: None,
            parser_version: None,
            waypoints: Vec::new(),
        }
    }

    fn batch(records: Vec<ClipAngleRecord>, complete: bool) -> ScanBatch {
        let present_keys: Vec<String> = records.iter().map(|r| r.canonical_key.clone()).collect();
        ScanBatch {
            version: PROTOCOL_VERSION,
            generation: 1,
            complete,
            stats: ProducerStats::default(),
            present_keys,
            records,
            media: Vec::new(),
            media_present_paths: Vec::new(),
            media_inventory: false,
            clip_events: Vec::new(),
            clip_events_inventory: false,
        }
    }

    fn waypoint_count_for_key(conn: &Connection, key: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*)
               FROM clip_waypoints w
               JOIN clips c ON c.id = w.clip_id
              WHERE c.canonical_key = ?1",
            [key],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn front_attempt_state(conn: &Connection, key: &str) -> Option<String> {
        conn.query_row(
            "SELECT parse_state FROM front_parse_attempts WHERE canonical_key = ?1",
            [key],
            |r| r.get(0),
        )
        .ok()
    }

    #[test]
    fn apply_ingests_clip_angle_and_waypoints() {
        let mut conn = open_in_memory().unwrap();
        let dir = "TeslaCam/SavedClips/2026-06-01_20-10-04";
        let key = "0:TeslaCam/SavedClips/2026-06-01_20-10-04/2026-06-01_20-10-04";
        let b = batch(
            vec![
                front_record(key, dir, 1_700_000_000),
                other_record(key, dir, "back", 1_700_000_000),
            ],
            true,
        );

        let report = apply(&mut conn, &b, DeriveConfig::default()).unwrap();
        assert_eq!(report.clips_written, 2);
        assert_eq!(report.front_walked, 1);
        assert_eq!(report.waypoints, 2);
        assert_eq!(report.record_errors, 0);

        assert_eq!(count(&conn, "clips"), 1);
        assert_eq!(count(&conn, "angles"), 2);
        assert_eq!(count(&conn, "clip_waypoints"), 2);
    }

    #[test]
    fn legacy_front_record_without_parse_state_uses_replace_path() {
        let mut conn = open_in_memory().unwrap();
        let dir = "TeslaCam/SavedClips/2026-06-01_20-10-04";
        let key = "0:TeslaCam/SavedClips/2026-06-01_20-10-04/2026-06-01_20-10-04";
        let mut legacy = front_record(key, dir, 1_700_000_000);
        legacy.parse_state = None;
        legacy.parse_fingerprint = None;
        legacy.parser_version = None;
        apply(
            &mut conn,
            &batch(vec![legacy], true),
            DeriveConfig::default(),
        )
        .unwrap();
        assert_eq!(waypoint_count_for_key(&conn, key), 2);
        assert_eq!(
            front_attempt_state(&conn, key).as_deref(),
            Some("parsed_with_waypoints")
        );
    }

    #[test]
    fn parse_error_preserves_existing_waypoints() {
        let mut conn = open_in_memory().unwrap();
        let dir = "TeslaCam/SavedClips/2026-06-01_20-10-04";
        let key = "0:TeslaCam/SavedClips/2026-06-01_20-10-04/2026-06-01_20-10-04";
        apply(
            &mut conn,
            &batch(vec![front_record(key, dir, 1_700_000_000)], true),
            DeriveConfig::default(),
        )
        .unwrap();
        assert_eq!(waypoint_count_for_key(&conn, key), 2);

        let parse_error = front_record_with_state(
            key,
            dir,
            1_700_000_000,
            "parse_error",
            Vec::new(),
            None,
            None,
        );
        let report = apply(&mut conn, &batch(vec![parse_error], true), DeriveConfig::default())
            .unwrap();
        assert_eq!(report.front_parse_errors, 1);
        assert_eq!(waypoint_count_for_key(&conn, key), 2);
        assert_eq!(front_attempt_state(&conn, key).as_deref(), Some("parse_error"));
    }

    #[test]
    fn read_error_preserves_existing_waypoints() {
        let mut conn = open_in_memory().unwrap();
        let dir = "TeslaCam/SavedClips/2026-06-01_20-10-04";
        let key = "0:TeslaCam/SavedClips/2026-06-01_20-10-04/2026-06-01_20-10-04";
        apply(
            &mut conn,
            &batch(vec![front_record(key, dir, 1_700_000_000)], true),
            DeriveConfig::default(),
        )
        .unwrap();
        assert_eq!(waypoint_count_for_key(&conn, key), 2);

        let read_error = front_record_with_state(
            key,
            dir,
            1_700_000_000,
            "read_error",
            Vec::new(),
            None,
            None,
        );
        let report = apply(&mut conn, &batch(vec![read_error], true), DeriveConfig::default())
            .unwrap();
        assert_eq!(report.front_read_errors, 1);
        assert_eq!(waypoint_count_for_key(&conn, key), 2);
        assert_eq!(front_attempt_state(&conn, key).as_deref(), Some("read_error"));
    }

    #[test]
    fn unknown_parse_state_with_empty_waypoints_is_non_destructive() {
        // Finding-1 regression: an unknown/`legacy_unknown` parse_state (which
        // `from_wire` collapses to ParsedWithWaypoints) carrying an EMPTY
        // waypoint list must NEVER delete prior good GPS. Only the explicit
        // `no_waypoints` state may clear the cache.
        let mut conn = open_in_memory().unwrap();
        let dir = "TeslaCam/SavedClips/2026-06-01_20-10-04";
        let key = "0:TeslaCam/SavedClips/2026-06-01_20-10-04/2026-06-01_20-10-04";
        apply(
            &mut conn,
            &batch(vec![front_record(key, dir, 1_700_000_000)], true),
            DeriveConfig::default(),
        )
        .unwrap();
        assert_eq!(waypoint_count_for_key(&conn, key), 2);

        let unknown = front_record_with_state(
            key,
            dir,
            1_700_000_000,
            "legacy_unknown",
            Vec::new(),
            None,
            None,
        );
        apply(&mut conn, &batch(vec![unknown], true), DeriveConfig::default()).unwrap();
        // Prior GPS preserved; not wiped by the incoherent empty record.
        assert_eq!(waypoint_count_for_key(&conn, key), 2);
    }

    #[test]
    fn no_waypoints_clears_existing_waypoints_and_records_attempt() {
        let mut conn = open_in_memory().unwrap();
        let dir = "TeslaCam/SavedClips/2026-06-01_20-10-04";
        let key = "0:TeslaCam/SavedClips/2026-06-01_20-10-04/2026-06-01_20-10-04";
        apply(
            &mut conn,
            &batch(vec![front_record(key, dir, 1_700_000_000)], true),
            DeriveConfig::default(),
        )
        .unwrap();
        assert_eq!(waypoint_count_for_key(&conn, key), 2);

        let no_waypoints = front_record_with_state(
            key,
            dir,
            1_700_000_000,
            "no_waypoints",
            Vec::new(),
            Some(1_700_000_000),
            Some(0.0),
        );
        let report = apply(&mut conn, &batch(vec![no_waypoints], true), DeriveConfig::default())
            .unwrap();
        assert_eq!(report.front_no_waypoints, 1);
        assert_eq!(waypoint_count_for_key(&conn, key), 0);
        assert_eq!(front_attempt_state(&conn, key).as_deref(), Some("no_waypoints"));
    }

    #[test]
    fn no_waypoints_on_new_clip_records_attempt_with_empty_cache() {
        let mut conn = open_in_memory().unwrap();
        let dir = "TeslaCam/SavedClips/2026-06-01_20-10-04";
        let key = "0:TeslaCam/SavedClips/new/2026-06-01_20-10-04";
        let no_waypoints = front_record_with_state(
            key,
            dir,
            1_700_000_000,
            "no_waypoints",
            Vec::new(),
            Some(1_700_000_000),
            Some(0.0),
        );
        apply(
            &mut conn,
            &batch(vec![no_waypoints], true),
            DeriveConfig::default(),
        )
        .unwrap();
        assert_eq!(waypoint_count_for_key(&conn, key), 0);
        assert_eq!(front_attempt_state(&conn, key).as_deref(), Some("no_waypoints"));
    }

    #[test]
    fn front_error_states_are_distinctly_counted_in_report() {
        let mut conn = open_in_memory().unwrap();
        let dir = "TeslaCam/SavedClips/2026-06-01_20-10-04";
        let parse_key = "0:TeslaCam/SavedClips/a/2026-06-01_20-10-04";
        let read_key = "0:TeslaCam/SavedClips/b/2026-06-01_20-11-04";
        let b = batch(
            vec![
                front_record_with_state(
                    parse_key,
                    dir,
                    1_700_000_000,
                    "parse_error",
                    Vec::new(),
                    None,
                    None,
                ),
                front_record_with_state(
                    read_key,
                    dir,
                    1_700_000_100,
                    "read_error",
                    Vec::new(),
                    None,
                    None,
                ),
            ],
            true,
        );
        let report = apply(&mut conn, &b, DeriveConfig::default()).unwrap();
        assert_eq!(report.front_walked, 2);
        assert_eq!(report.front_parse_errors, 1);
        assert_eq!(report.front_read_errors, 1);
        assert_eq!(report.front_no_waypoints, 0);
    }

    #[test]
    fn parse_error_does_not_downgrade_clip_ended_at_or_duration() {
        let mut conn = open_in_memory().unwrap();
        let dir = "TeslaCam/SavedClips/2026-06-01_20-10-04";
        let key = "0:TeslaCam/SavedClips/2026-06-01_20-10-04/2026-06-01_20-10-04";
        apply(
            &mut conn,
            &batch(vec![front_record(key, dir, 1_700_000_000)], true),
            DeriveConfig::default(),
        )
        .unwrap();
        let before: (Option<i64>, Option<f64>) = conn
            .query_row(
                "SELECT ended_at, duration_s FROM clips WHERE canonical_key = ?1",
                [key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();

        let parse_error = front_record_with_state(
            key,
            dir,
            1_700_000_000,
            "parse_error",
            Vec::new(),
            None,
            None,
        );
        apply(
            &mut conn,
            &batch(vec![parse_error], true),
            DeriveConfig::default(),
        )
        .unwrap();
        let after: (Option<i64>, Option<f64>) = conn
            .query_row(
                "SELECT ended_at, duration_s FROM clips WHERE canonical_key = ?1",
                [key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(after, before);
    }

    fn media_rec(rel_path: &str) -> MediaFileRecord {
        MediaFileRecord {
            partition: "slot1".to_owned(),
            rel_path: rel_path.to_owned(),
            name: rel_path.to_owned(),
            size_bytes: 219_770,
            modified_local: Some("2026-06-01T20:10:04".to_owned()),
        }
    }

    fn media_batch(media: Vec<MediaFileRecord>, inventory: bool, complete: bool) -> ScanBatch {
        let mut b = batch(Vec::new(), complete);
        b.media_present_paths = media.iter().map(|m| m.rel_path.clone()).collect();
        b.media = media;
        b.media_inventory = inventory;
        b
    }

    fn media_row(conn: &Connection, rel_path: &str) -> Option<(String, i64, Option<String>)> {
        conn.query_row(
            "SELECT name, size_bytes, modified FROM media_entries WHERE rel_path = ?1",
            [rel_path],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok()
    }

    fn clip_event_rec(
        event_dir_key: &str,
        est_lat: Option<f64>,
        est_lon: Option<f64>,
    ) -> ClipEventRecord {
        ClipEventRecord {
            event_dir_key: event_dir_key.to_owned(),
            bucket: Bucket::SavedClips,
            primary_canonical_key: "slot0:TeslaCam/SavedClips/clip".to_owned(),
            timestamp_utc: 1_700_000_000,
            timestamp_local_naive: 1_700_000_000,
            timestamp_has_offset: false,
            est_lat,
            est_lon,
            reason: Some("sentry".to_owned()),
            city: Some("Seattle".to_owned()),
            camera: Some("front".to_owned()),
        }
    }

    fn clip_event_batch(
        events: Vec<ClipEventRecord>,
        inventory: bool,
        complete: bool,
    ) -> ScanBatch {
        let mut b = batch(Vec::new(), complete);
        b.clip_events = events;
        b.clip_events_inventory = inventory;
        b
    }

    #[test]
    fn apply_upserts_and_prunes_media_when_inventoried() {
        let mut conn = open_in_memory().unwrap();
        let b = media_batch(vec![media_rec("LockChime.wav")], true, true);
        let report = apply(&mut conn, &b, DeriveConfig::default()).unwrap();
        assert_eq!(report.media_written, 1);
        assert_eq!(report.media_pruned, 0);
        let row = media_row(&conn, "LockChime.wav").unwrap();
        assert_eq!(row.0, "LockChime.wav");
        assert_eq!(row.1, 219_770);
        assert_eq!(row.2.as_deref(), Some("2026-06-01T20:10:04"));

        // A later complete inventory with the chime GONE prunes the row.
        let empty = media_batch(Vec::new(), true, true);
        let report2 = apply(&mut conn, &empty, DeriveConfig::default()).unwrap();
        assert_eq!(report2.media_pruned, 1);
        assert_eq!(count(&conn, "media_entries"), 0);
    }

    #[test]
    fn apply_idempotent_media_no_duplicate_rows() {
        let mut conn = open_in_memory().unwrap();
        let b = media_batch(vec![media_rec("LockChime.wav")], true, true);
        apply(&mut conn, &b, DeriveConfig::default()).unwrap();
        apply(&mut conn, &b, DeriveConfig::default()).unwrap();
        assert_eq!(count(&conn, "media_entries"), 1);
    }

    #[test]
    fn media_unaware_batch_never_touches_catalog() {
        let mut conn = open_in_memory().unwrap();
        // First, a media-aware pass installs a row.
        let installed = media_batch(vec![media_rec("LockChime.wav")], true, true);
        apply(&mut conn, &installed, DeriveConfig::default()).unwrap();
        assert_eq!(count(&conn, "media_entries"), 1);

        // Then a pass from an OLD (media-unaware) scannerd: empty media,
        // inventory=false, complete=true. It must NOT prune the row.
        let old = media_batch(Vec::new(), false, true);
        let report = apply(&mut conn, &old, DeriveConfig::default()).unwrap();
        assert_eq!(report.media_written, 0);
        assert_eq!(report.media_pruned, 0);
        assert_eq!(count(&conn, "media_entries"), 1);
    }

    #[test]
    fn incomplete_media_inventory_upserts_but_does_not_prune() {
        let mut conn = open_in_memory().unwrap();
        let installed = media_batch(vec![media_rec("LockChime.wav")], true, true);
        apply(&mut conn, &installed, DeriveConfig::default()).unwrap();

        // A torn (incomplete) pass that no longer sees the chime must keep
        // the row — prune is gated on `complete`.
        let torn = media_batch(Vec::new(), true, false);
        let report = apply(&mut conn, &torn, DeriveConfig::default()).unwrap();
        assert_eq!(report.media_pruned, 0);
        assert_eq!(count(&conn, "media_entries"), 1);
    }

    #[test]
    fn apply_persists_clip_events() {
        let mut conn = open_in_memory().unwrap();
        let event_key = "slot0:TeslaCam/SavedClips/2026-06-01_20-10-04";
        let batch = clip_event_batch(
            vec![clip_event_rec(event_key, Some(47.6), Some(-122.3))],
            true,
            true,
        );
        let report = apply(&mut conn, &batch, DeriveConfig::default()).unwrap();
        assert_eq!(report.clip_events_written, 1);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM clip_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let est_lat: Option<f64> = conn
            .query_row(
                "SELECT est_lat FROM clip_events WHERE event_dir_key = ?1",
                [event_key],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(est_lat, Some(47.6));
    }

    #[test]
    fn apply_prunes_vanished_clip_events() {
        let mut conn = open_in_memory().unwrap();
        let event_key = "slot0:TeslaCam/SavedClips/2026-06-01_20-10-04";
        let seed = clip_event_batch(vec![clip_event_rec(event_key, None, None)], true, true);
        apply(&mut conn, &seed, DeriveConfig::default()).unwrap();
        assert_eq!(count(&conn, "clip_events"), 1);

        let next = clip_event_batch(Vec::new(), true, true);
        let report = apply(&mut conn, &next, DeriveConfig::default()).unwrap();
        assert_eq!(report.clip_events_pruned, 1);
        assert_eq!(count(&conn, "clip_events"), 0);
    }

    #[test]
    fn apply_without_clip_events_inventory_preserves_rows() {
        let mut conn = open_in_memory().unwrap();
        let event_key = "slot0:TeslaCam/SavedClips/2026-06-01_20-10-04";
        let seed = clip_event_batch(vec![clip_event_rec(event_key, None, None)], true, true);
        apply(&mut conn, &seed, DeriveConfig::default()).unwrap();
        assert_eq!(count(&conn, "clip_events"), 1);

        let unaware = clip_event_batch(Vec::new(), false, true);
        let report = apply(&mut conn, &unaware, DeriveConfig::default()).unwrap();
        assert_eq!(report.clip_events_written, 0);
        assert_eq!(report.clip_events_pruned, 0);
        assert_eq!(count(&conn, "clip_events"), 1);
    }

    #[test]
    fn apply_is_idempotent_across_a_serde_round_trip() {
        let mut conn = open_in_memory().unwrap();
        let dir = "TeslaCam/SavedClips/2026-06-01_20-10-04";
        let key = "0:TeslaCam/SavedClips/2026-06-01_20-10-04/2026-06-01_20-10-04";
        let b = batch(vec![front_record(key, dir, 1_700_000_000)], true);

        // The cross-process path serializes the batch over the socket; a
        // serde round-trip before apply must not change the outcome.
        let json = serde_json::to_string(&b).unwrap();
        let decoded: ScanBatch = serde_json::from_str(&json).unwrap();
        assert_eq!(b, decoded);

        let first = apply(&mut conn, &decoded, DeriveConfig::default()).unwrap();
        let second = apply(&mut conn, &decoded, DeriveConfig::default()).unwrap();
        assert_eq!(first.clips_written, second.clips_written);
        assert_eq!(first.waypoints, second.waypoints);

        // Re-applying the same facts must not duplicate rows.
        assert_eq!(count(&conn, "clips"), 1);
        assert_eq!(count(&conn, "angles"), 1);
        assert_eq!(count(&conn, "clip_waypoints"), 2);
    }

    #[test]
    fn incomplete_batch_does_not_prune() {
        let mut conn = open_in_memory().unwrap();
        let dir = "TeslaCam/SavedClips/2026-06-01_20-10-04";
        let key_a = "0:TeslaCam/SavedClips/a/2026-06-01_20-10-04";
        let key_b = "0:TeslaCam/SavedClips/b/2026-06-01_20-11-04";

        // Seed two clips via a complete batch.
        let seed = batch(
            vec![
                front_record(key_a, dir, 1_700_000_000),
                front_record(key_b, dir, 1_700_000_100),
            ],
            true,
        );
        apply(&mut conn, &seed, DeriveConfig::default()).unwrap();
        assert_eq!(count(&conn, "clips"), 2);

        // An INCOMPLETE batch listing only key_a must NOT prune key_b.
        let partial = batch(vec![front_record(key_a, dir, 1_700_000_000)], false);
        let report = apply(&mut conn, &partial, DeriveConfig::default()).unwrap();
        assert_eq!(report.pruned, 0);
        assert_eq!(count(&conn, "clips"), 2);

        // A COMPLETE batch listing only key_a prunes the vanished key_b.
        let full = batch(vec![front_record(key_a, dir, 1_700_000_000)], true);
        let report = apply(&mut conn, &full, DeriveConfig::default()).unwrap();
        assert_eq!(report.pruned, 1);
        assert_eq!(count(&conn, "clips"), 1);
    }

    #[test]
    fn front_parse_attempts_prune_only_on_complete_batches() {
        let mut conn = open_in_memory().unwrap();
        let dir = "TeslaCam/SavedClips/2026-06-01_20-10-04";
        let key_a = "0:TeslaCam/SavedClips/a/2026-06-01_20-10-04";
        let key_b = "0:TeslaCam/SavedClips/b/2026-06-01_20-11-04";

        let seed = batch(
            vec![
                front_record(key_a, dir, 1_700_000_000),
                front_record(key_b, dir, 1_700_000_100),
            ],
            true,
        );
        apply(&mut conn, &seed, DeriveConfig::default()).unwrap();
        assert_eq!(count(&conn, "front_parse_attempts"), 2);

        let incomplete = batch(vec![front_record(key_a, dir, 1_700_000_000)], false);
        apply(&mut conn, &incomplete, DeriveConfig::default()).unwrap();
        assert_eq!(count(&conn, "front_parse_attempts"), 2);

        let complete = batch(vec![front_record(key_a, dir, 1_700_000_000)], true);
        apply(&mut conn, &complete, DeriveConfig::default()).unwrap();
        assert_eq!(count(&conn, "front_parse_attempts"), 1);
        assert_eq!(
            front_attempt_state(&conn, key_a).as_deref(),
            Some("parsed_with_waypoints")
        );
        assert_eq!(front_attempt_state(&conn, key_b), None);
    }

    #[test]
    fn malformed_record_is_skipped_not_fatal() {
        let mut conn = open_in_memory().unwrap();
        let dir = "TeslaCam/SavedClips/2026-06-01_20-10-04";
        let good = "0:TeslaCam/SavedClips/good/2026-06-01_20-10-04";
        let bad = "0:TeslaCam/SavedClips/bad/2026-06-01_20-11-04";

        // A non-front record carrying waypoints fails per-record validation.
        let mut bad_record = other_record(bad, dir, "back", 1_700_000_100);
        bad_record.waypoints = vec![wire_waypoint_from_walk(
            &waypoint(0, 0.0, 1.0, 2.0),
            1_700_000_100,
        )];

        let b = batch(
            vec![front_record(good, dir, 1_700_000_000), bad_record],
            true,
        );
        let report = apply(&mut conn, &b, DeriveConfig::default()).unwrap();
        assert_eq!(report.record_errors, 1);
        assert_eq!(report.clips_written, 1);
        // The good clip still landed.
        assert_eq!(count(&conn, "clips"), 1);
    }

    #[test]
    fn version_mismatch_is_fatal() {
        let mut conn = open_in_memory().unwrap();
        let dir = "TeslaCam/SavedClips/2026-06-01_20-10-04";
        let key = "0:TeslaCam/SavedClips/2026-06-01_20-10-04/2026-06-01_20-10-04";
        let mut b = batch(vec![front_record(key, dir, 1_700_000_000)], true);
        b.version = PROTOCOL_VERSION + 1;
        assert!(apply(&mut conn, &b, DeriveConfig::default()).is_err());
    }
}

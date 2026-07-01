//! v1 schema DDL and the forward-only migration ladder.
//!
//! The schema realizes contract **D1**
//! ([`docs/specs/contracts/indexd-schema.md`]) plus two internal additions
//! documented inline. It is **PROVISIONAL** until the operator OP-3 freeze;
//! [`SCHEMA_VERSION_NOTE`] records that.
//!
//! Migrations are **forward-only and idempotent**: each carries a unique
//! ascending [`Migration::version`]; [`apply`](super::apply_migrations)
//! runs every migration whose version exceeds the DB's current
//! `MAX(schema_version.version)` inside a single transaction. A DB whose
//! version exceeds [`LATEST_VERSION`] (written by a newer binary) is a hard
//! error — we never downgrade.

/// One forward-only migration step.
pub struct Migration {
    /// Monotonic version this migration brings the DB up to.
    pub version: i64,
    /// Human note recorded in `schema_version.note`.
    pub note: &'static str,
    /// DDL executed as a batch when this migration is applied.
    pub sql: &'static str,
}

/// Marker recorded in the v1 seed row so an inspector can see the schema
/// is not yet frozen.
pub const SCHEMA_VERSION_NOTE: &str = "v1 (PROVISIONAL — pre-OP-3 freeze)";

/// The highest schema version this binary knows how to produce. A DB
/// reporting a higher version was written by a newer `indexd` and must
/// not be opened read-write.
pub const LATEST_VERSION: i64 = 4;

/// The ordered migration ladder. Index order MUST match ascending
/// `version`; [`MIGRATIONS`] is validated by a test.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        note: SCHEMA_VERSION_NOTE,
        sql: V1_SQL,
    },
    Migration {
        version: 2,
        note: "v2 — media_entries (p2 read-only inventory)",
        sql: V2_SQL,
    },
    Migration {
        version: 3,
        note: "v3 — clip_events (event.json sidecar)",
        sql: V3_SQL,
    },
    Migration {
        version: 4,
        note: "v4 — front_parse_attempts (durable front parse outcomes)",
        sql: V4_SQL,
    },
];

/// v2 DDL: the MEDIA (p2) read-only inventory the media screens display.
/// Pure derived state owned by indexd from scannerd's media facts; webd
/// reads it but never writes it. The row identity is
/// `(partition, rel_path)` so the same file name on different partitions
/// (or a future per-folder layout) cannot collide.
const V2_SQL: &str = "
CREATE TABLE media_entries (
    id          INTEGER PRIMARY KEY,
    partition   TEXT    NOT NULL,
    rel_path    TEXT    NOT NULL,
    name        TEXT    NOT NULL,
    size_bytes  INTEGER NOT NULL,
    modified    TEXT,
    updated_at  INTEGER NOT NULL,
    UNIQUE (partition, rel_path)
);
";

/// v3 DDL: raw `event.json` metadata sidecar keyed by event directory.
/// This is NOT derived state: unlike trips/events (which are dropped and
/// rebuilt), these facts are scanner-sourced and survive derive rebuilds.
/// `indexd` writes the rows directly from `scannerd`'s clip-event facts and
/// reads them later at derive time.
const V3_SQL: &str = "
CREATE TABLE clip_events (
    event_dir_key         TEXT    PRIMARY KEY,
    bucket                TEXT    NOT NULL,
    primary_canonical_key TEXT    NOT NULL,
    timestamp_utc         INTEGER NOT NULL,
    timestamp_local_naive INTEGER NOT NULL,
    timestamp_has_offset  INTEGER NOT NULL,
    est_lat               REAL,
    est_lon               REAL,
    reason                TEXT,
    city                  TEXT,
    camera                TEXT,
    updated_at            INTEGER NOT NULL
);
";

/// v4 DDL: durable per-clip front parse outcomes. This records every
/// observed front parse result so failed/retried parses never silently
/// erase provenance. Backfill is conservative: historical clips with any
/// cached waypoints are marked `parsed_with_waypoints`; clips without
/// waypoints are `legacy_unknown` (never inferred as terminal empty).
const V4_SQL: &str = "
CREATE TABLE front_parse_attempts (
    canonical_key     TEXT PRIMARY KEY NOT NULL,
    parse_state       TEXT    NOT NULL,
    parse_fingerprint TEXT,
    parser_version    INTEGER,
    attempted_at      INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL
);

INSERT OR IGNORE INTO front_parse_attempts
    (canonical_key, parse_state, parse_fingerprint, parser_version, attempted_at, updated_at)
SELECT c.canonical_key,
       CASE
           WHEN EXISTS (SELECT 1 FROM clip_waypoints w WHERE w.clip_id = c.id)
           THEN 'parsed_with_waypoints'
           ELSE 'legacy_unknown'
       END,
       NULL,
       NULL,
       CAST(strftime('%s','now') AS INTEGER),
       CAST(strftime('%s','now') AS INTEGER)
  FROM clips c
  JOIN angles a ON a.clip_id = c.id
 WHERE lower(a.camera) = 'front';
";

/// v1 DDL: contract D1's proposed schema, plus two internal additions
/// flagged in the build notes:
///   * `trips.polyline` BLOB — the RDP-simplified cached polyline (OQ-2
///     resolves to BOTH durable `trip_points` rows AND a cached blob).
///   * `events.front_frame_index` — the v1 VCL frame index, kept alongside
///     D1's `front_frame_offset` (ms) so v1 parity is preserved without
///     losing D1's millisecond contract.
///   * `clip_waypoints` — a derived, rebuildable cache of the sampled SEI
///     telemetry so trips/events can be re-derived without re-walking the
///     media. Pure derived state (dropped/rebuilt with trips/events).
const V1_SQL: &str = "
-- schema versioning ---------------------------------------------------
CREATE TABLE schema_version (
    version     INTEGER NOT NULL,
    applied_at  INTEGER NOT NULL,
    note        TEXT
);

-- clips: a recording session (a group of camera angles) ---------------
CREATE TABLE clips (
    id             INTEGER PRIMARY KEY,
    canonical_key  TEXT    NOT NULL UNIQUE,
    started_at     INTEGER NOT NULL,
    ended_at       INTEGER,
    partition      TEXT    NOT NULL,
    folder_class   TEXT    NOT NULL,
    is_sentry      INTEGER NOT NULL DEFAULT 0,
    duration_s     REAL,
    availability   TEXT    NOT NULL DEFAULT 'present',
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);

-- angles: one camera file within a clip -------------------------------
CREATE TABLE angles (
    id          INTEGER PRIMARY KEY,
    clip_id     INTEGER NOT NULL REFERENCES clips(id) ON DELETE CASCADE,
    camera      TEXT    NOT NULL,
    file_ref    TEXT    NOT NULL,
    view_kind   TEXT    NOT NULL DEFAULT 'archive',
    offset_ms   INTEGER NOT NULL DEFAULT 0,
    duration_s  REAL,
    size_bytes  INTEGER,
    UNIQUE (clip_id, camera)
);

-- clip_waypoints: derived SEI telemetry cache (INTERNAL, rebuildable) -
-- Mirrors the v1 worker waypoints so trips/events can be re-derived
-- without re-walking the media. Front-camera only (derivation source).
CREATE TABLE clip_waypoints (
    clip_id        INTEGER NOT NULL REFERENCES clips(id) ON DELETE CASCADE,
    seq            INTEGER NOT NULL,
    frame_index    INTEGER NOT NULL,
    offset_ms      REAL    NOT NULL,
    t              INTEGER,
    lat            REAL    NOT NULL,
    lon            REAL    NOT NULL,
    speed          REAL,
    heading        REAL,
    accel_x        REAL,
    accel_y        REAL,
    accel_z        REAL,
    autopilot      TEXT,
    gear           TEXT,
    has_gps_fix    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (clip_id, seq)
);

-- trips: per-day driving segments -------------------------------------
CREATE TABLE trips (
    id           INTEGER PRIMARY KEY,
    day          TEXT    NOT NULL,
    started_at   INTEGER NOT NULL,
    ended_at     INTEGER NOT NULL,
    bbox_min_lat REAL, bbox_min_lon REAL,
    bbox_max_lat REAL, bbox_max_lon REAL,
    distance_m   REAL,
    point_count  INTEGER NOT NULL DEFAULT 0,
    polyline     BLOB,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
);
CREATE INDEX idx_trips_day        ON trips(day);
CREATE INDEX idx_trips_started_at ON trips(started_at);

-- trip_points: the GPS polyline (durable rows; OQ-2) ------------------
CREATE TABLE trip_points (
    trip_id  INTEGER NOT NULL REFERENCES trips(id) ON DELETE CASCADE,
    seq      INTEGER NOT NULL,
    t        INTEGER NOT NULL,
    lat      REAL    NOT NULL,
    lon      REAL    NOT NULL,
    speed    REAL,
    heading  REAL,
    PRIMARY KEY (trip_id, seq)
);

-- events: hard-brake / hard-accel / sharp-turn / autopilot / sentry ---
CREATE TABLE events (
    id                 INTEGER PRIMARY KEY,
    trip_id            INTEGER REFERENCES trips(id) ON DELETE SET NULL,
    clip_id            INTEGER REFERENCES clips(id) ON DELETE SET NULL,
    type               TEXT    NOT NULL,
    severity           INTEGER,
    t                  INTEGER NOT NULL,
    lat                REAL, lon REAL,
    front_frame_offset INTEGER,
    front_frame_index  INTEGER,
    description        TEXT,
    created_at         INTEGER NOT NULL
);
CREATE INDEX idx_events_trip ON events(trip_id);
CREATE INDEX idx_events_clip ON events(clip_id);
CREATE INDEX idx_events_t    ON events(t);

-- archive_items: the retention/value/durability/delete unit -----------
CREATE TABLE archive_items (
    id             INTEGER PRIMARY KEY,
    folder_class   TEXT    NOT NULL,
    path           TEXT    NOT NULL UNIQUE,
    clip_id        INTEGER REFERENCES clips(id) ON DELETE SET NULL,
    size_bytes     INTEGER NOT NULL DEFAULT 0,
    file_count     INTEGER NOT NULL DEFAULT 1,
    archived_at    INTEGER NOT NULL,
    delete_state   TEXT    NOT NULL DEFAULT 'LIVE'
                   CHECK (delete_state IN
                     ('LIVE','DELETE_CLAIMED','DELETING','DELETED',
                      'DELETE_FAILED','QUARANTINED')),
    delete_gen     TEXT,
    bytes_freed    INTEGER,
    durable        INTEGER NOT NULL DEFAULT 0,
    pinned         INTEGER NOT NULL DEFAULT 0,
    user_disposable INTEGER NOT NULL DEFAULT 0,
    has_event_json INTEGER NOT NULL DEFAULT 0,
    has_geo        INTEGER NOT NULL DEFAULT 0,
    event_severity INTEGER,
    sentry_flood   INTEGER NOT NULL DEFAULT 0,
    value_score    INTEGER,
    suppress_until INTEGER,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);
CREATE INDEX idx_archive_state    ON archive_items(delete_state);
CREATE INDEX idx_archive_class    ON archive_items(folder_class);
CREATE INDEX idx_archive_value    ON archive_items(delete_state, value_score);
CREATE INDEX idx_archive_suppress ON archive_items(suppress_until);
CREATE INDEX idx_archive_candidate
    ON archive_items(folder_class, durable, delete_state, pinned, value_score);

-- archive_item_clips: many-to-many ------------------------------------
CREATE TABLE archive_item_clips (
    archive_item_id INTEGER NOT NULL REFERENCES archive_items(id) ON DELETE CASCADE,
    clip_id         INTEGER NOT NULL REFERENCES clips(id)         ON DELETE CASCADE,
    PRIMARY KEY (archive_item_id, clip_id)
);
CREATE INDEX idx_aic_clip ON archive_item_clips(clip_id);

-- eviction_tombstones: anti-thrash record -----------------------------
CREATE TABLE eviction_tombstones (
    id             INTEGER PRIMARY KEY,
    source_path    TEXT    NOT NULL,
    folder_class   TEXT    NOT NULL,
    size_bytes     INTEGER,
    mtime          INTEGER,
    content_hash   TEXT,
    reason         TEXT    NOT NULL,
    delete_gen     TEXT    NOT NULL,
    durable_at_evict INTEGER NOT NULL DEFAULT 0,
    suppress_until INTEGER NOT NULL,
    created_at     INTEGER NOT NULL
);
CREATE INDEX idx_tombstone_path     ON eviction_tombstones(source_path);
CREATE INDEX idx_tombstone_suppress ON eviction_tombstones(suppress_until);

-- leases: shape owned here, protocol owned by D3 ----------------------
CREATE TABLE leases (
    id              INTEGER PRIMARY KEY,
    archive_item_id INTEGER NOT NULL REFERENCES archive_items(id) ON DELETE CASCADE,
    kind            TEXT    NOT NULL CHECK (kind IN ('upload','playback')),
    holder          TEXT    NOT NULL,
    gen             TEXT    NOT NULL,
    boot_id         TEXT    NOT NULL,
    acquired_wall   INTEGER,
    expires_mono_ms INTEGER NOT NULL,
    preempt_req     INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_leases_item   ON leases(archive_item_id);
CREATE INDEX idx_leases_expiry ON leases(boot_id, expires_mono_ms);

-- prefs/settings: UI + policy knobs (JSON values) ---------------------
CREATE TABLE prefs (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use rusqlite::{Connection, params};

    use super::{LATEST_VERSION, MIGRATIONS, V1_SQL, V2_SQL, V3_SQL, V4_SQL};

    #[test]
    fn ladder_is_monotonic_and_matches_latest() {
        let mut prev = 0_i64;
        for migration in MIGRATIONS {
            assert!(
                migration.version > prev,
                "migration versions must strictly ascend"
            );
            prev = migration.version;
        }
        assert_eq!(
            prev, LATEST_VERSION,
            "LATEST_VERSION must equal the last migration version"
        );
    }

    #[test]
    fn v4_backfills_front_parse_attempts_conservatively() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(V1_SQL).unwrap();
        conn.execute_batch(V2_SQL).unwrap();
        conn.execute_batch(V3_SQL).unwrap();
        for version in 1_i64..=3 {
            conn.execute(
                "INSERT INTO schema_version(version, applied_at, note) VALUES(?1, 0, 'seed')",
                params![version],
            )
            .unwrap();
        }

        let now = 1_700_000_000_i64;
        conn.execute(
            "INSERT INTO clips
                (id, canonical_key, started_at, partition, folder_class, is_sentry, availability, created_at, updated_at)
             VALUES
                (1, 'k-has-waypoints', 1000, 'slot0', 'SavedClips', 0, 'present', ?1, ?1),
                (2, 'k-no-waypoints', 2000, 'slot0', 'SavedClips', 0, 'present', ?1, ?1)",
            params![now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO angles(clip_id, camera, file_ref, view_kind, offset_ms)
             VALUES
                (1, 'front', 'a-front.mp4', 'ro_usb', 0),
                (2, 'front', 'b-front.mp4', 'ro_usb', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO clip_waypoints(clip_id, seq, frame_index, offset_ms, lat, lon)
             VALUES(1, 0, 0, 0.0, 1.0, 2.0)",
            [],
        )
        .unwrap();

        let tx = conn.transaction().unwrap();
        tx.execute_batch(V4_SQL).unwrap();
        tx.execute(
            "INSERT INTO schema_version(version, applied_at, note) VALUES(4, 0, 'v4')",
            [],
        )
        .unwrap();
        tx.commit().unwrap();

        let has: String = conn
            .query_row(
                "SELECT parse_state FROM front_parse_attempts WHERE canonical_key = 'k-has-waypoints'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let none: String = conn
            .query_row(
                "SELECT parse_state FROM front_parse_attempts WHERE canonical_key = 'k-no-waypoints'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has, "parsed_with_waypoints");
        assert_eq!(none, "legacy_unknown");
    }
}

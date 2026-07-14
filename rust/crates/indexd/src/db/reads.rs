//! indexd read-side queries for eviction and crash-recovery workflows.

use rusqlite::{Connection, params};

use crate::db::DbError;

/// Hard cap for server-exposed list queries to bound frame size.
const MAX_LIST_ROWS: u32 = 512;

/// One safe eviction candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvictionCandidate {
    /// Archive item id.
    pub id: i64,
    /// Archive-root-relative path.
    pub path: String,
    /// Archive bytes.
    pub size_bytes: i64,
    /// Archive completion epoch seconds.
    pub archived_at: i64,
    /// Source folder class.
    pub folder_class: String,
}

/// One row requiring delete-state recovery handling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryRow {
    /// Archive item id.
    pub id: i64,
    /// Current delete state.
    pub delete_state: String,
    /// Archive-root-relative path.
    pub path: String,
    /// Archive bytes.
    pub size_bytes: i64,
    /// Delete generation token (if present).
    pub delete_gen: Option<String>,
}

/// List oldest-first deletion candidates from the strict hard-delete allowlist.
///
/// # Errors
///
/// Returns [`DbError`] if the query fails.
pub fn list_eviction_candidates(
    conn: &Connection,
    recency_floor_epoch: i64,
    now_epoch: i64,
    allow_undurable: bool,
    limit: u32,
) -> Result<Vec<EvictionCandidate>, DbError> {
    let capped = i64::from(limit.min(MAX_LIST_ROWS));
    // Recency is gated on clips.started_at (the true recording instant from the
    // Tesla filename/mvhd, epoch-seconds) — NOT archive_items.archived_at, whose
    // Pi wall-clock value is unreliable on a clock-less device. recency_floor_epoch
    // is likewise epoch-seconds. An item is a candidate only if EVERY linked clip is
    // RecentClips, non-Sentry, has a known (>0) start, and the NEWEST is older than
    // the floor; anything else fails closed (INNER JOIN + all-clip HAVING guards).
    let mut stmt = conn.prepare(
        "SELECT ai.id, ai.path, ai.size_bytes, ai.archived_at, ai.folder_class
           FROM archive_items AS ai
           JOIN archive_item_clips AS aic ON aic.archive_item_id = ai.id
           JOIN clips AS c ON c.id = aic.clip_id
          WHERE ai.delete_state = 'LIVE'
            AND (ai.durable = 1 OR ?3 = 1)
            AND ai.pinned = 0
            AND ai.folder_class = 'RecentClips'
            AND (ai.suppress_until IS NULL OR ai.suppress_until < ?2)
          GROUP BY ai.id, ai.path, ai.size_bytes, ai.archived_at, ai.folder_class
         HAVING MIN(CASE WHEN c.folder_class = 'RecentClips' THEN 1 ELSE 0 END) = 1
            AND MAX(c.is_sentry) = 0
            AND MIN(CASE WHEN c.started_at > 0 THEN 1 ELSE 0 END) = 1
            AND MAX(c.started_at) < ?1
          ORDER BY MIN(c.started_at) ASC, ai.id ASC
          LIMIT ?4",
    )?;
    let rows = stmt.query_map(
        params![
          recency_floor_epoch,
          now_epoch,
          i64::from(allow_undurable),
          capped
        ],
        |row| {
          Ok(EvictionCandidate {
              id: row.get(0)?,
              path: row.get(1)?,
              size_bytes: row.get(2)?,
              archived_at: row.get(3)?,
              folder_class: row.get(4)?,
          })
        },
    )?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// List rows that are in transitional delete states and require recovery.
///
/// # Errors
///
/// Returns [`DbError`] if the query fails.
pub fn list_recovery_rows(conn: &Connection) -> Result<Vec<RecoveryRow>, DbError> {
    let mut stmt = conn.prepare(
        "SELECT id, delete_state, path, size_bytes, delete_gen
           FROM archive_items
          WHERE delete_state NOT IN ('LIVE','DELETED')
          ORDER BY id ASC
          LIMIT 512",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(RecoveryRow {
            id: row.get(0)?,
            delete_state: row.get(1)?,
            path: row.get(2)?,
            size_bytes: row.get(3)?,
            delete_gen: row.get(4)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use rusqlite::{Connection, params};

    use super::{list_eviction_candidates, list_recovery_rows};
    use crate::db::open_in_memory;

    #[derive(Debug, Clone)]
    struct ArchiveSeed<'a> {
        folder_class: &'a str,
        path: &'a str,
        size_bytes: i64,
        archived_at: i64,
        delete_state: &'a str,
        durable: i64,
        pinned: i64,
        suppress_until: Option<i64>,
        delete_gen: Option<&'a str>,
    }

    fn insert_archive_item_unlinked(conn: &Connection, seed: &ArchiveSeed<'_>) -> i64 {
        conn.execute(
            "INSERT INTO archive_items
                (folder_class, path, size_bytes, file_count, archived_at, delete_state,
                 durable, pinned, suppress_until, delete_gen, created_at, updated_at)
             VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7, ?8, ?9, 0, 0)",
            params![
                seed.folder_class,
                seed.path,
                seed.size_bytes,
                seed.archived_at,
                seed.delete_state,
                seed.durable,
                seed.pinned,
                seed.suppress_until,
                seed.delete_gen
            ],
        )
        .expect("insert archive item");
        conn.last_insert_rowid()
    }

    fn insert_linked_clip(
        conn: &Connection,
        archive_item_id: i64,
        canonical_key: &str,
        started_at: i64,
        folder_class: &str,
    ) {
        conn.execute(
            "INSERT INTO clips (canonical_key, started_at, partition, folder_class, created_at, updated_at)
             VALUES (?1, ?2, 'p', ?3, 0, 0)",
            params![canonical_key, started_at, folder_class],
        )
        .expect("insert clip");
        let clip_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO archive_item_clips (archive_item_id, clip_id) VALUES (?1, ?2)",
            params![archive_item_id, clip_id],
        )
        .expect("insert archive-item clip link");
    }

    fn insert_archive_item(conn: &Connection, seed: &ArchiveSeed<'_>) -> i64 {
        let archive_item_id = insert_archive_item_unlinked(conn, seed);
        insert_linked_clip(
            conn,
            archive_item_id,
            &format!("clip:{}", seed.path),
            seed.archived_at,
            seed.folder_class,
        );
        archive_item_id
    }

    fn seed_eviction_candidate_mix(conn: &Connection) -> (i64, i64) {
        let old_a = insert_archive_item(
            conn,
            &ArchiveSeed {
                folder_class: "RecentClips",
                path: "archive/eligible-old-a",
                size_bytes: 1_000,
                archived_at: 100,
                delete_state: "LIVE",
                durable: 1,
                pinned: 0,
                suppress_until: None,
                delete_gen: None,
            },
        );
        let old_b = insert_archive_item(
            conn,
            &ArchiveSeed {
                folder_class: "RecentClips",
                path: "archive/eligible-old-b",
                size_bytes: 2_000,
                archived_at: 200,
                delete_state: "LIVE",
                durable: 1,
                pinned: 0,
                suppress_until: None,
                delete_gen: None,
            },
        );
        for seed in [
            ArchiveSeed {
                folder_class: "RecentClips",
                path: "archive/excluded-pinned",
                size_bytes: 3_000,
                archived_at: 50,
                delete_state: "LIVE",
                durable: 1,
                pinned: 1,
                suppress_until: None,
                delete_gen: None,
            },
            ArchiveSeed {
                folder_class: "RecentClips",
                path: "archive/excluded-nondurable",
                size_bytes: 3_000,
                archived_at: 60,
                delete_state: "LIVE",
                durable: 0,
                pinned: 0,
                suppress_until: None,
                delete_gen: None,
            },
            ArchiveSeed {
                folder_class: "SentryClips",
                path: "archive/excluded-sentry",
                size_bytes: 3_000,
                archived_at: 70,
                delete_state: "LIVE",
                durable: 1,
                pinned: 0,
                suppress_until: None,
                delete_gen: None,
            },
            ArchiveSeed {
                folder_class: "RecentClips",
                path: "archive/excluded-suppressed",
                size_bytes: 3_000,
                archived_at: 80,
                delete_state: "LIVE",
                durable: 1,
                pinned: 0,
                suppress_until: Some(10_000),
                delete_gen: None,
            },
            ArchiveSeed {
                folder_class: "RecentClips",
                path: "archive/excluded-too-recent",
                size_bytes: 3_000,
                archived_at: 9_500,
                delete_state: "LIVE",
                durable: 1,
                pinned: 0,
                suppress_until: None,
                delete_gen: None,
            },
            ArchiveSeed {
                folder_class: "RecentClips",
                path: "archive/excluded-not-live",
                size_bytes: 3_000,
                archived_at: 90,
                delete_state: "DELETE_CLAIMED",
                durable: 1,
                pinned: 0,
                suppress_until: None,
                delete_gen: Some("deadbeef"),
            },
        ] {
            insert_archive_item(conn, &seed);
        }
        (old_a, old_b)
    }

    #[test]
    fn list_eviction_candidates_returns_only_safe_oldest_recent_durable_live_rows() {
        let conn = open_in_memory().expect("open db");
        let (old_a, old_b) = seed_eviction_candidate_mix(&conn);

        let all =
            list_eviction_candidates(&conn, 1_000, 1_000, false, 100).expect("query candidates");
        let ids: Vec<i64> = all.iter().map(|row| row.id).collect();
        assert_eq!(ids, vec![old_a, old_b]);
        assert_eq!(
            all.first().map(|row| row.folder_class.as_str()),
            Some("RecentClips")
        );

        let limited =
            list_eviction_candidates(&conn, 1_000, 1_000, false, 1).expect("limited query");
        assert_eq!(limited.first().map(|row| row.id), Some(old_a));
    }

    #[test]
    fn list_eviction_candidates_opt_in_includes_undurable_but_still_filters() {
        let conn = open_in_memory().expect("open db");
        let (old_a, old_b) = seed_eviction_candidate_mix(&conn);
        let rows = list_eviction_candidates(&conn, 1_000, 1_000, true, 100).expect("query");
        let paths: Vec<&str> = rows.iter().map(|r| r.path.as_str()).collect();
        assert!(paths.contains(&"archive/excluded-nondurable"));
        assert!(!paths.iter().any(|p| p.contains("pinned")));
        assert!(!paths.iter().any(|p| p.contains("sentry")));
        assert!(!paths.iter().any(|p| p.contains("suppressed")));
        assert!(!paths.iter().any(|p| p.contains("too-recent")));
        assert!(!paths.iter().any(|p| p.contains("not-live")));
        assert!(rows.iter().any(|r| r.id == old_a));
        assert!(rows.iter().any(|r| r.id == old_b));
    }

    #[test]
    fn list_eviction_candidates_excludes_item_with_no_linked_clip() {
        let conn = open_in_memory().expect("open db");
        insert_archive_item_unlinked(
            &conn,
            &ArchiveSeed {
                folder_class: "RecentClips",
                path: "archive/unlinked-old",
                size_bytes: 1_000,
                archived_at: 100,
                delete_state: "LIVE",
                durable: 1,
                pinned: 0,
                suppress_until: None,
                delete_gen: None,
            },
        );

        let rows = list_eviction_candidates(&conn, 1_000, 1_000, false, 100).expect("query");
        assert!(rows.is_empty());
    }

    #[test]
    fn list_eviction_candidates_excludes_item_with_zero_started_at() {
        let conn = open_in_memory().expect("open db");
        let item = insert_archive_item_unlinked(
            &conn,
            &ArchiveSeed {
                folder_class: "RecentClips",
                path: "archive/zero-started-at",
                size_bytes: 1_000,
                archived_at: 100,
                delete_state: "LIVE",
                durable: 1,
                pinned: 0,
                suppress_until: None,
                delete_gen: None,
            },
        );
        insert_linked_clip(&conn, item, "clip:zero-started-at", 0, "RecentClips");

        let rows = list_eviction_candidates(&conn, 1_000, 1_000, false, 100).expect("query");
        assert!(rows.is_empty());
    }

    #[test]
    fn list_eviction_candidates_excludes_when_linked_clip_not_recentclips() {
        let conn = open_in_memory().expect("open db");
        let item = insert_archive_item_unlinked(
            &conn,
            &ArchiveSeed {
                folder_class: "RecentClips",
                path: "archive/mismatched-folder-class",
                size_bytes: 1_000,
                archived_at: 100,
                delete_state: "LIVE",
                durable: 1,
                pinned: 0,
                suppress_until: None,
                delete_gen: None,
            },
        );
        insert_linked_clip(&conn, item, "clip:mismatched-folder-class", 100, "SentryClips");

        let rows = list_eviction_candidates(&conn, 1_000, 1_000, false, 100).expect("query");
        assert!(rows.is_empty());
    }

    #[test]
    fn list_eviction_candidates_excludes_stale_archived_at_but_fresh_started_at() {
        let conn = open_in_memory().expect("open db");
        let item = insert_archive_item_unlinked(
            &conn,
            &ArchiveSeed {
                folder_class: "RecentClips",
                path: "archive/stale-archived-fresh-started",
                size_bytes: 1_000,
                archived_at: 100,
                delete_state: "LIVE",
                durable: 1,
                pinned: 0,
                suppress_until: None,
                delete_gen: None,
            },
        );
        insert_linked_clip(
            &conn,
            item,
            "clip:stale-archived-fresh-started",
            9_500,
            "RecentClips",
        );

        let rows = list_eviction_candidates(&conn, 1_000, 1_000, false, 100).expect("query");
        assert!(rows.is_empty());
    }

    #[test]
    fn list_eviction_candidates_deletes_when_started_at_old_even_if_archived_at_fresh() {
        let conn = open_in_memory().expect("open db");
        let item = insert_archive_item_unlinked(
            &conn,
            &ArchiveSeed {
                folder_class: "RecentClips",
                path: "archive/fresh-archived-old-started",
                size_bytes: 1_000,
                archived_at: 9_500,
                delete_state: "LIVE",
                durable: 1,
                pinned: 0,
                suppress_until: None,
                delete_gen: None,
            },
        );
        insert_linked_clip(
            &conn,
            item,
            "clip:fresh-archived-old-started",
            100,
            "RecentClips",
        );

        let rows = list_eviction_candidates(&conn, 1_000, 1_000, false, 100).expect("query");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, item);
    }

    fn insert_linked_recent_clip_sentry_flagged(
        conn: &Connection,
        archive_item_id: i64,
        canonical_key: &str,
        started_at: i64,
    ) {
        conn.execute(
            "INSERT INTO clips
                (canonical_key, started_at, partition, folder_class, is_sentry, created_at, updated_at)
             VALUES (?1, ?2, 'p', 'RecentClips', 1, 0, 0)",
            params![canonical_key, started_at],
        )
        .expect("insert sentry-flagged clip");
        let clip_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO archive_item_clips (archive_item_id, clip_id) VALUES (?1, ?2)",
            params![archive_item_id, clip_id],
        )
        .expect("insert archive-item clip link");
    }

    #[test]
    fn list_eviction_candidates_excludes_multiclip_item_when_any_clip_is_fresh() {
        let conn = open_in_memory().expect("open db");
        let item = insert_archive_item_unlinked(
            &conn,
            &ArchiveSeed {
                folder_class: "RecentClips",
                path: "archive/multiclip-one-fresh",
                size_bytes: 1_000,
                archived_at: 100,
                delete_state: "LIVE",
                durable: 1,
                pinned: 0,
                suppress_until: None,
                delete_gen: None,
            },
        );
        // One old segment and one fresh segment on the same item: MAX(started_at)
        // must protect it (a MIN would wrongly delete footage recorded moments ago).
        insert_linked_clip(&conn, item, "clip:multiclip-old", 100, "RecentClips");
        insert_linked_clip(&conn, item, "clip:multiclip-fresh", 9_500, "RecentClips");

        let rows = list_eviction_candidates(&conn, 1_000, 1_000, false, 100).expect("query");
        assert!(rows.is_empty());
    }

    #[test]
    fn list_eviction_candidates_excludes_multiclip_item_when_any_clip_has_zero_started_at() {
        let conn = open_in_memory().expect("open db");
        let item = insert_archive_item_unlinked(
            &conn,
            &ArchiveSeed {
                folder_class: "RecentClips",
                path: "archive/multiclip-one-zero",
                size_bytes: 1_000,
                archived_at: 100,
                delete_state: "LIVE",
                durable: 1,
                pinned: 0,
                suppress_until: None,
                delete_gen: None,
            },
        );
        insert_linked_clip(&conn, item, "clip:multiclip-known", 100, "RecentClips");
        insert_linked_clip(&conn, item, "clip:multiclip-unknown", 0, "RecentClips");

        let rows = list_eviction_candidates(&conn, 1_000, 1_000, false, 100).expect("query");
        assert!(rows.is_empty());
    }

    #[test]
    fn list_eviction_candidates_excludes_item_with_sentry_flagged_recent_clip() {
        let conn = open_in_memory().expect("open db");
        let item = insert_archive_item_unlinked(
            &conn,
            &ArchiveSeed {
                folder_class: "RecentClips",
                path: "archive/sentry-flagged-recent",
                size_bytes: 1_000,
                archived_at: 100,
                delete_state: "LIVE",
                durable: 1,
                pinned: 0,
                suppress_until: None,
                delete_gen: None,
            },
        );
        // Defense-in-depth: a clip whose folder_class is RecentClips but is_sentry=1
        // (a would-be ingest inconsistency) must never be selected.
        insert_linked_recent_clip_sentry_flagged(&conn, item, "clip:sentry-flagged", 100);

        let rows = list_eviction_candidates(&conn, 1_000, 1_000, false, 100).expect("query");
        assert!(rows.is_empty());
    }

    #[test]
    fn list_recovery_rows_excludes_live_and_deleted() {
        let conn = open_in_memory().expect("open db");
        insert_archive_item(
            &conn,
            &ArchiveSeed {
                folder_class: "RecentClips",
                path: "archive/live",
                size_bytes: 1,
                archived_at: 1,
                delete_state: "LIVE",
                durable: 1,
                pinned: 0,
                suppress_until: None,
                delete_gen: None,
            },
        );
        insert_archive_item(
            &conn,
            &ArchiveSeed {
                folder_class: "RecentClips",
                path: "archive/deleted",
                size_bytes: 2,
                archived_at: 2,
                delete_state: "DELETED",
                durable: 1,
                pinned: 0,
                suppress_until: None,
                delete_gen: Some("done"),
            },
        );
        let claimed = insert_archive_item(
            &conn,
            &ArchiveSeed {
                folder_class: "RecentClips",
                path: "archive/claimed",
                size_bytes: 3,
                archived_at: 3,
                delete_state: "DELETE_CLAIMED",
                durable: 1,
                pinned: 0,
                suppress_until: None,
                delete_gen: Some("g1"),
            },
        );
        let deleting = insert_archive_item(
            &conn,
            &ArchiveSeed {
                folder_class: "RecentClips",
                path: "archive/deleting",
                size_bytes: 4,
                archived_at: 4,
                delete_state: "DELETING",
                durable: 1,
                pinned: 0,
                suppress_until: None,
                delete_gen: Some("g2"),
            },
        );
        let failed = insert_archive_item(
            &conn,
            &ArchiveSeed {
                folder_class: "RecentClips",
                path: "archive/failed",
                size_bytes: 5,
                archived_at: 5,
                delete_state: "DELETE_FAILED",
                durable: 1,
                pinned: 0,
                suppress_until: None,
                delete_gen: Some("g3"),
            },
        );
        let quarantined = insert_archive_item(
            &conn,
            &ArchiveSeed {
                folder_class: "RecentClips",
                path: "archive/quarantined",
                size_bytes: 6,
                archived_at: 6,
                delete_state: "QUARANTINED",
                durable: 1,
                pinned: 0,
                suppress_until: None,
                delete_gen: None,
            },
        );

        let rows = list_recovery_rows(&conn).expect("query recovery rows");
        let ids: Vec<i64> = rows.iter().map(|row| row.id).collect();
        assert_eq!(ids, vec![claimed, deleting, failed, quarantined]);
        assert_eq!(
            rows.first().map(|row| row.delete_state.as_str()),
            Some("DELETE_CLAIMED")
        );
        assert_eq!(
            rows.first().and_then(|row| row.delete_gen.as_deref()),
            Some("g1")
        );
    }
}

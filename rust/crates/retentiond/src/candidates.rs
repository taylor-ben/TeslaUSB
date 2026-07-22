//! Archive-candidate contract types shared across `retentiond`.
//!
//! Defines the [`Candidate`] / [`CandidateAngle`] records and the
//! [`CandidateSource`] trait consumed by the archive driver. The production
//! implementation is `VolumeCandidateSource` (see `crate::volume_source`),
//! which reads the car-visible `teslacam.img` volume directly (ADR-0005);
//! this module carries no catalog or `SQLite` dependency.

use std::io;

/// One camera angle eligible for archive copy via the `ReadFile` seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateAngle {
    /// Camera token (`front`, `back`, ...).
    pub camera: String,
    /// Volume-root-relative source path (`angles.file_ref`).
    pub file_ref: String,
    /// Milliseconds from clip start.
    pub offset_ms: i64,
    /// Angle duration in seconds when known.
    pub duration_s: Option<i64>,
    /// Source size in bytes for this angle.
    pub size_bytes: u64,
}

/// One `RecentClips` clip selected for archiving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// `clips.id`.
    pub clip_id: i64,
    /// Canonical clip key.
    pub canonical_key: String,
    /// Partition label (`slot0`, ...).
    pub partition: String,
    /// Clip start epoch seconds.
    pub started_at: i64,
    /// Clip end epoch seconds.
    pub ended_at: i64,
    /// Clip duration in seconds when known.
    pub duration_s: Option<i64>,
    /// Source exFAT volume serial from the boot sector.
    pub source_volume_serial: u32,
    /// Stable source fingerprint for archive-local dedup marker matching.
    pub source_fingerprint: String,
    /// Live `ro_usb` angles to copy.
    pub angles: Vec<CandidateAngle>,
}

/// Candidate inventory seam consumed by `archive_recent_once`.
pub trait CandidateSource {
    /// List clips that should be archived in this cycle.
    ///
    /// # Errors
    ///
    /// Returns an error when the source inventory cannot be queried.
    fn list_candidates(&self) -> io::Result<Vec<Candidate>>;
}

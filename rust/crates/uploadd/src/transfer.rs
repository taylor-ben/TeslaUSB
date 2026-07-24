//! The **transfer backend seam** ([`Uploader`]) and integrity verification.
//!
//! A transfer is resumable and integrity-verified ([`uploadd.md`] §2.2, §3.3):
//! the engine pushes the source in chunks (paced under the `WiFi` cap), then
//! asks the backend to [`Uploader::finalize`] and checks the result against the
//! queue row's [`VerifySpec`] with [`verify_digest`]. A mismatch (or wrong
//! algorithm) is detected and retried — never flagged durable.
//!
//! # Backend choice — `rclone` vs. a small Rust uploader (OPEN / ASK-FIRST)
//!
//! `uploadd.md` §2.2 leaves the backend a **"choose at build"** decision, and
//! [`wifi-upload-throttle.md`] OQ-4 confirms the self-pacing implementation
//! (rclone `--bwlimit` vs. a Rust token bucket) is the builder's call. This lane
//! deliberately does **not** pick one — the decision is abstracted behind
//! [`Uploader`] and reported to the supervisor as an ASK-FIRST item. The two
//! options, for the record:
//!
//! * **`rclone`** — broadest provider coverage (S3, B2, Drive, `WebDAV`, …),
//!   battle-tested resumable transfers and `--bwlimit`, and it matches the Python
//!   reference (`cloud_rclone_service.py`). Cost: a large external binary on a
//!   RAM-constrained Pi Zero 2 W, and shelling out / parsing its output.
//! * **a small Rust uploader** — minimal footprint (no external process), tight
//!   control over chunking and the token-bucket pace, and a single static
//!   binary. Cost: we must implement (and maintain) per-provider auth and
//!   multipart/resumable semantics ourselves, narrowing provider breadth.
//!
//! Recommendation carried to the supervisor: **start with `rclone`** for
//! provider breadth and parity with the reference, keeping [`Uploader`] as the
//! seam so a Rust uploader can replace it later without touching the core. No
//! choice is hardcoded in this crate.
//!
//! [`uploadd.md`]: ../../../../docs/specs/uploadd.md
//! [`wifi-upload-throttle.md`]: ../../../../docs/specs/contracts/wifi-upload-throttle.md

use std::fmt::{Display, Formatter};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::TransferError;
use crate::source::ContentHash;

/// Which concrete transfer backend the live binary wires up. **No `Default`** —
/// the choice is an explicit build/ops decision (ASK-FIRST), never silently
/// defaulted by the core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferBackend {
    /// Shell out to `rclone` (broad provider support; larger footprint).
    Rclone,
    /// A small in-process Rust uploader (minimal footprint; narrower providers).
    RustUploader,
}

/// The resumable, chunked transfer backend. The live impl is `rclone` or a Rust
/// uploader (see module docs); tests inject a deterministic mock that can fail
/// mid-transfer or return a wrong digest.
///
/// There is **no** delete/remove method — `uploadd` never removes the Pi-side
/// source, and remote cleanup is a separate retention concern, not part of the
/// upload transfer path.
pub trait Uploader {
    /// Push `data` to the remote object `remote_key` at byte `offset`. Idempotent
    /// at the offset level: re-sending the same offset after a resume overwrites
    /// the same range, so a retry cannot corrupt or duplicate content.
    ///
    /// # Errors
    /// Returns [`TransferError::Chunk`] (carrying `offset`) on a transmit
    /// failure, so the queue can resume from the last good checkpoint.
    fn put_chunk(&self, remote_key: &str, offset: u64, data: &[u8]) -> Result<(), TransferError>;

    /// Finalize the remote object and return its **remote-computed** content
    /// digest over `total_bytes`, for integrity verification.
    ///
    /// # Errors
    /// Returns [`TransferError::Finalize`] if the object cannot be finalized.
    fn finalize(&self, remote_key: &str, total_bytes: u64) -> Result<ContentHash, TransferError>;
}

/// Supported native remote-hash algorithms from `cloud_upload_queue.verify_alg`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyAlg {
    /// SHA-256.
    Sha256,
    /// MD5.
    Md5,
    /// CRC32C.
    Crc32c,
    /// SHA-1.
    Sha1,
    /// Microsoft Graph `QuickXorHash`.
    Quickxor,
    /// Dropbox content hash.
    Dropbox,
}

impl VerifyAlg {
    /// Canonical wire/string form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
            Self::Md5 => "md5",
            Self::Crc32c => "crc32c",
            Self::Sha1 => "sha1",
            Self::Quickxor => "quickxor",
            Self::Dropbox => "dropbox",
        }
    }
}

impl Display for VerifyAlg {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Parse error for [`VerifyAlg`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseVerifyAlgError;

impl Display for ParseVerifyAlgError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("unknown verify algorithm")
    }
}

impl std::error::Error for ParseVerifyAlgError {}

impl FromStr for VerifyAlg {
    type Err = ParseVerifyAlgError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "sha256" => Ok(Self::Sha256),
            "md5" => Ok(Self::Md5),
            "crc32c" => Ok(Self::Crc32c),
            "sha1" => Ok(Self::Sha1),
            "quickxor" => Ok(Self::Quickxor),
            "dropbox" => Ok(Self::Dropbox),
            _ => Err(ParseVerifyAlgError),
        }
    }
}

/// Per-row verification contract from `cloud_upload_queue`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VerifySpec {
    /// Verify with a backend-native hash algorithm and expected value.
    Native {
        /// Requested hash algorithm.
        alg: VerifyAlg,
        /// Canonical local hash value text (`rclone hashsum <alg>` format).
        expected: String,
    },
    /// `verify_alg = "none"`: verify by confirmed copy size only.
    CopyIntegrity,
}

/// Remote verification evidence produced after transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteVerify {
    /// Native hash + value from the remote.
    Native {
        /// Algorithm the remote reported.
        alg: VerifyAlg,
        /// Hash value text from the remote.
        value: String,
    },
    /// Confirmed remote object size for copy-integrity backends.
    CopyIntegrity {
        /// Confirmed remote object size in bytes.
        size_bytes: u64,
    },
}

impl RemoteVerify {
    /// Wrap an SHA-256 digest as native verification evidence.
    #[must_use]
    pub fn sha256(hash: ContentHash) -> Self {
        Self::Native {
            alg: VerifyAlg::Sha256,
            value: content_hash_hex(hash),
        }
    }
}

fn content_hash_hex(hash: ContentHash) -> String {
    let mut out = String::with_capacity(64);
    for b in hash.0 {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Outcome of the integrity check after a transfer finalizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Integrity {
    /// Remote verification matched the expected spec — upload is trustworthy.
    Verified,
    /// Remote verification did **not** match — corrupt/partial transfer. The
    /// engine resets the resume checkpoint and retries; it is **never** flagged
    /// durable.
    Corrupt,
}

/// Compare remote verification evidence to the expected row contract.
#[must_use]
pub fn verify_digest(expected: &VerifySpec, remote: &RemoteVerify, total_bytes: u64) -> Integrity {
    match (expected, remote) {
        (
            VerifySpec::Native {
                alg: expected_alg,
                expected: expected_value,
            },
            RemoteVerify::Native {
                alg: remote_alg,
                value: remote_value,
            },
        ) if expected_value.len() <= 256
            && remote_value.len() <= 256
            && expected_alg == remote_alg
            && expected_value == remote_value =>
        {
            Integrity::Verified
        }
        (VerifySpec::CopyIntegrity, RemoteVerify::CopyIntegrity { size_bytes })
            if *size_bytes == total_bytes =>
        {
            Integrity::Verified
        }
        _ => Integrity::Corrupt,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::{Integrity, RemoteVerify, VerifyAlg, VerifySpec, verify_digest};

    #[test]
    fn native_match_is_verified() {
        let spec = VerifySpec::Native {
            alg: VerifyAlg::Sha256,
            expected: "abc123".to_owned(),
        };
        let remote = RemoteVerify::Native {
            alg: VerifyAlg::Sha256,
            value: "abc123".to_owned(),
        };
        assert_eq!(verify_digest(&spec, &remote, 10), Integrity::Verified);
    }

    #[test]
    fn native_mismatch_is_corrupt() {
        let spec = VerifySpec::Native {
            alg: VerifyAlg::Sha256,
            expected: "abc123".to_owned(),
        };
        let remote = RemoteVerify::Native {
            alg: VerifyAlg::Sha256,
            value: "ffff".to_owned(),
        };
        assert_eq!(verify_digest(&spec, &remote, 10), Integrity::Corrupt);
    }

    #[test]
    fn native_wrong_alg_is_corrupt() {
        let spec = VerifySpec::Native {
            alg: VerifyAlg::Sha256,
            expected: "abc123".to_owned(),
        };
        let remote = RemoteVerify::Native {
            alg: VerifyAlg::Md5,
            value: "abc123".to_owned(),
        };
        assert_eq!(verify_digest(&spec, &remote, 10), Integrity::Corrupt);
    }

    #[test]
    fn copy_integrity_size_match_is_verified() {
        let spec = VerifySpec::CopyIntegrity;
        let remote = RemoteVerify::CopyIntegrity { size_bytes: 4096 };
        assert_eq!(verify_digest(&spec, &remote, 4096), Integrity::Verified);
    }

    #[test]
    fn copy_integrity_size_mismatch_is_corrupt() {
        let spec = VerifySpec::CopyIntegrity;
        let remote = RemoteVerify::CopyIntegrity { size_bytes: 4000 };
        assert_eq!(verify_digest(&spec, &remote, 4096), Integrity::Corrupt);
    }
}

//! `webd` client-side transport for `scannerd` `VolumeStats`.
//!
//! Wire types intentionally remain crate-local (same pattern as
//! `read_client.rs`): no shared proto crate coupling.

use std::io;
#[cfg(any(unix, test))]
use std::io::{Read, Write};

#[cfg(any(unix, test))]
use serde::{Deserialize, Serialize};

/// `scannerd` stat socket path.
#[cfg(unix)]
pub const SCANNERD_STAT_SOCKET_PATH: &str = "/run/teslausb/scannerd-stat.sock";
#[cfg(any(unix, test))]
const MAX_REQUEST_FRAME: u32 = 64 * 1024;

#[cfg(any(unix, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
struct VolumeStatsRequest {
    #[serde(default)]
    slot: u8,
}

#[cfg(any(unix, test))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum VolumeStatsReply {
    Ok {
        cluster_count: u32,
        bytes_per_cluster: u64,
        used_clusters: u32,
        free_clusters: u32,
        total_bytes: u64,
        used_bytes: u64,
        free_bytes: u64,
        stable: bool,
    },
    Unavailable,
    Error {
        message: String,
    },
}

/// Decoded stat payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeStats {
    pub cluster_count: u32,
    pub bytes_per_cluster: u64,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
    pub used_clusters: u32,
    pub free_clusters: u32,
    pub stable: bool,
}

/// Distinguish "no readable volume" from transport/decode errors.
///
/// The variants are only ever constructed on the `#[cfg(any(unix, test))]`
/// decode path; on a non-Unix host (e.g. the Windows UAT builder) the client
/// always returns a transport error, so the variants are dead there.
#[cfg_attr(not(any(unix, test)), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeStatsOutcome {
    Stats(VolumeStats),
    Unavailable,
}

#[cfg_attr(not(any(unix, test)), allow(dead_code))]
#[derive(Debug, thiserror::Error)]
pub enum VolumeStatsError {
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("frame too large: {len} > {cap} bytes")]
    FrameTooLarge { len: usize, cap: usize },
    #[error("scannerd stat error: {message}")]
    Server { message: String },
}

pub trait VolumeStatsClient: Send + Sync {
    /// Query `TeslaCam` volume stats from scannerd.
    ///
    /// # Errors
    ///
    /// Returns transport/framing/decode failures or server-side errors.
    fn volume_stats(&self) -> Result<VolumeStatsOutcome, VolumeStatsError>;
}

#[cfg(any(unix, test))]
fn frame_cap_usize(cap: u32) -> Result<usize, VolumeStatsError> {
    usize::try_from(cap).map_err(|_| VolumeStatsError::Decode("frame cap overflow".to_owned()))
}

#[cfg(any(unix, test))]
fn read_frame(stream: &mut impl Read, cap: u32) -> Result<Vec<u8>, VolumeStatsError> {
    let mut len_buf = [0_u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len_u32 = u32::from_le_bytes(len_buf);
    let len = usize::try_from(len_u32)
        .map_err(|_| VolumeStatsError::Decode("frame length overflow".to_owned()))?;
    let cap_len = frame_cap_usize(cap)?;
    if len > cap_len {
        return Err(VolumeStatsError::FrameTooLarge { len, cap: cap_len });
    }
    let mut payload = vec![0_u8; len];
    stream.read_exact(&mut payload)?;
    Ok(payload)
}

#[cfg(any(unix, test))]
fn write_frame(stream: &mut impl Write, payload: &[u8], cap: u32) -> Result<(), VolumeStatsError> {
    let cap_len = frame_cap_usize(cap)?;
    if payload.len() > cap_len {
        return Err(VolumeStatsError::FrameTooLarge {
            len: payload.len(),
            cap: cap_len,
        });
    }
    let len_u32 = u32::try_from(payload.len()).map_err(|_| VolumeStatsError::FrameTooLarge {
        len: payload.len(),
        cap: cap_len,
    })?;
    stream.write_all(&len_u32.to_le_bytes())?;
    stream.write_all(payload)?;
    stream.flush()?;
    Ok(())
}

#[cfg(any(unix, test))]
fn decode_reply(reply: VolumeStatsReply) -> Result<VolumeStatsOutcome, VolumeStatsError> {
    match reply {
        VolumeStatsReply::Ok {
            cluster_count,
            bytes_per_cluster,
            used_clusters,
            free_clusters,
            total_bytes,
            used_bytes,
            free_bytes,
            stable,
        } => Ok(VolumeStatsOutcome::Stats(VolumeStats {
            cluster_count,
            bytes_per_cluster,
            total_bytes,
            used_bytes,
            free_bytes,
            used_clusters,
            free_clusters,
            stable,
        })),
        VolumeStatsReply::Unavailable => Ok(VolumeStatsOutcome::Unavailable),
        VolumeStatsReply::Error { message } => Err(VolumeStatsError::Server { message }),
    }
}

#[cfg(unix)]
const READ_TIMEOUT_SECS: u64 = 60;
#[cfg(unix)]
const WRITE_TIMEOUT_SECS: u64 = 10;

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnixVolumeStatsClient {
    socket_path: std::path::PathBuf,
}

#[cfg(unix)]
impl UnixVolumeStatsClient {
    #[must_use]
    pub fn new(socket_path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }
}

#[cfg(unix)]
impl VolumeStatsClient for UnixVolumeStatsClient {
    fn volume_stats(&self) -> Result<VolumeStatsOutcome, VolumeStatsError> {
        use std::os::unix::net::UnixStream;
        use std::time::Duration;

        let mut stream = UnixStream::connect(&self.socket_path)?;
        stream.set_read_timeout(Some(Duration::from_secs(READ_TIMEOUT_SECS)))?;
        stream.set_write_timeout(Some(Duration::from_secs(WRITE_TIMEOUT_SECS)))?;

        let payload = serde_json::to_vec(&VolumeStatsRequest::default())
            .map_err(|err| VolumeStatsError::Decode(err.to_string()))?;
        write_frame(&mut stream, &payload, MAX_REQUEST_FRAME)?;
        let reply_payload = read_frame(&mut stream, MAX_REQUEST_FRAME)?;
        let reply: VolumeStatsReply = serde_json::from_slice(&reply_payload)
            .map_err(|err| VolumeStatsError::Decode(err.to_string()))?;
        decode_reply(reply)
    }
}

/// Fallback client used on non-Unix hosts where the Unix socket transport is
/// unavailable. This returns an I/O unsupported error to distinguish transport
/// absence from server-side `Unavailable`.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableVolumeStatsClient;

impl VolumeStatsClient for UnavailableVolumeStatsClient {
    fn volume_stats(&self) -> Result<VolumeStatsOutcome, VolumeStatsError> {
        Err(VolumeStatsError::Io(io::Error::new(
            io::ErrorKind::Unsupported,
            "scannerd stat socket is unavailable on this platform",
        )))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn decode_reply_maps_wire_variants() {
        let ok = VolumeStatsReply::Ok {
            cluster_count: 10,
            bytes_per_cluster: 512,
            used_clusters: 3,
            free_clusters: 7,
            total_bytes: 5120,
            used_bytes: 1536,
            free_bytes: 3584,
            stable: true,
        };
        let mapped = decode_reply(ok).unwrap();
        assert!(matches!(
            mapped,
            VolumeStatsOutcome::Stats(VolumeStats {
                cluster_count: 10,
                ..
            })
        ));
        assert!(matches!(
            decode_reply(VolumeStatsReply::Unavailable).unwrap(),
            VolumeStatsOutcome::Unavailable
        ));
        assert!(matches!(
            decode_reply(VolumeStatsReply::Error {
                message: "x".to_owned()
            }),
            Err(VolumeStatsError::Server { .. })
        ));
    }

    #[test]
    fn frame_roundtrip_and_decode() {
        let reply = VolumeStatsReply::Ok {
            cluster_count: 8,
            bytes_per_cluster: 1024,
            used_clusters: 2,
            free_clusters: 6,
            total_bytes: 8192,
            used_bytes: 2048,
            free_bytes: 6144,
            stable: false,
        };
        let encoded = serde_json::to_vec(&reply).unwrap();
        let mut frame = Vec::new();
        write_frame(&mut frame, &encoded, MAX_REQUEST_FRAME).unwrap();
        let payload = read_frame(&mut Cursor::new(frame), MAX_REQUEST_FRAME).unwrap();
        let decoded: VolumeStatsReply = serde_json::from_slice(&payload).unwrap();
        assert_eq!(decoded, reply);
    }
}

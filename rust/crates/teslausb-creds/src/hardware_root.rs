use pbkdf2::pbkdf2_hmac;
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::{CredsError, SALT_LEN};

/// Domain separator prepended to the PBKDF2 password input.
pub const DOMAIN_SEPARATOR: &[u8] = b"teslausb-cloud-creds/v1";

/// Source of hardware identity material for credential key derivation.
pub trait HardwareRoot {
    /// Return the full `/proc/cpuinfo` `Serial` value, trimmed and normalized.
    ///
    /// # Errors
    ///
    /// Returns [`CredsError`] if the serial value cannot be loaded/parsed.
    fn serial_ascii(&self) -> Result<String, CredsError>;
    /// Return the `/etc/machine-id` value, trimmed.
    ///
    /// # Errors
    ///
    /// Returns [`CredsError`] if the machine-id value cannot be loaded/parsed.
    fn machine_id_ascii(&self) -> Result<String, CredsError>;
}

/// Production [`HardwareRoot`] implementation for Linux hosts.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ProcHardwareRoot;

impl HardwareRoot for ProcHardwareRoot {
    fn serial_ascii(&self) -> Result<String, CredsError> {
        let cpuinfo = std::fs::read_to_string("/proc/cpuinfo")?;
        parse_cpuinfo_serial(&cpuinfo)
    }

    fn machine_id_ascii(&self) -> Result<String, CredsError> {
        let machine_id = std::fs::read_to_string("/etc/machine-id")?;
        let trimmed = machine_id.trim();
        if trimmed.is_empty() {
            return Err(CredsError::MachineIdEmpty);
        }
        Ok(trimmed.to_owned())
    }
}

/// Test-friendly injectable root values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticHardwareRoot {
    serial: String,
    machine_id: String,
}

impl StaticHardwareRoot {
    /// Build a fixed hardware root for deterministic tests.
    #[must_use]
    pub fn new(serial: impl Into<String>, machine_id: impl Into<String>) -> Self {
        Self {
            serial: serial.into(),
            machine_id: machine_id.into(),
        }
    }
}

impl HardwareRoot for StaticHardwareRoot {
    fn serial_ascii(&self) -> Result<String, CredsError> {
        normalize_serial(&self.serial)
    }

    fn machine_id_ascii(&self) -> Result<String, CredsError> {
        let trimmed = self.machine_id.trim();
        if trimmed.is_empty() {
            return Err(CredsError::MachineIdEmpty);
        }
        Ok(trimmed.to_owned())
    }
}

/// Derive an AES-256 key from hardware root material and `tesla_salt`.
///
/// Password input is exactly:
/// `b"teslausb-cloud-creds/v1" || serial_ascii || 0x00 || machine_id_ascii`.
///
/// # Errors
///
/// Returns [`CredsError`] if hardware identity cannot be loaded or if
/// `kdf_iters` is zero.
pub fn derive_key(
    root: &dyn HardwareRoot,
    salt: &[u8; SALT_LEN],
    kdf_iters: u32,
) -> Result<Zeroizing<[u8; 32]>, CredsError> {
    if kdf_iters == 0 {
        return Err(CredsError::InvalidKdfIterations);
    }
    let serial = root.serial_ascii()?;
    let machine_id = root.machine_id_ascii()?;
    let mut password = Zeroizing::new(Vec::<u8>::with_capacity(
        DOMAIN_SEPARATOR.len() + serial.len() + 1 + machine_id.len(),
    ));
    password.extend_from_slice(DOMAIN_SEPARATOR);
    password.extend_from_slice(serial.as_bytes());
    password.push(0_u8);
    password.extend_from_slice(machine_id.as_bytes());

    let mut key = Zeroizing::new([0_u8; 32]);
    pbkdf2_hmac::<Sha256>(&password, salt, kdf_iters, &mut *key);
    Ok(key)
}

pub(crate) fn parse_cpuinfo_serial(cpuinfo: &str) -> Result<String, CredsError> {
    for line in cpuinfo.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim() != "Serial" {
            continue;
        }
        return normalize_serial(value);
    }
    Err(CredsError::CpuSerialMissing)
}

fn normalize_serial(raw: &str) -> Result<String, CredsError> {
    let serial = raw.trim().to_ascii_lowercase();
    if serial.is_empty() {
        return Err(CredsError::CpuSerialMalformed);
    }
    if !serial.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(CredsError::CpuSerialMalformed);
    }
    Ok(serial)
}

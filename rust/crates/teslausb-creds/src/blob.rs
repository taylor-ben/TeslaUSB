use aes_gcm::aead::{AeadInPlace, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce, Tag};
use std::fmt;
use zeroize::Zeroizing;

use crate::{
    BLOB_MAGIC, BLOB_VERSION, CredsError, DEFAULT_KDF_ITERS, KDF_ID_PBKDF2_SHA256, NONCE_LEN,
    SALT_LEN, TAG_LEN,
};

const HEADER_LEN: usize = BLOB_MAGIC.len() + 1 + 1 + 4 + SALT_LEN + NONCE_LEN;

/// Key material used to encrypt/decrypt a credential blob.
#[derive(Clone)]
pub struct BlobKeyMaterial {
    /// 32-byte AES-256 key.
    pub key: Zeroizing<[u8; 32]>,
    /// 32-byte credential salt that is embedded in the blob header.
    pub salt: [u8; SALT_LEN],
    /// PBKDF2 iteration count embedded in the blob header.
    pub kdf_iters: u32,
}

/// Parsed blob header fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedBlobHeader {
    /// Blob version.
    pub version: u8,
    /// KDF identifier.
    pub kdf_id: u8,
    /// PBKDF2 iterations recorded in the blob.
    pub kdf_iters: u32,
    /// 32-byte `tesla_salt`.
    pub salt: [u8; SALT_LEN],
    /// AES-GCM nonce.
    pub nonce: [u8; NONCE_LEN],
    /// Header length in bytes (`magic..=nonce`).
    pub header_len: usize,
}

/// Encrypt `plaintext` into the frozen blob framing using a fresh CSPRNG nonce.
///
/// # Errors
///
/// Returns [`CredsError`] if nonce generation fails or the payload cannot be
/// encrypted.
pub fn encrypt(plaintext: &[u8], key: &BlobKeyMaterial) -> Result<Vec<u8>, CredsError> {
    let mut nonce = [0_u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce).map_err(|err| CredsError::Random(err.to_string()))?;
    encrypt_with_nonce(plaintext, key, nonce)
}

/// Encrypt `plaintext` with a caller-provided nonce.
///
/// This exists for deterministic test vectors.
///
/// # Errors
///
/// Returns [`CredsError`] if `kdf_iters` is zero or encryption fails.
fn encrypt_with_nonce(
    plaintext: &[u8],
    key: &BlobKeyMaterial,
    nonce: [u8; NONCE_LEN],
) -> Result<Vec<u8>, CredsError> {
    if key.kdf_iters == 0 {
        return Err(CredsError::InvalidKdfIterations);
    }
    let header = build_header(key.salt, nonce, key.kdf_iters);
    let cipher = Aes256Gcm::new_from_slice(key.key.as_ref())
        .map_err(|_| CredsError::InvalidBlob("invalid key length"))?;
    let mut ciphertext = plaintext.to_vec();
    let tag = cipher
        .encrypt_in_place_detached(Nonce::from_slice(&nonce), &header, &mut ciphertext)
        .map_err(|_| CredsError::DecryptFailed)?;
    let mut blob = header;
    blob.extend_from_slice(&ciphertext);
    blob.extend_from_slice(tag.as_slice());
    Ok(blob)
}

/// Deterministic encrypt helper exposed for committed KAT tests.
///
/// # Errors
///
/// Returns [`CredsError`] on invalid parameters or encryption failure.
pub fn encrypt_with_nonce_for_test(
    plaintext: &[u8],
    key: &BlobKeyMaterial,
    nonce: [u8; NONCE_LEN],
) -> Result<Vec<u8>, CredsError> {
    encrypt_with_nonce(plaintext, key, nonce)
}

/// Decrypt a frozen credential blob.
///
/// Returns decrypted plaintext in [`Zeroizing`] storage.
///
/// # Errors
///
/// Returns [`CredsError::DecryptFailed`] for authentication failures (including
/// wrong machine/derived key) and typed parse errors for malformed headers.
pub fn decrypt(blob: &[u8], key: &BlobKeyMaterial) -> Result<Zeroizing<Vec<u8>>, CredsError> {
    let (_, plaintext) = decrypt_with_parsed_header(blob, key)?;
    Ok(plaintext)
}

/// Decrypt a blob and return both parsed header and plaintext.
///
/// # Errors
///
/// Returns [`CredsError`] on malformed blobs or auth failure.
pub fn decrypt_with_parsed_header(
    blob: &[u8],
    key: &BlobKeyMaterial,
) -> Result<(ParsedBlobHeader, Zeroizing<Vec<u8>>), CredsError> {
    let header = parse_header(blob)?;
    let payload = blob
        .get(header.header_len..)
        .ok_or(CredsError::InvalidBlob("missing ciphertext+tag"))?;
    if payload.len() < TAG_LEN {
        return Err(CredsError::InvalidBlob("missing gcm tag"));
    }
    let split = payload
        .len()
        .checked_sub(TAG_LEN)
        .ok_or(CredsError::InvalidBlob("missing gcm tag"))?;
    let (ciphertext, tag_bytes) = payload.split_at(split);
    let aad = blob
        .get(0..header.header_len)
        .ok_or(CredsError::InvalidBlob("aad range overflow"))?;

    let cipher = Aes256Gcm::new_from_slice(key.key.as_ref())
        .map_err(|_| CredsError::InvalidBlob("invalid key length"))?;
    let mut plaintext = Zeroizing::new(ciphertext.to_vec());
    cipher
        .decrypt_in_place_detached(
            Nonce::from_slice(&header.nonce),
            aad,
            &mut plaintext,
            Tag::from_slice(tag_bytes),
        )
        .map_err(|_| CredsError::DecryptFailed)?;
    Ok((header, plaintext))
}

fn build_header(salt: [u8; SALT_LEN], nonce: [u8; NONCE_LEN], kdf_iters: u32) -> Vec<u8> {
    let mut header = Vec::with_capacity(HEADER_LEN);
    header.extend_from_slice(BLOB_MAGIC);
    header.push(BLOB_VERSION);
    header.push(KDF_ID_PBKDF2_SHA256);
    header.extend_from_slice(&kdf_iters.to_be_bytes());
    header.extend_from_slice(&salt);
    header.extend_from_slice(&nonce);
    header
}

fn parse_header(blob: &[u8]) -> Result<ParsedBlobHeader, CredsError> {
    if blob.len() < HEADER_LEN + TAG_LEN {
        return Err(CredsError::InvalidBlob("blob too short"));
    }
    let mut offset = 0_usize;
    let magic = take(blob, &mut offset, BLOB_MAGIC.len())?;
    if magic != BLOB_MAGIC {
        return Err(CredsError::InvalidBlob("bad magic"));
    }
    let version = take(blob, &mut offset, 1)?
        .first()
        .copied()
        .ok_or(CredsError::InvalidBlob("missing version"))?;
    if version != BLOB_VERSION {
        return Err(CredsError::UnsupportedBlobVersion(version));
    }
    let kdf_id = take(blob, &mut offset, 1)?
        .first()
        .copied()
        .ok_or(CredsError::InvalidBlob("missing kdf id"))?;
    if kdf_id != KDF_ID_PBKDF2_SHA256 {
        return Err(CredsError::UnsupportedKdfId(kdf_id));
    }
    let iters_bytes = take(blob, &mut offset, 4)?;
    let mut iters_buf = [0_u8; 4];
    iters_buf.copy_from_slice(iters_bytes);
    let kdf_iters = u32::from_be_bytes(iters_buf);
    if kdf_iters == 0 {
        return Err(CredsError::InvalidKdfIterations);
    }
    let mut salt = [0_u8; SALT_LEN];
    salt.copy_from_slice(take(blob, &mut offset, SALT_LEN)?);
    let mut nonce = [0_u8; NONCE_LEN];
    nonce.copy_from_slice(take(blob, &mut offset, NONCE_LEN)?);
    Ok(ParsedBlobHeader {
        version,
        kdf_id,
        kdf_iters,
        salt,
        nonce,
        header_len: offset,
    })
}

fn take<'a>(src: &'a [u8], offset: &mut usize, len: usize) -> Result<&'a [u8], CredsError> {
    let end = offset
        .checked_add(len)
        .ok_or(CredsError::InvalidBlob("offset overflow"))?;
    let chunk = src
        .get(*offset..end)
        .ok_or(CredsError::InvalidBlob("truncated header"))?;
    *offset = end;
    Ok(chunk)
}

impl Default for BlobKeyMaterial {
    fn default() -> Self {
        Self {
            key: Zeroizing::new([0_u8; 32]),
            salt: [0_u8; SALT_LEN],
            kdf_iters: DEFAULT_KDF_ITERS,
        }
    }
}

impl fmt::Debug for BlobKeyMaterial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlobKeyMaterial")
            .field("key", &"<redacted>")
            .field("salt", &self.salt)
            .field("kdf_iters", &self.kdf_iters)
            .finish()
    }
}

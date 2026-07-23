# Contract: cloud provider credentials (Rust AEAD, hardware-bound)

Status: **NOT FROZEN — reconciled design draft with OPEN ITEMS.** Target surface
for **P2**/**P3**/**P7**. A Tier-3 security review (cycle 2,
`files/cloud-p0-review-reconciliation.md`) found the draft left security-critical
choices open while calling itself frozen. Corrected below; the exact contracts are
**pinned in P2**, not here.

> **RESOLVED (Opus decision + GPT-5.5 second opinion, operator-confirmed 2026):**
>   **AEAD = AES-256-GCM; KDF = PBKDF2-HMAC-SHA256** (see §2 for the frozen
>   scheme). **Hardware root = software-only (Option A)** — operator explicitly
>   declined the customer-OTP root, accepting the ~32-bit SD-only-theft ceiling
>   (§2.1). No OTP seam is built. P2 still owes the exact byte framing + committed
>   KAT vectors (§2, closed against the compiler).
>
> **OPEN ITEMS (pin in P2, security-critical):**
> - **Enumerate the exact allowed option-key set per backend type** (a positive
>   allow-list). "Including but not limited to …" is not enforceable; every
>   unknown/banned key → **whole-request reject** (webd 400).
> - **Resolve credential ownership (D9):** uploadd is named sole owner, yet webd
>   validates+rewrites the blob and `LoadCredential=` is a startup copy (no
>   hot-reload) while uploadd promises never to self-restart. Freeze one model —
>   e.g. an uploadd-owned `set_credentials` write + an explicit supervisor-cycle
>   contract — before P2/P4 code.
> - **Redact inside the rclone engine before persistence.** `rclone.rs` embeds
>   raw stderr/stdout in its error, and `QueueStore::persist` stores it —
>   sanitizing only at the API/JobHub edge is too late (D8).
> - **Safe-open is an FD handoff, not a path (D10):** validating with `openat2`
>   then passing a *pathname* to rclone reopens the file (symlink-swap TOCTOU);
>   and `ProtectSystem=strict` does not hide the live LUN — use `InaccessiblePaths`
>   / a mount namespace. Freeze the actual mechanism in P2/P3.

Rust reimplementation of v1's Fernet-encrypted provider config. **No Python, no
Fernet library** — the *intent* (encrypted-at-rest, hardware-bound) ported to a
vetted Rust AEAD.

---

## 1. What it stores
Operator cloud credentials for a **single active destination**, one of v1's three
flows:
1. **OAuth** — Drive/OneDrive/Dropbox; a pasted `rclone authorize` token blob.
2. **S3-style** — S3/B2/Wasabi access key + secret + region/endpoint.
3. **NAS / custom-rclone** — SFTP/WebDAV/SMB/FTP/S3-compat/AzureBlob/Swift via a
   typed form **or** a pasted `rclone.conf` (single remote).

## 2. At-rest format — `cloud_provider_creds.bin` (M4)
- **AEAD (FROZEN): AES-256-GCM.** Hardware-accelerated on the Cortex-A53 (ARMv8
  crypto ext); writes are rare so a random 96-bit nonce is safe. Framed, versioned
  blob: `magic ‖ version ‖ kdf_id ‖ kdf_iters ‖ salt ‖ nonce ‖ ciphertext ‖ tag`.
  **AAD binds the header** (`magic‖version‖kdf_id‖kdf_iters‖salt‖nonce`) so a
  downgrade/parameter swap fails the tag. Nonce is a **fresh CSPRNG 96 bits per
  write** (never reused); **fail closed if the RNG is unavailable**. Exact byte
  offsets/endianness (big-endian for multibyte header ints) + the canonical
  plaintext schema (a versioned serde struct for the §1 flows) are pinned in P2
  and locked by committed KAT vectors.

### 2.1 Key derivation (FROZEN scheme — software-only hardware root, Option A)
- **KDF = PBKDF2-HMAC-SHA256**, `dklen = 32` (the AES-256 key), directly.
  `kdf_id = 1` selects this scheme; `kdf_iters` (u32 in the header) records the
  iteration count so it can be **raised later without breaking old blobs**. Default
  **600 000** iterations (OWASP-class, memory-light — will not OOM `uploadd` on the
  512 MB Pi with `OOMScoreAdjust=900`, unlike a memory-hard KDF).
- **Why PBKDF2, not HKDF/Argon2id:** the confirmed hardware root is **low-entropy**
  (§ below), so the security-relevant operation is **iteration-stretch**. A single
  fast HKDF pass would leave the ~32-bit root trivially brute-forceable; a
  memory-hard KDF adds OOM risk for no extra benefit on a fixed (non-password)
  input. PBKDF2's iteration count is exactly the knob that raises the SD-only
  brute-force cost.
- **Root material (password input):** a fixed domain string ‖ **full SoC serial**
  (from `/proc/cpuinfo` `Serial`, used in full — **never** the last-8/32-bit form)
  ‖ `0x00` ‖ `/etc/machine-id`. **Salt** = `tesla_salt.bin` (32 random bytes,
  per-install). Wrong machine ⇒ AEAD decrypt **fails closed** (generic error, no
  partial/plaintext fallback).
- **Security ceiling (documented, operator-accepted):** `machine-id` and
  `tesla_salt.bin` live **on the SD card**, so they add no secrecy against
  card-only theft; the **only** off-card input is the SoC serial, which on this Pi
  is **~32-bit, factory-assigned, non-random** (OTP row 28). Therefore an attacker
  who images **only the SD card** faces a ~32-bit search stretched by
  `kdf_iters` — meaningfully raised vs plaintext (and vs v1, which bound to the
  on-card `machine-id` alone), but **not** a strong secret store. The strong root
  (a burned 256-bit customer-OTP secret) was **explicitly declined** by the
  operator; do **not** build an OTP path. State this ceiling in the P6 Cloud-screen
  docs so operators aren't misled.
- **Atomic write:** temp in the same dir → `fsync` → `rename` → `fsync` dir;
  mode `0600`, owned by the uploadd service user. `tesla_salt.bin` is created
  once (mode `0600`) and backed up with the blob.
- **Test vectors** (M4): committed known-answer vectors (fixed key/nonce/AAD →
  ciphertext/tag) so the AEAD framing can't silently regress.

## 3. Runtime rendering — transient `rclone.conf` in tmpfs
uploadd synthesizes a **single-remote** `rclone.conf` at
`/run/teslausb/rclone.conf` (tmpfs), mode `0600`, containing exactly one
`[teslausb]` section built from the decrypted creds:
- SFTP/WebDAV/SMB/FTP passwords passed through **`rclone obscure`**; S3/B2/Wasabi
  secrets verbatim; OAuth token blob inlined.
- **Never** written to the SD/persistent fs. Removed on exit **and** proactively
  at startup (D9 cleanup).

## 4. Backend allow-list + **option-key allow-list** (D8 — security-critical)

Two layers, both enforced:

1. **Type allow-list:** `sftp, webdav, smb, ftp, s3, b2, wasabi, azureblob,
   swift, drive, onedrive, dropbox`. **Reject** `crypt, union, chunker, local,
   http, alias`, `cache`, `crypt`-wrappers, and anything not listed.
   - **Normalize `wasabi` → `type = s3, provider = Wasabi`** (M6): `type=wasabi`
     is **not** a valid rclone backend and would fail at runtime.
2. **Per-key allow-list (the real teeth):** whatever the source (form *or* pasted
   `rclone.conf`), only a **known-safe option key set per backend type** is
   copied into `[teslausb]`. **Reject/strip** any key that can execute a command,
   read a local path, or reach a side channel, including but not limited to:
   `command`, `*_command` (e.g. WebDAV **`bearer_token_command`**), `sftp` key/
   agent/known-hosts-command hooks, `*_helper`, arbitrary `headers`,
   unix-socket/`--rc`* options, `env_auth`, `file`/path-valued auth options, and
   any `--` passthrough. A pasted `rclone.conf` with **more than one section**, or
   any unknown key, is **rejected whole** — never merged. This closes the "paste a
   crypt/command remote and get code/file-exfil" class.

## 5. Ownership, reload & cleanup lifecycle (D9)
- **Single owner:** **uploadd** owns decrypt + render. `LoadCredential=` copies
  the blob into the unit's private `$CREDENTIALS_DIRECTORY` **once at start**
  (systemd does not hot-reload it), so a credential *change* from webd requires an
  uploadd **reload/restart** signal — the contract specifies that path (webd asks
  the supervisor to cycle uploadd), not an expectation that the running process
  re-reads the file.
- **OAuth refresh:** when rclone refreshes an OAuth token, uploadd must **read the
  updated token back** out of the runtime remote and **re-encrypt** it to
  `cloud_provider_creds.bin` (else the refreshed token is lost on the next
  reboot). This read-back+re-encrypt path is part of P3.
- **Cleanup is startup + stop (not Drop-reliant):** `panic = "abort"` (confirmed
  `rust/Cargo.toml`) and `SIGKILL` both **bypass `Drop`**, so tmpfs cleanup must
  not depend on a destructor. Remove `/run/teslausb/rclone.conf` (and any rendered
  secret) **at process startup** *and* via systemd **`ExecStopPost=`**; treat any
  pre-existing file as stale and overwrite/remove.

## 6. v1 migration (D9)
v1's Fernet blob (16-byte-key derived, different framing) is **not decryptable** by
this 32-byte AEAD scheme. There is **no silent import**: on first B-1 boot the
operator **re-enters** credentials through the Cloud screen (P6). Document this in
P6; do not attempt to parse or reuse the v1 blob.

## 7. Sandbox (D8)
rclone runs **unprivileged** (dedicated service user, `NoNewPrivileges=yes`,
`ProtectSystem=strict`, `PrivateTmp=yes`, a minimal `ReadOnlyPaths`/
`ReadWritePaths` set covering only the archive-read root and `/run/teslausb`), so
even a malicious remote definition that slipped the allow-list cannot escalate,
write outside its lane, or read the live LUN. Aligns with the §4 hardening in
`uploadd.md` (D10 safe-open) + M3 resource bounds.

## 8. Per-backend verification capability (D3)
Frozen table uploadd's verify step (`uploadd.md` §3.3) consults — there is **no
universal SHA-256**:

| backend | native hash for `rclone hashsum` | verify policy |
|---|---|---|
| s3 / b2 / wasabi | ETag/MD5 (or SHA where offered) | native hashsum compare |
| drive | MD5/SHA (per file) | native hashsum compare |
| onedrive | SHA1/quickXor | native hashsum compare |
| dropbox | dropbox content-hash | native hashsum compare |
| sftp | often none | rely on rclone copy integrity (size + any available hash) |
| webdav | usually none | rely on rclone copy integrity |
| smb / ftp | none | rely on rclone copy integrity |
| azureblob / swift | MD5 | native hashsum compare |

A full `--download` re-verify (2× bandwidth) is **opt-in + throttled only**. The
child's `hash_alg` (`indexd-cloud-schema.md` §2.1) records which algorithm was
used so the dedup identity stays consistent.

## 9. Tests (P2/P3 acceptance)
AEAD round-trip; **wrong-machine decrypt fails closed**; committed KAT vectors;
AAD-tamper/downgrade rejected; type allow-list (reject crypt/union/local/http);
**per-key allow-list** (reject `bearer_token_command`, `*_command`, `env_auth`,
multi-section paste); `wasabi`→`s3,provider=Wasabi` normalization; `obscure`
applied to sftp/webdav/smb/ftp only; tmpfs render `0600` + **startup and stop
cleanup** (survives a simulated SIGKILL: file removed on next start); OAuth
read-back re-encrypt persists a rotated token.

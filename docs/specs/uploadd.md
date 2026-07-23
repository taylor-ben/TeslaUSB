# uploadd — durable, resumable, throttled cloud-upload daemon

Status: **NOT FROZEN — reconciled design draft with OPEN ITEMS** (cloud-sync
buildout, 2026-07). A first draft was frozen-reviewed twice (Tier-3 adversarial);
the second pass **overturned several code-level premises** the draft relied on
(see `files/cloud-p0-review-reconciliation.md`, cycle 2). Those premise errors are
corrected below; the remaining wire-level details are marked **OPEN** and are to
be finalized in their implementing phase against the compiler — **not** pre-frozen
here. Do not treat any table below as a frozen contract until its OPEN items close.

> **⚠️ Corrected premises (verified against code, 2026-07):**
> - The archive driver is **Phase-1 `RecentClips`-only** (`archive_driver.rs`
>   hardcodes `folder_class: "RecentClips"`); it does **not** register Sentry/Saved
>   event directories. Cloud-uploading *events* is therefore **gated on an
>   archive-registration path for those folders that does not exist yet** (§2).
> - The durable queue identity is a **single `ArchiveItemId(i64)`**
>   (`queue.rs` — `QueueItem.id`, `UploadQueue` dedups/`get`/`select_next` on it).
>   Modeling per-child objects requires a **queue-identity type change** in P1/P3;
>   it is not an existing capability (§3.1).
> - retentiond publishes storage pressure as a **file**
>   (`/run/teslausb/retentiond.governor.json`, `{uploads_allowed: bool}`) — **no
>   socket, no `seq`, no action** (§6.2).
> - The upload **lease is boot-scoped monotonic ms** (`leases.boot_id` +
>   `expires_mono_ms`), **not** unix-seconds wall-clock (RTC-less target) — see the
>   schema contract.
>
> **OPEN ITEMS (finalize in-phase, not here):** exact throttle-action precedence
> ordering (`gate()` is link-first today); the cancellable rclone child-supervisor
> + thread-safe lease seam; the safe-open **FD-handoff** mechanism that closes the
> openat2→pass-pathname-to-rclone TOCTOU (§4/§6.3).

Concept source: v1
[`CLOUD_ARCHIVE.md`](https://github.com/mphacker/TeslaUSB/blob/main/docs/CLOUD_ARCHIVE.md).
Ported idiomatically to Rust — **no Python**.

---

## 1. Purpose & scope

`uploadd` copies dashcam events off the Pi to an operator-configured cloud
remote — automatically, in priority order, without knocking WiFi offline, and
resumably across crashes. It is the B-1 home for v1's Cloud Archive worker.

**In scope:** *what* to upload, *in what order*, staying under the WiFi TX cap,
resuming without duplication, holding an upload lease so the space governor can't
evict a file mid-transfer, integrity-verifying each transfer, and flagging
durability so retention can delete the car-side copy.

**Out of scope (owned elsewhere):** producing the archive (`retentiond`
`archive_driver`); deleting anything (`retentiond` is the sole deleter —
`single-writer-lease.md` §4; uploadd has **no delete seam**); deriving the WiFi
cap/link state (`wifid`, D4); storage backpressure (`retentiond` storage plane);
credential encryption (`cloud-provider-creds.md`); persistence (indexd is the
sole SQLite writer — `indexd-cloud-schema.md`).

## 2. The unit of work: archive item + child objects (D2) — **gated on event registration**

An archived clip/event is a **group of camera-angle files** (plus, for real
Tesla events, an `event.json`), registered as **one `archive_items` row**. The
Phase-1 `archive_driver.rs` proves this shape for `RecentClips` (one
`ArchiveRegistration` with N `angles`), but it **hardcodes
`folder_class: "RecentClips"` and does not register Sentry/Saved event
directories**. So:

- **PREREQUISITE (not owned by cloud-sync):** uploading Sentry/Saved *events*
  requires an archive-registration path that catalogs those folders (with their
  angle files + `event.json`) as `archive_items`. Until that exists, cloud-sync
  can only target what indexd actually holds (RecentClips-class archives). This
  gate is tracked in `plan.md`; do not assume Sentry/Saved archive rows exist.
- A single `rclone copyto` + a single hash cannot upload/verify a whole group, so
  the model is **parent `archive_items` row + child objects** (one child = one
  angle file). The parent's `durable` flag flips **only when every child is
  verified** (§6.6 / D5).
- **The queue cannot express a child today.** `queue.rs::QueueItem.id` is a single
  `ArchiveItemId(i64)` and `UploadQueue` dedups/selects on it, so N children of
  one parent collapse to one item. Representing children requires a
  **queue-identity type change** (P1/P3): a child-level key `(archive_item_id,
  child_key)` threaded through `QueueItem`/`UploadQueue`/`select_next`. This is
  design, not existing behavior.
- **Candidates come from indexd** (a `cloud_candidates` RPC), never a raw fs scan
  — a scan cannot derive `archive_item_id`. indexd returns source paths + a
  reusable local content hash; the **backend-specific** verification hash is
  derived by uploadd after it knows the provider (D3).

## 3. The core (host-testable, pure)

The core library `uploadd::*` decides policy behind traits (seams); real side
effects live in the gated binary (§7). Seams: `source::ArchiveSource`,
`transfer::Uploader` (Rust-uploader path only — see §5), `rclone::CommandRunner`
(the chosen path), `lease::LeaseClient`, `queue::QueueStore`,
`durability::DurabilityClient`, `throttle::ThrottleSource`, `time::Clock`/`Waiter`.
Both `engine::UploadEngine` (chunked) and `rclone::RcloneUploadEngine`
(whole-file) satisfy the **per-item** `serve::UploadProcessor` contract, and
`serve::Scheduler` drives either unchanged.

### 3.1 The durable child-object queue (state machine)

`queue.rs::UploadState`: `Queued → InProgress → Done` (terminal), with
`InProgress → Failed(reason, reset_offset)` and retry `Failed → InProgress`.

`QueueItem` (one child object): `id` (`ArchiveItemId` of the **parent event** +
child discriminator — see D7 identity), `source_rel`, `remote_key`, `category`,
`seq` (FIFO tiebreak), `total_bytes`, `expected_hash`, `state`, `bytes_uploaded`,
`attempts`, `last_error`.

- **Idempotent enqueue** dedupes on the queue identity (D7). Re-offering the same
  identity is a no-op.
- **Resume granularity is backend-dependent (D1):** the state machine *models* a
  byte checkpoint, but the **rclone backend resumes at whole-file granularity**
  (`bytes_uploaded ∈ {0, total}`; `copyto` overwrites, so a restart re-copies the
  whole file — safe, non-duplicating). Sub-file checkpointing is reserved for a
  future Rust uploader.
- **Completion is terminal**; **`Failed` is retryable** while
  `attempts < max_attempts` **and** its persisted **not-before** backoff has
  elapsed (M2).

### 3.2 Priority order

`PriorityPolicy` ranks `UploadCategory`: **`EventSentry` → `Trip` → `Bulk`**,
FIFO by `seq`. The candidate step (§6) computes v1's order
`folder_index * 1000 + content_score` (folder axis dominates; oldest-event-first
within a folder), then maps **folder-class → category** per the pinned mapping in
`indexd-cloud-schema.md` §4 so the pure policy reproduces it. Priority reorder
changes only `seq`/ordering — **never** `remote_key` derivation (D7).

### 3.3 Transfer & integrity (D1, D3)

The chosen backend is **rclone whole-file** (§5). Integrity is **capability-based
per backend (D3)** — there is no universal SHA-256:

- Prefer the remote's **native checksum** via `rclone hashsum <alg>` where the
  backend supports one (e.g. S3/B2). Compare to the child's `expected_hash`
  computed in that same algorithm.
- Where the remote exposes **no usable hash** (some WebDAV/SFTP configs), rely on
  **rclone's own copy integrity** (it verifies size, and native hash where
  available, during `copyto`) as the primary durability signal; a `copyto` that
  exits 0 under that policy is treated as verified.
- A full `--download` re-verify is **only** used when explicitly enabled and its
  2× bandwidth is calibrated and throttled — never silently.

The per-backend verification capability + the exact durability criterion are
frozen in `cloud-provider-creds.md` §8.

## 4. Hard invariants

1. **Source only from the archive — enforced, not lexical (D10).** Beyond
   `ArchiveRoot::resolve`'s textual guard, the live `ArchiveSource` opens through
   a root-directory FD with `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS |
   RESOLVE_NO_MAGICLINKS)` (or an equivalent descriptor-based traversal), and
   rclone is invoked so it does **not** follow symlinks — a symlink planted under
   the archive cannot redirect a read to the live LUN. The root is validated
   absolute once at startup.
2. **Never deletes Pi-side files.** No delete seam exists.
3. **Never exceeds the WiFi TX cap.** The rclone path bounds rate with
   `rclone --bwlimit` seeded from the wifid cap (the "belt"); the kernel `tc` cap
   is the "braces". Uploads **stop entirely** when the throttle gate says pause
   (§6.2). *(The D4 `max_chunk_bytes` per-write ceiling is **not** enforceable by
   rclone and applies only to the Rust-uploader `Pacer` path — D1.)*
4. **Never reboots/restarts anything.** Failures retry in-queue with backoff.

## 5. Backend decision — rclone, **PROVISIONAL, locked at Phase 8 (D1)**

`rclone::RcloneUploadEngine` implements `UploadProcessor` directly: whole-file
`rclone copyto` (paced by `--bwlimit`) + capability-based verify (§3.3), behind
the `CommandRunner` subprocess seam. Chosen for provider breadth + v1 parity.
The `transfer::Uploader` chunked seam is preserved for a future Rust uploader but
is **not** on the rclone path.

The decision is **provisional until the Phase 8 WiFi TX-cap spike proves** rclone
(with `--bwlimit`, `--buffer-size 0`, one transfer) stays under the measured
BCM43436 SDIO-deadlock threshold on the Pi Zero 2 W. If it cannot, the fallback
is the in-process Rust uploader (which *can* honor `max_chunk_bytes`). Do not
mark the backend frozen before Phase 8.

## 6. Live-wiring contract (`uploadd serve` — the P3 deliverable)

Today `main.rs serve` prints + exits `FAILURE`. P3 provides the **live seam
implementations** and runs `serve::Scheduler` + `RcloneUploadEngine`.

### 6.1 Persistence — indexd RPC clients
`QueueStore`, `LeaseClient` (upload-kind), `DurabilityClient` over the indexd
control socket (framed-JSON `{"cmd":…}`, mirror
`webd::indexd_client::UnixIndexdClient`). The concrete verbs (**which do not exist
in `proto.rs`/`server.rs` yet** — P1 adds them), envelopes, error mapping, TTL
units, and pagination are frozen in `indexd-cloud-schema.md` §3.

### 6.2 Throttle — two planes, receipt-age freshness (D4)
`ThrottleSource` combines the **wifid link plane** (`WifiThrottle`) **and the
retentiond storage plane**; the effective gate is `wifi_allows &&
storage_allows`. When paused uploadd takes the most-restrictive action — but the
exact **action precedence is OPEN** (retentiond's own `ThrottleSnapshot::gate` is
link-first today; the ordering across `drain_no_new` / `pause_at_checkpoint` /
`abort_resume_later` must be frozen with the P3 impl, not assumed here).

- **Transport (corrected):**
  - *wifi plane* — read over the existing wifid IPC socket. Note `get_ap_status`
    **already returns the full `WifiStatus` incl. throttle state**; whether P3
    consumes that or adds a dedicated `get_throttle` verb is an **OPEN** choice
    (if both exist they must stay coherent). Either way uploadd does not invent a
    file.
  - *storage plane* — retentiond publishes **`/run/teslausb/retentiond.governor.json`**
    (a file: `{ uploads_allowed: bool, … }`), **not** a socket and **without** a
    `seq`/action field. P3 either consumes that file or, preferably, P5 extends
    retentiond to emit a richer signal (seq + action) alongside it. The
    `StoragePressure` type in `throttle.rs` is the *consumed* shape; the
    **adapter from governor.json → StoragePressure is a P3/P5 deliverable**, and
    a missing/stale file fails closed.
- **Freshness = receipt-age based:** equal `seq` **refreshes** the receipt age
  (an unchanged body is still fresh); a **lower** `seq` is rejected; a publisher
  restart carries an instance/generation id so a reset `seq` isn't mistaken for
  stale. (The wifi plane has a `seq`; the storage file does **not** yet — so its
  freshness is **file-mtime/receipt-age** based until retentiond gains a `seq`.)
  Fail closed (`uploads_allowed=false`) only when the **last good receipt exceeds
  the staleness window**, or either plane is unreadable. Never authorize an upload
  before a fresh *allowed* state (`WifiThrottle::closed()` is the boot default).
- **Decode fix:** uploadd's `throttle::PauseReason` must gain **`ApConcurrent`**
  (wifid emits `ap_concurrent`; today uploadd cannot deserialize it). Actions:
  `run`, `drain_no_new`, `abort_resume_later`, `pause_at_checkpoint`. For the
  **whole-file rclone path**, `pause_at_checkpoint` is treated as
  `abort_resume_later` (kill the child, resume whole-file later).

### 6.3 Archive reads — `ArchiveSource`
`size`/`read_chunk` over ext4, accepting only `ArchivePath`, via the D10 safe
open. Used to compute each child's `expected_hash` at candidate time; **reuse the
hash retentiond already computed at archive-copy time** where available (M3)
rather than re-reading. rclone reads the file itself for the actual transfer.

### 6.4 Candidate discovery → dedup → enqueue
On a timer, on WiFi-connect, and on a `sync-now` signal: request candidates from
indexd (`cloud_candidates`, paginated) for the enabled folders (`SentryClips`,
`SavedClips`, `ArchivedClips` — **never** `RecentClips`), in configured priority
order. For each child object: **dedup-check** by `(destination_id,
canonical_remote_key)` + hash/size (D7) — skip only on a hash/size match;
otherwise idempotently enqueue. A differing-hash collision on the same key
**parks** for operator inspection, never overwrites. Config
(`cloud_config_get`) supplies folders/priority/reserve/retry/toggles.

### 6.5 Reserve gate (M6)
Before starting a new event, if `reserve_gb` is set, require
`remote_free − reserve_gb ≥ next_event_size` (via `rclone about`). When the
backend doesn't support `rclone about`, skip the check and log once; do not block.

### 6.6 Loop & atomic completion (D5, M2, M3)
Per child: throttle-gate → resolve safe `ArchivePath` → acquire upload lease
(hold it across the transfer; **renew from a background thread** because
`rclone copyto` is a single blocking subprocess the single-threaded core can't
renew mid-copy — D1) → `copyto` under `--bwlimit`, `--retries 1`, `--buffer-size 0`,
bounded output, a timeout, and **process-group cancellation** so a cap
*reduction* or `abort_resume_later` **kills and restarts** it → verify per §3.3 →
`cloud_upload_commit` (§ below) → release lease. On failure: `cloud_upload_fail`
(persist attempts + capped-exponential **not-before** backoff with jitter;
auth/config errors classified terminal) → retry per policy → push a **sanitized**
failure to webd's JobHub (M5).

**Atomic completion (D5):** a verified child is committed with **one idempotent
`cloud_upload_commit` transaction** that records the dedup row, appends history,
completes the queue row, and — **when all of the parent's children are Done** —
sets `archive_items.durable=1`, all keyed by `(destination_id, child,
content-version)`. Never mark the parent durable on a single child.

### 6.7 Enablement gate
`serve` going live is gated on the **Phase 8 TX-cap spike** (§5) and the security
prerequisites (webd auth — D8). On enablement, `uploadd.service` moves
`TESLAUSB_STAGED_SERVICES` → `TESLAUSB_APP_SERVICES`, gains `LoadCredential=`
(`cloud-provider-creds.md` §5), and the **resource bounds are mandatory (M3):**
`Nice=19`, `IOSchedulingClass=idle`, one transfer/checker, bounded subprocess
output, timeouts, and cgroup memory/IO/CPU limits — `OOMScoreAdjust=900` is a
tiebreaker, not a bound.

## 7. Resolved open questions
- **Backend:** rclone, provisional/Phase-8-locked (§5).
- **Persistence home:** indexd migration v6, not a separate `cloud_sync.db`.
- **Self-pace (rclone path):** `rclone --bwlimit` (not the in-process token
  bucket, which is the Rust-uploader path only).
- **Unit of work:** child object; parent-event durability (D2).
- **Lease timing:** TUNABLE config; a background renewal thread is required for
  the rclone path (D1).

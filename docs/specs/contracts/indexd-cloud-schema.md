# Contract: indexd cloud-sync persistence (migration v6 + RPCs)

Status: **NOT FROZEN — reconciled design draft with OPEN ITEMS.** Target surface
for **P1** (indexd), consumed by **P3**/**P4**. A twice-run Tier-3 adversarial
review overturned code-level premises in the first draft (see
`files/cloud-p0-review-reconciliation.md`, cycle 2); those are corrected here, and
the exact RPC **wire envelopes remain OPEN** — they are pinned in P1 against the
real `proto.rs`/`server.rs` `status`-tagged framing, not invented here.

> **⚠️ Corrected premises (verified against code):**
> - **Leases are boot-scoped monotonic ms, NOT unix seconds.** The v5 `leases`
>   table is `boot_id TEXT` + `expires_mono_ms INTEGER` (index
>   `idx_leases_expiry(boot_id, expires_mono_ms)`). The target is **RTC-less**;
>   wall-clock epoch expiry is unsafe. Lease RPCs use `ttl_ms` + return
>   `{boot_id, expires_mono_ms}`.
> - **The durable queue keys on a single `ArchiveItemId(i64)`** (`queue.rs`).
>   Per-child rows require a queue-identity change (§2.1) — this schema *defines*
>   the child key; the Rust `QueueItem`/`UploadQueue` types must change to match.
> - **The archive driver is `RecentClips`-only** (`folder_class:"RecentClips"`
>   hardcoded). Sentry/Saved *event* rows are **not produced today** (§4); cloud
>   upload of events is gated on that registration path.
>
> **OPEN ITEMS (pin in P1):** exact request/response JSON per verb incl. the
> `status`-tagged success/error envelope + error codes; cursor encoding + order +
> stability; the manual-retry verb (`cloud_queue_retry`, resolving parked
> collisions — not a plain upsert); collision storage (a differing-hash offer
> cannot share the one PK row with the existing object); `destination_id`
> lifecycle + **byte-exact `remote_key` normalization** (shared by dedup across
> all lanes).

indexd is the **single SQLite writer**. Cloud state consolidates here — **no
separate `cloud_sync.db`**.

---

## 1. Reuse what already exists at v5 — do NOT re-add

Verified in `indexd/src/db/migrations.rs` (`LATEST_VERSION = 5`):

- **`leases`** with `kind IN ('upload','playback')` — the **upload lease already
  exists**. v6 adds only *verbs*, not the table.
- **`archive_items.durable`** — the durability flag. Cloud completion sets it
  (§3, D5); do not add a parallel `uploaded_verified` column.
- **`prefs(key, value)`** — the KV store webd already validates/forwards
  (`set_pref`). **Non-secret** cloud config lives here (§5).
- **`archive_items.id`** (i64) is the `ArchiveItemId` = the **parent event**.

Migration **v6 is additive-only** (forward-only invariant, `migrations.rs`): new
tables + indexes; no destructive change to v1–v5. It must be **idempotent** and
pass the existing up/forward-only migration tests.

## 2. New tables (v6) — DDL hardened (M7)

All columns `NOT NULL` unless a nullable is deliberate; every count/size has a
`CHECK (… >= 0)`; hashes `CHECK(length(hash)=64)` lowercase-hex; enums are
`CHECK(x IN (…))`; text fields length-capped.

### 2.1 `cloud_upload_queue` — one row per **child object** (D2, D7)
```
archive_item_id  INTEGER  -- FK archive_items(id); the PARENT event
child_key        TEXT     -- child discriminator (relative path within the event)
destination_id   TEXT     -- which configured remote (D7 identity component)
remote_key       TEXT     -- canonical destination object key (D7)
category         TEXT     CHECK(category IN ('event_sentry','trip','bulk'))
seq              INTEGER  CHECK(seq >= 0)          -- FIFO tiebreak
total_bytes      INTEGER  CHECK(total_bytes >= 0)
bytes_uploaded   INTEGER  CHECK(bytes_uploaded >= 0 AND bytes_uploaded <= total_bytes)
expected_hash    TEXT     -- backend verification value (algorithm-specific length); NULL until derived
verify_alg       TEXT     CHECK(verify_alg IN ('sha256','md5','crc32c','sha1','quickxor','dropbox','none'))
content_sha256   TEXT     CHECK(length(content_sha256) = 64)  -- always-present local identity (reused from archive copy)
state            TEXT     CHECK(state IN ('queued','in_progress','done','failed','parked'))
attempts         INTEGER  CHECK(attempts >= 0)
not_before       INTEGER  -- unix secs; retry backoff gate (M2), NULL = ready
last_error       TEXT     -- sanitized, capped (D8); NULL when none
PRIMARY KEY (destination_id, remote_key)          -- = the D7 dedup identity
```
`parked` = differing-hash collision on an existing key (D7); needs operator
action, never auto-overwritten.

### 2.2 `cloud_synced_files` — dedup oracle / durability cache (D7)
```
destination_id  TEXT
remote_key      TEXT
archive_item_id INTEGER
child_key       TEXT
content_sha256  TEXT CHECK(length(content_sha256)=64)  -- dedup identity component
verify_alg      TEXT
verify_value    TEXT  -- backend hash actually compared (algorithm-specific length); NULL if none
size_bytes      INTEGER CHECK(size_bytes >= 0)
synced_at       INTEGER
completion_seq  INTEGER CHECK(completion_seq >= 0)  -- monotonic; stats baseline (M1)
PRIMARY KEY (destination_id, remote_key)
```
Identity is `(destination_id, remote_key)` **+ hash/size** match. This table is a
**cache/oracle**, not the source of truth for a *file's* durability — the parent
event's `archive_items.durable` is (set in §3). A same-key/different-hash offer
does not dedup; it parks (§2.1).

### 2.3 `cloud_sync_history` — completed/failed transfer log (M1, M5)
```
id              INTEGER PRIMARY KEY
completion_seq  INTEGER CHECK(completion_seq >= 0)  -- shared monotonic counter (M1)
archive_item_id INTEGER
child_key       TEXT
destination_id  TEXT
outcome         TEXT CHECK(outcome IN ('uploaded','failed'))
size_bytes      INTEGER CHECK(size_bytes >= 0)
at              INTEGER
error_class     TEXT    -- sanitized class only on failure (D8); NULL otherwise
```

### 2.4 `cloud_meta` — cumulative counters + reset baseline (M1)
Single-row (or KV) table holding the **monotonic `completion_seq` allocator** and
the reset **baseline** (`stats_baseline_seq`, `stats_baseline_at`). Cumulative
Synced/Transferred are **derived** as `COUNT(*)/SUM(size_bytes)` over
`cloud_sync_history WHERE outcome='uploaded' AND completion_seq >
stats_baseline_seq` — **not** an incrementable integer (removes the D5
double-count race). "Reset counters" sets `stats_baseline_seq = current seq`.

### 2.5 `cloud_provider_config` — **non-secret** config (secrets: creds store)
Folders enabled, per-folder priority, `reserve_gb`, retry policy
(`max_attempts`, base backoff), toggles (`keep_until_backed_up`, auto-sync).
May be a typed projection of `prefs` keys rather than a new table — P1 decides,
but the **key set is frozen here**. **No credential material** ever lands here.

Indexes: `cloud_upload_queue(state, category, seq)` (ready-work scan),
`cloud_sync_history(completion_seq)`, `cloud_synced_files(archive_item_id)`.

## 3. RPC verbs (framed-JSON over the indexd control socket) — D6

**These do not exist in `proto.rs`/`server.rs` today.** P1 adds each as a
`{"cmd":…}` request with a **`status`-tagged** response envelope (matching the
existing indexd framing — the exact JSON per verb is an **OPEN item** pinned in
P1), mirroring `set_pref`. Frame cap = **64 KiB** (`MAX_FRAME`), 15 s timeout,
non-unix stub → `Unavailable`. **Lease time is boot-scoped monotonic ms**
(`ttl_ms` in, `{boot_id, expires_mono_ms}` out — never epoch); other ages are
unix seconds; all sizes bytes.

| cmd | args | returns | notes |
|---|---|---|---|
| `cloud_candidates` | `folders[]`, `after_cursor?`, `limit` | `{items[], next_cursor?}` | **paginated** (D6); items carry `archive_item_id`, `child_key`, source path, size, **local `content_sha256`** (backend hash derived later by uploadd) |
| `cloud_queue_load` | `after_cursor?`, `limit` | `{items[], next_cursor?}` | **paginated**; resume the durable queue on boot |
| `cloud_queue_upsert` | `item` | `{state}` | idempotent on `(destination_id, remote_key)`; returns `parked` on hash-collision |
| `cloud_queue_retry` | `archive_item_id`, `child_key?`, `resolution` | `{state}` | **manual retry** — resets attempts/backoff and resolves a `parked` collision (`keep_existing`/`rekey`/`replace`); NOT expressible via upsert |
| `upload_lease_acquire` | `archive_item_id`, `ttl_ms` | `{granted, token, boot_id, expires_mono_ms}` | `kind='upload'` on the existing `leases` table |
| `upload_lease_renew` | `token`, `ttl_ms` | `{ok, expires_mono_ms}` | called from uploadd's renewal thread (D1) |
| `upload_lease_release` | `token` | `{ok}` | |
| `cloud_upload_commit` | `queue_pk`, `attempt_id`, `hash`, `hash_alg`, `size` | `{ok, durable_parent}` | **one transaction (D5)**; commit fields **derived from the locked queue row**, keyed by `attempt_id` for idempotency — see §3.1 |
| `cloud_upload_fail` | `queue_pk`, `attempt_id`, `error_class`, `not_before`, `terminal` | `{ok, state}` | **idempotent on `attempt_id`** (a retried fail must not double-count attempts/history) |
| `cloud_stats_get` | — | `{synced_count, synced_bytes, since_at}` | derived vs baseline (M1) |
| `cloud_stats_reset` | — | `{ok, baseline_seq}` | sets baseline = current seq (M1) |
| `cloud_config_get` / `cloud_config_put` | (config) | (config) | non-secret only; validate like `set_pref` |
| `cloud_history_load` | `after_cursor?`, `limit` | `{items[], next_cursor?}` | **paginated**, capped limit (m2) |

There is **no** `cloud_stats_bump` verb (D5 — it was non-idempotent and
double-counted on retries; stats are derived).

### 3.1 `cloud_upload_commit` — the atomic transaction (D5)
In **one SQLite transaction**, idempotent on `(destination_id, remote_key,
hash)`:
1. allocate the next `completion_seq` from `cloud_meta`;
2. upsert `cloud_synced_files` (dedup row + `completion_seq`);
3. insert `cloud_sync_history(outcome='uploaded', completion_seq, …)`;
4. set `cloud_upload_queue.state='done'` for `queue_pk`;
5. **if every child of `archive_item_id` is now `done`**, set
   `archive_items.durable = 1` — else leave it 0.
Re-running with the same key is a no-op returning the prior result (crash-safe).
`durable_parent` in the reply tells uploadd/retentiond the event is fully backed
up.

## 4. Folder → category map — **based on real `folder_class`, gated** (M6)

Maps the **registered** `folder_class` (indexd `FolderClass`: `RecentClips`,
`SavedClips`, `SentryClips`, …) → `UploadCategory`. **Reality check:** the
Phase-1 archive driver **only ever registers `RecentClips`**
(`archive_driver.rs` hardcodes it), so today only the `RecentClips` row is
reachable. `SavedClips`/`SentryClips` are valid `FolderClass` values indexd can
*parse*, but **no archive rows are produced for them yet** — those rows depend on
the event-registration prerequisite (uploadd.md §2). `ArchivedClips` is a
retentiond **destination** concept, **not** a source `folder_class`, so it is
**not** a candidate key.

| registered `folder_class` | category | available today? |
|---|---|---|
| SentryClips | `event_sentry` | **no** — needs event registration |
| SavedClips  | `event_sentry` | **no** — needs event registration |
| RecentClips | `bulk` (or per-config) | **yes** (Phase-1) |

`cloud_auto_delete_old` semantics are **deferred** (M6): they conflict with the
"no unrequested delete" retention invariant and are out of P1 scope.

## 5. Retention coupling (P5)
`retentiond`'s `cloud_policy_satisfied` reads `archive_items.durable` (already the
gate model). v6 changes nothing structural — P5 wires the read + reconciles the
v1 `keep_until_backed_up` toggle into `cloud_provider_config`. No eviction-policy
redesign.

## 6. Tests (P1 acceptance)
Migration v5→v6 up + **idempotent re-run** + forward-only guard; every RPC
round-trip; `cloud_upload_commit` **idempotency** (double-commit = single history
row, `durable` flips only on last child) + **partial-children** (parent stays 0);
dedup match vs same-key-different-hash **park**; pagination cursor stability under
concurrent inserts; stats derivation vs a mid-stream `cloud_stats_reset`; DDL
`CHECK` rejections (negative size, `bytes_uploaded > total`, bad hash length, bad
enum).

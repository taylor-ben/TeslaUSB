# Event-archive registration (Sentry / Saved / TrackMode)

> **STATUS: NOT FROZEN — reconciled design draft (v2) with OPEN ITEMS.**
> Grounds cloud-sync **P0.5**, the foundation gate for uploading Sentry/Saved
> *events* (operator chose **option 1 = full v1 parity**). v2 folds in a GPT-5.5
> adversarial review that verified every claim against source and found **8 blocking
> errors in v1** — corrected below with file:line evidence. It remains a **design
> draft**, not a frozen contract: each OPEN ITEM (§6) closes in-phase against the
> compiler. Do **not** re-freeze.
>
> **Tier 3** (recording-critical retention daemon). GPT-5.3-codex implements; GPT-5.5
> reviews. The §5 architecture (event arm + finalization protocol) still needs a
> confirming GPT-5.5 pass on the concrete diff before it lands.

## 1. Why this gate exists (CORRECTED)

Cloud sync must upload **event** clips, not just the RecentClips ring. Every
downstream cloud phase keys off **`archive_items`** rows.

**v1 error (corrected):** `archive_items.durable` does **not** mean "verified local
copy." It means a **durable OFF-DEVICE (cloud) copy exists** —
`durability.rs:64-70`: *"uploadd flips this to Durable on a remotely-verified
upload; it is the gate for evicting the local archive copy."* Registration writes
`durable=0` (`indexd/db/ingest.rs:271-283,328-344`); only a remotely-verified upload
sets `durable=1` (`indexd/db/mutations.rs:734-746`; `uploadd/durability.rs`).

So P0.5's real target is a **local-verified `archive_items` row with `durable=0`**
for each event — exactly what the RecentClips path already produces via
`register_archived_clip`. Cloud upload (P3+) later flips `durable=1`. **No
durability-contract migration is needed** (the reviewer's suggested migration
over-reaches; the existing semantics are fine once we target `durable=0`).

**Today no Sentry/Saved/TrackMode event becomes any `archive_items` row at all**, so
event upload has nothing to enqueue. P0.5 closes that gap by producing the local
(`durable=0`) archive row.

## 2. Tesla on-disk event layout (the input)

```
TeslaCam/
  RecentClips/                       # flat ring — already archived (see §3)
    2026-06-19_10-00-00-front.mp4 ...
  SavedClips/<event-ts>/             # one directory per event
    2026-06-19_10-00-00-front.mp4    # per-camera, per-minute SEGMENTS (many)
    2026-06-19_10-01-00-front.mp4    # a multi-minute event = MANY timestamps
    event.json                       # folder-level metadata sidecar
    thumb.png                        # folder-level thumbnail
  SentryClips/<event-ts>/            # same shape
  TeslaTrackMode/<event-ts>/         # same shape; event.json NOT parsed today (§6)
```

An event folder is **not** one clip. Scanner already splits it into N per-timestamp
segment-clips (each with camera angles) plus two folder-level sidecars.

## 3. Verified current state (file:line)

### 3.1 What already works
- **scannerd catalogs events (read-only).** The exFAT walk recurses all subdirs
  (`scannerd/walk.rs:80-175`); `Bucket::{SavedClips,SentryClips,TeslaTrackMode}`
  lands each segment as a `clips` row (`folder_class` set) with per-camera `angles`
  (`view_kind='ro_usb'`) (`scannerd/produce.rs:111-124,682-735`; grouped/upserted at
  `indexd/apply.rs:330-419`). `event.json` is parsed into the **`clip_events`** table
  (indexd v3), keyed by `event_dir_key`.
  **Caveat:** `clip_events` is *ephemeral car inventory* — it is **pruned when the
  folder disappears from the car** (`indexd/apply.rs:580-590`;
  `indexd/db/ingest.rs:743-773`). It is **not** durable archive metadata.
- **indexd registration accepts event folder classes.** `indexd/server.rs`
  `parse_folder_class` (~378-386) accepts `RecentClips|SavedClips|SentryClips|
  TeslaTrackMode`, rejects `ArchivedClips`; `handle_register_archived_clip` writes
  `archive_items`+`clips`(+`angles`) with `durable=0`.
- **Storage is N:1, not 1:1.** `archive_items.path` is UNIQUE and
  `archive_item_clips` is **M:N** (`indexd/db/migrations.rs:261-300`). Repeated
  registrations for different segment keys sharing one archive path upsert **one**
  item and append links (`indexd/db/ingest.rs:328-364`). (But each call resets
  `durable=0`, rewrites the legacy `clip_id`, and never removes stale links — which is
  why **events** use the dedicated `finalize_event_archive` RPC (§5.2), not this
  per-call accumulation; `register_archived_clip` stays as-is for RecentClips.)
- **The event-archive policy core exists + is unit-tested.** `serve.rs`
  `archive_event_folder`(554, **`pub`**) → `decide_event_archive`(`archive.rs:313`)
  → `run_verified_pass`(`archive.rs:184`, copy-then-verify) →
  `catalog.record_verified_pass`(586). `folder.rs` models Saved/Sentry/Track as
  event folders. `manifest.rs::DirManifest`, `archive.rs::VerifiedArchivePass`
  present.

### 3.2 What is missing — UNWIRED, plus retired live seams (CORRECTED/EXPANDED)
- `run_cycle` has **no non-test caller** (sole call `serve.rs:2393`, in the test
  module from :1029). Production `main.rs::run_serve` builds the RecentClips-only
  source+driver (`main.rs:330,594-603`) and uses `RetentionLoop` only for
  `recover`/`drain_to_target` (`main.rs:510-516,669-675`). **Confirmed by review.**
- **Four live seams the event path needs are STUBBED `Unsupported`:**
  - `LiveCatalog::record_verified_pass` (`live.rs:419-428`) — can't record a pass.
  - `LiveArchiveStore::source_identity` (`live.rs:280-285`) and
    `list_source_rel_names` (`live.rs:287-292`) — **"direct source probing is
    retired; inventory comes from indexd SQLite candidates."**
  - `LiveCatalog::mark_recent_archived` (`live.rs:512-517`) — Recent arm can't run.
  ⇒ Merely wiring `run_cycle` + replacing the catalog stub still fails live, AND the
  intended source model is **indexd catalog candidates, not re-reading the volume.**
- **Production car-delete does not exist.** `NoCarHandoff` always `Refused`
  (`main.rs:225-233`). gadgetd `Mutation::DeletePath` carries only `rel_path`, no
  digest to revalidate (`gadgetd/handoff.rs:58-77`). The governor samples the **Pi
  archive root** (`main.rs:531`), not the car-visible volume that
  `EventArchiveContext.car_volume_pressured` (`archive.rs:280-286`) needs.

### 3.3 Overturned recommendation (doubt-driven RECONCILE)
The `event-archive-map` investigation recommended minting `archive_items` from the
**scan-ingest** path with "retentiond NO CHANGE NEEDED" — **rejected**: an
`archive_items` row must be backed by a verified physical copy in the Pi archive
root; a scan copies nothing. The fix is completing the retentiond event-archive
wiring (§5).

## 4. Goal & non-goals (CORRECTED)

**Goal:** in production, a stable Sentry/Saved (optionally TrackMode) event folder is
copied to the Pi archive root, verified (copy-then-verify), and registered so indexd
holds a **local (`durable=0`) `archive_items`** row for the event — its camera
segments linked as `clips`/`angles`, its sidecars (`event.json`/`thumb.png`)
physically archived and their metadata persisted durably — enabling cloud upload
(P3+) to enqueue and later flip `durable=1`.

**Non-goals (explicitly OUT of P0.5):**
- **Car-side deletion under pressure.** Not production-capable today (§3.2). P0.5
  archives + registers only; it never deletes from the car. A car-volume pressure
  source + a digest-revalidating gadgetd protocol are **separate future Tier-3
  work**. `cloud_policy_satisfied`/`car_volume_pressured` are held so no delete is
  ever requested (§6).
- Cloud upload itself, dedup oracle, history, UI (P1/P3/P4).
- Changing RecentClips archiving (must stay byte-for-byte identical).

## 5. Design — add an event arm sourced from indexd candidates

Adopt the reviewer's **third architecture option** (A and B are both unsound —
§5.3). Three parts; the finalization contract (§5.2) is the load-bearing OPEN work.

### 5.1 Event candidate source = indexd catalog, NOT volume re-read
Because direct source probing is retired (§3.2), the event producer must build each
event's inventory from **indexd** (scanner-populated `clips`/`angles`/`clip_events`
for `folder_class IN (SavedClips,SentryClips[,TeslaTrackMode])`), not by descending
the volume image like `VolumeCandidateSource`. It yields, per candidate event
folder, the segment set (canonical keys, partition, timestamps, camera angle
`file_ref`s) + the sidecar list + source-volume identity. Add the indexd read
RPC(s) needed to enumerate event folders and their segments/sidecars.

### 5.2 Verified copy + a realizable registration/finalization contract  **[OPEN-1 RESOLVED]**
`record_verified_pass(folder_key, {id,digest,bytes})` (`serve.rs:100-106`) structurally
**cannot** build an `ArchiveRegistration` (which needs canonical_key, partition,
timestamps, duration, angle paths — `register_client.rs:18-35`). So the event arm
does not rely on that callback to register, and it does **not** reuse the published
per-segment `register_archived_clip` path for events (that path makes partial rows/
angles observable pre-finalize, re-clobbers aggregates on replay, and resets `durable`
— unsafe for an atomic event; it stays **unchanged** for RecentClips only). Instead:

**A. A new `finalize_event_archive` RPC — one atomic, idempotent, digest-bound
transaction** registering the whole verified event as **one** `archive_items` row.
`register_archived_clip` is untouched.

**B. Physical publication (BEFORE the DB tx).** The event arm publishes the verified
copy into a **fresh, pass-specific immutable generation directory** and verifies its
**exact** destination file set. Reason: `run_verified_pass` (`archive.rs:195-261`)
re-checks the *source* set/identities but **never** inspects the destination for stale
extras, so reusing a prior dest can leave orphan files that inflate the true
`file_count`/`size_bytes`. A pre-commit crash then leaves only an orphan dir (GC'd
later); the tx switches `archive_items.path` to the new generation dir, cleanly
swapping generations.

**C. The finalize frame** (single, bounded — see frame-safety below) carries:
`pass_id`, `source_event_key`, `source_generation` (opaque composite, incl. boot
identity — see §5.4), `source_volume_id?` (nullable until propagated — §6 deferred),
`manifest_digest`, `segment_set_digest`, `expected_segment_count`, `size_bytes`,
`file_count`, `archived_at`, `generation_dir_path`, sidecar facts
(`has_event_json`/`has_geo`/`event_severity?`), and the authoritative **video-segment
clip identity set** (canonical keys). Sidecars (`event.json`/`thumb`) are **not** clips:
they count toward `size_bytes`/`file_count` (whole-manifest totals) but are **not**
linked in `archive_item_clips`.

**D. The finalize transaction** (single SQLite tx; readers see only the old or the new
complete generation, never a partial one):
1. Resolve the row by **source-event identity** `(source_volume_id, source_event_key)`
   (partial UNIQUE — §6/v7), not path alone.
2. Validate the supplied set is self-consistent: `count == expected_segment_count`,
   recomputed `segment_set_digest` matches, every key has the event-folder prefix,
   partition/`folder_class` match, no duplicate camera per segment.
3. Cross-check `size_bytes == VerifiedArchivePass.bytes` and
   `file_count == manifest.len()` (both whole-manifest totals incl. sidecars).
4. **Idempotent replay:** if the stored `manifest_digest == incoming` **and**
   `verified_pass_id` is set → recompute/compare aggregates + link-set, return the
   existing id, **write nothing** (crucially, do **not** touch `durable` — uploadd may
   have set it since).
5. **Stale rejection:** a stored row whose `source_generation`/`manifest_digest`
   indicates a **newer** observation than the incoming pass → reject (the pass is
   stale; e.g. the event expanded after verification).
6. **New generation** (new/changed digest): reject if a delete lease / cloud op is
   active; set `durable = 0` (a new local generation has no off-device copy — §5.4);
   write aggregates + sidecar flags + the full verification tuple
   (`manifest_digest`, `verified_pass_id`, `source_generation`, `source_event_key`,
   `source_volume_id?`, `segment_set_digest`); **replace links atomically** (delete
   *this* item's `archive_item_clips`, insert exactly the supplied segment clip set);
   reconcile archive angles for pruned links; switch `path` to `generation_dir_path`.
7. Commit; return the item id.

Frame safety: one archive_items row = **one event folder** (one trigger's timestamped
subfolder), whose child set is provably bounded (~10-min window, TrackMode ≤ ~1 hr,
≤ 6 cameras ⇒ tens to a few hundred files ≈ 6–36 KiB of keys, well under
`MAX_REQUEST_FRAME`=64 KiB — `register_client.rs:13-14`, `indexd/proto.rs:10-11`). The
caller supplies the set in **one** finalize frame — this decouples finalize from the
live catalog (robust to card-pull) without a staging state machine. **Fail-safe:** if
a set would exceed the frame budget, finalize **rejects** with a distinct "event too
large — chunked staging not implemented" error (fail-closed: the archive copy exists
and is safe; it is simply unregistered until chunked staging ships — OPEN-4). Chunked
staging, if ever needed, slots behind the same RPC name; not observed in real
TeslaCam data.

### 5.3 Wire an event-only arm into the daemon (reviewer's third option)
- **Keep the production RecentClips driver unchanged** (its staging/probe/promote/
  outbox/register flow — `archive_driver.rs:184-320,370-475` — and the single
  governor `drain_to_target`).
- **Add an event-only arm** that, each cycle, pulls event candidates (§5.1), runs the
  verified copy, and registers/finalizes (§5.2) — reusing the public
  `archive_event_folder` policy where it fits, but with a **production
  `ArchiveStore`/catalog seam** (the retired stubs replaced only for the paths the
  event arm actually uses).
- **Rejected — Option A** (adopt `run_cycle` in `main.rs`): its Recent arm calls the
  **stubbed** `mark_recent_archived` (`live.rs:512-517`) and lacks the production
  driver's staging/promote/outbox, so it would **break RecentClips**; `run_cycle`
  also governs internally, double-counting the retained drain.
- **Rejected — Option B** (extend the phase-1 driver): `Candidate` is one camera
  clip with no sidecars/event manifest (`candidates.rs:11-47`) and the driver
  hardcodes `RecentClips` (`archive_driver.rs:383-397,452-466`) — not a simple new
  source.

### 5.4 Event finality & digest binding (prevents deleting/registering a partial event)
`ManifestTracker` requires only consecutive identical observations — **no
time-quiescence** (`manifest.rs:139-197`); the 60 s quiescence is the *separate*
RecentClips per-file tracker (`volume_source.rs:19-22`). And `ArchiveVerification`
stores only a random pass **id, not the digest** (`durability.rs:32-61`). So a "has
any recorded pass?" check would treat a **later-expanded** event as verified. The
event arm must persist `(source generation, event folder key, exact manifest digest,
pass)` and **invalidate verification on any digest change**, registering only against
the persisted verified digest. (Since P0.5 does no car-delete, the danger here is
registering a *partial/incomplete* event, not deleting one — still must be
prevented.)

**Two independent axes — do not conflate** (`durability.rs:1-22,64-75`):
`ArchiveVerification` (Pi-side copy trustworthy; the `verified_pass_id` this contract
writes) is **separate** from `Durability` (`archive_items.durable` = a durable
**off-device** copy, flipped by **uploadd** on remote verify). So finalize records
*verification*, sets `durable = 0` on a new generation, and **never** rewrites
`durable` on idempotent replay. `source_generation` is an **opaque composite** that
includes boot identity, because the scanner generation counter resets to 0 every
process/reboot (`indexd/main.rs:315,351-353,374`) and a naked `u64` would alias a
pre-reboot pass as current. The verified tuple is persisted on `archive_items` via
**migration v7** (§6/OPEN-1).

## 6. OPEN ITEMS (close in-phase — do NOT pre-freeze)

> **STATUS (cloud lane B1):** the migration-v7 schema and the `finalize_event_archive`
> RPC (item 1) are now **FROZEN and being implemented** as the *shared* v7 alongside the
> cloud sealed-upload-set protocol — authority `docs/specs/contracts/indexd-cloud-schema.md`
> §7 (one v7, no competing migration). B1 builds v7 + `finalize_event_archive` +
> `cloud_prepare/finalize_parent_upload`, **fully tested via fixtures that call
> `finalize_event_archive` directly**. The **production event-arm daemon wiring** that
> actually calls `finalize_event_archive` in-flight (the `volume_serial`→`ScanBatch`
> propagation in item 2, sidecar persistence item 3, TrackMode item 5, the
> enumeration/`ArchiveStore` seams items 6–7) remains **this spec's separate P0.5 lane —
> NOT built under B1**. Until it lands, event `archive_items` carry `manifest_digest=NULL`,
> so cloud `prepare` fails closed and no event is cloud-evictable in production (intended).

1. **Registration/finalization contract (the core OPEN).**  **[RESOLVED — §5.2.]**
   Decision: a **new `finalize_event_archive` RPC** (NOT extending
   `register_archived_clip`, which stays unchanged for RecentClips); one atomic,
   idempotent, digest-bound tx; a **single bounded** frame carrying the caller-supplied
   verified segment set (no staging state machine — the per-event child set is bounded;
   oversize fails closed → chunked staging deferred to OPEN-4). Persist via **migration
   v7** (additive/forward-only/idempotent, like v6): nullable `archive_items` columns
   `manifest_digest`, `verified_pass_id`, `source_generation`, `source_event_key`,
   `source_volume_id`, `segment_set_digest`, plus a **partial UNIQUE** over
   `(source_volume_id, source_event_key) WHERE source_event_key IS NOT NULL` (legacy/
   Recent rows exempt). `has_event_json`/`has_geo` already exist — finalize is their
   **sole writer**. `durable` is **off-device** (uploadd-owned): finalize sets `0` on a
   new generation and never rewrites it on replay (§5.4).
2. **Idempotency across reboots (RTC-less, boot_id leases).**  **[MOSTLY RESOLVED —
   §5.2/§5.4.]** Registration binds to `(source_volume_id, source_event_key)` +
   `manifest_digest`; the link set is replaced transactionally; the event arm
   stages/promotes an **exact immutable generation dir** and verifies its dest set
   (closing the `run_verified_pass` stale-dest-extra hole). `source_generation` is an
   opaque composite incl. boot identity (naked counter aliases across reboot). **Still
   open (wiring dep):** `volume_serial` exists at scan time (`boot.rs:50`) but is **not**
   propagated into `ScanBatch` (`record.rs:336-383`), so `source_volume_id` is NULL
   until the scan wire protocol carries it; until then event identity = `source_event_key`
   alone (single-recording-volume appliance ⇒ low collision risk).
3. **Sidecar persistence.** Persist archived `event.json`/`thumb.png` identity +
   parsed metadata **at finalization** (do NOT depend on ephemeral `clip_events`,
   which is pruned when the car folder disappears).
4. **Frame-bounded protocol** (§5.2): **[RESOLVED to a single bounded finalize frame.]**
   The per-event child set is bounded (one trigger's subfolder), so the caller supplies
   it in one 64 KiB-safe frame; oversize **fails closed** with a distinct error. Chunked
   staging (begin/stage/finalize) is deferred here — add behind the same RPC name only
   if real data ever exceeds the frame (not observed). Crash-recovery + frame-boundary
   tests still required (§7).
5. **TeslaTrackMode `event.json`.** `scannerd/produce.rs:282-296` collects sidecars
   only for Saved/Sentry. Decide if TrackMode parity is in P0.5; if yes, open the gate.
6. **Indexd event-enumeration read RPC(s)** (§5.1) — exact shape.
7. **Production `ArchiveStore` seam for the event arm** — which retired stubs
   (`source_identity`/`list_source_rel_names`) must be replaced vs. designed around
   the indexd-candidate inventory (§3.2 says inventory should come from indexd, so
   the event arm may avoid source probing entirely — confirm).

**Resolved by code inspection (were wrongly "open" in v1):**
- Keys are `{numeric-slot}:{parent}/{segment-ts}` for segments and
  `{numeric-slot}:{parent}` for event folders; partition = `slot{slot}`
  (`scannerd/produce.rs:111-124,136-140,299-301`). i.e. `0:TeslaCam/...`, not
  `slot0:...`. Match ingest exactly — not a design choice.
- Sidecars are physically archived (verified pass copies all manifest entries,
  unfiltered) **iff the §5.1 producer includes them in the manifest**.

## 7. Test plan (goal-driven — write first; NO car-delete)

- **Unit (host, container):** event candidate source builds the expected segment +
  sidecar inventory for a multi-camera, multi-segment `SentryClips/<ts>/` from indexd
  fixtures; `DirManifest` digest stability across identical scans; **finality** —
  a folder that gains a later segment invalidates a prior verification (§5.4).
- **Registration contract:** the exact frames emitted for an N-segment event stay
  under 64 KiB; finalization yields **one** `archive_items` row (`durable=0`,
  correct `folder_class`, `has_event_json`/`has_geo` set), linked to the N segment
  `clips`; re-run is idempotent (no dup item, no stale links) bound to volume
  identity + digest. Plus:
  - **Idempotent replay preserves `durable`:** finalize, flip `durable=1` (simulated
    uploadd), replay the same pass → row unchanged, **`durable` stays 1** (finalize
    must not reset it).
  - **Stale rejection:** an event that gains a segment (new `manifest_digest`) between
    verification and finalize → the stale pass is **rejected**, not registered.
  - **New generation prunes:** a larger prior generation → links **and** stale archive
    angles for dropped segments are removed; `path` switches to the new generation dir.
  - **Concurrent reader:** a reader during finalize observes only the old-complete or
    new-complete row — never partial aggregates/links (single-tx atomicity).
  - **Oversize fail-closed:** a set exceeding the frame budget → distinct
    "event too large" error, **no** partial row written.
  - **Generation alias after reboot:** a pre-reboot `source_generation` (counter reset
    to 0) does **not** alias as current — the opaque composite (boot identity) rejects it.
- **Migration v7:** `v6→v7` upgrade and fresh-schema both yield the nullable columns +
  partial UNIQUE; legacy/Recent rows (NULL `source_event_key`) are exempt from the
  event-identity constraint; re-running the migration is idempotent.
- **Regression:** RecentClips archiving unchanged (byte-for-byte) — the event arm is
  additive; the Recent driver + single governor drain are untouched.
- **Hardware (hardware-test skill):** a real Sentry event on `cybertruckusb.local`
  yields a `durable=0` event `archive_items` row visible to the archive read path.
  **No car-delete assertion** (out of scope).

## 8. Delegation

Implementation → a `gpt-5.3-codex` background lane with: this doc, the charter, the
§7 acceptance tests, and the resolved OPEN-1 finalization contract as its binding
input. Opus reconciles the diff (builds/tests/reads it) and routes the GPT-5.5
Tier-3 review. OPEN ITEMS close against the compiler in-phase — not pre-frozen here.

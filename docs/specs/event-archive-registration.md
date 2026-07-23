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
  `durable=0`, rewrites the legacy `clip_id`, and never removes stale links — see
  OPEN-1.)
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

### 5.2 Verified copy + a realizable registration/finalization contract
`record_verified_pass(folder_key, {id,digest,bytes})` (`serve.rs:100-106`) structurally
**cannot** build an `ArchiveRegistration` (which needs canonical_key, partition,
timestamps, duration, angle paths — `register_client.rs:18-35`). So the event arm
does not rely on that callback to register. Instead (design intent, OPEN-1/§6):
1. Stage → `run_verified_pass` copies **every** manifest entry (camera segments +
   sidecars) to the Pi archive dest and dest-hashes them (`archive.rs:195-261` copies
   without extension filtering — sidecars are archived, confirmed).
2. Register each **segment** via the existing per-clip `register_archived_clip`
   (folder_class=Sentry/Saved, `durable=0`), sharing one event **archive path** so
   `archive_item_clips` accumulates the N segment links onto **one** event
   `archive_items` row (N:1 mechanism, §3.1).
3. **Atomically finalize** the event archive_item bound to `(source-volume identity,
   event folder key, exact manifest digest)` — persisting sidecar metadata
   (`has_event_json`/`has_geo`/`event.json` facts) and reconciling the link set
   (removing stale links). The exact finalize verb (extend `register_archived_clip`
   vs. a new `finalize_event_archive` RPC) is **OPEN-1**.

Frame safety: a single whole-event payload can exceed `MAX_REQUEST_FRAME`=64 KiB
(`register_client.rs:13-14`; `indexd/proto.rs:10-11`) for events with hundreds of
segments/angles → the protocol **must** be incremental (per-segment register +
bounded finalize), never one giant frame (OPEN-4).

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

## 6. OPEN ITEMS (close in-phase — do NOT pre-freeze)

1. **Registration/finalization contract (the core OPEN).** Reconcile the per-segment
   `register_archived_clip` (resets `durable=0`, rewrites legacy `clip_id`, never
   prunes stale `archive_item_clips` links) with an event = N segments + sidecars.
   Choose: extend the existing RPC vs. add `finalize_event_archive`. Must be atomic,
   digest-bound, idempotent, and persist sidecar metadata (`has_event_json`/`has_geo`
   — which `ArchiveRegistration` does **not** carry today and the SQL does **not**
   write, `register_client.rs:18-48`, `indexd/db/ingest.rs:328-356`).
2. **Idempotency across reboots (RTC-less, boot_id leases).** Event `FolderFact`
   lacks the volume serial + source fingerprint the Recent path carries
   (`candidates.rs:41-44`; `archive_driver.rs:129-140`). Bind registration to
   source-volume identity + manifest digest; replace the link set transactionally;
   stage/promote an exact directory generation (or verify the dest file set — a
   stale extra dest file is not caught by `run_verified_pass` today).
3. **Sidecar persistence.** Persist archived `event.json`/`thumb.png` identity +
   parsed metadata **at finalization** (do NOT depend on ephemeral `clip_events`,
   which is pruned when the car folder disappears).
4. **Frame-bounded protocol** (§5.2): incremental per-segment + finalize, with
   crash-recovery + frame-boundary tests.
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
  identity + digest.
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

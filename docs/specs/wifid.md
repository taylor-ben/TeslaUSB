# SPEC — `wifid` (STA/AP state machine + SDIO chip-reset watchdog)

> Parent: [`SPEC.md`](./SPEC.md) · Criticality: disposable (but reliability-sensitive)
> Language: Rust · Reference: `wifi_service.py`, `wifi_hostapd.py`, cloud_archive `wifi.py`.

## 1. Objective

Provide WiFi connectivity for cloud upload and the local UI **without ever
endangering the car's write path**. WiFi is **convenience, not a reliability
dependency**: it must never reboot the Pi while the car is writing, and must
avoid the BCM43436 SDIO deadlock.

## 2. Responsibilities

1. **STA/AP state machine:** connect to home WiFi (STA) when reachable; fall back
   to **AP mode** (hostapd + a DHCP server, e.g. dnsmasq) for onboarding/management
   when home WiFi is unreachable. **Auto mode is STA↔AP mutually exclusive** (the safe
   default); an **opt-in Force-on mode runs a concurrent AP+STA** (§7.3 — operator-revised,
   hardware-proven 2026-07-17). The AP must never endanger recording.
   "Reachable" = associated **and** carrier/IP up **and** a cheap reachability
   probe (gateway ping / DNS) succeeds within a timeout — not mere association;
   debounce flaps before switching mode. The **AP must be WPA2** (never open).
2. **Credential storage:** STA PSK and AP passphrase are owned by `wifid` and
   persisted **root-only (`0600`)** (reference: `wifi_service.py`,
   `wifi_hostapd.py`); never world-readable, never logged, never surfaced to the
   SPA. `webd` requests changes via IPC; it does not read the secrets.
3. **TX rate limiting:** enforce a token-bucket / `tc` TX cap (coordinated with
   `uploadd`) to stay **under the SDIO-deadlock threshold** (exact Mbps/chunk
   size from prototype unknown #4).
4. **Liveness watchdog:** detect a wedged chip and recover by **resetting the WiFi
   chip only** (`rmmod/modprobe brcmfmac`) — **not** the whole Pi. A full Pi
   reboot is permitted **only if USB is already idle** (car not writing), and
   even then is a last resort. This is the **single sanctioned non-`gadgetd`
   reboot** recorded in [`SPEC.md` §2 invariant 4](./SPEC.md).
5. **AP onboarding** integration with the captive portal (`webd` `/portal`):
   serve the portal over the AP's DHCP/DNS so a joining phone is redirected to it.
6. **Expose status** (mode, link, signal, throttle state) to `webd`.

## 3. Non-responsibilities

- Does not perform uploads (that is `uploadd`; `wifid` only provides/limits the
  link).
- Does not own cloud config.
- Must never take an action that resets/reboots while the car is writing.

## 4. Acceptance criteria

- [ ] Auto mode cleanly switches STA↔AP (mutually exclusive); opt-in Force-on runs a stable
      concurrent AP+STA without disrupting recording (§7.3).
- [ ] TX stays under the measured SDIO-deadlock threshold under sustained upload.
- [ ] A wedged chip recovers via `rmmod/modprobe` without rebooting the Pi.
- [ ] Never reboots while USB/car write activity is present (verified against the
      `gadgetd` write heartbeat).
- [ ] Runs within `MemoryMax`.

## 5. Testing

- State-machine tests (STA↔AP transitions; mutual exclusion).
- Throttle test (token bucket caps sustained TX at the configured rate).
- Recovery test (simulated wedge → chip reset path chosen, not Pi reboot;
  reboot path gated on USB-idle).

## 6. Boundaries

**ALWAYS** treat WiFi as non-critical; prefer chip reset over Pi reboot; keep TX
under the deadlock threshold; gate any reboot on USB-idle.
**ASK FIRST** before changing the throttle threshold or the recovery escalation
policy.
**NEVER** let the AP disrupt recording or the write path (§7.3); never reboot the Pi while
the car is writing; never let WiFi recovery endanger the write path.

## 7. Network management & `webd` integration (revised 2026-07-15)

> Reconciles the spec with the device reality: on the shipped image **NetworkManager**
> (seeded by **netplan**) owns `wlan0` and the home-STA profile (`netplan-wlan0-<ssid>`),
> **not** `wifid`. `wifid` runs as the AP-safety / throttle / watchdog arbiter and only
> brings pre-provisioned profiles up/down — it deliberately never *creates* a profile and
> never passes a PSK on a command line (see `src/nmcli.rs` module docs). The §4.13
> scan / saved-list / join / forget surface therefore matches NM's model, not `wifid`'s
> single-PSK model. (Second-opinion reconciled with GPT-5.5; see `files/hw-results.md`.)

### 7.1 Authority
- **NetworkManager is authoritative** for STA saved profiles, scan results, and the active
  connection. UI-created networks live in NM's root-only connection store — **not** written
  to netplan and **not** stored in `wifid`'s credential file.
- **`wifid` owns** the AP passphrase, the TX throttle, the SDIO watchdog, and STA↔AP
  arbitration (Auto = mutually exclusive; Force-on = concurrent, §7.3). For Phase B it must treat *any* active NM Wi-Fi
  connection on the Wi-Fi interface as "STA is up", not only its configured `sta_profile`
  name (today it matches only `teslausb-sta` + its own cred store, so it does not recognise
  the live netplan STA — this must be reconciled before any mutation ships).
- **`webd` never reads secrets.** Join passphrases are one-way writes (SPA → webd → NM);
  never read back, logged, or surfaced in status.

### 7.2 `webd` `/api/wifi/*` contract
- **Phase A (read-only — no lock-out risk):**
  - `GET /api/wifi/status` → `{ connected, ssid, signal, security, ip, iface }` (active connection).
  - `GET /api/wifi/networks` → `{ networks: [{ ssid, signal, security, saved, active }] }`
    (merged scan + saved; externally-owned `netplan-*` profiles flagged so Phase B can protect them).
  - `POST /api/wifi/scan` → trigger a **rate-limited** NM rescan (safe while associated), return the refreshed list.
  - Implemented by read-only `nmcli` shell-out in `webd` (`src/wifi.rs`) with tolerant pure
    parsers + unit tests, mirroring `wifid/src/nmcli.rs`; **`wifid` untouched in Phase A**.
- **Phase B (mutating — join / disconnect / forget):** guarded, and gated on a `wifid`
  **management lease** so a mutation cannot race AP-fallback. Requires: POST-only +
  same-origin/CSRF protection; never mutate the active / `netplan-*` profile in place;
  snapshot the known-good profile and auto-rollback (NM checkpoint) on failure or
  unconfirmed reachability; typed confirmation to forget/disconnect the active network;
  PSKs set via a root-only NM keyfile / D-Bus, **never** an `nmcli … password …` argv.
  *(Full contract finalised when Phase B is implemented.)*
- **Phase C:** setup AP + captive portal (Apple/Android/Windows/generic) + auto-restore timer.

### 7.3 Concurrent AP+STA (revised 2026-07-17 — supersedes the "never concurrent" invariant)

> The original spec (§1, §4, §6, §7.1) said **never run AP and STA concurrently**. The operator
> revised this: on the single-radio Pi Zero 2 W the AP must be reachable **even while home Wi-Fi
> (STA) is up** ("AP reachable even on home Wi-Fi", the v1 `uap0` virtual-interface model).
> Concurrency is now **hardware-proven** and shipped. **Auto** mode stays STA↔AP mutually
> exclusive as the safe default; concurrency is strictly **opt-in (Force-on)**.

**Mode semantics**
- **Auto (default):** mutex fallback — the AP appears **only** when the STA can't reach any
  configured SSID. Never concurrent. (Preserves the original mutual-exclusion safety.)
- **Force-on:** concurrent AP+STA — a virtual AP interface (`uap0`) runs hostapd + dnsmasq
  alongside the live STA (`wlan0`) on the one radio. WPA2 always, never open.
- **Force-off:** AP never up.

**Load-bearing invariant (preserved): the AP must never endanger recording.** When the AP is
concurrently active, `wifid` caps `uap0` TX (`tc`) and folds `ap_active` into the SDIO-deadlock
budget so aggregate radio TX stays under the deadlock threshold; if the cap can't apply, uploads
pause (fail-safe). Uploads are additionally throttled (`ap_concurrent`) while the AP is up.

**Hardware stability barriers — all four are required on the BCM43430/43436 FullMAC chip
(empirically proven 2026-07-17; see `files/hw-results.md`). Do NOT remove any of them:**
1. **Power-save OFF** on `wlan0` (and `uap0`), re-asserted every desired tick. STA power-save
   sleeps the shared radio, so the AP vif never hears client auth/assoc frames → clients cannot
   associate (zero MLME, `num_sta=0`).
2. **Split-tick settle** — create + up + NM-unmanage `uap0` on tick N; fire `hostapd -B`
   START_AP on tick N+1 (~2 s later). START_AP within <~2 s of vif-up fails firmware beacon-set
   (`-52`) and never reaches `AP-ENABLED`.
3. **Race-free IP-flush recovery** — hostapd's START_AP mode-transition flushes `uap0`'s IPv4.
   `wifid` re-asserts the IP **only** (no teardown) on a later tick when
   `type AP && hostapd && iface && !ip_present`, and starts dnsmasq **separately** only after the
   AP has been `active` for **two consecutive ticks** — so dnsmasq never binds during the flush
   window. Without this, dnsmasq dies ("unknown interface uap0") and the AP churns
   teardown/recreate on the shared radio.
4. **NM-unmanage `uap0`** so NetworkManager doesn't fight hostapd for the new vif.

**Proof (2026-07-17):** concurrent Force-on AP `TeslaUSB-Setup` on `uap0` / `192.168.4.1` with an
external client (DHCP lease `192.168.4.32`, gateway ping 3/3, `client_count:1`) held ~170 s while
the STA stayed associated (−58..−62 dBm, gateway-reachable) and SSH never dropped; uploads capped
`ap_concurrent`; zero teardown churn, zero "unknown interface uap0".

**Provisioning follow-up:** persist power-save-off (NM `wifi.powersave=2`) and the
`unmanaged-devices=interface-name:uap0` rule via `setup.sh` so the barriers survive a fresh OS.

**Known pre-existing follow-ups (out of scope of the concurrency work, tracked separately):**
channel-follow reconfigure lacks a live-channel re-read guard (stale-channel `-52` risk on a
mid-tick STA roam); the phase-2 activation tick publishes throttle before bring-up (one-tick
upload uncap on the dead→alive transition, self-heals next tick).

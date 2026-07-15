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
   when home WiFi is unreachable. **Never run AP and STA concurrently.**
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

- [ ] Cleanly switches STA↔AP; never both at once.
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
**NEVER** run AP+STA concurrently; never reboot the Pi while the car is writing;
never let WiFi recovery endanger the write path.

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
  arbitration (never both at once). For Phase B it must treat *any* active NM Wi-Fi
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

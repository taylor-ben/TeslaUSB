import { useEffect, useRef, useState } from "preact/hooks";
import { Fragment } from "preact";
import { api } from "../api/client";
import type {
  GadgetStatus,
  HealthBlock,
  Pref,
  StorageHealth,
  SystemHealth,
  SystemMetrics,
  WifiNetwork,
  WifiStatus,
} from "../api/types";

/**
 * Home screen (Task 5.2) — a faithful visual + structural reproduction of the
 * legacy Flask settings / device-status dashboard (`index.html`), the page
 * captured as the parity baseline at `docs/tasks/parity-baseline/media-hub/`.
 *
 * Read-only by construction. The device-status, System Health, Live Metrics and
 * Storage Health sections are populated from webd's read-only probe endpoints
 * (`/api/system/health`, `/api/system/metrics`, `/api/storage/health`, added in
 * 5.1d). Those handlers never 5xx and degrade to `unknown`/null for any
 * subsystem webd cannot honestly observe (the governor tier, inactive services,
 * car-owned exFAT volumes, SD wear telemetry), so a row that has no real signal
 * renders the legacy template's grey "unknown / —" state verbatim rather than a
 * fabricated value — which is exactly the parity baseline. The Video Indexer row
 * is the one System Health entry derived client-side from the catalog (clip
 * count + newest-clip age). `GET /api/settings` still populates the config-form
 * fields. CPU and SD Card I/O now render sampled client-side deltas from raw
 * counters; the old USB I/O tile was removed because there is no honest
 * per-device USB counter separate from mmcblk0 writes.
 *
 * The config form (Mapping & Indexing) is reproduced for structural parity but
 * is inert: buttons are `type="button"` and the form `preventDefault`s, so the
 * screen can never issue a mutation.
 */

function pref(prefs: Pref[] | null, key: string, dflt = ""): string {
  const p = prefs?.find((x) => x.key === key);
  return p ? p.value : dflt;
}

const METRIC_TILES = [
  { id: "metric-load", label: "Load (1m / 5m / 15m)" },
  { id: "metric-cpu", label: "CPU" },
  { id: "metric-temp", label: "CPU temp" },
  { id: "metric-mem", label: "Memory" },
  { id: "metric-swap", label: "Swap" },
  { id: "metric-sd", label: "SD Card I/O" },
];

const TIMEZONES = [
  "UTC",
  "America/Los_Angeles",
  "America/Denver",
  "America/Chicago",
  "America/New_York",
  "Europe/London",
  "Europe/Berlin",
];

// System Health subsystems + severity colors — transcribed verbatim from the
// legacy index.html card (Phase 4.2). The legacy JS renders any subsystem key
// absent from the probe payload as a grey "unknown" dot + "—" message; we feed
// a payload where ONLY `indexer` (Video Indexer) is populated, derived from the
// read-only catalog, so every other (system-probe) row degrades to that legacy
// "—" state rather than a fabricated value.
const SUBSYSTEMS = [
  { key: "gadget", label: "USB Gadget" },
  { key: "worker", label: "Background Worker" },
  { key: "indexer", label: "Video Indexer" },
  { key: "disk", label: "SD Card" },
  { key: "storage_writable", label: "Storage Roots" },
  { key: "network", label: "WiFi" },
  { key: "journal", label: "Recent Errors" },
];

const SEV_COLORS: Record<string, string> = {
  ok: "var(--accent-success, #4caf50)",
  warn: "var(--accent-warning, #ff9800)",
  error: "var(--accent-error,   #f44336)",
  unknown: "var(--text-secondary, #888)",
};

// Overall-severity → the device-status banner copy + the System-Health rollup
// label. Mirrors the legacy template's wording; `unknown` keeps the baseline's
// "Status Unknown / Unable to determine…" degraded text.
const STATUS_COPY: Record<string, { title: string; detail: string }> = {
  ok: { title: "Online", detail: "All systems nominal." },
  warn: { title: "Degraded", detail: "One or more subsystems need attention." },
  error: { title: "Attention needed", detail: "A subsystem is reporting an error." },
  unknown: {
    title: "Status Unknown",
    detail: "Unable to determine current device status.",
  },
};

const OVERALL_LABEL: Record<string, string> = {
  ok: "Healthy",
  warn: "Degraded",
  error: "Attention needed",
  unknown: "Unknown",
};

/** Bytes → a compact human string (GB above 1 GiB, else MB). */
function humanBytes(n: number | null | undefined): string {
  if (n == null || !Number.isFinite(n)) return "—";
  const gib = n / 1024 ** 3;
  if (gib >= 1) return `${gib.toFixed(gib >= 10 ? 0 : 1)} GB`;
  return `${(n / 1024 ** 2).toFixed(0)} MB`;
}

/** Bytes/sec → compact rate string ("1.2 MB/s" / "512 KB/s" / "0 B/s"). */
function formatRate(bps: number | null): string {
  if (bps == null || !Number.isFinite(bps) || bps < 0) return "—";
  if (bps >= 1024 ** 2) return `${(bps / 1024 ** 2).toFixed(1)} MB/s`;
  if (bps >= 1024) return `${(bps / 1024).toFixed(0)} KB/s`;
  return `${Math.round(bps)} B/s`;
}

/** Seconds → "Xd Yh Zm" (drops leading zero units). */
function formatUptime(s: number | null | undefined): string {
  if (s == null || !Number.isFinite(s) || s < 0) return "—";
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  const parts: string[] = [];
  if (d) parts.push(`${d}d`);
  if (h || d) parts.push(`${h}h`);
  parts.push(`${m}m`);
  return `up ${parts.join(" ")}`;
}

/** Epoch-seconds → a short local clock string for the "Updated …" footer. */
function formatUpdated(epoch: number | null | undefined): string {
  if (epoch == null || !Number.isFinite(epoch)) return "—";
  return new Date(epoch * 1000).toLocaleTimeString();
}

function isAbortError(err: unknown): boolean {
  return err instanceof DOMException && err.name === "AbortError";
}

function wifiSignalBars(signal: number): string {
  if (signal >= 75) return "▂▄▆█";
  if (signal >= 50) return "▂▄▆";
  if (signal >= 25) return "▂▄";
  return "▂";
}

/** A used/total memory tile → `{value:"NN%", detail:"used / total"}`. */
function memTile(
  m: { total_bytes: number; available_bytes: number; used_pct: number } | null,
  emptyDetail: string,
): { value: string; detail: string } {
  if (!m || m.total_bytes <= 0) return { value: "—", detail: emptyDetail };
  const used = m.total_bytes - m.available_bytes;
  return {
    value: `${Math.round(m.used_pct)}%`,
    detail: `${humanBytes(used)} / ${humanBytes(m.total_bytes)}`,
  };
}

/** Per-tile value/detail from the metrics payload; unprobed tiles stay "—". */
function metricFor(
  id: string,
  m: SystemMetrics | null,
  derived: { cpuPct: number | null; sdReadBps: number | null; sdWriteBps: number | null },
): { value: string; detail: string } {
  if (!m) return { value: "—", detail: "" };
  switch (id) {
    case "metric-load":
      return m.load
        ? {
            value: `${m.load.one.toFixed(2)} / ${m.load.five.toFixed(2)} / ${m.load.fifteen.toFixed(2)}`,
            detail: "",
          }
        : { value: "—", detail: "" };
    case "metric-mem":
      return memTile(m.mem, "");
    case "metric-swap":
      return memTile(m.swap, "none");
    case "metric-temp":
      return m.cpu_temp_c != null && Number.isFinite(m.cpu_temp_c)
        ? {
            value: `${m.cpu_temp_c.toFixed(1)} \u00b0C`,
            detail:
              m.cpu_temp_c >= 80
                ? "throttling"
                : m.cpu_temp_c >= 70
                  ? "warm"
                  : "",
          }
        : { value: "—", detail: "" };
    case "metric-cpu":
      return derived.cpuPct != null
        ? { value: `${Math.round(derived.cpuPct)}%`, detail: "" }
        : { value: "—", detail: "" };
    case "metric-sd":
      return derived.sdReadBps != null
        ? {
           value: `${formatRate(derived.sdReadBps)} / ${formatRate(derived.sdWriteBps)}`,
           detail: "read / write",
          }
        : { value: "—", detail: "" };
    default:
      return { value: "—", detail: "" };
  }
}

export function MediaHub() {
  const [prefs, setPrefs] = useState<Pref[] | null>(null);
  const [indexer, setIndexer] = useState<HealthBlock | null>(null);
  const [health, setHealth] = useState<SystemHealth | null>(null);
  const [metrics, setMetrics] = useState<SystemMetrics | null>(null);
  const [storage, setStorage] = useState<StorageHealth | null>(null);
  const [wifiStatus, setWifiStatus] = useState<WifiStatus | null>(null);
  const [wifiNetworks, setWifiNetworks] = useState<WifiNetwork[]>([]);
  const [wifiLoading, setWifiLoading] = useState(true);
  const [wifiScanLoading, setWifiScanLoading] = useState(false);
  const [gadget, setGadget] = useState<GadgetStatus | null>(null);
  const [gadgetUnavailable, setGadgetUnavailable] = useState(false);
  const [derived, setDerived] = useState<{
    cpuPct: number | null;
    sdReadBps: number | null;
    sdWriteBps: number | null;
  }>({ cpuPct: null, sdReadBps: null, sdWriteBps: null });
  const prevSample = useRef<{
    total: number;
    idle: number;
    read: number;
    write: number;
    at: number;
  } | null>(null);
  const wifiScanCtrl = useRef<AbortController | null>(null);

  useEffect(() => {
    const ctrl = new AbortController();
    api
      .settings(ctrl.signal)
      .then(setPrefs)
      .catch(() => {
        // Read-only degrade: fall back to template defaults without logging,
        // so an absent/empty prefs store never trips the zero-console gate.
      });
    // Device-status reads (5.1d). Each handler never 5xx and self-degrades to
    // unknown/null, so on the rare transport error we simply leave the section
    // in its loading/unknown state without logging (zero-console gate).
    api.systemHealth(ctrl.signal).then(setHealth).catch(() => {});
    api.storageHealth(ctrl.signal).then(setStorage).catch(() => {});
    // USB-gadget status is the first cross-daemon control-socket read: it talks
    // to gadgetd's live socket and so, unlike the catalog reads, it CAN be
    // unavailable (gadgetd down / not running). Surface an honest "unavailable"
    // state on a real failure; ignore aborts (unmount) to keep the console clean.
    api
      .gadgetStatus(ctrl.signal)
      .then(setGadget)
      .catch(() => {
        if (!ctrl.signal.aborted) setGadgetUnavailable(true);
      });
    // Video Indexer status is the one System Health row that IS catalog data:
    // clip count + newest-clip age, derived from the read-only catalog. Every
    // other subsystem comes from /api/system/health (or stays unknown).
    api
      .clips({ limit: 500 }, ctrl.signal)
      .then((page) => {
        const clips = page.items;
        const count = clips.length;
        if (count === 0) {
          setIndexer({ severity: "unknown", message: "0 clips indexed" });
          return;
        }
        const newest = Math.max(...clips.map((c) => c.started_at));
        const daysOld = Math.floor((Date.now() / 1000 - newest) / 86400);
        setIndexer({
          severity: "ok",
          message: `${count} clips indexed; newest is ${daysOld} d old`,
        });
      })
      .catch(() => {
        // Degrade: the Video Indexer row stays "—"/unknown like the others.
      });
    return () => ctrl.abort();
  }, []);

  useEffect(() => {
    let active = true;
    const ctrl = new AbortController();
    setWifiLoading(true);
    Promise.all([api.wifiStatus(ctrl.signal), api.wifiNetworks(ctrl.signal)])
      .then(([status, networks]) => {
        if (!active) return;
        setWifiStatus(status);
        setWifiNetworks(networks.networks);
      })
      .catch((err) => {
        if (!active || isAbortError(err)) return;
        setWifiStatus(null);
        setWifiNetworks([]);
      })
      .finally(() => {
        if (active) setWifiLoading(false);
      });
    return () => {
      active = false;
      ctrl.abort();
    };
  }, []);

  useEffect(
    () => () => {
      wifiScanCtrl.current?.abort();
    },
    [],
  );

  useEffect(() => {
    let active = true;
    let ctrl: AbortController | null = null;
    const tick = () => {
      ctrl?.abort();
      ctrl = new AbortController();
      api
        .systemMetrics(ctrl.signal)
        .then((m) => {
          if (!active) return;
          setMetrics(m);
          const now = Date.now();
          const prev = prevSample.current;
          let next = {
            cpuPct: null as number | null,
            sdReadBps: null as number | null,
            sdWriteBps: null as number | null,
          };
          if (
            m.cpu_times &&
            prev &&
            m.cpu_times.total > prev.total &&
            m.cpu_times.idle >= prev.idle
          ) {
            const dTotal = m.cpu_times.total - prev.total;
            const dIdle = m.cpu_times.idle - prev.idle;
            next.cpuPct = Math.max(0, Math.min(100, 100 * (1 - dIdle / dTotal)));
          }
          if (m.sd_io && prev) {
            const secs = (now - prev.at) / 1000;
            if (
              secs > 0 &&
              m.sd_io.read_bytes >= prev.read &&
              m.sd_io.write_bytes >= prev.write
            ) {
              next.sdReadBps = (m.sd_io.read_bytes - prev.read) / secs;
              next.sdWriteBps = (m.sd_io.write_bytes - prev.write) / secs;
            }
          }
          setDerived(next);
          // Advance each counter's baseline independently: a poll that is
          // missing one source must not freeze the other metric's delta.
          prevSample.current = {
            total: m.cpu_times?.total ?? prev?.total ?? 0,
            idle: m.cpu_times?.idle ?? prev?.idle ?? 0,
            read: m.sd_io?.read_bytes ?? prev?.read ?? 0,
            write: m.sd_io?.write_bytes ?? prev?.write ?? 0,
            at: now,
          };
        })
        .catch(() => {
          // Read-only degrade; ignore aborts/transport errors (zero-console gate).
        });
    };
    tick();
    const h = setInterval(tick, 2000);
    return () => {
      active = false;
      clearInterval(h);
      ctrl?.abort();
    };
  }, []);

  const onWifiScan = () => {
    if (wifiScanLoading) return;
    wifiScanCtrl.current?.abort();
    const ctrl = new AbortController();
    wifiScanCtrl.current = ctrl;
    setWifiScanLoading(true);
    api
      .wifiScan(ctrl.signal)
      .then((networks) => {
        setWifiNetworks(networks.networks);
        return api
          .wifiStatus(ctrl.signal)
          .then(setWifiStatus)
          .catch((err) => {
            if (!isAbortError(err)) setWifiStatus(null);
          });
      })
      .catch((err) => {
        if (!isAbortError(err)) setWifiNetworks([]);
      })
      .finally(() => {
        if (!ctrl.signal.aborted) setWifiScanLoading(false);
      });
  };

  const overall = health?.overall ?? "unknown";
  const statusCopy = STATUS_COPY[overall] ?? STATUS_COPY.unknown;

  return (
    // Bare screen content — the router hoists a single shared <Shell> and
    // supplies the active nav key (this Path-A dashboard is routed at
    // `/settings` → "settings"), so this screen no longer wraps itself in Shell
    // (was <Shell active="settings"> in the standalone 5.2 build before the 5.3
    // router landed).
    <div class="container" data-screen="settings-dashboard">
        {/* Device Status — derived from /api/system/health overall severity;
            falls back to the baseline "unknown" copy until the probe resolves. */}
        <div class={`device-status-card device-status-${overall}`}>
          <div class="device-status-header">
            <span
              class={`status-dot status-${overall}`}
              style={`background:${SEV_COLORS[overall] ?? SEV_COLORS.unknown};`}
            />
            <div class="device-status-info">
              <strong>{statusCopy.title}</strong>
              <p>{statusCopy.detail}</p>
            </div>
          </div>
        </div>

        {/* System Health — the legacy probe (/api/system/health) is NOT part of
            the read-only catalog API, so each subsystem row degrades to the
            legacy "unknown / —" state EXCEPT Video Indexer, which is derived
            from the read-only catalog (clip count + newest age) — exactly the
            baseline's "N clips indexed; newest is M d old". No system metric is
            fabricated; the overall stays the legacy degraded default. */}
        <details class="settings-section" id="system-health-section" open>
          <summary>System Health</summary>
          <div class="section-content">
            <div
              id="system-health-card"
              style="display:flex; flex-direction:column; gap:6px;"
            >
              <p
                id="system-health-overall"
                style="margin:0 0 6px; padding:8px 12px; border-radius:8px; font-size:0.95rem; display:flex; align-items:center; gap:8px; background:var(--bg-secondary); border:1px solid var(--border-color);"
              >
                <span
                  class={`health-dot health-dot-${overall}`}
                  aria-hidden="true"
                  style={`width:10px; height:10px; border-radius:50%; display:inline-block; flex-shrink:0; background:${SEV_COLORS[overall] ?? SEV_COLORS.unknown};`}
                />
                <span id="system-health-overall-text">
                  {OVERALL_LABEL[overall] ?? OVERALL_LABEL.unknown}
                </span>
              </p>
              <div
                id="system-health-rows"
                style="display:grid; grid-template-columns:auto auto 1fr; gap:6px 12px; align-items:center; font-size:0.9rem;"
              >
                {SUBSYSTEMS.map((sub) => {
                  let sev: string;
                  let msg: string;
                  if (sub.key === "indexer") {
                    const webdIdx = health?.subsystems?.indexer ?? null;
                    const cat = indexer;
                    sev =
                      webdIdx && webdIdx.severity !== "unknown"
                        ? webdIdx.severity
                        : (cat?.severity ?? "unknown");
                    const catMsg = cat?.message ?? null;
                    if (webdIdx && (webdIdx.severity === "warn" || webdIdx.severity === "error")) {
                      msg = catMsg ? `${webdIdx.message} — ${catMsg}` : webdIdx.message;
                    } else {
                      msg = catMsg ?? "—";
                    }
                  } else {
                    const block = health?.subsystems?.[sub.key] ?? null;
                    sev = block?.severity ?? "unknown";
                    msg = block?.message ?? "—";
                  }
                  return (
                    <Fragment key={sub.key}>
                      <div>
                        <span
                          aria-label={sev}
                          style={`width:10px; height:10px; border-radius:50%; display:inline-block; flex-shrink:0; background:${SEV_COLORS[sev] ?? SEV_COLORS.unknown};`}
                        />
                      </div>
                      <div style="color:var(--text-primary)">{sub.label}</div>
                      <div style="color:var(--text-secondary); overflow:hidden; text-overflow:ellipsis; white-space:nowrap;">
                        {msg}
                      </div>
                    </Fragment>
                  );
                })}
              </div>
            </div>
          </div>
        </details>

        {/* Live Metrics — load/temp/memory come from probe payload; CPU and SD
            throughput are sampled client-side as deltas across polls. */}
        <details class="settings-section" id="live-metrics-section" open>
          <summary>Live Metrics</summary>
          <div class="section-content">
            <div
              id="live-metrics-card"
              style="display:flex; flex-direction:column; gap:8px;"
            >
              <div
                id="live-metrics-grid"
                style="display:grid; grid-template-columns:repeat(auto-fit, minmax(200px, 1fr)); gap:10px;"
              >
                {METRIC_TILES.map((t) => {
                  const { value, detail } = metricFor(t.id, metrics, derived);
                  return (
                    <div class="metric-tile" id={t.id} key={t.id}>
                      <div class="metric-label">{t.label}</div>
                      <div class="metric-value">{value}</div>
                      <div class="metric-detail">{detail || "\u00a0"}</div>
                    </div>
                  );
                })}
              </div>
              <p
                id="live-metrics-foot"
                style="margin:0; font-size:0.78rem; color:var(--text-secondary);"
              >
                Updated{" "}
                <span id="live-metrics-updated">
                  {formatUpdated(metrics?.updated_at)}
                </span>{" "}
                ·{" "}
                <span id="live-metrics-uptime">
                  {formatUptime(metrics?.uptime_s)}
                </span>
              </p>
            </div>
          </div>
        </details>

        {/* USB Drive — live state from gadgetd's control socket
            (GET /api/gadget/status), the first cross-daemon control-socket read
            surfaced in the SPA. Unlike the catalog reads this CAN be unavailable
            (gadgetd down / not running), in which case we show an honest
            "unavailable" state rather than fabricating a "connected" status. */}
        <details class="settings-section" id="usb-gadget-section" open>
          <summary>USB Drive</summary>
          <div class="section-content">
            {gadget ? (
              <div
                id="usb-gadget-card"
                style="display:grid; grid-template-columns:auto 1fr; gap:6px 12px; font-size:0.9rem; align-items:center;"
              >
                <div style="color:var(--text-secondary)">Presented to car</div>
                <div data-testid="usb-present" style="color:var(--text-primary)">
                  {gadget.present ? "Yes" : "No"}
                </div>
                <div style="color:var(--text-secondary)">Controller bound</div>
                <div data-testid="usb-bound" style="color:var(--text-primary)">
                  {gadget.bound
                    ? `Yes (${gadget.udc_state ?? "unknown"})`
                    : "No"}
                </div>
                <div style="color:var(--text-secondary)">Dashcam image</div>
                <div style="color:var(--text-primary); overflow:hidden; text-overflow:ellipsis; white-space:nowrap;">
                  {gadget.lun_file ?? "\u2014"}
                </div>
                <div style="color:var(--text-secondary)">Media image</div>
                <div style="color:var(--text-primary); overflow:hidden; text-overflow:ellipsis; white-space:nowrap;">
                  {gadget.media_lun_file ?? "\u2014"}
                </div>
                <div style="color:var(--text-secondary)">Media mount (read)</div>
                <div
                  data-testid="usb-media-ro"
                  style="color:var(--text-primary); overflow:hidden; text-overflow:ellipsis; white-space:nowrap;"
                >
                  {gadget.media_ro_mounted === null
                    ? "\u2014"
                    : gadget.media_ro_mounted
                      ? `Mounted${gadget.media_ro_path ? ` (${gadget.media_ro_path})` : ""}`
                      : `Not mounted${gadget.media_ro_error ? ` \u2014 ${gadget.media_ro_error}` : ""}`}
                </div>
              </div>
            ) : gadgetUnavailable ? (
              <div
                data-testid="usb-gadget-unavailable"
                style="text-align:center; padding:12px; color:var(--text-secondary)"
              >
                USB gadget status is unavailable (gadgetd is not reachable).
              </div>
            ) : (
              <div
                data-testid="usb-gadget-loading"
                style="text-align:center; padding:12px; color:var(--text-secondary)"
              >
                Loading USB status&#8230;
              </div>
            )}
          </div>
        </details>

        {/* WiFi Networks — read-only NetworkManager view (no join/disconnect). */}
        <details class="settings-section" id="wifi-networks-section">
          <summary>WiFi Networks</summary>
          <div class="section-content">
            <div style="display:flex;justify-content:space-between;align-items:center;margin-bottom:4px">
              <p style="font-size:0.85em;color:var(--text-secondary);margin:0">
                Networks higher in the list are preferred when multiple are in
                range.
              </p>
              <button
                type="button"
                class="edit-btn"
                id="btnWifiScan"
                style="padding:4px 10px;font-size:0.85em;white-space:nowrap"
                disabled={wifiLoading || wifiScanLoading}
                onClick={onWifiScan}
              >
                {wifiScanLoading ? "Scanning…" : "Scan"}
              </button>
            </div>
            <div id="savedNetworksList">
              {wifiLoading ? (
                <div style="text-align:center;padding:12px;color:var(--text-secondary)">
                  Loading Wi-Fi networks…
                </div>
              ) : (
                <div style="display:flex;flex-direction:column;gap:8px">
                  <div
                    id="wifi-current-connection"
                    style="display:flex;justify-content:space-between;align-items:center;padding:8px 10px;border:1px solid var(--border-color);border-radius:8px;background:var(--bg-secondary)"
                  >
                    <div style="overflow:hidden;text-overflow:ellipsis;white-space:nowrap">
                      <strong>{wifiStatus?.ssid ?? "Not connected"}</strong>
                      {wifiStatus?.signal != null ? (
                        <span style="margin-left:8px;color:var(--text-secondary);font-size:0.85em">
                          {wifiSignalBars(wifiStatus.signal)} {wifiStatus.signal}%
                        </span>
                      ) : null}
                    </div>
                    {wifiStatus?.connected ? (
                      <span
                        style="font-size:0.75em;padding:2px 8px;border-radius:999px;background:var(--bg-secondary);border:1px solid var(--accent-success, #4caf50);color:var(--accent-success, #4caf50)"
                      >
                        Connected
                      </span>
                    ) : null}
                  </div>
                  {wifiNetworks.length > 0 ? (
                    <div
                      id="wifi-networks-list"
                      style="display:flex;flex-direction:column;gap:6px"
                    >
                      {wifiNetworks.map((network) => (
                        <div
                          key={network.ssid}
                          class="wifi-network-row"
                          style="display:flex;align-items:center;gap:10px;padding:8px 10px;border:1px solid var(--border-color);border-radius:8px"
                        >
                          <div style="flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap">
                            {network.ssid}
                          </div>
                          <div style="font-size:0.85em;color:var(--text-secondary);white-space:nowrap">
                            {wifiSignalBars(network.signal)} {network.signal}%
                          </div>
                          <div style="display:flex;align-items:center;gap:6px;white-space:nowrap">
                            {network.security ? <span aria-label="Secured">🔒</span> : null}
                            {network.saved ? (
                              <span
                                style="font-size:0.75em;padding:2px 8px;border-radius:999px;background:var(--bg-secondary);border:1px solid var(--border-color);color:var(--text-secondary)"
                              >
                                Saved
                              </span>
                            ) : null}
                            {network.active ? (
                              <span
                                style="font-size:0.75em;padding:2px 8px;border-radius:999px;background:var(--bg-secondary);border:1px solid var(--accent-success, #4caf50);color:var(--accent-success, #4caf50)"
                              >
                                Connected
                              </span>
                            ) : null}
                          </div>
                        </div>
                      ))}
                    </div>
                  ) : (
                    <div style="text-align:center;padding:12px;color:var(--text-secondary)">
                      No nearby Wi-Fi networks found.
                    </div>
                  )}
                </div>
              )}
            </div>
          </div>
        </details>

        {/* Access Point — degraded (no Wi-Fi tooling), matching the baseline. */}
        <details class="settings-section">
          <summary>Access Point</summary>
          <div class="section-content">
            <div style="color: var(--error-text);">
              AP status unavailable (Wi-Fi tooling is not part of the read-only
              catalog build).
            </div>
          </div>
        </details>

        {/* Storage &amp; Auto-Cleanup — link card (parity). */}
        <details class="settings-section">
          <summary>Storage &amp; Auto-Cleanup</summary>
          <div class="section-content">
            <a
              href="/storage"
              class="action-card"
              style="display:flex; align-items:center; gap:12px; padding:14px; text-decoration:none; color:inherit; border:1px solid var(--border-color); border-radius:8px; min-height:44px;"
            >
              <svg class="inline-icon" width="24" height="24" aria-hidden="true">
                <use href="/static/icons/lucide-sprite.svg#icon-hard-drive" />
              </svg>
              <div style="flex:1">
                <strong>Storage Settings</strong>
                <p style="margin:4px 0 0; font-size:0.85rem; color:var(--text-secondary)">
                  Adjust USB drive sizes and tier-aware auto-cleanup for
                  TeslaCam.
                </p>
              </div>
              <svg
                class="inline-icon"
                width="20"
                height="20"
                aria-hidden="true"
                style="color:var(--text-secondary)"
              >
                <use href="/static/icons/lucide-sprite.svg#icon-chevron-right" />
              </svg>
            </a>
          </div>
        </details>

        {/* Mapping & Indexing — config form, inert; bound to /api/settings. */}
        <details class="settings-section">
          <summary>Mapping &amp; Indexing</summary>
          <div class="section-content">
            <form onSubmit={(e) => e.preventDefault()}>
              <p style="font-size:0.85rem; color:var(--text-secondary); margin:0 0 16px">
                Tesla embeds GPS coordinates and telemetry data (speed, braking,
                steering) inside each dashcam video. The indexer extracts this
                data and builds a database of trips, routes, and driving events.
              </p>
              <div class="settings-form-grid">
                <div class="form-group">
                  <label style="font-size:0.85rem">
                    <strong>Trip gap (minutes)</strong>
                  </label>
                  <input
                    type="number"
                    name="trip_gap_minutes"
                    value={pref(prefs, "trip_gap_minutes", "10")}
                    min="1"
                    max="60"
                    class="settings-form-input"
                  />
                </div>
                <div class="form-group">
                  <label for="mapping-speed-limit" style="font-size:0.85rem">
                    <strong>Speed alert (mph)</strong>
                  </label>
                  <input
                    id="mapping-speed-limit"
                    type="number"
                    name="speed_limit_mph"
                    value={pref(prefs, "speed_limit_mph", "85")}
                    min="0"
                    max="200"
                    step="5"
                    class="settings-form-input"
                  />
                </div>
                <div class="form-group">
                  <label for="mapping-speed-units" style="font-size:0.85rem">
                    <strong>Map speed display units</strong>
                  </label>
                  <select
                    id="mapping-speed-units"
                    name="speed_units"
                    class="settings-form-input"
                    value={pref(prefs, "speed_units", "mph")}
                  >
                    <option value="mph">mph</option>
                    <option value="kph">kph</option>
                  </select>
                </div>
                <div class="form-group">
                  <label
                    for="mapping-display-timezone"
                    style="font-size:0.85rem"
                  >
                    <strong>Map day timezone</strong>
                  </label>
                  <select
                    id="mapping-display-timezone"
                    name="display_timezone"
                    class="settings-form-input"
                    value={pref(prefs, "display_timezone")}
                  >
                    <option value="">Auto (use this device's timezone)</option>
                    {TIMEZONES.map((tz) => (
                      <option value={tz} key={tz}>
                        {tz}
                      </option>
                    ))}
                  </select>
                </div>
              </div>
              <button
                type="button"
                class="btn btn-primary"
                style="width:100%"
                disabled
              >
                Save Mapping Settings
              </button>
            </form>
          </div>
        </details>

        {/* Storage Health — from /api/storage/health. Capacity-derived severity
            + summary with best-effort wear telemetry probes. */}
        <details class="settings-section" id="storage-health-section">
          <summary>Storage Health</summary>
          <div class="section-content" id="storage-health-card">
            <div class="storage-health-header">
              <span
                class={`status-dot health-dot health-dot-${storage?.severity ?? "unknown"}`}
                id="storage-health-dot"
                aria-label="Storage health severity"
                style={`background:${SEV_COLORS[storage?.severity ?? "unknown"] ?? SEV_COLORS.unknown};`}
              />
              <strong id="storage-health-summary">
                {storage?.summary ?? "Checking…"}
              </strong>
            </div>
            <dl class="storage-health-grid" id="storage-health-grid">
              <dt>Device</dt>
              <dd>{storage?.device ?? "—"}</dd>
              <dt>Filesystem</dt>
              <dd>{storage?.fstype ?? "—"}</dd>
              <dt>Mount</dt>
              <dd>{storage?.mount ?? "—"}</dd>
              <dt>Filesystem errors</dt>
              <dd>{storage?.fs_errors == null ? "—" : String(storage.fs_errors)}</dd>
              <dt>TRIM</dt>
              <dd>{storage?.trim ?? "—"}</dd>
            </dl>
            <p class="storage-health-footer">
              SD cards expose no wear telemetry (no SMART, no per-block
              checksums). Plan to replace the card every 12 months and keep cloud
              archive enabled so a card failure never costs you data.
            </p>
          </div>
        </details>

        {/* System — host facts read live from webd's /api/system/metrics;
            anything webd cannot observe renders as "—" rather than fabricated. */}
        <details class="settings-section">
          <summary>System</summary>
          <div class="section-content">
            <div style="display:grid; grid-template-columns:auto 1fr; gap:6px 16px; font-size:0.9rem;">
              <span style="color:var(--text-secondary)">Hostname</span>
              <strong>{metrics?.hostname ?? "—"}</strong>
              <span style="color:var(--text-secondary)">IP Address</span>
              <span>{metrics?.ip_address ?? "—"}</span>
              <span style="color:var(--text-secondary)">Uptime</span>
              <span>{formatUptime(metrics?.uptime_s)}</span>
              <span style="color:var(--text-secondary)">Platform</span>
              <span>{metrics?.platform ?? "—"}</span>
              <span style="color:var(--text-secondary)">Memory</span>
              <span>
                {metrics?.mem
                  ? `${humanBytes(metrics.mem.total_bytes - metrics.mem.available_bytes)} / ${humanBytes(metrics.mem.total_bytes)}`
                  : "—"}
              </span>
              <span style="color:var(--text-secondary)">Version</span>
              <code style="font-size:0.8rem">B-1</code>
            </div>
          </div>
        </details>
      </div>
  );
}

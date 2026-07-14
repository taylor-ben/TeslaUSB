/**
 * HudController — the imperative Tesla-HUD overlay driven over a native
 * `<video>`. The HUD is a non-Preact concern (it mutates a fixed set of DOM
 * nodes every animation frame from the video's playback clock), so it is driven
 * through this plain controller created/destroyed from a Preact ref/effect in
 * `screens/EventPlayer.tsx` — mirroring the "imperative lib behind a ref"
 * pattern established by `map/controller.ts`.
 *
 * Telemetry comes from one of two sources, in priority order:
 *  1. `window.__TESLAUSB_HUD_FIXTURE__` — seeded samples injected by the UAT
 *     (the SMPTE test-pattern fixtures carry no embedded SEI), exactly as the
 *     0.4 parity baseline did.
 *  2. Otherwise `/api/clips/:id/telemetry` JSON parsed server-side from the
 *     archived MP4's embedded SEI track (the production path with real footage).
 *
 * `window.__TESLAUSB_HUD__` exposes the live source, sample count and current
 * HUD state so Playwright can assert on real overlay behaviour, not just DOM.
 */
import {
  DEFAULT_HUD,
  fixtureTelemetry,
  sampleAt,
  sampleToHud,
  type HudState,
  type TelemetrySample,
} from "./telemetry";

/** The HUD DOM nodes the controller mutates (owned by the Preact render). */
export interface HudElements {
  root?: HTMLElement;
  gear: HTMLElement;
  speed: HTMLElement;
  steering: HTMLElement;
  brakePedal: HTMLElement;
  throttlePedal: HTMLElement;
  blinkerLeft: HTMLElement;
  blinkerRight: HTMLElement;
  autopilot: HTMLElement;
}

type TelemetrySource = "fixture" | "sei" | "none";

interface HudHooks {
  build: string;
  source: TelemetrySource;
  sampleCount: number;
  /** Number of times the HUD DOM has been applied (proves the rAF loop ran). */
  frames: number;
  getState(): HudState;
}

export class HudController {
  private readonly video: HTMLVideoElement;
  private readonly hud: HudElements;
  private readonly root?: HTMLElement;
  private samples: TelemetrySample[] = [];
  private source: TelemetrySource = "none";
  /** Seconds added to video.currentTime before sampling telemetry, so a single
   *  front-sourced telemetry track lines up with a non-front camera that starts
   *  at a different clip offset. 0 for the front camera. */
  private timeOffsetSec = 0;
  private state: HudState = { ...DEFAULT_HUD };
  private frames = 0;
  private rafId = 0;
  private destroyed = false;
  private hooks: HudHooks;
  private readonly onFrame = () => this.tick();

  constructor(video: HTMLVideoElement, hud: HudElements) {
    this.video = video;
    this.hud = hud;
    this.root = hud.root;

    this.hooks = {
      build:
        (window as unknown as { __TESLAUSB_BUILD__?: string }).__TESLAUSB_BUILD__ ??
        "dev",
      source: "none",
      sampleCount: 0,
      frames: 0,
      getState: () => this.state,
    };
    (window as unknown as { __TESLAUSB_HUD__?: HudHooks }).__TESLAUSB_HUD__ =
      this.hooks;

    // Update on the key playback transitions too (not just rAF) so the HUD is
    // correct even when the tab is throttled or autoplay is blocked headless.
    video.addEventListener("loadedmetadata", this.onFrame);
    video.addEventListener("play", this.onFrame);
    video.addEventListener("timeupdate", this.onFrame);
    video.addEventListener("seeked", this.onFrame);

    this.rafId = requestAnimationFrame(this.loop);
  }

  private loop = () => {
    if (this.destroyed) return;
    this.tick();
    this.rafId = requestAnimationFrame(this.loop);
  };

  /**
   * (Re)load telemetry for the current clip/camera. Uses the seeded fixture if
   * present, otherwise fetches telemetry JSON from webd. Never
   * throws — failure leaves the HUD at its neutral default.
   */
  async loadTelemetry(telemetryUrl: string): Promise<void> {
    const fixture = fixtureTelemetry();
    if (fixture) {
      this.setSamples(fixture, "fixture");
      return;
    }
    try {
      const resp = await fetch(telemetryUrl, {
        headers: { Accept: "application/json" },
      });
      if (!resp.ok) {
        this.setSamples([], "none");
        return;
      }
      const parsed = (await resp.json()) as TelemetrySample[];
      this.setSamples(
        Array.isArray(parsed) ? parsed : [],
        Array.isArray(parsed) && parsed.length > 0 ? "sei" : "none",
      );
    } catch {
      this.setSamples([], "none");
    }
  }

  /**
   * Shift the telemetry clock by `sec` (video-seconds) so front-sourced samples
   * line up with a non-front camera whose file starts at a different clip offset.
   * Pass 0 for the front camera. Applies immediately (no reload required).
   */
  setTimeOffset(sec: number): void {
    this.timeOffsetSec = Number.isFinite(sec) ? sec : 0;
    this.tick();
  }

  private setSamples(samples: TelemetrySample[], source: TelemetrySource) {
    this.samples = samples;
    this.source = samples.length > 0 ? source : "none";
    this.hooks.source = this.source;
    this.hooks.sampleCount = samples.length;
    this.syncEmptyState();
    this.tick();
  }

  private tick() {
    if (this.destroyed) return;
    const t = (this.video.currentTime || 0) + this.timeOffsetSec;
    const sample = this.samples.length > 0 ? sampleAt(this.samples, t) : null;
    this.state = sample ? sampleToHud(sample) : { ...DEFAULT_HUD };
    this.apply(this.state);
    this.syncEmptyState();
    this.frames++;
    this.hooks.frames = this.frames;
  }

  private syncEmptyState() {
    this.root?.classList.toggle("hud-empty", this.source === "none");
  }

  private apply(s: HudState) {
    const { hud } = this;
    hud.gear.textContent = s.gear;
    hud.speed.textContent = String(s.speed);
    hud.steering.style.setProperty("--wheel-rotation", `${s.steering}deg`);
    hud.brakePedal.style.setProperty("--pedal-fill", `${s.brakeFill}%`);
    hud.throttlePedal.style.setProperty("--pedal-fill", `${s.throttleFill}%`);
    hud.blinkerLeft.classList.toggle("active", s.blinkerLeft);
    hud.blinkerRight.classList.toggle("active", s.blinkerRight);
    hud.autopilot.textContent = s.autopilot;
    hud.autopilot.classList.toggle("active", s.autopilotActive);
  }

  destroy() {
    this.destroyed = true;
    cancelAnimationFrame(this.rafId);
    this.video.removeEventListener("loadedmetadata", this.onFrame);
    this.video.removeEventListener("play", this.onFrame);
    this.video.removeEventListener("timeupdate", this.onFrame);
    this.video.removeEventListener("seeked", this.onFrame);
    const win = window as unknown as { __TESLAUSB_HUD__?: HudHooks };
    if (win.__TESLAUSB_HUD__ === this.hooks) win.__TESLAUSB_HUD__ = undefined;
  }
}

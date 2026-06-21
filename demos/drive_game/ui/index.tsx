import { render, createSignal } from "solite-runtime";
import { Bar } from "./Bar";

type TelemetryKey =
  | "speedMph"
  | "headingDeg"
  | "steering"
  | "throttle"
  | "brake"
  | "x"
  | "y"
  | "fps"
  | "maxSpeed";

declare global {
  interface Window {
    state: Record<string, unknown>;
  }

  var state: Record<string, unknown>;
}

const telemetryKeys: Record<TelemetryKey, TelemetryKey> = {
  speedMph: "speedMph",
  headingDeg: "headingDeg",
  steering: "steering",
  throttle: "throttle",
  brake: "brake",
  x: "x",
  y: "y",
  fps: "fps",
  maxSpeed: "maxSpeed",
};

function numberValue(name: TelemetryKey, fallbackValue = 0): number {
  const value = Number(globalThis.state[telemetryKeys[name]]);
  return Number.isFinite(value) ? value : fallbackValue;
}

const COMPASS = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];

// Max-speed slider bounds, in mph. These mirror the clamp in the Rust HUD.
const MIN_MPH = 20;
const MAX_MPH = 50;

// Window-space geometry of the slider track, in logical (CSS) pixels. The card
// is anchored to the top-right with a fixed offset, so the track's vertical
// extent is constant regardless of window size. Keep in sync with styles.css.
const TRACK_TOP = 98;
const TRACK_HEIGHT = 170;


function valueFromY(y: number): number {
  let fraction = (TRACK_TOP + TRACK_HEIGHT - y) / TRACK_HEIGHT;
  fraction = Math.max(0, Math.min(1, fraction));
  return Math.round(MIN_MPH + fraction * (MAX_MPH - MIN_MPH));
}

function applyMaxSpeed(value: number): void {
  globalThis.state.maxSpeed = value;
  // Push the new cap back to the Rust simulation.
  sendEvent("maxSpeed", JSON.stringify(value));
}

function App() {
  const speed = () => numberValue("speedMph");
  const heading = () => numberValue("headingDeg");
  const steering = () => numberValue("steering");
  const throttle = () => numberValue("throttle");
  const brake = () => numberValue("brake");
  const x = () => numberValue("x");
  const y = () => numberValue("y");
  const fps = () => numberValue("fps");
  const mode = () => String(globalThis.state.mode || "live");
  const maxSpeed = () => numberValue("maxSpeed", MAX_MPH);

  const gear = () => {
    const s = speed();
    if (s < -0.5) return "R";
    if (s < 1.0) return "N";
    return "D";
  };
  const compass = () => COMPASS[Math.round(heading() / 45) % 8];
  const speedFill = () => ({
    width: Math.min(100, (Math.abs(speed()) / Math.max(maxSpeed(), 1)) * 100) + "%",
  });

  const sliderFrac = () => (maxSpeed() - MIN_MPH) / (MAX_MPH - MIN_MPH);
  const sliderFill = () => {
    const p = sliderFrac() * 100;
    return {
      background:
        "linear-gradient(to top," +
        " #1f9e7a 0%, #8af7c8 " + p + "%," +
        " rgba(4, 14, 16, 0.92) " + p + "%, rgba(4, 14, 16, 0.92) 100%)",
    };
  };
  // Rectangular handle, vertically centered on the current value. Positioned in
  // card-space, matching .slider-track (top: 74, height: 170 in styles.css).
  const HANDLE_H = 14;
  const handleStyle = () => ({
    top: 74 + (1 - sliderFrac()) * 170 - HANDLE_H / 2 + "px",
  });

  // Drag handling. Pressing the slider starts a drag and mounts a full-screen
  // capture layer so pointer moves/release are tracked anywhere on screen — not
  // just over the thin track. Without this, drifting off the track mid-drag
  // (or releasing outside it) silently drops the gesture.
  const [dragging, setDragging] = createSignal(false);
  const startDrag = (clientY: number) => {
    setDragging(true);
    applyMaxSpeed(valueFromY(clientY));
  };
  const moveDrag = (clientY: number) => {
    if (dragging()) applyMaxSpeed(valueFromY(clientY));
  };
  const endDrag = () => setDragging(false);

  return (
    <div class="screen">
      <section class="telemetry-card">
        <div class="card-head">
          <div class="eyebrow">Solite HUD</div>
          <div class="pill">{mode}</div>
        </div>
        <h1>Grid Runner</h1>

        <div class="speed-cluster">
          <div class="readout speed">
            <span>{() => Math.abs(speed()).toFixed(0)}</span>
            <small>mph</small>
          </div>
          <div class="gear-badge">{gear}</div>
        </div>
        <div class="speed-gauge">
          <div class="speed-gauge-fill" style={speedFill()}></div>
        </div>

        <div class="metric-grid">
          <div class="metric">
            <label>Heading</label>
            <strong>{() => heading().toFixed(0)}° {compass}</strong>
          </div>
          <div class="metric">
            <label>Position</label>
            <strong>{() => x().toFixed(1)}, {() => y().toFixed(1)}</strong>
          </div>
        </div>

        <Bar label="Steer" value={steering} side={() => (steering() < 0 ? "left" : "right")} />
        <Bar label="Gas" value={throttle} side={() => "right"} />
        <Bar label="Brake" value={brake} side={() => "left"} />
      </section>

      <section class="speed-card">
        <div class="speed-card-title">Max</div>
        <div class="speed-card-value">{() => maxSpeed().toFixed(0)}</div>
        <div class="speed-card-unit">mph</div>
        <div class="slider-track" style={sliderFill()}></div>
        <div class="slider-handle" style={handleStyle()}></div>
        <div
          class="slider-hit"
          onClick={(event) => startDrag(event.y)}
          onMouseUp={endDrag}
        ></div>
      </section>

      <section class="status-card">
        <div class="controls-title">Controls</div>
        <div class="controls-grid">
          <kbd>↑</kbd><span>throttle</span>
          <kbd>↓</kbd><span>brake / reverse</span>
          <kbd>←</kbd><span>steer left</span>
          <kbd>→</kbd><span>steer right</span>
        </div>
      </section>

      <section class="fps-card">
        <div class="fps-label">FPS</div>
        <div class="fps-value">{() => fps().toFixed(0)}</div>
      </section>

      {() =>
        dragging() ? (
          <div
            class="drag-capture"
            onMouseMove={(event) => moveDrag(event.y)}
            onMouseUp={endDrag}
            onMouseLeave={endDrag}
          ></div>
        ) : null
      }
    </div>
  );
}

render(() => <App />, __SOL_ROOT__);

// Dashboard scope (bead 5hr): play CV into a module and watch the response.
// A 1V/oct step sequencer or an LFO drives the circuit's input net; the backend
// runs an ngspice transient; a CRT-style canvas plots stepped-in vs slewed-out.
import { useEffect, useMemo, useRef, useState } from "preact/hooks";
import { api, type SimResult, type SimPayload } from "./api";

// Debounced sim runner that KEEPS the last trace on the screen while the next
// one computes (a scope shouldn't blank between runs) and surfaces errors.
function useSim(name: string, payload: SimPayload, deps: unknown[]) {
  const [state, setState] = useState<{
    data: SimResult | null;
    error: string | null;
    loading: boolean;
  }>({ data: null, error: null, loading: true });
  useEffect(() => {
    let live = true;
    setState((s) => ({ ...s, loading: true, error: null }));
    const id = setTimeout(() => {
      api
        .sim(name, payload)
        .then((data) => live && setState({ data, error: null, loading: false }))
        .then(undefined, (e) =>
          live && setState((s) => ({ data: s.data, error: String(e), loading: false })),
        );
    }, 350);
    return () => {
      live = false;
      clearTimeout(id);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);
  return state;
}

type Pt = [number, number]; // [t_s, volts]
type Source = "seq" | "lfo";
type Shape = "sine" | "tri" | "square";

// RATE knob → OTA bias voltage. Near the negative rail (~V(IABC)) the bias current
// starves to nA and the slew stretches out; above it the slew is near-instant.
const RAIL = -10.8;
const BIAS_FAST = 5;

const SEMI = ["C", "C♯", "D", "D♯", "E", "F", "F♯", "G", "G♯", "A", "A♯", "B"];
const noteName = (s: number) => SEMI[((s % 12) + 12) % 12] + (4 + Math.floor(s / 12));
const semiToVolt = (s: number) => s / 12; // 1V/octave

// A stepped 1V/oct sequence as PWL breakpoints (sharp edges the slew rounds off).
function seqPwl(steps: number[], win: number): Pt[] {
  const dur = win / steps.length;
  const eps = win / 6000;
  const pts: Pt[] = [[0, semiToVolt(steps[0])]];
  for (let i = 1; i < steps.length; i++) {
    const t = i * dur;
    pts.push([t, semiToVolt(steps[i - 1])]);
    pts.push([t + eps, semiToVolt(steps[i])]);
  }
  pts.push([win, semiToVolt(steps[steps.length - 1])]);
  return pts;
}

// An LFO sampled into PWL points.
function lfoPwl(shape: Shape, rateHz: number, amp: number, off: number, win: number): Pt[] {
  const N = 256;
  const pts: Pt[] = [];
  for (let k = 0; k <= N; k++) {
    const t = (win * k) / N;
    const ph = (rateHz * t) % 1;
    const s =
      shape === "sine"
        ? Math.sin(2 * Math.PI * ph)
        : shape === "tri"
          ? ph < 0.5
            ? 4 * ph - 1
            : 3 - 4 * ph
          : ph < 0.5
            ? 1
            : -1;
    pts.push([t, off + amp * s]);
  }
  return pts;
}

export function ScopeSection({ name, version }: { name: string; version: number }) {
  const [source, setSource] = useState<Source>("seq");
  const [steps, setSteps] = useState<number[]>([0, 12, 7, 5, 12, 3, 7, 0]);
  const [shape, setShape] = useState<Shape>("tri");
  const [lfoRate, setLfoRate] = useState(200); // Hz
  const [lfoAmp, setLfoAmp] = useState(2.5); // V
  const [rate, setRate] = useState(0.5); // RATE knob 0=fast .. 1=slow
  const [winMs, setWinMs] = useState(600); // scope timebase (ms)

  const winS = winMs / 1000;
  const drive: Pt[] = useMemo(
    () => (source === "seq" ? seqPwl(steps, winS) : lfoPwl(shape, lfoRate, lfoAmp, 0, winS)),
    [source, steps, shape, lfoRate, lfoAmp, winS],
  );

  // The RATE knob starves the OTA bias (RATE_CV + CV_AMT toward the neg rail):
  // slew = IABC/C, and IABC shrinks toward the rail. The musical range is squeezed
  // near the rail, so map the knob with a cubic so the slow end gets most travel.
  // Floor the curve just shy of the rail so max RATE glides (~20ms) instead of
  // starving IABC to zero and freezing the integrator.
  const bias = RAIL + (BIAS_FAST - RAIL) * Math.max(Math.pow(1 - rate, 3.5), 0.0008);
  const payload: SimPayload = {
    pwl: drive,
    step_s: winS / 2000,
    stop_s: winS,
    cv: [
      { net: "RATE_CV", pwl: [[0, bias], [winS, bias]] },
      { net: "CV_AMT", pwl: [[0, bias], [winS, bias]] },
    ],
  };
  // Re-run whenever the drive, RATE, timebase, or a rebuild (version) changes.
  const sim = useSim(name, payload, [name, drive, bias, winS, version]);

  return (
    <section class="scope-section">
      <div class="scope-head">
        <h2>Scope</h2>
        <span class="scope-sub muted">play CV in · watch the response</span>
        <span class="spacer" />
        <div class="seg" role="group" aria-label="Signal source">
          <button aria-pressed={source === "seq"} onClick={() => setSource("seq")}>
            Sequencer
          </button>
          <button aria-pressed={source === "lfo"} onClick={() => setSource("lfo")}>
            LFO
          </button>
        </div>
      </div>

      <ScopeCanvas drive={drive} result={sim.data} winS={winS} loading={sim.loading} />
      {sim.error && <p class="scope-err error">{sim.error}</p>}

      <div class="scope-controls">
        {source === "seq" ? (
          <Sequencer steps={steps} onChange={setSteps} />
        ) : (
          <Lfo
            shape={shape}
            rate={lfoRate}
            amp={lfoAmp}
            onShape={setShape}
            onRate={setLfoRate}
            onAmp={setLfoAmp}
          />
        )}
        <div class="timebase">
          <label>
            rate
            <input
              type="range"
              min={0}
              max={1}
              step={0.01}
              value={rate}
              onInput={(e) => setRate(+(e.target as HTMLInputElement).value)}
            />
            <span class="mono">{rate < 0.5 ? "fast" : rate < 0.85 ? "slew" : "slow"}</span>
          </label>
          <label>
            timebase
            <input
              type="range"
              min={5}
              max={2000}
              step={5}
              value={winMs}
              onInput={(e) => setWinMs(+(e.target as HTMLInputElement).value)}
            />
            <span class="mono">{winMs} ms</span>
          </label>
        </div>
      </div>
    </section>
  );
}

// The 8-step 1V/oct sequencer: a vertical pitch slider per step.
function Sequencer({ steps, onChange }: { steps: number[]; onChange: (s: number[]) => void }) {
  const set = (i: number, v: number) => {
    const next = steps.slice();
    next[i] = v;
    onChange(next);
  };
  return (
    <div class="sequencer" role="group" aria-label="Pitch sequencer">
      {steps.map((s, i) => (
        <label class="step" key={i}>
          <input
            class="step-slider"
            type="range"
            min={-12}
            max={24}
            step={1}
            value={s}
            aria-label={`Step ${i + 1}`}
            onInput={(e) => set(i, +(e.target as HTMLInputElement).value)}
          />
          <span class="step-note mono">{noteName(s)}</span>
          <span class="step-n muted">{i + 1}</span>
        </label>
      ))}
    </div>
  );
}

function Lfo({
  shape,
  rate,
  amp,
  onShape,
  onRate,
  onAmp,
}: {
  shape: Shape;
  rate: number;
  amp: number;
  onShape: (s: Shape) => void;
  onRate: (n: number) => void;
  onAmp: (n: number) => void;
}) {
  return (
    <div class="lfo">
      <div class="seg" role="group" aria-label="LFO shape">
        {(["sine", "tri", "square"] as Shape[]).map((s) => (
          <button key={s} aria-pressed={shape === s} onClick={() => onShape(s)}>
            {s}
          </button>
        ))}
      </div>
      <label>
        rate
        <input
          type="range"
          min={20}
          max={2000}
          step={10}
          value={rate}
          onInput={(e) => onRate(+(e.target as HTMLInputElement).value)}
        />
        <span class="mono">{rate} Hz</span>
      </label>
      <label>
        amp
        <input
          type="range"
          min={0.5}
          max={5}
          step={0.1}
          value={amp}
          onInput={(e) => onAmp(+(e.target as HTMLInputElement).value)}
        />
        <span class="mono">{amp.toFixed(1)} V</span>
      </label>
    </div>
  );
}

// The CRT display: grid + the drive (dim) and probed output (bright phosphor).
function ScopeCanvas({
  drive,
  result,
  winS,
  loading,
}: {
  drive: Pt[];
  result: SimResult | null;
  winS: number;
  loading: boolean;
}) {
  const ref = useRef<HTMLCanvasElement | null>(null);

  useEffect(() => {
    const cv = ref.current;
    if (!cv) return;
    const dpr = window.devicePixelRatio || 1;
    const w = cv.clientWidth;
    const h = cv.clientHeight;
    cv.width = w * dpr;
    cv.height = h * dpr;
    const g = cv.getContext("2d");
    if (!g) return;
    g.setTransform(dpr, 0, 0, dpr, 0, 0);
    g.clearRect(0, 0, w, h);

    // Voltage range: symmetric around 0, covering both traces (min ±1.5V).
    let vmax = 1.5;
    for (const [, v] of drive) vmax = Math.max(vmax, Math.abs(v));
    if (result) for (const p of result.output) vmax = Math.max(vmax, Math.abs(p.v));
    vmax = Math.ceil(vmax * 1.15);

    const pad = 34;
    const x = (t: number) => pad + ((w - pad - 8) * t) / winS;
    const y = (v: number) => h / 2 - ((h / 2 - 12) * v) / vmax;

    // Grid + axes.
    g.strokeStyle = "rgba(80,200,140,0.13)";
    g.lineWidth = 1;
    g.beginPath();
    for (let i = 0; i <= 10; i++) {
      const gx = pad + ((w - pad - 8) * i) / 10;
      g.moveTo(gx, 8);
      g.lineTo(gx, h - 16);
    }
    for (let i = -2; i <= 2; i++) {
      const gy = y((vmax * i) / 2);
      g.moveTo(pad, gy);
      g.lineTo(w - 8, gy);
    }
    g.stroke();
    // Zero line.
    g.strokeStyle = "rgba(80,200,140,0.35)";
    g.beginPath();
    g.moveTo(pad, y(0));
    g.lineTo(w - 8, y(0));
    g.stroke();

    // Axis labels.
    g.fillStyle = "rgba(150,220,180,0.6)";
    g.font = "10px ui-monospace, monospace";
    g.textAlign = "right";
    g.fillText(`${vmax}V`, pad - 4, y(vmax) + 8);
    g.fillText(`0`, pad - 4, y(0) + 3);
    g.fillText(`-${vmax}V`, pad - 4, y(-vmax) - 2);
    g.textAlign = "center";
    g.fillText(`${(winS * 1000).toFixed(0)} ms`, w / 2, h - 3);

    // Drive trace (the commanded input — a stiff source, so it IS the IN node).
    const plot = (pts: Array<[number, number]>, stroke: string, glow: number) => {
      g.strokeStyle = stroke;
      g.lineWidth = 1.6;
      g.shadowColor = stroke;
      g.shadowBlur = glow;
      g.beginPath();
      pts.forEach(([t, v], i) => (i ? g.lineTo(x(t), y(v)) : g.moveTo(x(t), y(v))));
      g.stroke();
      g.shadowBlur = 0;
    };
    plot(drive, "rgba(240,180,90,0.85)", 4); // input — amber
    if (result) plot(result.output.map((p) => [p.t_s, p.v]), "rgba(90,240,160,0.95)", 7); // output — green

    // Legend + status.
    g.textAlign = "left";
    g.fillStyle = "rgba(240,180,90,0.9)";
    g.fillText("● in", pad + 4, 16);
    g.fillStyle = "rgba(90,240,160,0.95)";
    g.fillText(`● out ${result ? result.probe_net : ""}`, pad + 44, 16);
    if (loading) {
      g.fillStyle = "rgba(150,220,180,0.7)";
      g.textAlign = "right";
      g.fillText("running…", w - 12, 16);
    }
  }, [drive, result, winS, loading]);

  return <canvas class="scope-canvas" ref={ref} />;
}

// The control-surface placement editor (cll.2 / cll.3 / cll.4).
//
// Hand-place the 5-8 parts a person has an opinion about — jacks, pots,
// switches, LEDs — and leave the 40-odd passives to the placer. The auto-placer
// cannot make an ergonomics decision and never will; `crate::placement` says so
// in its own docs. This is where the human makes it.
//
// WHY THIS EDITS INTENT AND NOT COORDINATES. Every gesture here is written back
// as a `placement.toml` operation, not as an (x, y) pair. Drag a whole jack strip
// and the file still says "a column at x=6.0 on 13.5mm pitch"; change the pitch
// and all three move. That is the entire reason this exists instead of a KiCad
// drag, which would hand you three coordinates with the reason discarded.
//
// WHERE THE THINKING LIVES. Deciding which patterns survive a move — "is this
// still a column?" — is a fact about the file format, not about a UI, so it sits
// in `crate::placement_edit` and this file never reimplements it (DESIGN 2.2).
// A drag posts `targets` ("these end up here"); undo posts `restore` ("say this
// again"). What arrives back is the authored file, so the editor always shows
// what the file actually says rather than what it hoped.
//
// COHERENCE. Nothing here writes a cutout position. It writes placement.toml;
// the board follows the placement; the panel is derived back from the board.
// That chain is what stops the panel and the board drifting apart, so the editor
// stops at the first link and tells you to rebuild.

import { useCallback, useEffect, useMemo, useRef, useState } from "preact/hooks";

import {
  api,
  ApiError,
  type Controls,
  type PlacedPart,
  type PlacementDoc,
  type PlacementFile,
  type PlacementOp,
  type PlacementRequest,
  type PlacementWriteResult,
  type Point,
} from "./api";

// --- geometry ---------------------------------------------------------------

/** One Eurorack HP. The grid a panel is actually laid out on. */
const HP_MM = 5.08;
/** Snap radius in mm. Wide enough to catch, narrow enough not to fight. */
const SNAP_MM = 0.6;
/** Arrow-key nudge, and its shifted big brother. A knob 0.4mm off centre is a
 *  real defect and mouse precision will not find it. */
const NUDGE_MM = 0.1;
const NUDGE_BIG_MM = 1.0;

/** Where a gesture wants parts to end up. The server turns this into file ops. */
type Targets = Map<string, Point>;

/** A pattern flattened out of the file, with the fields the editor cares about. */
interface Pat {
  kind: "column" | "row" | "grid";
  refdes: string[];
  cols: number;
}

function patternsOf(file: PlacementFile): Pat[] {
  const p = file.patterns ?? {};
  return [
    ...(p.column ?? []).map((c) => ({
      kind: "column" as const,
      refdes: c.refdes,
      cols: 1,
    })),
    ...(p.row ?? []).map((r) => ({
      kind: "row" as const,
      refdes: r.refdes,
      cols: 1,
    })),
    ...(p.grid ?? []).map((g) => ({
      kind: "grid" as const,
      refdes: g.refdes,
      cols: g.cols,
    })),
  ];
}

/** Which pattern, if any, claims a part — for labelling and for warning that a
 *  part is about to leave one. */
function patternFor(file: PlacementFile, refdes: string): Pat | null {
  return patternsOf(file).find((p) => p.refdes.includes(refdes)) ?? null;
}

// --- snapping (cll.3) -------------------------------------------------------

interface Snap {
  value: number;
  guide: number | null;
}

/** Nearest candidate within `SNAP_MM`, or the value untouched. */
function snapTo(v: number, candidates: number[]): Snap {
  let best: number | null = null;
  let bestD = SNAP_MM;
  for (const c of candidates) {
    const d = Math.abs(c - v);
    if (d < bestD) {
      bestD = d;
      best = c;
    }
  }
  return best === null ? { value: v, guide: null } : { value: best, guide: best };
}

/**
 * What a dragged part can snap to on each axis.
 *
 * The HP grid because that is what a panel is laid out on; other parts' x/y
 * because "line this up with that" is the commonest intent and eyeballing it is
 * what goes wrong; the panel centreline because a single column of jacks
 * belongs on it; and the part's own envelope from each edge, which is the
 * closest that particular piece of hardware can legally sit.
 */
function snapCandidates(
  ctl: Controls,
  moving: Set<string>,
  envelope: [number, number],
): { xs: number[]; ys: number[] } {
  const xs: number[] = [ctl.width_mm / 2];
  const ys: number[] = [ctl.height_mm / 2];
  for (let g = 0; g <= ctl.width_mm + 1e-9; g += HP_MM / 2) xs.push(g);
  for (const p of ctl.parts) {
    if (!p.panel || moving.has(p.refdes)) continue;
    xs.push(p.x);
    ys.push(p.y);
  }
  const [ew, eh] = envelope;
  xs.push(ew / 2, ctl.width_mm - ew / 2);
  ys.push(eh / 2, ctl.height_mm - eh / 2);
  return { xs, ys };
}

// --- component --------------------------------------------------------------

const fmt = (n: number) => n.toFixed(2).replace(/\.?0+$/, "");

export function PlacementEditor({
  name,
  version,
  onSaved,
}: {
  name: string;
  version: number;
  onSaved?: (r: PlacementWriteResult) => void;
}) {
  const [ctl, setCtl] = useState<Controls | null>(null);
  const [doc, setDoc] = useState<PlacementDoc | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [sel, setSel] = useState<Set<string>>(new Set());
  const [drag, setDrag] = useState<{ dx: number; dy: number } | null>(null);
  const [marquee, setMarquee] = useState<{
    x0: number;
    y0: number;
    x1: number;
    y1: number;
  } | null>(null);
  const [guides, setGuides] = useState<{ x: number | null; y: number | null }>({
    x: null,
    y: null,
  });
  const [undoStack, setUndo] = useState<PlacementFile[]>([]);
  const [redoStack, setRedo] = useState<PlacementFile[]>([]);
  const [moved, setMoved] = useState<string | null>(null);
  const [pitch, setPitch] = useState("13.5");
  const [gridCols, setGridCols] = useState("2");

  const svgRef = useRef<SVGSVGElement>(null);

  useEffect(() => {
    let live = true;
    setError(null);
    Promise.all([api.controls(name), api.placement(name)])
      .then(([c, d]) => {
        if (!live) return;
        setCtl(c);
        setDoc(d);
      })
      .catch((e: unknown) =>
        setError(e instanceof Error ? e.message : String(e)),
      );
    return () => {
      live = false;
    };
  }, [name, version]);

  // Where each part is *now*: the placement file wins where it has an opinion,
  // otherwise the board shows where the placer put it. That difference is also
  // what tells the user an edit is pending a rebuild.
  const boardAt = useMemo(() => {
    const m = new Map<string, Point>();
    for (const p of ctl?.parts ?? []) m.set(p.refdes, { x: p.x, y: p.y });
    return m;
  }, [ctl]);

  const posOf = useCallback(
    (refdes: string): Point =>
      doc?.positions[refdes] ?? boardAt.get(refdes) ?? { x: 0, y: 0 },
    [doc, boardAt],
  );

  const panelParts = useMemo(
    () => (ctl?.parts ?? []).filter((p) => p.panel),
    [ctl],
  );

  /** Selection plus the live drag offset — what the canvas actually draws. */
  const shownAt = useCallback(
    (refdes: string): Point => {
      const p = posOf(refdes);
      if (drag && sel.has(refdes))
        return { x: p.x + drag.dx, y: p.y + drag.dy };
      return p;
    },
    [posOf, drag, sel],
  );

  // --- writing ---

  const send = useCallback(
    async (req: PlacementRequest, remember: "undo" | "redo" | "none") => {
      if (!doc) return;
      if ("ops" in req && req.ops.length === 0) return;
      const before = doc.file;
      setBusy(true);
      setError(null);
      try {
        const r = await api.savePlacement(name, req);
        setDoc({
          path: r.path,
          exists: r.written || doc.exists,
          file: r.file,
          positions: r.positions,
        });
        // A request that turned out to be a no-op is not a step to undo — and it
        // has no side effects, so a notice from the previous write must not
        // linger and look like this one's.
        setMoved(null);
        if (!r.written) return;
        if (remember === "undo") {
          setUndo((s) => [...s, before]);
          setRedo([]);
        } else if (remember === "redo") {
          setRedo((s) => [...s, before]);
        } else {
          setUndo((s) => [...s, before]);
        }
        setMoved(
          r.side_effects.length === 0
            ? null
            : r.side_effects
                .map(
                  (m) =>
                    `${m.refdes} moved ${fmt(
                      Math.hypot(m.to.x - m.from.x, m.to.y - m.from.y),
                    )}mm`,
                )
                .join(", "),
        );
        onSaved?.(r);
      } catch (e: unknown) {
        setError(
          e instanceof ApiError ? e.message : e instanceof Error ? e.message : String(e),
        );
      } finally {
        setBusy(false);
      }
    },
    [doc, name, onSaved],
  );

  /**
   * Move the selection to these points. Which patterns survive is the server's
   * call — see `placement_edit::ops_for_targets` — so drag, nudge, align and
   * mirror all preserve intent by exactly the same rule.
   */
  const place = useCallback(
    (targets: Targets) => send({ targets: Object.fromEntries(targets) }, "undo"),
    [send],
  );

  const shift = useCallback(
    (dx: number, dy: number) => {
      const t: Targets = new Map();
      for (const r of sel) {
        const p = posOf(r);
        t.set(r, { x: p.x + dx, y: p.y + dy });
      }
      place(t);
    },
    [sel, posOf, place],
  );

  // Undo asks for a whole file back rather than replaying an inverse gesture, so
  // it works for every tool — including ones not written yet — and still goes
  // through the ordinary writer, so comments and untouched tables survive.
  const undo = useCallback(() => {
    const prev = undoStack[undoStack.length - 1];
    if (!prev) return;
    setUndo((s) => s.slice(0, -1));
    send({ restore: prev }, "redo");
  }, [undoStack, send]);

  const redo = useCallback(() => {
    const next = redoStack[redoStack.length - 1];
    if (!next) return;
    setRedo((s) => s.slice(0, -1));
    send({ restore: next }, "none");
  }, [redoStack, send]);

  // --- pointer ---

  /** Client pixels → panel millimetres. */
  const toPanel = useCallback(
    (e: PointerEvent | MouseEvent): Point | null => {
      const svg = svgRef.current;
      if (!svg || !ctl) return null;
      const ctm = svg.getScreenCTM();
      if (!ctm) return null;
      const pt = new DOMPoint(e.clientX, e.clientY).matrixTransform(
        ctm.inverse(),
      );
      // SVG y runs down the screen; panel y runs up from the bottom edge.
      return { x: pt.x, y: ctl.height_mm - pt.y };
    },
    [ctl],
  );

  const startDrag = (e: PointerEvent, part: PlacedPart) => {
    e.stopPropagation();
    e.preventDefault();
    const origin = toPanel(e);
    if (!origin || !ctl) return;

    const additive = e.shiftKey || e.metaKey;
    const next = additive
      ? new Set(sel).add(part.refdes)
      : sel.has(part.refdes)
        ? sel
        : new Set([part.refdes]);
    setSel(next);

    const anchor = posOf(part.refdes);
    const env = part.envelope_mm ?? [0, 0];
    const cands = snapCandidates(ctl, next, env);
    const grab = { x: origin.x - anchor.x, y: origin.y - anchor.y };

    const onMove = (ev: PointerEvent) => {
      const p = toPanel(ev);
      if (!p) return;
      const want = { x: p.x - grab.x, y: p.y - grab.y };
      // Alt is the "get out of my way" modifier: the one time you need 0.3mm
      // off-grid, the tool must not fight you.
      if (ev.altKey) {
        setGuides({ x: null, y: null });
        setDrag({ dx: want.x - anchor.x, dy: want.y - anchor.y });
        return;
      }
      const sx = snapTo(want.x, cands.xs);
      const sy = snapTo(want.y, cands.ys);
      setGuides({ x: sx.guide, y: sy.guide });
      setDrag({ dx: sx.value - anchor.x, dy: sy.value - anchor.y });
    };
    const onUp = (ev: PointerEvent) => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
      window.removeEventListener("keydown", onEsc, true);
      setGuides({ x: null, y: null });
      const p = toPanel(ev);
      setDrag(null);
      if (!p) return;
      const want = { x: p.x - grab.x, y: p.y - grab.y };
      const fx = ev.altKey ? want.x : snapTo(want.x, cands.xs).value;
      const fy = ev.altKey ? want.y : snapTo(want.y, cands.ys).value;
      const dx = fx - anchor.x;
      const dy = fy - anchor.y;
      if (Math.abs(dx) < 1e-6 && Math.abs(dy) < 1e-6) return;
      const t: Targets = new Map();
      for (const r of next) {
        const q = posOf(r);
        t.set(r, { x: q.x + dx, y: q.y + dy });
      }
      place(t);
    };
    // Escape abandons the drag — a mis-drag on a 5-part panel is expensive.
    const onEsc = (ev: KeyboardEvent) => {
      if (ev.key !== "Escape") return;
      ev.stopPropagation();
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
      window.removeEventListener("keydown", onEsc, true);
      setDrag(null);
      setGuides({ x: null, y: null });
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
    window.addEventListener("keydown", onEsc, true);
  };

  const startMarquee = (e: PointerEvent) => {
    const start = toPanel(e);
    if (!start) return;
    if (!e.shiftKey) setSel(new Set());
    const base = e.shiftKey ? new Set(sel) : new Set<string>();
    const onMove = (ev: PointerEvent) => {
      const p = toPanel(ev);
      if (!p) return;
      setMarquee({ x0: start.x, y0: start.y, x1: p.x, y1: p.y });
      const [lo, hi] = [Math.min(start.x, p.x), Math.max(start.x, p.x)];
      const [bo, bi] = [Math.min(start.y, p.y), Math.max(start.y, p.y)];
      const hit = new Set(base);
      for (const q of panelParts) {
        const at = posOf(q.refdes);
        if (at.x >= lo && at.x <= hi && at.y >= bo && at.y <= bi)
          hit.add(q.refdes);
      }
      setSel(hit);
    };
    const onUp = () => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
      setMarquee(null);
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
  };

  // --- keyboard ---

  const onKeyDown = (e: KeyboardEvent) => {
    const mod = e.metaKey || e.ctrlKey;
    if (mod && e.key.toLowerCase() === "z") {
      e.preventDefault();
      if (e.shiftKey) redo();
      else undo();
      return;
    }
    if (e.key === "Escape") {
      setSel(new Set());
      return;
    }
    if (mod && e.key.toLowerCase() === "a") {
      e.preventDefault();
      setSel(new Set(panelParts.map((p) => p.refdes)));
      return;
    }
    const step = e.shiftKey ? NUDGE_BIG_MM : NUDGE_MM;
    const by: Record<string, [number, number]> = {
      ArrowLeft: [-step, 0],
      ArrowRight: [step, 0],
      ArrowUp: [0, step],
      ArrowDown: [0, -step],
    };
    const d = by[e.key];
    if (!d || sel.size === 0) return;
    e.preventDefault();
    shift(d[0], d[1]);
  };

  // --- tools (cll.3 alignment, cll.4 spacing) ---

  const selected = useMemo(() => [...sel], [sel]);

  const alignTo = (axis: "x" | "y", how: "min" | "max" | "mid" | number) => {
    if (selected.length === 0) return;
    const vals = selected.map((r) => posOf(r)[axis]);
    const to =
      typeof how === "number"
        ? how
        : how === "min"
          ? Math.min(...vals)
          : how === "max"
            ? Math.max(...vals)
            : (Math.min(...vals) + Math.max(...vals)) / 2;
    const t: Targets = new Map();
    for (const r of selected) {
      const p = posOf(r);
      t.set(r, axis === "x" ? { x: to, y: p.y } : { x: p.x, y: to });
    }
    place(t);
  };

  const mirror = () => {
    if (!ctl || selected.length === 0) return;
    const t: Targets = new Map();
    for (const r of selected) {
      const p = posOf(r);
      t.set(r, { x: ctl.width_mm - p.x, y: p.y });
    }
    place(t);
  };

  /**
   * The selection ordered along an axis — the one it is most spread over unless
   * the caller names one. A column reads bottom-to-top, a row left-to-right,
   * which is the order `positions()` expands them in.
   */
  const ordered = useCallback(
    (axis?: "column" | "row") => {
      const pts = selected.map((r) => ({ r, p: posOf(r) }));
      const span = (f: (q: Point) => number) =>
        Math.max(...pts.map((q) => f(q.p))) - Math.min(...pts.map((q) => f(q.p)));
      const vertical =
        axis === "column" ||
        (axis !== "row" && span((p) => p.y) >= span((p) => p.x));
      pts.sort((a, b) => (vertical ? a.p.y - b.p.y : a.p.x - b.p.x));
      return { pts, vertical };
    },
    [selected, posOf],
  );

  /** The spacing the selection already has — the starting point a typed pitch
   *  replaces. Inferring it beats making someone measure their own board. */
  const derivedPitch = useCallback(
    (axis?: "column" | "row"): number | null => {
      if (selected.length < 2) return null;
      const { pts, vertical } = ordered(axis);
      const n = pts.length;
      const span = vertical
        ? pts[n - 1].p.y - pts[0].p.y
        : pts[n - 1].p.x - pts[0].p.x;
      return span / (n - 1);
    },
    [selected, ordered],
  );

  // Selecting a strip prefills the pitch with what it currently is, so the field
  // starts from the board rather than from a number nobody chose.
  useEffect(() => {
    const p = derivedPitch();
    if (p !== null && Math.abs(p) > 1e-6) setPitch(fmt(Math.abs(p)));
    // Only when the selection itself changes — retyping must not be clobbered.
  }, [sel]); // eslint-disable-line react-hooks/exhaustive-deps

  /** Set pitch / distribute: the operation and the file format are one thing. */
  const strip = (mode: "pitch" | "distribute", axis?: "column" | "row") => {
    if (!doc || selected.length < 2) return;
    const { pts, vertical } = ordered(axis);
    const step =
      mode === "distribute" ? derivedPitch(axis) : Number(pitch);
    if (step === null || !Number.isFinite(step) || step === 0) {
      setError("pitch must be a non-zero number of millimetres");
      return;
    }
    makePattern(vertical ? "column" : "row", pts, step);
  };

  const makePattern = (
    kind: "column" | "row",
    pts: { r: string; p: Point }[],
    step: number,
  ) => {
    if (!doc) return;
    const refdes = pts.map((q) => q.r);
    const first = pts[0].p;
    // An override wins over a pattern, so a part that has one would not join the
    // strip. Clear first — ops apply in order, so this is still one write.
    const ops: PlacementOp[] = refdes
      .filter((r) => r in (doc.file.controls ?? {}))
      .map((r) => ({ op: "clear_control", refdes: r }));
    ops.push(
      kind === "column"
        ? { op: "set_column", refdes, x: first.x, from_y: first.y, pitch: step }
        : { op: "set_row", refdes, y: first.y, from_x: first.x, pitch: step },
    );
    send({ ops }, "undo");
  };

  const makeGrid = () => {
    if (!doc || selected.length < 2) return;
    const cols = Math.max(1, Math.floor(Number(gridCols) || 1));
    // Read across then down, which is how `positions()` expands a grid.
    const pts = selected
      .map((r) => ({ r, p: posOf(r) }))
      .sort((a, b) => b.p.y - a.p.y || a.p.x - b.p.x);
    const step = Number(pitch);
    if (!Number.isFinite(step) || step === 0) {
      setError("pitch must be a non-zero number of millimetres");
      return;
    }
    const ops: PlacementOp[] = pts
      .map((q) => q.r)
      .filter((r) => r in (doc.file.controls ?? {}))
      .map((r) => ({ op: "clear_control", refdes: r }));
    ops.push({
      op: "set_grid",
      refdes: pts.map((q) => q.r),
      x: pts[0].p.x,
      y: pts[0].p.y,
      cols,
      pitch_x: step,
      pitch_y: step,
    });
    send({ ops }, "undo");
  };

  const breakPattern = () => {
    if (selected.length === 0) return;
    send({ ops: [{ op: "break_pattern", refdes: selected }] }, "undo");
  };

  const rejoin = () => {
    if (!doc) return;
    const ops: PlacementOp[] = selected
      .filter((r) => r in (doc.file.controls ?? {}))
      .map((r) => ({ op: "clear_control", refdes: r }));
    send({ ops }, "undo");
  };

  // --- render ---

  if (error && !ctl)
    return <p class="place-error">Placement editor unavailable — {error}</p>;
  if (!ctl || !doc) return <p class="muted">Reading the board…</p>;
  if (!ctl.built)
    return (
      <p class="muted">
        Nothing to place yet — run <code>lob build {name}</code> so the editor
        knows which parts exist and where the placer put them.
      </p>
    );

  const W = ctl.width_mm;
  const H = ctl.height_mm;
  const PAD = 6;
  // Panel y is measured up from the bottom edge; SVG y runs down the screen.
  const sy = (y: number) => H - y;

  const pending = panelParts.filter((p) => {
    const a = posOf(p.refdes);
    return Math.hypot(a.x - p.x, a.y - p.y) > 0.05;
  });

  const selPattern =
    selected.length > 0 ? patternFor(doc.file, selected[0]) : null;
  // A whole strip selected moves as a strip; part of one means those members
  // leave it. Saying which is the difference between a tool that keeps intent
  // and one that quietly shreds it.
  const wholePattern =
    selPattern !== null &&
    selPattern.refdes.length === selected.length &&
    selPattern.refdes.every((r) => sel.has(r));
  const leaving = selected.filter((r) => {
    const p = patternFor(doc.file, r);
    return p !== null && !p.refdes.every((m) => sel.has(m));
  });

  return (
    <div class="place">
      <div class="place-bar">
        <span class="place-sel">
          {selected.length === 0
            ? "Nothing selected"
            : selected.length === 1
              ? `${selected[0]} · ${fmt(posOf(selected[0]).x)}, ${fmt(posOf(selected[0]).y)} mm`
              : `${selected.length} selected`}
        </span>
        {wholePattern && (
          <span class="place-tag" title="Moving this moves the whole pattern">
            {selPattern.kind} of {selPattern.refdes.length}
          </span>
        )}
        {leaving.length > 0 && (
          <span
            class="place-tag warn"
            title="These are part of a pattern; moving them writes an override so they leave it — the rest of the strip stays put"
          >
            {leaving.join(", ")} will leave its pattern
          </span>
        )}
        <span class="spacer" />
        <button
          class="btn tiny"
          disabled={busy || undoStack.length === 0}
          onClick={undo}
          title="Undo (⌘Z)"
        >
          Undo
        </button>
        <button
          class="btn tiny"
          disabled={busy || redoStack.length === 0}
          onClick={redo}
          title="Redo (⇧⌘Z)"
        >
          Redo
        </button>
      </div>

      <div class="place-tools" role="group" aria-label="Alignment">
        <span class="place-tool-label">Align</span>
        <button class="btn tiny" disabled={busy || selected.length < 2} onClick={() => alignTo("x", "min")}>
          Left
        </button>
        <button class="btn tiny" disabled={busy || selected.length < 2} onClick={() => alignTo("x", "mid")}>
          Centre X
        </button>
        <button class="btn tiny" disabled={busy || selected.length < 2} onClick={() => alignTo("x", "max")}>
          Right
        </button>
        <button class="btn tiny" disabled={busy || selected.length < 2} onClick={() => alignTo("y", "max")}>
          Top
        </button>
        <button class="btn tiny" disabled={busy || selected.length < 2} onClick={() => alignTo("y", "mid")}>
          Middle
        </button>
        <button class="btn tiny" disabled={busy || selected.length < 2} onClick={() => alignTo("y", "min")}>
          Bottom
        </button>
        <button
          class="btn tiny"
          disabled={busy || selected.length === 0}
          onClick={() => alignTo("x", W / 2)}
          title="Centre on the panel's vertical axis — the one people eyeball wrongly"
        >
          On panel axis
        </button>
        <button
          class="btn tiny"
          disabled={busy || selected.length === 0}
          onClick={mirror}
          title="Mirror the selection about the panel centreline"
        >
          Mirror
        </button>
      </div>

      <div class="place-tools" role="group" aria-label="Spacing">
        <span class="place-tool-label">Space</span>
        <input
          class="place-pitch"
          type="number"
          step="0.1"
          min="0"
          value={pitch}
          aria-label="Pitch in millimetres"
          onInput={(e) => setPitch((e.target as HTMLInputElement).value)}
        />
        <span class="place-unit">mm</span>
        {/* The HP grid a panel is laid out on, plus the jack pitch this repo's
            shipped panel actually uses. Not invented numbers. */}
        {[HP_MM, 2 * HP_MM, 16, 3 * HP_MM].map((p) => (
          <button
            key={p}
            class="btn tiny ghost"
            onClick={() => setPitch(String(Number(p.toFixed(2))))}
            title={
              p === 16
                ? "The jack pitch on the shipped slew-limiter panel"
                : `${Math.round(p / HP_MM)} HP`
            }
          >
            {fmt(p)}
          </button>
        ))}
        <button
          class="btn tiny"
          disabled={busy || selected.length < 2}
          onClick={() => strip("pitch")}
          title="Space the selection at exactly this pitch, along the axis it is already spread over"
        >
          Set pitch
        </button>
        <button
          class="btn tiny"
          disabled={busy || selected.length < 3}
          onClick={() => strip("distribute")}
          title="Even spacing between the two outermost parts — pitch derived, not given"
        >
          Distribute
        </button>
        <span class="place-gap" />
        {/* The axis is inferred by "Set pitch"; these force it, for a selection
            that is nearly square or that is about to become a strip. */}
        <button
          class="btn tiny"
          disabled={busy || selected.length < 2}
          onClick={() => strip("pitch", "column")}
          title="Make a column at this pitch"
        >
          Make column
        </button>
        <button
          class="btn tiny"
          disabled={busy || selected.length < 2}
          onClick={() => strip("pitch", "row")}
          title="Make a row at this pitch"
        >
          Make row
        </button>
        <span class="place-gap" />
        <input
          class="place-cols"
          type="number"
          step="1"
          min="1"
          value={gridCols}
          aria-label="Grid columns"
          onInput={(e) => setGridCols((e.target as HTMLInputElement).value)}
        />
        <button class="btn tiny" disabled={busy || selected.length < 2} onClick={makeGrid}>
          Make grid
        </button>
        <span class="place-gap" />
        <button
          class="btn tiny"
          disabled={busy || selected.length === 0}
          onClick={breakPattern}
          title="Expand the pattern to explicit positions — nothing moves"
        >
          Break
        </button>
        <button
          class="btn tiny"
          disabled={busy || selected.length === 0}
          onClick={rejoin}
          title="Drop the explicit override so the part rejoins its pattern"
        >
          Rejoin
        </button>
      </div>

      {/* tabindex so the canvas can take arrow keys — 0.1mm precision is not a
          thing a mouse can deliver. */}
      <div
        class="place-canvas"
        tabIndex={0}
        onKeyDown={onKeyDown}
        role="application"
        aria-label="Panel placement"
      >
        <svg
          ref={svgRef}
          viewBox={`${-PAD} ${-PAD} ${W + 2 * PAD} ${H + 2 * PAD}`}
          preserveAspectRatio="xMidYMid meet"
          onPointerDown={startMarquee}
        >
          <rect
            class="place-panel"
            x={0}
            y={0}
            width={W}
            height={H}
            rx={1.5}
          />
          {/* The HP grid this panel is actually laid out on. */}
          {Array.from({ length: Math.floor(W / HP_MM) }, (_, i) => (
            <line
              key={i}
              class="place-grid"
              x1={(i + 1) * HP_MM}
              y1={0}
              x2={(i + 1) * HP_MM}
              y2={H}
            />
          ))}

          {/* Board internals: context, not cargo. Nobody hand-places an 0603,
              but placing a jack strip blind to what is under it is how a
              control ends up on top of an op-amp. */}
          {(ctl.parts ?? [])
            .filter((p) => !p.panel)
            .map((p) => (
              <circle
                key={p.refdes}
                class="place-internal"
                cx={p.x}
                cy={sy(p.y)}
                r={0.6}
              >
                <title>
                  {p.refdes} {p.value} — placed automatically
                </title>
              </circle>
            ))}

          {panelParts.map((p) => {
            const at = shownAt(p.refdes);
            const on = sel.has(p.refdes);
            const [ew, eh] = p.envelope_mm ?? [8, 8];
            return (
              <g
                key={p.refdes}
                class={`place-part k-${p.kind ?? "other"}${on ? " on" : ""}`}
                onPointerDown={(e) => startDrag(e as PointerEvent, p)}
              >
                {/* The envelope, not the hole: a knob's skirt is what fouls its
                    neighbour, and that collision is invisible in a footprint view. */}
                <rect
                  class="place-env"
                  x={at.x - ew / 2}
                  y={sy(at.y) - eh / 2}
                  width={ew}
                  height={eh}
                  rx={1}
                />
                {p.hole?.shape === "rect" ? (
                  <rect
                    class="place-hole"
                    x={at.x - p.hole.width_mm / 2}
                    y={sy(at.y) - p.hole.height_mm / 2}
                    width={p.hole.width_mm}
                    height={p.hole.height_mm}
                    rx={p.hole.corner_radius_mm}
                  />
                ) : (
                  <circle
                    class="place-hole"
                    cx={at.x}
                    cy={sy(at.y)}
                    r={(p.hole?.shape === "circle" ? p.hole.diameter_mm : 6) / 2}
                  />
                )}
                <text class="place-label" x={at.x} y={sy(at.y) - eh / 2 - 0.8}>
                  {p.refdes}
                </text>
                <title>
                  {p.refdes} {p.value} — {fmt(at.x)}, {fmt(at.y)} mm
                </title>
              </g>
            );
          })}

          {guides.x !== null && (
            <line class="place-guide" x1={guides.x} y1={0} x2={guides.x} y2={H} />
          )}
          {guides.y !== null && (
            <line
              class="place-guide"
              x1={0}
              y1={sy(guides.y)}
              x2={W}
              y2={sy(guides.y)}
            />
          )}
          {marquee && (
            <rect
              class="place-marquee"
              x={Math.min(marquee.x0, marquee.x1)}
              y={sy(Math.max(marquee.y0, marquee.y1))}
              width={Math.abs(marquee.x1 - marquee.x0)}
              height={Math.abs(marquee.y1 - marquee.y0)}
            />
          )}
        </svg>
      </div>

      <p class="place-hint muted">
        click to select · shift or drag a box for several · arrows nudge 0.1mm,
        with shift 1mm · hold alt to ignore snapping · esc abandons a drag
      </p>
      {error && <p class="place-error">{error}</p>}
      {moved && (
        <p class="place-moved">
          Reshaping the pattern also moved {moved} — nothing you dragged.
        </p>
      )}
      {pending.length > 0 && (
        <p class="place-pending">
          {pending.length} control{pending.length === 1 ? "" : "s"} moved since
          the last build. The board follows the placement, and the panel follows
          the board — run <code>lob build {name}</code> (or the Build button) to
          see it.
        </p>
      )}
      <p class="place-file muted">
        writing <code>{doc.path}</code>
        {doc.exists ? "" : " (new)"}
      </p>
    </div>
  );
}

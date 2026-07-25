import { useEffect, useRef, useState } from "preact/hooks";
import type { ComponentChildren } from "preact";
import hljs from "highlight.js/lib/core";
import python from "highlight.js/lib/languages/python";
import {
  api,
  artifactUrl,
  type Artifact,
  type Bom,
  type Circuit,
  type EditResult,
  type ManifestEditPayload,
  type Orders,
  type RepoInfo,
  type SourceDoc,
} from "./api.ts";
import { ScopeSection } from "./scope.tsx";

hljs.registerLanguage("python", python);

// ---------------------------------------------------------------------------
// Tiny path router (no dependency; the server's SPA fallback serves index.html
// for any non-asset path, so pushState routing Just Works).
// ---------------------------------------------------------------------------

function usePath(): string {
  const [path, setPath] = useState(location.pathname);
  useEffect(() => {
    const on = () => setPath(location.pathname);
    addEventListener("popstate", on);
    return () => removeEventListener("popstate", on);
  }, []);
  return path;
}

function navigate(to: string) {
  if (to === location.pathname) return;
  history.pushState({}, "", to);
  dispatchEvent(new PopStateEvent("popstate"));
}

function Link({ to, children }: { to: string; children: ComponentChildren }) {
  return (
    <a
      href={to}
      onClick={(e) => {
        e.preventDefault();
        navigate(to);
      }}
    >
      {children}
    </a>
  );
}

// A minimal load-once async hook with loading/error states.
function useAsync<T>(load: () => Promise<T>, deps: unknown[]) {
  const [state, setState] = useState<{
    data: T | null;
    error: string | null;
    loading: boolean;
  }>({ data: null, error: null, loading: true });
  useEffect(() => {
    let live = true;
    setState({ data: null, error: null, loading: true });
    load()
      .then((data) => live && setState({ data, error: null, loading: false }))
      .then(undefined, (e) =>
        live && setState({ data: null, error: String(e), loading: false }),
      );
    return () => {
      live = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);
  return state;
}

// ---------------------------------------------------------------------------
// App shell
// ---------------------------------------------------------------------------

export function App() {
  const path = usePath();
  const scope = path.match(/^\/c\/([^/]+)\/scope$/);
  const circuit = path.match(/^\/c\/([^/]+)$/);
  return (
    <div class="app">
      <TopBar />
      {scope ? (
        <ScopePage name={decodeURIComponent(scope[1])} />
      ) : circuit ? (
        // The circuit workspace is a full-width IDE split, not the narrow column.
        <CircuitPage name={decodeURIComponent(circuit[1])} />
      ) : (
        <main class="content">
          <HomePage />
        </main>
      )}
    </div>
  );
}

// The scope is its own full page (bead 5hr): a dedicated bench for playing CV
// into the module and watching the response.
function ScopePage({ name }: { name: string }) {
  const board = useLiveCircuit(name, 0).circuit?.artifacts.find(
    (a) => a.kind === "board",
  );
  const version = board?.mtime_ms ?? 0;
  return (
    <main class="content scope-page">
      <div class="crumbs">
        <Link to={`/c/${encodeURIComponent(name)}`}>← {name}</Link>
      </div>
      <div class="head-row">
        <h1>{name} · scope</h1>
        <span class="spacer" />
        <LiveBadge version={version} />
      </div>
      <ScopeSection name={name} version={version} />
    </main>
  );
}

// Poll the circuit detail so the workspace tracks the repo LIVE — when a `lob
// build` rewrites the out/ tree, the renders / BOM / freshness update on their
// own, and you watch the module change instead of reloading.
function useLiveCircuit(name: string, reloadKey: number) {
  const [circuit, setCircuit] = useState<Circuit | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    setCircuit(null);
    const tick = () =>
      api.circuit(name).then(
        (c) => {
          if (live) {
            setCircuit(c);
            setError(null);
          }
        },
        (e) => live && setError(String(e)),
      );
    tick();
    const id = setInterval(tick, 2500);
    return () => {
      live = false;
      clearInterval(id);
    };
  }, [name, reloadKey]);
  return { circuit, error };
}

// A pulsing "live" indicator + when the board was last (re)built.
function LiveBadge({ version }: { version: number }) {
  const [, bump] = useState(0);
  useEffect(() => {
    const id = setInterval(() => bump((n) => n + 1), 1000);
    return () => clearInterval(id);
  }, []);
  const ago = version ? relTime(Date.now() - version) : null;
  return (
    <span class="live-badge" title="Auto-refreshing every 2.5s">
      <span class="live-dot" /> live{ago ? ` · board built ${ago}` : ""}
    </span>
  );
}

function relTime(ms: number): string {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 60) return `${s}s ago`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m}m ago`;
  return `${Math.round(m / 60)}h ago`;
}

function TopBar() {
  const { data } = useAsync<RepoInfo>(() => api.repo(), []);
  return (
    <header class="topbar">
      <Link to="/">
        <span class="brand">{data?.brand ?? "legion-of-bom"}</span>
      </Link>
      {data?.name && <span class="repo-name">{data.name}</span>}
    </header>
  );
}

// ---------------------------------------------------------------------------
// Home: repo masthead + circuit list
// ---------------------------------------------------------------------------

function HomePage() {
  const [reloadKey, setReloadKey] = useState(0);
  const [saved, setSaved] = useState<EditResult | null>(null);
  const repo = useAsync<RepoInfo>(() => api.repo(), [reloadKey]);
  const list = useAsync<Circuit[]>(() => api.circuits(), []);

  if (repo.error) return <NotARepo error={repo.error} />;
  const onSaved = (r: EditResult) => {
    setSaved(r);
    setReloadKey((k) => k + 1);
  };
  return (
    <>
      <div class="masthead">
        <h1>{repo.data?.name ?? "…"}</h1>
        {repo.data && <BrandLine brand={repo.data.brand} onSaved={onSaved} />}
        {repo.data && <p class="path">{repo.data.root}</p>}
      </div>
      {saved && <StagedNote result={saved} />}
      {list.loading && <p class="muted">Loading circuits…</p>}
      {list.error && <p class="error">{list.error}</p>}
      {list.data && list.data.length === 0 && (
        <p class="muted">No circuits declared in lob.toml.</p>
      )}
      <ul class="circuit-list">
        {list.data?.map((c) => (
          <li key={c.name}>
            <Link to={`/c/${encodeURIComponent(c.name)}`}>
              <span class="c-name">{c.name}</span>
            </Link>
            <span class="c-meta">
              {c.kit && <span class="tag">{c.kit}</span>}
              {c.has_build_copy && <span class="tag ghost">build copy</span>}
            </span>
            <ArtifactChips artifacts={c.artifacts} name={c.name} compact />
          </li>
        ))}
      </ul>
    </>
  );
}

function NotARepo({ error }: { error: string }) {
  return (
    <div class="masthead">
      <h1>No circuits repo</h1>
      <p class="error">{error}</p>
      <p class="muted">Run <code>lob serve</code> from inside a repo with a lob.toml.</p>
    </div>
  );
}

// The repo brand line, editable in place (writes [repo].brand → stage).
function BrandLine({
  brand,
  onSaved,
}: {
  brand: string | null;
  onSaved: (r: EditResult) => void;
}) {
  const [editing, setEditing] = useState(false);
  const [value, setValue] = useState(brand ?? "");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  if (!editing) {
    return (
      <p class="sub brand-line">
        {brand ?? <span class="muted">no brand</span>}{" "}
        <button
          class="btn tiny"
          onClick={() => {
            setValue(brand ?? "");
            setErr(null);
            setEditing(true);
          }}
        >
          edit
        </button>
      </p>
    );
  }

  const save = async () => {
    setBusy(true);
    setErr(null);
    try {
      const r = await api.edit({ brand: value });
      setEditing(false);
      onSaved(r);
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <p class="sub brand-line editing">
      <input
        value={value}
        placeholder="Brand line"
        onInput={(e) => setValue(val(e))}
      />
      <button class="btn tiny primary" disabled={busy} onClick={save}>
        {busy ? "…" : "Save & stage"}
      </button>
      <button class="btn tiny" disabled={busy} onClick={() => setEditing(false)}>
        cancel
      </button>
      {err && <span class="error"> {err}</span>}
    </p>
  );
}

// ---------------------------------------------------------------------------
// Circuit detail
// ---------------------------------------------------------------------------

function CircuitPage({ name }: { name: string }) {
  const [reloadKey, setReloadKey] = useState(0);
  const [saved, setSaved] = useState<EditResult | null>(null);
  const [codeOpen, setCodeOpen] = useState(true);
  const { circuit, error } = useLiveCircuit(name, reloadKey);

  if (!circuit) {
    return (
      <main class="content">
        {error ? <p class="error">{error}</p> : <p class="muted">Loading…</p>}
      </main>
    );
  }

  const onSaved = (r: EditResult) => {
    setSaved(r);
    setReloadKey((k) => k + 1);
  };
  // The board's mtime is the cache-buster: renders + BOM re-fetch when it changes.
  const board = circuit.artifacts.find((a) => a.kind === "board");
  const version = board?.mtime_ms ?? 0;

  return (
    <div class={`ide${codeOpen ? "" : " code-collapsed"}`}>
      <aside class="code-pane">
        <div class="pane-head">
          <span class="pane-title mono">{circuit.source}</span>
          <button class="btn tiny" onClick={() => setCodeOpen(false)} title="Collapse">
            ‹ hide
          </button>
        </div>
        <SourceView name={circuit.name} reloadKey={reloadKey} />
      </aside>

      {!codeOpen && (
        <button class="code-reopen" onClick={() => setCodeOpen(true)} title="Show source">
          code ›
        </button>
      )}

      <main class="work-pane">
        <div class="crumbs">
          <Link to="/">← all circuits</Link>
        </div>
        <div class="head-row">
          <h1>{circuit.name}</h1>
          {circuit.panel_hp != null && (
            <span class="tag hp-tag" title="Panel width">{circuit.panel_hp} HP</span>
          )}
          {circuit.kit && <span class="tag">{circuit.kit}</span>}
          <span class="spacer" />
          <Link to={`/c/${encodeURIComponent(circuit.name)}/scope`}>
            <span class="btn tiny scope-link">◠ Scope</span>
          </Link>
          <LiveBadge version={version} />
        </div>

        {saved && <StagedNote result={saved} />}

        <Renders circuit={circuit} version={version} />
        <BomSection name={circuit.name} version={version} />
        <MetadataEditor circuit={circuit} onSaved={onSaved} />

        <Section title="Artifacts">
          <ArtifactChips artifacts={circuit.artifacts} name={circuit.name} />
        </Section>

        <EmbeddedDoc artifacts={circuit.artifacts} label="guide" title="Build guide" />
        <EmbeddedDoc artifacts={circuit.artifacts} label="vbom" title="Visual BOM" />

        <OrdersSection name={circuit.name} />
      </main>
    </div>
  );
}

// The PCB (an interactive render↔layout viewer) + the panel. Rendered on demand
// by the backend — slow the first time (kicad-cli), cached after.
function Renders({ circuit, version }: { circuit: Circuit; version: number }) {
  const base = `/api/circuits/${encodeURIComponent(circuit.name)}/render`;
  const built = circuit.artifacts.some(
    (a) => a.kind === "board" && a.status !== "missing",
  );
  if (!built && !circuit.panel) return null;
  return (
    <Section title="Board &amp; panel">
      {!built && (
        <p class="muted">
          Board not built — run <code>lob build {circuit.name}</code>.
        </p>
      )}
      <div class="board-panel">
        {built && <PcbViewer name={circuit.name} version={version} />}
        {circuit.panel && (
          <div class="panel-strip">
            {/* The panel is independent of the board — cache-bust it on the panel
                spec's own mtime so a board rebuild doesn't re-fetch it. */}
            <RenderImage
              src={`${base}?view=panel&v=${circuit.panel_mtime_ms ?? 0}`}
              caption={panelCaption(circuit)}
            />
          </div>
        )}
      </div>
    </Section>
  );
}

// "Panel · 8 HP · black" from whatever the spec declares.
function panelCaption(c: Circuit): string {
  const bits = ["Panel"];
  if (c.panel_hp != null) bits.push(`${c.panel_hp} HP`);
  bits.push(c.panel_finish ?? "black");
  return bits.join(" · ");
}

const PCB_VIEWS: { key: string; label: string }[] = [
  { key: "board-top", label: "Render" },
  { key: "board-layout", label: "Layout" },
  { key: "board-bottom", label: "Bottom" },
  { key: "schematic", label: "Schematic" },
  { key: "gerber", label: "Gerber" },
];

// Fab layers, in the order the checklist shows them. Defaults are the stack you
// actually want to see first: both coppers, the outline and the holes.
const GERBER_LAYERS: { key: string; label: string; on: boolean }[] = [
  { key: "cu-top", label: "Copper top", on: true },
  { key: "cu-bot", label: "Copper bottom", on: true },
  { key: "silk-top", label: "Silk top", on: false },
  { key: "silk-bot", label: "Silk bottom", on: false },
  { key: "mask-top", label: "Mask top", on: false },
  { key: "mask-bot", label: "Mask bottom", on: false },
  { key: "paste-top", label: "Paste top", on: false },
  { key: "paste-bot", label: "Paste bottom", on: false },
  { key: "drill", label: "Drill", on: true },
  { key: "outline", label: "Outline", on: true },
  { key: "other", label: "Fab / courtyard", on: false },
];

// Toggle photoreal render ↔ 2D layout ↔ schematic, zoom + pan (hk0), plus an SMD
// filter (a2r) — off shows the through-hole-only board a mixed-kit builder solders.
function PcbViewer({ name, version }: { name: string; version: number }) {
  const [view, setView] = useState("board-top");
  const [smd, setSmd] = useState(true);
  const [layers, setLayers] = useState<string[]>(
    GERBER_LAYERS.filter((l) => l.on).map((l) => l.key),
  );
  const isBoard = view.startsWith("board-");
  const isGerber = view === "gerber";
  const src =
    `/api/circuits/${encodeURIComponent(name)}/render?view=${view}&v=${version}` +
    (isBoard && !smd ? "&smd=0" : "") +
    (isGerber ? `&layers=${layers.join(",")}` : "");
  const toggleLayer = (k: string) =>
    setLayers((cur) =>
      cur.includes(k) ? cur.filter((x) => x !== k) : [...cur, k],
    );
  return (
    <div class="pcb-viewer">
      <div class="pcb-bar">
        <div class="seg pcb-seg" role="group" aria-label="PCB view">
          {PCB_VIEWS.map((v) => (
            <button
              key={v.key}
              aria-pressed={view === v.key}
              onClick={() => setView(v.key)}
            >
              {v.label}
            </button>
          ))}
        </div>
        {isGerber && (
          <span class="layer-count muted">{layers.length} layers</span>
        )}
        {isBoard && (
          <label class="smd-toggle" title="Hide surface-mount parts — the through-hole board you solder">
            <input
              type="checkbox"
              checked={smd}
              onChange={(e) => setSmd((e.target as HTMLInputElement).checked)}
            />
            SMD
          </label>
        )}
      </div>
      {isGerber && (
        <div class="layer-list" role="group" aria-label="Gerber layers">
          {GERBER_LAYERS.map((l) => (
            <label key={l.key} class="layer-item">
              <input
                type="checkbox"
                checked={layers.includes(l.key)}
                onChange={() => toggleLayer(l.key)}
              />
              <span class={`swatch sw-${l.key}`} />
              {l.label}
            </label>
          ))}
        </div>
      )}
      {/* key=view remounts on toggle so the transform resets between views */}
      <ZoomPan key={`${view}-${smd}`} src={src} alt={`PCB ${view}`} />
      <p class="pcb-hint muted">scroll to zoom · drag to pan · double-click to reset</p>
    </div>
  );
}

// A pannable, zoomable image (wheel = zoom toward cursor, drag = pan). Vanilla —
// no dependency; works for both the raster render and the vector layout.
function ZoomPan({ src, alt }: { src: string; alt: string }) {
  const box = useRef<HTMLDivElement | null>(null);
  const [t, setT] = useState({ s: 1, x: 0, y: 0 });
  const [status, setStatus] = useState<"loading" | "ok" | "error">("loading");
  const drag = useRef<{ x: number; y: number } | null>(null);

  useEffect(() => {
    const el = box.current;
    if (!el) return;
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      const rect = el.getBoundingClientRect();
      const cx = e.clientX - rect.left;
      const cy = e.clientY - rect.top;
      const factor = e.deltaY < 0 ? 1.15 : 1 / 1.15;
      setT((p) => {
        const s = Math.min(12, Math.max(0.4, p.s * factor));
        const k = s / p.s;
        return { s, x: cx - (cx - p.x) * k, y: cy - (cy - p.y) * k };
      });
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
  }, []);

  const center = () => {
    const el = box.current;
    const img = el?.querySelector("img") as HTMLImageElement | null;
    setT({ s: 1, x: el && img ? Math.max(0, (el.clientWidth - img.clientWidth) / 2) : 0, y: 0 });
  };
  const onDown = (e: MouseEvent) => {
    drag.current = { x: e.clientX, y: e.clientY };
  };
  const onMove = (e: MouseEvent) => {
    if (!drag.current) return;
    const dx = e.clientX - drag.current.x;
    const dy = e.clientY - drag.current.y;
    drag.current = { x: e.clientX, y: e.clientY };
    setT((p) => ({ ...p, x: p.x + dx, y: p.y + dy }));
  };
  const onUp = () => {
    drag.current = null;
  };

  return (
    <div
      class="zoompan"
      ref={box}
      onMouseDown={onDown}
      onMouseMove={onMove}
      onMouseUp={onUp}
      onMouseLeave={onUp}
      onDblClick={center}
    >
      {status !== "ok" && (
        <div class="zp-status muted">
          {status === "loading" ? "rendering…" : "unavailable"}
        </div>
      )}
      <img
        src={src}
        alt={alt}
        draggable={false}
        class={status === "error" ? "hidden" : ""}
        style={`transform: translate(${t.x}px, ${t.y}px) scale(${t.s})`}
        onLoad={() => {
          setStatus("ok");
          center();
        }}
        onError={() => setStatus("error")}
      />
    </div>
  );
}

function RenderImage({ src, caption }: { src: string; caption: string }) {
  const [state, setState] = useState<"loading" | "ok" | "error">("loading");
  return (
    <figure class="render">
      <div class="render-frame">
        {state !== "error" && (
          <img
            src={src}
            alt={caption}
            loading="lazy"
            class={state === "loading" ? "hidden" : ""}
            onLoad={() => setState("ok")}
            onError={() => setState("error")}
          />
        )}
        {state === "loading" && <div class="render-status muted">rendering…</div>}
        {state === "error" && <div class="render-status muted">unavailable</div>}
      </div>
      <figcaption>{caption}</figcaption>
    </figure>
  );
}

// The circuit's SKiDL source, filling the code pane — read-only (topology stays
// code, DESIGN 1.3). Polled so an edit to the .py shows up live.
function SourceView({ name, reloadKey }: { name: string; reloadKey: number }) {
  const [doc, setDoc] = useState<SourceDoc | null>(null);
  useEffect(() => {
    let live = true;
    const tick = () =>
      api.source(name).then(
        (d) => live && setDoc(d),
        () => {},
      );
    tick();
    const id = setInterval(tick, 4000);
    return () => {
      live = false;
      clearInterval(id);
    };
  }, [name, reloadKey]);

  if (!doc) return <div class="src-loading muted">loading source…</div>;
  const lines = doc.content.split("\n").length;
  const html = hljs.highlight(doc.content, { language: "python" }).value;
  return (
    <div class="source-view">
      <pre class="source hljs">
        <code
          class="language-python"
          // highlight.js output; content is the repo's own source file.
          dangerouslySetInnerHTML={{ __html: html }}
        />
      </pre>
      <div class="src-foot muted">{lines} lines · read-only (edit in your editor)</div>
    </div>
  );
}

const KITS = ["auto", "tht", "smd", "mixed"];

// Build copy + kit — the only circuit fields a non-engineer edits. Topology
// stays code (DESIGN 1.3); nothing here can reach source/panel.
function MetadataEditor({
  circuit,
  onSaved,
}: {
  circuit: Circuit;
  onSaved: (r: EditResult) => void;
}) {
  const [editing, setEditing] = useState(false);
  const b = circuit.build;

  if (editing) {
    return (
      <BuildCopyForm
        circuit={circuit}
        onCancel={() => setEditing(false)}
        onSaved={(r) => {
          setEditing(false);
          onSaved(r);
        }}
      />
    );
  }

  return (
    <section class="card build-copy">
      <div class="card-head">
        <h3>Build copy</h3>
        <button class="btn" onClick={() => setEditing(true)}>
          Edit
        </button>
      </div>
      {b?.intro ? <p class="intro">{b.intro}</p> : <p class="muted">No intro yet.</p>}
      {b && b.tools.length > 0 && (
        <div>
          <h4>Tools</h4>
          <ul>{b.tools.map((t) => <li key={t}>{t}</li>)}</ul>
        </div>
      )}
      {b && b.cautions.length > 0 && (
        <div class="cautions">
          <h4>Before you start</h4>
          <ul>{b.cautions.map((t) => <li key={t}>{t}</li>)}</ul>
        </div>
      )}
      <p class="kit-line muted">Kit: {circuit.kit ?? "auto"}</p>
    </section>
  );
}

function BuildCopyForm({
  circuit,
  onCancel,
  onSaved,
}: {
  circuit: Circuit;
  onCancel: () => void;
  onSaved: (r: EditResult) => void;
}) {
  const b = circuit.build;
  const [intro, setIntro] = useState(b?.intro ?? "");
  const [tools, setTools] = useState((b?.tools ?? []).join("\n"));
  const [cautions, setCautions] = useState((b?.cautions ?? []).join("\n"));
  const [kit, setKit] = useState(circuit.kit ?? "auto");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  const save = async () => {
    setBusy(true);
    setErr(null);
    const payload: ManifestEditPayload = {
      circuits: [
        {
          name: circuit.name,
          build_intro: intro,
          build_tools: lines(tools),
          build_cautions: lines(cautions),
          kit,
        },
      ],
    };
    try {
      onSaved(await api.edit(payload));
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section class="card build-copy editing">
      <div class="card-head">
        <h3>Edit build copy</h3>
      </div>
      <label>
        Intro
        <textarea rows={4} value={intro} onInput={(e) => setIntro(val(e))} />
      </label>
      <label>
        Tools <span class="muted">(one per line)</span>
        <textarea rows={4} value={tools} onInput={(e) => setTools(val(e))} />
      </label>
      <label>
        Cautions <span class="muted">(one per line)</span>
        <textarea rows={3} value={cautions} onInput={(e) => setCautions(val(e))} />
      </label>
      <label class="kit-select">
        Kit
        <select value={kit} onChange={(e) => setKit(val(e))}>
          {KITS.map((k) => (
            <option key={k} value={k}>{k}</option>
          ))}
        </select>
      </label>
      {err && <p class="error">{err}</p>}
      <div class="form-actions">
        <button class="btn primary" disabled={busy} onClick={save}>
          {busy ? "Saving…" : "Save & stage"}
        </button>
        <button class="btn" disabled={busy} onClick={onCancel}>
          Cancel
        </button>
      </div>
      <p class="hint muted">
        Writes lob.toml and <code>git add</code>s it — never commits. Commit the
        staged batch yourself when ready.
      </p>
    </section>
  );
}

function StagedNote({ result }: { result: EditResult }) {
  if (result.staged) {
    return (
      <div class="banner ok">
        Saved &amp; staged — {result.staged_files.length} file
        {result.staged_files.length === 1 ? "" : "s"} staged (uncommitted). Commit
        when ready.
      </div>
    );
  }
  return (
    <div class="banner warn">
      Saved to lob.toml, but not staged{result.git_error ? `: ${result.git_error}` : ""}.
    </div>
  );
}

/** Split a textarea into trimmed, non-empty lines. */
function lines(s: string): string[] {
  return s
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l.length > 0);
}

/** The value of a form control from an input/change event. */
function val(e: Event): string {
  return (e.target as HTMLInputElement | HTMLTextAreaElement | HTMLSelectElement)
    .value;
}

function statusClass(s: string) {
  return s === "fresh" ? "ok" : s === "stale" ? "warn" : "off";
}

function ArtifactChips({
  artifacts,
  compact = false,
}: {
  artifacts: Artifact[];
  name: string;
  compact?: boolean;
}) {
  const shown = compact
    ? artifacts.filter((a) => a.status !== "missing")
    : artifacts;
  if (compact && shown.length === 0)
    return <span class="chips"><span class="chip off">not built</span></span>;
  return (
    <span class="chips">
      {shown.map((a) => {
        const cls = `chip ${statusClass(a.status)}`;
        const label = `${a.label}${a.status === "stale" ? " · stale" : ""}`;
        return a.status === "missing" ? (
          <span key={a.kind} class={cls}>{label}</span>
        ) : (
          <a
            key={a.kind}
            class={cls}
            href={artifactUrl(a.path)}
            target="_blank"
            rel="noopener"
          >
            {label}
          </a>
        );
      })}
    </span>
  );
}

function EmbeddedDoc({
  artifacts,
  label,
  title,
}: {
  artifacts: Artifact[];
  label: string;
  title: string;
}) {
  const a = artifacts.find((x) => x.label === label && x.status !== "missing");
  if (!a) return null;
  const url = artifactUrl(a.path);
  return (
    <Section title={title} extra={<a href={url} target="_blank" rel="noopener">open ↗</a>}>
      <iframe class="doc-frame" src={url} title={title} loading="lazy" />
    </Section>
  );
}

function BomSection({ name, version }: { name: string; version: number }) {
  const [price, setPrice] = useState(false);
  // `version` (board mtime) in the deps refetches the BOM when the board rebuilds.
  const bom = useAsync<Bom>(() => api.bom(name, price), [name, price, version]);

  return (
    <Section
      title="Bill of materials"
      extra={
        <label class="price-toggle">
          <input
            type="checkbox"
            checked={price}
            onChange={(e) => setPrice((e.target as HTMLInputElement).checked)}
          />{" "}
          live pricing
        </label>
      }
    >
      {bom.loading && <p class="muted">Loading…</p>}
      {bom.error && <p class="error">{bom.error}</p>}
      {bom.data && !bom.data.built && (
        <p class="muted">Not built yet — run <code>lob build {name}</code>.</p>
      )}
      {bom.data && bom.data.built && (
        <>
          {price && !bom.data.priced && (
            <p class="muted">No live prices (set MOUSER_API_KEY, or parts lack MPNs).</p>
          )}
          <table class="bom">
            <thead>
              <tr>
                <th>Refs</th>
                <th>Value</th>
                <th>Footprint</th>
                <th class="num">Qty</th>
                {bom.data.priced && <th class="num">Unit</th>}
                {bom.data.priced && <th class="num">Ext</th>}
              </tr>
            </thead>
            <tbody>
              {bom.data.lines.map((l, i) => (
                <tr key={i}>
                  <td>{l.refdes.join(", ")}</td>
                  <td>{l.value}</td>
                  <td class="mono">{l.footprint ?? "—"}</td>
                  <td class="num">{l.qty}</td>
                  {bom.data!.priced && <td class="num">{money(l.unit_price)}</td>}
                  {bom.data!.priced && <td class="num">{money(l.ext_price)}</td>}
                </tr>
              ))}
            </tbody>
            {bom.data.priced && bom.data.total != null && (
              <tfoot>
                <tr>
                  <td colSpan={5} class="num">Total</td>
                  <td class="num">{money(bom.data.total)}</td>
                </tr>
              </tfoot>
            )}
          </table>
        </>
      )}
    </Section>
  );
}

function OrdersSection({ name }: { name: string }) {
  const o = useAsync<Orders>(() => api.orders(name), [name]);
  if (o.loading || o.error) return null;
  const data = o.data!;
  return (
    <Section title="Panel orders">
      {!data.available && (
        <p class="muted">Order tracking unavailable (no dolt store).</p>
      )}
      {data.available && data.orders.length === 0 && (
        <p class="muted">No orders recorded.</p>
      )}
      {data.orders.length > 0 && (
        <table class="orders">
          <thead>
            <tr>
              <th>Vendor</th>
              <th>Status</th>
              <th>Ordered</th>
              <th>Tracking</th>
            </tr>
          </thead>
          <tbody>
            {data.orders.map((r) => (
              <tr key={r.id}>
                <td>{r.vendor ?? "—"}</td>
                <td><span class={`chip ${r.status === "received" ? "ok" : "warn"}`}>{r.status}</span></td>
                <td>{r.ordered_at ?? "—"}</td>
                <td class="mono">{r.tracking_ref ?? "—"}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Section>
  );
}

// ---------------------------------------------------------------------------
// Shared bits
// ---------------------------------------------------------------------------

function Section({
  title,
  extra,
  children,
}: {
  title: string;
  extra?: ComponentChildren;
  children: ComponentChildren;
}) {
  return (
    <section class="section">
      <div class="section-head">
        <h3>{title}</h3>
        {extra && <span class="section-extra">{extra}</span>}
      </div>
      {children}
    </section>
  );
}

function money(v: number | null): string {
  return v == null ? "—" : `$${v.toFixed(2)}`;
}

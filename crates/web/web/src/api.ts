// Typed client for the localhost dashboard API (p58.3). No auth, no CSRF
// (DESIGN 2.5) — a plain same-origin JSON fetch. Types mirror the server DTOs.

export class ApiError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

async function getJson<T>(path: string): Promise<T> {
  const resp = await fetch(path, { credentials: "same-origin" });
  return unwrap<T>(resp);
}

async function postJson<T>(path: string, body: unknown): Promise<T> {
  const resp = await fetch(path, {
    method: "POST",
    credentials: "same-origin",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  return unwrap<T>(resp);
}

async function unwrap<T>(resp: Response): Promise<T> {
  const body = await resp.json().catch(() => ({}));
  if (!resp.ok) {
    const message =
      (body as { error?: string }).error ?? `request failed (${resp.status})`;
    throw new ApiError(resp.status, message);
  }
  return body as T;
}

export type ArtifactStatus = "missing" | "fresh" | "stale";

export interface Artifact {
  kind: string;
  label: string;
  /** Repo-relative path, e.g. `out/slew/slew-guide.html`. */
  path: string;
  status: ArtifactStatus;
  mtime_ms: number | null;
}

export interface BuildCopy {
  intro: string | null;
  tools: string[];
  cautions: string[];
}

export interface Circuit {
  name: string;
  /// Null for an imported circuit, which has no source to show.
  source: string | null;
  /// The imported fab-package directory, when this is somebody else's board.
  import: string | null;
  panel: string | null;
  panel_hp: number | null;
  panel_finish: string | null;
  panel_format: string | null;
  panel_mtime_ms: number | null;
  notes: string | null;
  kit: string | null;
  has_build_copy: boolean;
  build: BuildCopy | null;
  input_mtime_ms: number | null;
  artifacts: Artifact[];
}

export interface RepoInfo {
  root: string;
  name: string | null;
  brand: string | null;
  logo: string | null;
  circuits: number;
}

export interface SourceDoc {
  name: string;
  path: string;
  language: string;
  content: string;
}

export interface BomLine {
  mpn: string | null;
  value: string;
  footprint: string | null;
  refdes: string[];
  qty: number;
  unit_price: number | null;
  ext_price: number | null;
  image_url: string | null;
  /** The photo this line uses, resolved as the Visual BOM resolves it. */
  photo_src: string | null;
  /** Crop over `photo_src` as `[x, y, w, h]` fractions, if one was chosen. */
  crop: [number, number, number, number] | null;
}

export interface Bom {
  built: boolean;
  priced: boolean;
  total: number | null;
  lines: BomLine[];
}

export interface Order {
  id: number;
  module: string;
  vendor: string | null;
  status: string;
  ordered_at: string | null;
  tracking_ref: string | null;
  notes: string | null;
}

export interface Orders {
  available: boolean;
  orders: Order[];
}

/** A whitelisted metadata edit — never touches topology. Absent field = leave. */
export interface BuildResult {
  ok: boolean;
  output: string;
}

export interface CircuitEditPayload {
  name: string;
  build_intro?: string;
  build_tools?: string[];
  build_cautions?: string[];
  kit?: string;
}
export interface ManifestEditPayload {
  brand?: string;
  circuits?: CircuitEditPayload[];
}

export interface EditResult {
  written: boolean;
  staged: boolean;
  git_error: string | null;
  /** All repo-relative paths staged (uncommitted) — the batch to commit. */
  staged_files: string[];
}

export interface SimPoint {
  t_s: number;
  v: number;
}
export interface SimResult {
  input_net: string;
  probe_net: string;
  stop_s: number;
  output: SimPoint[];
}
export interface CvDrivePayload {
  net: string;
  pwl: [number, number][];
}
export interface SimPayload {
  pwl: [number, number][];
  step_s: number;
  stop_s: number;
  cv?: CvDrivePayload[];
  probe?: string;
}

/** How many footprints of each mounting style sit on one face of a board. */
export interface MountCounts {
  tht: number;
  smd: number;
}
/**
 * What is mounted on each face of the built board. The viewer uses it so its SMD
 * filter can say what it will do: hiding SMD on a face that has none is a no-op,
 * and a control that silently no-ops looks broken.
 */
export interface BoardSides {
  front: MountCounts;
  back: MountCounts;
}

/** One design rule's standing against the built board. */
export interface RuleCheck {
  tier: "physical" | "electrical" | "preference";
  subject: string;
  detail: string;
  margin_mm: number;
  ok: boolean;
}
/**
 * Every rule the built board is held to. Worst first, passes included — the
 * point of the panel is to show what is being checked, not only what failed.
 */
export interface RuleReport {
  rules: RuleCheck[];
  broken: number;
  checked: number;
}

// --- Hand placement (cll) ----------------------------------------------------
// The control surface is placed by a person, the board internals by the placer.
// These types mirror the two halves of that: `Controls` is the built board read
// into panel space (what to draw), `PlacementDoc` is the authored file (what it
// MEANS — "these three jacks are a column on 13.5mm pitch"). Keeping both is the
// point: coordinates alone would let the editor shred the intent on every save.

export interface Point {
  x: number;
  y: number;
}

export type Hole =
  | { shape: "circle"; diameter_mm: number }
  | {
      shape: "rect";
      width_mm: number;
      height_mm: number;
      corner_radius_mm: number;
    };

export interface PlacedPart {
  refdes: string;
  value: string;
  footprint: string;
  /** Mount point in panel space — where the hardware comes through the panel. */
  x: number;
  y: number;
  rotation_deg: number;
  back: boolean;
  /** Panel hardware (movable) vs a board internal (drawn dimmed, not movable). */
  panel: boolean;
  kind?: "pot" | "switch" | "led" | "jack";
  hole?: Hole;
  /** `[w, h]` the hardware needs — a knob's skirt, a jack's nut. */
  envelope_mm?: [number, number];
}

export interface Controls {
  built: boolean;
  width_mm: number;
  height_mm: number;
  hp: number;
  parts: PlacedPart[];
}

export interface PatternColumn {
  refdes: string[];
  x: number;
  from_y: number;
  pitch: number;
}
export interface PatternRow {
  refdes: string[];
  y: number;
  from_x: number;
  pitch: number;
}
export interface PatternGrid {
  refdes: string[];
  x: number;
  y: number;
  cols: number;
  pitch_x: number;
  pitch_y: number;
}
export interface Patterns {
  column?: PatternColumn[];
  row?: PatternRow[];
  grid?: PatternGrid[];
}
/** The placement file as authored — intent, not coordinates. */
export interface PlacementFile {
  hp?: number | null;
  controls?: Record<string, Point>;
  patterns?: Patterns;
}

export interface PlacementDoc {
  path: string;
  exists: boolean;
  file: PlacementFile;
  positions: Record<string, Point>;
}

/** One edit. The server applies a batch in order, so a multi-step tool is one write. */
export type PlacementOp =
  | { op: "set_control"; refdes: string; x: number; y: number }
  | { op: "clear_control"; refdes: string }
  | {
      op: "set_column";
      refdes: string[];
      x: number;
      from_y: number;
      pitch: number;
    }
  | { op: "set_row"; refdes: string[]; y: number; from_x: number; pitch: number }
  | {
      op: "set_grid";
      refdes: string[];
      x: number;
      y: number;
      cols: number;
      pitch_x: number;
      pitch_y: number;
    }
  | { op: "drop_from_pattern"; refdes: string }
  | { op: "break_pattern"; refdes: string[] };

/**
 * One edit request, in exactly one of three forms. Deciding which patterns
 * survive a move is placement reasoning and lives in the core library, not here
 * — so the editor says what it wants and the server works out the operations.
 *
 * - `ops` — already file-shaped ("make these a column on 13.5mm pitch")
 * - `targets` — "these parts end up here": drag, nudge, align, mirror
 * - `restore` — "make the file say this again", which is undo for every tool
 */
export type PlacementRequest =
  | { ops: PlacementOp[] }
  | { targets: Record<string, Point> }
  | { restore: PlacementFile };

/** A part that moved WITHOUT being dragged, because a pattern was reshaped. */
export interface Moved {
  refdes: string;
  from: Point;
  to: Point;
}

export interface PlacementWriteResult {
  written: boolean;
  staged: boolean;
  git_error?: string | null;
  staged_files: string[];
  path: string;
  file: PlacementFile;
  positions: Record<string, Point>;
  side_effects: Moved[];
}

export const api = {
  repo: () => getJson<RepoInfo>("/api/repo"),
  controls: (name: string) =>
    getJson<Controls>(`/api/circuits/${encodeURIComponent(name)}/controls`),
  placement: (name: string) =>
    getJson<PlacementDoc>(`/api/circuits/${encodeURIComponent(name)}/placement`),
  savePlacement: (name: string, req: PlacementRequest) =>
    postJson<PlacementWriteResult>(
      `/api/circuits/${encodeURIComponent(name)}/placement`,
      req,
    ),
  rules: (name: string) =>
    getJson<RuleReport>(`/api/circuits/${encodeURIComponent(name)}/rules`),
  sides: (name: string) =>
    getJson<BoardSides>(`/api/circuits/${encodeURIComponent(name)}/sides`),
  circuits: () => getJson<Circuit[]>("/api/circuits"),
  build: (name: string) =>
    postJson<BuildResult>(
      `/api/circuits/${encodeURIComponent(name)}/build`,
      {},
    ),
  circuit: (name: string) =>
    getJson<Circuit>(`/api/circuits/${encodeURIComponent(name)}`),
  source: (name: string) =>
    getJson<SourceDoc>(`/api/circuits/${encodeURIComponent(name)}/source`),
  bom: (name: string, price = false, photos = false) => {
    const q = new URLSearchParams();
    if (price) q.set("price", "true");
    if (photos) q.set("photos", "true");
    const qs = q.toString();
    return getJson<Bom>(
      `/api/circuits/${encodeURIComponent(name)}/bom${qs ? `?${qs}` : ""}`,
    );
  },
  /** Save a crop over a part photo, or clear it with `null`. */
  saveCrop: (src: string, crop: [number, number, number, number] | null) =>
    postJson<{ ok: boolean; cropped: boolean }>("/api/image/crop", { src, crop }),
  orders: (name: string) =>
    getJson<Orders>(`/api/circuits/${encodeURIComponent(name)}/orders`),
  edit: (payload: ManifestEditPayload) =>
    postJson<EditResult>("/api/edit", payload),
  sim: (name: string, payload: SimPayload) =>
    postJson<SimResult>(
      `/api/circuits/${encodeURIComponent(name)}/sim`,
      payload,
    ),
};

/**
 * URL serving a part photo's bytes. `raw` gives the uncropped original (what the
 * crop editor works on); otherwise the cropped result the build will use.
 *
 * `v` busts the browser cache: the bytes change when a crop is saved but the
 * source URL does not, so without it a save appears to do nothing.
 */
export function photoUrl(src: string, raw = false, v = 0): string {
  const q = new URLSearchParams({ src });
  if (raw) q.set("raw", "true");
  if (v) q.set("v", String(v));
  return `/api/image?${q.toString()}`;
}

/** URL that serves a built artifact file (strips the leading `out/`). */
export function artifactUrl(path: string): string {
  return "/artifacts/" + path.replace(/^out\//, "");
}

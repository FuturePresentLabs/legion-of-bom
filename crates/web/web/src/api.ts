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

export const api = {
  repo: () => getJson<RepoInfo>("/api/repo"),
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
  bom: (name: string, price = false) =>
    getJson<Bom>(
      `/api/circuits/${encodeURIComponent(name)}/bom${price ? "?price=true" : ""}`,
    ),
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

/** URL that serves a built artifact file (strips the leading `out/`). */
export function artifactUrl(path: string): string {
  return "/artifacts/" + path.replace(/^out\//, "");
}

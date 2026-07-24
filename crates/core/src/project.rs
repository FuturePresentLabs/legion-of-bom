//! The repo/project **read model** — the shared view both `lob` (the CLI's
//! `status`/`circuits`) and the localhost dashboard (epic p58) render.
//!
//! DESIGN 2.2 is explicit that nothing may be web-only: anything the dashboard
//! shows, the CLI can show headless. So the "what's in this circuits repo, and
//! which of its artifacts are built / stale" logic lives here in the core, over
//! the [`Manifest`], rather than in either surface. Both heads call
//! [`ProjectView::discover`] and read the same struct.
//!
//! Pure library + filesystem stats — no network, no build side effects. It
//! reports the state of the `out/<name>/` tree the pipeline writes (the artifact
//! filenames mirror what the `guide`/`bom`/`fab` commands emit).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::manifest::{BuildCopy, Manifest, ManifestError, MANIFEST_NAME};
use crate::panel::PanelFile;

/// The output tree the pipeline writes, relative to the repo root.
pub const OUT_DIR: &str = "out";

/// A built (or buildable) artifact for a circuit. Each variant maps to one
/// concrete file under `out/<name>/`, named exactly as the pipeline writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactKind {
    /// `out/<name>/<name>-guide.html` — the step-by-step assembly guide.
    Guide,
    /// `out/<name>/<name>-guide.pdf` — the print-ready guide.
    GuidePdf,
    /// `out/<name>/<name>-vbom.html` — the Visual BOM.
    Vbom,
    /// `out/<name>/<name>_bom.csv` — the BOM CSV.
    BomCsv,
    /// `out/<name>/<name>.kicad_pcb` — the placed + routed board.
    Board,
    /// `out/<name>/fab/<name>-gerbers.zip` — the DRC-gated fab package.
    Fab,
}

impl ArtifactKind {
    /// Every artifact kind, in display order.
    pub const ALL: [ArtifactKind; 6] = [
        ArtifactKind::Guide,
        ArtifactKind::GuidePdf,
        ArtifactKind::Vbom,
        ArtifactKind::BomCsv,
        ArtifactKind::Board,
        ArtifactKind::Fab,
    ];

    /// A short, stable label for CLI columns / UI badges.
    pub fn label(self) -> &'static str {
        match self {
            ArtifactKind::Guide => "guide",
            ArtifactKind::GuidePdf => "guide-pdf",
            ArtifactKind::Vbom => "vbom",
            ArtifactKind::BomCsv => "bom",
            ArtifactKind::Board => "board",
            ArtifactKind::Fab => "fab",
        }
    }

    /// This artifact's path for circuit `name`, relative to the repo root.
    pub fn rel_path(self, name: &str) -> PathBuf {
        let dir = Path::new(OUT_DIR).join(name);
        match self {
            ArtifactKind::Guide => dir.join(format!("{name}-guide.html")),
            ArtifactKind::GuidePdf => dir.join(format!("{name}-guide.pdf")),
            ArtifactKind::Vbom => dir.join(format!("{name}-vbom.html")),
            ArtifactKind::BomCsv => dir.join(format!("{name}_bom.csv")),
            ArtifactKind::Board => dir.join(format!("{name}.kicad_pcb")),
            ArtifactKind::Fab => dir.join("fab").join(format!("{name}-gerbers.zip")),
        }
    }
}

/// Build state of one artifact relative to its circuit's inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactStatus {
    /// Never built.
    Missing,
    /// Built and at least as new as the newest input.
    Fresh,
    /// Built, but an input (source / panel / manifest) changed after it.
    Stale,
}

/// One artifact's presence + freshness, resolved against the `out/` tree.
#[derive(Debug, Clone, Serialize)]
pub struct ArtifactView {
    pub kind: ArtifactKind,
    /// Convenience mirror of [`ArtifactKind::label`] for JSON consumers.
    pub label: &'static str,
    /// Repo-relative path (e.g. `out/slew_limiter/slew_limiter-guide.html`).
    pub path: PathBuf,
    pub status: ArtifactStatus,
    /// Modification time as epoch milliseconds, or `None` if the file is absent.
    pub mtime_ms: Option<u64>,
}

/// Repo-level brand identity (DESIGN 7.9), surfaced for document mastheads.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RepoView {
    pub name: Option<String>,
    pub brand: Option<String>,
    pub logo: Option<String>,
}

/// One circuit as the dashboard/CLI sees it: its manifest metadata plus the
/// freshness of every artifact in its `out/` tree.
#[derive(Debug, Clone, Serialize)]
pub struct CircuitView {
    pub name: String,
    /// SKiDL source, repo-relative, exactly as declared in `lob.toml`.
    pub source: String,
    /// Panel spec, repo-relative, if declared.
    pub panel: Option<String>,
    /// Panel width in HP, read from the declared panel spec (if any).
    pub panel_hp: Option<u16>,
    /// Panel finish token (`black`/`silver`/`#rrggbb`…) as written in the spec;
    /// `None` means the default (black). Drives the finish swatch + render color.
    pub panel_finish: Option<String>,
    /// Panel format (`eurorack`, later `intellijel-1u`…), from the spec.
    pub panel_format: Option<String>,
    /// Panel-spec file mtime (epoch ms) — the render cache-buster for the panel,
    /// independent of the board so a board rebuild doesn't re-fetch the panel.
    pub panel_mtime_ms: Option<u64>,
    /// Design-notes doc, repo-relative, if declared.
    pub notes: Option<String>,
    /// The effective kit type (circuit override else repo default), if any.
    pub kit: Option<String>,
    /// Whether the circuit carries per-circuit build copy (5uj.5).
    pub has_build_copy: bool,
    /// The per-circuit build copy, if any.
    pub build: Option<BuildCopy>,
    /// Newest input (source | panel | manifest) as epoch ms — the freshness
    /// baseline every artifact is compared against.
    pub input_mtime_ms: Option<u64>,
    /// Presence + freshness of every [`ArtifactKind`], in [`ArtifactKind::ALL`]
    /// order.
    pub artifacts: Vec<ArtifactView>,
}

impl CircuitView {
    /// The view of one artifact kind, if present in the inventory.
    pub fn artifact(&self, kind: ArtifactKind) -> Option<&ArtifactView> {
        self.artifacts.iter().find(|a| a.kind == kind)
    }
}

/// The whole circuits repo, ready to render. Built once from a [`Manifest`] +
/// the repo root; a snapshot of the filesystem at build time.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectView {
    /// The repo root (the directory holding `lob.toml`).
    pub root: PathBuf,
    pub repo: RepoView,
    pub circuits: Vec<CircuitView>,
}

impl ProjectView {
    /// Build the read model for the repo rooted at `root` from an already-loaded
    /// `manifest`, stat-ing the `out/` tree for artifact freshness.
    pub fn build(root: &Path, manifest: &Manifest) -> Self {
        let manifest_mtime = mtime(&root.join(MANIFEST_NAME));

        let circuits = manifest
            .circuits
            .iter()
            .map(|c| {
                // Newest input: the source, its panel (if any), and the manifest
                // itself — matching `lob status`. `Option`'s ordering treats a
                // missing file as older than any present one.
                let mut input = mtime(&c.source_path(root)).max(manifest_mtime);
                // Panel spec: fold its mtime into freshness AND read its HP /
                // finish / format so the dashboard can show and tune them.
                let (panel_hp, panel_finish, panel_format, panel_mtime_ms) =
                    match c.panel_path(root).filter(|p| p.is_file()) {
                        Some(panel) => {
                            input = input.max(mtime(&panel));
                            let pf = std::fs::read_to_string(&panel)
                                .ok()
                                .and_then(|t| PanelFile::from_toml(&t).ok());
                            let (hp, finish, format) = match pf {
                                Some(pf) => (pf.hp, pf.finish, Some(pf.format)),
                                None => (None, None, None),
                            };
                            (hp, finish, format, mtime(&panel).and_then(to_ms))
                        }
                        None => (None, None, None, None),
                    };

                let artifacts = ArtifactKind::ALL
                    .iter()
                    .map(|&kind| {
                        let rel = kind.rel_path(&c.name);
                        let m = mtime(&root.join(&rel));
                        let status = match m {
                            None => ArtifactStatus::Missing,
                            Some(mt) => match input {
                                Some(inp) if mt < inp => ArtifactStatus::Stale,
                                _ => ArtifactStatus::Fresh,
                            },
                        };
                        ArtifactView {
                            kind,
                            label: kind.label(),
                            path: rel,
                            status,
                            mtime_ms: m.and_then(to_ms),
                        }
                    })
                    .collect();

                CircuitView {
                    name: c.name.clone(),
                    source: c.source.clone(),
                    panel: c.panel.clone(),
                    panel_hp,
                    panel_finish,
                    panel_format,
                    panel_mtime_ms,
                    notes: c.notes.clone(),
                    kit: c.effective_kit(&manifest.defaults).map(str::to_string),
                    has_build_copy: c.build.is_some(),
                    build: c.build.clone(),
                    input_mtime_ms: input.and_then(to_ms),
                    artifacts,
                }
            })
            .collect();

        ProjectView {
            root: root.to_path_buf(),
            repo: RepoView {
                name: manifest.repo.name.clone(),
                brand: manifest.repo.brand.clone(),
                logo: manifest.repo.logo.clone(),
            },
            circuits,
        }
    }

    /// Discover the nearest circuits repo from `start`, load its manifest, and
    /// build the view.
    pub fn discover(start: &Path) -> Result<Self, ManifestError> {
        let (root, manifest) = Manifest::discover(start)?;
        Ok(Self::build(&root, &manifest))
    }

    /// The view for the circuit declared under `name`, if any.
    pub fn circuit(&self, name: &str) -> Option<&CircuitView> {
        self.circuits.iter().find(|c| c.name == name)
    }
}

/// Modification time of `path`, or `None` if it doesn't exist / isn't statable.
fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// A `SystemTime` as epoch milliseconds (`None` for pre-epoch / overflow).
fn to_ms(t: SystemTime) -> Option<u64> {
    t.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    /// A throwaway repo under the temp dir, cleaned up on drop.
    struct TempRepo(PathBuf);

    impl TempRepo {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "lob-project-test-{}-{tag}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempRepo(dir)
        }
        fn write(&self, rel: &str, body: &str) {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const MANIFEST: &str = r#"
        [repo]
        name = "puget-hardware"
        brand = "Puget Audio"

        [defaults]
        kit = "auto"

        [[circuit]]
        name = "slew_limiter"
        source = "slew_limiter.py"
        panel = "slew_limiter_panel.toml"
        kit = "mixed"
        build.intro = "A slew limiter."

        [[circuit]]
        name = "crossfader"
        source = "crossfader.py"
    "#;

    fn status_of(c: &CircuitView, kind: ArtifactKind) -> ArtifactStatus {
        c.artifact(kind).unwrap().status
    }

    #[test]
    fn build_reports_metadata_and_effective_kit() {
        let repo = TempRepo::new("meta");
        repo.write(MANIFEST_NAME, MANIFEST);
        repo.write("slew_limiter.py", "# skidl");
        repo.write("crossfader.py", "# skidl");
        let manifest = Manifest::load(&repo.0).unwrap();

        let view = ProjectView::build(&repo.0, &manifest);
        assert_eq!(view.repo.brand.as_deref(), Some("Puget Audio"));
        assert_eq!(view.circuits.len(), 2);

        let slew = view.circuit("slew_limiter").unwrap();
        assert_eq!(slew.kit.as_deref(), Some("mixed")); // own override
        assert!(slew.has_build_copy);
        assert_eq!(slew.panel.as_deref(), Some("slew_limiter_panel.toml"));

        let cross = view.circuit("crossfader").unwrap();
        assert_eq!(cross.kit.as_deref(), Some("auto")); // repo default
        assert!(!cross.has_build_copy);
        assert!(cross.panel.is_none());
    }

    #[test]
    fn artifact_freshness_missing_fresh_and_stale() {
        let repo = TempRepo::new("fresh");
        repo.write(MANIFEST_NAME, MANIFEST);
        repo.write("slew_limiter.py", "# skidl");
        repo.write("crossfader.py", "# skidl");

        // Build the guide AFTER the inputs → fresh. Leave vbom/fab unbuilt.
        sleep(Duration::from_millis(15));
        repo.write("out/slew_limiter/slew_limiter-guide.html", "<html>");

        let manifest = Manifest::load(&repo.0).unwrap();
        let view = ProjectView::build(&repo.0, &manifest);
        let slew = view.circuit("slew_limiter").unwrap();
        assert_eq!(status_of(slew, ArtifactKind::Guide), ArtifactStatus::Fresh);
        assert_eq!(status_of(slew, ArtifactKind::Vbom), ArtifactStatus::Missing);
        assert_eq!(status_of(slew, ArtifactKind::Fab), ArtifactStatus::Missing);
        assert!(slew
            .artifact(ArtifactKind::Guide)
            .unwrap()
            .mtime_ms
            .is_some());

        // Now touch the source AFTER the guide → the guide goes stale.
        sleep(Duration::from_millis(15));
        repo.write("slew_limiter.py", "# skidl v2");
        let view = ProjectView::build(&repo.0, &manifest);
        let slew = view.circuit("slew_limiter").unwrap();
        assert_eq!(status_of(slew, ArtifactKind::Guide), ArtifactStatus::Stale);
    }

    #[test]
    fn every_kind_is_inventoried_in_order() {
        let repo = TempRepo::new("kinds");
        repo.write(MANIFEST_NAME, MANIFEST);
        let manifest = Manifest::load(&repo.0).unwrap();
        let view = ProjectView::build(&repo.0, &manifest);
        let slew = view.circuit("slew_limiter").unwrap();
        let kinds: Vec<ArtifactKind> = slew.artifacts.iter().map(|a| a.kind).collect();
        assert_eq!(kinds, ArtifactKind::ALL);
        // Fab lives under the fab/ subdir.
        assert_eq!(
            slew.artifact(ArtifactKind::Fab).unwrap().path,
            Path::new("out/slew_limiter/fab/slew_limiter-gerbers.zip")
        );
    }
}

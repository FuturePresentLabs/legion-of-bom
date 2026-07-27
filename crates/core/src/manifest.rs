//! The circuits-repo project model — `lob.toml`. 8mj.1.
//!
//! A circuits repo is a plain git repo holding one or more circuits (DESIGN 2.4).
//! `lob.toml` at its root is the "content model" (the Hugo-config analogue): it
//! declares the repo's brand + defaults and, per circuit, where the SKiDL source
//! and panel spec live plus the sidecar data a bare `.py` can't carry — the kit
//! type, reference notes, and the per-circuit **build copy** (5uj.5: kit intro,
//! tool list, kit-level cautions). This is the single structure the repo-aware
//! CLI, and later the dashboard/agent, all read — so `lob guide slew_limiter`
//! works by *name* from inside the repo instead of by hand-fed paths.
//!
//! Plain files + git on purpose: hand-editable, diffable, round-trippable.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The manifest file name looked up at (and above) the working directory.
pub const MANIFEST_NAME: &str = "lob.toml";

/// Errors loading a circuits-repo manifest.
/// A circuit declaration that makes no sense — caught after parsing, since serde
/// can only see that fields are absent, not that the combination is meaningless.
#[derive(Debug, thiserror::Error)]
#[error("circuit '{name}': {problem}")]
pub struct CircuitDeclError {
    pub name: String,
    pub problem: &'static str,
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("no {MANIFEST_NAME} found in {0} or any parent directory")]
    NotFound(PathBuf),
    #[error("reading {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parsing {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
}

/// A parsed `lob.toml`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct Manifest {
    #[serde(default)]
    pub repo: RepoMeta,
    #[serde(default)]
    pub defaults: Defaults,
    /// The lob-managed circuits, in declaration order (`[[circuit]]` tables).
    #[serde(default, rename = "circuit")]
    pub circuits: Vec<CircuitEntry>,
}

/// Repo-level metadata for document mastheads / brand identity (DESIGN 7.9).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct RepoMeta {
    #[serde(default)]
    pub name: Option<String>,
    /// Brand line shown on generated docs (e.g. "Puget Audio").
    #[serde(default)]
    pub brand: Option<String>,
    /// Brand logo, relative to the repo root.
    #[serde(default)]
    pub logo: Option<String>,
}

/// Repo-wide defaults applied to every circuit unless the circuit overrides them.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct Defaults {
    /// Assembly kit type default (`tht`/`smd`/`mixed`/`auto`).
    #[serde(default)]
    pub kit: Option<String>,
    /// Whether build guides step through surface-mount parts. Defaults to false:
    /// an SMD board arrives assembled, so the guide covers what the builder
    /// actually fits. Per-circuit `guide_smd` overrides this.
    #[serde(default)]
    pub guide_smd: Option<bool>,
}

/// One lob-managed circuit, declared as a `[[circuit]]` table.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct CircuitEntry {
    /// Stable id — used for `out/<name>/…` and as the `lob <cmd> <name>` handle.
    pub name: String,
    /// SKiDL source, relative to the repo root. Absent for an imported circuit,
    /// which has no definition we can run — only the artefacts of one.
    #[serde(default)]
    pub source: Option<String>,
    /// An **imported** circuit: a directory holding somebody else's finished fab
    /// package (JLC-style BOM + CPL + gerbers), relative to the repo root.
    ///
    /// A fab package carries components and where they sit, but no netlist, so an
    /// imported circuit can be seen, costed and re-ordered, and cannot be
    /// simulated, ERC'd or re-laid-out. Keeping it a distinct field rather than a
    /// flavour of `source` is what keeps that distinction impossible to miss.
    #[serde(default)]
    pub import: Option<String>,
    /// Panel spec (TOML), relative to the repo root.
    #[serde(default)]
    pub panel: Option<String>,
    /// Kit-type override (`tht`/`smd`/`mixed`/`auto`); falls back to `defaults.kit`.
    #[serde(default)]
    pub kit: Option<String>,
    /// Whether this circuit's build guide steps through surface-mount parts.
    /// Overrides `[defaults] guide_smd`.
    #[serde(default)]
    pub guide_smd: Option<bool>,
    /// A human design-notes doc (reference only), relative to the repo root.
    #[serde(default)]
    pub notes: Option<String>,
    /// Board revision printed on the silkscreen, e.g. "v1.2". A board with no
    /// revision on it is one nobody can identify in a support thread.
    #[serde(default)]
    pub rev: Option<String>,
    /// A short design note printed on the silkscreen under the title —
    /// topology, a licence, a URL. Keep it short; it competes for the bottom
    /// edge with the title and the logo.
    #[serde(default)]
    pub silk_note: Option<String>,
    /// Per-circuit build copy shown in the guide (5uj.5).
    #[serde(default)]
    pub build: Option<BuildCopy>,
}

/// Per-circuit build copy — the kit-level layer beneath per-kind (in-lob) and
/// per-part (parts library) assembly copy. Project-specific, lives with the repo.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct BuildCopy {
    /// A short kit introduction shown atop the build guide.
    #[serde(default)]
    pub intro: Option<String>,
    /// Tools the builder needs ("Soldering iron", "Flush cutters", …).
    #[serde(default)]
    pub tools: Vec<String>,
    /// Kit-level cautions surfaced before the steps (ESD, panel order, …).
    #[serde(default)]
    pub cautions: Vec<String>,
}

impl Manifest {
    /// Parse a manifest from TOML text.
    pub fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Circuits that declare neither a source nor an import, or both. A circuit is
    /// either defined here or brought in from somewhere else; it cannot be
    /// neither, and "both" hides which one the pipeline would actually use.
    pub fn declaration_errors(&self) -> Vec<CircuitDeclError> {
        self.circuits
            .iter()
            .filter_map(|c| {
                let problem = match (&c.source, &c.import) {
                    (None, None) => {
                        "needs either `source` (a definition) or `import` (a fab package)"
                    }
                    (Some(_), Some(_)) => "has both `source` and `import` — pick one",
                    _ => return None,
                };
                Some(CircuitDeclError {
                    name: c.name.clone(),
                    problem,
                })
            })
            .collect()
    }

    /// Load the manifest at `repo_root/lob.toml`.
    pub fn load(repo_root: &Path) -> Result<Self, ManifestError> {
        let path = repo_root.join(MANIFEST_NAME);
        let text = std::fs::read_to_string(&path).map_err(|source| ManifestError::Io {
            path: path.clone(),
            source,
        })?;
        Self::from_toml(&text).map_err(|source| ManifestError::Parse { path, source })
    }

    /// Find the nearest circuits repo by walking up from `start` until a
    /// `lob.toml` is found; returns the directory that contains it.
    pub fn find_repo_root(start: &Path) -> Option<PathBuf> {
        start
            .ancestors()
            .find(|dir| dir.join(MANIFEST_NAME).is_file())
            .map(Path::to_path_buf)
    }

    /// Locate + load the nearest manifest from `start`, returning it with the repo
    /// root it was found in.
    pub fn discover(start: &Path) -> Result<(PathBuf, Manifest), ManifestError> {
        let root = Self::find_repo_root(start)
            .ok_or_else(|| ManifestError::NotFound(start.to_path_buf()))?;
        let manifest = Self::load(&root)?;
        Ok((root, manifest))
    }

    /// The circuit declared under `name`, if any.
    pub fn circuit(&self, name: &str) -> Option<&CircuitEntry> {
        self.circuits.iter().find(|c| c.name == name)
    }
}

impl CircuitEntry {
    /// The SKiDL source path, resolved against the repo root.
    pub fn source_path(&self, repo_root: &Path) -> Option<PathBuf> {
        self.source.as_ref().map(|s| repo_root.join(s))
    }

    /// The imported fab-package directory, resolved against the repo root.
    pub fn import_path(&self, repo_root: &Path) -> Option<PathBuf> {
        self.import.as_ref().map(|p| repo_root.join(p))
    }

    /// Whether this circuit is imported rather than defined here.
    pub fn is_imported(&self) -> bool {
        self.source.is_none() && self.import.is_some()
    }

    /// The panel-spec path, resolved against the repo root, if declared.
    pub fn panel_path(&self, repo_root: &Path) -> Option<PathBuf> {
        self.panel.as_ref().map(|p| repo_root.join(p))
    }

    /// The effective kit type: the circuit's own override, else the repo default.
    pub fn effective_kit<'a>(&'a self, defaults: &'a Defaults) -> Option<&'a str> {
        self.kit.as_deref().or(defaults.kit.as_deref())
    }

    /// Whether this circuit's build guide steps through surface-mount parts:
    /// the circuit's own setting, else the repo default, else off.
    pub fn effective_guide_smd(&self, defaults: &Defaults) -> bool {
        self.guide_smd.or(defaults.guide_smd).unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        [repo]
        name = "puget-hardware"
        brand = "Puget Audio"
        logo = "brand/puget-logo.svg"

        [defaults]
        kit = "auto"

        [[circuit]]
        name = "slew_limiter"
        source = "slew_limiter.py"
        panel = "slew_limiter_panel.toml"
        kit = "mixed"
        notes = "slew-limiter-circuit.md"
        build.intro = "A voltage-controlled slew limiter."
        build.tools = ["Soldering iron", "Flush cutters"]
        build.cautions = ["The SMD parts come pre-assembled by JLCPCB."]

        [[circuit]]
        name = "crossfader"
        source = "crossfader.py"
    "#;

    #[test]
    fn parses_repo_defaults_and_circuits() {
        let m = Manifest::from_toml(SAMPLE).expect("parse");
        assert_eq!(m.repo.brand.as_deref(), Some("Puget Audio"));
        assert_eq!(m.defaults.kit.as_deref(), Some("auto"));
        assert_eq!(m.circuits.len(), 2);

        let slew = m.circuit("slew_limiter").expect("slew present");
        assert_eq!(slew.source.as_deref(), Some("slew_limiter.py"));
        assert_eq!(slew.panel.as_deref(), Some("slew_limiter_panel.toml"));
        let build = slew.build.as_ref().expect("build copy");
        assert_eq!(
            build.intro.as_deref(),
            Some("A voltage-controlled slew limiter.")
        );
        assert_eq!(build.tools, vec!["Soldering iron", "Flush cutters"]);
        assert_eq!(build.cautions.len(), 1);
    }

    #[test]
    fn effective_kit_prefers_circuit_then_default() {
        let m = Manifest::from_toml(SAMPLE).unwrap();
        // slew declares its own kit; crossfader inherits the repo default.
        assert_eq!(
            m.circuit("slew_limiter")
                .unwrap()
                .effective_kit(&m.defaults),
            Some("mixed")
        );
        assert_eq!(
            m.circuit("crossfader").unwrap().effective_kit(&m.defaults),
            Some("auto")
        );
    }

    #[test]
    fn paths_resolve_against_repo_root() {
        let m = Manifest::from_toml(SAMPLE).unwrap();
        let root = Path::new("/repo");
        let slew = m.circuit("slew_limiter").unwrap();
        assert_eq!(
            slew.source_path(root),
            Some(PathBuf::from("/repo/slew_limiter.py"))
        );
        assert_eq!(
            slew.panel_path(root),
            Some(PathBuf::from("/repo/slew_limiter_panel.toml"))
        );
        assert_eq!(m.circuit("crossfader").unwrap().panel_path(root), None);
    }

    #[test]
    fn missing_required_field_is_a_parse_error() {
        // `name` is still required by the schema itself.
        assert!(Manifest::from_toml("[[circuit]]\nsource = \"x.py\"\n").is_err());
    }

    /// `source` became optional so a circuit can be *imported* instead, which
    /// means serde alone can no longer tell a valid declaration from a useless
    /// one. The combination is checked separately.
    #[test]
    fn a_circuit_needs_exactly_one_of_source_or_import() {
        let neither = Manifest::from_toml("[[circuit]]\nname = \"x\"\n").unwrap();
        assert_eq!(neither.declaration_errors().len(), 1);

        let both =
            Manifest::from_toml("[[circuit]]\nname = \"x\"\nsource = \"x.py\"\nimport = \"pkg\"\n")
                .unwrap();
        assert_eq!(both.declaration_errors().len(), 1);

        let ok = Manifest::from_toml(
            "[[circuit]]\nname = \"a\"\nsource = \"a.py\"\n\
             [[circuit]]\nname = \"b\"\nimport = \"pkg/b\"\n",
        )
        .unwrap();
        assert!(ok.declaration_errors().is_empty());
    }

    #[test]
    fn find_repo_root_walks_up_to_the_manifest() {
        let dir = std::env::temp_dir().join(format!("lob-manifest-test-{}", std::process::id()));
        let nested = dir.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.join(MANIFEST_NAME), "[repo]\nname=\"t\"\n").unwrap();
        assert_eq!(
            Manifest::find_repo_root(&nested).as_deref(),
            Some(dir.as_path())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

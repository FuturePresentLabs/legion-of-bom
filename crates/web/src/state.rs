//! Shared application state.
//!
//! Deliberately tiny: the dashboard is stateless beyond the repo root it serves.
//! Every request rebuilds the [`ProjectView`] from disk, so edits to `lob.toml`
//! or a rebuilt `out/` tree show up live with no restart and no cache to
//! invalidate. No session/db/auth (DESIGN 2.5) — this is localhost, single-user.

use std::path::{Path, PathBuf};

use legion_of_bom_core::{ManifestError, ProjectView};

/// The state every handler shares.
pub struct AppState {
    /// The circuits-repo root (the directory holding `lob.toml`).
    root: PathBuf,
}

impl AppState {
    /// Wrap an already-resolved repo root.
    pub fn new(root: PathBuf) -> Self {
        AppState { root }
    }

    /// The repo root being served.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A fresh snapshot of the repo — its manifest plus `out/` artifact freshness.
    /// Rebuilt per call so the dashboard always reflects the current filesystem.
    pub fn project(&self) -> Result<ProjectView, ManifestError> {
        ProjectView::discover(&self.root)
    }
}

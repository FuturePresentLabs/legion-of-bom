//! The staging half of the metadata round-trip (p58.5).
//!
//! lob only ever **stages** an edited file (`git add`) — it never commits or
//! pushes. The human batches several dashboard edits into the git index and
//! commits them once, when ready, with their own message. That keeps lob out of
//! the repo's history entirely (no per-save commit spam) and matches the
//! conservative, local, single-user posture (DESIGN 2.5): lob writes files and
//! stages them; authoring history stays a human decision.

use std::path::Path;
use std::process::Command;

/// Errors staging a file.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git not found on PATH")]
    NotFound,
    #[error("not a git repository: {0}")]
    NotARepo(String),
    #[error("git add failed: {0}")]
    AddFailed(String),
    #[error("running git: {0}")]
    Io(std::io::Error),
}

/// `git -C root add -- <paths>`. A no-op for an empty list. Never commits.
pub fn stage(root: &Path, paths: &[&Path]) -> Result<(), GitError> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(root).arg("add").arg("--");
    for p in paths {
        cmd.arg(p);
    }
    let out = cmd.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            GitError::NotFound
        } else {
            GitError::Io(e)
        }
    })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if stderr.contains("not a git repository") {
            return Err(GitError::NotARepo(stderr));
        }
        return Err(GitError::AddFailed(stderr));
    }
    Ok(())
}

/// Repo-relative paths currently staged in `root` (from `git diff --cached
/// --name-only`), so a surface can show "N files staged, uncommitted". Empty on
/// any git error.
pub fn staged_paths(root: &Path) -> Vec<String> {
    let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--cached", "--name-only"])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

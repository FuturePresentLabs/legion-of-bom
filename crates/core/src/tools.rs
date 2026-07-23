//! Discovery of the external command-line tools the pipeline shells out to
//! (ngspice for simulation, KiCad's `kicad-cli` for layout/outputs).
//!
//! Stages use this to locate a tool or fail gracefully when it is absent —
//! never panic (see the convention in CLAUDE.md). `lob doctor` renders the same
//! probes as a preflight. DESIGN.md 5.1, 6.6.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A required-or-optional external CLI, and how to find and version it.
#[derive(Debug, Clone)]
pub struct Tool {
    /// Display name.
    pub name: &'static str,
    /// Command to look for on `PATH`.
    pub command: &'static str,
    /// Absolute fallback locations tried if it is not on `PATH` (e.g. macOS app
    /// bundles). The first existing one wins.
    pub fallbacks: &'static [&'static str],
    /// Arguments that make the tool print its version.
    pub version_args: &'static [&'static str],
    /// Whether the working pipeline loop cannot run without it.
    pub required: bool,
}

/// The result of probing for a [`Tool`].
#[derive(Debug, Clone)]
pub struct ToolStatus {
    pub name: &'static str,
    pub required: bool,
    /// Resolved executable path, if found.
    pub path: Option<PathBuf>,
    /// A version line, if the tool ran and printed one.
    pub version: Option<String>,
}

impl ToolStatus {
    /// True if the executable was located.
    pub fn found(&self) -> bool {
        self.path.is_some()
    }

    /// True if this tool is missing but the loop needs it.
    pub fn missing_required(&self) -> bool {
        self.required && !self.found()
    }
}

impl Tool {
    /// Locate the tool (`PATH`, then fallbacks) and read its version.
    pub fn probe(&self) -> ToolStatus {
        let path = find_on_path(self.command).or_else(|| {
            self.fallbacks
                .iter()
                .map(PathBuf::from)
                .find(|p| p.is_file())
        });
        let version = path.as_deref().and_then(|p| self.read_version(p));
        ToolStatus {
            name: self.name,
            required: self.required,
            path,
            version,
        }
    }

    fn read_version(&self, path: &Path) -> Option<String> {
        let output = Command::new(path).args(self.version_args).output().ok()?;
        let text = if output.stdout.is_empty() {
            String::from_utf8_lossy(&output.stderr)
        } else {
            String::from_utf8_lossy(&output.stdout)
        };
        first_version_line(&text)
    }
}

/// The external tools the Phase 0 loop depends on.
///
/// ngspice is required (simulation is core to the loop); `kicad-cli` is optional
/// at Phase 0 (layout/board output is the stretch task).
pub fn phase0_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "ngspice",
            command: "ngspice",
            fallbacks: &[],
            version_args: &["--version"],
            required: true,
        },
        Tool {
            name: "kicad-cli",
            command: "kicad-cli",
            fallbacks: &["/Applications/KiCad/KiCad.app/Contents/MacOS/kicad-cli"],
            version_args: &["--version"],
            required: false,
        },
    ]
}

/// Resolve the `kicad-cli` executable (`PATH`, then the macOS app-bundle
/// fallback), for the DRC-readback + export steps.
pub fn kicad_cli_path() -> Option<PathBuf> {
    phase0_tools()
        .iter()
        .find(|t| t.name == "kicad-cli")
        .and_then(|t| t.probe().path)
}

/// Pick the most informative version line: the first non-empty line that
/// contains a digit (skips banner lines like ngspice's `******`), falling back
/// to the first non-empty line.
pub(crate) fn first_version_line(text: &str) -> Option<String> {
    let mut fallback = None;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if fallback.is_none() {
            fallback = Some(line.to_string());
        }
        if line.chars().any(|c| c.is_ascii_digit()) {
            return Some(line.to_string());
        }
    }
    fallback
}

/// Search `PATH` for an executable, returning the first hit. A `command` that
/// already contains a path separator is used verbatim.
pub fn find_on_path(command: &str) -> Option<PathBuf> {
    if command.contains(std::path::MAIN_SEPARATOR) {
        let p = PathBuf::from(command);
        return p.is_file().then_some(p);
    }
    let paths = env::var_os("PATH")?;
    env::split_paths(&paths)
        .map(|dir| dir.join(command))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_skips_banner_prefers_digits() {
        let ngspice = "******\n** ngspice-45.2 : Circuit level simulation program\n**";
        assert_eq!(
            first_version_line(ngspice).as_deref(),
            Some("** ngspice-45.2 : Circuit level simulation program")
        );
        assert_eq!(first_version_line("10.0.0\n").as_deref(), Some("10.0.0"));
        assert_eq!(first_version_line("   \n\n").as_deref(), None);
        // No digits anywhere: fall back to the first non-empty line.
        assert_eq!(
            first_version_line("\nhello\nworld").as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn find_on_path_locates_a_ubiquitous_binary() {
        // `sh` exists on every unix PATH used in CI/dev.
        #[cfg(unix)]
        assert!(
            find_on_path("sh").is_some(),
            "sh should be discoverable on PATH"
        );
        // A nonsense command resolves to nothing.
        assert!(find_on_path("definitely-not-a-real-tool-xyz").is_none());
    }
}

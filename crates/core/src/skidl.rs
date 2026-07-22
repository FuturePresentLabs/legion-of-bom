//! The SKiDL runner — the circuit-definition frontend for Phase 0 (DESIGN.md
//! 3.1). It shells out to run a SKiDL Python script, which emits a KiCad netlist
//! and an ERC report. The netlist is what the parser (a [`CircuitSource`] impl)
//! turns into the internal model.
//!
//! [`CircuitSource`]: crate::source::CircuitSource
//!
//! The runner is a *producer* that runs before there is a circuit, so it is not
//! a [`Stage`](crate::stage::Stage); it hands a [`SkidlRun`] to the rest of the
//! pipeline. It runs SKiDL in a dedicated working directory so SKiDL's stray
//! artifacts (`*.erc`, `*.log`, `*_sklib.py`) land there, not in the repo root.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::stage::StageError;

/// The env var SKiDL 2.2.x (kicad9 backend) reads to find KiCad symbol libraries.
pub const SYMBOL_ENV: &str = "KICAD9_SYMBOL_DIR";

/// Default macOS location of KiCad's symbol libraries.
pub const MACOS_KICAD_SYMBOLS: &str =
    "/Applications/KiCad/KiCad.app/Contents/SharedSupport/symbols";

/// The env var for KiCad footprint libraries, and the macOS default.
pub const FOOTPRINT_ENV: &str = "KICAD9_FOOTPRINT_DIR";
pub const MACOS_KICAD_FOOTPRINTS: &str =
    "/Applications/KiCad/KiCad.app/Contents/SharedSupport/footprints";

/// Resolve the KiCad footprint directory: `KICAD9_FOOTPRINT_DIR` if it points at
/// a real directory, else the macOS default if it exists.
pub fn kicad_footprint_dir() -> Option<std::path::PathBuf> {
    if let Some(val) = std::env::var_os(FOOTPRINT_ENV) {
        let p = std::path::PathBuf::from(val);
        if p.is_dir() {
            return Some(p);
        }
    }
    let default = std::path::Path::new(MACOS_KICAD_FOOTPRINTS);
    default.is_dir().then(|| default.to_path_buf())
}

/// A Python interpreter and whether it came from the project venv.
#[derive(Debug, Clone)]
pub struct PythonInfo {
    pub path: PathBuf,
    pub from_venv: bool,
}

/// Locate the project venv's Python interpreter, if present.
pub fn venv_python() -> Option<PathBuf> {
    let candidate = Path::new(".venv").join("bin").join("python");
    candidate.is_file().then_some(candidate)
}

/// The interpreter to use: the project venv's Python if present, else `python3`.
pub fn find_python() -> PythonInfo {
    match venv_python() {
        Some(path) => PythonInfo {
            path,
            from_venv: true,
        },
        None => PythonInfo {
            path: PathBuf::from("python3"),
            from_venv: false,
        },
    }
}

/// Ask an interpreter for its installed SKiDL version, if importable.
pub fn skidl_version(python: &Path) -> Option<String> {
    let output = Command::new(python)
        .args(["-c", "import skidl; print(skidl.__version__)"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!version.is_empty()).then_some(version)
}

/// How the KiCad symbol directory was resolved.
#[derive(Debug, Clone)]
pub enum SymbolDir {
    /// From the `KICAD9_SYMBOL_DIR` env var.
    Env(PathBuf),
    /// Falling back to the macOS KiCad app-bundle default.
    MacosDefault(PathBuf),
}

impl SymbolDir {
    pub fn path(&self) -> &Path {
        match self {
            SymbolDir::Env(p) | SymbolDir::MacosDefault(p) => p,
        }
    }
}

/// Resolve the KiCad symbol directory: the env var if it points at a real
/// directory, else the macOS default if it exists, else `None`.
pub fn kicad_symbol_dir() -> Option<SymbolDir> {
    if let Some(val) = std::env::var_os(SYMBOL_ENV) {
        let p = PathBuf::from(val);
        if p.is_dir() {
            return Some(SymbolDir::Env(p));
        }
    }
    let default = Path::new(MACOS_KICAD_SYMBOLS);
    default
        .is_dir()
        .then(|| SymbolDir::MacosDefault(default.to_path_buf()))
}

/// Runs a SKiDL script to produce a netlist + ERC report.
#[derive(Debug, Clone)]
pub struct SkidlRunner {
    /// Python interpreter to invoke.
    pub python: PathBuf,
    /// KiCad symbol directory passed to SKiDL, if resolved.
    pub symbol_dir: Option<PathBuf>,
    /// Working directory; SKiDL runs here and all artifacts land here.
    pub work_dir: PathBuf,
}

/// The result of a successful SKiDL run.
#[derive(Debug, Clone)]
pub struct SkidlRun {
    /// The script that was run (absolute).
    pub script: PathBuf,
    /// The generated KiCad netlist.
    pub netlist_path: PathBuf,
    /// The ERC report text, if SKiDL wrote one.
    pub erc_report: Option<String>,
}

impl SkidlRunner {
    /// Build a runner by discovering the interpreter and symbol dir; artifacts
    /// go under `work_dir`.
    pub fn discover(work_dir: impl Into<PathBuf>) -> Self {
        SkidlRunner {
            python: find_python().path,
            symbol_dir: kicad_symbol_dir().map(|s| s.path().to_path_buf()),
            work_dir: work_dir.into(),
        }
    }

    /// Run `script`, returning the netlist + ERC. Errors (missing interpreter,
    /// non-zero exit, no netlist produced) are returned, never panicked.
    pub fn run(&self, script: &Path) -> Result<SkidlRun, StageError> {
        let script = script.canonicalize().map_err(|e| {
            StageError::Other(format!(
                "circuit script not found: {}: {e}",
                script.display()
            ))
        })?;
        std::fs::create_dir_all(&self.work_dir)?;
        // Absolute work dir: the child runs with cwd = work_dir, so every other
        // path handed to it must be absolute too.
        let work_dir = self.work_dir.canonicalize()?;

        // Absolutize the interpreter for the same reason: a program path
        // containing a separator is resolved against the child's cwd. Use a
        // *lexical* absolute (not canonicalize) so a venv symlink like
        // `.venv/bin/python` is preserved — resolving it would run the base
        // interpreter and lose the venv's site-packages (i.e. skidl). A bare
        // name like `python3` is left alone for the OS to resolve via PATH.
        let python = if self
            .python
            .to_string_lossy()
            .contains(std::path::MAIN_SEPARATOR)
        {
            std::path::absolute(&self.python)?
        } else {
            self.python.clone()
        };

        let stem = script
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("circuit");
        let netlist_path = work_dir.join(format!("{stem}.net"));

        let mut cmd = Command::new(&python);
        cmd.arg(&script)
            .arg("--output")
            .arg(&netlist_path)
            // Run in work_dir so SKiDL's *.erc/*.log/*_sklib.py land here.
            .current_dir(&work_dir);
        if let Some(dir) = &self.symbol_dir {
            cmd.env(SYMBOL_ENV, dir);
        }

        let output = cmd.output().map_err(|e| {
            StageError::ToolNotFound(format!("python interpreter {}: {e}", self.python.display()))
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(StageError::ToolFailed {
                tool: format!("skidl ({})", script.display()),
                code: output.status.code().unwrap_or(-1),
                stderr: tail(&stderr, 20),
            });
        }
        if !netlist_path.is_file() {
            return Err(StageError::Other(format!(
                "SKiDL exited 0 but produced no netlist at {}",
                netlist_path.display()
            )));
        }

        let erc_report = std::fs::read_to_string(work_dir.join(format!("{stem}.erc"))).ok();
        Ok(SkidlRun {
            script,
            netlist_path,
            erc_report,
        })
    }
}

/// The last `n` lines of `s`, joined — keeps error output bounded.
fn tail(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_interpreter_errors_not_panics() {
        // A real (empty) script file so canonicalize() succeeds and we reach —
        // and must survive — the spawn path with a bogus interpreter.
        let script = std::env::temp_dir().join("lob-test-missing-py-script.py");
        std::fs::write(&script, b"# stand-in script\n").unwrap();
        let runner = SkidlRunner {
            python: PathBuf::from("/nonexistent/python-xyz"),
            symbol_dir: None,
            work_dir: std::env::temp_dir().join("lob-test-missing-py"),
        };
        let err = runner.run(&script).unwrap_err();
        assert!(
            matches!(err, StageError::ToolNotFound(_)),
            "expected ToolNotFound, got {err:?}"
        );
        let _ = std::fs::remove_file(&script);
    }

    #[test]
    fn missing_script_errors() {
        let runner = SkidlRunner::discover(std::env::temp_dir().join("lob-test-missing-script"));
        let err = runner.run(Path::new("/no/such/circuit.py")).unwrap_err();
        assert!(
            matches!(err, StageError::Other(_)),
            "expected Other, got {err:?}"
        );
    }

    #[test]
    fn tail_keeps_last_lines() {
        assert_eq!(tail("a\nb\nc\nd", 2), "c\nd");
        assert_eq!(tail("only", 5), "only");
    }
}

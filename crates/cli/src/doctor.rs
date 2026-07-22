//! `lob doctor` — a preflight that probes the external toolchain the pipeline
//! shells out to (ngspice, kicad-cli, SKiDL/Python, KiCad symbol libraries) and
//! reports what is found, where, and what is missing. See docs/TOOLING.md.

use std::path::{Path, PathBuf};
use std::process::Command;

use legion_of_bom_core::{phase0_tools, ToolStatus};

/// Default macOS location of KiCad's symbol libraries; SKiDL reads these to
/// resolve parts like `Device:R`.
const MACOS_KICAD_SYMBOLS: &str = "/Applications/KiCad/KiCad.app/Contents/SharedSupport/symbols";

/// The env var SKiDL 2.2.x (kicad9 backend) uses to find symbol libraries.
const SYMBOL_ENV: &str = "KICAD9_SYMBOL_DIR";

/// Outcome of one bespoke check (SKiDL, symbol dir).
struct Check {
    ok: bool,
    detail: String,
}

/// Run all probes. Returns an error (non-zero exit) if a required tool is missing.
pub fn run() -> anyhow::Result<()> {
    println!("legion-of-bom toolchain check\n");
    let mut ok = true;

    // External CLIs (ngspice = required, kicad-cli = optional at Phase 0).
    for status in phase0_tools().iter().map(|t| t.probe()) {
        print_tool(&status);
        ok &= !status.missing_required();
    }

    // SKiDL (Python) — required for the circuit-definition stage.
    let skidl = probe_skidl();
    print_row(skidl.ok, true, "skidl (Python)", &skidl.detail);
    ok &= skidl.ok;

    // KiCad symbol libraries — SKiDL needs these to resolve parts.
    let symbols = probe_symbol_dir();
    print_row(symbols.ok, true, "KiCad symbols", &symbols.detail);
    ok &= symbols.ok;

    println!();
    if ok {
        println!("✓ all required tools found");
        Ok(())
    } else {
        anyhow::bail!("one or more required tools are missing — see docs/TOOLING.md");
    }
}

fn print_tool(status: &ToolStatus) {
    let detail = match (&status.path, &status.version) {
        (Some(path), Some(version)) => format!("{version}  [{}]", path.display()),
        (Some(path), None) => format!("found  [{}]", path.display()),
        (None, _) => "not found".to_string(),
    };
    print_row(status.found(), status.required, status.name, &detail);
}

fn print_row(ok: bool, required: bool, name: &str, detail: &str) {
    let mark = if ok {
        "✓"
    } else if required {
        "✗"
    } else {
        "—"
    };
    let tag = if required { "required" } else { "optional" };
    println!("  {mark} {name:<16} ({tag})  {detail}");
}

/// Locate the project venv's Python interpreter, if present.
fn venv_python() -> Option<PathBuf> {
    let candidate = Path::new(".venv").join("bin").join("python");
    candidate.is_file().then_some(candidate)
}

/// Check that SKiDL is importable from the project venv (preferred) or PATH python3.
fn probe_skidl() -> Check {
    let (python, source) = match venv_python() {
        Some(p) => (p, ".venv"),
        None => (PathBuf::from("python3"), "PATH"),
    };
    let output = Command::new(&python)
        .args(["-c", "import skidl; print(skidl.__version__)"])
        .output();
    match output {
        Ok(out) if out.status.success() => {
            let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
            Check {
                ok: true,
                detail: format!("skidl {version}  [{source} python]"),
            }
        }
        Ok(_) => Check {
            ok: false,
            detail: format!(
                "not importable via {source} python — run: python3 -m venv .venv && \
                 .venv/bin/pip install -r requirements.txt"
            ),
        },
        Err(_) => Check {
            ok: false,
            detail: "no python interpreter found (.venv/bin/python or python3)".to_string(),
        },
    }
}

/// Check that KiCad symbol libraries are reachable (env var set, or the macOS
/// default path exists so the user just needs to export it).
fn probe_symbol_dir() -> Check {
    if let Some(dir) = std::env::var_os(SYMBOL_ENV) {
        let dir = PathBuf::from(dir);
        return if dir.is_dir() {
            Check {
                ok: true,
                detail: format!("{SYMBOL_ENV}={}", dir.display()),
            }
        } else {
            Check {
                ok: false,
                detail: format!("{SYMBOL_ENV} set but not a directory: {}", dir.display()),
            }
        };
    }
    if Path::new(MACOS_KICAD_SYMBOLS).is_dir() {
        Check {
            ok: false,
            detail: format!("{SYMBOL_ENV} unset — export {SYMBOL_ENV}={MACOS_KICAD_SYMBOLS}"),
        }
    } else {
        Check {
            ok: false,
            detail: format!("{SYMBOL_ENV} unset and no KiCad symbols at default path"),
        }
    }
}

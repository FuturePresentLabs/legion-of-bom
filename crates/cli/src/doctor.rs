//! `lob doctor` — a preflight that probes the external toolchain the pipeline
//! shells out to (ngspice, kicad-cli, SKiDL/Python, KiCad symbol libraries) and
//! reports what is found, where, and what is missing. See docs/TOOLING.md.

use legion_of_bom_core::skidl::{
    find_python, kicad_symbol_dir, skidl_version, SymbolDir, SYMBOL_ENV,
};
use legion_of_bom_core::{phase0_tools, ToolStatus};

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
    let python = find_python();
    let source = if python.from_venv { ".venv" } else { "PATH" };
    match skidl_version(&python.path) {
        Some(version) => print_row(
            true,
            true,
            "skidl (Python)",
            &format!("skidl {version}  [{source} python]"),
        ),
        None => {
            ok = false;
            print_row(
                false,
                true,
                "skidl (Python)",
                &format!(
                    "not importable via {source} python — run: python3 -m venv .venv && \
                     .venv/bin/pip install -r requirements.txt"
                ),
            );
        }
    }

    // KiCad symbol libraries — SKiDL needs these to resolve parts.
    match kicad_symbol_dir() {
        Some(SymbolDir::Env(p)) => print_row(
            true,
            true,
            "KiCad symbols",
            &format!("{SYMBOL_ENV}={}", p.display()),
        ),
        Some(SymbolDir::MacosDefault(p)) => print_row(
            true,
            true,
            "KiCad symbols",
            &format!(
                "{} (macOS default — export {SYMBOL_ENV} to pin it)",
                p.display()
            ),
        ),
        None => {
            ok = false;
            print_row(
                false,
                true,
                "KiCad symbols",
                &format!("{SYMBOL_ENV} unset and no KiCad symbols found"),
            );
        }
    }

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

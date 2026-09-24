use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

const TASK_SCHEMA: &str = "dev.fpl.lob.run/v1";
const EVENT_SCHEMA: &str = "dev.fpl.design-run-event/v1";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Task {
    schema: String,
    operation: Operation,
    family: String,
    brief: String,
    #[serde(default = "default_output_dir")]
    output_dir: PathBuf,
    #[serde(default = "default_min_confidence")]
    min_confidence: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Operation {
    DesignBoard,
}

#[derive(Debug, Serialize)]
struct Artifact {
    path: String,
    bytes: u64,
    sha256: String,
}

fn default_output_dir() -> PathBuf {
    PathBuf::from(".edgerunner/lob")
}

fn default_min_confidence() -> f64 {
    0.5
}

fn report(kind: &str, message: &str, data: Value, exit_code: Option<i32>) -> Result<()> {
    if env::var_os("EDGERUNNER_REPORT_URL").is_none() {
        return Ok(());
    }
    let mut command = Command::new("edgerunner-report");
    command.args([kind, "--message", message, "--data", &data.to_string()]);
    if let Some(code) = exit_code {
        command.args(["--exit-code", &code.to_string()]);
    }
    let status = command.status().context("launching edgerunner-report")?;
    if !status.success() {
        bail!("edgerunner-report failed with {status}");
    }
    Ok(())
}

fn stage(name: &str, event: &str) -> Result<()> {
    report(
        "progress",
        &format!(
            "{name} {}",
            if event == "stage_started" {
                "started"
            } else {
                "completed"
            }
        ),
        json!({"schema": EVENT_SCHEMA, "event": event, "stage": name}),
        None,
    )
}

fn run_stage(name: &str, args: &[&str]) -> Result<()> {
    stage(name, "stage_started")?;
    let status = Command::new("lob")
        .args(args)
        .status()
        .with_context(|| format!("running lob {name}"))?;
    if !status.success() {
        bail!("lob {name} failed with {status}");
    }
    stage(name, "stage_completed")
}

fn artifacts(root: &Path) -> Result<Vec<Artifact>> {
    WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            let bytes = fs::read(entry.path())?;
            Ok(Artifact {
                path: entry.path().display().to_string(),
                bytes: bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(&bytes)),
            })
        })
        .collect()
}

fn execute() -> Result<()> {
    let encoded = env::var("EDGERUNNER_TASK_JSON").context("EDGERUNNER_TASK_JSON is required")?;
    let task: Task = serde_json::from_str(&encoded).context("parsing LOB task")?;
    if task.schema != TASK_SCHEMA {
        bail!("unsupported task schema {:?}", task.schema);
    }
    if !(0.0..=1.0).contains(&task.min_confidence) {
        bail!("min_confidence must be between 0 and 1");
    }
    if env::var_os("OODA_API_KEY").is_none() {
        if let Some(key) = env::var_os("BIFROST_API_KEY") {
            env::set_var("OODA_API_KEY", key);
        }
    }
    if env::var_os("OODA_BASE_URL").is_none() {
        if let Some(url) = env::var_os("BIFROST_BASE_URL") {
            env::set_var("OODA_BASE_URL", url);
        }
    }
    match task.operation {
        Operation::DesignBoard => {}
    }
    fs::create_dir_all(&task.output_dir)?;
    let base = task.output_dir.join("design");
    let trace = task.output_dir.join("decision-trace.json");
    let circuit = task.output_dir.join("circuit.py");
    let board = task.output_dir.join("board.kicad_pcb");
    let strings = [
        base.to_string_lossy().to_string(),
        trace.to_string_lossy().to_string(),
        circuit.to_string_lossy().to_string(),
        board.to_string_lossy().to_string(),
    ];

    report(
        "started",
        "LOB design run started",
        json!({"schema": TASK_SCHEMA}),
        None,
    )?;
    run_stage(
        "spec",
        &[
            "spec",
            &task.family,
            "--brief",
            &task.brief,
            "--out",
            &strings[0],
            "--trace",
            &strings[1],
        ],
    )?;
    let trace: Value = serde_json::from_slice(&fs::read(&trace)?)?;
    let low_confidence = trace
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|decision| {
            let confidence = decision.get("confidence")?.as_f64()?;
            (confidence < task.min_confidence).then(|| {
                json!({
                    "key": decision.get("key"),
                    "confidence": confidence
                })
            })
        })
        .collect::<Vec<_>>();
    if !low_confidence.is_empty() {
        report(
            "needs-approval",
            "LOB brief needs clarification",
            json!({
                "schema": EVENT_SCHEMA,
                "event": "needs_human",
                "outcome": "needs_human",
                "reason": "low_confidence_decisions",
                "decisions": low_confidence
            }),
            Some(2),
        )?;
        bail!("needs human: one or more decisions were below the confidence threshold");
    }
    run_stage(
        "schematic",
        &[
            "schematic",
            &format!("{}.json", strings[0]),
            "--out",
            &strings[2],
        ],
    )?;
    run_stage("pipeline", &["run", &strings[2]])?;
    run_stage("board", &["board", &strings[2], "--out", &strings[3]])?;
    run_stage("drc", &["drc", &strings[3]])?;

    let manifest = artifacts(&task.output_dir)?;
    let manifest_path = task.output_dir.join("artifacts.json");
    fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;
    report(
        "files-changed",
        "LOB artifacts ready",
        json!({"schema": EVENT_SCHEMA, "event": "artifact_written", "artifacts": manifest}),
        None,
    )?;
    report(
        "completed",
        "LOB design run completed",
        json!({"manifest": manifest_path}),
        Some(0),
    )?;
    Ok(())
}

fn main() -> ExitCode {
    match execute() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("lob-edgerunner-runner: {error:#}");
            let _ = report(
                "failed",
                "LOB design run failed",
                json!({"error": error.to_string()}),
                Some(1),
            );
            ExitCode::FAILURE
        }
    }
}

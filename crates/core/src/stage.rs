//! Pipeline stage traits and the report types that `lob run` composes.
//!
//! A [`Stage`] reads a circuit through [`CircuitSource`] and returns a
//! [`StageOutcome`] (findings + pass/fail) — or a [`StageError`] if it could not
//! run at all (missing external tool, unreadable input). `lob run` chains stages
//! and aggregates their outcomes into a [`PipelineReport`]. See DESIGN.md 2.2.

use crate::source::CircuitSource;

/// Severity of a single finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

/// One observation produced by a stage: an ERC warning, a sim mismatch, etc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    pub message: String,
}

impl Finding {
    pub fn info(message: impl Into<String>) -> Self {
        Finding {
            severity: Severity::Info,
            message: message.into(),
        }
    }
    pub fn warning(message: impl Into<String>) -> Self {
        Finding {
            severity: Severity::Warning,
            message: message.into(),
        }
    }
    pub fn error(message: impl Into<String>) -> Self {
        Finding {
            severity: Severity::Error,
            message: message.into(),
        }
    }
}

/// The result of running one stage that *was able to run*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageOutcome {
    /// Stage name, for the report.
    pub stage: String,
    /// Whether the stage considers the circuit acceptable.
    pub passed: bool,
    /// Findings emitted while running.
    pub findings: Vec<Finding>,
}

impl StageOutcome {
    /// A passing outcome with no findings.
    pub fn passed(stage: impl Into<String>) -> Self {
        StageOutcome {
            stage: stage.into(),
            passed: true,
            findings: Vec::new(),
        }
    }

    /// A failing outcome carrying a single error finding.
    pub fn failed(stage: impl Into<String>, message: impl Into<String>) -> Self {
        StageOutcome {
            stage: stage.into(),
            passed: false,
            findings: vec![Finding::error(message)],
        }
    }

    /// Attach a finding (builder-style).
    pub fn with(mut self, finding: Finding) -> Self {
        if finding.severity == Severity::Error {
            self.passed = false;
        }
        self.findings.push(finding);
        self
    }

    /// True if any finding is an error.
    pub fn has_errors(&self) -> bool {
        self.findings.iter().any(|f| f.severity == Severity::Error)
    }
}

/// A stage that could not run at all (as opposed to running and failing).
#[derive(Debug, thiserror::Error)]
pub enum StageError {
    /// A required external tool (SKiDL, ngspice, KiCad) was not found.
    #[error("external tool not found: {0}")]
    ToolNotFound(String),
    /// An external tool ran but exited non-zero.
    #[error("{tool} failed (exit {code}): {stderr}")]
    ToolFailed {
        tool: String,
        code: i32,
        stderr: String,
    },
    /// I/O error reading inputs or writing artifacts.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// Anything else that prevents the stage from running.
    #[error("{0}")]
    Other(String),
}

/// A composable pipeline stage over a circuit.
pub trait Stage {
    /// Stage name (appears in the report).
    fn name(&self) -> &str;
    /// Run the stage against a circuit.
    fn run(&self, circuit: &dyn CircuitSource) -> Result<StageOutcome, StageError>;
}

/// Aggregated result of running a sequence of stages.
#[derive(Debug, Default, Clone)]
pub struct PipelineReport {
    pub outcomes: Vec<StageOutcome>,
}

impl PipelineReport {
    pub fn new() -> Self {
        PipelineReport::default()
    }

    pub fn push(&mut self, outcome: StageOutcome) {
        self.outcomes.push(outcome);
    }

    /// True only if every stage passed.
    pub fn passed(&self) -> bool {
        self.outcomes.iter().all(|o| o.passed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Circuit;

    /// A trivial stage that always passes — proves the trait composes.
    struct NoopStage;
    impl Stage for NoopStage {
        fn name(&self) -> &str {
            "noop"
        }
        fn run(&self, _circuit: &dyn CircuitSource) -> Result<StageOutcome, StageError> {
            Ok(StageOutcome::passed("noop").with(Finding::info("nothing to do")))
        }
    }

    #[test]
    fn compose_a_stage_into_a_report() {
        let circuit = Circuit::new("demo");
        let stages: Vec<Box<dyn Stage>> = vec![Box::new(NoopStage)];
        let mut report = PipelineReport::new();
        for stage in &stages {
            report.push(stage.run(&circuit).expect("noop cannot fail"));
        }
        assert!(report.passed());
        assert_eq!(report.outcomes.len(), 1);
        assert_eq!(report.outcomes[0].stage, "noop");
    }

    #[test]
    fn error_finding_flips_passed() {
        let outcome = StageOutcome::passed("erc").with(Finding::error("floating net"));
        assert!(!outcome.passed);
        assert!(outcome.has_errors());
    }
}

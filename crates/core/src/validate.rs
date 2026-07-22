//! Validation stage — surface SKiDL/KiCad ERC results as structured findings.
//! First-pass validation per DESIGN.md 4.1.
//!
//! ERC runs inside the SKiDL script; the runner captures its report. Here we
//! turn each `ERC WARNING:` / `ERC ERROR:` line into a [`Finding`] and fail the
//! stage only if ERC reported actual errors (warnings pass — e.g. the RC demo's
//! open-port single-pin-net notices).

use crate::stage::{Finding, StageOutcome};

const STAGE: &str = "validate";

/// Turn a SKiDL ERC report into a stage outcome.
pub fn validate_erc(erc_report: Option<&str>) -> StageOutcome {
    let Some(report) = erc_report else {
        return StageOutcome::passed(STAGE).with(Finding::warning("no ERC report was produced"));
    };

    let mut warnings = 0usize;
    let mut errors = 0usize;
    let mut outcome = StageOutcome::passed(STAGE);
    for line in report.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("ERC WARNING:") {
            warnings += 1;
            outcome = outcome.with(Finding::warning(rest.trim().to_string()));
        } else if let Some(rest) = line.strip_prefix("ERC ERROR:") {
            // `with` flips the outcome to failed on an error finding.
            errors += 1;
            outcome = outcome.with(Finding::error(rest.trim().to_string()));
        }
    }
    outcome.with(Finding::info(format!(
        "ERC: {warnings} warning(s), {errors} error(s)"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warnings_pass_errors_fail() {
        let clean = "ERC WARNING: Only one pin attached to net IN.\n\
                     ERC INFO: 1 warnings found while running ERC.\n\
                     ERC INFO: 0 errors found while running ERC.";
        let outcome = validate_erc(Some(clean));
        assert!(outcome.passed);
        assert!(!outcome.has_errors());

        let bad = "ERC ERROR: Two output pins connected together on net OUT.";
        let outcome = validate_erc(Some(bad));
        assert!(!outcome.passed);
        assert!(outcome.has_errors());
    }

    #[test]
    fn missing_report_warns_but_passes() {
        let outcome = validate_erc(None);
        assert!(outcome.passed);
    }
}

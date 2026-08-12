//! Starting a circuits repo, and keeping generated files out of it.
//!
//! Every stage of the pipeline writes artifacts *beside* its inputs — SKiDL drops
//! `.net`/`.erc`/`.log`/`_sklib.py` next to the script, `lob panel pcb` writes a
//! `.kicad_pcb` and a gerber directory next to the panel spec, ngspice leaves
//! `.dat` files, `lob drc` leaves `-drc.rpt`. That is convenient and it means a
//! repo accumulates outputs at the top level unless something says otherwise.
//!
//! The rule this file encodes: **a circuits repo tracks inputs, not outputs.**
//! An output is anything `lob` can regenerate from the inputs — if it is checked
//! in, every rebuild churns the diff and reviewing a real change means reading
//! past a thousand lines of regenerated gerber.
//!
//! The list lives here rather than in a template file so there is one source of
//! truth: `lob init` writes it, and anything that needs to *ask* whether a path
//! is generated can read the same patterns.

/// One ignore rule and why it exists — the comment is written into the file, so
/// somebody reading `.gitignore` in six months learns what produces the file.
pub struct IgnoreRule {
    pub why: &'static str,
    pub patterns: &'static [&'static str],
}

/// Everything `lob` generates, grouped by what produces it.
pub const GENERATED: &[IgnoreRule] = &[
    IgnoreRule {
        why: "lob build/fab/guide — the whole artifact tree",
        patterns: &["out/"],
    },
    IgnoreRule {
        why: "SKiDL run byproducts, written next to the circuit script",
        patterns: &["*.net", "*.erc", "*.log", "*_sklib.py", "skidl_REPL.py"],
    },
    IgnoreRule {
        why: "lob panel pcb — board + gerbers, written next to the panel spec",
        patterns: &[
            "*-panel-gerbers/",
            "*-panel-gerbers.zip",
            "*_panel.kicad_pcb",
            "*_panel.kicad_pro",
            "*_panel.kicad_prl",
        ],
    },
    IgnoreRule {
        why: "lob drc — violation reports",
        patterns: &["*-drc.rpt"],
    },
    IgnoreRule {
        why: "ngspice — simulation output",
        patterns: &["*.dat", "*.cir.out"],
    },
    IgnoreRule {
        why: "KiCad session/backup files",
        patterns: &["*.kicad_prl", "*-backups/", "fp-info-cache"],
    },
    IgnoreRule {
        why: "Parts library: the Dolt store is local, the CSV export is the record",
        patterns: &[".lob/parts/.dolt/"],
    },
    IgnoreRule {
        why: "Local Python venv for SKiDL",
        patterns: &[".venv/"],
    },
    IgnoreRule {
        why: "OS / editor cruft",
        patterns: &[
            ".DS_Store",
            "**/.DS_Store",
            ".history/",
            "**/.history/",
            "*:Zone.Identifier",
        ],
    },
];

/// Marker for the block this tool owns. Everything between the markers is
/// rewritten on each run; anything outside is the author's and is never touched.
const BEGIN: &str = "# --- lob generated outputs (managed by `lob init`) ---";
const END: &str = "# --- end lob generated outputs ---";

/// The managed block, rendered.
pub fn ignore_block() -> String {
    let mut s = String::from(BEGIN);
    s.push_str("\n# A circuits repo tracks INPUTS. Everything below is regenerated\n");
    s.push_str("# by `lob`, so checking it in only churns the diff.\n");
    for rule in GENERATED {
        s.push_str(&format!("\n# {}\n", rule.why));
        for p in rule.patterns {
            s.push_str(p);
            s.push('\n');
        }
    }
    s.push_str(END);
    s.push('\n');
    s
}

/// Merge the managed block into an existing `.gitignore` body.
///
/// Idempotent, and **non-destructive**: an author's own rules are kept exactly as
/// they are. If a previous managed block is present it is replaced in place, so
/// re-running after a `lob` upgrade picks up new patterns without duplicating.
pub fn merge_gitignore(existing: &str) -> String {
    let block = ignore_block();
    if let (Some(a), Some(b)) = (existing.find(BEGIN), existing.find(END)) {
        if a < b {
            let mut out = String::with_capacity(existing.len() + block.len());
            out.push_str(&existing[..a]);
            out.push_str(&block);
            out.push_str(existing[b + END.len()..].trim_start_matches('\n'));
            return out;
        }
    }
    let mut out = existing.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&block);
    out
}

/// Would `lob` regenerate this repo-relative path? Used to report files that are
/// tracked but should not be.
pub fn is_generated(rel: &str) -> bool {
    let rel = rel.trim_start_matches("./");
    GENERATED.iter().flat_map(|r| r.patterns).any(|p| {
        let p = p.trim_end_matches('/');
        if let Some(suffix) = p.strip_prefix("**/") {
            return rel == suffix || rel.ends_with(&format!("/{suffix}"));
        }
        if let Some(ext) = p.strip_prefix('*') {
            // `*-panel-gerbers/` also matches everything beneath it.
            return rel.ends_with(ext)
                || rel.split('/').next().is_some_and(|f| f.ends_with(ext))
                || rel.contains(&format!("{ext}/"));
        }
        rel == p || rel.starts_with(&format!("{p}/"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merging_is_idempotent_and_keeps_the_authors_rules() {
        let mine = "# my own rule\nsecrets.env\n";
        let once = merge_gitignore(mine);
        let twice = merge_gitignore(&once);
        assert_eq!(once, twice, "re-running must not duplicate the block");
        assert!(once.contains("secrets.env"), "author's rules survive");
        assert!(once.contains("out/"));
        assert_eq!(once.matches(BEGIN).count(), 1);
    }

    /// The reason the block has markers: a `lob` upgrade adds patterns, and the
    /// author must not have to hand-merge them.
    #[test]
    fn an_older_block_is_replaced_in_place_not_appended() {
        let old = format!("keep-me\n\n{BEGIN}\nout/\n{END}\n\ntrailing-rule\n");
        let merged = merge_gitignore(&old);
        assert_eq!(merged.matches(BEGIN).count(), 1);
        assert!(merged.contains("keep-me"));
        assert!(
            merged.contains("trailing-rule"),
            "rules after the block survive"
        );
        assert!(merged.contains("*-panel-gerbers/"), "new patterns arrive");
    }

    #[test]
    fn recognises_generated_paths_and_leaves_inputs_alone() {
        for gen in [
            "out/slew_limiter/slew_limiter.kicad_pcb",
            "slew_limiter_panel-panel-gerbers/slew_limiter_panel-B_Cu.gbl",
            "slew_limiter_panel-panel-gerbers.zip",
            "slew_limiter_panel.kicad_pcb",
            "slew_limiter.net",
            "rc_lowpass-drc.rpt",
            "slew_core.dat",
            ".lob/parts/.dolt/config.json",
        ] {
            assert!(is_generated(gen), "{gen} is generated");
        }
        for input in [
            "lob.toml",
            "slew_limiter.py",
            "slew_limiter_panel.toml",
            "slew-limiter-circuit.md",
            ".lob/parts/house_parts.csv",
            "supersynthesis/2opfm/2OPFM_REV5_JLCBOM.csv",
            "brand/puget-logo.svg",
        ] {
            assert!(
                !is_generated(input),
                "{input} is an INPUT and must be tracked"
            );
        }
    }
}

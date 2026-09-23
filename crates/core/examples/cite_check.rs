//! Check candidate datasheet facts before they enter a catalog.
//!
//! Reads a JSON array of `{part, pdf, key, page, quote, ...}` (as a fact
//! extraction pass produces), and runs every quote through
//! [`legion_of_bom_core::datasheet::check`] against the PDF's own pages — the
//! same check a catalog's citations pass at `verify` time. Prints one line per
//! fact, `ok` or the failure, and exits non-zero if any failed.
//!
//! Usage: `cargo run -p legion-of-bom-core --example cite_check -- <facts.json> <pdf-dir>`

use std::collections::HashMap;
use std::path::PathBuf;

use legion_of_bom_core::datasheet::{check, pages, Citation, Datasheet};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let facts_path = PathBuf::from(
        args.next()
            .ok_or("usage: cite_check <facts.json> <pdf-dir>")?,
    );
    let pdf_dir = PathBuf::from(
        args.next()
            .ok_or("usage: cite_check <facts.json> <pdf-dir>")?,
    );
    let facts: Vec<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(facts_path)?)?;

    let mut cache: HashMap<String, Vec<String>> = HashMap::new();
    let mut failed = 0;
    for f in &facts {
        let s = |k: &str| f[k].as_str().unwrap_or_default().to_string();
        let pdf = s("pdf");
        if !cache.contains_key(&pdf) {
            cache.insert(pdf.clone(), pages(&pdf_dir.join(&pdf))?);
        }
        // Leaked: this is a one-shot CLI, and Citation holds 'static strs.
        let source: &'static Datasheet = Box::leak(Box::new(Datasheet {
            part: Box::leak(s("part").into_boxed_str()),
            url: "",
            sha256: "",
        }));
        let c = Citation {
            source,
            page: f["page"].as_u64().unwrap_or(0) as usize,
            quote: Box::leak(s("quote").into_boxed_str()),
        };
        match check(&c, &cache[&pdf]) {
            Ok(()) => println!("ok    {} {}", s("part"), s("key")),
            Err(e) => {
                failed += 1;
                println!("FAIL  {} {}: {e}", s("part"), s("key"));
            }
        }
    }
    println!("{} of {} facts verified", facts.len() - failed, facts.len());
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

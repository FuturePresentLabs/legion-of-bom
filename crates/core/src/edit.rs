//! Format-preserving **metadata** edits to `lob.toml` (p58.5).
//!
//! The dashboard's editing surface writes back only the *content* fields a
//! non-engineer owns — the repo brand, and per-circuit build copy (intro / tools
//! / cautions) + kit choice. It NEVER touches circuit topology or source/panel
//! wiring: DESIGN 1.3 keeps the circuit itself code, edited by engineers. That
//! boundary is enforced structurally here — [`apply_edit`] only ever addresses
//! `[repo].brand` and a circuit's `build.*` / `kit` keys; there is no code path
//! that can reach `name`, `source`, or `panel`.
//!
//! Edits go through `toml_edit` so the file's comments, key order, and multi-line
//! strings survive a round-trip (a plain serde re-serialize would flatten all of
//! that away). After writing, the result is re-parsed as a [`Manifest`] so a
//! malformed edit fails loudly instead of corrupting the repo.

use std::path::Path;

use serde::Deserialize;
use toml_edit::{value, Array, DocumentMut, Item, Table};

use crate::manifest::{Manifest, MANIFEST_NAME};

/// Errors applying a metadata edit.
#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error("reading {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("parsing {MANIFEST_NAME}: {0}")]
    Parse(#[from] toml_edit::TomlError),
    #[error("no circuit '{0}' in {MANIFEST_NAME}")]
    CircuitNotFound(String),
    #[error("edit produced an invalid {MANIFEST_NAME}: {0}")]
    Invalid(toml::de::Error),
}

/// A whitelisted set of metadata changes. Every field is optional; `None` leaves
/// the current value untouched, `Some(_)` sets it (an empty string / empty list
/// removes the key). Only content fields appear here — never topology.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ManifestEdit {
    /// Repo brand line (`[repo].brand`).
    #[serde(default)]
    pub brand: Option<String>,
    /// Per-circuit edits, matched by `name`.
    #[serde(default)]
    pub circuits: Vec<CircuitEdit>,
}

/// Metadata changes for one circuit, matched by its (immutable) `name`.
#[derive(Debug, Clone, Deserialize)]
pub struct CircuitEdit {
    /// Which circuit to edit — used only to locate the `[[circuit]]` entry; the
    /// name itself is never rewritten.
    pub name: String,
    /// Kit-level intro copy (`build.intro`).
    #[serde(default)]
    pub build_intro: Option<String>,
    /// Tool list (`build.tools`).
    #[serde(default)]
    pub build_tools: Option<Vec<String>>,
    /// Kit-level cautions (`build.cautions`).
    #[serde(default)]
    pub build_cautions: Option<Vec<String>>,
    /// Kit-type override (`kit`).
    #[serde(default)]
    pub kit: Option<String>,
}

impl ManifestEdit {
    /// Whether this edit would change anything.
    pub fn is_empty(&self) -> bool {
        self.brand.is_none()
            && self.circuits.iter().all(|c| {
                c.build_intro.is_none()
                    && c.build_tools.is_none()
                    && c.build_cautions.is_none()
                    && c.kit.is_none()
            })
    }
}

/// Apply `edit` to the `lob.toml` at `root`, preserving formatting, and return
/// the new file text. Does not write to disk — see [`edit_manifest`].
pub fn apply_edit_str(toml: &str, edit: &ManifestEdit) -> Result<String, EditError> {
    let mut doc: DocumentMut = toml.parse()?;
    apply_to_doc(&mut doc, edit)?;
    let out = doc.to_string();
    // Fail loudly on a corrupt result rather than persisting it.
    Manifest::from_toml(&out).map_err(EditError::Invalid)?;
    Ok(out)
}

/// Apply `edit` to `root/lob.toml` in place, preserving formatting.
pub fn edit_manifest(root: &Path, edit: &ManifestEdit) -> Result<(), EditError> {
    let path = root.join(MANIFEST_NAME);
    let text = std::fs::read_to_string(&path).map_err(|source| EditError::Io {
        path: path.clone(),
        source,
    })?;
    let out = apply_edit_str(&text, edit)?;
    std::fs::write(&path, out).map_err(|source| EditError::Io { path, source })
}

fn apply_to_doc(doc: &mut DocumentMut, edit: &ManifestEdit) -> Result<(), EditError> {
    if let Some(brand) = &edit.brand {
        let repo = doc
            .entry("repo")
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_mut()
            .expect("repo is a table");
        set_or_remove_str(repo, "brand", brand);
    }

    for ce in &edit.circuits {
        let table = find_circuit_mut(doc, &ce.name)
            .ok_or_else(|| EditError::CircuitNotFound(ce.name.clone()))?;

        if let Some(kit) = &ce.kit {
            set_or_remove_str(table, "kit", kit);
        }

        // build.* lives in a nested `build` table on the circuit.
        if ce.build_intro.is_some() || ce.build_tools.is_some() || ce.build_cautions.is_some() {
            let build = table
                .entry("build")
                .or_insert_with(|| Item::Table(Table::new()))
                .as_table_mut()
                .expect("build is a table");
            if let Some(intro) = &ce.build_intro {
                set_or_remove_str(build, "intro", intro);
            }
            if let Some(tools) = &ce.build_tools {
                set_or_remove_arr(build, "tools", tools);
            }
            if let Some(cautions) = &ce.build_cautions {
                set_or_remove_arr(build, "cautions", cautions);
            }
        }
    }
    Ok(())
}

/// The `[[circuit]]` table whose `name` matches, if any.
fn find_circuit_mut<'a>(doc: &'a mut DocumentMut, name: &str) -> Option<&'a mut Table> {
    doc.get_mut("circuit")?
        .as_array_of_tables_mut()?
        .iter_mut()
        .find(|t| t.get("name").and_then(Item::as_str) == Some(name))
}

/// Set `key` to `v`, or remove it when `v` is blank.
fn set_or_remove_str(table: &mut Table, key: &str, v: &str) {
    if v.trim().is_empty() {
        table.remove(key);
    } else {
        table[key] = value(v);
    }
}

/// Set `key` to the list `items`, or remove it when the list is empty.
fn set_or_remove_arr(table: &mut Table, key: &str, items: &[String]) {
    if items.is_empty() {
        table.remove(key);
    } else {
        let arr: Array = items.iter().collect();
        table[key] = value(arr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"# Puget circuits repo.
[repo]
name = "puget-hardware"
brand = "Puget Audio"   # brand line

[[circuit]]
name = "slew_limiter"
source = "slew_limiter.py"   # topology — must never be touched
kit = "mixed"
build.intro = """
A multi-line
intro."""
build.tools = ["Soldering iron"]
"#;

    #[test]
    fn edits_build_copy_and_preserves_comments_and_topology() {
        let edit = ManifestEdit {
            brand: None,
            circuits: vec![CircuitEdit {
                name: "slew_limiter".into(),
                build_intro: Some("A shorter intro.".into()),
                build_tools: Some(vec!["Soldering iron".into(), "Flush cutters".into()]),
                build_cautions: Some(vec!["ESD-sensitive.".into()]),
                kit: None,
            }],
        };
        let out = apply_edit_str(SAMPLE, &edit).expect("apply");

        // Comments + the untouched topology survive.
        assert!(out.contains("# Puget circuits repo."));
        assert!(out.contains("source = \"slew_limiter.py\""));
        assert!(out.contains("# topology — must never be touched"));
        // New values landed and re-parse.
        let m = Manifest::from_toml(&out).unwrap();
        let c = m.circuit("slew_limiter").unwrap();
        let b = c.build.as_ref().unwrap();
        assert_eq!(b.intro.as_deref(), Some("A shorter intro."));
        assert_eq!(b.tools, vec!["Soldering iron", "Flush cutters"]);
        assert_eq!(b.cautions, vec!["ESD-sensitive."]);
        // Untouched fields stay.
        assert_eq!(c.kit.as_deref(), Some("mixed"));
    }

    #[test]
    fn empty_value_removes_the_key() {
        let edit = ManifestEdit {
            brand: Some(String::new()), // clear brand
            circuits: vec![CircuitEdit {
                name: "slew_limiter".into(),
                build_intro: None,
                build_tools: Some(vec![]), // clear tools
                build_cautions: None,
                kit: None,
            }],
        };
        let out = apply_edit_str(SAMPLE, &edit).unwrap();
        let m = Manifest::from_toml(&out).unwrap();
        assert!(m.repo.brand.is_none());
        assert!(m
            .circuit("slew_limiter")
            .unwrap()
            .build
            .as_ref()
            .unwrap()
            .tools
            .is_empty());
    }

    #[test]
    fn brand_update_keeps_repo_name() {
        let edit = ManifestEdit {
            brand: Some("Puget Modular".into()),
            circuits: vec![],
        };
        let out = apply_edit_str(SAMPLE, &edit).unwrap();
        let m = Manifest::from_toml(&out).unwrap();
        assert_eq!(m.repo.brand.as_deref(), Some("Puget Modular"));
        assert_eq!(m.repo.name.as_deref(), Some("puget-hardware"));
    }

    #[test]
    fn unknown_circuit_is_an_error() {
        let edit = ManifestEdit {
            brand: None,
            circuits: vec![CircuitEdit {
                name: "nope".into(),
                build_intro: Some("x".into()),
                build_tools: None,
                build_cautions: None,
                kit: None,
            }],
        };
        assert!(matches!(
            apply_edit_str(SAMPLE, &edit),
            Err(EditError::CircuitNotFound(_))
        ));
    }
}

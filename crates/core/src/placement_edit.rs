//! Format-preserving edits to a **placement file** (cll.1).
//!
//! Sibling to [`crate::panel_edit`], and the write half of [`crate::placement`].
//! The placement editor's alignment and spacing tools are not a separate model —
//! they *are* this file format, exposed as operations, so every one of them lands
//! here as a [`PlacementOp`].
//!
//! Two properties make that safe to do from a UI:
//!
//! * **Format-preserving.** Edits go through `toml_edit`, so comments, key order
//!   and untouched tables survive a round-trip. A person who hand-authored a
//!   strip and annotated why should still recognise the file after a drag.
//! * **Re-parsed to validate.** The result is parsed back as a
//!   [`PlacementFile`] *and* expanded through [`PlacementFile::positions`] before
//!   it is returned, so a malformed or self-contradicting edit fails loudly
//!   instead of corrupting the file the board is built from.
//!
//! ## Intent, not coordinates
//!
//! Every op is chosen so the file keeps saying *why* parts sit where they do.
//! "Set the pitch on these three jacks" writes one `[[patterns.column]]` with a
//! `pitch`, not three coordinates that happen to be 13.5mm apart. That is the
//! whole reason this exists rather than a KiCad drag.
//!
//! ## The escape hatch is preserved exactly
//!
//! [`crate::placement`] specifies an asymmetry: two *patterns* claiming one
//! refdes is an error, but an explicit `[controls]` entry overriding a pattern is
//! legal and is the documented way to nudge one part out of a strip without
//! unpicking the strip. Nothing here changes that — [`PlacementOp::SetControl`]
//! is exactly that hatch, and [`PlacementOp::SetColumn`] and friends deliberately
//! leave existing overrides alone rather than silently clearing them. Composing
//! [`PlacementOp::ClearControl`] first is how a caller asks for the other
//! behaviour, and since ops apply in order that is a one-batch operation.
//!
//! ## Coordinates are panel space
//!
//! Millimetres from the panel's bottom-left, which is how the file reads and how
//! a person thinks about a front panel. The board-frame flip lives in
//! [`PlacementFile::anchors`] and is not duplicated here.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml_edit::{value, Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value};

use crate::placement::{PlacementFile, Point};

/// Millimetre precision written to the file. A micron is three orders of
/// magnitude below anything a fab house resolves, so rounding here costs nothing
/// and keeps a dragged coordinate from landing as `20.319999999999993`.
const MM_DECIMALS: i32 = 3;

/// One edit to a placement file. Ops apply in order, so a caller can express
/// "stop overriding these, then make them a column" as a single batch.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PlacementOp {
    /// Pin one part at an explicit panel-space point (`[controls]`). Overrides
    /// whatever pattern would otherwise place it — this is the escape hatch, and
    /// it is also what dragging a single part out of a strip should write.
    SetControl { refdes: String, x: f64, y: f64 },
    /// Delete a part's explicit override so it rejoins its pattern. A no-op when
    /// there was no override, so replaying a batch is safe.
    ClearControl { refdes: String },
    /// Create or update a `[[patterns.column]]` — parts stacked up from `from_y`
    /// at `pitch`, all at `x`.
    SetColumn {
        refdes: Vec<String>,
        x: f64,
        from_y: f64,
        pitch: f64,
    },
    /// Create or update a `[[patterns.row]]` — parts spread right from `from_x`
    /// at `pitch`, all at `y`.
    SetRow {
        refdes: Vec<String>,
        y: f64,
        from_x: f64,
        pitch: f64,
    },
    /// Create or update a `[[patterns.grid]]` — `cols` wide, filled across then
    /// downward from the top-left cell `(x, y)`.
    SetGrid {
        refdes: Vec<String>,
        x: f64,
        y: f64,
        cols: usize,
        pitch_x: f64,
        pitch_y: f64,
    },
    /// Remove a part from whatever pattern names it; a pattern left with no
    /// members is deleted. The parts *after* it in the strip close the gap, which
    /// is what a strip losing a member means — see [`PlacementEditResult`], whose
    /// `side_effects` reports exactly which parts that moved. To take one part
    /// out *without* disturbing its neighbours, use
    /// [`SetControl`](PlacementOp::SetControl) instead.
    DropFromPattern { refdes: String },
    /// Expand every pattern that names any of `refdes` into explicit
    /// `[controls]` entries at the positions those parts currently occupy, and
    /// delete the pattern. For when the strip stops being a strip.
    ///
    /// Nothing moves: the coordinates written are the ones the pattern was
    /// already producing. This is the one op that deliberately *increases* the
    /// file's entropy, so it is a thing the user asks for by name rather than
    /// something any other op does as a side effect.
    BreakPattern { refdes: Vec<String> },
}

impl PlacementOp {
    /// Every reference designator this op names — what the write guard checks.
    fn refdes(&self) -> Vec<&str> {
        match self {
            PlacementOp::SetControl { refdes, .. }
            | PlacementOp::ClearControl { refdes }
            | PlacementOp::DropFromPattern { refdes } => vec![refdes.as_str()],
            PlacementOp::SetColumn { refdes, .. }
            | PlacementOp::SetRow { refdes, .. }
            | PlacementOp::SetGrid { refdes, .. }
            | PlacementOp::BreakPattern { refdes } => refdes.iter().map(String::as_str).collect(),
        }
    }
}

/// A part that moved without being named in the edit, because a pattern it
/// belongs to was reshaped. Surfaced so the UI can say so out loud instead of
/// letting a strip silently reflow.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Moved {
    pub refdes: String,
    pub from: Point,
    pub to: Point,
}

/// The outcome of a successful edit: the new text (already validated), the
/// positions it expands to, and any collateral movement.
#[derive(Debug, Clone, Serialize)]
pub struct PlacementEditResult {
    /// The new file text. [`edit_placement`] has already written this to disk;
    /// [`apply_placement_edit_str`] leaves writing to the caller.
    pub toml: String,
    /// The new file as *authored* — patterns and overrides, not coordinates.
    /// Returned alongside the expanded positions because a caller that keeps this
    /// can compute an exact inverse of whatever it just did, which is how the
    /// editor gets undo without reimplementing pattern expansion.
    pub file: PlacementFile,
    /// Panel-space positions after the edit, refdes → point.
    pub positions: HashMap<String, Point>,
    /// Parts that moved but were not named in the edit.
    pub side_effects: Vec<Moved>,
}

/// Errors applying a placement edit.
#[derive(Debug, thiserror::Error)]
pub enum PlacementEditError {
    #[error("reading {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parsing placement file: {0}")]
    Parse(#[from] toml_edit::TomlError),
    #[error("the placement file on disk is already invalid ({0}) — fix it before editing")]
    Stale(String),
    #[error("edit produced an invalid placement file: {0}")]
    Invalid(String),
    #[error("edit produced a contradictory placement: {0}")]
    Conflict(String),
    #[error("no such part in this circuit: {}", .0.join(", "))]
    UnknownRefdes(Vec<String>),
    #[error("a pattern needs at least one refdes")]
    EmptyPattern,
    #[error("{0} is named twice in the same pattern")]
    RepeatedRefdes(String),
    #[error("a grid needs cols >= 1")]
    EmptyGrid,
    #[error("{0} is not a usable millimetre value")]
    NotFinite(&'static str),
    #[error("a pattern with more than one part needs a non-zero pitch")]
    ZeroPitch,
    #[error("[{0}] is not a table in this placement file")]
    NotATable(&'static str),
}

// ---------------------------------------------------------------------------
// Intent preservation — the alignment and spacing tools' shared core
// ---------------------------------------------------------------------------

/// Position tolerance when asking "can this pattern still express these points".
/// Well above float noise from a UI's arithmetic, well below the 1µm the writer
/// rounds to.
const SHAPE_EPS: f64 = 5e-4;

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() <= SHAPE_EPS
}

/// Turn *"these parts should end up here"* into the smallest set of operations
/// that says so — keeping every pattern that can still describe the result.
///
/// This is the one function every placement tool goes through: drag, keyboard
/// nudge, align-left, centre-on-panel, mirror. It is what makes those tools edit
/// **intent** rather than coordinates, and it lives here rather than in the
/// dashboard because "is this still a column?" is a fact about the file format,
/// not about a UI (DESIGN 2.2).
///
/// A pattern only takes part when the move covers **all** of it. Moving *part* of
/// a strip is the escape hatch — those members get overrides and the strip stays
/// put — so the four cases are:
///
/// 1. Covered, and the shape still fits → the pattern is updated. Aligning a jack
///    strip's x leaves the file saying "a column at x=6.0", not five coordinates
///    that happen to share an x. Any override on a member is cleared, because
///    parts that now sit exactly on the strip *are* on the strip.
/// 2. Covered, shape no longer fits, no member overridden — aligning a column
///    onto a single y, say → the pattern is broken deliberately. Breaking pins
///    every member where it already is, so the `SetControl`s that follow are the
///    only thing that moves anything, and no dormant table is left behind.
/// 3. Covered, shape no longer fits, but a member was already overridden → the
///    pattern is left alone and everything gets an override. The arrangement was
///    already ad-hoc, and destroying a strip's definition on the way past would
///    throw away intent that `ClearControl` could still bring back.
/// 4. Not covered → an explicit `[controls]` override. For a strip member that is
///    the documented escape hatch: it leaves the strip without unpicking it.
pub fn ops_for_targets(
    file: &PlacementFile,
    targets: &BTreeMap<String, Point>,
) -> Vec<PlacementOp> {
    let mut ops = Vec::new();
    let mut done: HashSet<String> = HashSet::new();
    let overridden: HashSet<&str> = file.controls.keys().map(String::as_str).collect();

    let mut resolve = |members: &[String], derived: Option<PlacementOp>| {
        if !members.iter().all(|r| targets.contains_key(r)) {
            return;
        }
        let held: Vec<&String> = members
            .iter()
            .filter(|r| overridden.contains(r.as_str()))
            .collect();
        match derived {
            Some(op) => {
                // An override beats the pattern, so it has to go before the
                // pattern can place these parts again.
                for r in held {
                    ops.push(PlacementOp::ClearControl { refdes: r.clone() });
                }
                ops.push(op);
                done.extend(members.iter().cloned());
            }
            // Case 3: keep a definition that an override has already suspended.
            None if !held.is_empty() => {}
            None => ops.push(PlacementOp::BreakPattern {
                refdes: members.to_vec(),
            }),
        }
    };

    for c in &file.patterns.column {
        resolve(&c.refdes, derive_column(&c.refdes, targets));
    }
    for r in &file.patterns.row {
        resolve(&r.refdes, derive_row(&r.refdes, targets));
    }
    for g in &file.patterns.grid {
        resolve(&g.refdes, derive_grid(g, targets));
    }
    // BTreeMap, so the tail is ordered and two identical requests produce two
    // identical files.
    for (refdes, p) in targets {
        if !done.contains(refdes) {
            ops.push(PlacementOp::SetControl {
                refdes: refdes.clone(),
                x: p.x,
                y: p.y,
            });
        }
    }
    ops
}

/// The operations that make `file` say what `want` says.
///
/// Used for undo: the dashboard keeps the authored file from before an edit and
/// asks for it back, so undo works for every tool — including ones not written
/// yet — without the client tracking what each one did. The result still goes
/// through the ordinary writer, so comments and untouched tables survive.
pub fn ops_to_reach(file: &PlacementFile, want: &PlacementFile) -> Vec<PlacementOp> {
    let members = |f: &PlacementFile| -> BTreeSet<String> {
        f.patterns
            .column
            .iter()
            .flat_map(|c| c.refdes.iter())
            .chain(f.patterns.row.iter().flat_map(|r| r.refdes.iter()))
            .chain(f.patterns.grid.iter().flat_map(|g| g.refdes.iter()))
            .cloned()
            .collect()
    };
    let wanted = members(want);
    let mut ops = Vec::new();

    // Parts the current file has in a pattern that the target does not.
    for refdes in members(file).difference(&wanted) {
        ops.push(PlacementOp::DropFromPattern {
            refdes: refdes.clone(),
        });
    }
    // Re-state every wanted pattern. Re-stating an unchanged one is a no-op
    // write and cheap at the sizes a control surface runs to.
    for c in &want.patterns.column {
        ops.push(PlacementOp::SetColumn {
            refdes: c.refdes.clone(),
            x: c.x,
            from_y: c.from_y,
            pitch: c.pitch,
        });
    }
    for r in &want.patterns.row {
        ops.push(PlacementOp::SetRow {
            refdes: r.refdes.clone(),
            y: r.y,
            from_x: r.from_x,
            pitch: r.pitch,
        });
    }
    for g in &want.patterns.grid {
        ops.push(PlacementOp::SetGrid {
            refdes: g.refdes.clone(),
            x: g.x,
            y: g.y,
            cols: g.cols,
            pitch_x: g.pitch_x,
            pitch_y: g.pitch_y,
        });
    }
    // Overrides: drop the ones the target does not have, then restore its own.
    let keep: BTreeSet<&String> = want.controls.keys().collect();
    for refdes in file
        .controls
        .keys()
        .collect::<BTreeSet<_>>()
        .difference(&keep)
    {
        ops.push(PlacementOp::ClearControl {
            refdes: (*refdes).clone(),
        });
    }
    for (refdes, p) in want.controls.iter().collect::<BTreeMap<_, _>>() {
        ops.push(PlacementOp::SetControl {
            refdes: refdes.clone(),
            x: p.x,
            y: p.y,
        });
    }
    ops
}

/// Can a column still describe exactly these points? `None` = it has stopped
/// being a column, and the caller breaks it rather than writing a lie.
fn derive_column(members: &[String], t: &BTreeMap<String, Point>) -> Option<PlacementOp> {
    let at = |r: &String| t.get(r).copied();
    let n = members.len();
    let first = at(&members[0])?;
    let pitch = if n > 1 {
        at(&members[1])?.y - first.y
    } else {
        0.0
    };
    if n > 1 && pitch == 0.0 {
        return None;
    }
    for (i, m) in members.iter().enumerate() {
        let p = at(m)?;
        if !near(p.x, first.x) || !near(p.y, first.y + pitch * i as f64) {
            return None;
        }
    }
    // A strip read top-to-bottom is the same strip; write it upward rather than
    // leaving a negative pitch in a file a person has to read.
    let (members, from, pitch) = if pitch < 0.0 {
        let mut m = members.to_vec();
        m.reverse();
        let from = at(&m[0])?;
        (m, from, -pitch)
    } else {
        (members.to_vec(), first, pitch)
    };
    Some(PlacementOp::SetColumn {
        refdes: members,
        x: from.x,
        from_y: from.y,
        pitch,
    })
}

fn derive_row(members: &[String], t: &BTreeMap<String, Point>) -> Option<PlacementOp> {
    let at = |r: &String| t.get(r).copied();
    let n = members.len();
    let first = at(&members[0])?;
    let pitch = if n > 1 {
        at(&members[1])?.x - first.x
    } else {
        0.0
    };
    if n > 1 && pitch == 0.0 {
        return None;
    }
    for (i, m) in members.iter().enumerate() {
        let p = at(m)?;
        if !near(p.y, first.y) || !near(p.x, first.x + pitch * i as f64) {
            return None;
        }
    }
    let (members, from, pitch) = if pitch < 0.0 {
        let mut m = members.to_vec();
        m.reverse();
        let from = at(&m[0])?;
        (m, from, -pitch)
    } else {
        (members.to_vec(), first, pitch)
    };
    Some(PlacementOp::SetRow {
        refdes: members,
        y: from.y,
        from_x: from.x,
        pitch,
    })
}

fn derive_grid(g: &crate::placement::Grid, t: &BTreeMap<String, Point>) -> Option<PlacementOp> {
    let at = |r: &String| t.get(r).copied();
    let (n, cols) = (g.refdes.len(), g.cols);
    if cols == 0 {
        return None;
    }
    let rows = n.div_ceil(cols);
    let first = at(&g.refdes[0])?;
    let pitch_x = if cols > 1 {
        at(&g.refdes[1])?.x - first.x
    } else {
        0.0
    };
    let pitch_y = if rows > 1 {
        first.y - at(&g.refdes[cols])?.y
    } else {
        0.0
    };
    if (cols > 1 && pitch_x == 0.0) || (rows > 1 && pitch_y == 0.0) {
        return None;
    }
    for (i, m) in g.refdes.iter().enumerate() {
        let p = at(m)?;
        let (col, row) = (i % cols, i / cols);
        // Across, then *down* the panel — and down is a lower y.
        if !near(p.x, first.x + pitch_x * col as f64) {
            return None;
        }
        if !near(p.y, first.y - pitch_y * row as f64) {
            return None;
        }
    }
    Some(PlacementOp::SetGrid {
        refdes: g.refdes.clone(),
        x: first.x,
        y: first.y,
        cols,
        pitch_x,
        pitch_y,
    })
}

/// Apply `ops` to a placement file's TOML text, preserving formatting, and return
/// the new text plus what it expands to. Does not write to disk — see
/// [`edit_placement`].
///
/// `known` is the write guard: the reference designators that actually exist in
/// the circuit. An edit naming anything else is refused outright, because a
/// placement for a part that does not exist is a hole in a panel with nothing
/// behind it. It is required rather than optional so there is no path that
/// skips the check.
///
/// An empty `toml` is a valid starting point — that is how the first drag on a
/// circuit with no placement file works.
pub fn apply_placement_edit_str(
    toml: &str,
    ops: &[PlacementOp],
    known: &HashSet<String>,
) -> Result<PlacementEditResult, PlacementEditError> {
    // Refuse the whole batch before touching the document: a half-applied edit
    // is worse than a rejected one.
    let unknown: Vec<String> = {
        let mut seen: Vec<String> = ops
            .iter()
            .flat_map(PlacementOp::refdes)
            .filter(|r| !known.contains(*r))
            .map(str::to_string)
            .collect();
        seen.sort();
        seen.dedup();
        seen
    };
    if !unknown.is_empty() {
        return Err(PlacementEditError::UnknownRefdes(unknown));
    }
    for op in ops {
        validate(op)?;
    }

    // Parse first for two reasons: an already-broken file must not be silently
    // overwritten, and the before-positions are what make `side_effects`
    // computable.
    let before = PlacementFile::from_toml(toml)
        .and_then(|f| f.positions())
        .map_err(|e| PlacementEditError::Stale(e.to_string()))?;

    let mut doc: DocumentMut = toml.parse()?;
    for op in ops {
        apply(&mut doc, op)?;
    }

    let out = doc.to_string();
    // Fail loudly on a corrupt result rather than persisting it.
    let file =
        PlacementFile::from_toml(&out).map_err(|e| PlacementEditError::Invalid(e.to_string()))?;
    // …and on a file that parses but cannot be expanded (two patterns claiming
    // one part), which is the failure that would otherwise reach the board.
    let positions = file
        .positions()
        .map_err(|e| PlacementEditError::Conflict(e.to_string()))?;

    let named: HashSet<&str> = ops.iter().flat_map(PlacementOp::refdes).collect();
    let mut side_effects: Vec<Moved> = positions
        .iter()
        .filter(|(r, _)| !named.contains(r.as_str()))
        .filter_map(|(r, to)| {
            let from = before.get(r)?;
            (from != to).then(|| Moved {
                refdes: r.clone(),
                from: *from,
                to: *to,
            })
        })
        .collect();
    side_effects.sort_by(|a, b| a.refdes.cmp(&b.refdes));

    Ok(PlacementEditResult {
        toml: out,
        file,
        positions,
        side_effects,
    })
}

/// Apply `ops` to the placement file at `path` in place, preserving formatting.
/// A missing file is created — the first hand placement on a circuit that never
/// had one is the common case, not an error.
pub fn edit_placement(
    path: &Path,
    ops: &[PlacementOp],
    known: &HashSet<String>,
) -> Result<PlacementEditResult, PlacementEditError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(PlacementEditError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    let result = apply_placement_edit_str(&text, ops, known)?;
    std::fs::write(path, &result.toml).map_err(|source| PlacementEditError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(result)
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

fn validate(op: &PlacementOp) -> Result<(), PlacementEditError> {
    let finite = |n: f64, what: &'static str| {
        n.is_finite()
            .then_some(())
            .ok_or(PlacementEditError::NotFinite(what))
    };
    let members = |refdes: &Vec<String>| -> Result<(), PlacementEditError> {
        if refdes.is_empty() {
            return Err(PlacementEditError::EmptyPattern);
        }
        let mut seen = HashSet::new();
        for r in refdes {
            if !seen.insert(r.as_str()) {
                return Err(PlacementEditError::RepeatedRefdes(r.clone()));
            }
        }
        Ok(())
    };
    // A pitch of zero stacks every member of a strip on one hole. On a single
    // part it is meaningless rather than wrong, so only multi-part patterns care.
    let pitch = |p: f64, n: usize, what: &'static str| -> Result<(), PlacementEditError> {
        finite(p, what)?;
        if n > 1 && p == 0.0 {
            return Err(PlacementEditError::ZeroPitch);
        }
        Ok(())
    };

    match op {
        PlacementOp::SetControl { x, y, .. } => {
            finite(*x, "x")?;
            finite(*y, "y")?;
        }
        PlacementOp::ClearControl { .. } | PlacementOp::DropFromPattern { .. } => {}
        PlacementOp::BreakPattern { refdes } => {
            if refdes.is_empty() {
                return Err(PlacementEditError::EmptyPattern);
            }
        }
        PlacementOp::SetColumn {
            refdes,
            x,
            from_y,
            pitch: p,
        } => {
            members(refdes)?;
            finite(*x, "x")?;
            finite(*from_y, "from_y")?;
            pitch(*p, refdes.len(), "pitch")?;
        }
        PlacementOp::SetRow {
            refdes,
            y,
            from_x,
            pitch: p,
        } => {
            members(refdes)?;
            finite(*y, "y")?;
            finite(*from_x, "from_x")?;
            pitch(*p, refdes.len(), "pitch")?;
        }
        PlacementOp::SetGrid {
            refdes,
            x,
            y,
            cols,
            pitch_x,
            pitch_y,
        } => {
            members(refdes)?;
            if *cols == 0 {
                return Err(PlacementEditError::EmptyGrid);
            }
            finite(*x, "x")?;
            finite(*y, "y")?;
            // Across is only a pitch when there is more than one column, and
            // downward only when there is more than one row.
            pitch(*pitch_x, (*cols).min(refdes.len()), "pitch_x")?;
            pitch(*pitch_y, refdes.len().div_ceil(*cols), "pitch_y")?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Document surgery
// ---------------------------------------------------------------------------

/// The three `[[patterns.*]]` array keys, in the order [`PlacementFile`] expands
/// them.
const PATTERN_KINDS: [&str; 3] = ["column", "row", "grid"];

fn apply(doc: &mut DocumentMut, op: &PlacementOp) -> Result<(), PlacementEditError> {
    match op {
        PlacementOp::SetControl { refdes, x, y } => {
            let controls = controls_table(doc)?;
            set_point(controls, refdes, *x, *y);
        }
        PlacementOp::ClearControl { refdes } => {
            // Only reach for the table if there is one; creating `[controls]`
            // just to delete nothing from it would be noise in the file.
            if let Some(t) = doc.get_mut("controls").and_then(Item::as_table_mut) {
                t.remove(refdes);
            }
        }
        PlacementOp::DropFromPattern { refdes } => {
            drop_members(doc, std::slice::from_ref(refdes))?;
        }
        PlacementOp::BreakPattern { refdes } => {
            break_patterns(doc, refdes)?;
        }
        PlacementOp::SetColumn {
            refdes,
            x,
            from_y,
            pitch,
        } => {
            let fields = [("x", *x), ("from_y", *from_y), ("pitch", *pitch)];
            set_pattern(doc, "column", refdes, &fields, &[])?;
        }
        PlacementOp::SetRow {
            refdes,
            y,
            from_x,
            pitch,
        } => {
            let fields = [("y", *y), ("from_x", *from_x), ("pitch", *pitch)];
            set_pattern(doc, "row", refdes, &fields, &[])?;
        }
        PlacementOp::SetGrid {
            refdes,
            x,
            y,
            cols,
            pitch_x,
            pitch_y,
        } => {
            let fields = [
                ("x", *x),
                ("y", *y),
                ("pitch_x", *pitch_x),
                ("pitch_y", *pitch_y),
            ];
            set_pattern(doc, "grid", refdes, &fields, &[("cols", *cols as i64)])?;
        }
    }
    Ok(())
}

/// The `[controls]` table, created if absent. Unlike `[patterns]` this is a real
/// header in the file, so it is not implicit.
fn controls_table(doc: &mut DocumentMut) -> Result<&mut Table, PlacementEditError> {
    doc.entry("controls")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or(PlacementEditError::NotATable("controls"))
}

/// Write `refdes = { x, y }`, updating an existing entry in place so its
/// formatting (inline vs table, spacing, trailing comment) survives.
fn set_point(controls: &mut Table, refdes: &str, x: f64, y: f64) {
    let (x, y) = (round_mm(x), round_mm(y));
    match controls.get_mut(refdes) {
        Some(Item::Value(Value::InlineTable(it))) => {
            set_inline_num(it, "x", x);
            set_inline_num(it, "y", y);
        }
        Some(Item::Table(t)) => {
            t["x"] = value(x);
            t["y"] = value(y);
        }
        _ => {
            let mut it = InlineTable::new();
            it.insert("x", Value::from(x));
            it.insert("y", Value::from(y));
            controls[refdes] = Item::Value(Value::InlineTable(it));
        }
    }
}

/// Replace an inline-table number while keeping the whitespace around it.
fn set_inline_num(it: &mut InlineTable, key: &str, n: f64) {
    match it.get_mut(key) {
        Some(old) => {
            let decor = old.decor().clone();
            *old = Value::from(n);
            *old.decor_mut() = decor;
        }
        None => {
            it.insert(key, Value::from(n));
        }
    }
}

/// Create or update one `[[patterns.<kind>]]` table.
///
/// An existing table of the same kind holding the same *set* of parts is updated
/// in place — that is the "set the pitch on this strip" path, and updating rather
/// than re-emitting is what keeps a hand-written comment attached to the strip it
/// describes. Otherwise the named parts are taken out of any pattern they were
/// in and a fresh table is appended.
fn set_pattern(
    doc: &mut DocumentMut,
    kind: &'static str,
    refdes: &[String],
    floats: &[(&str, f64)],
    ints: &[(&str, i64)],
) -> Result<(), PlacementEditError> {
    let wanted: HashSet<&str> = refdes.iter().map(String::as_str).collect();

    if let Some(arr) = pattern_array(doc, kind) {
        if let Some(t) = arr.iter_mut().find(|t| {
            table_refdes(t)
                .iter()
                .map(String::as_str)
                .collect::<HashSet<_>>()
                == wanted
        }) {
            // Order can still have changed (a re-ordered selection), so rewrite
            // the member list; the scalars follow.
            t["refdes"] = value(string_array(refdes));
            write_fields(t, floats, ints);
            return Ok(());
        }
    }

    drop_members(doc, refdes)?;
    let mut t = Table::new();
    t["refdes"] = value(string_array(refdes));
    write_fields(&mut t, floats, ints);
    pattern_array_mut(doc, kind)?.push(t);
    Ok(())
}

fn write_fields(t: &mut Table, floats: &[(&str, f64)], ints: &[(&str, i64)]) {
    for (k, v) in floats {
        t[*k] = value(round_mm(*v));
    }
    for (k, v) in ints {
        t[*k] = value(*v);
    }
}

/// Expand every pattern touching `refdes` into `[controls]` entries at the
/// positions it is currently producing, then delete it.
///
/// The positions are re-derived from the document as it stands *now* rather than
/// from where the batch started, because ops apply in order and an earlier op in
/// the same batch may already have moved the strip. Re-expanding is a parse of a
/// file with single-digit tables — cheap next to being subtly wrong.
fn break_patterns(doc: &mut DocumentMut, refdes: &[String]) -> Result<(), PlacementEditError> {
    let touched: HashSet<&str> = refdes.iter().map(String::as_str).collect();
    let current = PlacementFile::from_toml(&doc.to_string())
        .and_then(|f| f.positions())
        .map_err(|e| PlacementEditError::Conflict(e.to_string()))?;

    // Collect the members first: the borrow of `doc` for the pattern arrays
    // cannot overlap the borrow for `[controls]`.
    let mut members: Vec<String> = Vec::new();
    for kind in PATTERN_KINDS {
        let Some(arr) = pattern_array(doc, kind) else {
            continue;
        };
        for t in arr.iter() {
            let list = table_refdes(t);
            if list.iter().any(|r| touched.contains(r.as_str())) {
                members.extend(list);
            }
        }
    }
    if members.is_empty() {
        return Ok(());
    }
    // Pin every member where it already is, *then* remove the pattern — so the
    // one thing that must not happen (a part moving) cannot.
    for r in &members {
        if let Some(p) = current.get(r) {
            let controls = controls_table(doc)?;
            set_point(controls, r, p.x, p.y);
        }
    }
    drop_members(doc, &members)
}

/// Remove `refdes` from every pattern that names them, deleting any pattern left
/// with no members and any pattern array left empty.
fn drop_members(doc: &mut DocumentMut, refdes: &[String]) -> Result<(), PlacementEditError> {
    let drop: HashSet<&str> = refdes.iter().map(String::as_str).collect();
    for kind in PATTERN_KINDS {
        let Some(arr) = pattern_array(doc, kind) else {
            continue;
        };
        for t in arr.iter_mut() {
            let kept: Vec<String> = table_refdes(t)
                .into_iter()
                .filter(|r| !drop.contains(r.as_str()))
                .collect();
            if !kept.is_empty() {
                t["refdes"] = value(string_array(&kept));
            } else {
                // Marker: an empty list is not expressible in `PlacementFile`
                // semantics, so the table goes rather than being written back.
                t["refdes"] = value(Array::new());
            }
        }
        arr.retain(|t| !table_refdes(t).is_empty());
        if arr.is_empty() {
            if let Some(p) = doc.get_mut("patterns").and_then(Item::as_table_mut) {
                p.remove(kind);
            }
        }
    }
    // A `[patterns]` table with nothing under it would still parse, but it is
    // noise; drop it once its last array is gone.
    if doc
        .get("patterns")
        .and_then(Item::as_table)
        .is_some_and(|t| t.is_empty())
    {
        doc.as_table_mut().remove("patterns");
    }
    Ok(())
}

/// The `[[patterns.<kind>]]` array, if the file has one.
fn pattern_array<'a>(doc: &'a mut DocumentMut, kind: &str) -> Option<&'a mut ArrayOfTables> {
    doc.get_mut("patterns")?
        .as_table_mut()?
        .get_mut(kind)?
        .as_array_of_tables_mut()
}

/// The `[[patterns.<kind>]]` array, created if absent. `[patterns]` itself is
/// implicit so no bare `[patterns]` header is emitted above the arrays.
fn pattern_array_mut<'a>(
    doc: &'a mut DocumentMut,
    kind: &'static str,
) -> Result<&'a mut ArrayOfTables, PlacementEditError> {
    let patterns = doc
        .entry("patterns")
        .or_insert_with(|| {
            let mut t = Table::new();
            t.set_implicit(true);
            Item::Table(t)
        })
        .as_table_mut()
        .ok_or(PlacementEditError::NotATable("patterns"))?;
    patterns
        .entry(kind)
        .or_insert(Item::ArrayOfTables(ArrayOfTables::new()))
        .as_array_of_tables_mut()
        .ok_or(PlacementEditError::NotATable("patterns"))
}

/// A pattern table's member list. A malformed `refdes` reads as empty here and
/// is caught by the re-parse at the end, so this never has to guess.
fn table_refdes(t: &Table) -> Vec<String> {
    t.get("refdes")
        .and_then(Item::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn string_array(refdes: &[String]) -> Array {
    refdes.iter().map(String::as_str).collect()
}

/// Round to [`MM_DECIMALS`], and normalise `-0.0` so a part dragged to the panel
/// edge does not write a negative zero.
fn round_mm(n: f64) -> f64 {
    let f = 10f64.powi(MM_DECIMALS);
    let r = (n * f).round() / f;
    if r == 0.0 {
        0.0
    } else {
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"# Slew limiter — hand placement.
hp = 5

[controls]
RV1 = { x = 12.7, y = 113.5 }   # RATE, above the strip

[[patterns.column]]             # the jack strip
refdes = ["J1", "J2", "J4"]
x      = 6.0
from_y = 10.5
pitch  = 13.5
"#;

    fn known(refdes: &[&str]) -> HashSet<String> {
        refdes.iter().map(|r| r.to_string()).collect()
    }

    fn all() -> HashSet<String> {
        known(&["J1", "J2", "J4", "RV1", "RV2", "SW1", "SW2", "SW3", "SW4"])
    }

    fn apply_ok(toml: &str, ops: &[PlacementOp]) -> PlacementEditResult {
        apply_placement_edit_str(toml, ops, &all()).expect("apply")
    }

    #[test]
    fn setting_a_control_preserves_comments_and_the_pattern() {
        let r = apply_ok(
            SAMPLE,
            &[PlacementOp::SetControl {
                refdes: "RV1".into(),
                x: 12.7,
                y: 100.0,
            }],
        );
        assert!(r.toml.contains("# Slew limiter — hand placement."));
        assert!(r.toml.contains("# RATE, above the strip"));
        assert!(r.toml.contains("# the jack strip"));
        assert!(r.toml.contains("pitch  = 13.5"), "{}", r.toml);
        assert_eq!(r.positions["RV1"], Point { x: 12.7, y: 100.0 });
        // The strip is untouched, so nothing moved behind the user's back.
        assert!(r.side_effects.is_empty());
    }

    /// The escape hatch from `crate::placement`, driven from the editor: one part
    /// leaves the strip and the strip is not unpicked.
    #[test]
    fn overriding_a_pattern_member_leaves_the_pattern_intact() {
        let r = apply_ok(
            SAMPLE,
            &[PlacementOp::SetControl {
                refdes: "J2".into(),
                x: 20.0,
                y: 99.0,
            }],
        );
        assert!(
            r.toml.contains(r#"refdes = ["J1", "J2", "J4"]"#),
            "{}",
            r.toml
        );
        assert_eq!(r.positions["J2"], Point { x: 20.0, y: 99.0 });
        // J1 and J4 stay exactly where they were.
        assert_eq!(r.positions["J1"], Point { x: 6.0, y: 10.5 });
        assert_eq!(r.positions["J4"], Point { x: 6.0, y: 37.5 });
        assert!(r.side_effects.is_empty());
    }

    #[test]
    fn clearing_a_control_returns_the_part_to_its_pattern() {
        let overridden = apply_ok(
            SAMPLE,
            &[PlacementOp::SetControl {
                refdes: "J2".into(),
                x: 20.0,
                y: 99.0,
            }],
        )
        .toml;
        let r = apply_ok(
            &overridden,
            &[PlacementOp::ClearControl {
                refdes: "J2".into(),
            }],
        );
        assert_eq!(r.positions["J2"], Point { x: 6.0, y: 24.0 });
        // The unrelated override is still there.
        assert_eq!(r.positions["RV1"], Point { x: 12.7, y: 113.5 });
    }

    #[test]
    fn clearing_a_control_that_was_never_set_is_a_no_op() {
        let r = apply_ok(
            SAMPLE,
            &[PlacementOp::ClearControl {
                refdes: "J1".into(),
            }],
        );
        assert_eq!(r.positions["J1"], Point { x: 6.0, y: 10.5 });
    }

    /// The test the epic names: after "make column, pitch 13.5" the file holds one
    /// pattern table, not three coordinates that happen to share an x.
    #[test]
    fn making_a_column_writes_one_pattern_not_n_controls() {
        let r = apply_ok(
            "",
            &[PlacementOp::SetColumn {
                refdes: vec!["J1".into(), "J2".into(), "J4".into()],
                x: 6.0,
                from_y: 10.5,
                pitch: 13.5,
            }],
        );
        assert!(r.toml.contains("[[patterns.column]]"), "{}", r.toml);
        assert!(!r.toml.contains("[controls]"), "{}", r.toml);
        assert_eq!(r.toml.matches("[[patterns.column]]").count(), 1);
        assert_eq!(r.positions["J4"], Point { x: 6.0, y: 37.5 });
        // No bare `[patterns]` header above the array.
        assert!(!r.toml.contains("\n[patterns]"), "{}", r.toml);
    }

    /// Changing the pitch of an existing strip must not re-emit the table, or the
    /// comment explaining the strip is lost.
    #[test]
    fn setting_the_pitch_updates_the_strip_in_place() {
        let r = apply_ok(
            SAMPLE,
            &[PlacementOp::SetColumn {
                refdes: vec!["J1".into(), "J2".into(), "J4".into()],
                x: 6.0,
                from_y: 10.5,
                pitch: 16.0,
            }],
        );
        assert_eq!(r.toml.matches("[[patterns.column]]").count(), 1);
        assert!(r.toml.contains("# the jack strip"), "{}", r.toml);
        assert_eq!(r.positions["J2"], Point { x: 6.0, y: 26.5 });
        // J4 moved without being dragged — the caller is told.
        assert!(r.side_effects.is_empty(), "all three were named: {r:?}");
    }

    /// A selection given in a different order updates the same table rather than
    /// leaving a stale duplicate behind.
    #[test]
    fn reordering_a_strip_reuses_its_table() {
        let r = apply_ok(
            SAMPLE,
            &[PlacementOp::SetColumn {
                refdes: vec!["J4".into(), "J1".into(), "J2".into()],
                x: 6.0,
                from_y: 10.5,
                pitch: 13.5,
            }],
        );
        assert_eq!(r.toml.matches("[[patterns.column]]").count(), 1);
        assert_eq!(r.positions["J4"], Point { x: 6.0, y: 10.5 });
        assert_eq!(r.positions["J2"], Point { x: 6.0, y: 37.5 });
    }

    #[test]
    fn a_grid_round_trips_through_the_writer() {
        let r = apply_ok(
            SAMPLE,
            &[PlacementOp::SetGrid {
                refdes: vec!["SW1".into(), "SW2".into(), "SW3".into(), "SW4".into()],
                x: 6.0,
                y: 60.0,
                cols: 2,
                pitch_x: 12.0,
                pitch_y: 12.0,
            }],
        );
        assert!(r.toml.contains("[[patterns.grid]]"), "{}", r.toml);
        assert_eq!(r.positions["SW2"], Point { x: 18.0, y: 60.0 });
        // Across, then *down* the panel, which is a lower y.
        assert_eq!(r.positions["SW3"], Point { x: 6.0, y: 48.0 });
    }

    /// Moving parts between patterns must not leave them claimed by both — that
    /// is the one thing `PlacementFile::positions` refuses to guess about.
    #[test]
    fn a_part_moved_into_a_new_pattern_leaves_the_old_one() {
        let r = apply_ok(
            SAMPLE,
            &[PlacementOp::SetRow {
                refdes: vec!["J2".into(), "J4".into()],
                y: 50.0,
                from_x: 6.0,
                pitch: 10.0,
            }],
        );
        assert!(r.toml.contains(r#"refdes = ["J1"]"#), "{}", r.toml);
        assert_eq!(r.positions["J1"], Point { x: 6.0, y: 10.5 });
        assert_eq!(r.positions["J4"], Point { x: 16.0, y: 50.0 });
    }

    /// A strip that loses a middle member closes the gap. That is what a strip
    /// is — but the parts that slid are reported rather than moving silently.
    #[test]
    fn dropping_a_middle_member_closes_the_gap_and_reports_it() {
        let r = apply_ok(
            SAMPLE,
            &[PlacementOp::DropFromPattern {
                refdes: "J2".into(),
            }],
        );
        assert!(r.toml.contains(r#"refdes = ["J1", "J4"]"#), "{}", r.toml);
        assert!(!r.positions.contains_key("J2"));
        assert_eq!(r.positions["J4"], Point { x: 6.0, y: 24.0 });
        assert_eq!(
            r.side_effects,
            vec![Moved {
                refdes: "J4".into(),
                from: Point { x: 6.0, y: 37.5 },
                to: Point { x: 6.0, y: 24.0 },
            }]
        );
    }

    #[test]
    fn emptying_a_pattern_deletes_the_table_and_the_array() {
        let mut text = SAMPLE.to_string();
        for r in ["J1", "J2", "J4"] {
            text = apply_ok(&text, &[PlacementOp::DropFromPattern { refdes: r.into() }]).toml;
        }
        assert!(!text.contains("patterns"), "{text}");
        // The unrelated control and the file's own comments survive.
        assert!(text.contains("RV1"));
        assert!(text.contains("# Slew limiter — hand placement."));
        assert_eq!(PlacementFile::from_toml(&text).unwrap().hp, Some(5));
    }

    /// Breaking a strip must not move anything — that is the whole contract.
    #[test]
    fn breaking_a_pattern_pins_every_member_where_it_already_was() {
        let before = apply_ok(SAMPLE, &[]).positions;
        let r = apply_ok(
            SAMPLE,
            &[PlacementOp::BreakPattern {
                refdes: vec!["J2".into()],
            }],
        );
        assert!(!r.toml.contains("patterns"), "{}", r.toml);
        for refdes in ["J1", "J2", "J4"] {
            assert_eq!(r.positions[refdes], before[refdes], "{refdes} moved");
        }
        assert!(r.side_effects.is_empty());
        // The unrelated override and the file's comments survive.
        assert_eq!(r.positions["RV1"], Point { x: 12.7, y: 113.5 });
        assert!(r.toml.contains("# Slew limiter — hand placement."));
    }

    /// Ops apply in order, so a break must expand the strip as it stands *after*
    /// the earlier ops in the same batch — not as it was when the batch began.
    #[test]
    fn breaking_after_a_pitch_change_uses_the_new_pitch() {
        let r = apply_ok(
            SAMPLE,
            &[
                PlacementOp::SetColumn {
                    refdes: vec!["J1".into(), "J2".into(), "J4".into()],
                    x: 6.0,
                    from_y: 10.0,
                    pitch: 20.0,
                },
                PlacementOp::BreakPattern {
                    refdes: vec!["J1".into()],
                },
            ],
        );
        assert!(!r.toml.contains("patterns"), "{}", r.toml);
        assert_eq!(r.positions["J4"], Point { x: 6.0, y: 50.0 });
    }

    #[test]
    fn breaking_a_part_that_is_in_no_pattern_is_a_no_op() {
        let r = apply_ok(
            SAMPLE,
            &[PlacementOp::BreakPattern {
                refdes: vec!["RV1".into()],
            }],
        );
        assert!(r.toml.contains("[[patterns.column]]"), "{}", r.toml);
    }

    #[test]
    fn the_write_guard_refuses_parts_the_circuit_does_not_have() {
        let err = apply_placement_edit_str(
            SAMPLE,
            &[PlacementOp::SetControl {
                refdes: "J9".into(),
                x: 1.0,
                y: 1.0,
            }],
            &known(&["J1", "J2", "J4", "RV1"]),
        )
        .unwrap_err();
        assert!(matches!(err, PlacementEditError::UnknownRefdes(v) if v == ["J9"]));
    }

    /// A rejected batch must not half-apply: the guard runs before any surgery.
    #[test]
    fn a_batch_with_one_bad_refdes_changes_nothing() {
        let err = apply_placement_edit_str(
            SAMPLE,
            &[
                PlacementOp::SetControl {
                    refdes: "RV1".into(),
                    x: 1.0,
                    y: 1.0,
                },
                PlacementOp::SetControl {
                    refdes: "NOPE".into(),
                    x: 2.0,
                    y: 2.0,
                },
            ],
            &all(),
        )
        .unwrap_err();
        assert!(matches!(err, PlacementEditError::UnknownRefdes(_)));
    }

    #[test]
    fn ops_apply_in_order_so_clear_then_pattern_is_one_batch() {
        let r = apply_ok(
            SAMPLE,
            &[
                PlacementOp::ClearControl {
                    refdes: "RV1".into(),
                },
                PlacementOp::SetColumn {
                    refdes: vec!["RV1".into(), "RV2".into()],
                    x: 12.7,
                    from_y: 90.0,
                    pitch: 20.0,
                },
            ],
        );
        assert_eq!(r.positions["RV1"], Point { x: 12.7, y: 90.0 });
        assert_eq!(r.positions["RV2"], Point { x: 12.7, y: 110.0 });
    }

    #[test]
    fn coordinates_are_rounded_to_a_micron() {
        let r = apply_ok(
            "",
            &[PlacementOp::SetControl {
                refdes: "RV1".into(),
                x: 20.319999999999993,
                y: -0.0,
            }],
        );
        assert!(r.toml.contains("20.32"), "{}", r.toml);
        assert!(!r.toml.contains("-0.0"), "{}", r.toml);
    }

    #[test]
    fn an_already_broken_file_is_not_overwritten() {
        // Two patterns claiming J1 — parses, but cannot be expanded.
        let broken = r#"
[[patterns.column]]
refdes = ["J1"]
x = 6.0
from_y = 10.0
pitch = 10.0

[[patterns.row]]
refdes = ["J1"]
y = 50.0
from_x = 6.0
pitch = 10.0
"#;
        let err = apply_placement_edit_str(
            broken,
            &[PlacementOp::SetControl {
                refdes: "RV1".into(),
                x: 1.0,
                y: 1.0,
            }],
            &all(),
        )
        .unwrap_err();
        assert!(matches!(err, PlacementEditError::Stale(_)), "{err:?}");
    }

    #[test]
    fn rejects_degenerate_patterns() {
        let cases: Vec<(PlacementOp, &str)> = vec![
            (
                PlacementOp::SetColumn {
                    refdes: vec![],
                    x: 0.0,
                    from_y: 0.0,
                    pitch: 1.0,
                },
                "empty",
            ),
            (
                PlacementOp::SetColumn {
                    refdes: vec!["J1".into(), "J1".into()],
                    x: 0.0,
                    from_y: 0.0,
                    pitch: 1.0,
                },
                "repeated",
            ),
            (
                PlacementOp::SetColumn {
                    refdes: vec!["J1".into(), "J2".into()],
                    x: 0.0,
                    from_y: 0.0,
                    pitch: 0.0,
                },
                "zero pitch",
            ),
            (
                PlacementOp::SetGrid {
                    refdes: vec!["J1".into()],
                    x: 0.0,
                    y: 0.0,
                    cols: 0,
                    pitch_x: 1.0,
                    pitch_y: 1.0,
                },
                "zero cols",
            ),
            (
                PlacementOp::SetControl {
                    refdes: "J1".into(),
                    x: f64::NAN,
                    y: 0.0,
                },
                "NaN",
            ),
        ];
        for (op, what) in cases {
            assert!(
                apply_placement_edit_str(SAMPLE, &[op], &all()).is_err(),
                "{what} should be rejected"
            );
        }
    }

    /// A one-part column has no pitch to speak of, and demanding one would make
    /// "start a strip" impossible.
    #[test]
    fn a_single_part_pattern_may_have_a_zero_pitch() {
        let r = apply_ok(
            "",
            &[PlacementOp::SetColumn {
                refdes: vec!["J1".into()],
                x: 6.0,
                from_y: 10.0,
                pitch: 0.0,
            }],
        );
        assert_eq!(r.positions["J1"], Point { x: 6.0, y: 10.0 });
    }

    /// Round-tripping through the editor must not increase the file's entropy.
    #[test]
    fn re_applying_the_same_edit_is_idempotent() {
        let op = PlacementOp::SetColumn {
            refdes: vec!["J1".into(), "J2".into(), "J4".into()],
            x: 6.0,
            from_y: 10.5,
            pitch: 13.5,
        };
        let once = apply_ok(SAMPLE, std::slice::from_ref(&op)).toml;
        let twice = apply_ok(&once, std::slice::from_ref(&op)).toml;
        assert_eq!(once, twice);
    }

    #[test]
    fn an_empty_file_is_a_valid_starting_point() {
        let r = apply_ok("", &[]);
        assert!(r.positions.is_empty());
        assert_eq!(r.toml, "");
    }

    /// What the alignment and spacing tools are made of: given where parts should
    /// end up, does the file still say *why*?
    mod targets {
        use super::*;

        fn file() -> PlacementFile {
            PlacementFile::from_toml(SAMPLE).unwrap()
        }

        fn targets(pairs: &[(&str, f64, f64)]) -> BTreeMap<String, Point> {
            pairs
                .iter()
                .map(|(r, x, y)| (r.to_string(), Point { x: *x, y: *y }))
                .collect()
        }

        /// Run the derived ops for real, so what is asserted is the file, not a
        /// prediction about it.
        fn run(f: &PlacementFile, t: &BTreeMap<String, Point>) -> PlacementEditResult {
            let ops = ops_for_targets(f, t);
            apply_placement_edit_str(SAMPLE, &ops, &all()).expect("apply")
        }

        /// Dragging a whole strip moves the strip. The alternative — three
        /// coordinates that happen to be 13.5mm apart — is what this epic exists
        /// to avoid.
        #[test]
        fn moving_a_whole_column_keeps_it_a_column() {
            let t = targets(&[("J1", 12.0, 20.5), ("J2", 12.0, 34.0), ("J4", 12.0, 47.5)]);
            let r = run(&file(), &t);
            assert_eq!(r.toml.matches("[[patterns.column]]").count(), 1);
            assert!(r.toml.contains("pitch"), "{}", r.toml);
            assert!(!r.toml.contains("J1 = {"), "no coordinate soup: {}", r.toml);
            assert_eq!(r.positions["J4"], Point { x: 12.0, y: 47.5 });
        }

        /// Aligning a column's x is the case the epic calls out by name: the file
        /// should still say "a column at x=6.0" afterwards.
        #[test]
        fn aligning_a_columns_x_updates_the_pattern_not_its_members() {
            let t = targets(&[("J1", 9.0, 10.5), ("J2", 9.0, 24.0), ("J4", 9.0, 37.5)]);
            let r = run(&file(), &t);
            assert_eq!(r.toml.matches("[[patterns.column]]").count(), 1);
            // Updated in place, keeping the file's own key alignment.
            assert!(r.toml.contains("x      = 9.0"), "{}", r.toml);
            assert!(r.toml.contains(r#"refdes = ["J1", "J2", "J4"]"#));
            assert!(r.toml.contains("# the jack strip"));
            assert_eq!(r.positions["J4"], Point { x: 9.0, y: 37.5 });
        }

        /// A column aligned onto one y is not a column any more. Breaking it is
        /// the honest answer; leaving the table there with every member
        /// overridden would be a file that lies about its own shape.
        #[test]
        fn a_shape_a_pattern_cannot_express_breaks_it_deliberately() {
            let t = targets(&[("J1", 6.0, 50.0), ("J2", 14.0, 50.0), ("J4", 22.0, 50.0)]);
            let r = run(&file(), &t);
            assert!(!r.toml.contains("[[patterns.column]]"), "{}", r.toml);
            assert_eq!(r.positions["J2"], Point { x: 14.0, y: 50.0 });
            assert_eq!(r.positions["J4"], Point { x: 22.0, y: 50.0 });
            // And nothing moved that was not asked to.
            assert_eq!(r.positions["RV1"], Point { x: 12.7, y: 113.5 });
        }

        /// Moving *part* of a strip is the escape hatch, not a reshape: the one
        /// part leaves, and its neighbours do not budge.
        #[test]
        fn moving_one_member_overrides_it_and_leaves_the_strip_alone() {
            let t = targets(&[("J2", 20.0, 90.0)]);
            let r = run(&file(), &t);
            assert!(
                r.toml.contains(r#"refdes = ["J1", "J2", "J4"]"#),
                "{}",
                r.toml
            );
            assert_eq!(r.positions["J2"], Point { x: 20.0, y: 90.0 });
            assert_eq!(r.positions["J1"], Point { x: 6.0, y: 10.5 });
            assert_eq!(r.positions["J4"], Point { x: 6.0, y: 37.5 });
        }

        /// A mirror reverses a row. Writing it with a negative pitch would be
        /// correct and unreadable.
        #[test]
        fn a_mirrored_row_is_rewritten_bottom_up_rather_than_with_a_negative_pitch() {
            let f = PlacementFile::from_toml(
                r#"
[[patterns.row]]
refdes = ["J1", "J2", "J4"]
y      = 50.0
from_x = 4.0
pitch  = 8.0
"#,
            )
            .unwrap();
            // Mirrored about a 25.4mm panel: 4/12/20 -> 21.4/13.4/5.4.
            let t = targets(&[("J1", 21.4, 50.0), ("J2", 13.4, 50.0), ("J4", 5.4, 50.0)]);
            let ops = ops_for_targets(&f, &t);
            match &ops[0] {
                PlacementOp::SetRow {
                    refdes,
                    from_x,
                    pitch,
                    ..
                } => {
                    assert_eq!(refdes, &["J4", "J2", "J1"]);
                    assert!((*from_x - 5.4).abs() < 1e-9);
                    assert!((*pitch - 8.0).abs() < 1e-9, "pitch {pitch}");
                }
                other => panic!("expected a row, got {other:?}"),
            }
        }

        /// A strip whose member was nudged out has a *suspended* definition, not a
        /// dead one. Moving the lot to a shape it cannot describe must not destroy
        /// it — `ClearControl` can still bring it back.
        #[test]
        fn a_suspended_pattern_survives_a_move_it_cannot_describe() {
            let f = PlacementFile::from_toml(
                r#"
[controls]
J2 = { x = 20.0, y = 99.0 }

[[patterns.column]]
refdes = ["J1", "J2"]
x      = 6.0
from_y = 10.0
pitch  = 10.0
"#,
            )
            .unwrap();
            let t = targets(&[("J1", 8.0, 12.0), ("J2", 22.0, 101.0)]);
            let ops = ops_for_targets(&f, &t);
            assert!(
                ops.iter()
                    .all(|o| matches!(o, PlacementOp::SetControl { .. })),
                "{ops:?}"
            );
        }

        /// …but parts that come back onto the strip's shape rejoin it, override
        /// and all. Otherwise the file grows a pattern plus a set of overrides
        /// that say the same thing.
        #[test]
        fn parts_that_land_back_on_the_strip_rejoin_it() {
            let f = PlacementFile::from_toml(
                r#"
[controls]
J2 = { x = 20.0, y = 99.0 }

[[patterns.column]]
refdes = ["J1", "J2"]
x      = 6.0
from_y = 10.0
pitch  = 10.0
"#,
            )
            .unwrap();
            // Both land on one column again, 2mm to the right.
            let t = targets(&[("J1", 8.0, 10.0), ("J2", 8.0, 20.0)]);
            let ops = ops_for_targets(&f, &t);
            assert!(
                matches!(
                    ops.as_slice(),
                    [
                        PlacementOp::ClearControl { refdes },
                        PlacementOp::SetColumn { .. }
                    ] if refdes == "J2"
                ),
                "{ops:?}"
            );
        }

        #[test]
        fn a_grid_that_keeps_its_shape_stays_a_grid() {
            let f = PlacementFile::from_toml(
                r#"
[[patterns.grid]]
refdes  = ["SW1", "SW2", "SW3", "SW4"]
x       = 6.0
y       = 60.0
cols    = 2
pitch_x = 12.0
pitch_y = 12.0
"#,
            )
            .unwrap();
            // Shifted 2mm right, 5mm up — across then down is preserved.
            let t = targets(&[
                ("SW1", 8.0, 65.0),
                ("SW2", 20.0, 65.0),
                ("SW3", 8.0, 53.0),
                ("SW4", 20.0, 53.0),
            ]);
            let ops = ops_for_targets(&f, &t);
            assert!(
                matches!(ops.as_slice(), [PlacementOp::SetGrid { .. }]),
                "{ops:?}"
            );
        }

        /// Round trip: whatever a tool does, asking for the previous file back
        /// must restore it exactly. This is undo for every tool at once.
        #[test]
        fn asking_for_the_previous_file_back_restores_it() {
            let before = file();
            let cases: Vec<Vec<PlacementOp>> = vec![
                ops_for_targets(
                    &before,
                    &targets(&[("J1", 9.0, 10.5), ("J2", 9.0, 24.0), ("J4", 9.0, 37.5)]),
                ),
                ops_for_targets(
                    &before,
                    &targets(&[("J1", 6.0, 50.0), ("J2", 14.0, 50.0), ("J4", 22.0, 50.0)]),
                ),
                vec![PlacementOp::BreakPattern {
                    refdes: vec!["J1".into()],
                }],
                vec![PlacementOp::SetColumn {
                    refdes: vec!["J1".into(), "J2".into(), "J4".into(), "RV2".into()],
                    x: 3.0,
                    from_y: 5.0,
                    pitch: 20.0,
                }],
                vec![PlacementOp::DropFromPattern {
                    refdes: "J2".into(),
                }],
            ];
            for (i, ops) in cases.into_iter().enumerate() {
                let after = apply_placement_edit_str(SAMPLE, &ops, &all()).expect("apply");
                let undo = ops_to_reach(&after.file, &before);
                let back = apply_placement_edit_str(&after.toml, &undo, &all()).expect("undo");
                assert_eq!(
                    back.positions,
                    apply_ok(SAMPLE, &[]).positions,
                    "case {i} did not come back"
                );
                assert_eq!(back.file, before, "case {i} restored different intent");
            }
        }

        /// Restoring a file that is already what is wanted asks for no changes.
        #[test]
        fn reaching_a_file_that_is_already_current_is_a_no_op_write() {
            let f = file();
            let ops = ops_to_reach(&f, &f);
            let r = apply_placement_edit_str(SAMPLE, &ops, &all()).expect("apply");
            assert_eq!(r.file, f);
            assert!(r.side_effects.is_empty());
        }

        /// The ops a tool produces must not depend on hash-map iteration order,
        /// or two identical gestures write two different files.
        #[test]
        fn the_same_request_produces_the_same_ops_every_time() {
            let f = file();
            let t = targets(&[("RV1", 1.0, 2.0), ("RV2", 3.0, 4.0), ("J2", 5.0, 6.0)]);
            let first = format!("{:?}", ops_for_targets(&f, &t));
            for _ in 0..8 {
                assert_eq!(format!("{:?}", ops_for_targets(&f, &t)), first);
            }
        }
    }
}

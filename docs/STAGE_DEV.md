# Writing Pipeline Stages

How to add a new stage to the `legion-of-bom-core` pipeline.

## The trait

```rust
use legion_of_bom_core::{Stage, StageOutcome, StageError, CircuitSource};

pub struct MyStage;

impl Stage for MyStage {
    fn name(&self) -> &str {
        "my_stage"
    }

    fn run(&self, circuit: &dyn CircuitSource) -> Result<StageOutcome, StageError> {
        // Stage logic here
        Ok(StageOutcome::passed("my_stage"))
    }
}
```

## Return discipline

| Situation | Return |
|---|---|
| Stage ran, circuit is acceptable | `Ok(StageOutcome::passed(...))` |
| Stage ran, found problems | `Ok(StageOutcome::failed(...))` or `passed(...).with(Finding::error(...))` |
| Could not run (missing tool, bad input) | `Err(StageError::ToolNotFound(...))` |
| External tool exited non-zero | `Err(StageError::ToolFailed { tool, code, stderr })` |
| I/O error | `Err(StageError::Io(...))` |

**Never panic on missing tools.** The `lob doctor` command exists specifically so
users can discover missing tools before running the pipeline.

## Accessing circuit data

```rust
fn run(&self, circuit: &dyn CircuitSource) -> Result<StageOutcome, StageError> {
    for part in circuit.parts() {
        println!("{}: {} = {}", part.refdes, part.mpn, part.value);
    }
    for net in circuit.nets() {
        println!("{}: {} pins", net.name, net.pins.len());
    }
    // ...
}
```

The `CircuitSource` trait is deliberately thin:
- `parts()` → component list with refdes, MPN, value, footprint
- `nets()` → netlist with connected pins
- No SKiDL-specific types — a future native DSL implements the same trait

## Adding findings

```rust
let mut outcome = StageOutcome::passed("my_stage");

if some_condition {
    outcome = outcome.with(Finding::warning("something odd"));
}
if bad_condition {
    outcome = outcome.with(Finding::error("something broken"));
}

Ok(outcome)
```

An error finding automatically flips `passed` to `false`.

## Wiring into the CLI

Add your stage to `crates/cli/src/main.rs` in the `run()` function:

```rust
// After existing stages...
report.push(my_stage.run(&model)?);
```

Or compose stages dynamically:

```rust
let stages: Vec<Box<dyn Stage>> = vec![
    Box::new(SkidlStage::new()),
    Box::new(ParseStage::new()),
    Box::new(MyStage),
];

let mut report = PipelineReport::new();
for stage in &stages {
    report.push(stage.run(&circuit)?);
}
```

## Testing

Unit tests live in the same file as the stage (or in `tests/` for integration).
Use the test helpers in `stage.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Circuit;

    #[test]
    fn my_stage_passes_on_empty_circuit() {
        let circuit = Circuit::new("test");
        let stage = MyStage;
        let outcome = stage.run(&circuit).unwrap();
        assert!(outcome.passed);
    }
}
```

## External tools

If your stage shells out, use the `Tool`/`ToolStatus` machinery in
`legion-of-bom-core::tools`:

```rust
use legion_of_bom_core::tools::{find_on_path, Tool};

let ngspice = find_on_path("ngspice")
    .ok_or_else(|| StageError::ToolNotFound("ngspice".into()))?;
```

This ensures `lob doctor` can probe your dependency and report it to the user.

## Conventions

- Stage names are lowercase snake_case (`"simulate"`, `"verify"`, `"bom"`)
- Findings messages are plain English, no jargon
- Info findings are for progress/telemetry ("netlist has 12 parts")
- Warning findings are for issues that don't block ("no footprint on C1")
- Error findings are for blockers ("floating net VCC")

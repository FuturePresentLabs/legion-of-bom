//! Fine-pitch probe (legion-of-bom-y17.1): a real LQFP-48 (KiCad's
//! `Package_QFP:LQFP-48_7x7mm_P0.5mm`) with a 0603 resistor on each of its
//! first `N` pins, placed, routed and DRC'd through the shipping board
//! pipeline — the router's own clearance oracle is not the judge here, KiCad
//! is.
//!
//! Usage: `cargo run --release -p legion-of-bom-core --example fine_pitch_drc_probe [N]`

use legion_of_bom_core::{
    free_outline_template, run_drc, run_layout_loop, skidl::kicad_footprint_dir, BoardOptions,
    Circuit, LayoutLoop, Net, Part, PinRef,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let n: usize = std::env::args()
        .nth(1)
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(24);

    let mut parts =
        vec![Part::new("U1", "LQFP48").with_footprint("Package_QFP:LQFP-48_7x7mm_P0.5mm")];
    let mut nets = Vec::new();
    for i in 1..=n {
        let r = format!("R{i}");
        parts.push(Part::new(r.as_str(), "10k").with_footprint("Resistor_SMD:R_0603_1608Metric"));
        nets.push(Net::new(
            format!("P{i}"),
            vec![
                PinRef::new("U1", i.to_string()),
                PinRef::new(r.as_str(), "1"),
            ],
        ));
    }
    nets.push(Net::new(
        "GND",
        (1..=n)
            .map(|i| PinRef::new(format!("R{i}").as_str(), "2"))
            .collect(),
    ));
    let circuit = Circuit {
        name: "fine-pitch-probe".into(),
        parts,
        nets,
    };

    let footprint_dir = kicad_footprint_dir().ok_or("no KiCad footprint library found")?;
    let started = std::time::Instant::now();
    // Exactly what `lob board` does for a board with no panel.
    let mut options = BoardOptions::new(footprint_dir);
    let template = free_outline_template(&circuit, &mut options)?;
    println!(
        "outline: {:.0} x {:.0} mm",
        template.width_mm, template.height_mm
    );
    let report = run_layout_loop(&circuit, options, template, &LayoutLoop::default())?;
    println!(
        "laid out {n} pins in {:.1}s",
        started.elapsed().as_secs_f64()
    );
    println!("unrouted: {:?}", report.unresolved);

    let pcb = std::env::temp_dir().join("fine_pitch_probe.kicad_pcb");
    std::fs::write(&pcb, &report.board)?;
    let kicad_cli = legion_of_bom_core::kicad_cli_path().ok_or("no kicad-cli found")?;
    let report = run_drc(&pcb, &kicad_cli)?;
    println!(
        "DRC: {} violations, {} unconnected ({})",
        report.violations.len(),
        report.unconnected_items.len(),
        pcb.display()
    );
    for v in &report.violations {
        println!("  [{}] {}: {}", v.severity, v.kind, v.description);
    }
    Ok(())
}

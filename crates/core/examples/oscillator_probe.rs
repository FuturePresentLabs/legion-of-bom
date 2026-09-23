//! Live end-to-end check for the Pierce oscillator generator: a real
//! decide() call, then the rendered SKiDL script written to disk.
//! `cargo run -p legion-of-bom-core --example oscillator_probe -- <brief> <out.py>`

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut a = std::env::args().skip(1);
    let brief = a
        .next()
        .unwrap_or_else(|| "a real-time clock reference, low power".to_string());
    let out = a.next().unwrap_or_else(|| "oscillator.py".to_string());

    let client = ooda::CapturingClient::new(
        ooda::HttpClient::from_env()
            .map_err(|e| format!("{e} (need OODA_API_KEY -- see .env.example)"))?,
        ooda::Capture::for_current_binary()?,
    );
    let mut trace = ooda::Trace::new();
    let osc = legion_of_bom_core::generate_pierce_oscillator(&client, &mut trace, &brief)?;

    println!("brief: {brief}");
    println!(
        "chosen crystal: {} ({}, real CL={:.1}pF)",
        osc.crystal.key, osc.crystal.mpn, osc.crystal.cl_pf
    );
    println!("computed load cap: {:.1}pF", osc.load_cap_pf);

    let py = legion_of_bom_core::render_pierce_oscillator_skidl(&osc);
    std::fs::write(&out, &py)?;
    println!("wrote {out}");
    Ok(())
}

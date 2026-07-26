fn main() {
    for kw in [
        "toggle switch SPDT ON-OFF-ON",
        "sub miniature toggle switch 3 position",
        "MTS-103",
        "SPDT ON-OFF-ON PC toggle",
    ] {
        println!("\n--- {kw:?}");
        for c in legion_of_bom_core::lcsc_search_parts(kw, 8) {
            println!(
                "  {:<26} {:<12} {:<14} stock {:>7}  ${:.3}",
                c.mpn.chars().take(25).collect::<String>(),
                c.lcsc_code.clone().unwrap_or_default(),
                c.package
                    .clone()
                    .unwrap_or_default()
                    .chars()
                    .take(13)
                    .collect::<String>(),
                c.stock.map(|s| s.to_string()).unwrap_or("?".into()),
                c.unit_price.unwrap_or(0.0)
            );
        }
    }
}

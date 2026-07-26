//! Probe the Thonk product lookup for the keywords the Visual BOM sends.
//! `cargo run -p legion-of-bom-core --example thonk_probe`
fn main() {
    for q in [
        "thonkiconn 3.5mm jack sockets",
        "alpha 9mm pots vertical",
        "jack nuts and washers",
        "eurorack power header shrouded",
        "LM13700",
    ] {
        let hits = legion_of_bom_core::thonk_search(q, 3);
        println!("\n{q:?} -> {} hit(s)", hits.len());
        for h in &hits {
            println!(
                "   {:52} {}",
                &h.title[..h.title.len().min(52)],
                h.image_url.as_deref().unwrap_or("(no image)")
            );
        }
    }
}

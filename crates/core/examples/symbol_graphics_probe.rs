//! Ad hoc: does `read_symbol_graphics` actually resolve real shapes for a
//! given lib:part, against the real installed KiCad symbol library?
//! `cargo run -p legion-of-bom-core --example symbol_graphics_probe -- <lib> <part>`

fn main() {
    let mut a = std::env::args().skip(1);
    let lib = a.next().expect("usage: <lib> <part>");
    let part = a.next().expect("usage: <lib> <part>");
    let dir = std::path::Path::new("/Applications/KiCad/KiCad.app/Contents/SharedSupport/symbols");
    match legion_of_bom_core::symbols::read_symbol_graphics(dir, &lib, &part) {
        Some(g) => {
            println!("resolved {lib}:{part}");
            println!("  shapes: {}", g.shapes.len());
            for s in &g.shapes {
                println!("    {s:?}");
            }
            println!("  pins: {}", g.pins.len());
        }
        None => println!("FAILED to resolve {lib}:{part}"),
    }
}

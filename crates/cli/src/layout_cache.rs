use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use legion_of_bom_core::route::RoutingReport;
use legion_of_bom_core::{BoardOptions, CircuitSource, LayoutCache, LayoutLoop, SeededPlacer};
use serde_json::{json, Value};

const ALGORITHM: &str = "lob-cli-layout-v1";

pub struct CachedLayout {
    pub board: String,
    pub conflicts: Vec<String>,
    pub collisions: Vec<String>,
    pub not_placed: Vec<String>,
    pub routing: Option<RoutingReport>,
}

pub fn cache() -> LayoutCache {
    LayoutCache::new(PathBuf::from("out").join(".layout-cache"))
}

pub fn key(
    model: &dyn CircuitSource,
    options: &BoardOptions,
    template: &SeededPlacer,
    cfg: &LayoutLoop,
) -> Result<String> {
    let parts: Vec<_> = model
        .parts()
        .iter()
        .map(|part| {
            json!({
                "refdes": part.refdes.0,
                "value": part.value,
                "footprint": part.footprint,
                "library_part": part.library_part,
                "mpn": part.mpn,
                "sim_excluded": part.sim_excluded,
                "side": format!("{:?}", part.side),
                "sim": part.sim.as_ref().map(|sim| json!({
                    "device": sim.device,
                    "name": sim.name,
                    "library": sim.library,
                    "pins": sim.pins,
                })),
            })
        })
        .collect();
    let nets: Vec<_> = model
        .nets()
        .iter()
        .map(|net| {
            json!({
                "name": net.name,
                "class": net.net_class,
                "pins": net.pins.iter().map(|pin| json!({
                    "refdes": pin.refdes.0,
                    "pin": pin.pin,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut anchors: Vec<_> = template.anchors.iter().collect();
    anchors.sort_by_key(|(name, _)| *name);
    let mut nudges: Vec<_> = template.nudges.iter().collect();
    nudges.sort_by_key(|(name, _)| *name);
    let route = &options.route_options;
    let inputs = json!({
        "circuit": {"name": model.name(), "parts": parts, "nets": nets},
        "footprints": footprint_inputs(model, &options.footprint_dir)?,
        "template": {
            "width_mm": template.width_mm,
            "height_mm": template.height_mm,
            "origin_mm": template.origin_mm,
            "anchors": anchors,
            "nudges": nudges,
            "side_policy": format!("{:?}", template.side_policy),
        },
        "layout": {
            "mode": cfg.mode.as_str(),
            "max_iters": cfg.max_iters,
            "drc_every_iter": cfg.drc_every_iter,
            "kicad_cli": cfg.kicad_cli,
        },
        "board": {
            "router_enabled": options.router.is_some(),
            "ground_net": options.ground_net,
            "outline_margin_mm": options.outline_margin_mm,
            "fixed_outline": options.fixed_outline,
            "silk_values": format!("{:?}", options.silk_values),
            "title": options.title,
            "legend": {
                "brand": options.legend.brand,
                "rev": options.legend.rev,
                "note": options.legend.note,
            },
            "logo": options.logo.as_ref().map(|logo| &logo.subpaths),
        },
        "route": {
            "signal_width_mm": route.signal_width_mm,
            "via_size_mm": route.via_size_mm,
            "via_drill_mm": route.via_drill_mm,
            "clearance_mm": route.clearance_mm,
            "edge_clearance_mm": route.edge_clearance_mm,
            "grid_mm": route.grid_mm,
            "via_cost_mm": route.via_cost_mm,
            "back_penalty_mm": route.back_penalty_mm,
            "bounds": route.bounds,
            "front": route.front,
            "back": route.back,
            "max_expansions": route.max_expansions,
            "max_wall_time_ms": route.max_wall_time_ms,
            // Reporting does not change the routed artifact.
        },
    });
    LayoutCache::key(ALGORITHM, &inputs).context("computing layout cache key")
}

pub fn read(cache: &LayoutCache, key: &str) -> Result<Option<CachedLayout>> {
    let Some(value) = cache.get::<Value>(key)? else {
        return Ok(None);
    };
    let Some(object) = value.as_object() else {
        return Ok(None);
    };
    let Some(board) = object.get("board").and_then(Value::as_str) else {
        return Ok(None);
    };
    let strings = |field: &str| -> Option<Vec<String>> {
        object
            .get(field)?
            .as_array()?
            .iter()
            .map(|v| v.as_str().map(str::to_owned))
            .collect()
    };
    let routing = match object.get("routing") {
        Some(Value::Null) | None => None,
        Some(value) => match serde_json::from_value(value.clone()) {
            Ok(report) => Some(report),
            Err(_) => return Ok(None),
        },
    };
    Ok(Some(CachedLayout {
        board: board.to_owned(),
        conflicts: match strings("conflicts") {
            Some(values) => values,
            None => return Ok(None),
        },
        collisions: match strings("collisions") {
            Some(values) => values,
            None => return Ok(None),
        },
        not_placed: match strings("not_placed") {
            Some(values) => values,
            None => return Ok(None),
        },
        routing,
    }))
}

pub fn write(cache: &LayoutCache, key: &str, layout: &CachedLayout) -> Result<()> {
    cache.put_success(
        key,
        &json!({
            "board": layout.board,
            "conflicts": layout.conflicts,
            "collisions": layout.collisions,
            "not_placed": layout.not_placed,
            "routing": layout.routing,
        }),
    )?;
    Ok(())
}

fn footprint_inputs(model: &dyn CircuitSource, root: &Path) -> Result<Vec<Value>> {
    let house = legion_of_bom_core::parts::house_footprint_dir();
    let mut ids: Vec<_> = model
        .parts()
        .iter()
        .filter_map(|part| part.footprint.as_deref())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids.into_iter()
        .map(|id| {
            let Some((lib, name)) = id.split_once(':') else {
                return Ok(json!({"id": id, "invalid": true}));
            };
            let relative = PathBuf::from(format!("{lib}.pretty")).join(format!("{name}.kicad_mod"));
            let candidates = house
                .iter()
                .map(|dir| dir.join(&relative))
                .chain(std::iter::once(root.join(&relative)));
            for path in candidates {
                if let Ok(bytes) = std::fs::read(&path) {
                    return Ok(json!({"id": id, "bytes": bytes}));
                }
            }
            // Synthesized/vendored LobModule footprints are versioned with the
            // binary and therefore covered by ALGORITHM.
            Ok(json!({"id": id, "builtin_or_missing": true}))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use legion_of_bom_core::{Circuit, Part};

    fn fixture() -> (Circuit, BoardOptions, SeededPlacer, LayoutLoop) {
        let mut circuit = Circuit::new("cached");
        circuit.parts.push(Part::new("R1", "10k"));
        (
            circuit,
            BoardOptions::new(PathBuf::from("/nonexistent-footprints")),
            SeededPlacer {
                width_mm: 20.0,
                height_mm: 30.0,
                origin_mm: (1.0, 2.0),
                anchors: Default::default(),
                nudges: Default::default(),
                side_policy: legion_of_bom_core::PlacementSidePolicy::FrontOnly,
            },
            LayoutLoop::default(),
        )
    }

    fn test_cache(name: &str) -> (LayoutCache, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "lob-cli-layout-cache-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        (LayoutCache::new(&root), root)
    }

    #[test]
    fn concrete_key_hits_and_behavior_changes_miss() {
        let (mut circuit, mut options, template, cfg) = fixture();
        let original = key(&circuit, &options, &template, &cfg).unwrap();
        let (cache, root) = test_cache("key");
        let artifact = CachedLayout {
            board: "(kicad_pcb exact)".into(),
            conflicts: vec![],
            collisions: vec![],
            not_placed: vec!["J1".into()],
            routing: None,
        };
        write(&cache, &original, &artifact).unwrap();
        assert_eq!(
            read(&cache, &original).unwrap().unwrap().board,
            artifact.board
        );

        circuit.parts[0].value = "11k".into();
        let changed_circuit = key(&circuit, &options, &template, &cfg).unwrap();
        assert_ne!(original, changed_circuit);
        assert!(read(&cache, &changed_circuit).unwrap().is_none());

        options.route_options.grid_mm = 0.125;
        let changed_route = key(&circuit, &options, &template, &cfg).unwrap();
        assert_ne!(changed_circuit, changed_route);
        assert!(read(&cache, &changed_route).unwrap().is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn corrupt_or_incomplete_entry_is_a_miss() {
        let (circuit, options, template, cfg) = fixture();
        let cache_key = key(&circuit, &options, &template, &cfg).unwrap();
        let (cache, root) = test_cache("corrupt");
        write(
            &cache,
            &cache_key,
            &CachedLayout {
                board: "board".into(),
                conflicts: vec![],
                collisions: vec![],
                not_placed: vec![],
                routing: None,
            },
        )
        .unwrap();
        std::fs::write(root.join(format!("{cache_key}.json")), b"{broken").unwrap();
        assert!(read(&cache, &cache_key).unwrap().is_none());

        let incomplete_key = LayoutCache::key(ALGORITHM, &"incomplete").unwrap();
        cache
            .put_success(&incomplete_key, &json!({"board": "lyingly clean"}))
            .unwrap();
        assert!(read(&cache, &incomplete_key).unwrap().is_none());
        let _ = std::fs::remove_dir_all(root);
    }
}

//! A placed board as one named, colored GLB scene — never a rendered image.
//! [`crate::guide::PlacedPart`] already carries real per-part board-space
//! placement (`guide::parse_board`, re-reading a generated `.kicad_pcb`);
//! this module turns that into a glTF 2.0 scene an external viewer (a
//! `<model-viewer>`-style tool, or Blender) can highlight/explode by node
//! name — no lighting, camera, or baking is this module's job.
//!
//! Mirrors `pedalkernel-pro`'s `autolayout` crate's field shapes
//! (`MeshMaterial{name, base_color, metallic, roughness}`) so a future
//! cross-repo consumer maps cleanly. That crate already proved out the same
//! `assets/meshes/jolin/glb/*.glb` mesh library this module reads from — it
//! just never wrote a combined scene back out, only ever consumed meshes to
//! bake a render.
//!
//! A part whose footprint has no [`MESH_CATALOG`] entry — or when the mesh
//! directory itself can't be found — still gets a real, named, colored
//! node: a plain box sized from its own courtyard bounding box. One missing
//! mesh never fails the whole export.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use gltf_json::validation::Checked;
use gltf_json::{self as json};

use crate::guide::PlacedPart;
use crate::tools::find_upward;

/// 1 glTF unit = 1 meter (the spec's convention); board/part geometry here
/// is authored in millimeters.
const MM_TO_M: f32 = 0.001;
/// Standard PCB thickness — used for the board slab and to lift front-side
/// parts above it.
const BOARD_THICKNESS_MM: f64 = 1.6;
/// A generic component height for a part with no real mesh or height data.
const FALLBACK_BOX_HEIGHT_MM: f64 = 3.0;

/// A part's appearance: a real mesh's material, or a fallback box's color.
/// Field shape matches autolayout's own `MeshMaterial`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeshMaterial {
    pub name: &'static str,
    pub base_color: [f32; 4],
    pub metallic: f32,
    pub roughness: f32,
}

/// One footprint → real mesh mapping. `footprint_suffix` matches a placed
/// part's `footprint` field (stored `Library:Name` in a `.kicad_pcb`) by
/// suffix, so the match is independent of which `.pretty` library it came
/// from. Not every entry is an exact model match — see the inline notes;
/// an approximate stand-in mesh is still far more useful in a viewer than a
/// plain box, as long as it's honestly documented as approximate.
struct MeshEntry {
    footprint_suffix: &'static str,
    /// File name under `assets/meshes/jolin/glb/`.
    mesh_file: &'static str,
    material: MeshMaterial,
}

const MESH_CATALOG: &[MeshEntry] = &[
    // Exact match: PJ301M-12 is the vendored footprint name for a PJ301M-12
    // 3.5mm mono jack; the mesh is its close sibling PJ301BM (same family,
    // mounting-compatible).
    MeshEntry {
        footprint_suffix: "PJ301M-12",
        mesh_file: "jack_socket_PJ301BM_3.5mm.glb",
        material: MeshMaterial {
            name: "jack",
            base_color: [0.15, 0.15, 0.16, 1.0],
            metallic: 0.6,
            roughness: 0.4,
        },
    },
    // Exact match: RD901F is Alpha's real part number for a 9mm pot.
    MeshEntry {
        footprint_suffix: "POT-9MM-ALPHA",
        mesh_file: "pot_RD901F_6.35mm_shaft_alpha_9mm_vertical.glb",
        material: MeshMaterial {
            name: "pot",
            base_color: [0.2, 0.2, 0.22, 1.0],
            metallic: 0.3,
            roughness: 0.6,
        },
    },
    MeshEntry {
        footprint_suffix: "POT-9MM-KNURL",
        mesh_file: "pot_RD901F_6.35mm_shaft_alpha_9mm_vertical.glb",
        material: MeshMaterial {
            name: "pot",
            base_color: [0.2, 0.2, 0.22, 1.0],
            metallic: 0.3,
            roughness: 0.6,
        },
    },
    // Exact match: the installed KiCad library's own RD901F footprint (used
    // when a board pulls the pot from KiCad's standard library rather than
    // the vendored 4ms .pretty), not just the 4ms-vendored name above.
    MeshEntry {
        footprint_suffix: "Potentiometer_Alpha_RD901F-40-00D_Single_Vertical",
        mesh_file: "pot_RD901F_6.35mm_shaft_alpha_9mm_vertical.glb",
        material: MeshMaterial {
            name: "pot",
            base_color: [0.2, 0.2, 0.22, 1.0],
            metallic: 0.3,
            roughness: 0.6,
        },
    },
    // Exact match on shape/size.
    MeshEntry {
        footprint_suffix: "LED-3MM-SQUARE-ANODE",
        mesh_file: "LED_square_3mm_red.glb",
        material: MeshMaterial {
            name: "led",
            base_color: [0.9, 0.15, 0.15, 1.0],
            metallic: 0.0,
            roughness: 0.5,
        },
    },
    // Approximate: SS12D00 vs. the footprint's SS22D06 — different pole
    // count, same slide-switch mesh family. Close enough for a labeled,
    // colored viewer aid; not a manufacturing artifact.
    MeshEntry {
        footprint_suffix: "Slide_Switch_SS22D06-G6-H_Runrun",
        mesh_file: "switch_slide_onoff_SS12D00.glb",
        material: MeshMaterial {
            name: "switch",
            base_color: [0.1, 0.1, 0.1, 1.0],
            metallic: 0.2,
            roughness: 0.7,
        },
    },
    // Approximate: the mesh is a Bourns slide fader, the footprint is
    // Alpha's RA2045F-20 slide pot — no exact mesh for that part exists yet,
    // but both are slide-travel controls of a similar size.
    MeshEntry {
        footprint_suffix: "POT-SLIDER-LED-ALPHA-RA2045F-20",
        mesh_file: "fader_Bourns_45mm.glb",
        material: MeshMaterial {
            name: "fader",
            base_color: [0.15, 0.15, 0.15, 1.0],
            metallic: 0.2,
            roughness: 0.6,
        },
    },
    // RGB_ROTARY_ENCODER deliberately has no entry: it's an encoder+knob+RGB
    // LED assembly, and the only knob mesh available (a plain turn-knob)
    // would misrepresent it more than an honest labeled box does.
];

const FALLBACK_MATERIAL: MeshMaterial = MeshMaterial {
    name: "unmapped",
    base_color: [0.55, 0.55, 0.6, 1.0],
    metallic: 0.1,
    roughness: 0.8,
};

const BOARD_MATERIAL: MeshMaterial = MeshMaterial {
    name: "board",
    base_color: [0.05, 0.35, 0.15, 1.0],
    metallic: 0.0,
    roughness: 0.7,
};

/// Errors exporting a GLB scene.
#[derive(Debug, thiserror::Error)]
pub enum GltfError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("reading mesh {path}: {source}")]
    SourceMesh {
        path: PathBuf,
        #[source]
        source: gltf::Error,
    },
    #[error("mesh {path} has no mesh primitives")]
    EmptySourceMesh { path: PathBuf },
    #[error("encoding glb: {0}")]
    Encode(#[source] gltf::Error),
    #[error("encoding scene json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Raw triangle-mesh geometry — positions + a triangle-list index buffer,
/// both already flattened to plain f32/u32 so the exporter never needs to
/// know whether they came from a real jolin mesh or a fallback box.
struct Geometry {
    positions: Vec<[f32; 3]>,
    indices: Vec<u32>,
}

impl Geometry {
    /// A simple 8-vertex box, `w`×`d` mm in the board plane, `h` mm tall,
    /// sitting on the ground plane (`y = 0..h`) — the fallback shape for a
    /// part with no mapped mesh, and the board slab itself.
    fn box_mm(w_mm: f64, d_mm: f64, h_mm: f64) -> Geometry {
        let (hw, hd) = (
            (w_mm.max(0.1) as f32) * MM_TO_M / 2.0,
            (d_mm.max(0.1) as f32) * MM_TO_M / 2.0,
        );
        let h = (h_mm.max(0.1) as f32) * MM_TO_M;
        Geometry {
            positions: vec![
                [-hw, 0.0, -hd],
                [hw, 0.0, -hd],
                [hw, 0.0, hd],
                [-hw, 0.0, hd],
                [-hw, h, -hd],
                [hw, h, -hd],
                [hw, h, hd],
                [-hw, h, hd],
            ],
            indices: vec![
                0, 1, 2, 0, 2, 3, // bottom
                4, 6, 5, 4, 7, 6, // top
                0, 4, 5, 0, 5, 1, // -z side
                1, 5, 6, 1, 6, 2, // +x side
                2, 6, 7, 2, 7, 3, // +z side
                3, 7, 4, 3, 4, 0, // -x side
            ],
        }
    }

    /// Reads the first primitive of the first mesh out of a real `.glb`
    /// file's bytes. These are single-part component meshes (a jack, a
    /// knob) with one mesh each, so "first" is the real mesh, not a
    /// simplification.
    fn from_glb(bytes: &[u8], path: &Path) -> Result<Geometry, GltfError> {
        let (document, buffers, _images) =
            gltf::import_slice(bytes).map_err(|source| GltfError::SourceMesh {
                path: path.to_path_buf(),
                source,
            })?;
        let mesh = document
            .meshes()
            .next()
            .ok_or_else(|| GltfError::EmptySourceMesh {
                path: path.to_path_buf(),
            })?;
        let primitive = mesh
            .primitives()
            .next()
            .ok_or_else(|| GltfError::EmptySourceMesh {
                path: path.to_path_buf(),
            })?;
        let reader = primitive.reader(|b| buffers.get(b.index()).map(|d| d.0.as_slice()));
        let positions: Vec<[f32; 3]> = reader
            .read_positions()
            .ok_or_else(|| GltfError::EmptySourceMesh {
                path: path.to_path_buf(),
            })?
            .collect();
        let indices: Vec<u32> = match reader.read_indices() {
            Some(idx) => idx.into_u32().collect(),
            None => (0..positions.len() as u32).collect(),
        };
        Ok(Geometry { positions, indices })
    }
}

/// The fields one accessor + its buffer view need — bundled so
/// [`BufferBuilder::push_accessor`] takes one argument, not seven.
struct AccessorSpec<'a> {
    data: &'a [u8],
    type_: json::accessor::Type,
    component_type: json::accessor::ComponentType,
    count: usize,
    min: Option<serde_json::Value>,
    max: Option<serde_json::Value>,
    target: Option<json::buffer::Target>,
}

/// Accumulates every mesh's geometry into one combined binary buffer plus
/// the accessors/buffer-views that describe it — the low-level half of
/// assembling a GLB; [`push_part_node`] is the high-level half.
#[derive(Default)]
struct BufferBuilder {
    bytes: Vec<u8>,
    buffer_views: Vec<json::buffer::View>,
    accessors: Vec<json::Accessor>,
}

impl BufferBuilder {
    fn push_positions(&mut self, positions: &[[f32; 3]]) -> json::Index<json::Accessor> {
        let mut min = [f32::INFINITY; 3];
        let mut max = [f32::NEG_INFINITY; 3];
        for p in positions {
            for i in 0..3 {
                min[i] = min[i].min(p[i]);
                max[i] = max[i].max(p[i]);
            }
        }
        let mut data = Vec::with_capacity(positions.len() * 12);
        for p in positions {
            for c in p {
                data.extend_from_slice(&c.to_le_bytes());
            }
        }
        self.push_accessor(AccessorSpec {
            data: &data,
            type_: json::accessor::Type::Vec3,
            component_type: json::accessor::ComponentType::F32,
            count: positions.len(),
            min: Some(serde_json::json!(min)),
            max: Some(serde_json::json!(max)),
            target: Some(json::buffer::Target::ArrayBuffer),
        })
    }

    fn push_indices(&mut self, indices: &[u32]) -> json::Index<json::Accessor> {
        let mut data = Vec::with_capacity(indices.len() * 4);
        for i in indices {
            data.extend_from_slice(&i.to_le_bytes());
        }
        self.push_accessor(AccessorSpec {
            data: &data,
            type_: json::accessor::Type::Scalar,
            component_type: json::accessor::ComponentType::U32,
            count: indices.len(),
            min: None,
            max: None,
            target: Some(json::buffer::Target::ElementArrayBuffer),
        })
    }

    fn push_accessor(&mut self, spec: AccessorSpec) -> json::Index<json::Accessor> {
        let AccessorSpec {
            data,
            type_,
            component_type,
            count,
            min,
            max,
            target,
        } = spec;
        // glTF requires accessor data aligned to its component size; 4 bytes
        // covers both f32 and u32, the only component types this exporter emits.
        while !self.bytes.len().is_multiple_of(4) {
            self.bytes.push(0);
        }
        let byte_offset = self.bytes.len();
        self.bytes.extend_from_slice(data);

        let view = json::Index::push(
            &mut self.buffer_views,
            json::buffer::View {
                buffer: json::Index::new(0),
                byte_length: data.len().into(),
                byte_offset: Some(byte_offset.into()),
                byte_stride: None,
                target: target.map(Checked::Valid),
                name: None,
                extensions: None,
                extras: Default::default(),
            },
        );
        json::Index::push(
            &mut self.accessors,
            json::Accessor {
                buffer_view: Some(view),
                byte_offset: None,
                count: count.into(),
                component_type: Checked::Valid(json::accessor::GenericComponentType(
                    component_type,
                )),
                type_: Checked::Valid(type_),
                min,
                max,
                normalized: false,
                sparse: None,
                name: None,
                extensions: None,
                extras: Default::default(),
            },
        )
    }
}

/// Looks up (or registers) the glTF material for a [`MeshMaterial`], so
/// every part sharing one (every pot, every jack, …) shares one material
/// rather than getting a duplicate per node.
fn material_index(
    root: &mut json::Root,
    seen: &mut BTreeMap<&'static str, json::Index<json::Material>>,
    m: MeshMaterial,
) -> json::Index<json::Material> {
    if let Some(&idx) = seen.get(m.name) {
        return idx;
    }
    let idx = json::Index::push(
        &mut root.materials,
        json::Material {
            name: Some(m.name.to_string()),
            pbr_metallic_roughness: json::material::PbrMetallicRoughness {
                base_color_factor: json::material::PbrBaseColorFactor(m.base_color),
                metallic_factor: json::material::StrengthFactor(m.metallic),
                roughness_factor: json::material::StrengthFactor(m.roughness),
                base_color_texture: None,
                metallic_roughness_texture: None,
                extensions: None,
                extras: Default::default(),
            },
            alpha_cutoff: None,
            alpha_mode: Checked::Valid(json::material::AlphaMode::Opaque),
            double_sided: false,
            normal_texture: None,
            occlusion_texture: None,
            emissive_texture: None,
            emissive_factor: json::material::EmissiveFactor([0.0, 0.0, 0.0]),
            extensions: None,
            extras: Default::default(),
        },
    );
    seen.insert(m.name, idx);
    idx
}

/// A node's placement in the output scene: `(cx_mm, cy_mm, y_mm, rotation_deg)`
/// — `cx_mm`/`cy_mm` are the part's real KiCad board-space coordinates
/// (mapped to the scene's X/Z ground plane), `y_mm` is height above the
/// scene origin (the board's top surface), `rotation_deg` the part's real
/// footprint rotation, applied about the board's normal (scene Y).
type NodePlacement = (f64, f64, f64, f64);

fn y_rotation_quat(deg: f64) -> [f32; 4] {
    let half = (deg.to_radians() / 2.0) as f32;
    [0.0, half.sin(), 0.0, half.cos()]
}

/// Mirrors geometry through the ground plane (negates Y) and reverses
/// triangle winding to compensate (a mirror flips handedness, so an
/// un-reversed mirrored mesh would face inward-out). Every mesh here — a
/// real jolin part or a [`Geometry::box_mm`] fallback — is authored/built
/// assuming it sits on top of a surface, growing upward from local `y = 0`;
/// a back-side part needs the same shape hanging *below* its mounting
/// surface instead.
fn mirror_y(geo: &Geometry) -> Geometry {
    let positions = geo.positions.iter().map(|p| [p[0], -p[1], p[2]]).collect();
    let mut indices = geo.indices.clone();
    let (triangles, _remainder) = indices.as_chunks_mut::<3>();
    for tri in triangles {
        tri.swap(1, 2);
    }
    Geometry { positions, indices }
}

/// Adds one named, colored, placed node (its own mesh + material) to the
/// scene under construction. Returns the node's index to add to the scene's
/// root node list.
fn push_part_node(
    root: &mut json::Root,
    buf: &mut BufferBuilder,
    materials: &mut BTreeMap<&'static str, json::Index<json::Material>>,
    name: &str,
    geo: &Geometry,
    material: MeshMaterial,
    placement: NodePlacement,
) -> json::Index<json::Node> {
    let position_accessor = buf.push_positions(&geo.positions);
    let index_accessor = buf.push_indices(&geo.indices);
    let material_idx = material_index(root, materials, material);

    let mut attributes = BTreeMap::new();
    attributes.insert(
        Checked::Valid(json::mesh::Semantic::Positions),
        position_accessor,
    );

    let mesh_idx = json::Index::push(
        &mut root.meshes,
        json::Mesh {
            name: Some(name.to_string()),
            primitives: vec![json::mesh::Primitive {
                attributes,
                indices: Some(index_accessor),
                material: Some(material_idx),
                mode: Checked::Valid(json::mesh::Mode::Triangles),
                targets: None,
                extensions: None,
                extras: Default::default(),
            }],
            weights: None,
            extensions: None,
            extras: Default::default(),
        },
    );

    let (cx_mm, cy_mm, y_mm, rotation_deg) = placement;
    let translation = [
        (cx_mm as f32) * MM_TO_M,
        (y_mm as f32) * MM_TO_M,
        (cy_mm as f32) * MM_TO_M,
    ];

    json::Index::push(
        &mut root.nodes,
        json::Node {
            camera: None,
            children: None,
            extensions: None,
            extras: Default::default(),
            matrix: None,
            mesh: Some(mesh_idx),
            name: Some(name.to_string()),
            rotation: Some(json::scene::UnitQuaternion(y_rotation_quat(rotation_deg))),
            scale: None,
            translation: Some(translation),
            skin: None,
            weights: None,
        },
    )
}

/// Turns one placed, generated board into a single combined `.glb` scene:
/// one named node per part (name = refdes) plus one `board.top` slab node,
/// each positioned at its real board-space coordinates. `board_outline_mm`
/// is `(min_x, min_y, max_x, max_y)`, the same shape
/// [`crate::guide::board_outline`] already returns.
pub fn export_board_glb(
    board_name: &str,
    parts: &[PlacedPart],
    board_outline_mm: (f64, f64, f64, f64),
) -> Result<Vec<u8>, GltfError> {
    let mesh_dir = find_upward("assets/meshes/jolin/glb");

    let mut root = json::Root {
        asset: json::Asset {
            version: "2.0".to_string(),
            generator: Some("legion-of-bom".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut buf = BufferBuilder::default();
    let mut materials_seen = BTreeMap::new();
    let mut scene_nodes = Vec::with_capacity(parts.len() + 1);

    let (min_x, min_y, max_x, max_y) = board_outline_mm;
    let board_geo = Geometry::box_mm(max_x - min_x, max_y - min_y, BOARD_THICKNESS_MM);
    scene_nodes.push(push_part_node(
        &mut root,
        &mut buf,
        &mut materials_seen,
        "board.top",
        &board_geo,
        BOARD_MATERIAL,
        (
            (min_x + max_x) / 2.0,
            (min_y + max_y) / 2.0,
            -BOARD_THICKNESS_MM,
            0.0,
        ),
    ));

    for part in parts {
        let entry = MESH_CATALOG
            .iter()
            .find(|e| part.footprint.ends_with(e.footprint_suffix));
        let (geo, material) = match entry.zip(mesh_dir.as_deref()) {
            Some((entry, dir)) => {
                let path = dir.join(entry.mesh_file);
                let bytes = std::fs::read(&path)?;
                (Geometry::from_glb(&bytes, &path)?, entry.material)
            }
            None => {
                let (bx, by, ex, ey) = part.bbox;
                (
                    Geometry::box_mm(ex - bx, ey - by, FALLBACK_BOX_HEIGHT_MM),
                    FALLBACK_MATERIAL,
                )
            }
        };
        // Front parts sit on the board's top surface (world y = 0) and grow
        // upward, same as the geometry's own local convention. Back parts
        // hang from the board's bottom surface (world y = -thickness) and
        // need to grow downward instead — mirror the geometry rather than
        // just re-translating it, or it would grow the wrong way.
        let (geo, y_mm) = if part.back {
            (mirror_y(&geo), -BOARD_THICKNESS_MM)
        } else {
            (geo, 0.0)
        };
        scene_nodes.push(push_part_node(
            &mut root,
            &mut buf,
            &mut materials_seen,
            &part.refdes,
            &geo,
            material,
            (part.cx, part.cy, y_mm, part.rotation_deg),
        ));
    }

    let scene = json::Index::push(
        &mut root.scenes,
        json::Scene {
            extensions: None,
            extras: Default::default(),
            name: Some(board_name.to_string()),
            nodes: scene_nodes,
        },
    );
    root.scene = Some(scene);

    json::Index::push(
        &mut root.buffers,
        json::Buffer {
            byte_length: buf.bytes.len().into(),
            uri: None,
            name: None,
            extensions: None,
            extras: Default::default(),
        },
    );
    root.buffer_views = buf.buffer_views;
    root.accessors = buf.accessors;

    let json_bytes = serde_json::to_vec(&root)?;
    let glb = gltf::Glb {
        header: gltf::binary::Header {
            magic: *b"glTF",
            version: 2,
            length: 0, // recomputed by Glb::to_vec() from the real chunk sizes
        },
        json: std::borrow::Cow::Owned(json_bytes),
        bin: Some(std::borrow::Cow::Owned(buf.bytes)),
    };
    glb.to_vec().map_err(GltfError::Encode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guide::PlacedPart;

    fn part(refdes: &str, footprint: &str, cx: f64, cy: f64, back: bool) -> PlacedPart {
        PlacedPart {
            refdes: refdes.into(),
            value: String::new(),
            footprint: footprint.into(),
            cx,
            cy,
            rotation_deg: 0.0,
            bbox: (cx - 2.0, cy - 2.0, cx + 2.0, cy + 2.0),
            back,
            through_hole: true,
            pin1: None,
            polarity: None,
        }
    }

    /// A part with no MESH_CATALOG entry still produces a valid, named,
    /// colored node — the fallback-box path, no live mesh assets needed.
    #[test]
    fn unmapped_part_falls_back_to_a_named_colored_box() {
        let parts = vec![part("U1", "Package_SO:SOIC-8", 10.0, 10.0, false)];
        let bytes = export_board_glb("test-board", &parts, (0.0, 0.0, 20.0, 20.0)).expect("export");

        let (document, buffers, _images) = gltf::import_slice(&bytes).expect("re-parse own output");
        let names: Vec<&str> = document.nodes().filter_map(|n| n.name()).collect();
        assert!(names.contains(&"board.top"));
        assert!(names.contains(&"U1"));

        let u1 = document.nodes().find(|n| n.name() == Some("U1")).unwrap();
        let mesh = u1.mesh().expect("U1 has a mesh");
        let primitive = mesh.primitives().next().expect("mesh has a primitive");
        let material = primitive.material();
        assert_eq!(
            material.pbr_metallic_roughness().base_color_factor(),
            FALLBACK_MATERIAL.base_color
        );

        // Real, valid geometry — every buffer/accessor reference resolves.
        let reader = primitive.reader(|b| buffers.get(b.index()).map(|d| d.0.as_slice()));
        let positions: Vec<_> = reader.read_positions().expect("positions").collect();
        assert_eq!(positions.len(), 8); // the fallback box's 8 vertices
    }

    /// A front part sits flush on the board's top surface and grows
    /// upward; a back part hangs flush from the board's bottom surface and
    /// grows downward — never floating off the surface or sitting on the
    /// wrong side (the exact bug a live Blender import once caught: front
    /// parts floated `BOARD_THICKNESS_MM` above the surface, and back parts
    /// sat on the same side as front parts instead of underneath).
    #[test]
    fn front_and_back_parts_sit_flush_on_the_correct_face() {
        let parts = vec![
            part("U1", "Package_SO:SOIC-8", 10.0, 10.0, false), // front
            part("U2", "Package_SO:SOIC-8", 10.0, 10.0, true),  // back
        ];
        let bytes = export_board_glb("test-board", &parts, (0.0, 0.0, 20.0, 20.0)).expect("export");
        let (document, buffers, _images) = gltf::import_slice(&bytes).expect("re-parse own output");

        let world_y_range = |refdes: &str| -> (f32, f32) {
            let node = document.nodes().find(|n| n.name() == Some(refdes)).unwrap();
            let (translation, _rot, _scale) = node.transform().decomposed();
            let mesh = node.mesh().unwrap();
            let primitive = mesh.primitives().next().unwrap();
            let reader = primitive.reader(|b| buffers.get(b.index()).map(|d| d.0.as_slice()));
            let ys: Vec<f32> = reader
                .read_positions()
                .unwrap()
                .map(|p| p[1] + translation[1])
                .collect();
            (
                ys.iter().cloned().fold(f32::INFINITY, f32::min),
                ys.iter().cloned().fold(f32::NEG_INFINITY, f32::max),
            )
        };

        let board_top = 0.0f32; // world y=0 is the board's top surface, by construction.
        let board_bottom = -(BOARD_THICKNESS_MM as f32) * MM_TO_M;

        let (front_min, front_max) = world_y_range("U1");
        assert!(
            (front_min - board_top).abs() < 1e-6,
            "front part U1 should sit flush on the board surface, not float: min={front_min}"
        );
        assert!(front_max > front_min, "front part should extend upward");

        let (back_min, back_max) = world_y_range("U2");
        assert!(
            (back_max - board_bottom).abs() < 1e-6,
            "back part U2 should hang flush from the board's underside: max={back_max}"
        );
        assert!(back_min < back_max, "back part should extend downward");
    }

    /// A mapped footprint uses the real jolin mesh's geometry, not the
    /// fallback box — skipped if this checkout has no vendored mesh assets.
    #[test]
    fn mapped_part_uses_the_real_catalog_mesh() {
        let Some(dir) = find_upward("assets/meshes/jolin/glb") else {
            return;
        };
        if !dir.join("jack_socket_PJ301BM_3.5mm.glb").is_file() {
            return;
        }
        let parts = vec![part("J1", "Eurorack_4ms:PJ301M-12", 5.0, 5.0, false)];
        let bytes = export_board_glb("test-board", &parts, (0.0, 0.0, 20.0, 20.0)).expect("export");

        let (document, buffers, _images) = gltf::import_slice(&bytes).expect("re-parse own output");
        let j1 = document.nodes().find(|n| n.name() == Some("J1")).unwrap();
        let mesh = j1.mesh().unwrap();
        let primitive = mesh.primitives().next().unwrap();
        let reader = primitive.reader(|b| buffers.get(b.index()).map(|d| d.0.as_slice()));
        let positions: Vec<_> = reader.read_positions().unwrap().collect();
        // The real mesh has more geometry than an 8-vertex fallback box.
        assert!(positions.len() > 8, "expected real jack mesh geometry");
    }

    /// Two parts sharing a MeshMaterial share one glTF material, not one
    /// each — keeps the scene small and lets a viewer recolor "all pots"
    /// in one place.
    #[test]
    fn parts_sharing_a_material_share_one_gltf_material() {
        let parts = vec![
            part("R1", "Package_SO:SOIC-8", 5.0, 5.0, false),
            part("R2", "Package_SO:SOIC-8", 15.0, 5.0, false),
        ];
        let bytes = export_board_glb("test-board", &parts, (0.0, 0.0, 20.0, 20.0)).expect("export");
        let (document, _buffers, _images) =
            gltf::import_slice(&bytes).expect("re-parse own output");
        // board.top (its own material) + the shared fallback material.
        assert_eq!(document.materials().count(), 2);
    }
}

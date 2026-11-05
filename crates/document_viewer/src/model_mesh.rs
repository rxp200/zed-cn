use anyhow::{Context as _, Result, bail, ensure};
use std::collections::HashMap;

pub const MAX_TRIANGLES: usize = 2_000_000;
/// Hard cap on independently selectable bodies. A larger assembly keeps its
/// largest bodies and folds the rest into one summary part.
pub const MAX_PARTS: usize = 1024;
pub type Vertex = [f64; 3];

/// One independently selectable body of a model.
///
/// `STEP` assemblies provide names, colors and nesting from the product graph;
/// triangle soup formats such as `STL` get one part per connected component.
#[derive(Clone, Debug)]
pub struct ModelPart {
    pub name: String,
    pub color: Option<[u8; 3]>,
    pub triangle_count: usize,
    pub minimum: Vertex,
    pub maximum: Vertex,
    pub center: Vertex,
    /// Product-structure nesting level; 0 for flat triangle meshes.
    pub depth: u16,
}

impl ModelPart {
    fn new(name: String, color: Option<[u8; 3]>, triangle_count: usize, depth: u16) -> Self {
        Self {
            name,
            color,
            triangle_count,
            minimum: [f64::INFINITY; 3],
            maximum: [f64::NEG_INFINITY; 3],
            center: [0.0; 3],
            depth,
        }
    }
}

#[derive(Default)]
struct PartLayout {
    parts: Vec<ModelPart>,
    part_of_triangle: Vec<u32>,
}

pub struct ModelMesh {
    pub triangles: Vec<[Vertex; 3]>,
    pub minimum: Vertex,
    pub maximum: Vertex,
    pub area: f64,
    pub signed_volume: f64,
    pub degenerate_faces: usize,
    /// Length unit declared by a STEP file; `None` for raw triangle meshes.
    pub step_unit: Option<super::step_reader::StepUnit>,
    /// Number of solid bodies read from a STEP file (0 for raw meshes).
    pub step_solids: usize,
    /// STEP geometry is a tessellation of curved surfaces, so the exact-coordinate
    /// topology inspection would report tessellation seams as model defects.
    pub topology_reliable: bool,
    /// Independently selectable bodies, empty when the format has no part
    /// information or the mesh is a single connected body.
    pub parts: Vec<ModelPart>,
    /// Part index per triangle, parallel to `triangles` when `parts` is non-empty.
    pub part_of_triangle: Vec<u32>,
}

impl ModelMesh {
    pub fn parse(extension: &str, bytes: &[u8]) -> Result<Self> {
        log::info!(
            "[open-debug] ModelMesh::parse extension={extension} bytes={}",
            bytes.len()
        );
        let mut step_unit = None;
        let mut step_solids = 0;
        let mut topology_reliable = true;
        let (triangles, layout) = match extension.to_ascii_lowercase().as_str() {
            "stl" => {
                let triangles = parse_stl(bytes)?;
                let layout = split_stl_parts(&triangles);
                (triangles, layout)
            }
            "obj" => (
                parse_obj(std::str::from_utf8(bytes)?)?,
                PartLayout::default(),
            ),
            "ply" => (
                parse_ply(std::str::from_utf8(bytes)?)?,
                PartLayout::default(),
            ),
            "step" | "stp" => {
                let step = super::step_reader::parse(bytes)?;
                step_unit = Some(step.unit);
                step_solids = step.solids;
                topology_reliable = false;
                let layout = PartLayout::from_step_parts(step.parts, step.triangles.len());
                (step.triangles, layout)
            }
            _ => bail!("unsupported model format"),
        };
        ensure!(!triangles.is_empty(), "model has no triangle faces");
        let mut mesh = Self {
            triangles,
            minimum: [f64::INFINITY; 3],
            maximum: [f64::NEG_INFINITY; 3],
            area: 0.0,
            signed_volume: 0.0,
            degenerate_faces: 0,
            step_unit,
            step_solids,
            topology_reliable,
            parts: layout.parts,
            part_of_triangle: layout.part_of_triangle,
        };
        for triangle in &mesh.triangles {
            for vertex in triangle {
                ensure!(
                    vertex.iter().all(|value| value.is_finite()),
                    "non-finite model coordinate"
                );
                for axis in 0..3 {
                    mesh.minimum[axis] = mesh.minimum[axis].min(vertex[axis]);
                    mesh.maximum[axis] = mesh.maximum[axis].max(vertex[axis]);
                }
            }
            let normal = cross(sub(triangle[1], triangle[0]), sub(triangle[2], triangle[0]));
            let area = dot(normal, normal).sqrt() * 0.5;
            ensure!(area.is_finite(), "model coordinates exceed supported range");
            mesh.area += area;
            mesh.degenerate_faces += usize::from(area == 0.0);
        }
        // Translating the volume origin reduces cancellation for meshes far from the origin.
        let origin = mesh.minimum;
        mesh.signed_volume = mesh
            .triangles
            .iter()
            .map(|triangle| {
                dot(
                    sub(triangle[0], origin),
                    cross(sub(triangle[1], origin), sub(triangle[2], origin)),
                ) / 6.0
            })
            .sum();
        ensure!(
            mesh.area.is_finite() && mesh.signed_volume.is_finite(),
            "model measurements exceed supported range"
        );
        mesh.finish_parts();
        log::info!(
            "[open-debug] ModelMesh::parse done: {} triangles, {} degenerate",
            mesh.triangles.len(),
            mesh.degenerate_faces
        );
        Ok(mesh)
    }

    /// Resolves each part's bounding box from the triangles assigned to it.
    /// A mismatched layout is discarded rather than risking out-of-bounds ids.
    fn finish_parts(&mut self) {
        if self.parts.is_empty() {
            self.part_of_triangle.clear();
            return;
        }
        if self.part_of_triangle.len() != self.triangles.len() {
            self.parts.clear();
            self.part_of_triangle.clear();
            return;
        }
        let Self {
            triangles,
            parts,
            part_of_triangle,
            ..
        } = self;
        for part in parts.iter_mut() {
            part.minimum = [f64::INFINITY; 3];
            part.maximum = [f64::NEG_INFINITY; 3];
        }
        for (index, triangle) in triangles.iter().enumerate() {
            let Some(part) = parts.get_mut(part_of_triangle[index] as usize) else {
                continue;
            };
            for vertex in triangle {
                for axis in 0..3 {
                    part.minimum[axis] = part.minimum[axis].min(vertex[axis]);
                    part.maximum[axis] = part.maximum[axis].max(vertex[axis]);
                }
            }
        }
        for part in parts.iter_mut() {
            if !part.minimum.iter().all(|value| value.is_finite()) {
                part.minimum = [0.0; 3];
                part.maximum = [0.0; 3];
            }
            part.center =
                std::array::from_fn(|axis| part.minimum[axis] * 0.5 + part.maximum[axis] * 0.5);
        }
    }

    /// Bounding box center and longest box extent, the frame model coordinates are
    /// normalized into before rendering.
    pub fn frame(&self) -> (Vertex, f64) {
        let center: Vertex =
            std::array::from_fn(|axis| self.minimum[axis] * 0.5 + self.maximum[axis] * 0.5);
        let radius = sub(self.maximum, self.minimum)
            .into_iter()
            .fold(0.0f64, f64::max)
            .max(1e-20);
        (center, radius)
    }

    pub fn inspect(&self, cancel: &std::sync::atomic::AtomicBool) -> Option<Topology> {
        let mut vertices = HashMap::<[u64; 3], u32>::new();
        let mut edges = HashMap::<(u32, u32), (u32, i32)>::new();
        for (index, triangle) in self.triangles.iter().enumerate() {
            if index % 256 == 0 && cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return None;
            }
            let indices = triangle.map(|vertex| {
                let key = vertex.map(|value| if value == 0.0 { 0 } else { value.to_bits() });
                let next = vertices.len() as u32;
                *vertices.entry(key).or_insert(next)
            });
            for (first, second) in [
                (indices[0], indices[1]),
                (indices[1], indices[2]),
                (indices[2], indices[0]),
            ] {
                let edge = edges
                    .entry((first.min(second), first.max(second)))
                    .or_default();
                edge.0 += 1;
                edge.1 += if first < second { 1 } else { -1 };
            }
        }
        Some(Topology {
            vertices: vertices.len(),
            boundary_edges: edges.values().filter(|edge| edge.0 == 1).count(),
            non_manifold_edges: edges.values().filter(|edge| edge.0 > 2).count(),
            inconsistent_edges: edges
                .values()
                .filter(|edge| edge.0 == 2 && edge.1 != 0)
                .count(),
        })
    }
}

#[derive(Debug)]
pub struct Topology {
    pub vertices: usize,
    pub boundary_edges: usize,
    pub non_manifold_edges: usize,
    pub inconsistent_edges: usize,
}

pub fn sub(first: Vertex, second: Vertex) -> Vertex {
    std::array::from_fn(|axis| first[axis] - second[axis])
}
pub fn cross(first: Vertex, second: Vertex) -> Vertex {
    [
        first[1] * second[2] - first[2] * second[1],
        first[2] * second[0] - first[0] * second[2],
        first[0] * second[1] - first[1] * second[0],
    ]
}
pub fn dot(first: Vertex, second: Vertex) -> f64 {
    first[0] * second[0] + first[1] * second[1] + first[2] * second[2]
}

fn push(triangles: &mut Vec<[Vertex; 3]>, triangle: [Vertex; 3]) -> Result<()> {
    ensure!(
        triangles.len() < MAX_TRIANGLES,
        "model exceeds two million triangles"
    );
    triangles.try_reserve(1)?;
    triangles.push(triangle);
    Ok(())
}

fn parse_stl(bytes: &[u8]) -> Result<Vec<[Vertex; 3]>> {
    if let Some(count_bytes) = bytes.get(80..84) {
        let count = u32::from_le_bytes(count_bytes.try_into()?) as usize;
        if count.checked_mul(50).and_then(|size| size.checked_add(84)) == Some(bytes.len()) {
            ensure!(
                count <= MAX_TRIANGLES,
                "model exceeds two million triangles"
            );
            let mut triangles = Vec::new();
            triangles.try_reserve_exact(count)?;
            for record in bytes[84..].chunks_exact(50) {
                let mut triangle = [[0.0; 3]; 3];
                for (vertex_index, vertex) in triangle.iter_mut().enumerate() {
                    for (axis, value) in vertex.iter_mut().enumerate() {
                        let offset = 12 + vertex_index * 12 + axis * 4;
                        *value = f32::from_le_bytes(record[offset..offset + 4].try_into()?) as f64;
                    }
                }
                push(&mut triangles, triangle)?;
            }
            return Ok(triangles);
        }
    }
    let text = std::str::from_utf8(bytes).context("invalid or truncated STL")?;
    let mut triangles = Vec::new();
    let mut triangle = [[0.0; 3]; 3];
    let mut vertices = 0;
    let mut in_facet = false;
    ensure!(text.trim_start().starts_with("solid"), "invalid STL header");
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        match fields.next() {
            Some("facet") => {
                ensure!(!in_facet, "nested STL facet");
                in_facet = true;
                vertices = 0;
            }
            Some("vertex") => {
                ensure!(in_facet && vertices < 3, "invalid STL facet");
                triangle[vertices] = read_vertex(&mut fields)?;
                vertices += 1;
            }
            Some("endfacet") => {
                ensure!(in_facet && vertices == 3, "incomplete STL facet");
                push(&mut triangles, triangle)?;
                in_facet = false;
            }
            _ => {}
        }
    }
    ensure!(!in_facet, "truncated STL facet");
    Ok(triangles)
}

impl PartLayout {
    /// Converts the part descriptors emitted by the STEP reader into the
    /// triangle-index layout the viewer uses.
    fn from_step_parts(parts: Vec<super::step_reader::StepPart>, triangle_count: usize) -> Self {
        let parts: Vec<_> = parts
            .into_iter()
            .filter(|part| part.triangle_count > 0)
            .collect();
        if parts.is_empty() {
            return Self::default();
        }
        let total: usize = parts.iter().map(|part| part.triangle_count).sum();
        if total != triangle_count {
            log::warn!("[model] STEP part layout covers {total} of {triangle_count} triangles");
            return Self::default();
        }
        if parts.len() < 2 {
            return Self::default();
        }
        let mut part_of_triangle = Vec::with_capacity(triangle_count);
        let mut model_parts = Vec::with_capacity(parts.len());
        for (index, part) in parts.into_iter().enumerate() {
            part_of_triangle.extend(std::iter::repeat(index as u32).take(part.triangle_count));
            model_parts.push(ModelPart::new(
                part.name,
                part.color,
                part.triangle_count,
                part.depth,
            ));
        }
        Self {
            parts: model_parts,
            part_of_triangle,
        }
    }
}

fn quantized_vertex(vertex: Vertex, inverse: f64) -> [i64; 3] {
    vertex.map(|value| {
        let scaled = value * inverse;
        if scaled.is_nan() {
            0
        } else {
            let limit = 9.0e15;
            scaled.round().clamp(-limit, limit) as i64
        }
    })
}

fn vertex_hash(key: [i64; 3]) -> u32 {
    let mut hash = 0x811c_9dc5u32;
    for value in key {
        for byte in value.to_le_bytes() {
            hash ^= u32::from(byte);
            hash = hash.wrapping_mul(0x0100_0193);
        }
    }
    hash
}

fn record_corner(record: u64) -> usize {
    (record & 0xffff_ffff) as usize
}

fn record_triangle(record: u64) -> u32 {
    record_corner(record) as u32 / 3
}

fn corner_key(record: u64, triangles: &[[Vertex; 3]], inverse: f64) -> [i64; 3] {
    let corner = record_corner(record);
    quantized_vertex(triangles[corner / 3][corner % 3], inverse)
}

fn component_root(parent: &mut [u32], mut index: u32) -> u32 {
    let mut root = index;
    while parent[root as usize] != root {
        root = parent[root as usize];
    }
    while parent[index as usize] != index {
        let next = parent[index as usize];
        parent[index as usize] = root;
        index = next;
    }
    root
}

fn join_components(parent: &mut [u32], first: u32, second: u32) {
    let first = component_root(parent, first);
    let second = component_root(parent, second);
    if first != second {
        parent[first.max(second) as usize] = first.min(second);
    }
}

/// Splits triangle soup (STL) into connected bodies.
///
/// The naive `HashMap<vertex, id>` needs hundreds of megabytes at the supported
/// two-million-triangle limit. Instead each triangle corner is packed as a
/// 32-bit coordinate hash plus its corner index, the records are sorted, and
/// equal hashes are confirmed against the quantized coordinates. Peak extra
/// memory is eight bytes per corner plus the union-find table.
fn split_stl_parts(triangles: &[[Vertex; 3]]) -> PartLayout {
    if triangles.len() < 2 {
        return PartLayout::default();
    }
    let mut minimum = [f64::INFINITY; 3];
    let mut maximum = [f64::NEG_INFINITY; 3];
    for triangle in triangles {
        for vertex in triangle {
            if !vertex.iter().all(|value| value.is_finite()) {
                return PartLayout::default();
            }
            for axis in 0..3 {
                minimum[axis] = minimum[axis].min(vertex[axis]);
                maximum[axis] = maximum[axis].max(vertex[axis]);
            }
        }
    }
    let diagonal = sub(maximum, minimum).into_iter().fold(0.0f64, f64::max);
    if !diagonal.is_finite() || diagonal <= 0.0 {
        return PartLayout::default();
    }
    let inverse = 1.0 / (diagonal * 1e-7).max(1e-9);

    let mut records = Vec::with_capacity(triangles.len() * 3);
    for (triangle_index, triangle) in triangles.iter().enumerate() {
        for (corner, vertex) in triangle.iter().enumerate() {
            let hash = vertex_hash(quantized_vertex(*vertex, inverse));
            let corner = (triangle_index * 3 + corner) as u32;
            records.push((u64::from(hash) << 32) | u64::from(corner));
        }
    }
    records.sort_unstable();

    let mut parent: Vec<u32> = (0..triangles.len() as u32).collect();
    let mut start = 0;
    while start < records.len() {
        let hash = records[start] >> 32;
        let mut end = start + 1;
        while end < records.len() && records[end] >> 32 == hash {
            end += 1;
        }
        let group = &records[start..end];
        if group.len() > 1 {
            if group.len() <= 32 {
                for first in 0..group.len() {
                    let first_key = corner_key(group[first], triangles, inverse);
                    for second in first + 1..group.len() {
                        if first_key == corner_key(group[second], triangles, inverse) {
                            join_components(
                                &mut parent,
                                record_triangle(group[first]),
                                record_triangle(group[second]),
                            );
                        }
                    }
                }
            } else {
                let mut entries: Vec<([i64; 3], u32)> = group
                    .iter()
                    .map(|record| {
                        (
                            corner_key(*record, triangles, inverse),
                            record_triangle(*record),
                        )
                    })
                    .collect();
                entries.sort_unstable();
                let mut first = 0;
                while first < entries.len() {
                    let mut last = first + 1;
                    while last < entries.len() && entries[last].0 == entries[first].0 {
                        join_components(&mut parent, entries[first].1, entries[last].1);
                        last += 1;
                    }
                    first = last;
                }
            }
        }
        start = end;
    }

    let mut component_of_triangle = vec![u32::MAX; triangles.len()];
    let mut component_counts: Vec<u32> = Vec::new();
    let mut component_first: Vec<u32> = Vec::new();
    let mut roots: HashMap<u32, u32> = HashMap::new();
    for triangle in 0..triangles.len() {
        let root = component_root(&mut parent, triangle as u32);
        let component = match roots.entry(root) {
            std::collections::hash_map::Entry::Occupied(entry) => *entry.get(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let component = component_counts.len() as u32;
                component_counts.push(0);
                component_first.push(triangle as u32);
                entry.insert(component);
                component
            }
        };
        component_of_triangle[triangle] = component;
        component_counts[component as usize] += 1;
    }
    if component_counts.len() <= 1 {
        return PartLayout::default();
    }

    let mut part_indices = vec![0u32; component_counts.len()];
    let mut parts = Vec::new();
    if component_counts.len() <= MAX_PARTS {
        for (index, count) in component_counts.iter().enumerate() {
            part_indices[index] = index as u32;
            parts.push(ModelPart::new(
                i18n::t_args!("08a4c34560df2e32", index + 1),
                None,
                *count as usize,
                0,
            ));
        }
    } else {
        let mut order: Vec<usize> = (0..component_counts.len()).collect();
        order.sort_by(|&first, &second| {
            component_counts[second]
                .cmp(&component_counts[first])
                .then(component_first[first].cmp(&component_first[second]))
        });
        let keep = MAX_PARTS.saturating_sub(1);
        for (part_index, component) in order[..keep].iter().enumerate() {
            part_indices[*component] = part_index as u32;
            parts.push(ModelPart::new(
                i18n::t_args!("08a4c34560df2e32", part_index + 1),
                None,
                component_counts[*component] as usize,
                0,
            ));
        }
        let other_index = parts.len() as u32;
        for component in &order[keep..] {
            part_indices[*component] = other_index;
        }
        let other_count: u32 = order[keep..]
            .iter()
            .map(|component| component_counts[*component])
            .sum();
        parts.push(ModelPart::new(
            i18n::t!("dacb6d45653e6f98").to_string(),
            None,
            other_count as usize,
            0,
        ));
    }

    let part_of_triangle = component_of_triangle
        .into_iter()
        .map(|component| part_indices[component as usize])
        .collect();
    PartLayout {
        parts,
        part_of_triangle,
    }
}

fn read_vertex<'a>(fields: &mut impl Iterator<Item = &'a str>) -> Result<Vertex> {
    let mut vertex = [0.0; 3];
    for value in &mut vertex {
        *value = fields
            .next()
            .context("missing vertex coordinate")?
            .parse()?;
    }
    Ok(vertex)
}

fn parse_obj(text: &str) -> Result<Vec<[Vertex; 3]>> {
    let mut vertices = Vec::new();
    let mut triangles = Vec::new();
    for line in text.lines() {
        let mut fields = line
            .split('#')
            .next()
            .unwrap_or_default()
            .split_whitespace();
        match fields.next() {
            Some("v") => {
                ensure!(vertices.len() < MAX_TRIANGLES * 3, "too many vertices");
                vertices.push(read_vertex(&mut fields)?);
            }
            Some("f") => {
                let mut polygon = Vec::new();
                for field in fields {
                    ensure!(
                        polygon.len() < 3,
                        "OBJ currently requires triangulated faces"
                    );
                    let index: i64 = field
                        .split('/')
                        .next()
                        .context("missing face index")?
                        .parse()?;
                    let index = if index < 0 {
                        vertices.len() as i64 + index
                    } else {
                        index - 1
                    };
                    polygon.push(
                        *vertices
                            .get(usize::try_from(index)?)
                            .context("invalid face index")?,
                    );
                }
                let triangle: [Vertex; 3] = polygon
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("face must contain three vertices"))?;
                push(&mut triangles, triangle)?;
            }
            _ => {}
        }
    }
    Ok(triangles)
}

fn parse_ply(text: &str) -> Result<Vec<[Vertex; 3]>> {
    let mut lines = text.lines();
    ensure!(lines.next() == Some("ply"), "invalid PLY header");
    ensure!(
        lines.next() == Some("format ascii 1.0"),
        "currently only ASCII PLY is supported"
    );
    let mut vertex_count = 0usize;
    let mut face_count = 0usize;
    let mut element = "";
    let mut properties = Vec::new();
    let mut ended = false;
    for line in lines.by_ref() {
        let fields: Vec<_> = line.split_whitespace().collect();
        match fields.as_slice() {
            ["end_header"] => {
                ended = true;
                break;
            }
            ["element", name, count] => {
                element = name;
                match *name {
                    "vertex" => vertex_count = count.parse()?,
                    "face" => face_count = count.parse()?,
                    _ => bail!("unsupported PLY element"),
                }
            }
            ["property", _, name] if element == "vertex" => properties.push(*name),
            ["property", "list", _, _, name]
                if element == "face" && (*name == "vertex_indices" || *name == "vertex_index") => {}
            ["comment", ..] | ["obj_info", ..] => {}
            _ => bail!("unsupported PLY property layout"),
        }
    }
    ensure!(
        ended && vertex_count <= MAX_TRIANGLES * 3 && face_count <= MAX_TRIANGLES,
        "invalid or oversized PLY"
    );
    let axes = ["x", "y", "z"].map(|name| {
        properties
            .iter()
            .position(|property| *property == name)
            .context("missing PLY coordinate")
    });
    let axes = [
        axes[0]
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error}"))?
            .to_owned(),
        axes[1]
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error}"))?
            .to_owned(),
        axes[2]
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error}"))?
            .to_owned(),
    ];
    let mut vertices = Vec::new();
    vertices.try_reserve_exact(vertex_count)?;
    for _ in 0..vertex_count {
        let fields: Vec<_> = lines
            .next()
            .context("truncated PLY")?
            .split_whitespace()
            .collect();
        let mut vertex = [0.0; 3];
        for axis in 0..3 {
            vertex[axis] = fields
                .get(axes[axis])
                .context("missing PLY value")?
                .parse()?;
        }
        vertices.push(vertex);
    }
    let mut triangles = Vec::new();
    for _ in 0..face_count {
        let mut fields = lines.next().context("truncated PLY")?.split_whitespace();
        ensure!(
            fields.next() == Some("3"),
            "PLY currently requires triangulated faces"
        );
        let mut triangle = [[0.0; 3]; 3];
        for vertex in &mut triangle {
            let index: usize = fields.next().context("missing PLY index")?.parse()?;
            *vertex = *vertices.get(index).context("invalid PLY index")?;
        }
        push(&mut triangles, triangle)?;
    }
    Ok(triangles)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ascii_stl_properties_and_topology() {
        let mesh = ModelMesh::parse("stl", b"solid test\nfacet normal 0 0 1\nouter loop\nvertex 0 0 0\nvertex 1 0 0\nvertex 0 1 0\nendloop\nendfacet\nendsolid test").expect("parse");
        assert_eq!(mesh.area, 0.5);
        assert_eq!(mesh.maximum, [1.0, 1.0, 0.0]);
        assert_eq!(
            mesh.inspect(&std::sync::atomic::AtomicBool::new(false))
                .expect("inspection")
                .boundary_edges,
            3
        );
    }
    #[test]
    fn binary_stl_solid_header_is_not_ascii() {
        let mut bytes = vec![0; 134];
        bytes[..5].copy_from_slice(b"solid");
        bytes[80..84].copy_from_slice(&1u32.to_le_bytes());
        bytes[108..112].copy_from_slice(&1f32.to_le_bytes());
        bytes[124..128].copy_from_slice(&1f32.to_le_bytes());
        assert_eq!(ModelMesh::parse("stl", &bytes).expect("parse").area, 0.5);
        bytes.pop();
        assert!(ModelMesh::parse("stl", &bytes).is_err());
    }
    #[test]
    fn ply_properties_and_indices() {
        let ply = b"ply\nformat ascii 1.0\nelement vertex 3\nproperty float x\nproperty float y\nproperty float z\nelement face 1\nproperty list uchar int vertex_indices\nend_header\n0 0 0\n1 0 0\n0 1 0\n3 0 1 2\n";
        assert_eq!(ModelMesh::parse("ply", ply).expect("PLY").area, 0.5);
        assert!(ModelMesh::parse("ply", b"ply\nformat binary_little_endian 1.0\n").is_err());
    }

    #[test]
    fn closed_tetrahedron_volume_and_cancellation() {
        let mesh = ModelMesh::parse(
            "obj",
            b"v 0 0 0\nv 1 0 0\nv 0 1 0\nv 0 0 1\nf 1 3 2\nf 1 2 4\nf 1 4 3\nf 2 3 4",
        )
        .expect("tetrahedron");
        let topology = mesh
            .inspect(&std::sync::atomic::AtomicBool::new(false))
            .expect("inspect");
        assert_eq!(
            (
                topology.vertices,
                topology.boundary_edges,
                topology.non_manifold_edges,
                topology.inconsistent_edges
            ),
            (4, 0, 0, 0)
        );
        assert!((mesh.signed_volume - 1.0 / 6.0).abs() < 1e-12);
        assert!(
            mesh.inspect(&std::sync::atomic::AtomicBool::new(true))
                .is_none()
        );
    }

    fn two_triangle_stl(second_origin: f64) -> Vec<u8> {
        format!(
            "solid test\n\
             facet normal 0 0 1\nouter loop\nvertex 0 0 0\nvertex 1 0 0\nvertex 0 1 0\nendloop\nendfacet\n\
             facet normal 0 0 1\nouter loop\nvertex {second_origin} 0 0\nvertex {} 0 0\nvertex {second_origin} 1 0\nendloop\nendfacet\n\
             endsolid test",
            second_origin + 1.0
        )
        .into_bytes()
    }

    #[test]
    fn disconnected_stl_triangles_become_separate_parts() {
        let mesh = ModelMesh::parse("stl", &two_triangle_stl(10.0)).expect("split");
        assert_eq!(mesh.parts.len(), 2);
        assert_eq!(mesh.part_of_triangle, vec![0, 1]);
        assert_eq!(mesh.parts[0].triangle_count, 1);
        assert_eq!(mesh.parts[1].triangle_count, 1);
        assert_eq!(mesh.parts[0].center, [0.5, 0.5, 0.0]);
        assert_eq!(mesh.parts[1].center, [10.5, 0.5, 0.0]);
        assert!(mesh.parts.iter().all(|part| part.color.is_none()));
    }

    #[test]
    fn single_stl_triangle_keeps_the_flat_path() {
        let mesh = ModelMesh::parse("stl", &two_triangle_stl(0.0)).expect("single");
        assert!(mesh.parts.is_empty());
        assert!(mesh.part_of_triangle.is_empty());
    }

    #[test]
    fn split_welds_coordinates_within_its_tolerance() {
        let stl = b"solid test\n\
            facet normal 0 0 1\nouter loop\nvertex 0 0 0\nvertex 1 0 0\nvertex 0 1 0\nendloop\nendfacet\n\
            facet normal 0 0 1\nouter loop\nvertex 1.000000001 0 0\nvertex 2 0 0\nvertex 2 1 0\nendloop\nendfacet\n\
            endsolid test";
        let mesh = ModelMesh::parse("stl", stl).expect("welded");
        assert!(mesh.parts.is_empty(), "a welded pair must stay one body");
        let stl = b"solid test\n\
            facet normal 0 0 1\nouter loop\nvertex 0 0 0\nvertex 1 0 0\nvertex 0 1 0\nendloop\nendfacet\n\
            facet normal 0 0 1\nouter loop\nvertex 1.1 0 0\nvertex 2 0 0\nvertex 2 1 0\nendloop\nendfacet\n\
            endsolid test";
        let mesh = ModelMesh::parse("stl", stl).expect("separate");
        assert_eq!(mesh.parts.len(), 2);
    }

    #[test]
    fn too_many_stl_bodies_fold_into_one_other_part() {
        let mut stl = String::from("solid test\n");
        for index in 0..=MAX_PARTS {
            let base = (index * 4) as f64;
            stl.push_str(&format!(
                "facet normal 0 0 1\nouter loop\nvertex {base} 0 0\nvertex {} 0 0\nvertex {base} 1 0\nendloop\nendfacet\n",
                base + 1.0
            ));
        }
        stl.push_str("endsolid test\n");
        let mesh = ModelMesh::parse("stl", stl.as_bytes()).expect("many bodies");
        assert_eq!(mesh.parts.len(), MAX_PARTS);
        assert_eq!(
            mesh.parts
                .iter()
                .map(|part| part.triangle_count)
                .sum::<usize>(),
            MAX_PARTS + 1
        );
        assert_eq!(mesh.parts.last().map(|part| part.triangle_count), Some(2));
        assert_eq!(mesh.part_of_triangle.len(), MAX_PARTS + 1);
    }

    #[test]
    fn obj_indices_and_nonfinite_rejection() {
        assert!(ModelMesh::parse("obj", b"v 0 0 0\nv 1 0 0\nv 0 1 0\nf -3 -2 -1").is_ok());
        assert!(ModelMesh::parse("obj", b"v NaN 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3").is_err());
        assert!(ModelMesh::parse("obj", b"f 1 2 3").is_err());
    }
}

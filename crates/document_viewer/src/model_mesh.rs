use anyhow::{Context as _, Result, bail, ensure};
use std::collections::HashMap;

pub const MAX_TRIANGLES: usize = 2_000_000;
pub type Vertex = [f64; 3];

pub struct ModelMesh {
    pub triangles: Vec<[Vertex; 3]>,
    pub minimum: Vertex,
    pub maximum: Vertex,
    pub area: f64,
    pub signed_volume: f64,
    pub degenerate_faces: usize,
}

impl ModelMesh {
    pub fn parse(extension: &str, bytes: &[u8]) -> Result<Self> {
        let triangles = match extension.to_ascii_lowercase().as_str() {
            "stl" => parse_stl(bytes)?,
            "obj" => parse_obj(std::str::from_utf8(bytes)?)?,
            "ply" => parse_ply(std::str::from_utf8(bytes)?)?,
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
        Ok(mesh)
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

    #[test]
    fn obj_indices_and_nonfinite_rejection() {
        assert!(ModelMesh::parse("obj", b"v 0 0 0\nv 1 0 0\nv 0 1 0\nf -3 -2 -1").is_ok());
        assert!(ModelMesh::parse("obj", b"v NaN 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3").is_err());
        assert!(ModelMesh::parse("obj", b"f 1 2 3").is_err());
    }
}

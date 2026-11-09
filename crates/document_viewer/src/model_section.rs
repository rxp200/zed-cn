use crate::model_mesh::{ModelMesh, Vertex, sub};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

/// Normalized coordinates closer than this are treated as the same point when
/// stitching the cross-section curve back into loops.
const WELD: f64 = 1e-9;

/// Distance to the cutting plane below which a vertex counts as lying on it.
const PLANE_EPSILON: f64 = 1e-9;

/// Axis of an axis-aligned cutting plane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SectionAxis {
    X,
    Y,
    Z,
}

impl SectionAxis {
    pub const ALL: [SectionAxis; 3] = [SectionAxis::X, SectionAxis::Y, SectionAxis::Z];

    pub fn index(self) -> usize {
        match self {
            SectionAxis::X => 0,
            SectionAxis::Y => 1,
            SectionAxis::Z => 2,
        }
    }
}

/// Axis-aligned cutting plane of the model viewer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SectionPlane {
    pub axis: SectionAxis,
    /// Position along `axis` as a fraction of the model bounding box.
    pub offset: f64,
    /// Keeps the negative half-space instead of the positive one.
    pub flipped: bool,
}

impl SectionPlane {
    /// Plane position in normalized model coordinates, i.e. after `normalize`.
    pub fn threshold(self, mesh: &ModelMesh, center: Vertex, radius: f64) -> f64 {
        let axis = self.axis.index();
        let minimum = mesh.minimum[axis];
        let position = minimum + (mesh.maximum[axis] - minimum) * self.offset;
        (position - center[axis]) / radius
    }

    /// Sign of the kept half-space: vertices with `sign * (value - threshold) >= 0` survive.
    pub fn sign(self) -> f64 {
        if self.flipped { -1.0 } else { 1.0 }
    }
}

/// Maps model coordinates into the normalized frame the rasterizer uses.
pub fn normalize(vertex: Vertex, center: Vertex, radius: f64) -> Vertex {
    sub(vertex, center).map(|value| value / radius)
}

/// A triangle clipped against a [`SectionPlane`], in normalized model space.
pub struct ClippedTriangle {
    /// Resulting convex polygon; only the first `length` vertices are meaningful.
    pub polygon: [Vertex; 4],
    pub length: usize,
    /// Polygon edge lying on the cutting plane, when the triangle was cut.
    pub cut: Option<[Vertex; 2]>,
}

/// Clips one triangle against `sign * (vertex[axis] - threshold) >= 0`.
pub fn clip_triangle(
    triangle: [Vertex; 3],
    axis: usize,
    threshold: f64,
    sign: f64,
) -> ClippedTriangle {
    let distances = triangle.map(|vertex| sign * (vertex[axis] - threshold));
    let mut polygon = [[0.0; 3]; 4];
    let mut length = 0;
    for index in 0..3 {
        let next = (index + 1) % 3;
        if distances[index] >= 0.0 && length < polygon.len() {
            polygon[length] = triangle[index];
            length += 1;
        }
        // A vertex exactly on the plane is already emitted above; only a strict sign
        // change adds a new intersection point, which keeps the polygon free of
        // duplicate vertices when the plane passes through a corner.
        if distances[index] != 0.0
            && distances[next] != 0.0
            && (distances[index] > 0.0) != (distances[next] > 0.0)
            && length < polygon.len()
        {
            let t = distances[index] / (distances[index] - distances[next]);
            polygon[length] = std::array::from_fn(|component| {
                triangle[index][component]
                    + (triangle[next][component] - triangle[index][component]) * t
            });
            length += 1;
        }
    }
    let on_plane = |vertex: Vertex| (sign * (vertex[axis] - threshold)).abs() <= PLANE_EPSILON;
    let mut cut = None;
    if length >= 2 {
        for index in 0..length {
            let first = polygon[index];
            let second = polygon[(index + 1) % length];
            if on_plane(first) && on_plane(second) {
                cut = Some([first, second]);
                break;
            }
        }
    }
    ClippedTriangle {
        polygon,
        length,
        cut,
    }
}

/// Cross-section boundary of a mesh on one cutting plane.
pub struct Section {
    /// Closed boundary loops, in normalized model space, all lying on the plane.
    pub loops: Vec<Vec<Vertex>>,
    /// Area of the cut in squared model units.
    pub area: f64,
}

/// Builds the cross-section boundary of `mesh` on `plane`.
///
/// `center` and `radius` must be the frame from [`ModelMesh::frame`], i.e. the same
/// normalization the rasterizer applies. Returns `None` when cancelled.
pub fn build(
    mesh: &ModelMesh,
    plane: SectionPlane,
    center: Vertex,
    radius: f64,
    cancel: &AtomicBool,
) -> Option<Section> {
    let axis = plane.axis.index();
    let threshold = plane.threshold(mesh, center, radius);
    let sign = plane.sign();
    let mut points = Vec::<Vertex>::new();
    let mut indices = HashMap::<[i64; 3], usize>::new();
    let mut edges = Vec::<(usize, usize)>::new();
    let mut seen = HashSet::<(usize, usize)>::new();
    for (index, triangle) in mesh.triangles.iter().enumerate() {
        if index % 256 == 0 && cancel.load(Ordering::Relaxed) {
            return None;
        }
        let normalized = triangle.map(|vertex| normalize(vertex, center, radius));
        let Some(cut) = clip_triangle(normalized, axis, threshold, sign).cut else {
            continue;
        };
        let first = weld(cut[0], &mut points, &mut indices);
        let second = weld(cut[1], &mut points, &mut indices);
        if first == second {
            continue;
        }
        // Neighbouring triangles share their intersection points but not their
        // winding, so segments are stored as unordered index pairs and deduplicated.
        if seen.insert((first.min(second), first.max(second))) {
            edges.push((first, second));
        }
    }
    let mut adjacency = vec![Vec::<(usize, usize)>::new(); points.len()];
    for (id, (first, second)) in edges.iter().enumerate() {
        adjacency[*first].push((*second, id));
        adjacency[*second].push((*first, id));
    }
    let mut used = vec![false; edges.len()];
    let mut loops = Vec::new();
    for start in 0..points.len() {
        while adjacency[start].iter().any(|(_, id)| !used[*id]) {
            let mut loop_points = Vec::new();
            let mut current = start;
            loop {
                loop_points.push(points[current]);
                let Some(position) = adjacency[current].iter().position(|(_, id)| !used[*id])
                else {
                    break;
                };
                let (next, id) = adjacency[current][position];
                used[id] = true;
                current = next;
                if current == start {
                    break;
                }
            }
            // An open chain comes from a non-manifold or open surface; it cannot
            // bound a cross-section area, so only closed loops are kept.
            if current == start && loop_points.len() >= 3 {
                loops.push(loop_points);
            }
        }
    }
    let (first_axis, second_axis) = match axis {
        0 => (1, 2),
        1 => (0, 2),
        _ => (0, 1),
    };
    let area = loops_area(&loops, first_axis, second_axis) * radius * radius;
    Some(Section { loops, area })
}

fn weld(vertex: Vertex, points: &mut Vec<Vertex>, indices: &mut HashMap<[i64; 3], usize>) -> usize {
    let key = vertex.map(|value| (value / WELD).round() as i64);
    *indices.entry(key).or_insert_with(|| {
        points.push(vertex);
        points.len() - 1
    })
}

/// Area enclosed by the closed curve, independent of loop orientation.
///
/// Bands between consecutive vertex coordinates are integrated by sampling their
/// midpoint, where the covered width is linear and the even-odd rule already
/// accounts for holes and nested loops.
fn loops_area(loops: &[Vec<Vertex>], first_axis: usize, second_axis: usize) -> f64 {
    let mut edges = Vec::<(f64, f64, f64, f64)>::new();
    let mut rows = Vec::<f64>::new();
    for loop_points in loops {
        for index in 0..loop_points.len() {
            let first = loop_points[index];
            let second = loop_points[(index + 1) % loop_points.len()];
            if first[second_axis] == second[second_axis] {
                continue;
            }
            edges.push((
                first[first_axis],
                first[second_axis],
                second[first_axis],
                second[second_axis],
            ));
            rows.push(first[second_axis]);
        }
    }
    rows.sort_by(|first, second| {
        first
            .partial_cmp(second)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    rows.dedup_by(|first, second| (*first - *second).abs() <= WELD);
    let mut area = 0.0;
    for band in rows.windows(2) {
        let midpoint = (band[0] + band[1]) * 0.5;
        area += span(&edges, midpoint) * (band[1] - band[0]);
    }
    area
}

/// Width covered by the curve at one row, using the even-odd rule.
fn span(edges: &[(f64, f64, f64, f64)], row: f64) -> f64 {
    let mut crossings = Vec::new();
    for (first_u, first_v, second_u, second_v) in edges {
        let (low, high) = if first_v < second_v {
            (*first_v, *second_v)
        } else {
            (*second_v, *first_v)
        };
        if row <= low || row >= high {
            continue;
        }
        let t = (row - first_v) / (second_v - first_v);
        crossings.push(first_u + (second_u - first_u) * t);
    }
    crossings.sort_by(|first, second| {
        first
            .partial_cmp(second)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    crossings
        .chunks_exact(2)
        .map(|pair| (pair[1] - pair[0]).max(0.0))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn box_mesh(scale: f64) -> ModelMesh {
        let corners = [
            [-1.0, -1.0, -1.0],
            [1.0, -1.0, -1.0],
            [1.0, 1.0, -1.0],
            [-1.0, 1.0, -1.0],
            [-1.0, -1.0, 1.0],
            [1.0, -1.0, 1.0],
            [1.0, 1.0, 1.0],
            [-1.0, 1.0, 1.0],
        ];
        let mut obj = String::new();
        for corner in corners {
            obj.push_str(&format!(
                "v {} {} {}\n",
                corner[0] * scale,
                corner[1] * scale,
                corner[2] * scale
            ));
        }
        for [a, b, c, d] in [
            [1, 4, 3, 2],
            [5, 6, 7, 8],
            [1, 2, 6, 5],
            [3, 4, 8, 7],
            [1, 5, 8, 4],
            [2, 3, 7, 6],
        ] {
            obj.push_str(&format!("f {a} {b} {c}\nf {a} {c} {d}\n"));
        }
        ModelMesh::parse("obj", obj.as_bytes()).expect("box")
    }

    fn three_boxes() -> ModelMesh {
        let mut obj = String::new();
        let mut offset = 0;
        for (scale, translation) in [(1.0, 0.0), (0.25, 0.0), (0.5, 4.0)] {
            let corners = [
                [-1.0, -1.0, -1.0],
                [1.0, -1.0, -1.0],
                [1.0, 1.0, -1.0],
                [-1.0, 1.0, -1.0],
                [-1.0, -1.0, 1.0],
                [1.0, -1.0, 1.0],
                [1.0, 1.0, 1.0],
                [-1.0, 1.0, 1.0],
            ];
            for corner in corners {
                obj.push_str(&format!(
                    "v {} {} {}\n",
                    corner[0] * scale + translation,
                    corner[1] * scale,
                    corner[2] * scale
                ));
            }
            for [a, b, c, d] in [
                [1, 4, 3, 2],
                [5, 6, 7, 8],
                [1, 2, 6, 5],
                [3, 4, 8, 7],
                [1, 5, 8, 4],
                [2, 3, 7, 6],
            ] {
                obj.push_str(&format!(
                    "f {} {} {}\nf {} {} {}\n",
                    a + offset,
                    b + offset,
                    c + offset,
                    a + offset,
                    c + offset,
                    d + offset
                ));
            }
            offset += 8;
        }
        ModelMesh::parse("obj", obj.as_bytes()).expect("boxes")
    }

    #[test]
    fn clipping_keeps_drops_and_splits() {
        let axis = 2;
        let kept = clip_triangle([[0.0, 0.0, 1.0]; 3], axis, 0.5, 1.0);
        assert_eq!(kept.length, 3);
        assert!(kept.cut.is_none());

        let dropped = clip_triangle([[0.0, 0.0, 0.0]; 3], axis, 0.5, 1.0);
        assert_eq!(dropped.length, 0);

        let split = clip_triangle(
            [[0.0, 0.0, 0.0], [2.0, 0.0, 0.0], [0.0, 0.0, 2.0]],
            axis,
            1.0,
            1.0,
        );
        assert_eq!(split.length, 3);
        assert_eq!(split.cut, Some([[0.0, 0.0, 1.0], [1.0, 0.0, 1.0]]));
        assert!((split.polygon[0][2] - 1.0).abs() < 1e-12);
        assert!((split.polygon[2][2] - 1.0).abs() < 1e-12);

        // A plane through a corner must not duplicate the corner vertex.
        let corner = clip_triangle(
            [[0.0, 0.0, 0.0], [1.0, 0.0, 1.0], [0.0, 0.0, -1.0]],
            axis,
            0.0,
            1.0,
        );
        assert_eq!(corner.length, 3);
        assert_eq!(corner.cut, Some([[0.5, 0.0, 0.0], [0.0, 0.0, 0.0]]));
    }

    #[test]
    fn cube_cut_is_a_square() {
        let mesh = box_mesh(1.0);
        let (center, radius) = mesh.frame();
        let section = build(
            &mesh,
            SectionPlane {
                axis: SectionAxis::Y,
                offset: 0.5,
                flipped: false,
            },
            center,
            radius,
            &AtomicBool::new(false),
        )
        .expect("section");
        assert_eq!(section.loops.len(), 1);
        assert!((section.area - 4.0).abs() < 1e-9);
        // Side faces contribute their edge midpoints as well as the corners.
        let mut bounds = [[f64::INFINITY, f64::NEG_INFINITY]; 2];
        for vertex in &section.loops[0] {
            assert!(vertex[1].abs() < 1e-12);
            for (slot, value) in bounds.iter_mut().zip([vertex[0], vertex[2]]) {
                slot[0] = slot[0].min(value);
                slot[1] = slot[1].max(value);
            }
        }
        for [minimum, maximum] in bounds {
            assert!((minimum + 0.5).abs() < 1e-12);
            assert!((maximum - 0.5).abs() < 1e-12);
        }

        let flipped = build(
            &mesh,
            SectionPlane {
                axis: SectionAxis::Y,
                offset: 0.5,
                flipped: true,
            },
            center,
            radius,
            &AtomicBool::new(false),
        )
        .expect("flipped section");
        assert!((flipped.area - 4.0).abs() < 1e-9);
    }

    #[test]
    fn cut_outside_the_model_is_empty() {
        let mesh = box_mesh(1.0);
        let (center, radius) = mesh.frame();
        let section = build(
            &mesh,
            SectionPlane {
                axis: SectionAxis::X,
                offset: 2.0,
                flipped: false,
            },
            center,
            radius,
            &AtomicBool::new(false),
        )
        .expect("section");
        assert!(section.loops.is_empty());
        assert_eq!(section.area, 0.0);
    }

    #[test]
    fn nested_and_separate_cuts_subtract_holes() {
        let mesh = three_boxes();
        let (center, radius) = mesh.frame();
        let section = build(
            &mesh,
            SectionPlane {
                axis: SectionAxis::Z,
                offset: 0.5,
                flipped: false,
            },
            center,
            radius,
            &AtomicBool::new(false),
        )
        .expect("section");
        // A 2x2 square minus a 0.5x0.5 hole at the same place, plus a detached 1x1 square.
        let expected = 4.0 - 0.25 + 1.0;
        assert_eq!(section.loops.len(), 3);
        assert!((section.area - expected).abs() < 1e-9);
    }

    #[test]
    fn building_is_cancellable() {
        let mesh = box_mesh(1.0);
        let (center, radius) = mesh.frame();
        assert!(
            build(
                &mesh,
                SectionPlane {
                    axis: SectionAxis::Y,
                    offset: 0.5,
                    flipped: false,
                },
                center,
                radius,
                &AtomicBool::new(true),
            )
            .is_none()
        );
    }
}

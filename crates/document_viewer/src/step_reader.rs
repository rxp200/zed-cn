//! Read-only STEP (ISO 10303-21, AP203/AP214/AP242) geometry import.
//!
//! STEP stores boundary representations: NURBS or analytic surfaces trimmed by
//! curves. Rendering therefore needs a tessellator. The pure-Rust `truck` kernel
//! provides B-rep construction and tessellation; this module owns everything
//! `truck-stepio` does not model: products and assembly transforms, mapped items,
//! length units, the entity allow-list, resource budgets and user-facing errors.

use anyhow::{Result, anyhow, bail, ensure};
use ruststep::ast::{DataSection, EntityInstance, Name, Parameter};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use truck_geometry::prelude::SearchNearestParameter as _;
use truck_meshalgo::tessellation::{MeshedShape as _, RobustMeshableShape as _};
use truck_stepio::r#in::{Table, alias::*};
use truck_topology::compress::CompressedShell;

use crate::model_mesh::MAX_TRIANGLES;

/// Upper bound on the number of shells (bodies) considered in one file.
const MAX_SHELLS: usize = 4096;
/// Upper bound on the number of faces before tessellation starts. A pathological
/// B-rep with millions of faces would otherwise spend minutes building topology.
const MAX_FACES: usize = 500_000;
/// Wall-clock budget for the import. Tessellation has no cancellation hook, so
/// this is checked between shells and assembly nodes.
const PARSE_BUDGET: Duration = Duration::from_secs(120);
/// Guard against cyclic or absurdly deep assembly graphs.
const MAX_ASSEMBLY_DEPTH: usize = 32;
/// Guard against cyclic unit conversion chains.
const MAX_UNIT_DEPTH: usize = 4;

/// A length unit found in the file. All output coordinates are millimetres.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepUnit {
    Millimetre,
    Centimetre,
    Metre,
    Micrometre,
    Inch,
    Foot,
    Unknown,
}

impl StepUnit {
    /// Millimetres per file unit.
    fn scale(self) -> f64 {
        match self {
            Self::Millimetre => 1.0,
            Self::Centimetre => 10.0,
            Self::Metre => 1000.0,
            Self::Micrometre => 0.001,
            Self::Inch => 25.4,
            Self::Foot => 304.8,
            Self::Unknown => 1.0,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Millimetre => "mm",
            Self::Centimetre => "cm",
            Self::Metre => "m",
            Self::Micrometre => "µm",
            Self::Inch => "in",
            Self::Foot => "ft",
            Self::Unknown => "",
        }
    }
}

/// Flattened STEP geometry, ready for the existing model viewer.
pub struct StepMesh {
    pub triangles: Vec<[[f64; 3]; 3]>,
    /// Number of shells (solid bodies) that contributed triangles.
    pub solids: usize,
    pub unit: StepUnit,
}

/// A frame with an orthonormal basis and a uniform scale folded into it. STEP's
/// `AXIS2_PLACEMENT_3D` and transformation operators both map onto this.
#[derive(Clone, Copy, Debug)]
struct Frame {
    basis: [[f64; 3]; 3],
    origin: [f64; 3],
}

impl Frame {
    const IDENTITY: Self = Self {
        basis: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        origin: [0.0; 3],
    };

    fn from_axis2(origin: [f64; 3], axis: [f64; 3], reference: [f64; 3]) -> Self {
        let z = normalize(axis).unwrap_or([0.0, 0.0, 1.0]);
        let x = normalize(reference).unwrap_or_else(|| perpendicular(z));
        let x = normalize(sub(x, mul(z, dot(x, z)))).unwrap_or_else(|| perpendicular(z));
        let y = cross(z, x);
        Self {
            basis: [x, y, z],
            origin,
        }
    }

    fn apply(&self, point: [f64; 3]) -> [f64; 3] {
        let [x, y, z] = self.basis;
        [
            self.origin[0] + x[0] * point[0] + y[0] * point[1] + z[0] * point[2],
            self.origin[1] + x[1] * point[0] + y[1] * point[1] + z[1] * point[2],
            self.origin[2] + x[2] * point[0] + y[2] * point[1] + z[2] * point[2],
        ]
    }

    fn then(&self, child: &Self) -> Self {
        Self {
            basis: [
                apply_basis(self.basis, child.basis[0]),
                apply_basis(self.basis, child.basis[1]),
                apply_basis(self.basis, child.basis[2]),
            ],
            origin: self.apply(child.origin),
        }
    }

    fn inverse(&self) -> Option<Self> {
        let inverse = invert(self.basis)?;
        let origin = mul_mat_vec(inverse, self.origin);
        Some(Self {
            basis: inverse,
            origin: [-origin[0], -origin[1], -origin[2]],
        })
    }
}

/// One entity id plus its name and parameters. `truck-stepio` only decodes the
/// geometry/topology graph, so product assembly and unit records are read here.
struct RawEntity {
    name: String,
    parameters: Vec<Parameter>,
}

type RawEntities = HashMap<u64, RawEntity>;

/// Parses a STEP file into triangles in millimetres.
pub fn parse(bytes: &[u8]) -> Result<StepMesh> {
    let text = std::str::from_utf8(bytes).map_err(|_| anyhow!(i18n::t!("1b27f6899e658e81")))?;
    let exchange = ruststep::parser::parse(text)
        .map_err(|error| anyhow!(i18n::t!("80bd84afa3b5b7b2", error = error.to_string())))?;
    ensure!(!exchange.data.is_empty(), i18n::t!("c0a8e86e433996bb"));

    let deadline = Instant::now() + PARSE_BUDGET;
    let raw = collect_raw_entities(&exchange.data);
    let (table, failed) = build_table(&exchange.data);
    if failed > 0 {
        log::warn!("[step] {failed} entities could not be decoded by truck-stepio");
    }
    ensure!(!table.shell.is_empty(), i18n::t!("67c7488a47fd7b4c"));

    let unit = detect_unit(&raw);
    let scale = unit.scale();
    let placements = shell_placements(&raw, &table);
    ensure!(!placements.is_empty(), i18n::t!("67c7488a47fd7b4c"));

    // Start with a tolerance relative to the model size and coarsen it when a
    // finely tessellated assembly would blow the triangle budget.
    let mut triangles = Vec::new();
    let mut solids = 0;
    let mut coarseness = 1.0_f64;
    for attempt in 0..3 {
        match tessellate_shells(&table, &placements, scale, coarseness, deadline) {
            Ok((tessellated, shell_count)) => {
                triangles = tessellated;
                solids = shell_count;
                break;
            }
            Err(TessellateFailure::TooManyTriangles) if attempt < 2 => {
                coarseness *= 4.0;
                log::info!("[step] retrying tessellation with coarseness {coarseness}");
            }
            Err(TessellateFailure::TooManyTriangles) => {
                bail!(i18n::t!("2aea4809a12a9bd8"));
            }
            Err(TessellateFailure::TimedOut) => {
                bail!(i18n::t!("029605b1e2c5a4f8"));
            }
            Err(TessellateFailure::TooManyBodies) => {
                bail!(i18n::t!("1bc6546baf14812c"));
            }
            Err(TessellateFailure::Panicked) => {
                bail!(i18n::t!("c6a6a572362095e2"));
            }
            Err(TessellateFailure::Fatal(error)) => return Err(error),
        }
    }

    ensure!(!triangles.is_empty(), i18n::t!("b3c2a02674c98236"));
    log::info!(
        "[step] parsed {} solids, {} triangles, unit {}",
        solids,
        triangles.len(),
        unit.label()
    );
    Ok(StepMesh {
        triangles,
        solids,
        unit,
    })
}

enum TessellateFailure {
    TooManyTriangles,
    TimedOut,
    TooManyBodies,
    Panicked,
    Fatal(anyhow::Error),
}

impl From<anyhow::Error> for TessellateFailure {
    fn from(error: anyhow::Error) -> Self {
        Self::Fatal(error)
    }
}

fn tessellate_shells(
    table: &Table,
    placements: &[(u64, Frame)],
    scale: f64,
    coarseness: f64,
    deadline: Instant,
) -> Result<(Vec<[[f64; 3]; 3]>, usize), TessellateFailure> {
    let mut triangles = Vec::new();
    let mut solids = 0;
    let mut inspected_faces = 0;
    if placements.len() > MAX_SHELLS {
        return Err(TessellateFailure::TooManyBodies);
    }
    for (shell_id, placement) in placements {
        if Instant::now() >= deadline {
            return Err(TessellateFailure::TimedOut);
        }
        let Some(shell) = table.shell.get(shell_id) else {
            continue;
        };
        let output = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut compressed = table
                .to_compressed_shell(shell)
                .map_err(|error| anyhow!("{error}"))?;
            orient_boundaries(&mut compressed);
            let tolerance = tessellation_tolerance(&compressed, scale) * coarseness;
            let tessellated = compressed.robust_triangulation(tolerance);
            let polygon = tessellated.to_polygon();
            Ok::<_, anyhow::Error>((polygon, compressed.faces.len()))
        }));
        let (polygon, face_count) = match output {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => {
                log::warn!("[step] shell #{shell_id} is not convertible: {error}");
                continue;
            }
            Err(_) => {
                log::warn!("[step] shell #{shell_id} panicked while tessellating");
                continue;
            }
        };
        inspected_faces += face_count;
        if inspected_faces > MAX_FACES {
            return Err(TessellateFailure::Fatal(anyhow!(i18n::t!("2c715501987c136e"))));
        }
        append_polygon(&polygon, placement, scale, MAX_TRIANGLES, &mut triangles)?;
        solids += 1;
    }
    Ok((triangles, solids))
}

fn build_table(sections: &[DataSection]) -> (Table, usize) {
    let mut table = Table::default();
    let mut failed = 0usize;
    for section in sections {
        for instance in &section.entities {
            if let Err(error) = table.push_instance(instance) {
                eprintln!("[step-debug] push failed: {error}");
                failed += 1;
            }
        }
    }
    (table, failed)
}

/// Entities needed to reconstruct product structure, units and assembly frames.
/// Geometry and topology entities are left to `truck-stepio` so a large STEP
/// file is not duplicated in the raw table.
const STRUCTURAL_ENTITIES: &[&str] = &[
    "PRODUCT_DEFINITION_SHAPE",
    "SHAPE_DEFINITION_REPRESENTATION",
    "NEXT_ASSEMBLY_USAGE_OCCURRENCE",
    "REPRESENTATION_RELATIONSHIP_WITH_TRANSFORMATION",
    "ITEM_DEFINED_TRANSFORMATION",
    "AXIS2_PLACEMENT_3D",
    "MANIFOLD_SOLID_BREP",
    "BREP_WITH_VOIDS",
    "SHELL_BASED_SURFACE_MODEL",
    "CLOSED_SHELL",
    "OPEN_SHELL",
    "MAPPED_ITEM",
    "REPRESENTATION_MAP",
    "CARTESIAN_TRANSFORMATION_OPERATOR_3D",
    "CARTESIAN_TRANSFORMATION_OPERATOR_2D",
    "SHAPE_REPRESENTATION",
    "ADVANCED_BREP_SHAPE_REPRESENTATION",
    "MANIFOLD_SURFACE_SHAPE_REPRESENTATION",
    "GEOMETRICALLY_BOUNDED_SURFACE_SHAPE_REPRESENTATION",
    "GEOMETRICALLY_BOUNDED_WIREFRAME_SHAPE_REPRESENTATION",
    "FACETED_BREP_SHAPE_REPRESENTATION",
    "SI_UNIT",
    "CONVERSION_BASED_UNIT",
    "LENGTH_UNIT",
    "LENGTH_MEASURE_WITH_UNIT",
    "MEASURE_WITH_UNIT",
];

fn collect_raw_entities(sections: &[DataSection]) -> RawEntities {
    let mut raw = RawEntities::new();
    let mut referenced = HashSet::new();
    for section in sections {
        for instance in &section.entities {
            match instance {
                EntityInstance::Simple { id, record } => {
                    if STRUCTURAL_ENTITIES.contains(&record.name.as_str()) {
                        collect_references(&record.parameter, &mut referenced);
                        raw.insert(*id, raw_entity(&record.name, &record.parameter));
                    }
                }
                EntityInstance::Complex { id, subsuper } => {
                    for record in subsuper {
                        // Only unit records are needed from complex instances;
                        // keeping the last one per id leaves the SI_UNIT payload.
                        if matches!(
                            record.name.as_str(),
                            "SI_UNIT" | "CONVERSION_BASED_UNIT" | "LENGTH_UNIT"
                        ) {
                            raw.insert(*id, raw_entity(&record.name, &record.parameter));
                        }
                    }
                }
            }
        }
    }
    if !referenced.is_empty() {
        for section in sections {
            for instance in &section.entities {
                if let EntityInstance::Simple { id, record } = instance {
                    if referenced.contains(id)
                        && matches!(record.name.as_str(), "CARTESIAN_POINT" | "DIRECTION")
                    {
                        raw.insert(*id, raw_entity(&record.name, &record.parameter));
                    }
                }
            }
        }
    }
    raw
}

fn collect_references(parameter: &Parameter, referenced: &mut HashSet<u64>) {
    match parameter {
        Parameter::Ref(Name::Entity(id)) => {
            referenced.insert(*id);
        }
        Parameter::List(parameters) => {
            for parameter in parameters {
                collect_references(parameter, referenced);
            }
        }
        Parameter::Typed { parameter, .. } => collect_references(parameter, referenced),
        _ => {}
    }
}

fn raw_entity(name: &str, parameter: &Parameter) -> RawEntity {
    RawEntity {
        name: name.to_owned(),
        parameters: match parameter {
            Parameter::List(parameters) => parameters.clone(),
            other => vec![other.clone()],
        },
    }
}

/// Finds the file's length unit. STEP writes it as an SI unit inside a complex
/// `(LENGTH_UNIT() NAMED_UNIT(*) SI_UNIT(...))` instance, or as a
/// conversion-based unit whose factor points at another unit.
fn detect_unit(raw: &RawEntities) -> StepUnit {
    for entity in raw.values() {
        if entity.name == "CONVERSION_BASED_UNIT" {
            if let Some(unit) = conversion_unit(raw, entity, 0) {
                return unit;
            }
        }
    }
    let mut best = StepUnit::Unknown;
    for entity in raw.values() {
        match entity.name.as_str() {
            "SI_UNIT" => {
                let (prefix, name) = match entity.parameters.as_slice() {
                    [Parameter::Enumeration(name)] => (None, name.as_str()),
                    [prefix, Parameter::Enumeration(name)] => (enumeration(prefix), name.as_str()),
                    _ => continue,
                };
                let candidate = match name {
                    "METRE" => match prefix {
                        Some("MILLI") => StepUnit::Millimetre,
                        Some("CENTI") => StepUnit::Centimetre,
                        Some("MICRO") => StepUnit::Micrometre,
                        _ => StepUnit::Metre,
                    },
                    "INCH" => StepUnit::Inch,
                    "FOOT" => StepUnit::Foot,
                    _ => continue,
                };
                // Prefer a concrete unit over a later "unknown" default.
                return candidate;
            }
            "CONVERSION_BASED_UNIT" => {
                if let Some(candidate) = conversion_unit(raw, entity, 0) {
                    if best == StepUnit::Unknown {
                        best = candidate;
                    }
                }
            }
            _ => {}
        }
    }
    best
}

fn conversion_unit(raw: &RawEntities, entity: &RawEntity, depth: usize) -> Option<StepUnit> {
    if depth > MAX_UNIT_DEPTH {
        return None;
    }
    if let Some(Parameter::String(name)) = entity.parameters.first() {
        match name.to_ascii_uppercase().replace([' ', '_'], "").as_str() {
            "INCH" | "IN" => return Some(StepUnit::Inch),
            "FOOT" | "FT" => return Some(StepUnit::Foot),
            "MILLIMETRE" | "MILLIMETER" | "MM" => return Some(StepUnit::Millimetre),
            "CENTIMETRE" | "CENTIMETER" | "CM" => return Some(StepUnit::Centimetre),
            "METRE" | "METER" | "M" => return Some(StepUnit::Metre),
            _ => {}
        }
    }
    let measure = entity.parameters.get(1)?;
    let (factor, base) = match measure {
        Parameter::Ref(Name::Entity(id)) => {
            let entity = raw.get(id)?;
            let factor = length_measure(entity.parameters.first()?)?;
            match entity.parameters.get(1) {
                Some(unit) => (factor, unit_parameter(raw, unit, depth + 1)?.scale()),
                None => (factor, 1.0),
            }
        }
        inline => (
            length_measure(inline)?,
            unit_parameter(raw, inline, depth + 1).map_or(1.0, StepUnit::scale),
        ),
    };
    match factor * base {
        value if (value - 1.0).abs() < 1e-9 => Some(StepUnit::Millimetre),
        value if (value - 10.0).abs() < 1e-9 => Some(StepUnit::Centimetre),
        value if (value - 1000.0).abs() < 1e-9 => Some(StepUnit::Metre),
        value if (value - 0.001).abs() < 1e-12 => Some(StepUnit::Micrometre),
        value if (value - 25.4).abs() < 1e-6 => Some(StepUnit::Inch),
        value if (value - 304.8).abs() < 1e-6 => Some(StepUnit::Foot),
        _ => None,
    }
}

fn length_measure(parameter: &Parameter) -> Option<f64> {
    match parameter {
        Parameter::Typed { keyword, parameter } if keyword == "LENGTH_MEASURE" => {
            match parameter.as_ref() {
                Parameter::Real(value) => Some(*value),
                Parameter::Integer(value) => Some(*value as f64),
                _ => None,
            }
        }
        Parameter::Typed { parameter, .. } => length_measure(parameter),
        Parameter::List(parameters) => parameters.iter().find_map(length_measure),
        _ => None,
    }
}

/// Resolves the unit referenced by a `MEASURE_WITH_UNIT` / `LENGTH_MEASURE_WITH_UNIT`.
fn unit_parameter(raw: &RawEntities, parameter: &Parameter, depth: usize) -> Option<StepUnit> {
    if depth > MAX_UNIT_DEPTH {
        return None;
    }
    match parameter {
        Parameter::Ref(Name::Entity(id)) => {
            let entity = raw.get(id)?;
            match entity.name.as_str() {
                "SI_UNIT" => match entity.parameters.as_slice() {
                    [Parameter::Enumeration(name)] => match name.as_str() {
                        "METRE" => Some(StepUnit::Metre),
                        "INCH" => Some(StepUnit::Inch),
                        "FOOT" => Some(StepUnit::Foot),
                        _ => None,
                    },
                    [prefix, Parameter::Enumeration(name)] => {
                        match (enumeration(prefix), name.as_str()) {
                            (Some("MILLI"), "METRE") => Some(StepUnit::Millimetre),
                            (Some("CENTI"), "METRE") => Some(StepUnit::Centimetre),
                            (Some("MICRO"), "METRE") => Some(StepUnit::Micrometre),
                            (_, "METRE") => Some(StepUnit::Metre),
                            _ => None,
                        }
                    }
                    _ => None,
                },
                "CONVERSION_BASED_UNIT" => conversion_unit(raw, entity, depth),
                _ => None,
            }
        }
        Parameter::Typed { parameter, .. } => unit_parameter(raw, parameter, depth),
        Parameter::List(parameters) => parameters
            .iter()
            .find_map(|parameter| unit_parameter(raw, parameter, depth)),
        _ => None,
    }
}

/// Chooses a chordal tolerance relative to the model size so both a 5 mm part
/// and a 5 m assembly tessellate sensibly.
fn tessellation_tolerance(shell: &CompressedShell<Point3, Curve3D, Surface>, scale: f64) -> f64 {
    let mut minimum = [f64::INFINITY; 3];
    let mut maximum = [f64::NEG_INFINITY; 3];
    for point in &shell.vertices {
        let point = [point.x * scale, point.y * scale, point.z * scale];
        for axis in 0..3 {
            minimum[axis] = minimum[axis].min(point[axis]);
            maximum[axis] = maximum[axis].max(point[axis]);
        }
    }
    let extent = sub(maximum, minimum);
    let diagonal = dot(extent, extent).sqrt();
    if diagonal.is_finite() && diagonal > 0.0 {
        (diagonal * 5.0e-4).clamp(1.0e-5, 10.0)
    } else {
        0.01
    }
}

/// `truck` adds a spurious copy of a surface's parameter rectangle whenever all
/// boundary loops are clockwise in parameter space. STEP faces may be written
/// either way round, so reverse the dominant loop of such a face first.
fn orient_boundaries(shell: &mut CompressedShell<Point3, Curve3D, Surface>) {
    for face in &mut shell.faces {
        let mut areas = Vec::with_capacity(face.boundaries.len());
        for boundary in &face.boundaries {
            let mut points = Vec::with_capacity(boundary.len());
            let mut hint = None;
            for edge_index in boundary {
                let edge = &shell.edges[edge_index.index];
                let vertex = if edge_index.orientation {
                    edge.vertices.0
                } else {
                    edge.vertices.1
                };
                let Some(point) = shell.vertices.get(vertex) else {
                    continue;
                };
                let Some(uv) = face.surface.search_nearest_parameter(point, hint, 10) else {
                    continue;
                };
                hint = Some(uv);
                points.push([uv.0, uv.1]);
            }
            areas.push(signed_area(&points));
        }
        if areas.iter().any(|area| *area > 0.0) {
            continue;
        }
        let Some((index, _)) = areas
            .iter()
            .enumerate()
            .max_by(|left, right| left.1.abs().total_cmp(&right.1.abs()))
        else {
            continue;
        };
        if areas[index].abs() < 1e-9 {
            continue;
        }
        let boundary = &mut face.boundaries[index];
        boundary.reverse();
        for edge_index in boundary {
            edge_index.orientation = !edge_index.orientation;
        }
    }
}

fn signed_area(points: &[[f64; 2]]) -> f64 {
    if points.len() < 3 {
        return 0.0;
    }
    let mut area = 0.0;
    for index in 0..points.len() {
        let current = points[index];
        let next = points[(index + 1) % points.len()];
        area += current[0] * next[1] - next[0] * current[1];
    }
    area * 0.5
}

fn append_polygon(
    polygon: &PolygonMesh,
    placement: &Frame,
    scale: f64,
    limit: usize,
    triangles: &mut Vec<[[f64; 3]; 3]>,
) -> Result<(), TessellateFailure> {
    let positions = polygon.positions();
    let mut push = |triangle: [StandardVertex; 3]| -> Result<(), TessellateFailure> {
        if triangles.len() >= limit {
            return Err(TessellateFailure::TooManyTriangles);
        }
        let mut output = [[0.0; 3]; 3];
        for (slot, vertex) in output.iter_mut().zip(triangle) {
            let Some(point) = positions.get(vertex.pos) else {
                return Err(TessellateFailure::Panicked);
            };
            let point = [point.x * scale, point.y * scale, point.z * scale];
            *slot = placement.apply(point);
        }
        triangles.push(output);
        Ok(())
    };
    for triangle in polygon.faces().tri_faces() {
        push(*triangle)?;
    }
    for quad in polygon.faces().quad_faces() {
        push([quad[0], quad[1], quad[2]])?;
        push([quad[0], quad[2], quad[3]])?;
    }
    for face in polygon.faces().other_faces() {
        for index in 1..face.len().saturating_sub(1) {
            push([face[0], face[index], face[index + 1]])?;
        }
    }
    Ok(())
}

/// Decides where every shell in the file sits in model space. Files without an
/// assembly graph return each shell with the identity placement.
fn shell_placements(raw: &RawEntities, table: &Table) -> Vec<(u64, Frame)> {
    let mut all: Vec<u64> = table.shell.keys().copied().collect();
    all.sort_unstable();

    let graph = AssemblyGraph::build(raw);
    if graph.occurrences.is_empty() && graph.representation_shells.is_empty() {
        return all
            .into_iter()
            .map(|shell_id| (shell_id, Frame::IDENTITY))
            .collect();
    }

    let mut placements = Vec::new();
    let mut visited = HashSet::new();
    for root in &graph.roots {
        graph.collect(*root, &Frame::IDENTITY, 0, &mut placements, &mut visited);
    }

    let mut emitted = HashSet::new();
    let mut result = Vec::new();
    for (shell_id, frame) in placements {
        if emitted.insert((shell_id, frame_key(&frame))) {
            result.push((shell_id, frame));
        }
    }

    // Keep shells the assembly graph never reached (e.g. a file that mixes a
    // product structure with loose shells) so nothing silently disappears.
    let placed: HashSet<u64> = result.iter().map(|(shell_id, _)| *shell_id).collect();
    for shell_id in all {
        if !placed.contains(&shell_id) {
            result.push((shell_id, Frame::IDENTITY));
        }
    }
    result
}

fn frame_key(frame: &Frame) -> [u64; 12] {
    let mut key = [0u64; 12];
    for (index, column) in frame.basis.iter().enumerate() {
        for axis in 0..3 {
            key[index * 3 + axis] = column[axis].to_bits();
        }
    }
    for axis in 0..3 {
        key[9 + axis] = frame.origin[axis].to_bits();
    }
    key
}

/// Product/assembly graph extracted from the raw entity table.
#[derive(Default)]
struct AssemblyGraph {
    occurrences: Vec<(u64, u64)>,
    roots: Vec<u64>,
    /// Product definition → representations that carry its geometry.
    product_representations: HashMap<u64, Vec<u64>>,
    /// Representation → shells, with mapped-item placements already applied.
    representation_shells: HashMap<u64, Vec<(u64, Frame)>>,
    /// (child representation, parent representation) → child-to-parent frame.
    representation_transforms: HashMap<(u64, u64), Frame>,
}

impl AssemblyGraph {
    fn build(raw: &RawEntities) -> Self {
        let mut graph = Self::default();
        graph.collect_representations(raw);
        graph.collect_products(raw);
        graph.collect_transforms(raw);
        graph.collect_occurrences(raw);
        graph
    }

    fn collect_representations(&mut self, raw: &RawEntities) {
        for (id, entity) in raw {
            if !matches!(
                entity.name.as_str(),
                "SHAPE_REPRESENTATION"
                    | "ADVANCED_BREP_SHAPE_REPRESENTATION"
                    | "MANIFOLD_SURFACE_SHAPE_REPRESENTATION"
                    | "GEOMETRICALLY_BOUNDED_SURFACE_SHAPE_REPRESENTATION"
                    | "GEOMETRICALLY_BOUNDED_WIREFRAME_SHAPE_REPRESENTATION"
                    | "FACETED_BREP_SHAPE_REPRESENTATION"
            ) {
                continue;
            }
            let Some(items) = entity.parameters.get(1) else {
                continue;
            };
            let mut shells = Vec::new();
            self.collect_items(raw, items, &Frame::IDENTITY, 0, &mut shells);
            self.representation_shells.insert(*id, shells);
        }
    }

    fn collect_items(
        &self,
        raw: &RawEntities,
        items: &Parameter,
        placement: &Frame,
        depth: usize,
        shells: &mut Vec<(u64, Frame)>,
    ) {
        if depth > MAX_ASSEMBLY_DEPTH {
            return;
        }
        match items {
            Parameter::List(items) => {
                for item in items {
                    self.collect_items(raw, item, placement, depth + 1, shells);
                }
            }
            Parameter::Ref(Name::Entity(item_id)) => {
                let Some(entity) = raw.get(item_id) else {
                    return;
                };
                match entity.name.as_str() {
                    "MANIFOLD_SOLID_BREP" => {
                        if let Some(shell) = entity.parameters.get(1).and_then(entity_ref) {
                            shells.push((shell, *placement));
                        }
                    }
                    "BREP_WITH_VOIDS" => {
                        if let Some(shell) = entity.parameters.get(1).and_then(entity_ref) {
                            shells.push((shell, *placement));
                        }
                        if let Some(Parameter::List(voids)) = entity.parameters.get(2) {
                            for void in voids {
                                if let Some(shell) = entity_ref(void) {
                                    shells.push((shell, *placement));
                                }
                            }
                        }
                    }
                    "SHELL_BASED_SURFACE_MODEL" => {
                        if let Some(Parameter::List(shells_list)) = entity.parameters.get(1) {
                            for shell in shells_list {
                                if let Some(shell) = entity_ref(shell) {
                                    shells.push((shell, *placement));
                                }
                            }
                        }
                    }
                    "CLOSED_SHELL" | "OPEN_SHELL" => shells.push((*item_id, *placement)),
                    "MAPPED_ITEM" => {
                        let mapped = mapped_item_frame(raw, entity);
                        let Some(mapping) = entity.parameters.get(1).and_then(entity_ref) else {
                            return;
                        };
                        let Some(mapping_entity) = raw.get(&mapping) else {
                            return;
                        };
                        let Some(mapped_representation) =
                            mapping_entity.parameters.get(1).and_then(entity_ref)
                        else {
                            return;
                        };
                        let Some(children) = self.representation_shells.get(&mapped_representation)
                        else {
                            return;
                        };
                        let combined = placement.then(&mapped);
                        for (shell, local) in children {
                            shells.push((*shell, combined.then(local)));
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn collect_products(&mut self, raw: &RawEntities) {
        // PRODUCT_DEFINITION_SHAPE → SHAPE_DEFINITION_REPRESENTATION → representation.
        let mut shape_to_definition: HashMap<u64, u64> = HashMap::new();
        for (id, entity) in raw {
            if entity.name == "PRODUCT_DEFINITION_SHAPE" {
                if let Some(definition) = entity.parameters.get(2).and_then(entity_ref) {
                    shape_to_definition.insert(*id, definition);
                }
            }
        }
        for entity in raw.values() {
            if entity.name != "SHAPE_DEFINITION_REPRESENTATION" {
                continue;
            }
            // Some exporters include the inherited `name`/`description` fields
            // and some do not, so take the last two entity references.
            let references: Vec<u64> = entity.parameters.iter().filter_map(entity_ref).collect();
            let (Some(definition), Some(representation)) = (
                references.get(references.len().saturating_sub(2)),
                references.get(references.len().saturating_sub(1)),
            ) else {
                continue;
            };
            let product = shape_to_definition
                .get(&definition)
                .copied()
                .unwrap_or(definition);
            self.product_representations
                .entry(product)
                .or_default()
                .push(representation);
        }
    }

    fn collect_transforms(&mut self, raw: &RawEntities) {
        for entity in raw.values() {
            if entity.name != "REPRESENTATION_RELATIONSHIP_WITH_TRANSFORMATION" {
                continue;
            }
            let (Some(first), Some(second), Some(operator)) = (
                entity.parameters.get(2).and_then(entity_ref),
                entity.parameters.get(3).and_then(entity_ref),
                entity.parameters.get(4).and_then(entity_ref),
            ) else {
                continue;
            };
            let Some(operator) = raw.get(&operator) else {
                continue;
            };
            if operator.name != "ITEM_DEFINED_TRANSFORMATION" {
                continue;
            }
            let (Some(item_1), Some(item_2)) = (
                operator
                    .parameters
                    .get(2)
                    .and_then(|value| axis2_frame(raw, value)),
                operator
                    .parameters
                    .get(3)
                    .and_then(|value| axis2_frame(raw, value)),
            ) else {
                continue;
            };
            let Some(inverse) = item_1.inverse() else {
                continue;
            };
            // `ITEM_DEFINED_TRANSFORMATION` maps item_1 in the child representation
            // onto item_2 in the parent representation.
            self.representation_transforms
                .insert((first, second), item_2.then(&inverse));
        }
    }

    fn collect_occurrences(&mut self, raw: &RawEntities) {
        let mut parents = HashSet::new();
        let mut children = HashSet::new();
        for entity in raw.values() {
            if entity.name != "NEXT_ASSEMBLY_USAGE_OCCURRENCE" {
                continue;
            }
            let (Some(parent), Some(child)) = (
                entity.parameters.get(3).and_then(entity_ref),
                entity.parameters.get(4).and_then(entity_ref),
            ) else {
                continue;
            };
            self.occurrences.push((parent, child));
            parents.insert(parent);
            children.insert(child);
        }
        let mut roots: Vec<u64> = parents
            .iter()
            .copied()
            .filter(|parent| !children.contains(parent))
            .collect();
        roots.sort_unstable();
        if roots.is_empty() {
            roots = parents.into_iter().collect();
            roots.sort_unstable();
        }
        if roots.is_empty() {
            // No occurrence at all: every product is its own root.
            roots = self.product_representations.keys().copied().collect();
            roots.sort_unstable();
        }
        self.roots = roots;
    }

    /// Walks the product graph, emitting the shells of every visited product.
    fn collect(
        &self,
        product: u64,
        placement: &Frame,
        depth: usize,
        shells: &mut Vec<(u64, Frame)>,
        visited: &mut HashSet<(u64, [u64; 12])>,
    ) {
        if depth > MAX_ASSEMBLY_DEPTH {
            return;
        }
        for representation in self
            .product_representations
            .get(&product)
            .into_iter()
            .flatten()
        {
            for (shell, local) in self
                .representation_shells
                .get(representation)
                .into_iter()
                .flatten()
            {
                shells.push((*shell, placement.then(local)));
            }
        }
        for (parent, child) in &self.occurrences {
            if *parent != product {
                continue;
            }
            let local = self
                .occurrence_transform(*child, *parent)
                .unwrap_or(Frame::IDENTITY);
            let combined = placement.then(&local);
            if visited.insert((*child, frame_key(&combined))) {
                self.collect(*child, &combined, depth + 1, shells, visited);
            }
        }
    }

    fn occurrence_transform(&self, child: u64, parent: u64) -> Option<Frame> {
        let child_representations = self.product_representations.get(&child)?;
        let parent_representations = self.product_representations.get(&parent)?;
        for child_representation in child_representations {
            for parent_representation in parent_representations {
                if let Some(transform) = self
                    .representation_transforms
                    .get(&(*child_representation, *parent_representation))
                {
                    return Some(*transform);
                }
            }
        }
        None
    }
}

fn mapped_item_frame(raw: &RawEntities, mapped_item: &RawEntity) -> Frame {
    let Some(operator) = mapped_item.parameters.get(2).and_then(entity_ref) else {
        return Frame::IDENTITY;
    };
    let Some(operator) = raw.get(&operator) else {
        return Frame::IDENTITY;
    };
    match operator.name.as_str() {
        "CARTESIAN_TRANSFORMATION_OPERATOR_3D" => {
            let (Some(axis_1), Some(axis_2), Some(origin)) = (
                operator
                    .parameters
                    .get(1)
                    .and_then(|value| direction(raw, value)),
                operator
                    .parameters
                    .get(2)
                    .and_then(|value| direction(raw, value)),
                operator
                    .parameters
                    .get(3)
                    .and_then(|value| point(raw, value)),
            ) else {
                return Frame::IDENTITY;
            };
            let scale = operator.parameters.get(4).and_then(real).unwrap_or(1.0);
            let x = normalize(axis_1).unwrap_or([1.0, 0.0, 0.0]);
            let y = normalize(axis_2).unwrap_or_else(|| perpendicular(x));
            let y = normalize(sub(y, mul(x, dot(y, x)))).unwrap_or_else(|| perpendicular(x));
            let z = cross(x, y);
            Frame {
                basis: [mul(x, scale), mul(y, scale), mul(z, scale)],
                origin,
            }
        }
        "CARTESIAN_TRANSFORMATION_OPERATOR_2D" => {
            let (Some(axis_1), Some(axis_2), Some(origin)) = (
                operator
                    .parameters
                    .get(1)
                    .and_then(|value| direction2(raw, value)),
                operator
                    .parameters
                    .get(2)
                    .and_then(|value| direction2(raw, value)),
                operator
                    .parameters
                    .get(3)
                    .and_then(|value| point2(raw, value)),
            ) else {
                return Frame::IDENTITY;
            };
            let scale = operator.parameters.get(4).and_then(real).unwrap_or(1.0);
            let x = normalize([axis_1[0], axis_1[1], 0.0]).unwrap_or([1.0, 0.0, 0.0]);
            let y = normalize([axis_2[0], axis_2[1], 0.0]).unwrap_or([0.0, 1.0, 0.0]);
            Frame {
                basis: [mul(x, scale), mul(y, scale), [0.0, 0.0, scale]],
                origin: [origin[0], origin[1], 0.0],
            }
        }
        _ => Frame::IDENTITY,
    }
}

fn entity_ref(parameter: &Parameter) -> Option<u64> {
    match parameter {
        Parameter::Ref(Name::Entity(id)) => Some(*id),
        _ => None,
    }
}

fn axis2_frame(raw: &RawEntities, parameter: &Parameter) -> Option<Frame> {
    let entity = raw.get(&entity_ref(parameter)?)?;
    if entity.name != "AXIS2_PLACEMENT_3D" {
        return None;
    }
    let origin = entity
        .parameters
        .get(1)
        .and_then(|value| point(raw, value))?;
    let axis = entity
        .parameters
        .get(2)
        .and_then(|value| direction(raw, value))
        .unwrap_or([0.0, 0.0, 1.0]);
    let reference = entity
        .parameters
        .get(3)
        .and_then(|value| direction(raw, value))
        .unwrap_or_else(|| perpendicular(normalize(axis).unwrap_or([0.0, 0.0, 1.0])));
    Some(Frame::from_axis2(origin, axis, reference))
}

fn direction(raw: &RawEntities, parameter: &Parameter) -> Option<[f64; 3]> {
    let entity = raw.get(&entity_ref(parameter)?)?;
    if entity.name != "DIRECTION" {
        return None;
    }
    let Some(Parameter::List(ratios)) = entity.parameters.get(1) else {
        return None;
    };
    if ratios.len() < 3 {
        return None;
    }
    Some([real(&ratios[0])?, real(&ratios[1])?, real(&ratios[2])?])
}

fn direction2(raw: &RawEntities, parameter: &Parameter) -> Option<[f64; 2]> {
    let entity = raw.get(&entity_ref(parameter)?)?;
    if entity.name != "DIRECTION" {
        return None;
    }
    let Some(Parameter::List(ratios)) = entity.parameters.get(1) else {
        return None;
    };
    if ratios.len() < 2 {
        return None;
    }
    Some([real(&ratios[0])?, real(&ratios[1])?])
}

fn point(raw: &RawEntities, parameter: &Parameter) -> Option<[f64; 3]> {
    let entity = raw.get(&entity_ref(parameter)?)?;
    if entity.name != "CARTESIAN_POINT" {
        return None;
    }
    let Some(Parameter::List(coordinates)) = entity.parameters.get(1) else {
        return None;
    };
    if coordinates.len() < 3 {
        return None;
    }
    Some([
        real(&coordinates[0])?,
        real(&coordinates[1])?,
        real(&coordinates[2])?,
    ])
}

fn point2(raw: &RawEntities, parameter: &Parameter) -> Option<[f64; 2]> {
    let entity = raw.get(&entity_ref(parameter)?)?;
    if entity.name != "CARTESIAN_POINT" {
        return None;
    }
    let Some(Parameter::List(coordinates)) = entity.parameters.get(1) else {
        return None;
    };
    if coordinates.len() < 2 {
        return None;
    }
    Some([real(&coordinates[0])?, real(&coordinates[1])?])
}

fn real(parameter: &Parameter) -> Option<f64> {
    match parameter {
        Parameter::Real(value) => Some(*value),
        Parameter::Integer(value) => Some(*value as f64),
        _ => None,
    }
}

fn enumeration(parameter: &Parameter) -> Option<&str> {
    match parameter {
        Parameter::Enumeration(value) => Some(value),
        _ => None,
    }
}

fn normalize(vector: [f64; 3]) -> Option<[f64; 3]> {
    let length = dot(vector, vector).sqrt();
    if length.is_finite() && length > 1e-12 {
        Some(mul(vector, 1.0 / length))
    } else {
        None
    }
}

fn perpendicular(vector: [f64; 3]) -> [f64; 3] {
    let candidate = if vector[0].abs() < 0.9 {
        [1.0, 0.0, 0.0]
    } else {
        [0.0, 1.0, 0.0]
    };
    normalize(cross(vector, candidate)).unwrap_or([0.0, 0.0, 1.0])
}

fn apply_basis(basis: [[f64; 3]; 3], vector: [f64; 3]) -> [f64; 3] {
    [
        basis[0][0] * vector[0] + basis[1][0] * vector[1] + basis[2][0] * vector[2],
        basis[0][1] * vector[0] + basis[1][1] * vector[1] + basis[2][1] * vector[2],
        basis[0][2] * vector[0] + basis[1][2] * vector[1] + basis[2][2] * vector[2],
    ]
}

fn mul_mat_vec(matrix: [[f64; 3]; 3], vector: [f64; 3]) -> [f64; 3] {
    apply_basis(matrix, vector)
}

fn invert(matrix: [[f64; 3]; 3]) -> Option<[[f64; 3]; 3]> {
    let [a, b, c] = matrix;
    let determinant = dot(a, cross(b, c));
    if !determinant.is_finite() || determinant.abs() < 1e-18 {
        return None;
    }
    let inverse = 1.0 / determinant;
    let row_0 = mul(cross(b, c), inverse);
    let row_1 = mul(cross(c, a), inverse);
    let row_2 = mul(cross(a, b), inverse);
    Some([
        [row_0[0], row_1[0], row_2[0]],
        [row_0[1], row_1[1], row_2[1]],
        [row_0[2], row_1[2], row_2[2]],
    ])
}

fn sub(left: [f64; 3], right: [f64; 3]) -> [f64; 3] {
    [left[0] - right[0], left[1] - right[1], left[2] - right[2]]
}

fn mul(vector: [f64; 3], scalar: f64) -> [f64; 3] {
    [vector[0] * scalar, vector[1] * scalar, vector[2] * scalar]
}

fn dot(left: [f64; 3], right: [f64; 3]) -> f64 {
    left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
}

fn cross(left: [f64; 3], right: [f64; 3]) -> [f64; 3] {
    [
        left[1] * right[2] - left[2] * right[1],
        left[2] * right[0] - left[0] * right[2],
        left[0] * right[1] - left[1] * right[0],
    ]
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn millimetre_unit() -> &'static str {
        "#9000=(LENGTH_UNIT() NAMED_UNIT(*) SI_UNIT(.MILLI.,.METRE.));\n"
    }

    pub(crate) fn inch_unit() -> &'static str {
        "#9000=(LENGTH_UNIT() NAMED_UNIT(*) CONVERSION_BASED_UNIT('INCH',#9001));\n\
         #9001=LENGTH_MEASURE_WITH_UNIT(LENGTH_MEASURE(25.4),#9002);\n\
         #9002=(LENGTH_UNIT() NAMED_UNIT(*) SI_UNIT(.MILLI.,.METRE.));\n"
    }

    struct CubeEntities {
        text: String,
        next_id: u32,
        shell_id: u32,
    }

    /// One closed 10 mm cube starting at `origin`.
    fn cube_entities(mut next_id: u32, origin: [f64; 3]) -> CubeEntities {
        let mut entity = String::new();
        let mut next = || {
            let value = next_id;
            next_id += 1;
            value
        };
        let points = [
            [0.0, 0.0, 0.0],
            [10.0, 0.0, 0.0],
            [10.0, 10.0, 0.0],
            [0.0, 10.0, 0.0],
            [0.0, 0.0, 10.0],
            [10.0, 0.0, 10.0],
            [10.0, 10.0, 10.0],
            [0.0, 10.0, 10.0],
        ]
        .map(|point| {
            [
                point[0] + origin[0],
                point[1] + origin[1],
                point[2] + origin[2],
            ]
        });
        let mut point_ids = Vec::new();
        let mut vertex_ids = Vec::new();
        for [x, y, z] in points {
            let point = next();
            entity.push_str(&format!(
                "#{point}=CARTESIAN_POINT('',({x:.1},{y:.1},{z:.1}));\n"
            ));
            let vertex = next();
            entity.push_str(&format!("#{vertex}=VERTEX_POINT('',#{point});\n"));
            point_ids.push(point);
            vertex_ids.push(vertex);
        }

        let edge_specs: [(usize, usize, [f64; 3]); 12] = [
            (0, 1, [1.0, 0.0, 0.0]),
            (1, 2, [0.0, 1.0, 0.0]),
            (2, 3, [-1.0, 0.0, 0.0]),
            (3, 0, [0.0, -1.0, 0.0]),
            (4, 5, [1.0, 0.0, 0.0]),
            (5, 6, [0.0, 1.0, 0.0]),
            (6, 7, [-1.0, 0.0, 0.0]),
            (7, 4, [0.0, -1.0, 0.0]),
            (0, 4, [0.0, 0.0, 1.0]),
            (1, 5, [0.0, 0.0, 1.0]),
            (2, 6, [0.0, 0.0, 1.0]),
            (3, 7, [0.0, 0.0, 1.0]),
        ];
        let mut curves = Vec::new();
        for (start, end, direction) in edge_specs {
            let [x, y, z] = points[start];
            let direction_id = next();
            entity.push_str(&format!(
                "#{direction_id}=DIRECTION('',({:.1},{:.1},{:.1}));\n",
                direction[0], direction[1], direction[2]
            ));
            let vector_id = next();
            entity.push_str(&format!("#{vector_id}=VECTOR('',#{direction_id},1.0);\n"));
            let point_id = next();
            entity.push_str(&format!(
                "#{point_id}=CARTESIAN_POINT('',({x:.1},{y:.1},{z:.1}));\n"
            ));
            let line_id = next();
            entity.push_str(&format!("#{line_id}=LINE('',#{point_id},#{vector_id});\n"));
            let curve_id = next();
            entity.push_str(&format!(
                "#{curve_id}=EDGE_CURVE('',#{},#{},#{line_id},.T.);\n",
                vertex_ids[start], vertex_ids[end]
            ));
            curves.push(curve_id);
        }

        let loops: [[(usize, bool); 4]; 6] = [
            [(0, true), (1, true), (2, true), (3, true)],
            [(4, true), (5, true), (6, true), (7, true)],
            [(0, true), (9, true), (4, false), (8, false)],
            [(1, true), (10, true), (5, false), (9, false)],
            [(2, true), (11, true), (6, false), (10, false)],
            [(3, true), (8, true), (7, false), (11, false)],
        ];
        let normals: [[f64; 3]; 6] = [
            [0.0, 0.0, 1.0],
            [0.0, 0.0, 1.0],
            [0.0, -1.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [-1.0, 0.0, 0.0],
        ];
        let origins = [0usize, 4, 0, 1, 2, 0];
        let senses = [".F.", ".T.", ".T.", ".T.", ".T.", ".T."];
        let mut faces = Vec::new();
        for (loop_index, edges) in loops.iter().enumerate() {
            let mut oriented = Vec::new();
            for (edge, forward) in edges {
                let oriented_id = next();
                entity.push_str(&format!(
                    "#{oriented_id}=ORIENTED_EDGE('',*,*,#{},{});\n",
                    curves[*edge],
                    if *forward { ".T." } else { ".F." }
                ));
                oriented.push(oriented_id);
            }
            let loop_id = next();
            let list = oriented
                .iter()
                .map(|value| format!("#{value}"))
                .collect::<Vec<_>>()
                .join(",");
            entity.push_str(&format!("#{loop_id}=EDGE_LOOP('',({list}));\n"));
            let bound_id = next();
            entity.push_str(&format!(
                "#{bound_id}=FACE_OUTER_BOUND('',#{loop_id},.T.);\n"
            ));

            let normal = normals[loop_index];
            let direction_id = next();
            entity.push_str(&format!(
                "#{direction_id}=DIRECTION('',({:.1},{:.1},{:.1}));\n",
                normal[0], normal[1], normal[2]
            ));
            let reference = if normal[0].abs() > 0.5 {
                [0.0, 0.0, 1.0]
            } else {
                [1.0, 0.0, 0.0]
            };
            let reference_id = next();
            entity.push_str(&format!(
                "#{reference_id}=DIRECTION('',({:.1},{:.1},{:.1}));\n",
                reference[0], reference[1], reference[2]
            ));
            let placement_id = next();
            entity.push_str(&format!(
                "#{placement_id}=AXIS2_PLACEMENT_3D('',#{},#{direction_id},#{reference_id});\n",
                point_ids[origins[loop_index]]
            ));
            let plane_id = next();
            entity.push_str(&format!("#{plane_id}=PLANE('',#{placement_id});\n"));
            let face_id = next();
            entity.push_str(&format!(
                "#{face_id}=ADVANCED_FACE('',(#{bound_id}),#{plane_id},{});\n",
                senses[loop_index]
            ));
            faces.push(face_id);
        }

        let shell_id = next();
        let face_list = faces
            .iter()
            .map(|value| format!("#{value}"))
            .collect::<Vec<_>>()
            .join(",");
        entity.push_str(&format!("#{shell_id}=CLOSED_SHELL('',({face_list}));\n"));
        CubeEntities {
            text: entity,
            next_id,
            shell_id,
        }
    }

    fn wrap_data(header: &str) -> String {
        format!(
            "ISO-10303-21;\nHEADER;\nFILE_DESCRIPTION((''),'2;1');\n\
             FILE_NAME('mesh.step','2026-01-01T00:00:00',(''),(''),'','','');\n\
             FILE_SCHEMA(('AUTOMOTIVE_DESIGN'));\nENDSEC;\nDATA;\n{header}ENDSEC;\n\
             END-ISO-10303-21;\n"
        )
    }

    pub(crate) fn cube_step(unit: &str) -> String {
        let cube = cube_entities(1, [0.0, 0.0, 0.0]);
        wrap_data(&format!(
            "{unit}#{}=MANIFOLD_SOLID_BREP('',#{});\n{}",
            cube.next_id, cube.shell_id, cube.text
        ))
    }

    fn push_product(
        entities: &mut String,
        next: &mut u32,
        name: &str,
        brep: u32,
        definition_context: u32,
    ) -> (u32, u32) {
        let mut id = || {
            let value = *next;
            *next += 1;
            value
        };
        let product = id();
        let formation = id();
        let definition = id();
        let shape = id();
        let representation = id();
        let context = id();
        entities.push_str(&format!(
            "#{product}=PRODUCT('{name}','{name}','',(#9102));\n"
        ));
        entities.push_str(&format!(
            "#{formation}=PRODUCT_DEFINITION_FORMATION('','',#{product});\n"
        ));
        entities.push_str(&format!(
            "#{definition}=PRODUCT_DEFINITION('design','',#{formation},#{definition_context});\n"
        ));
        entities.push_str(&format!(
            "#{shape}=PRODUCT_DEFINITION_SHAPE('','',#{definition});\n"
        ));
        entities.push_str(&format!(
            "#{context}=GEOMETRIC_REPRESENTATION_CONTEXT(3);\n"
        ));
        entities.push_str(&format!(
            "#{representation}=ADVANCED_BREP_SHAPE_REPRESENTATION('',(#{brep}),#{context});\n"
        ));
        entities.push_str(&format!(
            "#{}=SHAPE_DEFINITION_REPRESENTATION(#{shape},#{representation});\n",
            id()
        ));
        (definition, representation)
    }

    fn assembly_step(unit: &str) -> String {
        let parent = cube_entities(1, [0.0, 0.0, 0.0]);
        let child = cube_entities(parent.next_id, [0.0, 0.0, 0.0]);
        let mut next = child.next_id;
        fn next_id(next: &mut u32) -> u32 {
            let value = *next;
            *next += 1;
            value
        }
        let parent_brep = next_id(&mut next);
        let child_brep = next_id(&mut next);
        let mut entities = String::new();
        entities.push_str("#9100=APPLICATION_CONTEXT('automotive design');\n");
        entities.push_str("#9102=PRODUCT_CONTEXT('',#9100,'mechanical');\n");
        entities.push_str("#9101=PRODUCT_DEFINITION_CONTEXT('design',#9100,'design');\n");
        entities.push_str(&format!(
            "#{parent_brep}=MANIFOLD_SOLID_BREP('',#{});\n",
            parent.shell_id
        ));
        entities.push_str(&format!(
            "#{child_brep}=MANIFOLD_SOLID_BREP('',#{});\n",
            child.shell_id
        ));
        let (parent_definition, parent_representation) =
            push_product(&mut entities, &mut next, "parent", parent_brep, 9101);
        let (child_definition, child_representation) =
            push_product(&mut entities, &mut next, "child", child_brep, 9101);

        entities.push_str(&format!(
            "#{}=NEXT_ASSEMBLY_USAGE_OCCURRENCE('1','child','',#{parent_definition},#{child_definition},$);\n",
            next_id(&mut next)
        ));
        let identity_point = next_id(&mut next);
        let direction = next_id(&mut next);
        let reference = next_id(&mut next);
        entities.push_str(&format!(
            "#{identity_point}=CARTESIAN_POINT('',(0.0,0.0,0.0));\n"
        ));
        entities.push_str(&format!("#{direction}=DIRECTION('',(0.0,0.0,1.0));\n"));
        entities.push_str(&format!("#{reference}=DIRECTION('',(1.0,0.0,0.0));\n"));
        let child_placement = next_id(&mut next);
        let parent_placement = next_id(&mut next);
        entities.push_str(&format!(
            "#{child_placement}=AXIS2_PLACEMENT_3D('',#{identity_point},#{direction},#{reference});\n"
        ));
        let translated = next_id(&mut next);
        entities.push_str(&format!(
            "#{translated}=CARTESIAN_POINT('',(20.0,0.0,0.0));\n"
        ));
        entities.push_str(&format!(
            "#{parent_placement}=AXIS2_PLACEMENT_3D('',#{translated},#{direction},#{reference});\n"
        ));
        let operator = next_id(&mut next);
        entities.push_str(&format!(
            "#{operator}=ITEM_DEFINED_TRANSFORMATION('','',#{child_placement},#{parent_placement});\n"
        ));
        entities.push_str(&format!(
            "#{}=REPRESENTATION_RELATIONSHIP_WITH_TRANSFORMATION('','',#{child_representation},#{parent_representation},#{operator});\n",
            next_id(&mut next)
        ));

        wrap_data(&format!("{unit}{entities}{}{}", parent.text, child.text))
    }

    fn bounds(triangles: &[[[f64; 3]; 3]]) -> ([f64; 3], [f64; 3]) {
        let mut minimum = [f64::INFINITY; 3];
        let mut maximum = [f64::NEG_INFINITY; 3];
        for triangle in triangles {
            for point in triangle {
                for axis in 0..3 {
                    minimum[axis] = minimum[axis].min(point[axis]);
                    maximum[axis] = maximum[axis].max(point[axis]);
                }
            }
        }
        (minimum, maximum)
    }

    fn surface_area(triangles: &[[[f64; 3]; 3]]) -> f64 {
        triangles
            .iter()
            .map(|[a, b, c]| {
                let normal = cross(sub(*b, *a), sub(*c, *a));
                0.5 * dot(normal, normal).sqrt()
            })
            .sum()
    }

    #[test]
    fn parses_a_closed_step_cube() {
        let mesh = parse(cube_step(millimetre_unit()).as_bytes()).expect("cube");
        assert_eq!(mesh.unit, StepUnit::Millimetre);
        assert_eq!(mesh.solids, 1);
        assert_eq!(mesh.triangles.len(), 12);
        assert!((surface_area(&mesh.triangles) - 600.0).abs() < 1e-6);
        let (minimum, maximum) = bounds(&mesh.triangles);
        assert_eq!(minimum, [0.0, 0.0, 0.0]);
        assert_eq!(maximum, [10.0, 10.0, 10.0]);
    }

    #[test]
    fn converts_inch_units_to_millimetres() {
        let mesh = parse(cube_step(inch_unit()).as_bytes()).expect("inch cube");
        assert_eq!(mesh.unit, StepUnit::Inch);
        let (_, maximum) = bounds(&mesh.triangles);
        assert!((maximum[0] - 254.0).abs() < 1e-6, "max {maximum:?}");
        assert!((surface_area(&mesh.triangles) - 600.0 * 25.4 * 25.4).abs() < 1e-3);
    }

    #[test]
    fn applies_assembly_transforms_once_per_instance() {
        let text = assembly_step(millimetre_unit());
        let mesh = parse(text.as_bytes()).expect("assembly");
        assert_eq!(mesh.solids, 2);
        assert_eq!(mesh.triangles.len(), 24);
        let (minimum, maximum) = bounds(&mesh.triangles);
        assert_eq!(minimum, [0.0, 0.0, 0.0]);
        assert_eq!(maximum, [30.0, 10.0, 10.0]);
    }

    #[test]
    fn reports_invalid_step_input() {
        assert!(parse(b"not a step file").is_err());
        assert!(parse(b"\xFF\xFE").is_err());
        let empty = wrap_data("");
        assert!(parse(empty.as_bytes()).is_err());
    }

    #[test]
    fn empty_polygon_adds_no_triangles() {
        let polygon = PolygonMesh::default();
        let mut triangles = Vec::new();
        assert!(append_polygon(&polygon, &Frame::IDENTITY, 1.0, 0, &mut triangles).is_ok());
        assert!(triangles.is_empty());
    }
}

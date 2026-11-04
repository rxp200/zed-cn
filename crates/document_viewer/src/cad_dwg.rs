//! Native reader for DWG drawings (AutoCAD 2000 to 2018).
//!
//! The DWG container is decoded by `opendwg` into its typed object model and
//! flattened into [`CadScene`] here, with the same layer, block, text and color
//! semantics as the DXF reader. Blocks are expanded on the fly instead of being
//! pre-flattened, so memory follows the scene budgets rather than the object
//! count. Unsupported entity types are counted instead of failing.

use std::collections::{HashMap, HashSet};
use std::fmt;

use gpui::SharedString;
use opendwg::object::{
    BoundaryPath, Entity, EntityList, Hatch, HatchEdge, Insert, Layer, PathEdges, Polyline2d,
    Spline, SplineForm, Vertex2d, Vertex3d,
};
use opendwg::{Color, Drawing, Error as DwgError, Handle};

use crate::cad_dxf::{Transform, insertion_transform, strip_text_codes, vertices_to_points};
use crate::cad_scene::{
    ACI_FOREGROUND, COLOR_INHERIT, CadColor, CadLayer, CadScene, MAX_INSERT_DEPTH, encode_aci,
    encode_truecolor, push_arc_points, push_ellipse_points,
};

/// Why a DWG file cannot be previewed at all.
#[derive(Debug)]
pub enum CadDwgError {
    /// A DWG version this reader does not decode (R12 to R14 and anything
    /// newer than R2018).
    UnsupportedVersion(SharedString),
    /// A password-protected drawing.
    Encrypted,
    /// The container or one of its sections is damaged or incomplete.
    Corrupt,
    /// The drawing parsed, but holds nothing that can be drawn.
    Empty,
}

impl CadDwgError {
    fn from_opendwg(error: DwgError) -> Self {
        match error {
            DwgError::UnsupportedVersion(version) => {
                Self::UnsupportedVersion(release_name(&version).into())
            }
            DwgError::Encrypted(_) => Self::Encrypted,
            _ => Self::Corrupt,
        }
    }
}

impl fmt::Display for CadDwgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion(version) => {
                f.write_str(&i18n::t_args!("90add587c97ee314", version))
            }
            Self::Encrypted => f.write_str(&i18n::t!("a21f8cb7c2076d33")),
            Self::Corrupt => f.write_str(&i18n::t!("3bb9251f6e5caa6a")),
            Self::Empty => f.write_str(&i18n::t!("96ab902f5adf7da4")),
        }
    }
}

impl std::error::Error for CadDwgError {}

/// Whether these bytes start with a DWG version sentinel.
///
/// DXF drawings are text, so a file can be identified by its content instead of
/// its extension: a DWG renamed to `.dxf` still opens, and a DXF renamed to
/// `.dwg` falls through to the DXF parser.
pub fn is_dwg(bytes: &[u8]) -> bool {
    let magic = &bytes[..bytes.len().min(6)];
    magic.len() == 6 && &magic[0..2] == b"AC" && magic[2..6].iter().all(u8::is_ascii_digit)
}

/// A readable release name for a version sentinel the decoder rejected.
fn release_name(version: &str) -> String {
    match version {
        "AC1009" => "R12 (AC1009)".to_owned(),
        "AC1012" => "R13 (AC1012)".to_owned(),
        "AC1014" => "R14 (AC1014)".to_owned(),
        other => other.to_owned(),
    }
}

/// Parses DWG bytes into a renderable scene.
pub fn parse(bytes: &[u8]) -> Result<CadScene, CadDwgError> {
    let drawing = Drawing::from_bytes(bytes).map_err(CadDwgError::from_opendwg)?;
    let mut reader = Reader::new(&drawing);
    reader.run();
    reader.finish()
}

struct Reader<'a> {
    drawing: &'a Drawing,
    scene: CadScene,
    layers: HashMap<Handle, u16>,
    /// Block records that belong to a paper-space layout.
    paper_space: HashSet<Handle>,
    /// Rays and xlines, clipped once the drawing bounds are known.
    infinite: Vec<([f64; 2], [f64; 2], u16, CadColor)>,
}

impl<'a> Reader<'a> {
    fn new(drawing: &'a Drawing) -> Self {
        Self {
            drawing,
            scene: CadScene::new(),
            layers: HashMap::new(),
            paper_space: paper_space_blocks(drawing),
            infinite: Vec::new(),
        }
    }

    fn run(&mut self) {
        let drawing = self.drawing;
        self.scene.units = Some(i32::from(drawing.header.insunits)).filter(|units| *units >= 0);
        self.scene.acad_version = drawing.source_version().map(|version| {
            SharedString::from(String::from_utf8_lossy(version.magic()).into_owned())
        });
        self.scene.code_page = opendwg::strings::encoding_for(drawing.header.dwgcodepage)
            .map(|encoding| SharedString::from(encoding.name().to_owned()));

        for (handle, layer) in drawing.layers() {
            let index = self.push_layer(layer);
            self.layers.insert(handle, index);
        }
        if self.scene.layers.is_empty() {
            // Every drawing has a `0` layer; a damaged table must not leave
            // entity layer lookups without a fallback.
            self.scene.push_layer(CadLayer {
                name: SharedString::from("0"),
                color: encode_aci(ACI_FOREGROUND),
                frozen: false,
                visible: true,
            });
        }

        // Paper-space entities are not drawn, but the status bar reports how
        // many were skipped, the same way the DXF reader counts group 67.
        for block in &self.paper_space {
            let count = drawing.entities_in(*block).count();
            self.scene.paper_space_entities = self
                .scene
                .paper_space_entities
                .saturating_add(u32::try_from(count).unwrap_or(u32::MAX));
        }

        let model_space = drawing.model_space();
        self.walk_block(model_space, Transform::identity(), None, COLOR_INHERIT, 0);
    }

    /// Walks every entity owned by `block`, applying the transform its insert
    /// established.
    fn walk_block(
        &mut self,
        block: Handle,
        transform: Transform,
        insert_layer: Option<u16>,
        block_color: CadColor,
        depth: u8,
    ) {
        if self.paper_space.contains(&block) {
            return;
        }
        let drawing = self.drawing;
        for handle in drawing.entities_in(block) {
            self.walk_entity(handle, &transform, insert_layer, block_color, depth);
        }
    }

    fn walk_entity(
        &mut self,
        handle: Handle,
        transform: &Transform,
        insert_layer: Option<u16>,
        block_color: CadColor,
        depth: u8,
    ) {
        let drawing = self.drawing;
        let Some((common, entity)) = drawing.entity(handle) else {
            return;
        };
        if common.invisible {
            return;
        }
        let layer = self.layer_index(common.layer, insert_layer);
        let color = entity_color(common.color.clone(), block_color);

        match entity {
            Entity::Line(line) => {
                let points = [[line.start[0], line.start[1]], [line.end[0], line.end[1]]];
                self.emit_path(&points, false, transform, layer, color);
            }
            Entity::Point(point) => {
                let point = [point.position[0], point.position[1]];
                self.emit_path(&[point, point], false, transform, layer, color);
            }
            Entity::Circle(circle) => {
                let center = [circle.center[0], circle.center[1]];
                let radius = circle.radius;
                if radius <= 0.0 {
                    return;
                }
                let first = [center[0] + radius, center[1]];
                let mut points = Vec::with_capacity(80);
                points.push(first);
                push_arc_points(&mut points, center, radius, 0.0, std::f64::consts::TAU);
                if let Some(last) = points.last_mut() {
                    *last = first;
                }
                self.emit_path(&points, true, transform, layer, color);
            }
            Entity::Arc(arc) => {
                let center = [arc.circle.center[0], arc.circle.center[1]];
                let radius = arc.circle.radius;
                if radius <= 0.0 {
                    return;
                }
                let mut sweep = arc.end_angle - arc.start_angle;
                // DWG arcs run counter-clockwise from the start angle, so a
                // non-positive sweep is a full turn.
                while sweep <= 1e-12 {
                    sweep += std::f64::consts::TAU;
                }
                let mut points = Vec::with_capacity(80);
                points.push([
                    center[0] + radius * arc.start_angle.cos(),
                    center[1] + radius * arc.start_angle.sin(),
                ]);
                push_arc_points(&mut points, center, radius, arc.start_angle, sweep);
                self.emit_path(&points, false, transform, layer, color);
            }
            Entity::Ellipse(ellipse) => {
                let center = [ellipse.center[0], ellipse.center[1]];
                let major = [ellipse.major_axis[0], ellipse.major_axis[1]];
                let ratio = ellipse.axis_ratio;
                if ratio <= 0.0 {
                    return;
                }
                let mut sweep = ellipse.end_angle - ellipse.start_angle;
                if sweep.abs() < 1e-12 {
                    sweep = std::f64::consts::TAU;
                }
                let (sin, cos) = ellipse.start_angle.sin_cos();
                let minor = [-major[1] * ratio, major[0] * ratio];
                let mut points = vec![[
                    center[0] + major[0] * cos + minor[0] * sin,
                    center[1] + major[1] * cos + minor[1] * sin,
                ]];
                push_ellipse_points(
                    &mut points,
                    center,
                    major,
                    ratio,
                    ellipse.start_angle,
                    sweep,
                );
                let closed = sweep.abs() >= std::f64::consts::TAU - 1e-9;
                if closed {
                    let first = points[0];
                    if let Some(last) = points.last_mut() {
                        *last = first;
                    }
                }
                self.emit_path(&points, closed, transform, layer, color);
            }
            Entity::LwPolyline(polyline) => {
                let vertices: Vec<(f64, f64, f64)> = polyline
                    .points
                    .iter()
                    .enumerate()
                    .map(|(index, point)| {
                        let bulge = polyline.bulges.get(index).copied().unwrap_or(0.0);
                        (point[0], point[1], bulge)
                    })
                    .collect();
                let closed = polyline.closed();
                let points = vertices_to_points(&vertices, closed);
                if points.len() >= 2 {
                    self.emit_path(&points, closed, transform, layer, color);
                }
            }
            Entity::Polyline2d(polyline) => {
                let vertices = self.polyline_vertices(polyline);
                let closed = polyline.flags & 1 != 0;
                let points = vertices_to_points(&vertices, closed);
                if points.len() >= 2 {
                    self.emit_path(&points, closed, transform, layer, color);
                }
            }
            Entity::Polyline3d(polyline) => {
                let drawing = self.drawing;
                let points: Vec<[f64; 2]> = owned_handles(&polyline.vertices)
                    .filter_map(|handle| match drawing.entity(handle) {
                        Some((_, Entity::Vertex3d(Vertex3d { point, .. }))) => {
                            Some([point[0], point[1]])
                        }
                        _ => None,
                    })
                    .collect();
                if points.len() >= 2 {
                    let closed = polyline.closed_flags & 1 != 0;
                    self.emit_path(&points, closed, transform, layer, color);
                }
            }
            Entity::Spline(spline) => {
                if let Some(points) = spline_points(spline) {
                    self.emit_path(&points, false, transform, layer, color);
                }
            }
            Entity::Solid(solid) => {
                let [first, second, third, fourth] = solid.corners;
                // A triangular solid repeats its last corner; the stored order
                // visits corners 3 and 4 swapped, as in DXF.
                let mut corners = vec![first, second, fourth, third];
                if corners[2] == corners[3] {
                    corners.pop();
                }
                self.emit_fill(&corners, transform, layer, color);
            }
            Entity::Trace(trace) => {
                let [first, second, third, fourth] = trace.corners;
                let mut corners = vec![first, second, fourth, third];
                if corners[2] == corners[3] {
                    corners.pop();
                }
                self.emit_fill(&corners, transform, layer, color);
            }
            Entity::Face3d(face) => {
                let mut corners: Vec<[f64; 2]> = face
                    .corners
                    .iter()
                    .map(|corner| [corner[0], corner[1]])
                    .collect();
                if corners.get(2) == corners.get(3) {
                    corners.pop();
                }
                self.emit_fill(&corners, transform, layer, color);
            }
            Entity::Text(text) => {
                let position = text
                    .alignment
                    .unwrap_or([text.insertion[0], text.insertion[1]]);
                let value = strip_text_codes(&text.value);
                self.emit_text(
                    &value,
                    position,
                    text.height,
                    text.rotation,
                    transform,
                    layer,
                    color,
                );
            }
            Entity::MText(text) => {
                let position = [text.insertion[0], text.insertion[1]];
                let rotation = text.x_axis_dir[1].atan2(text.x_axis_dir[0]);
                let value = strip_text_codes(&text.contents);
                self.emit_text(
                    &value,
                    position,
                    text.text_height,
                    rotation,
                    transform,
                    layer,
                    color,
                );
            }
            Entity::Insert(insert) => {
                self.expand_insert(insert, transform, layer, color, depth);
            }
            Entity::MInsert(array) => {
                self.expand_array(array, transform, layer, color, depth);
            }
            Entity::Dimension(dimension) => {
                // A dimension keeps its geometry in an anonymous block that is
                // already in drawing coordinates, so it takes the transform of
                // the block it belongs to as it is.
                let geometry = dimension.common.block;
                let block_color = resolved_block_color(&self.scene, layer, color);
                self.walk_block(
                    geometry,
                    *transform,
                    Some(layer),
                    block_color,
                    depth.saturating_add(1),
                );
            }
            Entity::Leader(leader) => {
                let points: Vec<[f64; 2]> = leader
                    .points
                    .iter()
                    .map(|point| [point[0], point[1]])
                    .collect();
                if points.len() >= 2 {
                    self.emit_path(&points, false, transform, layer, color);
                }
            }
            Entity::Hatch(hatch) => {
                self.emit_hatch(hatch, transform, layer, color);
            }
            Entity::Ray(ray) | Entity::Xline(ray) => {
                let direction = [ray.vector[0], ray.vector[1]];
                let length = direction[0].hypot(direction[1]);
                if !length.is_finite() || length <= 0.0 {
                    return;
                }
                self.infinite.push((
                    transform.apply([ray.point[0], ray.point[1]]),
                    transform.apply_direction([direction[0] / length, direction[1] / length]),
                    layer,
                    color,
                ));
            }
            Entity::Attrib(_)
            | Entity::AttDef(_)
            | Entity::SeqEnd
            | Entity::Block(_)
            | Entity::EndBlk
            | Entity::Viewport(_) => {}
            other => self.scene.note_skipped(entity_type_name(other)),
        }
    }

    /// Vertices of a 2D polyline as `(x, y, bulge)` tuples.
    fn polyline_vertices(&self, polyline: &Polyline2d) -> Vec<(f64, f64, f64)> {
        let drawing = self.drawing;
        owned_handles(&polyline.vertices)
            .filter_map(|handle| match drawing.entity(handle) {
                Some((_, Entity::Vertex2d(Vertex2d { point, bulge, .. }))) => {
                    Some((point[0], point[1], *bulge))
                }
                _ => None,
            })
            .collect()
    }

    fn expand_insert(
        &mut self,
        insert: &Insert,
        transform: &Transform,
        layer: u16,
        color: CadColor,
        depth: u8,
    ) {
        let drawing = self.drawing;
        let Some(block) = drawing.block_record(insert.block) else {
            self.scene.note_skipped("INSERT target");
            return;
        };
        if depth >= MAX_INSERT_DEPTH {
            self.scene.note_skipped("INSERT nesting");
            return;
        }
        if self.paper_space.contains(&insert.block) || block.is_xref {
            return;
        }
        let base = [block.base_point[0], block.base_point[1]];
        let local = insertion_transform(
            base,
            [insert.insertion[0], insert.insertion[1]],
            [insert.scale[0], insert.scale[1]],
            insert.rotation,
        );
        let composed = local.then(transform);
        let block_color = resolved_block_color(&self.scene, layer, color);
        // Block geometry on layer `0` takes the layer of the insert that
        // references it.
        self.walk_block(
            insert.block,
            composed,
            Some(layer),
            block_color,
            depth.saturating_add(1),
        );
    }

    /// EXPAND MINSERT: the same block at every row and column of the array.
    fn expand_array(
        &mut self,
        array: &opendwg::object::MInsert,
        transform: &Transform,
        layer: u16,
        color: CadColor,
        depth: u8,
    ) {
        let columns = array.columns.max(1);
        let rows = array.rows.max(1);
        let (sin, cos) = array.insert.rotation.sin_cos();
        for row in 0..rows {
            for column in 0..columns {
                let offset = [
                    f64::from(column) * array.column_spacing,
                    f64::from(row) * array.row_spacing,
                ];
                let mut cell = array.insert.clone();
                cell.insertion[0] += offset[0] * cos - offset[1] * sin;
                cell.insertion[1] += offset[0] * sin + offset[1] * cos;
                self.expand_insert(&cell, transform, layer, color, depth);
            }
        }
    }

    fn emit_hatch(&mut self, hatch: &Hatch, transform: &Transform, layer: u16, color: CadColor) {
        let mut loops: Vec<Vec<[f64; 2]>> = Vec::new();
        for path in &hatch.paths {
            let points = hatch_path_points(path);
            if points.len() >= 2 {
                loops.push(points);
            }
        }
        if loops.is_empty() {
            return;
        }
        let solid = hatch.pattern.is_none() || hatch.pattern_name.eq_ignore_ascii_case("SOLID");
        if solid && loops.len() == 1 {
            // Islands cannot be punched out of a filled polygon, so a fill with
            // inner loops is drawn as outlines instead of hiding its islands.
            self.emit_fill(&loops[0], transform, layer, color);
        } else if solid {
            self.emit_fill(&loops[0], transform, layer, color);
            for points in &loops[1..] {
                self.emit_path(points, true, transform, layer, color);
            }
        } else {
            for points in &loops {
                self.emit_path(points, true, transform, layer, color);
            }
        }
    }

    fn emit_path(
        &mut self,
        points: &[[f64; 2]],
        closed: bool,
        transform: &Transform,
        layer: u16,
        color: CadColor,
    ) {
        let points: Vec<[f64; 2]> = points.iter().map(|point| transform.apply(*point)).collect();
        self.scene.push_path(&points, closed, layer, color);
    }

    fn emit_fill(
        &mut self,
        points: &[[f64; 2]],
        transform: &Transform,
        layer: u16,
        color: CadColor,
    ) {
        let points: Vec<[f64; 2]> = points.iter().map(|point| transform.apply(*point)).collect();
        self.scene.push_fill(&points, layer, color);
    }

    fn emit_text(
        &mut self,
        text: &str,
        position: [f64; 2],
        height: f64,
        rotation: f64,
        transform: &Transform,
        layer: u16,
        color: CadColor,
    ) {
        if text.trim().is_empty() {
            return;
        }
        self.scene.push_text(
            text,
            transform.apply(position),
            if height > 0.0 {
                height * transform.text_scale()
            } else {
                0.0
            },
            rotation + transform.rotation(),
            layer,
            color,
        );
    }

    /// Resolves the entity's layer handle, creating the implicit layers a
    /// damaged table or a missing layer object would otherwise leave dangling.
    fn layer_index(&mut self, layer: Handle, insert_layer: Option<u16>) -> u16 {
        let index = match self.layers.get(&layer).copied() {
            Some(index) => index,
            None => match self.scene.zero_layer() {
                Some(zero) => zero,
                None => self.scene.push_layer(CadLayer {
                    name: SharedString::from("0"),
                    color: encode_aci(ACI_FOREGROUND),
                    frozen: false,
                    visible: true,
                }),
            },
        };
        if Some(index) == self.scene.zero_layer() {
            insert_layer.unwrap_or(index)
        } else {
            index
        }
    }

    fn push_layer(&mut self, layer: &Layer) -> u16 {
        let frozen = layer.frozen;
        self.scene.push_layer(CadLayer {
            name: SharedString::from(layer.entry.name.clone()),
            color: layer_color(layer.color.clone()),
            frozen,
            visible: !layer.off && !frozen,
        })
    }

    fn finish(mut self) -> Result<CadScene, CadDwgError> {
        self.scene
            .push_infinite_lines(std::mem::take(&mut self.infinite));
        if self.scene.is_empty() {
            return Err(CadDwgError::Empty);
        }
        Ok(self.scene)
    }
}

/// The block records that belong to the paper-space layouts.
fn paper_space_blocks(drawing: &Drawing) -> HashSet<Handle> {
    let mut blocks: HashSet<Handle> = drawing
        .layouts()
        .into_iter()
        .filter(|(_, layout)| !layout.name.eq_ignore_ascii_case("Model"))
        .map(|(_, layout)| layout.block_record)
        .collect();
    blocks.extend(
        drawing
            .block_records()
            .filter(|(_, record)| record.entry.name.starts_with("*Paper_Space"))
            .map(|(handle, _)| handle),
    );
    blocks
}

/// The handles of an entity list. Reads always leave these as owned lists; a
/// linked list from a partially read drawing has none to offer here.
fn owned_handles(list: &EntityList) -> impl Iterator<Item = Handle> + '_ {
    match list {
        EntityList::Owned(handles) => handles.as_slice(),
        EntityList::Linked { .. } => &[],
    }
    .iter()
    .copied()
}

fn layer_color(color: Color) -> CadColor {
    match color {
        Color::Index(index) => encode_aci(i16::from(index)),
        Color::Rgb { r, g, b } => encode_truecolor(u32::from_be_bytes([0, r, g, b])),
        Color::Book { rgb, .. } => encode_truecolor(rgb & 0x00ff_ffff),
        _ => encode_aci(ACI_FOREGROUND),
    }
}

/// An entity color as a scene color. [`COLOR_INHERIT`] means the layer color,
/// and is also what a ByBlock entity without a reference keeps.
fn entity_color(color: Color, block_color: CadColor) -> CadColor {
    match color {
        Color::ByLayer => COLOR_INHERIT,
        Color::ByBlock => block_color,
        Color::Index(index) => encode_aci(i16::from(index)),
        Color::Rgb { r, g, b } => encode_truecolor(u32::from_be_bytes([0, r, g, b])),
        Color::Book { rgb, .. } => encode_truecolor(rgb & 0x00ff_ffff),
        _ => COLOR_INHERIT,
    }
}

/// The color block entities inherit through ByBlock: the insert's own color,
/// resolved to the insert layer color when the insert itself is ByLayer.
fn resolved_block_color(scene: &CadScene, layer: u16, color: CadColor) -> CadColor {
    if color == COLOR_INHERIT {
        scene.layer(layer).color
    } else {
        color
    }
}

fn spline_points(spline: &Spline) -> Option<Vec<[f64; 2]>> {
    let points: Vec<[f64; 2]> = match &spline.form {
        // Exact NURBS evaluation is out of scope, as in the DXF reader: fit
        // points describe the drawn curve closely, control points are the
        // fallback.
        SplineForm::Fit(fit) => fit.points.iter().map(|p| [p[0], p[1]]).collect(),
        SplineForm::Control(control) => control.points.iter().map(|p| [p[0], p[1]]).collect(),
    };
    (points.len() >= 2).then_some(points)
}

/// Stitches a hatch boundary path into one polyline.
fn hatch_path_points(path: &BoundaryPath) -> Vec<[f64; 2]> {
    match &path.edges {
        PathEdges::Polyline(polyline) => {
            let vertices: Vec<(f64, f64, f64)> = polyline
                .vertices
                .iter()
                .map(|vertex| (vertex.point[0], vertex.point[1], vertex.bulge))
                .collect();
            vertices_to_points(&vertices, true)
        }
        PathEdges::Edges(edges) => {
            let mut points: Vec<[f64; 2]> = Vec::new();
            for edge in edges {
                match edge {
                    HatchEdge::Line { start, end } => {
                        push_once(&mut points, *start);
                        push_once(&mut points, *end);
                    }
                    HatchEdge::CircularArc {
                        center,
                        radius,
                        start_angle,
                        end_angle,
                        ccw,
                    } => {
                        push_once(&mut points, arc_start(*center, *radius, *start_angle));
                        let sweep = arc_sweep(*start_angle, *end_angle, *ccw);
                        push_arc_points(&mut points, *center, *radius, *start_angle, sweep);
                    }
                    HatchEdge::EllipticArc {
                        center,
                        major_end,
                        ratio,
                        start_angle,
                        end_angle,
                        ccw,
                    } => {
                        let sweep = arc_sweep(*start_angle, *end_angle, *ccw);
                        let minor = [-major_end[1] * ratio, major_end[0] * ratio];
                        let (sin, cos) = start_angle.sin_cos();
                        push_once(
                            &mut points,
                            [
                                center[0] + major_end[0] * cos + minor[0] * sin,
                                center[1] + major_end[1] * cos + minor[1] * sin,
                            ],
                        );
                        push_ellipse_points(
                            &mut points,
                            *center,
                            *major_end,
                            *ratio,
                            *start_angle,
                            sweep,
                        );
                    }
                    HatchEdge::Spline(spline) => {
                        let source: &[[f64; 2]] = if spline.fit_points.len() >= 2 {
                            spline.fit_points.as_slice()
                        } else {
                            &[]
                        };
                        if source.is_empty() {
                            for (point, _) in &spline.control_points {
                                push_once(&mut points, *point);
                            }
                        } else {
                            for point in source {
                                push_once(&mut points, *point);
                            }
                        }
                    }
                }
            }
            if points.len() >= 2 {
                points.push(points[0]);
            }
            points
        }
    }
}

fn arc_start(center: [f64; 2], radius: f64, angle: f64) -> [f64; 2] {
    [
        center[0] + radius * angle.cos(),
        center[1] + radius * angle.sin(),
    ]
}

/// The sweep from `start` to `end` in the direction the edge was drawn.
fn arc_sweep(start: f64, end: f64, ccw: bool) -> f64 {
    let mut sweep = end - start;
    if ccw {
        while sweep <= 0.0 {
            sweep += std::f64::consts::TAU;
        }
    } else {
        while sweep >= 0.0 {
            sweep -= std::f64::consts::TAU;
        }
    }
    sweep
}

/// Appends a point unless it repeats the previous one, so stitched edges share
/// their endpoints exactly.
fn push_once(points: &mut Vec<[f64; 2]>, point: [f64; 2]) {
    if points.last() != Some(&point) {
        points.push(point);
    }
}

fn entity_type_name(entity: &Entity) -> &'static str {
    match entity {
        Entity::Unknown(_) => "UNKNOWN",
        Entity::Shape(_) => "SHAPE",
        Entity::MLine(_) => "MLINE",
        Entity::Tolerance(_) => "TOLERANCE",
        Entity::MultiLeader(_) => "MULTILEADER",
        Entity::Image(_) => "IMAGE",
        Entity::Wipeout(_) => "WIPEOUT",
        Entity::PolyfaceMesh(_) => "POLYFACE",
        Entity::PolygonMesh(_) => "POLYGONMESH",
        Entity::Acis(_) => "ACIS",
        Entity::ProxyEntity(_) => "PROXY",
        Entity::Ole2Frame(_) => "OLE2FRAME",
        _ => "ENTITY",
    }
}

#[cfg(test)]
pub(crate) fn minimal_dwg() -> Vec<u8> {
    use opendwg::object::{Entity, EntityCommon, Line};

    let mut drawing = Drawing::new();
    let mut layer = Layer::new("WALLS");
    layer.color = Color::Index(1);
    let walls = drawing.add_layer(layer).expect("walls layer");
    let model = drawing.model_space();
    let mut common = EntityCommon::new(walls);
    common.color = Color::Index(1);
    drawing
        .add_entity(
            model,
            common,
            Entity::Line(Line::new([0.0, 0.0, 0.0], [10.0, 5.0, 0.0])),
        )
        .expect("line");
    drawing.to_bytes().expect("dwg bytes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use opendwg::object::{
        Arc, Circle, Entity, EntityCommon, LWPOLYLINE_CLOSED, Line, LwPolyline, Text,
    };

    fn drawing_with_line(rotation: f64, scale: f64) -> Drawing {
        let mut drawing = Drawing::new();
        let layer = drawing.layer_by_name("0").expect("layer 0");
        let block = drawing.add_block("PART").expect("block");
        drawing
            .add_entity(
                block,
                EntityCommon::new(layer),
                Entity::Line(Line::new([0.0, 0.0, 0.0], [1.0, 0.0, 0.0])),
            )
            .expect("block line");
        let model = drawing.model_space();
        let mut insert = Insert::new(block, [10.0, 20.0, 0.0]);
        insert.rotation = rotation;
        insert.scale = [scale; 3];
        let entity = Entity::Insert(insert);
        drawing
            .add_entity(model, EntityCommon::new(layer), entity)
            .expect("insert");
        drawing
    }

    #[test]
    fn version_sentinels_identify_the_container() {
        assert!(is_dwg(b"AC1015\x00\x00"));
        assert!(is_dwg(b"AC1032more"));
        assert!(!is_dwg(b"0\nSECTION\n"));
        assert!(!is_dwg(b"AutoCAD Binary DXF"));
        assert!(!is_dwg(b"AC10"));
    }

    #[test]
    fn lines_keep_their_layer_color_and_units() {
        let scene = parse(&minimal_dwg()).expect("scene");
        assert_eq!(scene.paths.len(), 1);
        assert_eq!(scene.points.len(), 2);
        assert_eq!(scene.points[0], [0.0, 0.0]);
        assert_eq!(scene.points[1], [10.0, 5.0]);
        assert_eq!(scene.layers.len(), 2);
        assert_eq!(scene.layers[1].name.as_ref(), "WALLS");
        assert_eq!(scene.layers[1].color, encode_aci(1));
        assert_eq!(scene.paths[0].color, encode_aci(1));
        assert_eq!(
            scene.units,
            Some(1),
            "$INSUNITS is carried to the status bar"
        );
    }

    #[test]
    fn block_geometry_is_placed_by_the_insert_transform() {
        let drawing = drawing_with_line(std::f64::consts::FRAC_PI_2, 3.0);
        let scene = parse(&drawing.to_bytes().expect("bytes")).expect("scene");
        assert_eq!(scene.paths.len(), 1);
        let points = &scene.points[..2];
        assert!((points[0][0] - 10.0).abs() < 1e-9, "{points:?}");
        assert!((points[0][1] - 20.0).abs() < 1e-9, "{points:?}");
        assert!((points[1][0] - 10.0).abs() < 1e-9, "{points:?}");
        assert!((points[1][1] - 23.0).abs() < 1e-9, "{points:?}");
    }

    #[test]
    fn by_block_entities_follow_the_insert_color() {
        let mut drawing = Drawing::new();
        let layer0 = drawing.layer_by_name("0").expect("layer 0");
        let block = drawing.add_block("PART").expect("block");
        let mut common = EntityCommon::new(layer0);
        common.color = Color::ByBlock;
        drawing
            .add_entity(
                block,
                common,
                Entity::Line(Line::new([0.0, 0.0, 0.0], [1.0, 0.0, 0.0])),
            )
            .expect("block line");
        let model = drawing.model_space();
        let mut common = EntityCommon::new(layer0);
        common.color = Color::Index(3);
        drawing
            .add_entity(
                model,
                common,
                Entity::Insert(Insert::new(block, [0.0, 0.0, 0.0])),
            )
            .expect("insert");
        let scene = parse(&drawing.to_bytes().expect("bytes")).expect("scene");
        assert_eq!(scene.paths[0].color, encode_aci(3));
    }

    #[test]
    fn block_geometry_on_layer_zero_takes_the_insert_layer() {
        let mut drawing = Drawing::new();
        let layer0 = drawing.layer_by_name("0").expect("layer 0");
        let walls = drawing.add_layer(Layer::new("WALLS")).expect("walls");
        let block = drawing.add_block("PART").expect("block");
        drawing
            .add_entity(
                block,
                EntityCommon::new(layer0),
                Entity::Line(Line::new([0.0, 0.0, 0.0], [1.0, 0.0, 0.0])),
            )
            .expect("block line");
        let model = drawing.model_space();
        drawing
            .add_entity(
                model,
                EntityCommon::new(walls),
                Entity::Insert(Insert::new(block, [0.0, 0.0, 0.0])),
            )
            .expect("insert");
        let scene = parse(&drawing.to_bytes().expect("bytes")).expect("scene");
        let walls = scene
            .layers
            .iter()
            .position(|layer| layer.name.as_ref() == "WALLS")
            .expect("walls layer");
        assert_eq!(scene.paths[0].layer as usize, walls);
    }

    #[test]
    fn curves_are_discretized_in_the_drawn_direction() {
        let mut drawing = Drawing::new();
        let layer0 = drawing.layer_by_name("0").expect("layer 0");
        let model = drawing.model_space();
        let arc = Arc {
            circle: Circle::new([0.0, 0.0, 0.0], 2.0),
            start_angle: 0.0,
            end_angle: std::f64::consts::FRAC_PI_2,
        };
        drawing
            .add_entity(model, EntityCommon::new(layer0), Entity::Arc(arc))
            .expect("arc");
        drawing
            .add_entity(
                model,
                EntityCommon::new(layer0),
                Entity::Circle(Circle::new([5.0, 5.0, 0.0], 1.0)),
            )
            .expect("circle");
        let scene = parse(&drawing.to_bytes().expect("bytes")).expect("scene");
        assert_eq!(scene.paths.len(), 2);
        let arc = &scene.paths[0];
        let arc_points = &scene.points[arc.start as usize..(arc.start + arc.len) as usize];
        assert_eq!(arc_points[0], [2.0, 0.0]);
        let last = arc_points.last().expect("arc end");
        assert!(
            last[0].abs() < 1e-9 && (last[1] - 2.0).abs() < 1e-9,
            "{last:?}"
        );
        let circle = &scene.paths[1];
        assert!(circle.closed);
        let circle_points =
            &scene.points[circle.start as usize..(circle.start + circle.len) as usize];
        assert_eq!(circle_points.first(), circle_points.last());
    }

    #[test]
    fn closed_bulged_polylines_keep_their_arcs() {
        let mut drawing = Drawing::new();
        let layer0 = drawing.layer_by_name("0").expect("layer 0");
        let model = drawing.model_space();
        let polyline = LwPolyline {
            flags: LWPOLYLINE_CLOSED,
            const_width: 0.0,
            elevation: 0.0,
            thickness: 0.0,
            extrusion: [0.0, 0.0, 1.0],
            points: vec![[0.0, 0.0], [4.0, 0.0]],
            bulges: vec![0.0, 1.0],
            vertex_ids: Vec::new(),
            widths: Vec::new(),
            read_points: opendwg::object::AsRead::default(),
        };
        drawing
            .add_entity(
                model,
                EntityCommon::new(layer0),
                Entity::LwPolyline(Box::new(polyline)),
            )
            .expect("polyline");
        let scene = parse(&drawing.to_bytes().expect("bytes")).expect("scene");
        assert_eq!(scene.paths.len(), 1);
        assert!(scene.paths[0].closed);
        assert!(scene.paths[0].len > 10, "bulge becomes an arc");
    }

    #[test]
    fn text_keeps_its_size_and_rotation() {
        let mut drawing = Drawing::new();
        let layer0 = drawing.layer_by_name("0").expect("layer 0");
        let model = drawing.model_space();
        let mut text = Text::new([2.0, 3.0, 0.0], 2.5, "A%%dB");
        text.rotation = std::f64::consts::FRAC_PI_2;
        drawing
            .add_entity(model, EntityCommon::new(layer0), Entity::Text(text))
            .expect("text");
        let scene = parse(&drawing.to_bytes().expect("bytes")).expect("scene");
        assert_eq!(scene.texts.len(), 1);
        let label = &scene.texts[0];
        assert_eq!(label.text.as_ref(), "A\u{b0}B");
        assert_eq!(label.position, [2.0, 3.0]);
        assert_eq!(label.height, 2.5);
        assert!((label.rotation - std::f64::consts::FRAC_PI_2).abs() < 1e-9);
    }

    #[test]
    fn paper_space_entities_are_counted_not_drawn() {
        let mut drawing = Drawing::new();
        let layer0 = drawing.layer_by_name("0").expect("layer 0");
        let model = drawing.model_space();
        drawing
            .add_entity(
                model,
                EntityCommon::new(layer0),
                Entity::Line(Line::new([0.0, 0.0, 0.0], [1.0, 0.0, 0.0])),
            )
            .expect("line");
        let paper = drawing
            .paper_spaces()
            .into_iter()
            .next()
            .expect("paper space");
        drawing
            .add_entity(
                paper,
                EntityCommon::new(layer0),
                Entity::Line(Line::new([0.0, 0.0, 0.0], [2.0, 0.0, 0.0])),
            )
            .expect("paper line");
        let scene = parse(&drawing.to_bytes().expect("bytes")).expect("scene");
        assert_eq!(scene.paths.len(), 1);
        assert_eq!(scene.paper_space_entities, 1);
    }

    #[test]
    fn layer_off_is_hidden_but_kept() {
        let mut drawing = Drawing::new();
        let mut layer = Layer::new("HIDDEN");
        layer.off = true;
        let hidden = drawing.add_layer(layer).expect("layer");
        drawing
            .add_entity(
                drawing.model_space(),
                EntityCommon::new(hidden),
                Entity::Line(Line::new([0.0, 0.0, 0.0], [1.0, 0.0, 0.0])),
            )
            .expect("line");
        let scene = parse(&drawing.to_bytes().expect("bytes")).expect("scene");
        let hidden = scene
            .layers
            .iter()
            .find(|layer| layer.name.as_ref() == "HIDDEN")
            .expect("hidden layer");
        assert!(!hidden.visible);
        assert!(!scene.initial_visibility()[scene.paths[0].layer as usize]);
    }

    #[test]
    fn deeply_nested_blocks_stop_at_the_depth_limit() {
        let mut drawing = Drawing::new();
        let layer0 = drawing.layer_by_name("0").expect("layer 0");
        let blocks: Vec<Handle> = (0..6)
            .map(|index| drawing.add_block(&format!("B{index}")).expect("block"))
            .collect();
        for (index, block) in blocks.iter().enumerate() {
            drawing
                .add_entity(
                    *block,
                    EntityCommon::new(layer0),
                    Entity::Line(Line::new([0.0, 0.0, 0.0], [1.0, 0.0, 0.0])),
                )
                .expect("block line");
            if let Some(next) = blocks.get(index + 1) {
                drawing
                    .add_entity(
                        *block,
                        EntityCommon::new(layer0),
                        Entity::Insert(Insert::new(*next, [0.0, 0.0, 0.0])),
                    )
                    .expect("nested insert");
            }
        }
        drawing
            .add_entity(
                drawing.model_space(),
                EntityCommon::new(layer0),
                Entity::Insert(Insert::new(blocks[0], [0.0, 0.0, 0.0])),
            )
            .expect("insert");
        let scene = parse(&drawing.to_bytes().expect("bytes")).expect("scene");
        assert!(
            scene
                .skipped
                .iter()
                .any(|(kind, _)| kind.as_ref() == "INSERT nesting"),
            "the recursion limit must be reported: {:?}",
            scene.skipped
        );
    }

    #[test]
    fn an_empty_drawing_says_so() {
        let bytes = Drawing::new().to_bytes().expect("bytes");
        assert!(matches!(parse(&bytes), Err(CadDwgError::Empty)));
    }

    #[test]
    fn an_unsupported_version_names_the_release() {
        let mut bytes = b"AC1014".to_vec();
        bytes.resize(256, 0);
        match parse(&bytes) {
            Err(CadDwgError::UnsupportedVersion(version)) => {
                assert_eq!(version.as_ref(), "R14 (AC1014)")
            }
            Err(other) => panic!("expected a version error, got {other:?}"),
            Ok(_) => panic!("expected a version error, got a scene"),
        }
    }

    #[test]
    fn a_damaged_container_is_reported_as_corrupt() {
        let mut bytes = b"AC1032".to_vec();
        bytes.resize(64, 0);
        assert!(matches!(parse(&bytes), Err(CadDwgError::Corrupt)));
    }
}

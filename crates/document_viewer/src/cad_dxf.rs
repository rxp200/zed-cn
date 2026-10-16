//! Bounded streaming parser for ASCII DXF drawings.
//!
//! The parser only ever looks at one group code / value pair at a time and
//! flattens the drawing into [`CadScene`] as it goes, so peak memory follows the
//! scene budgets rather than the entity count. Unsupported entity types are
//! counted instead of failing, and binary DXF or DWG containers are detected up
//! front so the reader can explain why they cannot be shown.

use std::{collections::HashMap, fmt};

use encoding_rs::{Encoding, GBK, UTF_8, WINDOWS_1251, WINDOWS_1252};
use gpui::SharedString;

use crate::cad_scene::{
    ACI_FOREGROUND, COLOR_INHERIT, CadBounds, CadColor, CadLayer, CadScene, MAX_BLOCK_POINTS,
    MAX_INSERT_DEPTH, encode_aci, encode_truecolor, push_arc_points, push_bulge_points,
    push_ellipse_points,
};

/// Why a DXF file cannot be previewed at all.
#[derive(Debug)]
pub enum CadError {
    /// The file is an AutoCAD DWG container, not a DXF drawing.
    Dwg,
    /// Binary DXF encodes group values differently from the ASCII text form.
    BinaryDxf,
    /// The file is not a DXF document.
    NotDxf,
    /// The document parsed, but holds nothing that can be drawn.
    Empty,
}

impl fmt::Display for CadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            CadError::Dwg => i18n::t!("4a57c500c76df00e"),
            CadError::BinaryDxf => i18n::t!("9d31bf6b9ebcc683"),
            CadError::NotDxf => i18n::t!("ee27e1fb58fdfa8a"),
            CadError::Empty => i18n::t!("96ab902f5adf7da4"),
        };
        f.write_str(&message)
    }
}

impl std::error::Error for CadError {}

/// Parses ASCII DXF bytes into a renderable scene.
pub fn parse(bytes: &[u8]) -> Result<CadScene, CadError> {
    reject_unsupported_containers(bytes)?;
    let mut parser = Parser::new(bytes);
    parser.run();
    parser.finish()
}

/// Rejects containers that are not ASCII DXF, before any parsing.
fn reject_unsupported_containers(bytes: &[u8]) -> Result<(), CadError> {
    let head = &bytes[..bytes.len().min(64)];
    if head.starts_with(b"AutoCAD Binary DXF") {
        return Err(CadError::BinaryDxf);
    }
    // DWG containers begin with a version sentinel such as AC1015 or AC1032.
    if head.len() >= 6 && &head[0..2] == b"AC" && head[2..6].iter().all(u8::is_ascii_digit) {
        return Err(CadError::Dwg);
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Section {
    None,
    /// `SECTION` was seen and the following group 2 names the section.
    Named,
    Header,
    Tables,
    Blocks,
    Entities,
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Kind {
    #[default]
    Other,
    Line,
    Circle,
    Arc,
    Ellipse,
    LwPolyline,
    Polyline,
    Vertex,
    Seqend,
    Spline,
    Point,
    Face3d,
    Solid,
    Trace,
    Text,
    MText,
    Insert,
    Dimension,
    Leader,
    XLine,
    Ray,
    Attrib,
}

impl Kind {
    fn from_type(name: &[u8]) -> Self {
        match name {
            b"LINE" => Kind::Line,
            b"CIRCLE" => Kind::Circle,
            b"ARC" => Kind::Arc,
            b"ELLIPSE" => Kind::Ellipse,
            b"LWPOLYLINE" => Kind::LwPolyline,
            b"POLYLINE" => Kind::Polyline,
            b"VERTEX" => Kind::Vertex,
            b"SEQEND" => Kind::Seqend,
            b"SPLINE" => Kind::Spline,
            b"POINT" => Kind::Point,
            b"3DFACE" => Kind::Face3d,
            b"SOLID" => Kind::Solid,
            b"TRACE" => Kind::Trace,
            b"TEXT" => Kind::Text,
            b"MTEXT" => Kind::MText,
            b"INSERT" => Kind::Insert,
            b"DIMENSION" => Kind::Dimension,
            b"LEADER" => Kind::Leader,
            b"XLINE" => Kind::XLine,
            b"RAY" => Kind::Ray,
            b"ATTRIB" => Kind::Attrib,
            _ => Kind::Other,
        }
    }

    /// Entities whose repeated `10`/`20` pairs form one point list.
    fn collects_points(self) -> bool {
        matches!(self, Kind::LwPolyline | Kind::Spline | Kind::Leader)
    }
}

/// A 2D affine transform plus the rotation and scale that produced it, so text
/// inside a block reference keeps its orientation.
#[derive(Clone, Copy, Debug)]
struct Transform {
    xx: f64,
    xy: f64,
    yx: f64,
    yy: f64,
    tx: f64,
    ty: f64,
    rotation: f64,
    scale: [f64; 2],
}

impl Transform {
    fn apply(&self, point: [f64; 2]) -> [f64; 2] {
        [
            self.xx * point[0] + self.xy * point[1] + self.tx,
            self.yx * point[0] + self.yy * point[1] + self.ty,
        ]
    }

    /// Transforms a direction, ignoring the translation.
    fn apply_direction(&self, direction: [f64; 2]) -> [f64; 2] {
        [
            self.xx * direction[0] + self.xy * direction[1],
            self.yx * direction[0] + self.yy * direction[1],
        ]
    }

    /// Applies `self` first and then `outer`, which is how a nested block
    /// reference composes with the one that contains it.
    fn then(&self, outer: &Transform) -> Transform {
        Transform {
            xx: outer.xx * self.xx + outer.xy * self.yx,
            xy: outer.xx * self.xy + outer.xy * self.yy,
            yx: outer.yx * self.xx + outer.yy * self.yx,
            yy: outer.yx * self.xy + outer.yy * self.yy,
            tx: outer.xx * self.tx + outer.xy * self.ty + outer.tx,
            ty: outer.yx * self.tx + outer.yy * self.ty + outer.ty,
            rotation: outer.rotation + self.rotation,
            scale: [
                outer.scale[0] * self.scale[0],
                outer.scale[1] * self.scale[1],
            ],
        }
    }

    /// Average axis scale, used to size text inside a block.
    fn text_scale(&self) -> f64 {
        let first = (self.xx * self.xx + self.yx * self.yx).sqrt();
        let second = (self.xy * self.xy + self.yy * self.yy).sqrt();
        ((first + second) / 2.0).abs()
    }
}

/// Geometry that has been read but not yet placed in the scene.
enum Pending {
    Path {
        points: Vec<[f64; 2]>,
        closed: bool,
    },
    Fill {
        points: Vec<[f64; 2]>,
    },
    Text {
        position: [f64; 2],
        height: f64,
        rotation: f64,
        text: String,
    },
    Infinite {
        origin: [f64; 2],
        direction: [f64; 2],
    },
    /// A block reference (or a dimension, which stores its geometry in one).
    Insert {
        block: String,
        insertion: [f64; 2],
        scale: [f64; 2],
        rotation: f64,
    },
}

impl Pending {
    fn point_count(&self) -> usize {
        match self {
            Pending::Path { points, .. } | Pending::Fill { points } => points.len(),
            _ => 0,
        }
    }
}

/// One entry of a block definition.
enum BlockEntity {
    Path {
        points: Vec<[f64; 2]>,
        closed: bool,
        layer: u16,
        color: CadColor,
    },
    Fill {
        points: Vec<[f64; 2]>,
        layer: u16,
        color: CadColor,
    },
    Text {
        position: [f64; 2],
        height: f64,
        rotation: f64,
        layer: u16,
        color: CadColor,
        text: String,
    },
    Infinite {
        origin: [f64; 2],
        direction: [f64; 2],
        layer: u16,
        color: CadColor,
    },
    /// A nested block reference, resolved when the block is expanded.
    Insert {
        block: String,
        insertion: [f64; 2],
        scale: [f64; 2],
        rotation: f64,
        layer: u16,
    },
}

struct Block {
    base: [f64; 2],
    entities: Vec<BlockEntity>,
    points: usize,
}

/// The `POLYLINE` entity's vertices arrive as separate `VERTEX` entities.
struct PendingPolyline {
    closed: bool,
    layer: u16,
    color: CadColor,
    vertices: Vec<(f64, f64, f64)>,
}

/// Entity scratch space, reused for every entity so parsing allocates once.
#[derive(Default)]
struct Entity {
    kind: Kind,
    type_name: Vec<u8>,
    layer_name: Vec<u8>,
    color: CadColor,
    invisible: bool,
    paper_space: bool,
    in_block: bool,
    first: Option<[f64; 2]>,
    second: Option<[f64; 2]>,
    third: Option<[f64; 2]>,
    fourth: Option<[f64; 2]>,
    values: Vec<(i32, f64)>,
    strings: Vec<(i32, Vec<u8>)>,
    /// Repeated point lists (`LWPOLYLINE`, `SPLINE` control points, `LEADER`).
    points: Vec<(f64, f64, f64)>,
    /// `SPLINE` fit points.
    fits: Vec<(f64, f64)>,
}

impl Entity {
    /// Resets the scratch entity while keeping the buffers' capacity, so a
    /// drawing with a million entities does not allocate a million times.
    fn reset(&mut self, kind: Kind) {
        self.kind = kind;
        self.type_name.clear();
        self.layer_name.clear();
        self.color = COLOR_INHERIT;
        self.invisible = false;
        self.paper_space = false;
        self.in_block = false;
        self.first = None;
        self.second = None;
        self.third = None;
        self.fourth = None;
        self.values.clear();
        self.strings.clear();
        self.points.clear();
        self.fits.clear();
    }

    fn number(&self, code: i32) -> Option<f64> {
        self.values
            .iter()
            .rev()
            .find(|entry| entry.0 == code)
            .map(|entry| entry.1)
    }

    fn integer(&self, code: i32) -> Option<i64> {
        self.number(code).map(|value| value as i64)
    }

    fn string(&self, code: i32) -> Option<&[u8]> {
        self.strings
            .iter()
            .rev()
            .find(|entry| entry.0 == code)
            .map(|entry| entry.1.as_slice())
    }

    /// Concatenates every value of `code` in encounter order, which is how DXF
    /// splits a long `MTEXT` string into 250-character chunks.
    fn joined_string(&self, code: i32) -> Vec<u8> {
        let mut joined = Vec::new();
        for entry in self.strings.iter().filter(|entry| entry.0 == code) {
            joined.extend_from_slice(&entry.1);
        }
        joined
    }
}

struct Parser<'a> {
    lines: Lines<'a>,
    scene: CadScene,
    layers: HashMap<String, u16>,
    blocks: Vec<Block>,
    block_index: HashMap<String, u32>,
    decoding: &'static Encoding,
    section: Section,
    /// Name of the `TABLE` currently being read.
    table: Option<Vec<u8>>,
    /// Whether the next group 2 names the table instead of a table entry.
    expect_table_name: bool,
    /// Type name of the table entry being read, if any.
    table_entry: Option<Vec<u8>>,
    header_variable: Option<Vec<u8>>,
    entity: Entity,
    block: Option<Block>,
    expect_block_header: bool,
    block_name: Vec<u8>,
    block_base: [f64; 2],
    polyline: Option<PendingPolyline>,
    infinite: Vec<([f64; 2], [f64; 2], u16, CadColor)>,
    saw_section: bool,
}

impl<'a> Parser<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
        Self {
            lines: Lines { bytes, position: 0 },
            scene: CadScene::new(),
            layers: HashMap::new(),
            blocks: Vec::new(),
            block_index: HashMap::new(),
            decoding: WINDOWS_1252,
            section: Section::None,
            table: None,
            expect_table_name: false,
            table_entry: None,
            header_variable: None,
            entity: Entity::default(),
            block: None,
            expect_block_header: false,
            block_name: Vec::new(),
            block_base: [0.0; 2],
            polyline: None,
            infinite: Vec::new(),
            saw_section: false,
        }
    }

    fn run(&mut self) {
        while let Some((code, value)) = self.lines.next_pair() {
            if code == 0 {
                self.finish_current();
                self.on_definition(value);
            } else {
                self.on_pair(code, value);
            }
        }
        self.finish_current();
    }

    /// Handles a `0` group, which starts or ends every structural element.
    fn on_definition(&mut self, value: &[u8]) {
        let name = trimmed(value);
        if ascii_eq(name, b"SECTION") {
            self.saw_section = true;
            self.section = Section::Named;
            return;
        }
        if ascii_eq(name, b"ENDSEC") {
            self.section = Section::None;
            self.table = None;
            self.table_entry = None;
            return;
        }
        if ascii_eq(name, b"EOF") {
            self.section = Section::None;
            self.lines.skip_to_end();
            return;
        }
        match self.section {
            Section::Tables => {
                if ascii_eq(name, b"TABLE") {
                    self.expect_table_name = true;
                    self.table_entry = None;
                } else if ascii_eq(name, b"ENDTAB") {
                    self.table = None;
                    self.table_entry = None;
                } else {
                    self.table_entry = Some(name.to_vec());
                    self.entity.reset(Kind::Other);
                    self.entity.type_name.extend_from_slice(name);
                }
            }
            Section::Blocks => {
                if ascii_eq(name, b"ENDBLK") {
                    self.close_block();
                } else if ascii_eq(name, b"BLOCK") {
                    self.block = Some(Block {
                        base: [0.0; 2],
                        entities: Vec::new(),
                        points: 0,
                    });
                    self.expect_block_header = true;
                    self.block_name.clear();
                    self.block_base = [0.0; 2];
                    self.entity.reset(Kind::Other);
                } else {
                    self.expect_block_header = false;
                    self.entity.reset(Kind::from_type(name));
                    self.entity.type_name.extend_from_slice(name);
                    self.entity.in_block = true;
                }
            }
            Section::Entities => {
                self.entity.reset(Kind::from_type(name));
                self.entity.type_name.extend_from_slice(name);
            }
            _ => {}
        }
    }

    fn on_pair(&mut self, code: i32, value: &[u8]) {
        match self.section {
            Section::Named => {
                if code == 2 {
                    self.section = match trimmed(value) {
                        b"HEADER" => Section::Header,
                        b"TABLES" => Section::Tables,
                        b"BLOCKS" => Section::Blocks,
                        b"ENTITIES" => Section::Entities,
                        _ => Section::Other,
                    };
                }
            }
            Section::Header => self.on_header_pair(code, value),
            Section::Tables => {
                if self.expect_table_name && code == 2 {
                    self.table = Some(trimmed(value).to_vec());
                    self.expect_table_name = false;
                    return;
                }
                self.on_entity_pair(code, value);
            }
            Section::Blocks => {
                if self.expect_block_header {
                    match code {
                        2 => self.block_name = trimmed(value).to_vec(),
                        10 | 20 => {
                            let axis = (code - 10) / 10;
                            self.block_base[axis as usize] = parse_number(value).unwrap_or(0.0);
                        }
                        _ => {}
                    }
                    return;
                }
                self.on_entity_pair(code, value);
            }
            Section::Entities => self.on_entity_pair(code, value),
            _ => {}
        }
    }

    fn on_header_pair(&mut self, code: i32, value: &[u8]) {
        if code == 9 {
            self.header_variable = Some(trimmed(value).to_vec());
            return;
        }
        let Some(variable) = self.header_variable.as_deref() else {
            return;
        };
        match variable {
            b"$ACADVER" if code == 1 => {
                let version = ascii_string(value);
                // DXF 2007 and later encode strings as UTF-8 regardless of the
                // code page declared in the header.
                if version
                    .strip_prefix("AC")
                    .and_then(|code| code.parse::<i32>().ok())
                    .is_some_and(|code| code >= 1021)
                {
                    self.decoding = UTF_8;
                }
                self.scene.acad_version = Some(SharedString::from(version));
            }
            b"$DWGCODEPAGE" if code == 1 || code == 3 => {
                let code_page = ascii_string(value);
                if self.decoding != UTF_8 {
                    self.decoding = encoding_for_code_page(&code_page);
                }
                self.scene.code_page = Some(SharedString::from(code_page));
            }
            b"$INSUNITS" if code == 70 => {
                self.scene.units = parse_number(value).map(|units| units as i32);
            }
            _ => {}
        }
    }

    fn on_entity_pair(&mut self, code: i32, value: &[u8]) {
        // A layer entry is a table record rather than drawable geometry, so its
        // colour codes are kept as raw values for the table parser.
        let table_entry = self.section == Section::Tables;
        let entity = &mut self.entity;
        let collects_points = entity.kind.collects_points();
        match code {
            8 => entity.layer_name = trimmed(value).to_vec(),
            60 => entity.invisible = parse_number(value).is_some_and(|flag| flag != 0.0),
            62 => {
                if let Some(index) = parse_number(value) {
                    if table_entry {
                        entity.values.push((62, index));
                    } else {
                        entity.color = encode_aci(index as i16);
                    }
                }
            }
            67 => entity.paper_space = parse_number(value).is_some_and(|flag| flag != 0.0),
            420 => {
                if let Some(rgb) = parse_number(value) {
                    if table_entry {
                        entity.values.push((420, rgb));
                    } else {
                        entity.color = encode_truecolor(rgb as u32);
                    }
                }
            }
            1..=3 => entity.strings.push((code, trimmed(value).to_vec())),
            10 | 20 if collects_points => {
                let Some(number) = parse_number(value) else {
                    return;
                };
                let axis = (code - 10) / 10;
                match axis {
                    0 => entity.points.push((number, 0.0, 0.0)),
                    _ => {
                        if let Some(point) = entity.points.last_mut() {
                            point.1 = number;
                        }
                    }
                }
            }
            11 | 21 if entity.kind == Kind::Spline => {
                let Some(number) = parse_number(value) else {
                    return;
                };
                let axis = (code - 10) / 10;
                match axis {
                    1 => entity.fits.push((number, 0.0)),
                    _ => {
                        if let Some(point) = entity.fits.last_mut() {
                            point.1 = number;
                        }
                    }
                }
            }
            42 if entity.kind == Kind::LwPolyline => {
                if let Some(bulge) = parse_number(value)
                    && let Some(vertex) = entity.points.last_mut()
                {
                    vertex.2 = bulge;
                }
            }
            10 | 20 | 11 | 21 | 12 | 22 | 13 | 23 => {
                let Some(number) = parse_number(value) else {
                    return;
                };
                // Codes 10/11/12/13 are X, 20/21/22/23 are the matching Y.
                let pair = (code - 10) % 10;
                let axis = (code - 10) / 10;
                let slot = match pair {
                    0 => &mut entity.first,
                    1 => &mut entity.second,
                    2 => &mut entity.third,
                    _ => &mut entity.fourth,
                };
                let mut point = slot.unwrap_or([0.0, 0.0]);
                point[axis as usize] = number;
                *slot = Some(point);
            }
            _ => {
                if let Some(number) = parse_number(value) {
                    entity.values.push((code, number));
                } else {
                    entity.strings.push((code, trimmed(value).to_vec()));
                }
            }
        }
    }

    /// Commits whatever the previous `0` group closed.
    fn finish_current(&mut self) {
        match self.section {
            Section::Tables => self.finish_table_entry(),
            Section::Blocks | Section::Entities => {
                if self.expect_block_header {
                    return;
                }
                match self.entity.kind {
                    Kind::Polyline => self.start_polyline(),
                    Kind::Vertex => self.push_polyline_vertex(),
                    Kind::Seqend => self.finish_polyline(),
                    _ => self.finish_entity(),
                }
            }
            _ => {}
        }
    }

    fn finish_table_entry(&mut self) {
        if self.table.as_deref() != Some(b"LAYER") {
            return;
        }
        if self.table_entry.as_deref() != Some(b"LAYER") {
            return;
        }
        let Some(name) = self.entity.string(2) else {
            return;
        };
        let name = self.decode(name);
        let flags = self.entity.integer(70).unwrap_or(0);
        let frozen = flags & 1 != 0;
        let mut color = COLOR_INHERIT;
        let mut on = true;
        if let Some(index) = self.entity.number(62) {
            let index = index as i16;
            // A negative color index turns the layer off.
            on = index >= 0;
            color = encode_aci(index.abs());
        }
        if let Some(rgb) = self.entity.number(420) {
            color = encode_truecolor(rgb as u32);
        }
        if color == COLOR_INHERIT {
            color = encode_aci(ACI_FOREGROUND);
        }
        let visible = on && !frozen;
        match self.layers.get(&name) {
            Some(&index) => {
                let layer = &mut self.scene.layers[index as usize];
                layer.color = color;
                layer.frozen = frozen;
                layer.visible = visible;
            }
            None => {
                let index = self.scene.push_layer(CadLayer {
                    name: name.clone().into(),
                    color,
                    frozen,
                    visible,
                });
                self.layers.insert(name, index);
            }
        }
    }

    fn start_polyline(&mut self) {
        let closed = self.entity.integer(70).is_some_and(|flags| flags & 1 != 0);
        let layer = self.layer_index();
        let color = self.entity.color;
        self.polyline = Some(PendingPolyline {
            closed,
            layer,
            color,
            vertices: Vec::new(),
        });
    }

    fn push_polyline_vertex(&mut self) {
        let (Some(polyline), Some(point)) = (&mut self.polyline, self.entity.first) else {
            return;
        };
        let bulge = self.entity.number(42).unwrap_or(0.0);
        polyline.vertices.push((point[0], point[1], bulge));
    }

    fn finish_polyline(&mut self) {
        let Some(polyline) = self.polyline.take() else {
            return;
        };
        if polyline.vertices.len() < 2 {
            return;
        }
        let points = vertices_to_points(&polyline.vertices, polyline.closed);
        self.emit(
            Pending::Path {
                points,
                closed: polyline.closed,
            },
            polyline.layer,
            polyline.color,
        );
    }

    fn finish_entity(&mut self) {
        if self.entity.kind == Kind::Other {
            if !self.entity.in_block && !self.entity.type_name.is_empty() {
                let name = ascii_string(&self.entity.type_name);
                self.scene.note_skipped(&name);
            }
            return;
        }
        if self.entity.invisible {
            return;
        }
        if self.entity.paper_space {
            self.scene.paper_space_entities = self.scene.paper_space_entities.saturating_add(1);
            return;
        }
        let Some(pending) = self.build_pending() else {
            return;
        };
        let layer = self.layer_index();
        let color = self.entity.color;
        self.emit(pending, layer, color);
    }

    /// Turns the current scratch entity into geometry. The scratch is moved out
    /// so the geometry code can also borrow the parser's decoder.
    fn build_pending(&mut self) -> Option<Pending> {
        let mut entity = std::mem::take(&mut self.entity);
        let pending = self.build_from_scratch(&mut entity);
        self.entity = entity;
        pending
    }

    fn build_from_scratch(&self, entity: &mut Entity) -> Option<Pending> {
        match entity.kind {
            Kind::Line => {
                let (start, end) = (entity.first?, entity.second?);
                Some(Pending::Path {
                    points: vec![start, end],
                    closed: false,
                })
            }
            Kind::Point => {
                let point = entity.first?;
                Some(Pending::Path {
                    points: vec![point, point],
                    closed: false,
                })
            }
            Kind::Circle => {
                let center = entity.first?;
                let radius = entity.number(40)?;
                if radius <= 0.0 {
                    return None;
                }
                let first = [center[0] + radius, center[1]];
                let mut points = Vec::with_capacity(80);
                points.push(first);
                push_arc_points(&mut points, center, radius, 0.0, std::f64::consts::TAU);
                // A full turn lands on the first point only up to rounding, so
                // the closing vertex is pinned to make the outline exact.
                if let Some(last) = points.last_mut() {
                    *last = first;
                }
                Some(Pending::Path {
                    points,
                    closed: true,
                })
            }
            Kind::Arc => {
                let center = entity.first?;
                let radius = entity.number(40)?;
                if radius <= 0.0 {
                    return None;
                }
                let start = entity.number(50).unwrap_or(0.0).to_radians();
                let end = entity.number(51).unwrap_or(360.0).to_radians();
                let mut sweep = end - start;
                // DXF arcs always run counter-clockwise from the start angle.
                while sweep <= 1e-12 {
                    sweep += std::f64::consts::TAU;
                }
                let mut points = Vec::with_capacity(80);
                points.push([
                    center[0] + radius * start.cos(),
                    center[1] + radius * start.sin(),
                ]);
                push_arc_points(&mut points, center, radius, start, sweep);
                Some(Pending::Path {
                    points,
                    closed: false,
                })
            }
            Kind::Ellipse => {
                let center = entity.first?;
                let major = entity.second?;
                let ratio = entity.number(40).unwrap_or(1.0);
                if ratio <= 0.0 {
                    return None;
                }
                let start = entity.number(41).unwrap_or(0.0);
                let end = entity.number(42).unwrap_or(std::f64::consts::TAU);
                let mut sweep = end - start;
                if sweep.abs() < 1e-12 {
                    sweep = std::f64::consts::TAU;
                }
                let minor = [-major[1] * ratio, major[0] * ratio];
                let (sin, cos) = start.sin_cos();
                let mut points = vec![[
                    center[0] + major[0] * cos + minor[0] * sin,
                    center[1] + major[1] * cos + minor[1] * sin,
                ]];
                push_ellipse_points(&mut points, center, major, ratio, start, sweep);
                let closed = sweep.abs() >= std::f64::consts::TAU - 1e-9;
                if closed {
                    let first = points[0];
                    if let Some(last) = points.last_mut() {
                        *last = first;
                    }
                }
                Some(Pending::Path { points, closed })
            }
            Kind::LwPolyline => {
                let closed = entity.integer(70).is_some_and(|flags| flags & 1 != 0);
                let vertices = std::mem::take(&mut entity.points);
                let points = vertices_to_points(&vertices, closed);
                if points.len() < 2 {
                    return None;
                }
                Some(Pending::Path { points, closed })
            }
            Kind::Spline => {
                // Exact NURBS evaluation is out of scope: fit points describe the
                // drawn curve closely, and control points are the fallback.
                let fits = std::mem::take(&mut entity.fits);
                let controls = std::mem::take(&mut entity.points);
                let points: Vec<[f64; 2]> = if fits.len() >= 2 {
                    fits.iter().map(|(x, y)| [*x, *y]).collect()
                } else {
                    controls.iter().map(|(x, y, _)| [*x, *y]).collect()
                };
                if points.len() < 2 {
                    return None;
                }
                Some(Pending::Path {
                    points,
                    closed: false,
                })
            }
            Kind::Face3d | Kind::Solid | Kind::Trace => {
                let mut corners =
                    vec![entity.first?, entity.second?, entity.fourth?, entity.third?];
                // A triangular face repeats its last corner.
                if corners[2] == corners[3] {
                    corners.pop();
                }
                if corners.len() < 3 {
                    return None;
                }
                Some(Pending::Fill { points: corners })
            }
            Kind::Leader => {
                let points: Vec<[f64; 2]> =
                    entity.points.iter().map(|(x, y, _)| [*x, *y]).collect();
                if points.len() < 2 {
                    return None;
                }
                Some(Pending::Path {
                    points,
                    closed: false,
                })
            }
            Kind::XLine | Kind::Ray => {
                let origin = entity.first?;
                let direction = entity.second?;
                let length = (direction[0] * direction[0] + direction[1] * direction[1]).sqrt();
                if !length.is_finite() || length <= 0.0 {
                    return None;
                }
                Some(Pending::Infinite {
                    origin,
                    direction: [direction[0] / length, direction[1] / length],
                })
            }
            Kind::Text | Kind::MText => {
                let position = entity.first?;
                let height = entity.number(40).unwrap_or(0.0);
                let rotation = entity.number(50).unwrap_or(0.0).to_radians();
                let raw = entity.joined_string(1);
                let text = strip_text_codes(&self.decode(&raw));
                if text.trim().is_empty() {
                    return None;
                }
                Some(Pending::Text {
                    position,
                    height,
                    rotation,
                    text,
                })
            }
            Kind::Insert | Kind::Dimension => {
                // A dimension keeps its geometry in the anonymous block named by
                // group 2, so it expands exactly like a block reference.
                let raw = entity.string(2)?.to_vec();
                let block = self.decode(&raw);
                let mut insertion = entity.first.unwrap_or([0.0; 2]);
                let mut scale = [
                    entity.number(41).unwrap_or(1.0),
                    entity.number(42).unwrap_or(1.0),
                ];
                let mut rotation = entity.number(50).unwrap_or(0.0).to_radians();
                if entity.kind == Kind::Dimension {
                    // Dimension block geometry is already in drawing coordinates.
                    insertion = [0.0; 2];
                    scale = [1.0; 2];
                    rotation = 0.0;
                }
                Some(Pending::Insert {
                    block,
                    insertion,
                    scale,
                    rotation,
                })
            }
            Kind::Attrib | Kind::Vertex | Kind::Seqend | Kind::Polyline | Kind::Other => None,
        }
    }

    /// Resolves the entity's layer name, creating an implicit layer when the
    /// table did not define one.
    fn layer_index(&mut self) -> u16 {
        let name = self.decode(&self.entity.layer_name);
        if let Some(&index) = self.layers.get(&name) {
            return index;
        }
        let index = self.scene.push_layer(CadLayer {
            name: name.clone().into(),
            color: encode_aci(ACI_FOREGROUND),
            frozen: false,
            visible: true,
        });
        self.layers.insert(name, index);
        index
    }

    /// Adds geometry either to the block being defined or to the drawing.
    fn emit(&mut self, pending: Pending, layer: u16, color: CadColor) {
        if self.entity.in_block {
            self.emit_to_block(pending, layer, color);
        } else {
            self.emit_to_scene(pending, layer, color);
        }
    }

    fn emit_to_block(&mut self, pending: Pending, layer: u16, color: CadColor) {
        let Some(block) = self.block.as_mut() else {
            return;
        };
        if block.points + pending.point_count() > MAX_BLOCK_POINTS {
            self.scene.truncated = true;
            return;
        }
        block.points += pending.point_count();
        match pending {
            Pending::Path { points, closed } => block.entities.push(BlockEntity::Path {
                points,
                closed,
                layer,
                color,
            }),
            Pending::Fill { points } => block.entities.push(BlockEntity::Fill {
                points,
                layer,
                color,
            }),
            Pending::Text {
                position,
                height,
                rotation,
                text,
            } => block.entities.push(BlockEntity::Text {
                position,
                height,
                rotation,
                layer,
                color,
                text,
            }),
            Pending::Infinite { origin, direction } => block.entities.push(BlockEntity::Infinite {
                origin,
                direction,
                layer,
                color,
            }),
            Pending::Insert {
                block: nested,
                insertion,
                scale,
                rotation,
            } => block.entities.push(BlockEntity::Insert {
                block: nested,
                insertion,
                scale,
                rotation,
                layer,
            }),
        }
    }

    fn emit_to_scene(&mut self, pending: Pending, layer: u16, color: CadColor) {
        match pending {
            Pending::Path { points, closed } => {
                self.scene.push_path(&points, closed, layer, color);
            }
            Pending::Fill { points } => {
                self.scene.push_fill(&points, layer, color);
            }
            Pending::Text {
                position,
                height,
                rotation,
                text,
            } => {
                self.scene
                    .push_text(&text, position, height, rotation, layer, color);
            }
            Pending::Infinite { origin, direction } => {
                self.infinite.push((origin, direction, layer, color));
            }
            Pending::Insert {
                block,
                insertion,
                scale,
                rotation,
            } => self.expand_insert(&block, insertion, scale, rotation, layer),
        }
    }

    /// Expands a block reference into the drawing, applying its transform.
    fn expand_insert(
        &mut self,
        block: &str,
        insertion: [f64; 2],
        scale: [f64; 2],
        rotation: f64,
        layer: u16,
    ) {
        let Some(&index) = self.block_index.get(block) else {
            self.scene.note_skipped("INSERT target");
            return;
        };
        let base = self.blocks[index as usize].base;
        let transform = insertion_transform(base, insertion, scale, rotation);
        let zero_layer = self.scene.zero_layer();
        let mut expander = Expander {
            blocks: &self.blocks,
            block_index: &self.block_index,
            scene: &mut self.scene,
            infinite: &mut self.infinite,
            zero_layer,
        };
        expander.insert(block, transform, layer, 0);
    }

    fn close_block(&mut self) {
        let Some(block) = self.block.take() else {
            return;
        };
        self.expect_block_header = false;
        if self.block_name.is_empty() {
            return;
        }
        let name = self.decode(&self.block_name);
        if self.block_index.contains_key(&name) {
            return;
        }
        self.block_index.insert(name, self.blocks.len() as u32);
        self.blocks.push(Block {
            base: self.block_base,
            entities: block.entities,
            points: block.points,
        });
    }

    fn decode(&self, value: &[u8]) -> String {
        let (text, _, _) = self.decoding.decode(value);
        text.into_owned()
    }

    fn finish(mut self) -> Result<CadScene, CadError> {
        if !self.saw_section {
            return Err(CadError::NotDxf);
        }
        self.clip_infinite_lines();
        if self.scene.is_empty() {
            return Err(CadError::Empty);
        }
        Ok(self.scene)
    }

    /// Infinite construction lines are clipped to the drawing bounds, which are
    /// only known once every entity has been read.
    fn clip_infinite_lines(&mut self) {
        let infinite = std::mem::take(&mut self.infinite);
        let Some(bounds) = self.scene.bounds.map(|bounds| bounds.expanded(0.05)) else {
            return;
        };
        for (origin, direction, layer, color) in infinite {
            if let Some(segment) = clip_line(origin, direction, bounds) {
                self.scene.push_clipped_path(&segment, false, layer, color);
            }
        }
    }
}

/// Flattens block definitions into the drawing without borrowing the parser.
struct Expander<'a> {
    blocks: &'a [Block],
    block_index: &'a HashMap<String, u32>,
    scene: &'a mut CadScene,
    infinite: &'a mut Vec<([f64; 2], [f64; 2], u16, CadColor)>,
    zero_layer: Option<u16>,
}

impl Expander<'_> {
    fn insert(&mut self, name: &str, transform: Transform, layer_override: u16, depth: u8) -> bool {
        if depth >= MAX_INSERT_DEPTH {
            self.scene.note_skipped("INSERT nesting");
            return false;
        }
        let Some(&index) = self.block_index.get(name) else {
            self.scene.note_skipped("INSERT target");
            return false;
        };
        let Some(block) = self.blocks.get(index as usize) else {
            return false;
        };
        // DXF gives block geometry on the implicit layer `0` the layer of the
        // entity that references the block.
        let zero_layer = self.zero_layer;
        let resolve_layer = move |layer: u16| {
            if Some(layer) == zero_layer {
                layer_override
            } else {
                layer
            }
        };
        for entity in &block.entities {
            match entity {
                BlockEntity::Path {
                    points,
                    closed,
                    layer,
                    color,
                } => {
                    let points: Vec<[f64; 2]> =
                        points.iter().map(|point| transform.apply(*point)).collect();
                    self.scene
                        .push_path(&points, *closed, resolve_layer(*layer), *color);
                }
                BlockEntity::Fill {
                    points,
                    layer,
                    color,
                } => {
                    let points: Vec<[f64; 2]> =
                        points.iter().map(|point| transform.apply(*point)).collect();
                    self.scene.push_fill(&points, resolve_layer(*layer), *color);
                }
                BlockEntity::Text {
                    position,
                    height,
                    rotation,
                    layer,
                    color,
                    text,
                } => {
                    let position = transform.apply(*position);
                    let height = if *height > 0.0 {
                        height * transform.text_scale()
                    } else {
                        0.0
                    };
                    self.scene.push_text(
                        text,
                        position,
                        height,
                        rotation + transform.rotation,
                        resolve_layer(*layer),
                        *color,
                    );
                }
                BlockEntity::Infinite {
                    origin,
                    direction,
                    layer,
                    color,
                } => {
                    self.infinite.push((
                        transform.apply(*origin),
                        transform.apply_direction(*direction),
                        resolve_layer(*layer),
                        *color,
                    ));
                }
                BlockEntity::Insert {
                    block: nested,
                    insertion,
                    scale,
                    rotation,
                    layer,
                } => {
                    let Some(&nested_index) = self.block_index.get(nested) else {
                        self.scene.note_skipped("INSERT target");
                        continue;
                    };
                    let base = self
                        .blocks
                        .get(nested_index as usize)
                        .map(|block| block.base)
                        .unwrap_or([0.0; 2]);
                    let local = insertion_transform(base, *insertion, *scale, *rotation);
                    let composed = local.then(&transform);
                    self.insert(nested, composed, resolve_layer(*layer), depth + 1);
                }
            }
        }
        true
    }
}

/// Walks the byte stream as `code` / `value` line pairs.
struct Lines<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Lines<'a> {
    fn at_end(&self) -> bool {
        self.position >= self.bytes.len()
    }

    fn skip_to_end(&mut self) {
        self.position = self.bytes.len();
    }

    fn next_line(&mut self) -> Option<&'a [u8]> {
        if self.at_end() {
            return None;
        }
        let rest = &self.bytes[self.position..];
        match rest.iter().position(|byte| *byte == b'\n') {
            Some(index) => {
                self.position += index + 1;
                Some(rest[..index].strip_suffix(b"\r").unwrap_or(&rest[..index]))
            }
            None => {
                self.position = self.bytes.len();
                Some(rest.strip_suffix(b"\r").unwrap_or(rest))
            }
        }
    }

    fn next_pair(&mut self) -> Option<(i32, &'a [u8])> {
        loop {
            let code_line = self.next_line()?;
            let Some(code) = parse_number(code_line) else {
                // Tolerate blank or damaged lines before a group code.
                continue;
            };
            let value = self.next_line()?;
            return Some((code as i32, value));
        }
    }
}

fn trimmed(value: &[u8]) -> &[u8] {
    let start = value
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(value.len());
    let end = value
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &value[start..end]
}

fn ascii_eq(value: &[u8], expected: &[u8]) -> bool {
    value.eq_ignore_ascii_case(expected)
}

fn parse_number(value: &[u8]) -> Option<f64> {
    let text = std::str::from_utf8(trimmed(value)).ok()?;
    text.parse::<f64>().ok()
}

fn ascii_string(value: &[u8]) -> String {
    String::from_utf8_lossy(trimmed(value)).into_owned()
}

/// Maps a `$DWGCODEPAGE` value to an encoding, defaulting to the Western code
/// page that most non-Unicode drawings use.
fn encoding_for_code_page(code_page: &str) -> &'static Encoding {
    match code_page.to_ascii_uppercase().as_str() {
        "UTF-8" | "UTF8" => UTF_8,
        "ANSI_936" | "GB2312" | "GBK" | "GB18030" => GBK,
        "ANSI_1251" => WINDOWS_1251,
        "ANSI_1250" => encoding_rs::WINDOWS_1250,
        "ANSI_1253" => encoding_rs::WINDOWS_1253,
        "ANSI_1254" => encoding_rs::WINDOWS_1254,
        "ANSI_1255" => encoding_rs::WINDOWS_1255,
        "ANSI_1256" => encoding_rs::WINDOWS_1256,
        "ANSI_1257" => encoding_rs::WINDOWS_1257,
        "ANSI_1258" => encoding_rs::WINDOWS_1258,
        "ANSI_874" => encoding_rs::WINDOWS_874,
        "ANSI_932" => encoding_rs::SHIFT_JIS,
        "ANSI_949" => encoding_rs::EUC_KR,
        "ANSI_950" => encoding_rs::BIG5,
        _ => WINDOWS_1252,
    }
}

fn insertion_transform(
    base: [f64; 2],
    insertion: [f64; 2],
    scale: [f64; 2],
    rotation: f64,
) -> Transform {
    let (sin, cos) = rotation.sin_cos();
    let xx = cos * scale[0];
    let xy = -sin * scale[1];
    let yx = sin * scale[0];
    let yy = cos * scale[1];
    Transform {
        xx,
        xy,
        yx,
        yy,
        tx: insertion[0] - (xx * base[0] + xy * base[1]),
        ty: insertion[1] - (yx * base[0] + yy * base[1]),
        rotation,
        scale,
    }
}

/// Builds a polyline's points, tessellating the bulge arcs between vertices.
fn vertices_to_points(vertices: &[(f64, f64, f64)], closed: bool) -> Vec<[f64; 2]> {
    let mut points = Vec::with_capacity(vertices.len() + 1);
    for (index, (x, y, _)) in vertices.iter().enumerate() {
        if index == 0 {
            points.push([*x, *y]);
            continue;
        }
        let (previous_x, previous_y, previous_bulge) = vertices[index - 1];
        if previous_bulge != 0.0 {
            push_bulge_points(
                &mut points,
                [previous_x, previous_y],
                [*x, *y],
                previous_bulge,
            );
        } else {
            points.push([*x, *y]);
        }
    }
    if closed && vertices.len() >= 2 {
        let (last_x, last_y, last_bulge) = vertices[vertices.len() - 1];
        let (first_x, first_y, _) = vertices[0];
        if last_bulge != 0.0 {
            push_bulge_points(
                &mut points,
                [last_x, last_y],
                [first_x, first_y],
                last_bulge,
            );
        } else {
            points.push([first_x, first_y]);
        }
    }
    points
}

/// Clips an infinite line to a box, returning the visible segment.
fn clip_line(origin: [f64; 2], direction: [f64; 2], bounds: CadBounds) -> Option<[[f64; 2]; 2]> {
    let mut start = f64::NEG_INFINITY;
    let mut end = f64::INFINITY;
    for axis in 0..2 {
        let low = bounds.min[axis];
        let high = bounds.max[axis];
        let delta = direction[axis];
        let position = origin[axis];
        if delta.abs() < 1e-12 {
            if position < low || position > high {
                return None;
            }
            continue;
        }
        let first = (low - position) / delta;
        let second = (high - position) / delta;
        let (first, second) = if first <= second {
            (first, second)
        } else {
            (second, first)
        };
        start = start.max(first);
        end = end.min(second);
    }
    if !start.is_finite() || !end.is_finite() || start >= end {
        return None;
    }
    Some([
        [
            origin[0] + direction[0] * start,
            origin[1] + direction[1] * start,
        ],
        [
            origin[0] + direction[0] * end,
            origin[1] + direction[1] * end,
        ],
    ])
}

/// Removes DXF text formatting codes so labels render as plain text.
///
/// `TEXT` uses `%%d` / `%%p` / `%%c` for degree, plus-minus and diameter;
/// `MTEXT` adds `\P` paragraphs, `\~` spaces and `\f...;`-style control
/// sequences with brace grouping.
fn strip_text_codes(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\\' => match characters.next() {
                Some('P') | Some('p') => output.push('\n'),
                Some('~') => output.push(' '),
                Some('\\') => output.push('\\'),
                Some('{') => output.push('{'),
                Some('}') => output.push('}'),
                Some('S') => {
                    // Stacked fraction: \S1/2; renders as 1/2.
                    for inner in characters.by_ref() {
                        if inner == ';' {
                            break;
                        }
                        output.push(if inner == '^' { '/' } else { inner });
                    }
                }
                Some(letter) if letter.is_ascii_alphabetic() => {
                    // \fArial|b0|i0; \H2.5x; \C1; and friends run to a semicolon.
                    for inner in characters.by_ref() {
                        if inner == ';' {
                            break;
                        }
                    }
                }
                Some(other) => output.push(other),
                None => {}
            },
            '{' | '}' => {}
            '%' => match characters.peek() {
                Some('%') => {
                    characters.next();
                    match characters.peek() {
                        Some('d') | Some('D') => {
                            characters.next();
                            output.push('\u{b0}');
                        }
                        Some('p') | Some('P') => {
                            characters.next();
                            output.push('\u{b1}');
                        }
                        Some('c') | Some('C') => {
                            characters.next();
                            output.push('\u{2300}');
                        }
                        _ => output.push('%'),
                    }
                }
                _ => output.push('%'),
            },
            _ => output.push(character),
        }
    }
    output
}

/// A minimal but structurally complete ASCII DXF document, with extra
/// `$HEADER` variables and the given entities. Shared by the reader tests.
#[cfg(test)]
pub(crate) fn minimal_dxf_with(header: &str, entities: &str) -> Vec<u8> {
    format!(
        "0\nSECTION\n2\nHEADER\n9\n$ACADVER\n1\nAC1015\n9\n$INSUNITS\n70\n4\n{header}0\nENDSEC\n\
         0\nSECTION\n2\nTABLES\n0\nTABLE\n2\nLAYER\n0\nLAYER\n2\n0\n70\n0\n62\n7\n\
         0\nLAYER\n2\nWALLS\n70\n0\n62\n1\n0\nENDTAB\n0\nENDSEC\n\
         0\nSECTION\n2\nBLOCKS\n0\nBLOCK\n2\nTRI\n10\n0.0\n20\n0.0\n\
         0\nLINE\n8\n0\n10\n0.0\n20\n0.0\n11\n1.0\n21\n0.0\n0\nENDBLK\n0\nENDSEC\n\
         0\nSECTION\n2\nENTITIES\n{entities}0\nENDSEC\n0\nEOF\n"
    )
    .into_bytes()
}

#[cfg(test)]
pub(crate) fn minimal_dxf(entities: &str) -> Vec<u8> {
    minimal_dxf_with("", entities)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_entities_keep_layer_and_geometry() {
        let bytes = minimal_dxf(
            "0\nLINE\n8\nWALLS\n10\n0.0\n20\n0.0\n11\n10.0\n21\n0.0\n\
             0\nCIRCLE\n8\nWALLS\n10\n5.0\n20\n5.0\n40\n2.5\n\
             0\nCOLORED\n8\nWALLS\n10\n0.0\n20\n0.0\n11\n1.0\n21\n1.0\n",
        );
        let scene = parse(&bytes).expect("scene");
        assert_eq!(scene.paths.len(), 2, "LINE and CIRCLE are drawable");
        assert_eq!(scene.layers.len(), 2);
        assert_eq!(scene.layers[0].name.as_ref(), "0");
        assert_eq!(scene.layers[1].name.as_ref(), "WALLS");
        assert_eq!(scene.layers[1].color, encode_aci(1));
        // The unsupported entity type is reported instead of failing the parse.
        assert_eq!(
            scene
                .skipped
                .iter()
                .find(|entry| entry.0.as_ref() == "COLORED")
                .map(|entry| entry.1),
            Some(1)
        );
        assert_eq!(scene.units, Some(4));
        assert_eq!(scene.acad_version.as_deref(), Some("AC1015"));
        let bounds = scene.bounds.expect("bounds");
        assert_eq!(bounds.min, [0.0, 0.0]);
        assert!((bounds.max[0] - 10.0).abs() < 1e-9);
        // The circle is closed and lands back on its first point.
        let circle = scene.paths[1];
        let points = &scene.points[circle.start as usize..(circle.start + circle.len) as usize];
        assert_eq!(points[0], [7.5, 5.0]);
        assert_eq!(points[0], points[points.len() - 1]);
    }

    #[test]
    fn lwpolyline_bulges_and_closing_are_tessellated() {
        let bytes = minimal_dxf(
            "0\nLWPOLYLINE\n8\n0\n90\n3\n70\n1\n10\n0.0\n20\n0.0\n10\n10.0\n20\n0.0\n\
             42\n1.0\n10\n10.0\n20\n10.0\n",
        );
        let scene = parse(&bytes).expect("scene");
        assert_eq!(scene.paths.len(), 1);
        let path = scene.paths[0];
        assert!(path.closed);
        assert!(path.len > 3, "bulge arcs add points: {}", path.len);
        let points = &scene.points[path.start as usize..(path.start + path.len) as usize];
        // The bulge sits on the (10,0) -> (10,10) segment, so the arc must swing
        // out to x = 15 instead of following the chord.
        assert!(points.iter().any(|point| point[0] > 14.9), "{points:?}");
        assert!(
            points
                .iter()
                .all(|point| point[1] >= -1e-9 && point[1] <= 10.0001)
        );
    }

    #[test]
    fn polyline_vertex_sequences_are_assembled() {
        let bytes = minimal_dxf(
            "0\nPOLYLINE\n8\nWALLS\n66\n1\n70\n1\n\
             0\nVERTEX\n8\nWALLS\n10\n0.0\n20\n0.0\n\
             0\nVERTEX\n8\nWALLS\n10\n5.0\n20\n0.0\n\
             0\nVERTEX\n8\nWALLS\n10\n5.0\n20\n5.0\n\
             0\nSEQEND\n8\nWALLS\n",
        );
        let scene = parse(&bytes).expect("scene");
        assert_eq!(scene.paths.len(), 1);
        let path = scene.paths[0];
        assert!(path.closed);
        // A closed polyline repeats its first vertex at the end.
        let points = &scene.points[path.start as usize..(path.start + path.len) as usize];
        assert_eq!(points.len(), 4);
        assert_eq!(points[0], points[3]);
    }

    #[test]
    fn block_references_are_expanded_and_transformed() {
        let bytes =
            minimal_dxf("0\nINSERT\n8\n0\n2\nTRI\n10\n10.0\n20\n0.0\n41\n2.0\n42\n2.0\n50\n90.0\n");
        let scene = parse(&bytes).expect("scene");
        assert_eq!(scene.paths.len(), 1);
        let path = scene.paths[0];
        let points = &scene.points[path.start as usize..(path.start + path.len) as usize];
        // The block line runs from (0,0) to (1,0); rotated 90 degrees, scaled by
        // two and translated to (10,0) it becomes (10,0) to (10,2).
        assert_eq!(points[0], [10.0, 0.0]);
        assert_eq!(points[1], [10.0, 2.0]);
    }

    #[test]
    fn block_geometry_on_layer_zero_inherits_the_insert_layer() {
        let bytes = minimal_dxf("0\nINSERT\n8\nWALLS\n2\nTRI\n10\n0.0\n20\n0.0\n");
        let scene = parse(&bytes).expect("scene");
        assert_eq!(scene.paths.len(), 1);
        let layer = scene.paths[0].layer;
        assert_eq!(scene.layers[layer as usize].name.as_ref(), "WALLS");
    }

    #[test]
    fn missing_block_targets_are_counted() {
        let bytes = minimal_dxf("0\nINSERT\n8\n0\n2\nMISSING\n10\n0.0\n20\n0.0\n");
        let scene = parse(&bytes);
        // Nothing drawable: the insert of an unknown block is reported.
        assert!(matches!(scene, Err(CadError::Empty)));
    }

    #[test]
    fn text_and_mtext_are_decoded_and_cleaned() {
        let bytes = minimal_dxf(
            "0\nTEXT\n8\n0\n10\n1.0\n20\n2.0\n40\n2.5\n1\nHello %%d world\n\
             0\nMTEXT\n8\n0\n10\n3.0\n20\n4.0\n40\n2.0\n1\nLine one\\PLine \\fArial|b0;two\n",
        );
        let scene = parse(&bytes).expect("scene");
        assert_eq!(scene.texts.len(), 2);
        assert_eq!(scene.texts[0].text.as_ref(), "Hello \u{b0} world");
        assert_eq!(scene.texts[0].height, 2.5);
        assert_eq!(scene.texts[1].text.as_ref(), "Line one\nLine two");
    }

    #[test]
    fn paper_space_and_invisible_entities_are_set_aside() {
        let bytes = minimal_dxf(
            "0\nLINE\n8\n0\n67\n1\n10\n0.0\n20\n0.0\n11\n1.0\n21\n1.0\n\
             0\nLINE\n8\n0\n60\n1\n10\n0.0\n20\n0.0\n11\n2.0\n21\n2.0\n\
             0\nLINE\n8\n0\n10\n0.0\n20\n0.0\n11\n3.0\n21\n3.0\n",
        );
        let scene = parse(&bytes).expect("scene");
        assert_eq!(scene.paths.len(), 1);
        assert_eq!(scene.paper_space_entities, 1);
    }

    #[test]
    fn infinite_lines_are_clipped_to_the_drawing() {
        let bytes = minimal_dxf(
            "0\nLINE\n8\n0\n10\n0.0\n20\n0.0\n11\n10.0\n21\n10.0\n\
             0\nXLINE\n8\n0\n10\n5.0\n20\n5.0\n11\n0.0\n21\n1.0\n",
        );
        let scene = parse(&bytes).expect("scene");
        assert_eq!(scene.paths.len(), 2);
        let clipped = scene.paths[1];
        let points = &scene.points[clipped.start as usize..(clipped.start + clipped.len) as usize];
        assert_ne!(points[0][1], points[1][1]);
        assert!(points.iter().all(|point| point[0] == 5.0), "{points:?}");
    }

    #[test]
    fn code_page_header_decodes_legacy_text() {
        // "图纸" encoded as GBK, which the header declares.
        let mut bytes = minimal_dxf_with(
            "9\n$DWGCODEPAGE\n3\nANSI_936\n",
            "0\nTEXT\n8\n0\n10\n0.0\n20\n0.0\n40\n1.0\n1\n",
        );
        let tail = b"0\nENDSEC\n0\nEOF\n".len();
        let position = bytes.len() - tail;
        let mut with_text = bytes[..position].to_vec();
        with_text.extend_from_slice(&[0xCD, 0xBC, 0xD6, 0xBD]);
        with_text.push(b'\n');
        with_text.extend_from_slice(&bytes[position..]);
        bytes = with_text;
        let scene = parse(&bytes).expect("scene");
        assert_eq!(scene.texts.len(), 1);
        assert_eq!(scene.texts[0].text.as_ref(), "\u{56fe}\u{7eb8}");
        assert_eq!(scene.code_page.as_deref(), Some("ANSI_936"));
    }

    #[test]
    fn r2007_documents_are_decoded_as_utf8() {
        let source =
            String::from_utf8(minimal_dxf("0\nTEXT\n8\n0\n10\n0.0\n20\n0.0\n40\n1.0\n1\n"))
                .expect("ascii fixture")
                .replace("AC1015", "AC1021");
        // Insert the UTF-8 text before the closing ENDSEC marker.
        let marker = "0\nENDSEC\n0\nEOF\n";
        let position = source.rfind(marker).expect("trailer");
        let mut bytes = source.as_bytes()[..position].to_vec();
        bytes.extend_from_slice("\u{56fe}\u{7eb8}\n".as_bytes());
        bytes.extend_from_slice(marker.as_bytes());
        let scene = parse(&bytes).expect("scene");
        assert_eq!(scene.texts[0].text.as_ref(), "\u{56fe}\u{7eb8}");
        assert_eq!(scene.acad_version.as_deref(), Some("AC1021"));
    }

    #[test]
    fn dwg_and_binary_dxf_are_rejected_with_a_reason() {
        assert!(matches!(
            parse(b"AC1032\x00\x00\x00\x00"),
            Err(CadError::Dwg)
        ));
        assert!(matches!(
            parse(b"AutoCAD Binary DXF\r\n\x1a\x00"),
            Err(CadError::BinaryDxf)
        ));
        assert!(matches!(parse(b"not a drawing"), Err(CadError::NotDxf)));
        assert!(matches!(
            parse(b"0\nSECTION\n2\nENTITIES\n0\nENDSEC\n0\nEOF\n"),
            Err(CadError::Empty)
        ));
    }

    #[test]
    fn text_codes_are_stripped() {
        assert_eq!(strip_text_codes("Hello %%d"), "Hello \u{b0}");
        assert_eq!(strip_text_codes("A\\PB"), "A\nB");
        assert_eq!(strip_text_codes("{\\fArial|b0;X}"), "X");
        assert_eq!(strip_text_codes("\\S1^2;"), "1/2");
        assert_eq!(strip_text_codes("100%%%"), "100%%");
    }

    #[test]
    fn transform_composition_matches_nested_blocks() {
        let inner = insertion_transform([0.0; 2], [1.0, 0.0], [2.0, 2.0], 0.0);
        let outer = insertion_transform([0.0; 2], [0.0, 0.0], [1.0, 1.0], 0.0);
        let combined = inner.then(&outer);
        assert_eq!(combined.apply([0.0, 0.0]), [1.0, 0.0]);
        assert_eq!(combined.apply([1.0, 0.0]), [3.0, 0.0]);
        assert_eq!(combined.rotation, 0.0);
        assert_eq!(combined.scale, [2.0, 2.0]);
    }

    #[test]
    fn parsing_tolerates_crlf_and_a_missing_trailing_newline() {
        let bytes = minimal_dxf("0\nLINE\n8\n0\n10\n0.0\n20\n0.0\n11\n1.0\n21\n1.0\n");
        let text = String::from_utf8(bytes).expect("ascii");
        let crlf = text.replace('\n', "\r\n");
        let trimmed = crlf.trim_end().to_owned();
        let scene = parse(trimmed.as_bytes()).expect("scene");
        assert_eq!(scene.paths.len(), 1);
    }
}

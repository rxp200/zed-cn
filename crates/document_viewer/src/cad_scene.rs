//! Compact render model for 2D CAD drawings.
//!
//! The DXF parser flattens a drawing into one shared point pool plus per
//! primitive ranges, so a drawing with a million segments costs a bounded,
//! predictable amount of memory instead of one heap allocation per entity.

use gpui::SharedString;

/// Point budget shared by every primitive in one drawing. A drawing is
/// truncated (and reports it) rather than growing without bound.
pub const MAX_POINTS: usize = 2_000_000;
pub const MAX_PATHS: usize = 1_000_000;
pub const MAX_FILLS: usize = 200_000;
pub const MAX_TEXTS: usize = 50_000;
pub const MAX_LAYERS: usize = 4_000;
/// Points retained for block definitions, which are expanded into the scene.
pub const MAX_BLOCK_POINTS: usize = 1_000_000;
/// How deeply block references may nest before they stop being expanded.
pub const MAX_INSERT_DEPTH: u8 = 4;

/// Circular arcs are tessellated at a fixed angular step. 5 degrees keeps the
/// chord sag under half a pixel for a circle up to about a thousand pixels
/// across, and makes the cost of one arc independent of the viewport.
pub const ARC_STEP_DEGREES: f64 = 5.0;
const MAX_ARC_STEPS: usize = 512;

/// A per-primitive color, in the same encoding as [`CadLayer::color`].
///
/// `0` inherits the layer color, `1..=255` is an AutoCAD Color Index value and
/// bit 24 marks a 24-bit true color in the low three bytes.
pub type CadColor = u32;

pub const COLOR_INHERIT: CadColor = 0;
pub const COLOR_TRUECOLOR_BIT: u32 = 0x0100_0000;

/// AutoCAD Color Index 7 is rendered as the background's contrasting color
/// rather than as literal white, so a drawing is readable on light themes.
pub const ACI_FOREGROUND: i16 = 7;

pub fn encode_aci(index: i16) -> CadColor {
    if index <= 0 {
        COLOR_INHERIT
    } else if index <= 255 {
        index as u32
    } else {
        COLOR_INHERIT
    }
}

pub fn encode_truecolor(rgb: u32) -> CadColor {
    (rgb & 0x00FF_FFFF) | COLOR_TRUECOLOR_BIT
}

/// Resolves an encoded color to RGB. `layer` is the owning layer's encoding, so
/// an inherited or by-block color falls back to the layer color and finally to
/// the palette's foreground.
pub fn resolve_color(color: CadColor, layer: CadColor, palette: &CadPalette) -> [u8; 3] {
    for candidate in [color, layer] {
        if candidate & COLOR_TRUECOLOR_BIT != 0 {
            let rgb = candidate & 0x00FF_FFFF;
            return [(rgb >> 16) as u8, (rgb >> 8) as u8, (rgb & 0xFF) as u8];
        }
        match candidate {
            0 => {}
            index if index <= 255 => return palette.aci[index as usize],
            _ => {}
        }
    }
    palette.foreground
}

/// The 256-entry AutoCAD Color Index palette, resolved once per frame on the
/// UI thread so the rasterizer stays free of theme state.
#[derive(Clone, PartialEq)]
pub struct CadPalette {
    pub foreground: [u8; 3],
    aci: [[u8; 3]; 256],
}

impl CadPalette {
    /// Builds the palette, substituting `foreground` for ACI 7 so white-on-dark
    /// and black-on-light drawings both read correctly.
    pub fn new(foreground: [u8; 3]) -> Self {
        let mut aci = ACI_RGB;
        aci[ACI_FOREGROUND as usize] = foreground;
        Self { foreground, aci }
    }
}

/// Bounds of a drawing in world coordinates (DXF units).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CadBounds {
    pub min: [f64; 2],
    pub max: [f64; 2],
}

impl CadBounds {
    pub fn new(point: [f64; 2]) -> Self {
        Self {
            min: point,
            max: point,
        }
    }

    pub fn include(&mut self, point: [f64; 2]) {
        self.min[0] = self.min[0].min(point[0]);
        self.min[1] = self.min[1].min(point[1]);
        self.max[0] = self.max[0].max(point[0]);
        self.max[1] = self.max[1].max(point[1]);
    }

    pub fn center(&self) -> [f64; 2] {
        [
            (self.min[0] + self.max[0]) / 2.0,
            (self.min[1] + self.max[1]) / 2.0,
        ]
    }

    pub fn width(&self) -> f64 {
        self.max[0] - self.min[0]
    }

    pub fn height(&self) -> f64 {
        self.max[1] - self.min[1]
    }

    /// The diagonal length, used to derive zoom-independent tolerances.
    pub fn diagonal(&self) -> f64 {
        (self.width() * self.width() + self.height() * self.height()).sqrt()
    }

    /// Grows the bounds by a fraction of its own size, so a flat drawing still
    /// has a usable viewport.
    pub fn expanded(&self, fraction: f64) -> Self {
        let margin = self.diagonal().max(self.width().max(self.height())) * fraction;
        let margin = if margin.is_finite() && margin > 0.0 {
            margin
        } else {
            1.0
        };
        Self {
            min: [self.min[0] - margin, self.min[1] - margin],
            max: [self.max[0] + margin, self.max[1] + margin],
        }
    }
}

#[derive(Clone, Debug)]
pub struct CadLayer {
    pub name: SharedString,
    /// Encoded color, see [`CadColor`].
    pub color: CadColor,
    pub frozen: bool,
    /// Whether the reader currently draws this layer.
    pub visible: bool,
}

/// A polyline stored as a range into [`CadScene::points`].
#[derive(Clone, Copy, Debug)]
pub struct CadPath {
    pub start: u32,
    pub len: u32,
    pub layer: u16,
    pub color: CadColor,
    pub closed: bool,
}

/// A filled polygon stored as a range into [`CadScene::points`].
#[derive(Clone, Copy, Debug)]
pub struct CadFill {
    pub start: u32,
    pub len: u32,
    pub layer: u16,
    pub color: CadColor,
}

#[derive(Clone, Debug)]
pub struct CadText {
    pub position: [f64; 2],
    pub height: f64,
    pub rotation: f64,
    pub layer: u16,
    pub color: CadColor,
    pub text: SharedString,
}

/// One drawing, flattened for rendering.
pub struct CadScene {
    pub points: Vec<[f64; 2]>,
    pub paths: Vec<CadPath>,
    pub fills: Vec<CadFill>,
    pub texts: Vec<CadText>,
    pub layers: Vec<CadLayer>,
    pub bounds: Option<CadBounds>,
    /// `$INSUNITS`, kept for the status bar.
    pub units: Option<i32>,
    pub acad_version: Option<SharedString>,
    pub code_page: Option<SharedString>,
    /// Entities skipped because they live in a paper-space layout.
    pub paper_space_entities: u32,
    /// Unsupported entity types and how often they were seen.
    pub skipped: Vec<(SharedString, u32)>,
    /// True once a budget stopped the parser from storing more geometry.
    pub truncated: bool,
}

impl Default for CadScene {
    fn default() -> Self {
        Self::new()
    }
}

impl CadScene {
    pub fn new() -> Self {
        Self {
            points: Vec::new(),
            paths: Vec::new(),
            fills: Vec::new(),
            texts: Vec::new(),
            layers: Vec::new(),
            bounds: None,
            units: None,
            acad_version: None,
            code_page: None,
            paper_space_entities: 0,
            skipped: Vec::new(),
            truncated: false,
        }
    }

    /// Whether the drawing has anything worth rendering.
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.fills.is_empty() && self.texts.is_empty()
    }

    pub fn layer(&self, index: u16) -> &CadLayer {
        &self.layers[index as usize]
    }

    /// The layer visibility a reader starts from: a layer that the file turned
    /// off or froze is hidden, everything else is drawn.
    pub fn initial_visibility(&self) -> Vec<bool> {
        self.layers
            .iter()
            .map(|layer| layer.visible && !layer.frozen)
            .collect()
    }

    /// Index of the implicit `0` layer, which DXF gives to most block geometry.
    pub fn zero_layer(&self) -> Option<u16> {
        self.layers
            .iter()
            .position(|layer| layer.name.as_ref() == "0")
            .map(|index| index as u16)
    }

    pub fn note_skipped(&mut self, kind: &str) {
        if let Some(entry) = self
            .skipped
            .iter_mut()
            .find(|entry| entry.0.as_ref() == kind)
        {
            entry.1 = entry.1.saturating_add(1);
        } else {
            self.skipped.push((SharedString::from(kind.to_owned()), 1));
        }
    }

    pub fn skipped_total(&self) -> u32 {
        self.skipped.iter().map(|entry| entry.1).sum()
    }

    /// Appends a polyline. Returns false when a budget is exhausted, which also
    /// sets [`CadScene::truncated`].
    pub fn push_path(
        &mut self,
        points: &[[f64; 2]],
        closed: bool,
        layer: u16,
        color: CadColor,
    ) -> bool {
        self.push_path_inner(points, closed, layer, color, true)
    }

    /// Appends a polyline without extending the drawing bounds, used for
    /// geometry that was clipped against bounds that are already known.
    pub fn push_clipped_path(
        &mut self,
        points: &[[f64; 2]],
        closed: bool,
        layer: u16,
        color: CadColor,
    ) -> bool {
        self.push_path_inner(points, closed, layer, color, false)
    }

    fn push_path_inner(
        &mut self,
        points: &[[f64; 2]],
        closed: bool,
        layer: u16,
        color: CadColor,
        extend_bounds: bool,
    ) -> bool {
        if points.len() < 2 {
            return true;
        }
        if self.paths.len() >= MAX_PATHS || self.points.len() + points.len() > MAX_POINTS {
            self.truncated = true;
            return false;
        }
        let start = self.points.len() as u32;
        for point in points {
            if !point[0].is_finite() || !point[1].is_finite() {
                self.truncated = true;
                return false;
            }
            if extend_bounds {
                self.include_in_bounds(*point);
            }
        }
        self.points.extend_from_slice(points);
        self.paths.push(CadPath {
            start,
            len: points.len() as u32,
            layer,
            color,
            closed,
        });
        true
    }

    pub fn push_fill(&mut self, points: &[[f64; 2]], layer: u16, color: CadColor) -> bool {
        if points.len() < 3 {
            return true;
        }
        if self.fills.len() >= MAX_FILLS || self.points.len() + points.len() > MAX_POINTS {
            self.truncated = true;
            return false;
        }
        let start = self.points.len() as u32;
        for point in points {
            if !point[0].is_finite() || !point[1].is_finite() {
                self.truncated = true;
                return false;
            }
            self.include_in_bounds(*point);
        }
        self.points.extend_from_slice(points);
        self.fills.push(CadFill {
            start,
            len: points.len() as u32,
            layer,
            color,
        });
        true
    }

    pub fn push_text(
        &mut self,
        text: &str,
        position: [f64; 2],
        height: f64,
        rotation: f64,
        layer: u16,
        color: CadColor,
    ) -> bool {
        if text.is_empty() {
            return true;
        }
        if self.texts.len() >= MAX_TEXTS {
            self.truncated = true;
            return false;
        }
        self.include_in_bounds(position);
        self.texts.push(CadText {
            position,
            height,
            rotation,
            layer,
            color,
            text: SharedString::from(text.to_owned()),
        });
        true
    }

    pub fn push_layer(&mut self, layer: CadLayer) -> u16 {
        if self.layers.len() >= MAX_LAYERS {
            self.truncated = true;
            // Overflow layers share the first one instead of growing forever.
            return 0;
        }
        self.layers.push(layer);
        (self.layers.len() - 1) as u16
    }

    fn include_in_bounds(&mut self, point: [f64; 2]) {
        if !point[0].is_finite() || !point[1].is_finite() {
            return;
        }
        match &mut self.bounds {
            Some(bounds) => bounds.include(point),
            None => self.bounds = Some(CadBounds::new(point)),
        }
    }

    /// Bounds of the currently visible layers, used by "fit to drawing".
    pub fn visible_bounds(&self, visible: &[bool]) -> Option<CadBounds> {
        let visible_layer = |layer: u16| visible.get(layer as usize).copied().unwrap_or(false);
        let mut bounds: Option<CadBounds> = None;
        let include = |point: [f64; 2], bounds: &mut Option<CadBounds>| match bounds {
            Some(bounds) => bounds.include(point),
            None => *bounds = Some(CadBounds::new(point)),
        };
        for path in &self.paths {
            if !visible_layer(path.layer) {
                continue;
            }
            for point in &self.points[path.start as usize..(path.start + path.len) as usize] {
                include(*point, &mut bounds);
            }
        }
        for fill in &self.fills {
            if !visible_layer(fill.layer) {
                continue;
            }
            for point in &self.points[fill.start as usize..(fill.start + fill.len) as usize] {
                include(*point, &mut bounds);
            }
        }
        for text in &self.texts {
            if visible_layer(text.layer) {
                include(text.position, &mut bounds);
            }
        }
        bounds
    }

    /// Number of primitives the reader will draw right now.
    pub fn visible_primitive_count(&self, visible: &[bool]) -> usize {
        let visible_layer = |layer: u16| visible.get(layer as usize).copied().unwrap_or(false);
        let paths = self
            .paths
            .iter()
            .filter(|path| visible_layer(path.layer))
            .count();
        let fills = self
            .fills
            .iter()
            .filter(|fill| visible_layer(fill.layer))
            .count();
        let texts = self
            .texts
            .iter()
            .filter(|text| visible_layer(text.layer))
            .count();
        paths + fills + texts
    }
}

/// Splits `sweep_degrees` into steps of at most [`ARC_STEP_DEGREES`].
fn arc_steps(sweep: f64) -> usize {
    let steps = (sweep.abs() / ARC_STEP_DEGREES).ceil();
    if !steps.is_finite() || steps <= 0.0 {
        return 1;
    }
    (steps as usize).clamp(1, MAX_ARC_STEPS)
}

/// Appends a circular arc starting after `start` and ending on `end`, sweeping
/// counter-clockwise when `sweep` is positive. The caller supplies the start
/// point so consecutive segments share their endpoints exactly.
pub fn push_arc_points(
    points: &mut Vec<[f64; 2]>,
    center: [f64; 2],
    radius: f64,
    start: f64,
    sweep: f64,
) {
    if !radius.is_finite() || radius <= 0.0 || !sweep.is_finite() || sweep == 0.0 {
        return;
    }
    let steps = arc_steps(sweep.to_degrees());
    for step in 1..=steps {
        let angle = start + sweep * (step as f64 / steps as f64);
        points.push([
            center[0] + radius * angle.cos(),
            center[1] + radius * angle.sin(),
        ]);
    }
}

/// Appends the arc described by the DXF bulge factor, between two vertices.
pub fn push_bulge_points(points: &mut Vec<[f64; 2]>, start: [f64; 2], end: [f64; 2], bulge: f64) {
    let chord = [end[0] - start[0], end[1] - start[1]];
    let length = (chord[0] * chord[0] + chord[1] * chord[1]).sqrt();
    if !bulge.is_finite() || bulge == 0.0 || length <= 0.0 {
        points.push(end);
        return;
    }
    let sweep = 4.0 * bulge.atan();
    let half = (sweep / 2.0).abs();
    let sin_half = half.sin();
    if sin_half.abs() < 1e-12 {
        points.push(end);
        return;
    }
    let radius = length / (2.0 * sin_half);
    let height = (radius * radius - (length / 2.0) * (length / 2.0))
        .max(0.0)
        .sqrt();
    let perpendicular = [-chord[1] / length, chord[0] / length];
    let sign = if bulge > 0.0 { 1.0 } else { -1.0 };
    let center = [
        start[0] + chord[0] / 2.0 + perpendicular[0] * height * sign,
        start[1] + chord[1] / 2.0 + perpendicular[1] * height * sign,
    ];
    let start_angle = (start[1] - center[1]).atan2(start[0] - center[0]);
    push_arc_points(points, center, radius, start_angle, sweep);
    // The parametric arc must land on the vertex exactly, otherwise a polyline
    // would show a one-step gap at every bulge.
    if let Some(last) = points.last_mut() {
        *last = end;
    }
}

/// Appends an ellipse arc defined by its center, major axis vector, minor/major
/// ratio and parametric sweep (radians).
pub fn push_ellipse_points(
    points: &mut Vec<[f64; 2]>,
    center: [f64; 2],
    major: [f64; 2],
    ratio: f64,
    start: f64,
    sweep: f64,
) {
    if !ratio.is_finite() || !sweep.is_finite() || sweep == 0.0 {
        return;
    }
    let minor = [-major[1] * ratio, major[0] * ratio];
    let steps = arc_steps(sweep.to_degrees());
    for step in 1..=steps {
        let parameter = start + sweep * (step as f64 / steps as f64);
        let (sin, cos) = parameter.sin_cos();
        points.push([
            center[0] + major[0] * cos + minor[0] * sin,
            center[1] + major[1] * cos + minor[1] * sin,
        ]);
    }
}

/// The canonical AutoCAD Color Index palette, indexed directly by ACI value
/// (element 0 is ACI 0).
///
/// It is fixed reference data shared by every DXF implementation; these values
/// match the tables published by ezdxf, libdxfrw and dxf-parser.
#[rustfmt::skip]
const ACI_RGB: [[u8; 3]; 256] = [
    [0x00, 0x00, 0x00], [0xFF, 0x00, 0x00], [0xFF, 0xFF, 0x00], [0x00, 0xFF, 0x00], [0x00, 0xFF, 0xFF], [0x00, 0x00, 0xFF], [0xFF, 0x00, 0xFF], [0xFF, 0xFF, 0xFF],
    [0x80, 0x80, 0x80], [0xC0, 0xC0, 0xC0], [0xFF, 0x00, 0x00], [0xFF, 0x7F, 0x7F], [0xCC, 0x00, 0x00], [0xCC, 0x66, 0x66], [0x99, 0x00, 0x00], [0x99, 0x4C, 0x4C],
    [0x7F, 0x00, 0x00], [0x7F, 0x3F, 0x3F], [0x4C, 0x00, 0x00], [0x4C, 0x26, 0x26], [0xFF, 0x3F, 0x00], [0xFF, 0x9F, 0x7F], [0xCC, 0x33, 0x00], [0xCC, 0x7F, 0x66],
    [0x99, 0x26, 0x00], [0x99, 0x5F, 0x4C], [0x7F, 0x1F, 0x00], [0x7F, 0x4F, 0x3F], [0x4C, 0x13, 0x00], [0x4C, 0x2F, 0x26], [0xFF, 0x7F, 0x00], [0xFF, 0xBF, 0x7F],
    [0xCC, 0x66, 0x00], [0xCC, 0x99, 0x66], [0x99, 0x4C, 0x00], [0x99, 0x72, 0x4C], [0x7F, 0x3F, 0x00], [0x7F, 0x5F, 0x3F], [0x4C, 0x26, 0x00], [0x4C, 0x39, 0x26],
    [0xFF, 0xBF, 0x00], [0xFF, 0xDF, 0x7F], [0xCC, 0x99, 0x00], [0xCC, 0xB2, 0x66], [0x99, 0x72, 0x00], [0x99, 0x85, 0x4C], [0x7F, 0x5F, 0x00], [0x7F, 0x6F, 0x3F],
    [0x4C, 0x39, 0x00], [0x4C, 0x42, 0x26], [0xFF, 0xFF, 0x00], [0xFF, 0xFF, 0x7F], [0xCC, 0xCC, 0x00], [0xCC, 0xCC, 0x66], [0x98, 0x98, 0x00], [0x98, 0x98, 0x4C],
    [0x7F, 0x7F, 0x00], [0x7F, 0x7F, 0x3F], [0x4C, 0x4C, 0x00], [0x4C, 0x4C, 0x26], [0xBF, 0xFF, 0x00], [0xDF, 0xFF, 0x7F], [0x99, 0xCC, 0x00], [0xB2, 0xCC, 0x66],
    [0x72, 0x98, 0x00], [0x85, 0x98, 0x4C], [0x5F, 0x7F, 0x00], [0x6F, 0x7F, 0x3F], [0x39, 0x4C, 0x00], [0x42, 0x4C, 0x26], [0x7F, 0xFF, 0x00], [0xBF, 0xFF, 0x7F],
    [0x66, 0xCC, 0x00], [0x99, 0xCC, 0x66], [0x4C, 0x98, 0x00], [0x72, 0x98, 0x4C], [0x3F, 0x7F, 0x00], [0x5F, 0x7F, 0x3F], [0x26, 0x4C, 0x00], [0x39, 0x4C, 0x26],
    [0x3F, 0xFF, 0x00], [0x9F, 0xFF, 0x7F], [0x33, 0xCC, 0x00], [0x7F, 0xCC, 0x66], [0x26, 0x98, 0x00], [0x5F, 0x98, 0x4C], [0x1F, 0x7F, 0x00], [0x4F, 0x7F, 0x3F],
    [0x13, 0x4C, 0x00], [0x2F, 0x4C, 0x26], [0x00, 0xFF, 0x00], [0x7F, 0xFF, 0x7F], [0x00, 0xCC, 0x00], [0x66, 0xCC, 0x66], [0x00, 0x98, 0x00], [0x4C, 0x98, 0x4C],
    [0x00, 0x7F, 0x00], [0x3F, 0x7F, 0x3F], [0x00, 0x4C, 0x00], [0x26, 0x4C, 0x26], [0x00, 0xFF, 0x3F], [0x7F, 0xFF, 0x9F], [0x00, 0xCC, 0x33], [0x66, 0xCC, 0x7F],
    [0x00, 0x98, 0x26], [0x4C, 0x98, 0x5F], [0x00, 0x7F, 0x1F], [0x3F, 0x7F, 0x4F], [0x00, 0x4C, 0x13], [0x26, 0x4C, 0x2F], [0x00, 0xFF, 0x7F], [0x7F, 0xFF, 0xBF],
    [0x00, 0xCC, 0x66], [0x66, 0xCC, 0x99], [0x00, 0x98, 0x4C], [0x4C, 0x98, 0x72], [0x00, 0x7F, 0x3F], [0x3F, 0x7F, 0x5F], [0x00, 0x4C, 0x26], [0x26, 0x4C, 0x39],
    [0x00, 0xFF, 0xBF], [0x7F, 0xFF, 0xDF], [0x00, 0xCC, 0x99], [0x66, 0xCC, 0xB2], [0x00, 0x98, 0x72], [0x4C, 0x98, 0x85], [0x00, 0x7F, 0x5F], [0x3F, 0x7F, 0x6F],
    [0x00, 0x4C, 0x39], [0x26, 0x4C, 0x42], [0x00, 0xFF, 0xFF], [0x7F, 0xFF, 0xFF], [0x00, 0xCC, 0xCC], [0x66, 0xCC, 0xCC], [0x00, 0x98, 0x98], [0x4C, 0x98, 0x98],
    [0x00, 0x7F, 0x7F], [0x3F, 0x7F, 0x7F], [0x00, 0x4C, 0x4C], [0x26, 0x4C, 0x4C], [0x00, 0xBF, 0xFF], [0x7F, 0xDF, 0xFF], [0x00, 0x99, 0xCC], [0x66, 0xB2, 0xCC],
    [0x00, 0x72, 0x98], [0x4C, 0x85, 0x98], [0x00, 0x5F, 0x7F], [0x3F, 0x6F, 0x7F], [0x00, 0x39, 0x4C], [0x26, 0x42, 0x4C], [0x00, 0x7F, 0xFF], [0x7F, 0xBF, 0xFF],
    [0x00, 0x66, 0xCC], [0x66, 0x99, 0xCC], [0x00, 0x4C, 0x98], [0x4C, 0x72, 0x98], [0x00, 0x3F, 0x7F], [0x3F, 0x5F, 0x7F], [0x00, 0x26, 0x4C], [0x26, 0x39, 0x4C],
    [0x00, 0x3F, 0xFF], [0x7F, 0x9F, 0xFF], [0x00, 0x33, 0xCC], [0x66, 0x7F, 0xCC], [0x00, 0x26, 0x98], [0x4C, 0x5F, 0x98], [0x00, 0x1F, 0x7F], [0x3F, 0x4F, 0x7F],
    [0x00, 0x13, 0x4C], [0x26, 0x2F, 0x4C], [0x00, 0x00, 0xFF], [0x7F, 0x7F, 0xFF], [0x00, 0x00, 0xCC], [0x66, 0x66, 0xCC], [0x00, 0x00, 0x98], [0x4C, 0x4C, 0x98],
    [0x00, 0x00, 0x7F], [0x3F, 0x3F, 0x7F], [0x00, 0x00, 0x4C], [0x26, 0x26, 0x4C], [0x3F, 0x00, 0xFF], [0x9F, 0x7F, 0xFF], [0x33, 0x00, 0xCC], [0x7F, 0x66, 0xCC],
    [0x26, 0x00, 0x98], [0x5F, 0x4C, 0x98], [0x1F, 0x00, 0x7F], [0x4F, 0x3F, 0x7F], [0x13, 0x00, 0x4C], [0x2F, 0x26, 0x4C], [0x7F, 0x00, 0xFF], [0xBF, 0x7F, 0xFF],
    [0x66, 0x00, 0xCC], [0x99, 0x66, 0xCC], [0x4C, 0x00, 0x98], [0x72, 0x4C, 0x98], [0x3F, 0x00, 0x7F], [0x5F, 0x3F, 0x7F], [0x26, 0x00, 0x4C], [0x39, 0x26, 0x4C],
    [0xBF, 0x00, 0xFF], [0xDF, 0x7F, 0xFF], [0x99, 0x00, 0xCC], [0xB2, 0x66, 0xCC], [0x72, 0x00, 0x98], [0x85, 0x4C, 0x98], [0x5F, 0x00, 0x7F], [0x6F, 0x3F, 0x7F],
    [0x39, 0x00, 0x4C], [0x42, 0x26, 0x4C], [0xFF, 0x00, 0xFF], [0xFF, 0x7F, 0xFF], [0xCC, 0x00, 0xCC], [0xCC, 0x66, 0xCC], [0x98, 0x00, 0x98], [0x98, 0x4C, 0x98],
    [0x7F, 0x00, 0x7F], [0x7F, 0x3F, 0x7F], [0x4C, 0x00, 0x4C], [0x4C, 0x26, 0x4C], [0xFF, 0x00, 0xBF], [0xFF, 0x7F, 0xDF], [0xCC, 0x00, 0x99], [0xCC, 0x66, 0xB2],
    [0x98, 0x00, 0x72], [0x98, 0x4C, 0x85], [0x7F, 0x00, 0x5F], [0x7F, 0x3F, 0x6F], [0x4C, 0x00, 0x39], [0x4C, 0x26, 0x42], [0xFF, 0x00, 0x7F], [0xFF, 0x7F, 0xBF],
    [0xCC, 0x00, 0x66], [0xCC, 0x66, 0x99], [0x98, 0x00, 0x4C], [0x98, 0x4C, 0x72], [0x7F, 0x00, 0x3F], [0x7F, 0x3F, 0x5F], [0x4C, 0x00, 0x26], [0x4C, 0x26, 0x39],
    [0xFF, 0x00, 0x3F], [0xFF, 0x7F, 0x9F], [0xCC, 0x00, 0x33], [0xCC, 0x66, 0x7F], [0x98, 0x00, 0x26], [0x98, 0x4C, 0x5F], [0x7F, 0x00, 0x1F], [0x7F, 0x3F, 0x4F],
    [0x4C, 0x00, 0x13], [0x4C, 0x26, 0x2F], [0x33, 0x33, 0x33], [0x5B, 0x5B, 0x5B], [0x84, 0x84, 0x84], [0xAD, 0xAD, 0xAD], [0xD6, 0xD6, 0xD6], [0xFF, 0xFF, 0xFF],
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aci_matches_the_reference_palette() {
        // A foreground the palette cannot produce by accident, so the ACI 7
        // substitution below is unambiguous.
        let palette = CadPalette::new([1, 2, 3]);
        for (index, expected) in [
            (1, [255, 0, 0]),
            (2, [255, 255, 0]),
            (7, [1, 2, 3]),
            (8, [128, 128, 128]),
            (11, [255, 127, 127]),
            (20, [255, 63, 0]),
            (30, [255, 127, 0]),
            (255, [255, 255, 255]),
        ] {
            assert_eq!(
                resolve_color(encode_aci(index), COLOR_INHERIT, &palette),
                expected,
                "ACI {index}"
            );
        }
    }

    #[test]
    fn foreground_substitutes_for_aci_seven() {
        let palette = CadPalette::new([10, 20, 30]);
        assert_eq!(
            resolve_color(encode_aci(7), COLOR_INHERIT, &palette),
            [10, 20, 30]
        );
        assert_eq!(
            resolve_color(COLOR_INHERIT, encode_aci(1), &palette),
            [255, 0, 0]
        );
        assert_eq!(
            resolve_color(encode_truecolor(0x123456), encode_aci(1), &palette),
            [0x12, 0x34, 0x56]
        );
        assert_eq!(
            resolve_color(COLOR_INHERIT, COLOR_INHERIT, &palette),
            [10, 20, 30]
        );
    }

    #[test]
    fn bulge_arcs_land_on_their_endpoints() {
        let mut points = vec![[0.0, 0.0]];
        push_bulge_points(&mut points, [0.0, 0.0], [1.0, 0.0], 1.0);
        assert_eq!(points.last(), Some(&[1.0, 0.0]));
        // A semicircle bulging up must pass below the chord: the control point
        // is at (0.5, -0.5) for a counter-clockwise sweep.
        assert!(
            points.iter().any(|point| point[1] < -0.49),
            "bulge 1.0 sweeps counter-clockwise: {points:?}"
        );
        assert!(points.len() > 10);

        let mut negative = vec![[0.0, 0.0]];
        push_bulge_points(&mut negative, [0.0, 0.0], [1.0, 0.0], -1.0);
        assert!(negative.iter().any(|point| point[1] > 0.49));
        assert_eq!(negative.last(), Some(&[1.0, 0.0]));
    }

    #[test]
    fn arc_tessellation_is_bounded_and_adapts_to_sweep() {
        let mut quarter = Vec::new();
        push_arc_points(
            &mut quarter,
            [0.0, 0.0],
            10.0,
            0.0,
            std::f64::consts::FRAC_PI_2,
        );
        assert_eq!(quarter.len(), 18);

        let mut full = Vec::new();
        push_arc_points(&mut full, [0.0, 0.0], 10.0, 0.0, std::f64::consts::TAU);
        assert_eq!(full.len(), 72);
        // Every tessellated point sits on the circle.
        for point in &full {
            let radius = (point[0] * point[0] + point[1] * point[1]).sqrt();
            assert!((radius - 10.0).abs() < 1e-9, "{point:?}");
        }
    }

    #[test]
    fn scene_budgets_are_enforced() {
        let mut scene = CadScene::new();
        assert!(scene.push_path(&[[0.0, 0.0], [1.0, 1.0]], false, 0, COLOR_INHERIT));
        // Degenerate paths are dropped rather than stored.
        assert!(scene.push_path(&[[0.0, 0.0]], false, 0, COLOR_INHERIT));
        assert_eq!(scene.paths.len(), 1);
        assert!(!scene.is_empty());

        let mut text_scene = CadScene::new();
        for index in 0..MAX_TEXTS + 10 {
            text_scene.push_text("label", [index as f64, 0.0], 1.0, 0.0, 0, COLOR_INHERIT);
        }
        assert_eq!(text_scene.texts.len(), MAX_TEXTS);
        assert!(text_scene.truncated);
    }

    #[test]
    fn visible_bounds_ignore_hidden_layers() {
        let mut scene = CadScene::new();
        scene.push_layer(CadLayer {
            name: "0".into(),
            color: encode_aci(7),
            frozen: false,
            visible: true,
        });
        scene.push_layer(CadLayer {
            name: "hidden".into(),
            color: encode_aci(1),
            frozen: false,
            visible: true,
        });
        scene.push_path(&[[0.0, 0.0], [1.0, 1.0]], false, 0, COLOR_INHERIT);
        scene.push_path(&[[100.0, 100.0], [101.0, 101.0]], false, 1, COLOR_INHERIT);
        let all = scene
            .visible_bounds(&scene.initial_visibility())
            .expect("bounds");
        assert_eq!(all.max, [101.0, 101.0]);
        let mut visible = scene.initial_visibility();
        visible[1] = false;
        let hidden = scene.visible_bounds(&visible).expect("bounds");
        assert_eq!(hidden.max, [1.0, 1.0]);
        assert_eq!(scene.visible_primitive_count(&visible), 1);
    }
}

use crate::{
    DocumentItem,
    model_mesh::{ModelMesh, Topology, Vertex, cross, dot, sub},
    model_section::{self, Section, SectionAxis, SectionPlane},
};
use gpui::{
    App, Bounds, Context, Entity, FocusHandle, Focusable, Hsla, IntoElement, MouseButton, PathBuilder,
    Pixels, Point, RenderImage, Rgba, Task, Window, canvas, img, point, px, rgb,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use ui::prelude::*;
use ui::{
    ContextMenu, Divider, DropdownMenu, Indicator, ToggleButtonGroup, ToggleButtonGroupSize,
    ToggleButtonGroupStyle, ToggleButtonSimple, Tooltip,
};
use util::ResultExt as _;

const GIZMO_SIZE: f32 = 64.0;
const GIZMO_ARM: f32 = 20.0;
const GIZMO_LABEL_RADIUS: f32 = 27.0;

/// Cross-section capping stitches the cut curve into loops, which is only kept
/// affordable for meshes the topology inspection already walks in full.
const SECTION_CAP_TRIANGLES: usize = 300_000;

/// Depth offsets that keep the cut surface and its outline from z-fighting with
/// the geometry they share the cutting plane with.
const CAP_DEPTH_BIAS: f64 = 1e-4;
const CUT_DEPTH_BIAS: f64 = 2e-4;

/// Isometric view cube in the viewport's top-right corner. Its bounding square
/// is a top-face rhombus on top of two side faces; the three clickable regions
/// tile that square so every face is hit exactly once.
const VIEW_CUBE_WIDTH: f32 = 84.0;
const VIEW_CUBE_TOP_HEIGHT: f32 = 42.0;
const VIEW_CUBE_HEIGHT: f32 = 84.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shading {
    Solid,
    SolidEdges,
    Wireframe,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewPreset {
    Isometric,
    Front,
    Back,
    Right,
    Left,
    Top,
    Bottom,
}

impl ViewPreset {
    const ALL: [ViewPreset; 7] = [
        ViewPreset::Isometric,
        ViewPreset::Front,
        ViewPreset::Back,
        ViewPreset::Right,
        ViewPreset::Left,
        ViewPreset::Top,
        ViewPreset::Bottom,
    ];

    fn orientation(self) -> (f64, f64) {
        match self {
            ViewPreset::Isometric => (0.65, -0.4),
            ViewPreset::Front => (0.0, 0.0),
            ViewPreset::Back => (std::f64::consts::PI, 0.0),
            ViewPreset::Right => (std::f64::consts::FRAC_PI_2, 0.0),
            ViewPreset::Left => (-std::f64::consts::FRAC_PI_2, 0.0),
            ViewPreset::Top => (0.0, std::f64::consts::FRAC_PI_2),
            ViewPreset::Bottom => (0.0, -std::f64::consts::FRAC_PI_2),
        }
    }

    fn label(self) -> &'static str {
        match self {
            ViewPreset::Isometric => i18n::t!("d7cde03d36a8013c"),
            ViewPreset::Front => i18n::t!("a617590202898821"),
            ViewPreset::Back => i18n::t!("76900f1bfd16c8d4"),
            ViewPreset::Right => i18n::t!("883361d5d682a157"),
            ViewPreset::Left => i18n::t!("58eb9032e3bb83f0"),
            ViewPreset::Top => i18n::t!("d5cdfcf7ff75338f"),
            ViewPreset::Bottom => i18n::t!("479ad21ac9096236"),
        }
    }
}

#[derive(Clone, Copy)]
struct Camera {
    yaw: f64,
    pitch: f64,
    zoom: f64,
    pan: [f64; 2],
    perspective: bool,
    shading: Shading,
    grid: bool,
    axes: bool,
}
impl Default for Camera {
    fn default() -> Self {
        Self {
            yaw: 0.65,
            pitch: -0.4,
            zoom: 1.0,
            pan: [0.0; 2],
            perspective: true,
            shading: Shading::SolidEdges,
            grid: true,
            axes: true,
        }
    }
}

/// Theme-derived colors for the software rasterizer, captured on the main
/// thread so the background render task never touches theme state.
#[derive(Clone, Copy, PartialEq)]
struct RasterStyle {
    model: [u8; 3],
    edge: [u8; 3],
    wire: [u8; 3],
    grid: [u8; 4],
    section: [u8; 3],
    cut: [u8; 3],
}

/// Cutting plane plus the cross-section loops built for it.
struct SectionRender {
    plane: SectionPlane,
    cap: Option<Arc<Section>>,
}

/// Cutting plane selection of the viewer.
#[derive(Clone, Copy, PartialEq)]
struct SectionState {
    enabled: bool,
    plane: SectionPlane,
    capped: bool,
}

impl Default for SectionState {
    fn default() -> Self {
        Self {
            enabled: false,
            // Cutting away the half that the default isometric view looks at
            // makes the model read as a section as soon as it is enabled.
            plane: SectionPlane {
                axis: SectionAxis::Z,
                offset: 0.5,
                flipped: true,
            },
            capped: true,
        }
    }
}

fn color_rgb(color: impl Into<Rgba>) -> [u8; 3] {
    let color = color.into();
    [
        (color.r * 255.0) as u8,
        (color.g * 255.0) as u8,
        (color.b * 255.0) as u8,
    ]
}

pub struct ModelReader {
    focus: FocusHandle,
    mesh: Arc<ModelMesh>,
    format: SharedString,
    file_size: u64,
    camera: Camera,
    preset: ViewPreset,
    view_cube_hover: Option<ViewPreset>,
    section: SectionState,
    section_geometry: Option<(SectionPlane, Arc<Section>)>,
    section_open: bool,
    section_track: Option<Bounds<Pixels>>,
    dragging_section: bool,
    panel_open: bool,
    geometry_open: bool,
    topology_open: bool,
    file_open: bool,
    topology: Option<Topology>,
    error: Option<SharedString>,
    image: Option<Arc<RenderImage>>,
    rendered_style: Option<RasterStyle>,
    dirty: bool,
    dimensions: [u32; 2],
    drag: Option<(Point<Pixels>, bool)>,
    cancel: Arc<AtomicBool>,
    render_task: Option<Task<()>>,
    inspect_task: Option<Task<()>>,
}

impl ModelReader {
    pub fn new(item: Entity<DocumentItem>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let window_handle = window.window_handle();
        cx.on_release(move |reader, cx| {
            reader.cancel.store(true, Ordering::Relaxed);
            reader.render_task = None;
            reader.inspect_task = None;
            if let Some(image) = reader.image.take() {
                window_handle
                    .update(cx, |_, window, _| window.drop_image(image))
                    .log_err();
            }
        })
        .detach();
        let (mesh, format, file_size) = {
            let item = item.read(cx);
            let format = item
                .file
                .path
                .extension()
                .map(|extension| extension.to_uppercase())
                .unwrap_or_default();
            let file_size = match item.file.disk_state {
                language::DiskState::Present { size, .. } => size,
                _ => 0,
            };
            (
                item.model.as_ref().expect("parsed model item").clone(),
                format,
                file_size,
            )
        };
        log::info!(
            "[open-debug] ModelReader::new format={} triangles={}",
            format,
            mesh.triangles.len()
        );
        let mut reader = Self {
            focus: cx.focus_handle(),
            mesh,
            format: format.into(),
            file_size,
            camera: Camera::default(),
            preset: ViewPreset::Isometric,
            view_cube_hover: None,
            section: SectionState::default(),
            section_geometry: None,
            section_open: true,
            section_track: None,
            dragging_section: false,
            panel_open: true,
            geometry_open: true,
            topology_open: true,
            file_open: true,
            topology: None,
            error: None,
            image: None,
            rendered_style: None,
            dirty: true,
            dimensions: [800, 600],
            drag: None,
            cancel: Arc::new(AtomicBool::new(false)),
            render_task: None,
            inspect_task: None,
        };
        // Topology is cheap enough for supported meshes and the panel presents
        // it immediately; oversized meshes stay lazy until explicitly requested.
        if reader.mesh.triangles.len() <= 300_000 {
            reader.inspect(cx);
        }
        reader
    }

    fn raster_style(&self, cx: &App) -> RasterStyle {
        let colors = cx.theme().colors();
        let model = color_rgb(colors.text_accent);
        RasterStyle {
            model,
            edge: model.map(|channel| (u16::from(channel) * 3 / 4 + 63) as u8),
            wire: color_rgb(colors.text_muted),
            grid: {
                let border: Rgba = colors.border.into();
                [
                    (border.r * 255.0) as u8,
                    (border.g * 255.0) as u8,
                    (border.b * 255.0) as u8,
                    110,
                ]
            },
            // A fixed warm hue stays distinguishable from the theme-colored model
            // surfaces and from the reference grid on light and dark themes alike.
            section: [186, 106, 74],
            cut: [246, 196, 150],
        }
    }

    fn request_render(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.dirty || self.render_task.is_some() {
            return;
        }
        self.dirty = false;
        let mesh = self.mesh.clone();
        let camera = self.camera;
        let style = self.rendered_style.unwrap_or(RasterStyle {
            model: [150, 160, 175],
            edge: [60, 64, 70],
            wire: [140, 140, 140],
            grid: [90, 90, 90, 110],
            section: [186, 106, 74],
            cut: [246, 196, 150],
        });
        let dimensions = self.dimensions;
        if self.image.is_none() {
            log::info!(
                "[open-debug] ModelReader first render dimensions={:?} triangles={}",
                dimensions,
                self.mesh.triangles.len()
            );
        }
        let cancel = self.cancel.clone();
        let plane = self.section.enabled.then_some(self.section.plane);
        let cached = self
            .section_geometry
            .as_ref()
            .filter(|(cached_plane, _)| Some(*cached_plane) == plane)
            .map(|(_, geometry)| geometry.clone());
        // Cap loops are independent of the camera, so they are built once per plane;
        // orbiting only re-runs the rasterizer.
        let build_cap = plane.is_some()
            && self.section.capped
            && self.mesh.triangles.len() <= SECTION_CAP_TRIANGLES
            && cached.is_none();
        let background = cx.background_spawn(async move {
            let cap = if build_cap {
                let (center, radius) = mesh.frame();
                let plane = plane?;
                Some(Arc::new(model_section::build(
                    &mesh, plane, center, radius, &cancel,
                )?))
            } else {
                cached
            };
            let section = plane.map(|plane| SectionRender { plane, cap });
            let rendered = rasterize(&mesh, camera, style, dimensions, section.as_ref(), &cancel)?;
            Some((rendered, plane, section.and_then(|section| section.cap)))
        });
        self.render_task = Some(cx.spawn_in(window, async move |this, cx| {
            let rendered = background.await;
            this.update_in(cx, |reader, window, cx| {
                reader.render_task = None;
                if let Some((image, plane, cap)) = rendered {
                    if let (Some(plane), Some(cap)) = (plane, cap) {
                        reader.section_geometry = Some((plane, cap));
                    }
                    if let Some(previous) = reader.image.replace(image) {
                        window.drop_image(previous).log_err();
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    fn inspect(&mut self, cx: &mut Context<Self>) {
        if self.inspect_task.is_some() {
            return;
        }
        // The edge table is deliberately temporary: only small summary counts survive inspection.
        let mesh = self.mesh.clone();
        if mesh.triangles.len() > 300_000 {
            self.error = Some(i18n::t!("18f6ec65fc213983").into());
            cx.notify();
            return;
        }
        let cancel = self.cancel.clone();
        let task = cx.background_spawn(async move { mesh.inspect(&cancel) });
        self.inspect_task = Some(cx.spawn(async move |this, cx| {
            let topology = task.await;
            this.update(cx, |reader, cx| {
                reader.topology = topology;
                reader.inspect_task = None;
                cx.notify();
            })
            .log_err();
        }));
    }

    fn set_preset(&mut self, preset: ViewPreset, cx: &mut Context<Self>) {
        let (yaw, pitch) = preset.orientation();
        self.camera.yaw = yaw;
        self.camera.pitch = pitch;
        self.preset = preset;
        self.dirty = true;
        cx.notify();
    }

    fn set_section_plane(&mut self, plane: SectionPlane, cx: &mut Context<Self>) {
        if self.section.plane == plane {
            return;
        }
        self.section.plane = plane;
        self.section_geometry = None;
        self.dirty = true;
        cx.notify();
    }

    fn set_section_capped(&mut self, capped: bool, cx: &mut Context<Self>) {
        if self.section.capped == capped {
            return;
        }
        self.section.capped = capped;
        self.section_geometry = None;
        self.dirty = true;
        cx.notify();
    }

    fn set_section_offset(&mut self, offset: f64, cx: &mut Context<Self>) {
        // 0.1% steps keep the label stable and bound how often the cap is rebuilt
        // while the position is dragged.
        let offset = (offset * 1000.0).round() / 1000.0;
        if (self.section.plane.offset - offset).abs() <= 1e-9 {
            return;
        }
        self.section.plane.offset = offset;
        self.section_geometry = None;
        self.dirty = true;
        cx.notify();
    }

    fn drag_section_offset(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(bounds) = self.section_track else {
            return;
        };
        let width = f64::from(bounds.size.width).max(1.0);
        let offset = (f64::from(position.x - bounds.origin.x) / width).clamp(0.0, 1.0);
        self.set_section_offset(offset, cx);
    }

    /// Caption of the selected preset, used by view-cube regression tests.
    #[cfg(test)]
    pub(crate) fn preset_label(&self) -> &'static str {
        self.preset.label()
    }

    fn mesh_closed(&self) -> bool {
        self.topology.as_ref().is_some_and(|topology| {
            topology.boundary_edges == 0
                && topology.non_manifold_edges == 0
                && topology.inconsistent_edges == 0
                && self.mesh.degenerate_faces == 0
        })
    }

    fn render_toolbar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let weak = cx.entity().downgrade();
        let current = self.preset;
        let view_menu =
            window.use_keyed_state(("model-view-menu", current as usize), cx, |window, cx| {
                ContextMenu::new(window, cx, |mut menu, _, _| {
                    for preset in ViewPreset::ALL {
                        let weak = weak.clone();
                        menu = menu.toggleable_entry(
                            preset.label(),
                            preset == current,
                            IconPosition::End,
                            None,
                            move |_, cx| {
                                weak.update(cx, |reader, cx| reader.set_preset(preset, cx))
                                    .log_err();
                            },
                        );
                    }
                    menu
                })
            });
        let perspective = self.camera.perspective;
        let shading = self.camera.shading;
        let grid = self.camera.grid;
        let axes = self.camera.axes;
        let section_enabled = self.section.enabled;
        let panel_open = self.panel_open;
        h_flex()
            .debug_selector(|| "model-toolbar".to_string())
            .flex_none()
            .gap_2()
            .px_3()
            .py_2()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                IconButton::new("model-reset", IconName::RotateCcw)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text(i18n::t!("ce89ae822e8ba072")))
                    .on_click(cx.listener(|reader, _, _, cx| {
                        reader.camera.yaw = ViewPreset::Isometric.orientation().0;
                        reader.camera.pitch = ViewPreset::Isometric.orientation().1;
                        reader.camera.zoom = 1.0;
                        reader.camera.pan = [0.0; 2];
                        reader.preset = ViewPreset::Isometric;
                        reader.dirty = true;
                        cx.notify();
                    })),
            )
            .child(
                DropdownMenu::new("model-view-dropdown", current.label(), view_menu)
                    .trigger_size(ButtonSize::Large),
            )
            .child(Divider::vertical())
            .child(
                ToggleButtonGroup::single_row(
                    "model-projection",
                    [
                        ToggleButtonSimple::new(i18n::t!("e3ad1bcc2cbd8537"), {
                            let weak = cx.entity().downgrade();
                            move |_, _, cx| {
                                weak.update(cx, |reader, cx| {
                                    reader.camera.perspective = true;
                                    reader.dirty = true;
                                    cx.notify();
                                })
                                .log_err();
                            }
                        }),
                        ToggleButtonSimple::new(i18n::t!("0f04d53421188d24"), {
                            let weak = cx.entity().downgrade();
                            move |_, _, cx| {
                                weak.update(cx, |reader, cx| {
                                    reader.camera.perspective = false;
                                    reader.dirty = true;
                                    cx.notify();
                                })
                                .log_err();
                            }
                        }),
                    ],
                )
                .auto_width()
                .size(ToggleButtonGroupSize::Large)
                .style(ToggleButtonGroupStyle::Outlined)
                .label_size(LabelSize::Default)
                .selected_index(usize::from(!perspective)),
            )
            .child(
                ToggleButtonGroup::single_row(
                    "model-shading",
                    [
                        ToggleButtonSimple::new(i18n::t!("b8b311b0f2739e0c"), {
                            let weak = cx.entity().downgrade();
                            move |_, _, cx| {
                                weak.update(cx, |reader, cx| {
                                    reader.camera.shading = Shading::Solid;
                                    reader.dirty = true;
                                    cx.notify();
                                })
                                .log_err();
                            }
                        }),
                        ToggleButtonSimple::new(i18n::t!("0a9ba68d289d8376"), {
                            let weak = cx.entity().downgrade();
                            move |_, _, cx| {
                                weak.update(cx, |reader, cx| {
                                    reader.camera.shading = Shading::SolidEdges;
                                    reader.dirty = true;
                                    cx.notify();
                                })
                                .log_err();
                            }
                        }),
                        ToggleButtonSimple::new(i18n::t!("04b765747018cb13"), {
                            let weak = cx.entity().downgrade();
                            move |_, _, cx| {
                                weak.update(cx, |reader, cx| {
                                    reader.camera.shading = Shading::Wireframe;
                                    reader.dirty = true;
                                    cx.notify();
                                })
                                .log_err();
                            }
                        }),
                    ],
                )
                .auto_width()
                .size(ToggleButtonGroupSize::Large)
                .style(ToggleButtonGroupStyle::Outlined)
                .label_size(LabelSize::Default)
                .selected_index(match shading {
                    Shading::Solid => 0,
                    Shading::SolidEdges => 1,
                    Shading::Wireframe => 2,
                }),
            )
            .child(Divider::vertical())
            .child(
                IconButton::new("model-section", IconName::Scissors)
                    .icon_size(IconSize::Small)
                    .toggle_state(section_enabled)
                    .tooltip(Tooltip::text(i18n::t!("dfca5da56b8cdb62")))
                    .on_click(cx.listener(|reader, _, _, cx| {
                        reader.section.enabled = !reader.section.enabled;
                        if reader.section.enabled {
                            reader.panel_open = true;
                        }
                        reader.dirty = true;
                        cx.notify();
                    })),
            )
            .child(Divider::vertical())
            .child(
                IconButton::new("model-grid", IconName::Table)
                    .icon_size(IconSize::Small)
                    .toggle_state(grid)
                    .tooltip(Tooltip::text(i18n::t!("873ebb9300e109b8")))
                    .on_click(cx.listener(|reader, _, _, cx| {
                        reader.camera.grid = !reader.camera.grid;
                        reader.dirty = true;
                        cx.notify();
                    })),
            )
            .child(
                IconButton::new("model-axes", IconName::Crosshair)
                    .icon_size(IconSize::Small)
                    .toggle_state(axes)
                    .tooltip(Tooltip::text(i18n::t!("890401de8beb4d43")))
                    .on_click(cx.listener(|reader, _, _, cx| {
                        reader.camera.axes = !reader.camera.axes;
                        cx.notify();
                    })),
            )
            .child(
                h_flex().ml_auto().child(
                    IconButton::new("model-panel", IconName::Split)
                        .icon_size(IconSize::Small)
                        .toggle_state(panel_open)
                        .tooltip(Tooltip::text(i18n::t!("b69d9c2a33105bc1")))
                        .on_click(cx.listener(|reader, _, _, cx| {
                            reader.panel_open = !reader.panel_open;
                            cx.notify();
                        })),
                ),
            )
    }

    fn render_panel(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let section_card = self.render_section_card(cx);
        let colors = cx.theme().colors();
        let dimensions = sub(self.mesh.maximum, self.mesh.minimum);
        let inspecting = self.inspect_task.is_some();
        let closed = self.mesh_closed();
        let topology = self.topology.as_ref();
        let geometry_open = self.geometry_open;
        let topology_open = self.topology_open;
        let file_open = self.file_open;
        v_flex()
            .debug_selector(|| "model-panel".to_string())
            .w(px(300.0))
            .flex_none()
            .h_full()
            .border_l_1()
            .border_color(colors.border)
            .bg(colors.panel_background)
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .justify_between()
                    .items_center()
                    .bg(colors.element_hover)
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                Icon::new(IconName::Info)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(Label::new(i18n::t!("02d1bbbbc90c39f3")).size(LabelSize::Small)),
                    )
                    .child(
                        IconButton::new("model-panel-close", IconName::Close)
                            .icon_size(IconSize::Small)
                            .on_click(cx.listener(|reader, _, _, cx| {
                                reader.panel_open = false;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                v_flex()
                    .id("model-panel-sections")
                    .p_3()
                    .gap_3()
                    .overflow_y_scroll()
                    .flex_1()
                    .min_h_0()
                    .child(model_card(cx).child(section_header(
                        "model-geometry-header",
                        i18n::t!("fa9da5754ccfd409"),
                        geometry_open,
                        cx,
                        cx.listener(|reader, _, _, cx| {
                            reader.geometry_open = !reader.geometry_open;
                            cx.notify();
                        }),
                    ))
                    .when(geometry_open, |section| {
                        section
                            .child(property_row(
                                i18n::t!("29b88538d1c54511"),
                                grouped(self.mesh.triangles.len()),
                            ))
                            .child(property_row(
                                i18n::t!("930586b2834930e5"),
                                topology
                                    .map(|topology| grouped(topology.vertices))
                                    .unwrap_or_else(|| i18n::t!("5b8aa0bf3c59daec").to_string()),
                            ))
                            .child(property_row(
                                i18n::t!("7452a2c6a7a497d8"),
                                format_quantity(dimensions[0]),
                            ))
                            .child(property_row(
                                i18n::t!("3677766bde03a33d"),
                                format_quantity(dimensions[1]),
                            ))
                            .child(property_row(
                                i18n::t!("291c0228365f2d7d"),
                                format_quantity(dimensions[2]),
                            ))
                            .child(property_row(
                                i18n::t!("9fb6669a77ea48bb"),
                                i18n::t!("5b8aa0bf3c59daec").to_string(),
                            ))
                            .when(closed, |section| {
                                section.child(property_row(
                                    i18n::t!("b10fb966d72063f5"),
                                    i18n::t!(
                                        "fb301391e0c9e9c9",
                                        value = format_quantity(self.mesh.signed_volume.abs())
                                    ),
                                ))
                            })
                    }))
                    .child(section_card)
                    .child(model_card(cx).child(section_header(
                        "model-topology-header",
                        i18n::t!("3a6c23b78b17490b"),
                        topology_open,
                        cx,
                        cx.listener(|reader, _, _, cx| {
                            reader.topology_open = !reader.topology_open;
                            cx.notify();
                        }),
                    ))
                    .when(topology_open, |section| {
                        let section = section
                            .child(check_row(
                                i18n::t!("0abb54d180cded75"),
                                topology.map(|topology| topology.boundary_edges),
                                inspecting,
                            ))
                            .child(check_row(
                                i18n::t!("830b893eee319bc7"),
                                topology.map(|topology| topology.non_manifold_edges),
                                inspecting,
                            ))
                            .child(check_row(
                                i18n::t!("c074951d4625d1c1"),
                                topology.map(|topology| topology.inconsistent_edges),
                                inspecting,
                            ))
                            .child(check_row(
                                i18n::t!("d44d74055685e9c8"),
                                topology.map(|_| self.mesh.degenerate_faces),
                                inspecting,
                            ))
                            .child(check_row(i18n::t!("16f8919589ea1cab"), None, inspecting))
                            .when_some(self.error.clone(), |section, error| {
                                section.child(
                                    div().px_3().py_0p5().child(
                                        Label::new(error)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Error),
                                    ),
                                )
                            });
                        section.when(topology.is_none() && self.error.is_none(), |section| {
                            section.child(
                                div().px_3().py_1().child(
                                    Button::new("model-inspect", i18n::t!("91d5290121a42adf"))
                                        .label_size(LabelSize::Small)
                                        .disabled(inspecting)
                                        .on_click(
                                            cx.listener(|reader, _, _, cx| reader.inspect(cx)),
                                        ),
                                ),
                            )
                        })
                    }))
                    .child(model_card(cx).child(section_header(
                        "model-file-header",
                        i18n::t!("50009ce1da4d15e1"),
                        file_open,
                        cx,
                        cx.listener(|reader, _, _, cx| {
                            reader.file_open = !reader.file_open;
                            cx.notify();
                        }),
                    ))
                    .when(file_open, |section| {
                        section
                            .child(property_row(
                                i18n::t!("2f343666aaa88c44"),
                                self.format.clone(),
                            ))
                            .child(property_row(
                                i18n::t!("1af851907331c0ed"),
                                format_bytes(self.file_size),
                            ))
                            .child(property_row(
                                i18n::t!("05ba10a2667aa584"),
                                i18n::t!(
                                    "beb6a740ef5a98e7",
                                    value = format_quantity(self.mesh.area)
                                ),
                            ))
                    })),
            )
    }

    fn render_section_card(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let enabled = self.section.enabled;
        let plane = self.section.plane;
        let capped = self.section.capped;
        let open = self.section_open;
        let capped_available = self.mesh.triangles.len() <= SECTION_CAP_TRIANGLES;
        let area = self
            .section_geometry
            .as_ref()
            .filter(|(cached_plane, _)| *cached_plane == plane)
            .map(|(_, geometry)| geometry.area);
        let axis_group = ToggleButtonGroup::single_row(
            "model-section-axis",
            SectionAxis::ALL.map(|axis| {
                let weak = cx.entity().downgrade();
                ToggleButtonSimple::new(axis_letter(axis), move |_, _, cx| {
                    weak.update(cx, |reader, cx| {
                        let mut plane = reader.section.plane;
                        plane.axis = axis;
                        reader.set_section_plane(plane, cx);
                    })
                    .log_err();
                })
            }),
        )
        .auto_width()
        .size(ToggleButtonGroupSize::Large)
        .style(ToggleButtonGroupStyle::Outlined)
        .label_size(LabelSize::Default)
        .selected_index(plane.axis.index());
        model_card(cx)
            .child(section_header(
                "model-section-header",
                i18n::t!("dfca5da56b8cdb62"),
                open,
                cx,
                cx.listener(|reader, _, _, cx| {
                    reader.section_open = !reader.section_open;
                    cx.notify();
                }),
            ))
            .when(open, |card| {
                if !enabled {
                    return card.child(
                        div().px_3().py_1p5().child(
                            Label::new(i18n::t!("7d45c4dd5d493928"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                    );
                }
                card.child(
                    h_flex()
                        .px_3()
                        .py_1p5()
                        .justify_between()
                        .gap_2()
                        .child(
                            Label::new(i18n::t!("ccbc1a1afe8543a8"))
                                .size(LabelSize::Default)
                                .color(Color::Muted),
                        )
                        .child(axis_group),
                )
                .child(self.render_section_position(cx))
                .child(
                    h_flex()
                        .px_3()
                        .py_1p5()
                        .gap_2()
                        .child(
                            Button::new("model-section-flip", i18n::t!("f99ebca9b508a837"))
                                .label_size(LabelSize::Small)
                                .toggle_state(plane.flipped)
                                .on_click(cx.listener(|reader, _, _, cx| {
                                    let mut plane = reader.section.plane;
                                    plane.flipped = !plane.flipped;
                                    reader.set_section_plane(plane, cx);
                                })),
                        )
                        .child(
                            Button::new("model-section-cap", i18n::t!("1257ac04471db97b"))
                                .label_size(LabelSize::Small)
                                .toggle_state(capped)
                                .on_click(cx.listener(|reader, _, _, cx| {
                                    let capped = !reader.section.capped;
                                    reader.set_section_capped(capped, cx);
                                })),
                        ),
                )
                .child(property_row(
                    i18n::t!("5854fbebb6baf28d"),
                    match area {
                        Some(area) => format_quantity(area),
                        None => i18n::t!("5b8aa0bf3c59daec").to_string(),
                    },
                ))
                .when(capped && !capped_available, |card| {
                    card.child(
                        div().px_3().py_0p5().child(
                            Label::new(i18n::t!("4ef7289c2b2aa148"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                    )
                })
            })
    }

    fn render_section_position(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity().downgrade();
        let offset = self.section.plane.offset;
        let accent = cx.theme().colors().text_accent;
        let track = cx.theme().colors().border;
        let handle_border = cx.theme().colors().background;
        v_flex()
            .px_3()
            .py_1p5()
            .gap_1()
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        Label::new(i18n::t!("c53edeb1ae539101"))
                            .size(LabelSize::Default)
                            .color(Color::Muted),
                    )
                    .child(Label::new(format!("{:.0}%", offset * 100.0)).size(LabelSize::Default)),
            )
            .child(
                div()
                    .id("model-section-track")
                    .debug_selector(|| "model-section-track".to_string())
                    .relative()
                    .h(px(20.0))
                    .w_full()
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|reader, event: &gpui::MouseDownEvent, _, cx| {
                            reader.dragging_section = true;
                            reader.drag_section_offset(event.position, cx);
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|reader, _, _, _| reader.dragging_section = false),
                    )
                    .on_mouse_move(cx.listener(|reader, event: &gpui::MouseMoveEvent, _, cx| {
                        if !event
                            .pressed_button
                            .is_some_and(|button| button == MouseButton::Left)
                        {
                            reader.dragging_section = false;
                            return;
                        }
                        if reader.dragging_section {
                            reader.drag_section_offset(event.position, cx);
                        }
                    }))
                    .child(
                        div()
                            .absolute()
                            .top(px(8.0))
                            .left_0()
                            .right_0()
                            .h(px(4.0))
                            .rounded_full()
                            .bg(track),
                    )
                    .child(
                        div()
                            .absolute()
                            .top(px(8.0))
                            .left_0()
                            .h(px(4.0))
                            .rounded_full()
                            .w(relative(offset as f32))
                            .bg(accent),
                    )
                    .child(
                        div()
                            .absolute()
                            .top(px(4.0))
                            .left(relative(offset as f32))
                            .ml(px(-6.0))
                            .size(px(12.0))
                            .rounded_full()
                            .border_2()
                            .border_color(handle_border)
                            .bg(accent),
                    )
                    .child(
                        canvas(
                            move |bounds, _, cx| {
                                entity
                                    .update(cx, |reader, _| reader.section_track = Some(bounds))
                                    .log_err();
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    ),
            )
    }

    fn render_viewport(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity().downgrade();
        let gizmo = self.camera.axes.then(|| gizmo_axes(&self.camera));
        div()
            .id("model-viewport")
            .debug_selector(|| "model-viewport".to_string())
            .relative()
            .flex_1()
            .h_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|reader, event: &gpui::MouseDownEvent, _, cx| {
                    reader.drag = Some((event.position, event.modifiers.shift));
                    cx.notify();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|reader, _, _, _| reader.drag = None),
            )
            .on_mouse_move(cx.listener(|reader, event: &gpui::MouseMoveEvent, _, cx| {
                if !event
                    .pressed_button
                    .is_some_and(|button| button == MouseButton::Left)
                {
                    reader.drag = None;
                    return;
                }
                if let Some((previous, pan)) = reader.drag {
                    let delta = event.position - previous;
                    if pan {
                        reader.camera.pan[0] += f64::from(delta.x) / 400.0;
                        reader.camera.pan[1] += f64::from(delta.y) / 400.0;
                    } else {
                        reader.camera.yaw += f64::from(delta.x) * 0.01;
                        reader.camera.pitch += f64::from(delta.y) * 0.01;
                    }
                    reader.drag = Some((event.position, pan));
                    reader.dirty = true;
                    cx.notify();
                }
            }))
            .on_scroll_wheel(
                cx.listener(|reader, event: &gpui::ScrollWheelEvent, _, cx| {
                    reader.camera.zoom = (reader.camera.zoom
                        * (f64::from(event.delta.pixel_delta(gpui::px(20.0)).y) * 0.002).exp())
                    .clamp(0.05, 20.0);
                    reader.dirty = true;
                    cx.notify();
                }),
            )
            .when_some(self.image.clone(), |element, image| {
                element.child(
                    img(image)
                        .absolute()
                        .size_full()
                        .object_fit(gpui::ObjectFit::Contain),
                )
            })
            .child(
                canvas(
                    move |bounds, _, cx| {
                        entity
                            .update(cx, |reader, cx| {
                                let width = f32::from(bounds.size.width).clamp(1.0, 1280.0) as u32;
                                let height = f32::from(bounds.size.height).clamp(1.0, 960.0) as u32;
                                if reader.dimensions != [width, height] {
                                    reader.dimensions = [width, height];
                                    reader.dirty = true;
                                    cx.notify();
                                }
                            })
                            .log_err();
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .when_some(gizmo, |element, axes| element.child(render_gizmo(axes)))
            .child(self.render_view_navigation(cx))
    }

    fn render_view_navigation(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let hovered = self.view_cube_hover;
        // One surface tone lightened in three steps, so the faces read as a
        // single isometric solid in both dark and light themes.
        let base = colors.element_background;
        let top_face = base.blend(colors.text.opacity(0.12));
        let front_face = base.blend(colors.text.opacity(0.06));
        let right_face = base.blend(colors.text.opacity(0.03));
        let edge = base.blend(colors.text.opacity(0.45));
        let label = colors.text;
        v_flex()
            .absolute()
            .top_4()
            .right_4()
            .gap_1()
            .items_center()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .debug_selector(|| "model-view-cube".to_string())
                    .relative()
                    .w(px(VIEW_CUBE_WIDTH))
                    .h(px(VIEW_CUBE_HEIGHT))
                    .child(
                        canvas(
                            move |bounds, _, _| bounds.origin,
                            move |_, origin, window, _| {
                                for (preset, vertices) in view_cube_faces() {
                                    let face = match preset {
                                        ViewPreset::Top => top_face,
                                        ViewPreset::Front => front_face,
                                        _ => right_face,
                                    };
                                    let face = if hovered == Some(preset) {
                                        face.blend(label.opacity(0.08))
                                    } else {
                                        face
                                    };
                                    paint_view_cube_face(window, origin, &vertices, face, edge);
                                }
                            },
                        )
                        .absolute()
                        .size_full(),
                    )
                    .child(view_cube_label(ViewPreset::Top, label))
                    .child(view_cube_label(ViewPreset::Front, label))
                    .child(view_cube_label(ViewPreset::Right, label))
                    .children(
                        view_cube_hit_regions()
                            .into_iter()
                            .map(|(preset, [x, y, width, height])| {
                                let id = view_cube_region_id(preset);
                                div()
                                    .id(id)
                                    .debug_selector(move || id.to_string())
                                    .absolute()
                                    .left(px(x))
                                    .top(px(y))
                                    .w(px(width))
                                    .h(px(height))
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |reader, _, _, cx| {
                                        reader.set_preset(preset, cx);
                                    }))
                                    .on_hover(cx.listener(move |reader, hovered, _, cx| {
                                        if *hovered {
                                            reader.view_cube_hover = Some(preset);
                                        } else if reader.view_cube_hover == Some(preset) {
                                            reader.view_cube_hover = None;
                                        }
                                        cx.notify();
                                    }))
                            }),
                    ),
            )
            .child(
                Button::new("model-navigation-isometric", ViewPreset::Isometric.label())
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|reader, _, _, cx| {
                        reader.set_preset(ViewPreset::Isometric, cx);
                    })),
            )
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let projection = if self.camera.perspective {
            i18n::t!("e3ad1bcc2cbd8537")
        } else {
            i18n::t!("0f04d53421188d24")
        };
        h_flex()
            .flex_none()
            .px_3()
            .py_1p5()
            .justify_between()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(
                Label::new(i18n::t!("30bd16f9d01e8dc6"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Label::new(format!(
                    "{} · {} · {}: {}",
                    projection,
                    i18n::t!("72bb90897ab1eadc"),
                    i18n::t!("9fb6669a77ea48bb"),
                    i18n::t!("5b8aa0bf3c59daec")
                ))
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            )
    }
}

/// Visible view-cube faces in cube-local coordinates: top, front and right.
/// The three faces share the inner vertex at the cube's center.
fn view_cube_faces() -> [(ViewPreset, [Point<Pixels>; 4]); 3] {
    let zero = px(0.0);
    let half = px(VIEW_CUBE_WIDTH / 2.0);
    let width = px(VIEW_CUBE_WIDTH);
    let middle = px(VIEW_CUBE_TOP_HEIGHT / 2.0);
    let top = px(VIEW_CUBE_TOP_HEIGHT);
    let bottom = px(VIEW_CUBE_HEIGHT);
    let shoulder = px((VIEW_CUBE_TOP_HEIGHT + VIEW_CUBE_HEIGHT) / 2.0);
    [
        (
            ViewPreset::Top,
            [
                point(half, zero),
                point(width, middle),
                point(half, top),
                point(zero, middle),
            ],
        ),
        (
            ViewPreset::Front,
            [
                point(zero, middle),
                point(half, top),
                point(half, bottom),
                point(zero, shoulder),
            ],
        ),
        (
            ViewPreset::Right,
            [
                point(half, top),
                point(width, middle),
                point(width, shoulder),
                point(half, bottom),
            ],
        ),
    ]
}

/// Clickable view-cube regions in cube-local coordinates as
/// `(x, y, width, height)`. The rectangles tile the cube's bounding square, so
/// each face wins the hit test for exactly its own area.
fn view_cube_hit_regions() -> [(ViewPreset, [f32; 4]); 3] {
    let half = VIEW_CUBE_WIDTH / 2.0;
    let face_height = VIEW_CUBE_HEIGHT - VIEW_CUBE_TOP_HEIGHT;
    [
        (
            ViewPreset::Top,
            [0.0, 0.0, VIEW_CUBE_WIDTH, VIEW_CUBE_TOP_HEIGHT],
        ),
        (
            ViewPreset::Front,
            [0.0, VIEW_CUBE_TOP_HEIGHT, half, face_height],
        ),
        (
            ViewPreset::Right,
            [half, VIEW_CUBE_TOP_HEIGHT, half, face_height],
        ),
    ]
}

fn view_cube_region_id(preset: ViewPreset) -> &'static str {
    match preset {
        ViewPreset::Top => "model-view-cube-top",
        ViewPreset::Front => "model-view-cube-front",
        ViewPreset::Right => "model-view-cube-right",
        _ => "model-view-cube-face",
    }
}

/// A face caption centered on its region instead of its parallelogram, which
/// keeps two-character labels inside the visible face.
fn view_cube_label(preset: ViewPreset, color: Hsla) -> impl IntoElement {
    let half = VIEW_CUBE_WIDTH / 2.0;
    let side_center = (VIEW_CUBE_TOP_HEIGHT + VIEW_CUBE_HEIGHT) / 2.0;
    let (left, width, center) = match preset {
        ViewPreset::Top => (0.0, VIEW_CUBE_WIDTH, VIEW_CUBE_TOP_HEIGHT / 2.0),
        ViewPreset::Front => (0.0, half, side_center),
        ViewPreset::Right => (half, half, side_center),
        _ => (0.0, VIEW_CUBE_WIDTH, VIEW_CUBE_TOP_HEIGHT / 2.0),
    };
    h_flex()
        .absolute()
        .left(px(left))
        .top(px(center - 8.0))
        .w(px(width))
        .h(px(16.0))
        .justify_center()
        .child(
            Label::new(preset.label())
                .size(LabelSize::XSmall)
                .color(Color::Custom(color)),
        )
}

/// Fills one view-cube face and strokes its outline. Face edges are stroked
/// more than once where two faces meet, so the color stays opaque.
fn paint_view_cube_face(
    window: &mut Window,
    origin: Point<Pixels>,
    vertices: &[Point<Pixels>; 4],
    fill: Hsla,
    edge: Hsla,
) {
    for (builder, color) in [
        (PathBuilder::fill(), fill),
        (PathBuilder::stroke(px(1.0)), edge),
    ] {
        let mut builder = builder;
        builder.move_to(origin + vertices[0]);
        for vertex in &vertices[1..] {
            builder.line_to(origin + *vertex);
        }
        builder.close();
        if let Ok(path) = builder.build() {
            window.paint_path(path, color);
        }
    }
}

/// Projected gizmo axis: screen-space direction, view-space depth, color and label.
type GizmoAxis = (f32, f32, f32, u32, &'static str);

fn axis_letter(axis: SectionAxis) -> &'static str {
    match axis {
        SectionAxis::X => "X",
        SectionAxis::Y => "Y",
        SectionAxis::Z => "Z",
    }
}

fn gizmo_axes(camera: &Camera) -> [GizmoAxis; 3] {
    let (yaw_sin, yaw_cos) = camera.yaw.sin_cos();
    let (pitch_sin, pitch_cos) = camera.pitch.sin_cos();
    [
        ([1.0, 0.0, 0.0], 0xe5534b, "X"),
        ([0.0, 1.0, 0.0], 0x57ab5a, "Y"),
        ([0.0, 0.0, 1.0], 0x539bf5, "Z"),
    ]
    .map(|(axis, color, label)| {
        let [x, y, z] = axis;
        let x1 = x * yaw_cos + z * yaw_sin;
        let z1 = -x * yaw_sin + z * yaw_cos;
        let y1 = y * pitch_cos - z1 * pitch_sin;
        let z2 = y * pitch_sin + z1 * pitch_cos;
        (x1 as f32, -(y1 as f32), z2 as f32, color, label)
    })
}

fn render_gizmo(axes: [GizmoAxis; 3]) -> impl IntoElement {
    div()
        .absolute()
        .left_2()
        .bottom_2()
        .size(px(GIZMO_SIZE))
        .child(
            canvas(
                move |bounds, _, _| {
                    let center = bounds.origin + point(px(GIZMO_SIZE / 2.0), px(GIZMO_SIZE / 2.0));
                    let mut strokes: Vec<(Point<Pixels>, Point<Pixels>, f32, u32)> = axes
                        .into_iter()
                        .map(|(dx, dy, depth, color, _)| {
                            (
                                center,
                                center + point(px(dx * GIZMO_ARM), px(dy * GIZMO_ARM)),
                                depth,
                                color,
                            )
                        })
                        .collect();
                    strokes.sort_by(|first, second| {
                        first
                            .2
                            .partial_cmp(&second.2)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                    strokes
                },
                move |_, strokes, window, _| {
                    for (start, end, _, color) in strokes {
                        let mut builder = PathBuilder::stroke(px(1.5));
                        builder.move_to(start);
                        builder.line_to(end);
                        if let Ok(path) = builder.build() {
                            window.paint_path(path, rgb(color));
                        }
                    }
                },
            )
            .size_full(),
        )
        .children(axes.into_iter().map(|(dx, dy, _, color, label)| {
            let half = GIZMO_SIZE / 2.0;
            div()
                .absolute()
                .left(px(half + dx * GIZMO_LABEL_RADIUS - 4.0))
                .top(px(half + dy * GIZMO_LABEL_RADIUS - 5.0))
                .child(
                    Label::new(label)
                        .size(LabelSize::XSmall)
                        .color(Color::Custom(rgb(color).into())),
                )
        }))
}

fn model_card(cx: &App) -> gpui::Div {
    let colors = cx.theme().colors();
    v_flex()
        .flex_none()
        .pb_2()
        .rounded_lg()
        .border_1()
        .border_color(colors.border_variant)
        .bg(colors.elevated_surface_background)
        .overflow_hidden()
}

fn section_header(
    id: &'static str,
    title: &'static str,
    open: bool,
    cx: &App,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let background = cx.theme().colors().element_hover;
    let hover_background = cx.theme().colors().element_active;
    h_flex()
        .id(id)
        .debug_selector(move || id.to_string())
        .px_3()
        .py_2()
        .gap_2()
        .items_center()
        .cursor_pointer()
        .bg(background)
        .hover(move |style| style.bg(hover_background))
        .on_click(on_click)
        .child(
            Icon::new(if open {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            })
            .size(IconSize::Small)
            .color(Color::Muted),
        )
        .child(Label::new(title).size(LabelSize::Default))
}

fn property_row(label: &'static str, value: impl Into<SharedString>) -> impl IntoElement {
    h_flex()
        .px_3()
        .py_1p5()
        .justify_between()
        .gap_2()
        .child(Label::new(label).size(LabelSize::Default).color(Color::Muted))
        .child(Label::new(value).size(LabelSize::Default))
}

fn check_row(label: &'static str, count: Option<usize>, inspecting: bool) -> impl IntoElement {
    let (value, color) = match (count, inspecting) {
        (Some(0), _) => (grouped(0), Color::Success),
        (Some(count), _) => (grouped(count), Color::Error),
        (None, true) => ("…".to_string(), Color::Muted),
        (None, false) => (i18n::t!("5b8aa0bf3c59daec").to_string(), Color::Muted),
    };
    h_flex()
        .px_3()
        .py_1p5()
        .justify_between()
        .child(Label::new(label).size(LabelSize::Default).color(Color::Muted))
        .child(
            h_flex()
                .gap_1p5()
                .items_center()
                .child(Label::new(value).size(LabelSize::Default))
                .child(Indicator::dot().color(color)),
        )
}

fn grouped(value: usize) -> String {
    let digits = value.to_string();
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            output.push(',');
        }
        output.push(digit);
    }
    output
}

fn format_quantity(value: f64) -> String {
    let negative = value < 0.0;
    let formatted = format!("{:.2}", value.abs());
    let Some((integer, fraction)) = formatted.split_once('.') else {
        return formatted;
    };
    let mut output = String::new();
    if negative {
        output.push('-');
    }
    for (index, digit) in integer.chars().enumerate() {
        if index > 0 && (integer.len() - index).is_multiple_of(3) {
            output.push(',');
        }
        output.push(digit);
    }
    output.push('.');
    output.push_str(fraction);
    output
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

impl Focusable for ModelReader {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ModelReader {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let style = self.raster_style(cx);
        if self.rendered_style != Some(style) {
            self.rendered_style = Some(style);
            self.dirty = true;
        }
        self.request_render(window, cx);
        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .bg(cx.theme().colors().editor_background)
            .child(self.render_toolbar(window, cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_flex()
                            .flex_1()
                            .h_full()
                            .min_w_0()
                            .min_h_0()
                            .child(self.render_viewport(cx))
                            .child(self.render_status_bar(cx)),
                    )
                    .when(self.panel_open, |element| {
                        element.child(self.render_panel(cx))
                    }),
            )
    }
}

fn rasterize(
    mesh: &ModelMesh,
    camera: Camera,
    style: RasterStyle,
    dimensions: [u32; 2],
    section: Option<&SectionRender>,
    cancel: &AtomicBool,
) -> Option<Arc<RenderImage>> {
    let [width, height] = dimensions;
    let length = width as usize * height as usize;
    let mut pixels = vec![0u8; length * 4];
    let mut depth = vec![f64::INFINITY; length];
    let (center, radius) = mesh.frame();
    let (yaw_sin, yaw_cos) = camera.yaw.sin_cos();
    let (pitch_sin, pitch_cos) = camera.pitch.sin_cos();
    // Pure yaw/pitch rotation into view space.
    let rotate = move |vertex: Vertex| {
        let x = vertex[0] * yaw_cos + vertex[2] * yaw_sin;
        let z = -vertex[0] * yaw_sin + vertex[2] * yaw_cos;
        [
            x,
            vertex[1] * pitch_cos - z * pitch_sin,
            vertex[1] * pitch_sin + z * pitch_cos,
        ]
    };
    let scale = f64::from(width.min(height)) * 0.8 * camera.zoom;
    let project = |vertex: Vertex| {
        let perspective = if camera.perspective {
            2.5 / (2.5 - vertex[2])
        } else {
            1.0
        };
        [
            f64::from(width) * 0.5 + (vertex[0] * perspective + camera.pan[0]) * scale,
            f64::from(height) * 0.5 - (vertex[1] * perspective - camera.pan[1]) * scale,
            -vertex[2],
        ]
    };
    if camera.grid {
        draw_grid(
            mesh,
            &rotate,
            &project,
            &mut pixels,
            &mut depth,
            dimensions,
            style.grid,
            cancel,
        );
    }
    let section_args = section.map(|section| {
        (
            section.plane.axis.index(),
            section.plane.threshold(mesh, center, radius),
            section.plane.sign(),
        )
    });
    let plane_view = section.map(|section| {
        let axis = section.plane.axis.index();
        let threshold = section.plane.threshold(mesh, center, radius);
        let point = rotate(std::array::from_fn(|component| {
            if component == axis { threshold } else { 0.0 }
        }));
        let normal = rotate(std::array::from_fn(|component| {
            if component == axis { 1.0 } else { 0.0 }
        }));
        (normal, dot(normal, point))
    });
    for (index, triangle) in mesh.triangles.iter().enumerate() {
        if index % 256 == 0 && cancel.load(Ordering::Relaxed) {
            return None;
        }
        // Clipping happens on normalized model coordinates, before rotation, so the
        // cutting plane stays a plain half-space test regardless of the camera.
        let normalized = triangle.map(|vertex| model_section::normalize(vertex, center, radius));
        let mut polygon = [[0.0; 3]; 4];
        let (length, cut) = match section_args {
            Some((axis, threshold, sign)) => {
                let clipped = model_section::clip_triangle(normalized, axis, threshold, sign);
                polygon = clipped.polygon;
                (clipped.length, clipped.cut)
            }
            None => {
                polygon[..3].copy_from_slice(&normalized);
                (3, None)
            }
        };
        if let Some(cut) = cut {
            draw_line(
                &mut pixels,
                &mut depth,
                dimensions,
                project(rotate(cut[0])),
                project(rotate(cut[1])),
                [style.cut[0], style.cut[1], style.cut[2], 255],
                CUT_DEPTH_BIAS,
            );
        }
        // A clipped triangle becomes a convex polygon, which fans out into one or
        // two triangles around its first vertex.
        for corner in 1..length.saturating_sub(1) {
            let rotated = [
                rotate(polygon[0]),
                rotate(polygon[corner]),
                rotate(polygon[corner + 1]),
            ];
            if !rasterize_triangle(
                rotated,
                style,
                camera.shading,
                &project,
                &mut pixels,
                &mut depth,
                dimensions,
                cancel,
            ) {
                return None;
            }
        }
    }
    if let (Some(section), Some(plane_view)) = (section, plane_view)
        && !fill_section_cap(
            section,
            &rotate,
            &project,
            plane_view,
            camera,
            &mut pixels,
            &mut depth,
            dimensions,
            style.section,
            cancel,
        )
    {
        return None;
    }
    let image = image::RgbaImage::from_raw(width, height, pixels)?;
    log::info!("[open-debug] ModelReader rasterized {width}x{height}");
    Some(Arc::new(RenderImage::new(smallvec::smallvec![
        image::Frame::new(image)
    ])))
}

/// Rasterizes one view-space triangle into the shared color and depth buffers.
///
/// Returns `false` when the render was cancelled.
#[allow(clippy::too_many_arguments)]
fn rasterize_triangle(
    rotated: [Vertex; 3],
    style: RasterStyle,
    shading: Shading,
    project: &impl Fn(Vertex) -> Vertex,
    pixels: &mut [u8],
    depth: &mut [f64],
    dimensions: [u32; 2],
    cancel: &AtomicBool,
) -> bool {
    let [width, height] = dimensions;
    let normal = cross(sub(rotated[1], rotated[0]), sub(rotated[2], rotated[0]));
    let normal_length = dot(normal, normal).sqrt().max(1e-20);
    let key = dot(normal, [0.35, 0.55, 0.76]).abs() / normal_length;
    let fill = dot(normal, [-0.5, -0.2, 0.6]).abs() / normal_length;
    let intensity = (0.30 + 0.62 * key + 0.22 * fill).min(1.0);
    let projected = rotated.map(project);
    let area = edge(projected[0], projected[1], projected[2]);
    if area.abs() < 1e-12 {
        return true;
    }
    let minimum_x = projected
        .iter()
        .map(|vertex| vertex[0])
        .fold(f64::INFINITY, f64::min)
        .floor()
        .max(0.0) as u32;
    let maximum_x = projected
        .iter()
        .map(|vertex| vertex[0])
        .fold(f64::NEG_INFINITY, f64::max)
        .ceil()
        .min(f64::from(width)) as u32;
    let minimum_y = projected
        .iter()
        .map(|vertex| vertex[1])
        .fold(f64::INFINITY, f64::min)
        .floor()
        .max(0.0) as u32;
    let maximum_y = projected
        .iter()
        .map(|vertex| vertex[1])
        .fold(f64::NEG_INFINITY, f64::max)
        .ceil()
        .min(f64::from(height)) as u32;
    for y in minimum_y..maximum_y {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        for x in minimum_x..maximum_x {
            let point = [f64::from(x) + 0.5, f64::from(y) + 0.5, 0.0];
            let weights = [
                edge(projected[1], projected[2], point) / area,
                edge(projected[2], projected[0], point) / area,
                edge(projected[0], projected[1], point) / area,
            ];
            if weights.iter().any(|weight| *weight < 0.0) {
                continue;
            }
            let distance = weights[0] * projected[0][2]
                + weights[1] * projected[1][2]
                + weights[2] * projected[2][2];
            let offset = y as usize * width as usize + x as usize;
            if distance >= depth[offset] {
                continue;
            }
            depth[offset] = distance;
            let line = weights.iter().any(|weight| *weight < 0.025);
            let color = match shading {
                Shading::Solid => shade(style.model, intensity),
                Shading::SolidEdges if line => [style.edge[0], style.edge[1], style.edge[2], 255],
                Shading::SolidEdges => shade(style.model, intensity),
                Shading::Wireframe if line => [style.wire[0], style.wire[1], style.wire[2], 255],
                Shading::Wireframe => continue,
            };
            pixels[offset * 4..offset * 4 + 4].copy_from_slice(&color);
        }
    }
    true
}

/// Fills the cross-section cap with a flat color.
///
/// Loops are filled with the even-odd rule in screen space, and every covered
/// pixel takes its depth from the plane equation, so the cap occludes exactly the
/// material the cut removed. Returns `false` when the render was cancelled.
#[allow(clippy::too_many_arguments)]
fn fill_section_cap(
    section: &SectionRender,
    rotate: &impl Fn(Vertex) -> Vertex,
    project: &impl Fn(Vertex) -> Vertex,
    plane: (Vertex, f64),
    camera: Camera,
    pixels: &mut [u8],
    depth: &mut [f64],
    dimensions: [u32; 2],
    color: [u8; 3],
    cancel: &AtomicBool,
) -> bool {
    let Some(cap) = section.cap.as_ref() else {
        return true;
    };
    let [width, height] = dimensions;
    let mut edges = Vec::<(f64, f64, f64, f64)>::new();
    for loop_points in &cap.loops {
        for index in 0..loop_points.len() {
            let start = project(rotate(loop_points[index]));
            let end = project(rotate(loop_points[(index + 1) % loop_points.len()]));
            if start[1] == end[1] {
                continue;
            }
            edges.push((start[0], start[1], end[0], end[1]));
        }
    }
    if edges.is_empty() {
        return true;
    }
    // Bucketing each edge under the first scanline it can span keeps the per-row
    // crossing test proportional to the edges that actually cross that row.
    let mut buckets = vec![Vec::<usize>::new(); height as usize + 1];
    for (index, edge) in edges.iter().enumerate() {
        let first = (edge.1.min(edge.3) - 0.5).ceil().max(0.0) as usize;
        if first < buckets.len() {
            buckets[first].push(index);
        }
    }
    let mut active = Vec::<usize>::new();
    for row in 0..height as usize {
        active.extend(buckets[row].iter().copied());
        let center_y = row as f64 + 0.5;
        let mut crossings = Vec::new();
        for index in &active {
            let (first_x, first_y, second_x, second_y) = edges[*index];
            let low = first_y.min(second_y);
            if center_y < low || center_y >= first_y.max(second_y) {
                continue;
            }
            crossings
                .push(first_x + (second_x - first_x) * (center_y - first_y) / (second_y - first_y));
        }
        crossings.sort_by(|first, second| {
            first
                .partial_cmp(second)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for pair in crossings.chunks_exact(2) {
            let start = (pair[0] - 0.5).ceil().max(0.0) as u32;
            let end = (pair[1] - 0.5).ceil().clamp(0.0, f64::from(width)) as u32;
            for x in start..end {
                if cancel.load(Ordering::Relaxed) {
                    return false;
                }
                let Some(distance) = plane_depth(camera, plane, x, row as u32, dimensions) else {
                    continue;
                };
                let offset = row * width as usize + x as usize;
                let distance = distance - CAP_DEPTH_BIAS;
                if distance < depth[offset] {
                    depth[offset] = distance;
                    pixels[offset * 4..offset * 4 + 4]
                        .copy_from_slice(&[color[0], color[1], color[2], 255]);
                }
            }
        }
        active.retain(|index| center_y < edges[*index].1.max(edges[*index].3));
    }
    true
}

/// Depth at which the view ray through a pixel center meets a plane, the plane
/// being given by its view-space normal and offset.
fn plane_depth(
    camera: Camera,
    plane: (Vertex, f64),
    x: u32,
    y: u32,
    dimensions: [u32; 2],
) -> Option<f64> {
    let [width, height] = dimensions;
    let scale = f64::from(width.min(height)) * 0.8 * camera.zoom;
    let u = (f64::from(x) + 0.5 - f64::from(width) * 0.5) / scale - camera.pan[0];
    let v = -(f64::from(y) + 0.5 - f64::from(height) * 0.5) / scale + camera.pan[1];
    let (normal, offset) = plane;
    if camera.perspective {
        // Substituting `perspective = 2.5 / (2.5 - z)` into `normal . point = offset`
        // and projecting the plane point gives the depth directly from the pixel.
        let denominator = offset - 2.5 * normal[2];
        if denominator.abs() <= 1e-12 {
            return None;
        }
        let perspective = (normal[0] * u + normal[1] * v - 2.5 * normal[2]) / denominator;
        if perspective <= 0.0 {
            return None;
        }
        let z = 2.5 - 2.5 / perspective;
        z.is_finite().then_some(-z)
    } else {
        if normal[2].abs() <= 1e-12 {
            return None;
        }
        let z = (offset - normal[0] * u - normal[1] * v) / normal[2];
        z.is_finite().then_some(-z)
    }
}

/// Strokes a screen-space line with linear depth, biased towards the camera.
fn draw_line(
    pixels: &mut [u8],
    depth: &mut [f64],
    dimensions: [u32; 2],
    start: Vertex,
    end: Vertex,
    color: [u8; 4],
    bias: f64,
) {
    let [width, height] = dimensions;
    let steps = ((end[0] - start[0]).hypot(end[1] - start[1]) * 1.5)
        .ceil()
        .max(1.0) as u32;
    for step in 0..=steps {
        let t = f64::from(step) / f64::from(steps);
        let x = (start[0] + (end[0] - start[0]) * t).round() as i64;
        let y = (start[1] + (end[1] - start[1]) * t).round() as i64;
        if x < 0 || y < 0 || x >= i64::from(width) || y >= i64::from(height) {
            continue;
        }
        let distance = start[2] + (end[2] - start[2]) * t - bias;
        let offset = y as usize * width as usize + x as usize;
        if distance < depth[offset] {
            depth[offset] = distance;
            pixels[offset * 4..offset * 4 + 4].copy_from_slice(&color);
        }
    }
}

fn shade(base: [u8; 3], intensity: f64) -> [u8; 4] {
    [
        (base[0] as f64 * intensity) as u8,
        (base[1] as f64 * intensity) as u8,
        (base[2] as f64 * intensity) as u8,
        255,
    ]
}

/// Rasterizes a grid floor at the model's lowest point so the viewer reads as
/// a grounded scene instead of a floating object.
fn draw_grid(
    mesh: &ModelMesh,
    view: &impl Fn(Vertex) -> Vertex,
    project: &impl Fn(Vertex) -> Vertex,
    pixels: &mut [u8],
    depth: &mut [f64],
    dimensions: [u32; 2],
    color: [u8; 4],
    cancel: &AtomicBool,
) {
    let center: Vertex =
        std::array::from_fn(|axis| mesh.minimum[axis] * 0.5 + mesh.maximum[axis] * 0.5);
    let radius = sub(mesh.maximum, mesh.minimum)
        .into_iter()
        .fold(0.0f64, f64::max)
        .max(1e-20);
    let floor = (mesh.minimum[1] - center[1]) / radius;
    const EXTENT: f64 = 0.78;
    const DIVISIONS: usize = 12;
    for division in 0..=DIVISIONS {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let offset = -EXTENT + 2.0 * EXTENT * division as f64 / DIVISIONS as f64;
        for (start, end) in [
            ([-EXTENT, floor, offset], [EXTENT, floor, offset]),
            ([offset, floor, -EXTENT], [offset, floor, EXTENT]),
        ] {
            draw_line(
                pixels,
                depth,
                dimensions,
                project(view(start)),
                project(view(end)),
                color,
                0.0,
            );
        }
    }
}

fn edge(first: Vertex, second: Vertex, point: Vertex) -> f64 {
    (point[0] - first[0]) * (second[1] - first[1]) - (point[1] - first[1]) * (second[0] - first[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_mesh() -> ModelMesh {
        ModelMesh::parse("obj", b"v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3").expect("mesh")
    }

    fn box_mesh() -> ModelMesh {
        let mut obj = String::new();
        for corner in [
            [-1.0, -1.0, -1.0],
            [1.0, -1.0, -1.0],
            [1.0, 1.0, -1.0],
            [-1.0, 1.0, -1.0],
            [-1.0, -1.0, 1.0],
            [1.0, -1.0, 1.0],
            [1.0, 1.0, 1.0],
            [-1.0, 1.0, 1.0],
        ] {
            obj.push_str(&format!("v {} {} {}\n", corner[0], corner[1], corner[2]));
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

    fn test_style() -> RasterStyle {
        RasterStyle {
            model: [120, 160, 210],
            edge: [48, 64, 84],
            wire: [200, 200, 200],
            grid: [90, 90, 90, 255],
            section: [200, 120, 60],
            cut: [250, 240, 130],
        }
    }

    fn section_render(mesh: &ModelMesh, offset: f64, capped: bool) -> SectionRender {
        let plane = SectionPlane {
            axis: SectionAxis::Z,
            offset,
            flipped: true,
        };
        let (center, radius) = mesh.frame();
        let cap = capped.then(|| {
            Arc::new(
                model_section::build(mesh, plane, center, radius, &AtomicBool::new(false))
                    .expect("section"),
            )
        });
        SectionRender { plane, cap }
    }

    fn opaque_pixels(image: &Arc<RenderImage>) -> usize {
        image
            .as_bytes(0)
            .expect("pixels")
            .chunks_exact(4)
            .filter(|pixel| pixel[3] != 0)
            .count()
    }

    fn count_color(image: &Arc<RenderImage>, color: [u8; 3]) -> usize {
        image
            .as_bytes(0)
            .expect("pixels")
            .chunks_exact(4)
            .filter(|pixel| pixel[..3] == color)
            .count()
    }

    #[test]
    fn render_is_bounded_and_cancellable() {
        let mesh = test_mesh();
        let image = rasterize(
            &mesh,
            Camera::default(),
            test_style(),
            [64, 64],
            None,
            &AtomicBool::new(false),
        )
        .expect("image");
        assert_eq!(image.as_bytes(0).expect("pixels").len(), 64 * 64 * 4);
        assert!(
            rasterize(
                &mesh,
                Camera::default(),
                test_style(),
                [64, 64],
                None,
                &AtomicBool::new(true)
            )
            .is_none()
        );
    }

    #[test]
    fn grid_floor_adds_pixels() {
        let mesh = test_mesh();
        let mut camera = Camera::default();
        camera.grid = true;
        let with_grid = rasterize(
            &mesh,
            camera,
            test_style(),
            [128, 128],
            None,
            &AtomicBool::new(false),
        )
        .expect("image");
        camera.grid = false;
        let without_grid = rasterize(
            &mesh,
            camera,
            test_style(),
            [128, 128],
            None,
            &AtomicBool::new(false),
        )
        .expect("image");
        assert!(opaque_pixels(&with_grid) > opaque_pixels(&without_grid));
    }

    #[test]
    fn shading_modes_differ() {
        let mesh = test_mesh();
        let mut camera = Camera::default();
        camera.grid = false;
        camera.shading = Shading::Solid;
        let solid = rasterize(
            &mesh,
            camera,
            test_style(),
            [128, 128],
            None,
            &AtomicBool::new(false),
        )
        .expect("image");
        camera.shading = Shading::Wireframe;
        let wireframe = rasterize(
            &mesh,
            camera,
            test_style(),
            [128, 128],
            None,
            &AtomicBool::new(false),
        )
        .expect("image");
        camera.shading = Shading::SolidEdges;
        let edges = rasterize(
            &mesh,
            camera,
            test_style(),
            [128, 128],
            None,
            &AtomicBool::new(false),
        )
        .expect("image");
        // Wireframe only strokes triangle outlines; solid modes fill interiors.
        assert!(opaque_pixels(&wireframe) < opaque_pixels(&solid));
        // Solid+edges keeps the fill and adds darker edge pixels.
        assert_eq!(opaque_pixels(&edges), opaque_pixels(&solid));
        assert!(
            edges
                .as_bytes(0)
                .expect("pixels")
                .chunks_exact(4)
                .any(|pixel| pixel[..3] == test_style().edge)
        );
    }

    #[test]
    fn section_clips_outlines_and_caps() {
        let mesh = box_mesh();
        let mut camera = Camera::default();
        camera.grid = false;
        let whole = rasterize(
            &mesh,
            camera,
            test_style(),
            [128, 128],
            None,
            &AtomicBool::new(false),
        )
        .expect("image");
        let open = section_render(&mesh, 0.5, false);
        let cut = rasterize(
            &mesh,
            camera,
            test_style(),
            [128, 128],
            Some(&open),
            &AtomicBool::new(false),
        )
        .expect("image");
        let capped = section_render(&mesh, 0.5, true);
        let filled = rasterize(
            &mesh,
            camera,
            test_style(),
            [128, 128],
            Some(&capped),
            &AtomicBool::new(false),
        )
        .expect("image");
        // Removing the half facing the camera must drop part of the silhouette.
        assert!(opaque_pixels(&cut) < opaque_pixels(&whole));
        // The cut curve is stroked either way; only capping fills the cut face.
        assert!(count_color(&cut, test_style().cut) > 0);
        assert_eq!(count_color(&cut, test_style().section), 0);
        assert!(count_color(&filled, test_style().cut) > 0);
        assert!(count_color(&filled, test_style().section) > 0);
    }

    #[test]
    fn orthogonal_projection_caps_the_section() {
        let mesh = box_mesh();
        let mut camera = Camera::default();
        camera.grid = false;
        camera.perspective = false;
        let capped = section_render(&mesh, 0.5, true);
        let filled = rasterize(
            &mesh,
            camera,
            test_style(),
            [128, 128],
            Some(&capped),
            &AtomicBool::new(false),
        )
        .expect("image");
        assert!(count_color(&filled, test_style().section) > 0);
    }

    #[test]
    fn section_outside_the_model_leaves_it_whole() {
        let mesh = box_mesh();
        let mut camera = Camera::default();
        camera.grid = false;
        let whole = rasterize(
            &mesh,
            camera,
            test_style(),
            [128, 128],
            None,
            &AtomicBool::new(false),
        )
        .expect("image");
        let outside = section_render(&mesh, 2.0, true);
        let image = rasterize(
            &mesh,
            camera,
            test_style(),
            [128, 128],
            Some(&outside),
            &AtomicBool::new(false),
        )
        .expect("image");
        assert_eq!(opaque_pixels(&image), opaque_pixels(&whole));
        assert_eq!(count_color(&image, test_style().cut), 0);
        assert_eq!(count_color(&image, test_style().section), 0);
    }

    #[test]
    fn number_and_byte_formatting() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(12), "12");
        assert_eq!(grouped(1292), "1,292");
        assert_eq!(grouped(1234567), "1,234,567");
        assert_eq!(format_quantity(122843.61), "122,843.61");
        assert_eq!(format_quantity(-284.14), "-284.14");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(64676), "63.2 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MiB");
    }

    #[test]
    fn view_cube_regions_tile_the_cube() {
        let regions = view_cube_hit_regions();
        let area: f32 = regions
            .iter()
            .map(|(_, [_, _, width, height])| width * height)
            .sum();
        assert_eq!(area, VIEW_CUBE_WIDTH * VIEW_CUBE_HEIGHT);
        for (index, (_, first)) in regions.iter().enumerate() {
            assert!(first[0] >= 0.0 && first[1] >= 0.0);
            assert!(first[2] > 0.0 && first[3] > 0.0);
            for (_, second) in regions.iter().skip(index + 1) {
                let overlap_x =
                    (first[0] + first[2]).min(second[0] + second[2]) - first[0].max(second[0]);
                let overlap_y =
                    (first[1] + first[3]).min(second[1] + second[3]) - first[1].max(second[1]);
                assert!(
                    overlap_x <= 0.0 || overlap_y <= 0.0,
                    "view cube regions must not overlap"
                );
            }
        }
        // The top face spans the full width; the side faces split the lower half.
        assert_eq!(regions[0].1[2], VIEW_CUBE_WIDTH);
        assert_eq!(regions[1].1[0], regions[2].1[0] - regions[1].1[2]);
    }

    #[test]
    fn view_cube_faces_meet_at_the_center() {
        let center = point(px(VIEW_CUBE_WIDTH / 2.0), px(VIEW_CUBE_TOP_HEIGHT));
        for (_, vertices) in view_cube_faces() {
            assert!(
                vertices.contains(&center),
                "every view cube face must share the inner vertex"
            );
            assert!(vertices.iter().all(|vertex| {
                vertex.x >= px(0.0)
                    && vertex.x <= px(VIEW_CUBE_WIDTH)
                    && vertex.y >= px(0.0)
                    && vertex.y <= px(VIEW_CUBE_HEIGHT)
            }));
        }
    }
}

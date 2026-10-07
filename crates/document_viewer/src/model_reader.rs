use crate::{
    DocumentItem,
    model_mesh::{ModelMesh, Topology, Vertex, cross, dot, sub},
};
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, MouseButton, PathBuilder, Pixels,
    Point, RenderImage, Rgba, Task, Window, canvas, img, point, px, rgb,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use ui::prelude::*;
use ui::{
    ContextMenu, Divider, DropdownMenu, Indicator, ToggleButtonGroup, ToggleButtonSimple, Tooltip,
};
use util::ResultExt as _;

const GIZMO_SIZE: f32 = 64.0;
const GIZMO_ARM: f32 = 20.0;
const GIZMO_LABEL_RADIUS: f32 = 27.0;

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
            shading: Shading::Solid,
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
}

fn color_rgb(color: impl Into<Rgba>) -> [u8; 3] {
    let color = color.into();
    [
        (color.r * 255.0) as u8,
        (color.g * 255.0) as u8,
        (color.b * 255.0) as u8,
    ]
}

fn darken(color: [u8; 3], factor: f32) -> [u8; 3] {
    color.map(|channel| (channel as f32 * factor) as u8)
}

pub struct ModelReader {
    focus: FocusHandle,
    mesh: Arc<ModelMesh>,
    format: SharedString,
    file_size: u64,
    camera: Camera,
    preset: ViewPreset,
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
        let mut reader = Self {
            focus: cx.focus_handle(),
            mesh,
            format: format.into(),
            file_size,
            camera: Camera::default(),
            preset: ViewPreset::Isometric,
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
            edge: darken(model, 0.4),
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
        });
        let dimensions = self.dimensions;
        let cancel = self.cancel.clone();
        let background = cx
            .background_spawn(async move { rasterize(&mesh, camera, style, dimensions, &cancel) });
        self.render_task = Some(cx.spawn_in(window, async move |this, cx| {
            let rendered = background.await;
            this.update_in(cx, |reader, window, cx| {
                reader.render_task = None;
                if let Some(image) = rendered {
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
        let panel_open = self.panel_open;
        h_flex()
            .gap_1()
            .p_1()
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
                    .trigger_size(ButtonSize::Compact),
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
                .selected_index(match shading {
                    Shading::Solid => 0,
                    Shading::SolidEdges => 1,
                    Shading::Wireframe => 2,
                }),
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
        let colors = cx.theme().colors();
        let dimensions = sub(self.mesh.maximum, self.mesh.minimum);
        let inspecting = self.inspect_task.is_some();
        let closed = self.mesh_closed();
        let topology = self.topology.as_ref();
        let geometry_open = self.geometry_open;
        let topology_open = self.topology_open;
        let file_open = self.file_open;
        v_flex()
            .w(px(260.0))
            .flex_none()
            .h_full()
            .border_l_1()
            .border_color(colors.border)
            .bg(colors.panel_background)
            .child(
                h_flex()
                    .px_2()
                    .py_1p5()
                    .justify_between()
                    .items_center()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(Label::new(i18n::t!("02d1bbbbc90c39f3")).size(LabelSize::Small))
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
                    .overflow_y_scroll()
                    .flex_1()
                    .min_h_0()
                    .child(section_header(
                        "model-geometry-header",
                        i18n::t!("fa9da5754ccfd409"),
                        geometry_open,
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
                    })
                    .child(section_header(
                        "model-topology-header",
                        i18n::t!("3a6c23b78b17490b"),
                        topology_open,
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
                    })
                    .child(section_header(
                        "model-file-header",
                        i18n::t!("50009ce1da4d15e1"),
                        file_open,
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
                    }),
            )
    }

    fn render_viewport(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity().downgrade();
        let gizmo = self.camera.axes.then(|| gizmo_axes(&self.camera));
        div()
            .id("model-viewport")
            .relative()
            .flex_1()
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
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let projection = if self.camera.perspective {
            i18n::t!("e3ad1bcc2cbd8537")
        } else {
            i18n::t!("0f04d53421188d24")
        };
        h_flex()
            .px_2()
            .py_1()
            .justify_between()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(
                Label::new(i18n::t!("30bd16f9d01e8dc6"))
                    .size(LabelSize::XSmall)
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

/// Projected gizmo axis: screen-space direction, view-space depth, color and label.
type GizmoAxis = (f32, f32, f32, u32, &'static str);

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
                .top(px(half - dy * GIZMO_LABEL_RADIUS - 5.0))
                .child(
                    Label::new(label)
                        .size(LabelSize::XSmall)
                        .color(Color::Custom(rgb(color).into())),
                )
        }))
}

fn section_header(
    id: &'static str,
    title: &'static str,
    open: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    h_flex()
        .id(id)
        .px_2()
        .py_1()
        .gap_1()
        .items_center()
        .cursor_pointer()
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
        .child(Label::new(title).size(LabelSize::Small).color(Color::Muted))
}

fn property_row(label: &'static str, value: impl Into<SharedString>) -> impl IntoElement {
    h_flex()
        .px_3()
        .py_0p5()
        .justify_between()
        .gap_2()
        .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
        .child(Label::new(value).size(LabelSize::Small))
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
        .py_0p5()
        .justify_between()
        .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
        .child(
            h_flex()
                .gap_1p5()
                .items_center()
                .child(Label::new(value).size(LabelSize::Small))
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
                    .child(self.render_viewport(cx))
                    .when(self.panel_open, |element| {
                        element.child(self.render_panel(cx))
                    }),
            )
            .child(self.render_status_bar(cx))
    }
}

fn rasterize(
    mesh: &ModelMesh,
    camera: Camera,
    style: RasterStyle,
    dimensions: [u32; 2],
    cancel: &AtomicBool,
) -> Option<Arc<RenderImage>> {
    let [width, height] = dimensions;
    let length = width as usize * height as usize;
    let mut pixels = vec![0u8; length * 4];
    let mut depth = vec![f64::INFINITY; length];
    let center: Vertex =
        std::array::from_fn(|axis| mesh.minimum[axis] * 0.5 + mesh.maximum[axis] * 0.5);
    let radius = sub(mesh.maximum, mesh.minimum)
        .into_iter()
        .fold(0.0f64, f64::max)
        .max(1e-20);
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
    // Model vertices are normalized to a unit extent before rotation.
    let view = |vertex: Vertex| rotate(sub(vertex, center).map(|value| value / radius));
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
    for (index, triangle) in mesh.triangles.iter().enumerate() {
        if index % 256 == 0 && cancel.load(Ordering::Relaxed) {
            return None;
        }
        let rotated = triangle.map(&view);
        let normal = cross(sub(rotated[1], rotated[0]), sub(rotated[2], rotated[0]));
        let normal_length = dot(normal, normal).sqrt().max(1e-20);
        let key = dot(normal, [0.35, 0.55, 0.76]).abs() / normal_length;
        let fill = dot(normal, [-0.5, -0.2, 0.6]).abs() / normal_length;
        let intensity = (0.30 + 0.62 * key + 0.22 * fill).min(1.0);
        let projected = rotated.map(&project);
        let area = edge(projected[0], projected[1], projected[2]);
        if area.abs() < 1e-12 {
            continue;
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
                return None;
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
                let color = match camera.shading {
                    Shading::Solid => shade(style.model, intensity),
                    Shading::SolidEdges if line => {
                        [style.edge[0], style.edge[1], style.edge[2], 255]
                    }
                    Shading::SolidEdges => shade(style.model, intensity),
                    Shading::Wireframe if line => {
                        [style.wire[0], style.wire[1], style.wire[2], 255]
                    }
                    Shading::Wireframe => continue,
                };
                pixels[offset * 4..offset * 4 + 4].copy_from_slice(&color);
            }
        }
    }
    let image = image::RgbaImage::from_raw(width, height, pixels)?;
    Some(Arc::new(RenderImage::new(smallvec::smallvec![
        image::Frame::new(image)
    ])))
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
    let [width, height] = dimensions;
    for division in 0..=DIVISIONS {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let offset = -EXTENT + 2.0 * EXTENT * division as f64 / DIVISIONS as f64;
        for (start, end) in [
            ([-EXTENT, floor, offset], [EXTENT, floor, offset]),
            ([offset, floor, -EXTENT], [offset, floor, EXTENT]),
        ] {
            let start = project(view(start));
            let end = project(view(end));
            let steps = ((end[0] - start[0]).hypot(end[1] - start[1]) * 1.5)
                .ceil()
                .max(1.0) as u32;
            for step in 0..=steps {
                let t = step as f64 / steps as f64;
                let x = (start[0] + (end[0] - start[0]) * t).round() as i64;
                let y = (start[1] + (end[1] - start[1]) * t).round() as i64;
                if x < 0 || y < 0 || x >= i64::from(width) || y >= i64::from(height) {
                    continue;
                }
                let distance = start[2] + (end[2] - start[2]) * t;
                let offset = y as usize * width as usize + x as usize;
                if distance < depth[offset] {
                    depth[offset] = distance;
                    pixels[offset * 4..offset * 4 + 4].copy_from_slice(&color);
                }
            }
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

    fn test_style() -> RasterStyle {
        RasterStyle {
            model: [120, 160, 210],
            edge: [48, 64, 84],
            wire: [200, 200, 200],
            grid: [90, 90, 90, 255],
        }
    }

    fn opaque_pixels(image: &Arc<RenderImage>) -> usize {
        image
            .as_bytes(0)
            .expect("pixels")
            .chunks_exact(4)
            .filter(|pixel| pixel[3] != 0)
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
            &AtomicBool::new(false),
        )
        .expect("image");
        camera.grid = false;
        let without_grid = rasterize(
            &mesh,
            camera,
            test_style(),
            [128, 128],
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
            &AtomicBool::new(false),
        )
        .expect("image");
        camera.shading = Shading::Wireframe;
        let wireframe = rasterize(
            &mesh,
            camera,
            test_style(),
            [128, 128],
            &AtomicBool::new(false),
        )
        .expect("image");
        camera.shading = Shading::SolidEdges;
        let edges = rasterize(
            &mesh,
            camera,
            test_style(),
            [128, 128],
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
}

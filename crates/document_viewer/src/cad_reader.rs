//! Native reader for 2D CAD drawings (ASCII DXF).
//!
//! Geometry is rasterized on a background thread into one frame per camera or
//! layer change, in the same style as the 3D model reader. Text is drawn as real
//! GPUI text on top of that frame, bounded to the labels that are both large
//! enough to read and inside the viewport.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use gpui::{
    Action as _, AnyElement, Context, Entity, FocusHandle, Focusable, Hsla, IntoElement,
    MouseButton, Pixels, Point, RenderImage, Task, Window, actions, canvas, img, px, rgb,
};
use ui::prelude::*;
use ui::{Checkbox, Divider, Tooltip};
use util::ResultExt as _;
use util::size::format_file_size;

use crate::DocumentItem;
use crate::cad_scene::{CadBounds, CadPalette, CadScene, resolve_color};

actions!(
    cad_reader,
    [
        /// Fits the whole drawing into the viewport.
        FitDrawing,
        /// Zooms in.
        ZoomIn,
        /// Zooms out.
        ZoomOut,
        /// Shows the drawing at one pixel per drawing unit.
        ZoomToActualSize,
        /// Toggles the layer list.
        ToggleLayers,
        /// Opens this DXF file in a text editor instead of the drawing view.
        OpenAsText,
    ]
);

/// One pixel per drawing unit, the reference scale for the zoom readout.
const ACTUAL_SIZE_ZOOM: f64 = 1.0;
/// Frame pixel budget. A larger viewport is rasterized at a smaller scale and
/// stretched by the image element, which keeps the aspect ratio and therefore
/// keeps the label overlay aligned.
const FRAME_PIXEL_BUDGET: u64 = 1280 * 960;
/// Upper bound on drawn line segments in one frame. Dense drawings report a
/// partial frame instead of stalling the renderer.
const MAX_FRAME_SEGMENTS: usize = 400_000;
/// Upper bound on text labels drawn in one frame.
const MAX_FRAME_LABELS: usize = 800;
/// Below this projected height a label is unreadable, so it is skipped.
const MIN_LABEL_PIXELS: f64 = 7.0;
/// The grid aims for a cell of roughly this many frame pixels.
const GRID_TARGET_PIXELS: f64 = 80.0;

#[derive(Clone, Copy, Debug, PartialEq)]
struct CadCamera {
    /// Drawing point at the center of the viewport.
    center: [f64; 2],
    /// Viewport pixels per drawing unit.
    zoom: f64,
    /// The zoom that fits the drawing, used to bound pan and zoom.
    fit_zoom: f64,
}

impl CadCamera {
    fn fit(bounds: CadBounds, viewport: [u32; 2]) -> Self {
        let [width, height] = [f64::from(viewport[0]), f64::from(viewport[1])];
        let zoom_x = if bounds.width() > 0.0 {
            width * 0.92 / bounds.width()
        } else {
            f64::INFINITY
        };
        let zoom_y = if bounds.height() > 0.0 {
            height * 0.92 / bounds.height()
        } else {
            f64::INFINITY
        };
        let zoom = zoom_x.min(zoom_y);
        let zoom = if zoom.is_finite() && zoom > 0.0 {
            zoom
        } else {
            ACTUAL_SIZE_ZOOM
        };
        Self {
            center: bounds.center(),
            zoom,
            fit_zoom: zoom,
        }
    }

    fn project(&self, point: [f64; 2], viewport: [u32; 2]) -> [f64; 2] {
        [
            f64::from(viewport[0]) / 2.0 + (point[0] - self.center[0]) * self.zoom,
            f64::from(viewport[1]) / 2.0 - (point[1] - self.center[1]) * self.zoom,
        ]
    }

    fn world_at(&self, pixel: [f64; 2], viewport: [u32; 2]) -> [f64; 2] {
        [
            self.center[0] + (pixel[0] - f64::from(viewport[0]) / 2.0) / self.zoom,
            self.center[1] - (pixel[1] - f64::from(viewport[1]) / 2.0) / self.zoom,
        ]
    }

    fn clamp_zoom(&self, zoom: f64) -> f64 {
        let lowest = (self.fit_zoom / 50_000.0).max(f64::MIN_POSITIVE);
        let highest = (self.fit_zoom * 50_000.0).max(ACTUAL_SIZE_ZOOM);
        zoom.clamp(lowest, highest)
    }
}

#[derive(Clone, PartialEq)]
struct CadRasterStyle {
    background: [u8; 3],
    palette: CadPalette,
    grid: [u8; 3],
}

struct RasteredFrame {
    image: Arc<RenderImage>,
    partial: bool,
}

pub struct CadReader {
    item: Entity<DocumentItem>,
    focus: FocusHandle,
    scene: Arc<CadScene>,
    /// Which layers the reader draws. The parsed scene stays immutable so the
    /// render thread can read it without a lock.
    visibility: Vec<bool>,
    file_size: u64,
    camera: CadCamera,
    viewport: [u32; 2],
    layers_open: bool,
    show_text: bool,
    show_grid: bool,
    image: Option<Arc<RenderImage>>,
    rendered_style: Option<CadRasterStyle>,
    rendered_camera: Option<CadCamera>,
    partial: bool,
    dirty: bool,
    drag: Option<Point<Pixels>>,
    cancel: Arc<AtomicBool>,
    render_task: Option<Task<()>>,
}

impl CadReader {
    pub fn new(item: Entity<DocumentItem>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let window_handle = window.window_handle();
        cx.on_release(move |reader, cx| {
            reader.cancel.store(true, Ordering::Relaxed);
            reader.render_task = None;
            if let Some(image) = reader.image.take() {
                window_handle
                    .update(cx, |_, window, _| window.drop_image(image))
                    .log_err();
            }
        })
        .detach();

        let (scene, file_size) = {
            let item = item.read(cx);
            let file_size = match item.file.disk_state {
                language::DiskState::Present { size, .. } => size,
                _ => 0,
            };
            (item.cad.clone().expect("parsed drawing item"), file_size)
        };
        let camera = match scene.bounds {
            Some(bounds) => CadCamera::fit(bounds, [1280, 960]),
            None => CadCamera {
                center: [0.0, 0.0],
                zoom: ACTUAL_SIZE_ZOOM,
                fit_zoom: ACTUAL_SIZE_ZOOM,
            },
        };

        let visibility = scene.initial_visibility();
        Self {
            item,
            focus: cx.focus_handle(),
            scene,
            visibility,
            file_size,
            camera,
            viewport: [0, 0],
            layers_open: false,
            show_text: true,
            show_grid: true,
            image: None,
            rendered_style: None,
            rendered_camera: None,
            partial: false,
            dirty: true,
            drag: None,
            cancel: Arc::new(AtomicBool::new(false)),
            render_task: None,
        }
    }

    /// Rebuilds the drawing from the item's new bytes while keeping the camera
    /// and layer/text/grid toggles, so a re-exported DXF stays in view.
    pub fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(scene) = self.item.read(cx).cad.clone() else {
            return;
        };
        let file_size = match self.item.read(cx).file.disk_state {
            language::DiskState::Present { size, .. } => size,
            _ => 0,
        };

        self.cancel.store(true, Ordering::Relaxed);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.render_task = None;
        if let Some(previous) = self.image.take() {
            window.drop_image(previous).log_err();
        }
        self.visibility = scene.initial_visibility();
        self.scene = scene;
        self.file_size = file_size;
        self.rendered_camera = None;
        self.partial = false;
        self.dirty = true;
        cx.notify();
    }

    fn raster_style(&self, cx: &App) -> CadRasterStyle {
        let colors = cx.theme().colors();
        let background = to_rgb(colors.editor_background);
        let foreground = to_rgb(colors.text);
        CadRasterStyle {
            background,
            palette: CadPalette::new(foreground),
            // The grid stays close to the background so it never competes with
            // drawing geometry.
            grid: mix(background, foreground, 0.18),
        }
    }

    fn layer_visible(&self, layer: u16) -> bool {
        self.visibility
            .get(layer as usize)
            .copied()
            .unwrap_or(false)
    }

    fn request_render(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.dirty || self.render_task.is_some() {
            return;
        }
        let Some(style) = self.rendered_style.clone() else {
            return;
        };
        self.dirty = false;
        let scene = self.scene.clone();
        let camera = self.camera;
        let viewport = self.viewport;
        let visibility = self.visibility.clone();
        let dimensions = raster_dimensions(viewport);
        let show_grid = self.show_grid;
        let cancel = self.cancel.clone();
        let background = cx.background_spawn(async move {
            rasterize(
                &scene,
                &visibility,
                camera,
                viewport,
                dimensions,
                &style,
                show_grid,
                &cancel,
            )
        });
        self.render_task = Some(cx.spawn_in(window, async move |this, cx| {
            let rendered = background.await;
            this.update_in(cx, |reader, window, cx| {
                reader.render_task = None;
                if let Some(rendered) = rendered {
                    reader.partial = rendered.partial;
                    reader.rendered_camera = Some(camera);
                    if let Some(previous) = reader.image.replace(rendered.image) {
                        window.drop_image(previous).log_err();
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    fn fit(&mut self, cx: &mut Context<Self>) {
        let bounds = self
            .scene
            .visible_bounds(&self.visibility)
            .or(self.scene.bounds);
        if let Some(bounds) = bounds {
            self.camera = CadCamera::fit(bounds, self.viewport);
            self.dirty = true;
            cx.notify();
        }
    }

    fn zoom_by(&mut self, factor: f64, anchor: [f64; 2], cx: &mut Context<Self>) {
        let before = self.camera.world_at(anchor, self.viewport);
        self.camera.zoom = self.camera.clamp_zoom(self.camera.zoom * factor);
        let after = self.camera.world_at(anchor, self.viewport);
        self.camera.center[0] += before[0] - after[0];
        self.camera.center[1] += before[1] - after[1];
        self.dirty = true;
        cx.notify();
    }

    fn zoom_to_actual_size(&mut self, cx: &mut Context<Self>) {
        self.camera.zoom = self.camera.clamp_zoom(ACTUAL_SIZE_ZOOM);
        self.dirty = true;
        cx.notify();
    }

    fn toggle_layer(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(visible) = self.visibility.get_mut(index) {
            *visible = !*visible;
            self.dirty = true;
            cx.notify();
        }
    }

    fn render_toolbar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let layers_label = if self.layers_open {
            i18n::t!("73ef367125e92124")
        } else {
            i18n::t!("1a47b08407571425")
        };
        let labels_label = if self.show_text {
            i18n::t!("5c3b7a359fe4403e")
        } else {
            i18n::t!("3df0f4e09cf75f90")
        };
        let grid_label = if self.show_grid {
            i18n::t!("8b2ef34414b2a709")
        } else {
            i18n::t!("873ebb9300e109b8")
        };
        h_flex()
            .id("cad-toolbar")
            .debug_selector(|| "cad-toolbar".to_string())
            .flex_none()
            .h(px(40.0))
            .w_full()
            .gap_1()
            .px_2()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Button::new("cad-fit", i18n::t!("021addc424ab6e21"))
                    .style(ButtonStyle::Subtle)
                    .on_click(cx.listener(|reader, _, _, cx| reader.fit(cx))),
            )
            .child(
                Button::new("cad-zoom-in", i18n::t!("80f8fbcfa0117633"))
                    .style(ButtonStyle::Subtle)
                    .on_click(cx.listener(|reader, _, _, cx| {
                        let anchor = center_of(reader.viewport);
                        reader.zoom_by(1.25, anchor, cx);
                    })),
            )
            .child(
                Button::new("cad-zoom-out", i18n::t!("290f68030501cd9c"))
                    .style(ButtonStyle::Subtle)
                    .on_click(cx.listener(|reader, _, _, cx| {
                        let anchor = center_of(reader.viewport);
                        reader.zoom_by(0.8, anchor, cx);
                    })),
            )
            .child(
                Button::new("cad-actual-size", i18n::t!("74cdbe70e394d004"))
                    .style(ButtonStyle::Subtle)
                    .on_click(cx.listener(|reader, _, _, cx| reader.zoom_to_actual_size(cx))),
            )
            .child(Divider::vertical())
            .child(
                Button::new("cad-layers", layers_label)
                    .style(ButtonStyle::Subtle)
                    .on_click(cx.listener(|reader, _, _, cx| {
                        reader.layers_open = !reader.layers_open;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("cad-text", labels_label)
                    .style(ButtonStyle::Subtle)
                    .on_click(cx.listener(|reader, _, _, cx| {
                        reader.show_text = !reader.show_text;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("cad-grid", grid_label)
                    .style(ButtonStyle::Subtle)
                    .on_click(cx.listener(|reader, _, _, cx| {
                        reader.show_grid = !reader.show_grid;
                        reader.dirty = true;
                        cx.notify();
                    })),
            )
            .child(div().flex_1())
            .child(
                Button::new("cad-open-as-text", i18n::t!("1e94e49191ae5056"))
                    .style(ButtonStyle::Subtle)
                    .tooltip(Tooltip::text(i18n::t!("a57b69caeee82f70")))
                    .on_click(cx.listener(|_, _, window, cx| {
                        window.dispatch_action(OpenAsText.boxed_clone(), cx);
                    })),
            )
    }

    fn render_viewport(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity().downgrade();
        let overlay = if self.show_text {
            self.render_labels(cx)
        } else {
            Vec::new()
        };
        div()
            .id("cad-viewport")
            .debug_selector(|| "cad-viewport".to_string())
            .relative()
            .flex_1()
            .h_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|reader, event: &gpui::MouseDownEvent, _, cx| {
                    reader.drag = Some(event.position);
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
                if let Some(previous) = reader.drag {
                    let delta = event.position - previous;
                    let zoom = reader.camera.zoom.max(f64::MIN_POSITIVE);
                    reader.camera.center[0] -= f64::from(delta.x) / zoom;
                    reader.camera.center[1] += f64::from(delta.y) / zoom;
                    reader.drag = Some(event.position);
                    reader.dirty = true;
                    cx.notify();
                }
            }))
            .on_scroll_wheel(
                cx.listener(|reader, event: &gpui::ScrollWheelEvent, _, cx| {
                    let delta = f64::from(event.delta.pixel_delta(px(20.0)).y);
                    let anchor = [f64::from(event.position.x), f64::from(event.position.y)];
                    reader.zoom_by((delta * 0.002).exp(), anchor, cx);
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
            .children(overlay)
            .child(
                canvas(
                    move |bounds, _, cx| {
                        entity
                            .update(cx, |reader, cx| {
                                let width = f32::from(bounds.size.width).clamp(1.0, 4096.0) as u32;
                                let height =
                                    f32::from(bounds.size.height).clamp(1.0, 4096.0) as u32;
                                if reader.viewport != [width, height] {
                                    let first = reader.viewport == [0, 0];
                                    reader.viewport = [width, height];
                                    if first {
                                        reader.fit(cx);
                                    }
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
    }

    /// Labels for the text entities that are readable at the current zoom.
    fn render_labels(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(camera) = self.rendered_camera else {
            return Vec::new();
        };
        let viewport = self.viewport;
        if viewport[0] == 0 {
            return Vec::new();
        }
        let palette = CadPalette::new(to_rgb(cx.theme().colors().text));
        let mut labels = Vec::new();
        for text in &self.scene.texts {
            if labels.len() >= MAX_FRAME_LABELS {
                break;
            }
            if !self.layer_visible(text.layer) {
                continue;
            }
            // A missing height means the file did not declare one; such a label
            // is shown at the minimum readable size instead of being dropped.
            let projected = text.height * camera.zoom;
            if text.height > 0.0 && projected < MIN_LABEL_PIXELS {
                continue;
            }
            let pixel_height = projected.clamp(MIN_LABEL_PIXELS, 96.0);
            let [x, y] = camera.project(text.position, viewport);
            if x < -200.0
                || y < -50.0
                || x > f64::from(viewport[0]) + 200.0
                || y > f64::from(viewport[1]) + 50.0
            {
                continue;
            }
            let layer_color = self
                .scene
                .layers
                .get(text.layer as usize)
                .map(|layer| layer.color)
                .unwrap_or(0);
            let [red, green, blue] = resolve_color(text.color, layer_color, &palette);
            labels.push(
                div()
                    .absolute()
                    .left(px(x as f32))
                    .top(px((y - pixel_height / 2.0) as f32))
                    .child(
                        Label::new(text.text.clone())
                            .size(LabelSize::Custom(rems_from_px(pixel_height as f32)))
                            .color(Color::Custom(Hsla::from(rgb((u32::from(red) << 16)
                                | (u32::from(green) << 8)
                                | u32::from(blue))))),
                    )
                    .into_any_element(),
            );
        }
        labels
    }

    fn render_layers(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().colors().border;
        let palette = CadPalette::new(to_rgb(cx.theme().colors().text));
        let mut rows = Vec::new();
        for (index, layer) in self.scene.layers.iter().enumerate() {
            let [red, green, blue] = resolve_color(layer.color, 0, &palette);
            let visible = self.visibility.get(index).copied().unwrap_or(false);
            let state: ToggleState = visible.into();
            rows.push(
                h_flex()
                    .id(("cad-layer", index))
                    .gap_2()
                    .px_2()
                    .py_1()
                    .child(
                        div()
                            .flex_none()
                            .size_2()
                            .rounded_sm()
                            .bg(Hsla::from(rgb((u32::from(red) << 16)
                                | (u32::from(green) << 8)
                                | u32::from(blue)))),
                    )
                    .child(Checkbox::new(("cad-layer-toggle", index), state).on_click(
                        cx.listener(move |reader, _, _, cx| reader.toggle_layer(index, cx)),
                    ))
                    .child(Label::new(layer.name.clone()).single_line())
                    .into_any_element(),
            );
        }
        v_flex()
            .id("cad-layers")
            .debug_selector(|| "cad-layers".to_string())
            .flex_none()
            .w(px(260.0))
            .h_full()
            .border_l_1()
            .border_color(border)
            .child(
                h_flex()
                    .flex_none()
                    .gap_2()
                    .px_2()
                    .py_2()
                    .child(
                        Label::new(i18n::t!("0bcd66e677f4ee72"))
                            .size(LabelSize::Small)
                            .weight(gpui::FontWeight::SEMIBOLD),
                    )
                    .child(
                        Label::new(i18n::t_args!("4e9c0dee0d467861", self.scene.layers.len()))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                v_flex()
                    .id("cad-layer-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(rows),
            )
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut details = Vec::new();
        if let Some(bounds) = self.scene.bounds {
            details.push(i18n::t!(
                "ece2cad2df943607",
                width = format_number(bounds.width()),
                height = format_number(bounds.height())
            ));
        }
        details.push(i18n::t_args!(
            "80fee18c4c797dd3",
            self.scene.visible_primitive_count(&self.visibility)
        ));
        if let Some(unit) = self.scene.units.and_then(unit_symbol) {
            details.push(unit.to_owned());
        }
        if self.scene.paper_space_entities > 0 {
            details.push(i18n::t_args!(
                "f9df4f12177135fc",
                self.scene.paper_space_entities
            ));
        }
        let skipped = self.scene.skipped_total();
        if skipped > 0 {
            let kinds = self
                .scene
                .skipped
                .iter()
                .take(3)
                .map(|entry| entry.0.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            details.push(i18n::t!("0a2d9e5cb246c9f2", count = skipped, kinds = kinds));
        }
        if self.scene.truncated {
            details.push(i18n::t!("f354349235ddd586").to_owned());
        }
        if self.partial {
            details.push(i18n::t!("8b2cfcc58121cfbd").to_owned());
        }
        details.push(i18n::t!(
            "5fae915de35e2dc2",
            size = format_file_size(self.file_size, false),
            zoom = format!("{:.0}", self.camera.zoom / ACTUAL_SIZE_ZOOM * 100.0)
        ));

        h_flex()
            .id("cad-status")
            .debug_selector(|| "cad-status".to_string())
            .flex_none()
            .h(px(24.0))
            .w_full()
            .gap_3()
            .px_2()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .children(details.into_iter().map(|detail| {
                Label::new(detail)
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .single_line()
            }))
    }

    fn render_empty(&self) -> AnyElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .debug_selector(|| "cad-empty".to_string())
            .child(Label::new(i18n::t!("4f86881b2f3cc387")).color(Color::Muted))
            .into_any_element()
    }
}

impl Focusable for CadReader {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for CadReader {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let style = self.raster_style(cx);
        if self.rendered_style.as_ref() != Some(&style) {
            self.rendered_style = Some(style);
            self.dirty = true;
        }
        self.request_render(window, cx);
        let empty = self.scene.visible_primitive_count(&self.visibility) == 0;
        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .key_context("CadReader")
            .bg(cx.theme().colors().editor_background)
            .on_action(cx.listener(|reader, _: &FitDrawing, _, cx| reader.fit(cx)))
            .on_action(cx.listener(|reader, _: &ZoomIn, _, cx| {
                let anchor = center_of(reader.viewport);
                reader.zoom_by(1.25, anchor, cx);
            }))
            .on_action(cx.listener(|reader, _: &ZoomOut, _, cx| {
                let anchor = center_of(reader.viewport);
                reader.zoom_by(0.8, anchor, cx);
            }))
            .on_action(cx.listener(|reader, _: &ZoomToActualSize, _, cx| {
                reader.zoom_to_actual_size(cx);
            }))
            .on_action(cx.listener(|reader, _: &ToggleLayers, _, cx| {
                reader.layers_open = !reader.layers_open;
                cx.notify();
            }))
            .child(self.render_toolbar(cx))
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
                            .when(empty, |element| element.child(self.render_empty()))
                            .when(!empty, |element| {
                                element
                                    .child(self.render_viewport(cx))
                                    .child(self.render_status_bar(cx))
                            }),
                    )
                    .when(self.layers_open, |element| {
                        element.child(self.render_layers(cx))
                    }),
            )
    }
}

fn center_of(viewport: [u32; 2]) -> [f64; 2] {
    [f64::from(viewport[0]) / 2.0, f64::from(viewport[1]) / 2.0]
}

/// Keeps one frame at a bounded pixel count while preserving the viewport's
/// aspect ratio, so the image element can stretch it without letterboxing and
/// the label overlay stays aligned.
fn raster_dimensions(viewport: [u32; 2]) -> [u32; 2] {
    let width = viewport[0].max(1);
    let height = viewport[1].max(1);
    let pixels = u64::from(width) * u64::from(height);
    if pixels <= FRAME_PIXEL_BUDGET {
        return [width, height];
    }
    let scale = (FRAME_PIXEL_BUDGET as f64 / pixels as f64).sqrt();
    [
        ((f64::from(width) * scale) as u32).max(1),
        ((f64::from(height) * scale) as u32).max(1),
    ]
}

fn to_rgb(color: impl Into<gpui::Rgba>) -> [u8; 3] {
    let color = color.into();
    [
        (color.r * 255.0).round() as u8,
        (color.g * 255.0).round() as u8,
        (color.b * 255.0).round() as u8,
    ]
}

fn mix(from: [u8; 3], to: [u8; 3], amount: f64) -> [u8; 3] {
    let blend = |first: u8, second: u8| {
        (f64::from(first) + (f64::from(second) - f64::from(first)) * amount).round() as u8
    };
    [
        blend(from[0], to[0]),
        blend(from[1], to[1]),
        blend(from[2], to[2]),
    ]
}

fn format_number(value: f64) -> String {
    if !value.is_finite() {
        return "0".to_owned();
    }
    if value.abs() >= 1000.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.3}")
    }
}

/// Drawing units, as declared by `$INSUNITS`.
fn unit_symbol(units: i32) -> Option<&'static str> {
    Some(match units {
        1 => "in",
        2 => "ft",
        3 => "mi",
        4 => "mm",
        5 => "cm",
        6 => "m",
        7 => "km",
        8 => "µin",
        9 => "mil",
        10 => "yd",
        14 => "dm",
        15 => "dam",
        16 => "hm",
        _ => return None,
    })
}

/// Rasterizes one frame. Pixels are written in BGRA, which is what GPUI's
/// `RenderImage` expects.
#[allow(clippy::too_many_arguments)]
fn rasterize(
    scene: &CadScene,
    visibility: &[bool],
    camera: CadCamera,
    viewport: [u32; 2],
    dimensions: [u32; 2],
    style: &CadRasterStyle,
    show_grid: bool,
    cancel: &AtomicBool,
) -> Option<RasteredFrame> {
    let [width, height] = dimensions;
    if width == 0 || height == 0 {
        return None;
    }
    let scale = if viewport[0] > 0 {
        f64::from(width) / f64::from(viewport[0])
    } else {
        1.0
    };
    let length = width as usize * height as usize;
    let mut pixels = vec![0u8; length * 4];
    let background = style.background;
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.copy_from_slice(&[background[2], background[1], background[0], 255]);
    }
    if cancel.load(Ordering::Relaxed) {
        return None;
    }

    let project = |point: [f64; 2]| -> [f64; 2] {
        let projected = camera.project(point, viewport);
        [projected[0] * scale, projected[1] * scale]
    };
    if show_grid && !draw_grid(dimensions, scale, camera, style, &mut pixels, cancel) {
        return None;
    }

    let visible = |layer: u16| visibility.get(layer as usize).copied().unwrap_or(false);
    for fill in &scene.fills {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        if !visible(fill.layer) {
            continue;
        }
        let layer_color = scene
            .layers
            .get(fill.layer as usize)
            .map(|layer| layer.color)
            .unwrap_or(0);
        let color = resolve_color(fill.color, layer_color, &style.palette);
        let points: Vec<[f64; 2]> = scene.points
            [fill.start as usize..(fill.start + fill.len) as usize]
            .iter()
            .map(|point| project(*point))
            .collect();
        fill_polygon(&points, color, &mut pixels, dimensions);
    }

    let mut drawn = 0usize;
    let mut partial = false;
    for path in &scene.paths {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        if !visible(path.layer) {
            continue;
        }
        if drawn >= MAX_FRAME_SEGMENTS {
            partial = true;
            break;
        }
        let layer_color = scene
            .layers
            .get(path.layer as usize)
            .map(|layer| layer.color)
            .unwrap_or(0);
        let color = resolve_color(path.color, layer_color, &style.palette);
        let points = &scene.points[path.start as usize..(path.start + path.len) as usize];
        let mut previous: Option<[f64; 2]> = None;
        let mut first: Option<[f64; 2]> = None;
        for point in points {
            let projected = project(*point);
            if first.is_none() {
                first = Some(projected);
            }
            if let Some(previous) = previous {
                drawn += 1;
                draw_segment(previous, projected, color, &mut pixels, dimensions);
            }
            previous = Some(projected);
        }
        if path.closed
            && let (Some(previous), Some(first)) = (previous, first)
        {
            drawn += 1;
            draw_segment(previous, first, color, &mut pixels, dimensions);
        }
    }

    let image = image::RgbaImage::from_raw(width, height, pixels)?;
    Some(RasteredFrame {
        image: Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(
            image
        )])),
        partial,
    })
}

/// Draws a grid at a "nice" spacing derived from the current zoom.
fn draw_grid(
    dimensions: [u32; 2],
    scale: f64,
    camera: CadCamera,
    style: &CadRasterStyle,
    pixels: &mut [u8],
    cancel: &AtomicBool,
) -> bool {
    let [width, height] = dimensions;
    let target = GRID_TARGET_PIXELS / (camera.zoom * scale).max(f64::MIN_POSITIVE);
    if !target.is_finite() || target <= 0.0 {
        return true;
    }
    let magnitude = 10f64.powf(target.log10().floor());
    let step = (target / magnitude).round().clamp(1.0, 100.0) * magnitude;
    if !step.is_finite() || step <= 0.0 {
        return true;
    }
    let pixels_per_world = camera.zoom * scale;
    let world_left = camera.center[0] - f64::from(width) / (2.0 * pixels_per_world);
    let world_right = camera.center[0] + f64::from(width) / (2.0 * pixels_per_world);
    let world_bottom = camera.center[1] - f64::from(height) / (2.0 * pixels_per_world);
    let world_top = camera.center[1] + f64::from(height) / (2.0 * pixels_per_world);
    let color = [style.grid[2], style.grid[1], style.grid[0], 255];

    // A zoomed-out view could otherwise ask for an unbounded number of lines.
    let maximum_lines = (u64::from(width) + u64::from(height)) as f64;
    let vertical = (world_left / step).floor() as i64;
    let last_vertical = (world_right / step).ceil() as i64;
    if (last_vertical - vertical) as f64 > maximum_lines {
        return true;
    }
    for index in vertical..=last_vertical {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let x = ((index as f64 * step - camera.center[0]) * pixels_per_world
            + f64::from(width) / 2.0)
            .round();
        if x >= 0.0 && x < f64::from(width) {
            let x = x as u32;
            for y in 0..height {
                let offset = (y as usize * width as usize + x as usize) * 4;
                pixels[offset..offset + 4].copy_from_slice(&color);
            }
        }
    }
    let horizontal = (world_bottom / step).floor() as i64;
    let last_horizontal = (world_top / step).ceil() as i64;
    if (last_horizontal - horizontal) as f64 > maximum_lines {
        return true;
    }
    for index in horizontal..=last_horizontal {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let y = (f64::from(height) / 2.0
            - (index as f64 * step - camera.center[1]) * pixels_per_world)
            .round();
        if y >= 0.0 && y < f64::from(height) {
            let row = y as usize * width as usize * 4;
            for x in 0..width {
                let offset = row + x as usize * 4;
                pixels[offset..offset + 4].copy_from_slice(&color);
            }
        }
    }
    true
}

/// Fills a polygon with the even-odd rule, one scanline at a time, which handles
/// both the convex quads of `SOLID`/`3DFACE` and concave boundaries.
fn fill_polygon(points: &[[f64; 2]], color: [u8; 3], pixels: &mut [u8], dimensions: [u32; 2]) {
    if points.len() < 3 {
        return;
    }
    let [width, height] = dimensions;
    let mut minimum = f64::INFINITY;
    let mut maximum = f64::NEG_INFINITY;
    for point in points {
        minimum = minimum.min(point[1]);
        maximum = maximum.max(point[1]);
    }
    if !minimum.is_finite() || !maximum.is_finite() {
        return;
    }
    let first_row = (minimum - 1.0).max(0.0) as u32;
    let last_row = (((maximum + 1.0).min(f64::from(height) - 1.0)).max(0.0)) as u32;
    let mut crossings: Vec<f64> = Vec::with_capacity(8);
    for row in first_row..=last_row {
        let y = row as f64 + 0.5;
        crossings.clear();
        for index in 0..points.len() {
            let first = points[index];
            let second = points[(index + 1) % points.len()];
            if (first[1] <= y) == (second[1] <= y) {
                continue;
            }
            let ratio = (y - first[1]) / (second[1] - first[1]);
            crossings.push(first[0] + (second[0] - first[0]) * ratio);
        }
        if crossings.len() < 2 {
            continue;
        }
        crossings.sort_by(|first, second| {
            first
                .partial_cmp(second)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for pair in crossings.chunks_exact(2) {
            let start = pair[0].ceil().max(0.0) as u32;
            let end = (pair[1].ceil().clamp(0.0, f64::from(width))) as u32;
            for x in start..end {
                let offset = row as usize * width as usize + x as usize;
                pixels[offset * 4..offset * 4 + 4]
                    .copy_from_slice(&[color[2], color[1], color[0], 255]);
            }
        }
    }
}

/// Steps one pixel at a time along a segment, so the cost is proportional to the
/// drawn length rather than to the number of primitives.
fn draw_segment(
    start: [f64; 2],
    end: [f64; 2],
    color: [u8; 3],
    pixels: &mut [u8],
    dimensions: [u32; 2],
) {
    let [width, height] = dimensions;
    let delta_x = end[0] - start[0];
    let delta_y = end[1] - start[1];
    let steps = delta_x.abs().max(delta_y.abs()).ceil();
    if !steps.is_finite() || steps < 1.0 {
        return;
    }
    let steps = steps.min(f64::from(width + height) * 2.0);
    let increment_x = delta_x / steps;
    let increment_y = delta_y / steps;
    let mut x = start[0];
    let mut y = start[1];
    let bgra = [color[2], color[1], color[0], 255];
    for step in 0..=steps as u32 {
        if step > 0 {
            x += increment_x;
            y += increment_y;
        }
        let pixel_x = x.round();
        let pixel_y = y.round();
        if pixel_x < 0.0 || pixel_y < 0.0 {
            continue;
        }
        let pixel_x = pixel_x as u32;
        let pixel_y = pixel_y as u32;
        if pixel_x >= width || pixel_y >= height {
            continue;
        }
        let offset = pixel_y as usize * width as usize + pixel_x as usize;
        pixels[offset * 4..offset * 4 + 4].copy_from_slice(&bgra);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cad_dxf::minimal_dxf;

    fn parse_scene(entities: &str) -> Arc<CadScene> {
        Arc::new(crate::cad_dxf::parse(&minimal_dxf(entities)).expect("scene"))
    }

    fn test_style() -> CadRasterStyle {
        CadRasterStyle {
            background: [0, 0, 0],
            palette: CadPalette::new([255, 255, 255]),
            grid: [40, 40, 40],
        }
    }

    fn visibility(scene: &CadScene) -> Vec<bool> {
        scene
            .layers
            .iter()
            .map(|layer| layer.visible && !layer.frozen)
            .collect()
    }

    fn render(
        scene: &Arc<CadScene>,
        visible: &[bool],
        camera: CadCamera,
        size: u32,
    ) -> RasteredFrame {
        rasterize(
            scene,
            visible,
            camera,
            [size, size],
            [size, size],
            &test_style(),
            false,
            &AtomicBool::new(false),
        )
        .expect("frame")
    }

    fn drawable_pixels(image: &Arc<RenderImage>) -> usize {
        image
            .as_bytes(0)
            .expect("pixels")
            .chunks_exact(4)
            .filter(|pixel| pixel[..3] != [0, 0, 0])
            .count()
    }

    #[test]
    fn frame_pixels_are_written_in_bgra() {
        let scene = parse_scene("0\nLINE\n8\nWALLS\n10\n0.0\n20\n0.0\n11\n10.0\n21\n0.0\n");
        let camera = CadCamera::fit(scene.bounds.unwrap(), [64, 64]);
        let frame = render(&scene, &visibility(&scene), camera, 64);
        let bytes = frame.image.as_bytes(0).expect("pixels");
        // Layer WALLS is ACI 1 (pure red), which is the last channel in BGRA.
        assert!(
            bytes.chunks_exact(4).any(|pixel| pixel == [0, 0, 255, 255]),
            "a pure red line must be written as BGRA"
        );
    }

    #[test]
    fn hidden_layers_are_not_rasterized() {
        let scene = parse_scene("0\nLINE\n8\nWALLS\n10\n0.0\n20\n0.0\n11\n10.0\n21\n0.0\n");
        let camera = CadCamera::fit(scene.bounds.unwrap(), [32, 32]);
        let mut hidden = visibility(&scene);
        hidden.iter_mut().for_each(|visible| *visible = false);
        let empty = render(&scene, &hidden, camera, 32);
        assert_eq!(drawable_pixels(&empty.image), 0);
        let shown = render(&scene, &visibility(&scene), camera, 32);
        assert!(drawable_pixels(&shown.image) > 0);
    }

    #[test]
    fn rasterization_is_cancellable() {
        let scene = parse_scene("0\nLINE\n8\nWALLS\n10\n0.0\n20\n0.0\n11\n10.0\n21\n0.0\n");
        let cancel = AtomicBool::new(true);
        assert!(
            rasterize(
                &scene,
                &visibility(&scene),
                CadCamera::fit(scene.bounds.unwrap(), [32, 32]),
                [32, 32],
                [32, 32],
                &test_style(),
                false,
                &cancel,
            )
            .is_none()
        );
    }

    #[test]
    fn raster_dimensions_preserve_the_aspect_ratio() {
        assert_eq!(raster_dimensions([640, 480]), [640, 480]);
        let wide = raster_dimensions([4000, 1000]);
        assert!(u64::from(wide[0]) * u64::from(wide[1]) <= FRAME_PIXEL_BUDGET);
        let original = 4000.0 / 1000.0;
        let scaled = f64::from(wide[0]) / f64::from(wide[1]);
        assert!((original - scaled).abs() < 0.02, "{wide:?}");
    }

    #[test]
    fn fitting_centers_the_drawing_in_the_viewport() {
        let bounds = CadBounds {
            min: [0.0, 0.0],
            max: [100.0, 50.0],
        };
        let camera = CadCamera::fit(bounds, [400, 400]);
        assert_eq!(camera.center, [50.0, 25.0]);
        let center = camera.project([50.0, 25.0], [400, 400]);
        assert!((center[0] - 200.0).abs() < 1e-9);
        assert!((center[1] - 200.0).abs() < 1e-9);
        // The drawing stays inside the viewport with a small margin.
        let corner = camera.project([100.0, 50.0], [400, 400]);
        assert!(corner[0] <= 400.0 && corner[1] >= 0.0, "{corner:?}");
    }

    #[test]
    fn zoom_keeps_the_point_under_the_cursor() {
        let bounds = CadBounds {
            min: [0.0, 0.0],
            max: [100.0, 100.0],
        };
        let mut camera = CadCamera::fit(bounds, [400, 400]);
        let anchor = [300.0, 120.0];
        let before = camera.world_at(anchor, [400, 400]);
        camera.zoom = camera.clamp_zoom(camera.zoom * 1.5);
        let after = camera.world_at(anchor, [400, 400]);
        camera.center[0] += before[0] - after[0];
        camera.center[1] += before[1] - after[1];
        let projected = camera.project(before, [400, 400]);
        assert!((projected[0] - anchor[0]).abs() < 1e-6, "{projected:?}");
        assert!((projected[1] - anchor[1]).abs() < 1e-6, "{projected:?}");
    }

    #[test]
    fn a_closed_circle_is_rasterized_on_every_side() {
        let scene = parse_scene("0\nCIRCLE\n8\nWALLS\n10\n5.0\n20\n5.0\n40\n4.0\n");
        let camera = CadCamera::fit(scene.bounds.unwrap(), [64, 64]);
        let frame = render(&scene, &visibility(&scene), camera, 64);
        let bytes = frame.image.as_bytes(0).expect("pixels");
        let width = 64usize;
        let lit = |x: usize, y: usize| bytes[(y * width + x) * 4 + 2] > 0;
        let center = camera.project([5.0, 5.0], [64, 64]);
        let radius = 4.0 * camera.zoom;
        // Every extreme of the circle must be drawn, which only happens when the
        // path closes back onto its first point.
        for (delta_x, delta_y) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
            let x = center[0] + f64::from(delta_x) * radius;
            let y = center[1] + f64::from(delta_y) * radius;
            let drawn = (-3..=3).any(|offset| {
                let candidate_x = (x.round() as i32 + offset).clamp(0, 63) as usize;
                let candidate_y = (y.round() as i32 + offset).clamp(0, 63) as usize;
                lit(candidate_x, candidate_y)
            });
            assert!(
                drawn,
                "the circle must be drawn at ({x:.1},{y:.1}) for direction ({delta_x},{delta_y})"
            );
        }
        // The center stays empty: only the outline is drawn.
        let center_x = center[0].round() as usize;
        let center_y = center[1].round() as usize;
        assert!(!lit(center_x, center_y));
    }

    #[test]
    fn thick_drawings_report_a_partial_frame() {
        let mut entities = String::new();
        for index in 0..MAX_FRAME_SEGMENTS + 100 {
            entities.push_str(&format!(
                "0\nLINE\n8\nWALLS\n10\n{index}.0\n20\n0.0\n11\n{index}.0\n21\n1.0\n"
            ));
        }
        let scene = parse_scene(&entities);
        assert!(scene.paths.len() > MAX_FRAME_SEGMENTS);
        let camera = CadCamera::fit(scene.bounds.unwrap(), [64, 64]);
        let frame = render(&scene, &visibility(&scene), camera, 64);
        assert!(frame.partial);
    }

    #[test]
    fn text_entities_survive_the_scene_budget() {
        let mut entities = String::new();
        for index in 0..MAX_FRAME_LABELS + 50 {
            entities.push_str(&format!(
                "0\nTEXT\n8\n0\n10\n{index}.0\n20\n0.0\n40\n10.0\n1\nlabel\n"
            ));
        }
        let scene = parse_scene(&entities);
        assert!(scene.texts.len() > MAX_FRAME_LABELS);
    }

    #[test]
    fn unit_symbols_are_only_reported_when_declared() {
        assert_eq!(unit_symbol(0), None);
        assert_eq!(unit_symbol(4), Some("mm"));
        assert_eq!(unit_symbol(6), Some("m"));
        assert_eq!(unit_symbol(99), None);
    }

    #[test]
    fn grid_spacing_stays_readable_across_zoom_levels() {
        for zoom in [0.001f64, 0.1, 1.0, 50.0, 10_000.0] {
            let camera = CadCamera {
                center: [0.0, 0.0],
                zoom,
                fit_zoom: zoom,
            };
            let mut pixels = vec![0u8; 32 * 32 * 4];
            assert!(draw_grid(
                [32, 32],
                1.0,
                camera,
                &test_style(),
                &mut pixels,
                &AtomicBool::new(false)
            ));
            let lit = pixels
                .chunks_exact(4)
                .filter(|p| p[..3] != [0, 0, 0])
                .count();
            assert!(lit > 0, "zoom {zoom} must still draw a grid");
        }
    }
}

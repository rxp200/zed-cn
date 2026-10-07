use crate::{
    DocumentItem,
    model_mesh::{ModelMesh, Topology, Vertex, cross, dot, sub},
};
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, MouseButton, Pixels, Point,
    RenderImage, Task, Window, canvas, img,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use ui::prelude::*;
use util::ResultExt as _;

#[derive(Clone, Copy)]
struct Camera {
    yaw: f64,
    pitch: f64,
    zoom: f64,
    pan: [f64; 2],
    wireframe: bool,
    perspective: bool,
}
impl Default for Camera {
    fn default() -> Self {
        Self {
            yaw: 0.65,
            pitch: -0.4,
            zoom: 1.0,
            pan: [0.0; 2],
            wireframe: false,
            perspective: false,
        }
    }
}

pub struct ModelReader {
    focus: FocusHandle,
    mesh: Arc<ModelMesh>,
    file_size: u64,
    camera: Camera,
    topology: Option<Topology>,
    error: Option<SharedString>,
    image: Option<Arc<RenderImage>>,
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
        Self {
            focus: cx.focus_handle(),
            mesh: item
                .read(cx)
                .model
                .as_ref()
                .expect("parsed model item")
                .clone(),
            file_size: match item.read(cx).file.disk_state {
                language::DiskState::Present { size, .. } => size,
                _ => 0,
            },
            camera: Camera::default(),
            topology: None,
            error: None,
            image: None,
            dirty: true,
            dimensions: [800, 600],
            drag: None,
            cancel: Arc::new(AtomicBool::new(false)),
            render_task: None,
            inspect_task: None,
        }
    }

    fn request_render(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.dirty || self.render_task.is_some() {
            return;
        }
        self.dirty = false;
        let mesh = self.mesh.clone();
        let camera = self.camera;
        let dimensions = self.dimensions;
        let cancel = self.cancel.clone();
        let background =
            cx.background_spawn(async move { rasterize(&mesh, camera, dimensions, &cancel) });
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
}

impl Focusable for ModelReader {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ModelReader {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.request_render(window, cx);
        let dimensions = sub(self.mesh.maximum, self.mesh.minimum);
        let statistics = format!(
            "{}: {} · {}: {:.6} × {:.6} × {:.6} · {}: {:.6}",
            i18n::t!("29b88538d1c54511"),
            self.mesh.triangles.len(),
            i18n::t!("383bbb8a68cf2785"),
            dimensions[0],
            dimensions[1],
            dimensions[2],
            i18n::t!("07c63ccdc90143ed"),
            self.mesh.area
        );
        let entity = cx.entity().downgrade();
        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .bg(cx.theme().colors().editor_background)
            .child(
                h_flex()
                    .gap_2()
                    .p_2()
                    .flex_wrap()
                    .child(
                        Button::new("model-reset", i18n::t!("ce89ae822e8ba072")).on_click(
                            cx.listener(|reader, _, _, cx| {
                                reader.camera = Camera::default();
                                reader.dirty = true;
                                cx.notify();
                            }),
                        ),
                    )
                    .children(
                        [
                            (0.0, 0.0, "MODEL_FRONT"),
                            (std::f64::consts::PI, 0.0, "MODEL_BACK"),
                            (std::f64::consts::FRAC_PI_2, 0.0, "MODEL_RIGHT"),
                            (-std::f64::consts::FRAC_PI_2, 0.0, "MODEL_LEFT"),
                            (0.0, std::f64::consts::FRAC_PI_2, "MODEL_TOP"),
                            (0.0, -std::f64::consts::FRAC_PI_2, "MODEL_BOTTOM"),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(index, (yaw, pitch, key))| {
                            let label = match key {
                                "MODEL_FRONT" => i18n::t!("a617590202898821"),
                                "MODEL_BACK" => i18n::t!("76900f1bfd16c8d4"),
                                "MODEL_RIGHT" => i18n::t!("883361d5d682a157"),
                                "MODEL_LEFT" => i18n::t!("58eb9032e3bb83f0"),
                                "MODEL_TOP" => i18n::t!("d5cdfcf7ff75338f"),
                                _ => i18n::t!("479ad21ac9096236"),
                            };
                            Button::new(("model-view", index), label).on_click(cx.listener(
                                move |reader, _, _, cx| {
                                    reader.camera.yaw = yaw;
                                    reader.camera.pitch = pitch;
                                    reader.dirty = true;
                                    cx.notify();
                                },
                            ))
                        }),
                    )
                    .child(
                        Button::new("model-wireframe", i18n::t!("0b1d0f618cd0fdfe")).on_click(
                            cx.listener(|reader, _, _, cx| {
                                reader.camera.wireframe = !reader.camera.wireframe;
                                reader.dirty = true;
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        Button::new("model-projection", i18n::t!("0941aece13f7362b")).on_click(
                            cx.listener(|reader, _, _, cx| {
                                reader.camera.perspective = !reader.camera.perspective;
                                reader.dirty = true;
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        Button::new("model-inspect", i18n::t!("91d5290121a42adf"))
                            .disabled(self.inspect_task.is_some())
                            .on_click(cx.listener(|reader, _, _, cx| reader.inspect(cx))),
                    ),
            )
            .child(Label::new(statistics).size(LabelSize::Small))
            .child(
                Label::new(i18n::t!(
                    "fb42ded5c52293c8",
                    bytes = self.file_size,
                    minimum = format!("{:?}", self.mesh.minimum),
                    maximum = format!("{:?}", self.mesh.maximum)
                ))
                .size(LabelSize::Small),
            )
            .child(
                Label::new(i18n::t!("0f5d9277ae8363bd"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .when_some(self.topology.as_ref(), |element, topology| {
                element.child(
                    Label::new(format!(
                        "{}: {} · {}: {} · {}: {} · {}: {} · {}: {}",
                        i18n::t!("46d800d5ba4df2a2"),
                        topology.vertices,
                        i18n::t!("0abb54d180cded75"),
                        topology.boundary_edges,
                        i18n::t!("830b893eee319bc7"),
                        topology.non_manifold_edges,
                        i18n::t!("c074951d4625d1c1"),
                        topology.inconsistent_edges,
                        i18n::t!("d44d74055685e9c8"),
                        self.mesh.degenerate_faces
                    ))
                    .size(LabelSize::Small),
                )
            })
            .when(
                self.topology.as_ref().is_some_and(|topology| {
                    topology.boundary_edges == 0
                        && topology.non_manifold_edges == 0
                        && topology.inconsistent_edges == 0
                        && self.mesh.degenerate_faces == 0
                }),
                |element| {
                    element.child(
                        Label::new(i18n::t!(
                            "924033ac926889c6",
                            volume = self.mesh.signed_volume.abs()
                        ))
                        .size(LabelSize::Small),
                    )
                },
            )
            .when_some(self.error.clone(), |element, error| {
                element.child(Label::new(error).color(Color::Error))
            })
            .child(
                div()
                    .id("model-viewport")
                    .relative()
                    .flex_1()
                    .w_full()
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
                    .on_scroll_wheel(cx.listener(
                        |reader, event: &gpui::ScrollWheelEvent, _, cx| {
                            reader.camera.zoom = (reader.camera.zoom
                                * (f64::from(event.delta.pixel_delta(gpui::px(20.0)).y) * 0.002)
                                    .exp())
                            .clamp(0.05, 20.0);
                            reader.dirty = true;
                            cx.notify();
                        },
                    ))
                    .when_some(self.image.clone(), |element, image| {
                        element.child(img(image).size_full().object_fit(gpui::ObjectFit::Contain))
                    })
                    .child(
                        canvas(
                            move |bounds, _, cx| {
                                entity
                                    .update(cx, |reader, cx| {
                                        let width =
                                            f32::from(bounds.size.width).clamp(1.0, 1280.0) as u32;
                                        let height =
                                            f32::from(bounds.size.height).clamp(1.0, 960.0) as u32;
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
                    ),
            )
    }
}

fn rasterize(
    mesh: &ModelMesh,
    camera: Camera,
    dimensions: [u32; 2],
    cancel: &AtomicBool,
) -> Option<Arc<RenderImage>> {
    let [width, height] = dimensions;
    let length = width as usize * height as usize;
    let mut pixels = vec![0u8; length * 4];
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.copy_from_slice(&[32, 28, 25, 255]);
    }
    let mut depth = vec![f64::INFINITY; length];
    let center: Vertex =
        std::array::from_fn(|axis| mesh.minimum[axis] * 0.5 + mesh.maximum[axis] * 0.5);
    let radius = sub(mesh.maximum, mesh.minimum)
        .into_iter()
        .fold(0.0f64, f64::max)
        .max(1e-20);
    let rotate = |vertex: Vertex| {
        let vertex = sub(vertex, center).map(|value| value / radius);
        let x = vertex[0] * camera.yaw.cos() + vertex[2] * camera.yaw.sin();
        let z = -vertex[0] * camera.yaw.sin() + vertex[2] * camera.yaw.cos();
        [
            x,
            vertex[1] * camera.pitch.cos() - z * camera.pitch.sin(),
            vertex[1] * camera.pitch.sin() + z * camera.pitch.cos(),
        ]
    };
    let scale = f64::from(width.min(height)) * 0.8 * camera.zoom;
    for (index, triangle) in mesh.triangles.iter().enumerate() {
        if index % 256 == 0 && cancel.load(Ordering::Relaxed) {
            return None;
        }
        let rotated = triangle.map(rotate);
        let normal = cross(sub(rotated[1], rotated[0]), sub(rotated[2], rotated[0]));
        let intensity =
            (dot(normal, [0.3, 0.5, 1.0]).abs() / dot(normal, normal).sqrt().max(1e-20)).min(1.0);
        let projected = rotated.map(|vertex| {
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
        });
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
                let color = if camera.wireframe && line {
                    [240, 220, 170, 255]
                } else if camera.wireframe {
                    [32, 28, 25, 255]
                } else {
                    [
                        (100.0 + intensity * 130.0) as u8,
                        (70.0 + intensity * 100.0) as u8,
                        (45.0 + intensity * 60.0) as u8,
                        255,
                    ]
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
fn edge(first: Vertex, second: Vertex, point: Vertex) -> f64 {
    (point[0] - first[0]) * (second[1] - first[1]) - (point[1] - first[1]) * (second[0] - first[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn render_is_bounded_and_cancellable() {
        let mesh = ModelMesh::parse("obj", b"v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3").expect("mesh");
        let image =
            rasterize(&mesh, Camera::default(), [64, 64], &AtomicBool::new(false)).expect("image");
        assert_eq!(image.as_bytes(0).expect("pixels").len(), 64 * 64 * 4);
        assert!(rasterize(&mesh, Camera::default(), [64, 64], &AtomicBool::new(true)).is_none());
    }
}

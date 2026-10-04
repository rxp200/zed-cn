use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, mpsc},
};

use futures::StreamExt as _;
use gpui::{
    AnyElement, App, Bounds, Context, DispatchPhase, Element, ElementId, Entity, EventEmitter,
    FocusHandle, Focusable, GlobalElementId, InspectorElementId, InteractiveElement, IntoElement,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels,
    Point, RenderImage, ScrollDelta, ScrollWheelEvent, SharedString, Style, Styled, Task, Window,
    actions, div, img, point, px, relative, size,
};
use hayro::hayro_syntax::Pdf;
use ui::prelude::*;
use util::ResultExt as _;

use crate::DocumentItem;

actions!(
    pdf_reader,
    [
        /// Zoom in the document.
        ZoomIn,
        /// Zoom out the document.
        ZoomOut,
        /// Fit the page width to the viewport.
        FitToWidth,
        /// Zoom to actual size (96 DPI).
        ZoomToActualSize,
        /// Go to the next page.
        NextPage,
        /// Go to the previous page.
        PreviousPage,
        /// Scroll one page down.
        ScrollPageDown,
        /// Scroll one page up.
        ScrollPageUp,
        /// Scroll down by one line.
        ScrollDown,
        /// Scroll up by one line.
        ScrollUp,
        /// Scroll to the start of the document.
        ScrollToStart,
        /// Scroll to the end of the document.
        ScrollToEnd
    ]
);

/// 1.0 zoom renders the document at 96 DPI.
const BASE_DPI_SCALE: f32 = 96.0 / 72.0;
const MIN_ZOOM: f32 = 0.25;
const MAX_ZOOM: f32 = 8.0;
const ZOOM_STEP: f32 = 1.25;
const PAGE_GAP: f32 = 12.0;
const SCROLL_LINE: f32 = 48.0;
/// Pages adjacent to the visible range that are rendered in advance.
const PREFETCH_PAGES: usize = 1;
/// Soft cap on the total pixel data kept in the page cache.
const CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;
/// Soft cap on the number of pages kept in the page cache.
const CACHE_MAX_PAGES: usize = 12;
/// Pages are re-rendered when the scale changed by more than this factor.
const RENDER_SCALE_TOLERANCE: f32 = 1.1;
/// Maximum pixels a single rendered page may occupy (limits peak memory).
const MAX_PAGE_PIXELS: f32 = 24_000_000.0;
/// Scrollbar width.
const SCROLLBAR_WIDTH: f32 = 6.0;

struct CachedPage {
    image: Arc<RenderImage>,
    scale: f32,
    bytes: usize,
    last_used: u64,
}

struct PdfDocumentInfo {
    page_sizes: Vec<(f32, f32)>,
}

struct PdfRequest {
    page: usize,
    scale: f32,
    generation: u64,
}

enum PdfResponse {
    Initialized(Result<PdfDocumentInfo, SharedString>),
    Rendered {
        page: usize,
        generation: u64,
        scale: f32,
        image: Arc<RenderImage>,
    },
}

struct ScrollbarDrag {
    start_mouse_y: Pixels,
    start_scroll_offset: Pixels,
}

pub struct PdfReader {
    item: Entity<DocumentItem>,
    project: Entity<project::Project>,
    focus_handle: FocusHandle,
    page_sizes: Option<Vec<(f32, f32)>>,
    error: Option<SharedString>,
    zoom: f32,
    did_initial_fit: bool,
    scroll_offset: Pixels,
    viewport_height: Pixels,
    container_width: Pixels,
    container_origin: Point<Pixels>,
    scale_factor: f32,
    page_offsets: Vec<f32>,
    page_offsets_zoom: f32,
    total_height: Pixels,
    cache: HashMap<usize, CachedPage>,
    cache_bytes: usize,
    cache_clock: u64,
    in_flight: HashSet<usize>,
    generation: u64,
    current_page: usize,
    request_tx: mpsc::Sender<PdfRequest>,
    scrollbar_drag: Option<ScrollbarDrag>,
    _response_task: Task<()>,
}

impl PdfReader {
    pub fn new(
        item: Entity<DocumentItem>,
        project: Entity<project::Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (request_tx, response_rx) = spawn_render_worker(item.read(cx).contents.clone());

        let response_task = cx.spawn(async move |this, cx| {
            let mut response_rx = response_rx;
            while let Some(response) = response_rx.next().await {
                if this
                    .update(cx, |reader, cx| reader.handle_response(response, cx))
                    .is_err()
                {
                    break;
                }
            }
        });

        Self {
            item,
            project,
            focus_handle: cx.focus_handle(),
            page_sizes: None,
            error: None,
            zoom: 1.0,
            did_initial_fit: false,
            scroll_offset: px(0.),
            viewport_height: px(0.),
            container_width: px(0.),
            container_origin: point(px(0.), px(0.)),
            scale_factor: 1.0,
            page_offsets: Vec::new(),
            page_offsets_zoom: 0.0,
            total_height: px(0.),
            cache: HashMap::new(),
            cache_bytes: 0,
            cache_clock: 0,
            in_flight: HashSet::new(),
            generation: 0,
            current_page: 0,
            request_tx,
            scrollbar_drag: None,
            _response_task: response_task,
        }
    }

    fn handle_response(&mut self, response: PdfResponse, cx: &mut Context<Self>) {
        match response {
            PdfResponse::Initialized(Ok(info)) => {
                self.page_sizes = Some(info.page_sizes);
                cx.notify();
            }
            PdfResponse::Initialized(Err(error)) => {
                self.error = Some(error);
                cx.notify();
            }
            PdfResponse::Rendered {
                page,
                generation,
                scale,
                image,
            } => {
                self.in_flight.remove(&page);
                if generation != self.generation {
                    return;
                }
                let bytes = image
                    .as_bytes(0)
                    .map(|bytes| bytes.len())
                    .unwrap_or_default();
                self.cache_clock += 1;
                self.cache.insert(
                    page,
                    CachedPage {
                        image,
                        scale,
                        bytes,
                        last_used: self.cache_clock,
                    },
                );
                self.cache_bytes += bytes;
                self.evict_cache();
                cx.notify();
            }
        }
    }

    fn render_scale(&self) -> f32 {
        self.zoom * BASE_DPI_SCALE * self.scale_factor
    }

    fn page_count(&self) -> usize {
        self.page_sizes.as_ref().map_or(0, Vec::len)
    }

    /// Recomputes cumulative page offsets for the current zoom level if stale.
    fn layout_pages(&mut self) {
        let Some(page_sizes) = &self.page_sizes else {
            return;
        };
        if self.page_offsets_zoom == self.zoom && self.page_offsets.len() == page_sizes.len() {
            return;
        }
        self.page_offsets.clear();
        let mut y = PAGE_GAP;
        for (_, height) in page_sizes {
            self.page_offsets.push(y);
            y += height * self.zoom + PAGE_GAP;
        }
        self.total_height = px(y);
        self.page_offsets_zoom = self.zoom;
    }

    fn clamp_scroll(&mut self) {
        let max_offset = (self.total_height - self.viewport_height).max(px(0.));
        self.scroll_offset = self.scroll_offset.clamp(px(0.), max_offset);
    }

    fn scroll_to(&mut self, offset: Pixels, cx: &mut Context<Self>) {
        self.scroll_offset = offset;
        self.clamp_scroll();
        cx.notify();
    }

    fn scroll_by(&mut self, delta: Pixels, cx: &mut Context<Self>) {
        self.scroll_to(self.scroll_offset + delta, cx);
    }

    fn scroll_to_page(&mut self, page: usize, cx: &mut Context<Self>) {
        if self.page_offsets.is_empty() {
            return;
        }
        let page = page.min(self.page_offsets.len().saturating_sub(1));
        self.scroll_to(px(self.page_offsets[page] - PAGE_GAP), cx);
    }

    fn set_zoom(&mut self, zoom: f32, cx: &mut Context<Self>) {
        let new_zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        if (self.zoom - new_zoom).abs() < f32::EPSILON {
            return;
        }
        // Keep the content around the current scroll position stable.
        let focus = self.scroll_offset + self.viewport_height / 2.0;
        let ratio = if self.total_height > px(0.) {
            f32::from(focus) / f32::from(self.total_height)
        } else {
            0.0
        };
        self.zoom = new_zoom;
        self.generation += 1;
        self.layout_pages();
        self.scroll_offset =
            px(f32::from(self.total_height) * ratio - f32::from(self.viewport_height) / 2.0);
        self.clamp_scroll();
        cx.notify();
    }

    pub fn fit_to_width(&mut self, cx: &mut Context<Self>) {
        let Some(page_sizes) = &self.page_sizes else {
            return;
        };
        let Some((width, _)) = page_sizes.first() else {
            return;
        };
        let available = f32::from(self.container_width) - 2.0 * PAGE_GAP;
        if available > 0.0 && *width > 0.0 {
            self.set_zoom(available / width, cx);
        }
    }

    fn visible_range(&self) -> std::ops::Range<usize> {
        let count = self.page_count();
        if count == 0 || self.page_offsets.len() != count {
            return 0..0;
        }
        let top = f32::from(self.scroll_offset);
        let bottom = top + f32::from(self.viewport_height);
        let mut start = count;
        let mut end = count;
        for (index, _) in self.page_offsets.iter().enumerate() {
            let page_top = self.page_offsets[index];
            let page_height =
                self.page_sizes.as_ref().map(|s| s[index].1).unwrap_or(0.0) * self.zoom;
            let page_bottom = page_top + page_height;
            if page_bottom >= top && start == count {
                start = index;
            }
            if page_top > bottom {
                end = index;
                break;
            }
        }
        if start == count {
            start = count.saturating_sub(1);
        }
        start..end.max(start + 1).min(count)
    }

    /// Requests background rendering for all pages in and around the visible
    /// range whose cached image is missing or at a stale scale.
    fn ensure_pages_rendered(&mut self) {
        let Some(page_sizes) = &self.page_sizes else {
            return;
        };
        let visible = self.visible_range();
        if visible.is_empty() {
            return;
        }
        let render_scale = self.render_scale();
        let start = visible.start.saturating_sub(PREFETCH_PAGES);
        let end = (visible.end + PREFETCH_PAGES).min(page_sizes.len());
        self.cache_clock += 1;
        for page in visible.start..visible.end {
            if let Some(cached) = self.cache.get_mut(&page) {
                cached.last_used = self.cache_clock;
            }
        }
        for page in start..end {
            let scale_is_fresh = self.cache.get(&page).is_some_and(|cached| {
                let ratio = cached.scale / render_scale;
                (RENDER_SCALE_TOLERANCE.recip()..=RENDER_SCALE_TOLERANCE).contains(&ratio)
            });
            if scale_is_fresh || self.in_flight.contains(&page) {
                continue;
            }
            let (width, height) = page_sizes[page];
            let scale = limit_render_scale(render_scale, width, height);
            self.in_flight.insert(page);
            if self
                .request_tx
                .send(PdfRequest {
                    page,
                    scale,
                    generation: self.generation,
                })
                .is_err()
            {
                self.in_flight.remove(&page);
            }
        }
        self.evict_cache();
    }

    fn evict_cache(&mut self) {
        let visible = self.visible_range();
        let prefetch_start = visible.start.saturating_sub(PREFETCH_PAGES);
        let prefetch_end = visible.end + PREFETCH_PAGES;
        while (self.cache_bytes > CACHE_MAX_BYTES || self.cache.len() > CACHE_MAX_PAGES)
            && self.cache.len() > visible.len()
        {
            let victim = self
                .cache
                .iter()
                .filter(|(page, _)| !(prefetch_start..prefetch_end).contains(*page))
                .min_by_key(|(_, cached)| cached.last_used)
                .map(|(page, _)| *page);
            let Some(victim) = victim else {
                break;
            };
            if let Some(removed) = self.cache.remove(&victim) {
                self.cache_bytes = self.cache_bytes.saturating_sub(removed.bytes);
            }
        }
    }

    fn update_current_page(&mut self) {
        let count = self.page_count();
        if count == 0 {
            return;
        }
        let center = f32::from(self.scroll_offset) + f32::from(self.viewport_height) / 2.0;
        let mut current = 0;
        for (index, offset) in self.page_offsets.iter().enumerate() {
            if *offset <= center {
                current = index;
            } else {
                break;
            }
        }
        self.current_page = current.min(count - 1);
    }

    pub fn current_page(&self) -> usize {
        self.current_page
    }

    pub fn total_pages(&self) -> usize {
        self.page_count()
    }

    pub fn zoom_level(&self) -> f32 {
        self.zoom
    }

    pub fn zoom_in(&mut self, cx: &mut Context<Self>) {
        self.set_zoom(self.zoom * ZOOM_STEP, cx);
    }

    pub fn zoom_out(&mut self, cx: &mut Context<Self>) {
        self.set_zoom(self.zoom / ZOOM_STEP, cx);
    }

    pub fn next_page(&mut self, cx: &mut Context<Self>) {
        self.scroll_to_page(self.current_page + 1, cx);
    }

    pub fn previous_page(&mut self, cx: &mut Context<Self>) {
        self.scroll_to_page(self.current_page.saturating_sub(1), cx);
    }

    fn handle_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.modifiers.control || event.modifiers.platform {
            let delta: f32 = match event.delta {
                ScrollDelta::Pixels(pixels) => pixels.y.into(),
                ScrollDelta::Lines(lines) => lines.y * SCROLL_LINE,
            };
            let factor = if delta > 0.0 {
                1.0 + delta.abs() * 0.01
            } else {
                1.0 / (1.0 + delta.abs() * 0.01)
            };
            self.scale_factor = window.scale_factor();
            self.set_zoom(self.zoom * factor, cx);
        } else {
            let delta = match event.delta {
                ScrollDelta::Pixels(pixels) => pixels.y,
                ScrollDelta::Lines(lines) => px(lines.y * SCROLL_LINE),
            };
            self.scroll_by(-delta, cx);
        }
    }

    fn handle_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left {
            return;
        }
        if let Some(thumb) = self.scrollbar_thumb() {
            let relative_position = event.position - self.container_origin;
            if relative_position.x >= thumb.x_start
                && relative_position.y >= thumb.thumb_top
                && relative_position.y <= thumb.thumb_bottom
            {
                self.scrollbar_drag = Some(ScrollbarDrag {
                    start_mouse_y: event.position.y,
                    start_scroll_offset: self.scroll_offset,
                });
                cx.notify();
            } else if relative_position.x >= thumb.x_start
                && relative_position.y >= px(0.)
                && relative_position.y <= self.viewport_height
            {
                // Click on the scrollbar track: jump one viewport.
                let direction = if relative_position.y < thumb.thumb_top {
                    -1.0
                } else {
                    1.0
                };
                self.scroll_by(self.viewport_height * direction, cx);
            }
        }
    }

    fn scrollbar_thumb(&self) -> Option<ScrollbarThumb> {
        if self.total_height <= self.viewport_height || self.viewport_height <= px(0.) {
            return None;
        }
        let track_height = f32::from(self.viewport_height);
        let ratio = track_height / f32::from(self.total_height);
        let thumb_height = (track_height * ratio).max(24.0);
        let max_offset = f32::from(self.total_height) - track_height;
        let progress = if max_offset > 0.0 {
            f32::from(self.scroll_offset) / max_offset
        } else {
            0.0
        };
        let thumb_top = (track_height - thumb_height) * progress;
        Some(ScrollbarThumb {
            thumb_top: px(thumb_top),
            thumb_bottom: px(thumb_top + thumb_height),
            thumb_height: px(thumb_height),
            x_start: self.container_width - px(SCROLLBAR_WIDTH + 8.0),
        })
    }
}

struct ScrollbarThumb {
    thumb_top: Pixels,
    thumb_bottom: Pixels,
    thumb_height: Pixels,
    x_start: Pixels,
}

impl EventEmitter<()> for PdfReader {}

impl Focusable for PdfReader {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PdfReader {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(error) = self.error.clone() {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(cx.theme().colors().editor_background)
                .child(Label::new(error).color(Color::Error))
                .into_any_element();
        }

        if self.page_sizes.is_none() {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(cx.theme().colors().editor_background)
                .child(Label::new(i18n::t!("d71a0adaa59e490a")).color(Color::Muted))
                .into_any_element();
        }

        div()
            .track_focus(&self.focus_handle(cx))
            .key_context("PdfReader")
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.zoom_in(cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom_out(cx)))
            .on_action(cx.listener(|this, _: &FitToWidth, _, cx| this.fit_to_width(cx)))
            .on_action(cx.listener(|this, _: &ZoomToActualSize, _, cx| {
                this.set_zoom(1.0, cx);
            }))
            .on_action(cx.listener(|this, _: &NextPage, _, cx| this.next_page(cx)))
            .on_action(cx.listener(|this, _: &PreviousPage, _, cx| this.previous_page(cx)))
            .on_action(cx.listener(|this, _: &ScrollPageDown, _, cx| {
                this.scroll_by(this.viewport_height * 0.9, cx);
            }))
            .on_action(cx.listener(|this, _: &ScrollPageUp, _, cx| {
                this.scroll_by(-this.viewport_height * 0.9, cx);
            }))
            .on_action(cx.listener(|this, _: &ScrollDown, _, cx| {
                this.scroll_by(px(SCROLL_LINE), cx);
            }))
            .on_action(cx.listener(|this, _: &ScrollUp, _, cx| {
                this.scroll_by(px(-SCROLL_LINE), cx);
            }))
            .on_action(cx.listener(|this, _: &ScrollToStart, _, cx| {
                this.scroll_to(px(0.), cx);
            }))
            .on_action(cx.listener(|this, _: &ScrollToEnd, _, cx| {
                this.scroll_to(this.total_height, cx);
            }))
            .on_action(cx.listener(|this, _: &editor::RevealInFileManager, _, cx| {
                let path = this.item.read(cx).abs_path(cx);
                if let Some(path) = path {
                    this.project
                        .update(cx, |project, cx| project.reveal_path(&path, cx));
                }
            }))
            .size_full()
            .relative()
            .bg(cx.theme().colors().panel_background)
            .child(
                div()
                    .id("pdf-scroll-container")
                    .size_full()
                    .overflow_hidden()
                    .on_scroll_wheel(cx.listener(Self::handle_scroll_wheel))
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::handle_mouse_down))
                    .child(PdfPagesElement::new(cx.entity())),
            )
            .into_any_element()
    }
}

struct PageFrame {
    origin: Point<Pixels>,
    size: gpui::Size<Pixels>,
    image: Option<Arc<RenderImage>>,
}

struct PdfPagesElement {
    reader: Entity<PdfReader>,
}

impl PdfPagesElement {
    fn new(reader: Entity<PdfReader>) -> Self {
        Self { reader }
    }
}

impl IntoElement for PdfPagesElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for PdfPagesElement {
    type RequestLayoutState = ();
    type PrepaintState = Vec<(AnyElement, Point<Pixels>)>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        (
            window.request_layout(
                Style {
                    size: size(relative(1.).into(), relative(1.).into()),
                    ..Default::default()
                },
                [],
                cx,
            ),
            (),
        )
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let scale_factor = window.scale_factor();
        let frames = self.reader.update(cx, |reader, cx| {
            reader.scale_factor = scale_factor;
            reader.container_width = bounds.size.width;
            reader.container_origin = bounds.origin;
            reader.viewport_height = bounds.size.height;
            if !reader.did_initial_fit && reader.page_sizes.is_some() {
                reader.did_initial_fit = true;
                reader.fit_to_width(cx);
            }
            reader.layout_pages();
            reader.clamp_scroll();
            reader.update_current_page();
            reader.ensure_pages_rendered();
            reader.visible_frames()
        });

        let mut children = Vec::with_capacity(frames.len());
        for frame in frames {
            let origin = point(
                bounds.origin.x + frame.origin.x,
                bounds.origin.y + frame.origin.y,
            );
            let mut page = div()
                .w(frame.size.width)
                .h(frame.size.height)
                .bg(gpui::white())
                .border_1()
                .border_color(cx.theme().colors().border);
            page = match frame.image {
                Some(image) => page.child(img(image).size_full()),
                None => page.child(
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(Label::new(i18n::t!("839d471683cbecfb")).color(Color::Muted)),
                ),
            };
            let mut element = page.into_any_element();
            element.prepaint_as_root(
                origin,
                frame.size.map(gpui::AvailableSpace::Definite),
                window,
                cx,
            );
            children.push((element, origin));
        }
        children
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        for (mut element, _) in prepaint.drain(..) {
            element.paint(window, cx);
        }

        let reader = self.reader.read(cx);
        if let Some(thumb) = reader.scrollbar_thumb() {
            let color = cx.theme().colors().scrollbar_thumb_background;
            let origin = point(
                bounds.origin.x + bounds.size.width - px(SCROLLBAR_WIDTH + 4.0),
                bounds.origin.y + thumb.thumb_top,
            );
            let thumb_bounds = Bounds {
                origin,
                size: size(px(SCROLLBAR_WIDTH), thumb.thumb_height),
            };
            window.paint_quad(gpui::fill(thumb_bounds, color).corner_radii(px(3.0)));
        }

        if self.reader.read(cx).scrollbar_drag.is_some() {
            let reader = self.reader.downgrade();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, _window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                if let Some(reader) = reader.upgrade() {
                    reader.update(cx, |reader, cx| {
                        if let Some(drag) = &reader.scrollbar_drag {
                            let max_offset = (f32::from(reader.total_height)
                                - f32::from(reader.viewport_height))
                            .max(1.0);
                            let track = (f32::from(reader.viewport_height)
                                - thumb_height_placeholder(reader))
                            .max(1.0);
                            let delta = event.position.y - drag.start_mouse_y;
                            let new_offset = drag.start_scroll_offset
                                + px(f32::from(delta) * max_offset / track);
                            reader.scroll_to(new_offset, cx);
                        }
                    });
                }
            });
            let reader = self.reader.downgrade();
            window.on_mouse_event(move |_event: &MouseUpEvent, phase, _window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                if let Some(reader) = reader.upgrade() {
                    reader.update(cx, |reader, _cx| {
                        reader.scrollbar_drag = None;
                    });
                }
            });
        }
    }
}

fn thumb_height_placeholder(reader: &PdfReader) -> f32 {
    reader
        .scrollbar_thumb()
        .map(|thumb| f32::from(thumb.thumb_height))
        .unwrap_or(24.0)
}

impl PdfReader {
    /// Computes the frames of the pages that intersect the viewport.
    fn visible_frames(&self) -> Vec<PageFrame> {
        let Some(page_sizes) = &self.page_sizes else {
            return Vec::new();
        };
        let visible = self.visible_range();
        let mut frames = Vec::with_capacity(visible.len());
        for index in visible {
            let (width, height) = page_sizes[index];
            let width = px(width * self.zoom);
            let height = px(height * self.zoom);
            let x = ((self.container_width - width) / 2.0).max(px(PAGE_GAP));
            let y = px(self.page_offsets[index]) - self.scroll_offset;
            let image = self.cache.get(&index).map(|cached| cached.image.clone());
            frames.push(PageFrame {
                origin: point(x, y),
                size: size(width, height),
                image,
            });
        }
        frames
    }
}

fn limit_render_scale(scale: f32, width: f32, height: f32) -> f32 {
    let pixels = width * scale * height * scale;
    if pixels > MAX_PAGE_PIXELS {
        scale * (MAX_PAGE_PIXELS / pixels).sqrt()
    } else {
        scale
    }
}

fn pixmap_to_render_image(mut pixmap: hayro::vello_cpu::Pixmap) -> Option<Arc<RenderImage>> {
    let width = pixmap.width() as u32;
    let height = pixmap.height() as u32;
    let bytes = pixmap.data_as_u8_slice_mut();
    // GPUI expects premultiplied BGRA, while vello produces premultiplied RGBA.
    for pixel in bytes.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(width, height, bytes.to_vec())?;
    Some(Arc::new(RenderImage::new(smallvec::SmallVec::from_const(
        [image::Frame::new(buffer)],
    ))))
}

fn spawn_render_worker(
    data: Arc<Vec<u8>>,
) -> (
    mpsc::Sender<PdfRequest>,
    futures::channel::mpsc::UnboundedReceiver<PdfResponse>,
) {
    let (request_tx, request_rx) = mpsc::channel::<PdfRequest>();
    let (response_tx, response_rx) = futures::channel::mpsc::unbounded::<PdfResponse>();
    std::thread::Builder::new()
        .name("document-viewer-pdf".into())
        .spawn(move || {
            run_render_worker(data, &request_rx, &response_tx);
        })
        .log_err();
    (request_tx, response_rx)
}

fn run_render_worker(
    data: Arc<Vec<u8>>,
    request_rx: &mpsc::Receiver<PdfRequest>,
    response_tx: &futures::channel::mpsc::UnboundedSender<PdfResponse>,
) {
    // `PdfData` 能直接接受 `Arc<Vec<u8>>`，而这里的 `data` 与
    // `DocumentItem.contents` 是同一个分配，因此不要复制成新的 `Vec<u8>`，
    // 否则整个文件会多驻留一份。
    let pdf = match Pdf::new(data) {
        Ok(pdf) => pdf,
        Err(error) => {
            let message = match error {
                hayro::hayro_syntax::LoadPdfError::Decryption(_) => {
                    SharedString::from(i18n::t!("ea7a8895915778e8").to_string())
                }
                hayro::hayro_syntax::LoadPdfError::Invalid => {
                    SharedString::from(i18n::t!("b1a6af4dee127f96").to_string())
                }
            };
            let _ = response_tx.unbounded_send(PdfResponse::Initialized(Err(message)));
            return;
        }
    };

    let page_sizes = pdf
        .pages()
        .iter()
        .map(|page| page.render_dimensions())
        .collect::<Vec<_>>();
    if response_tx
        .unbounded_send(PdfResponse::Initialized(Ok(PdfDocumentInfo { page_sizes })))
        .is_err()
    {
        return;
    }

    let interpreter_settings = hayro::hayro_interpret::InterpreterSettings::default();
    let render_cache = hayro::RenderCache::new();
    while let Ok(first) = request_rx.recv() {
        // Coalesce queued requests: only the newest request per page is rendered.
        let mut queue = std::collections::BTreeMap::new();
        let PdfRequest {
            page,
            scale,
            generation,
        } = first;
        queue.insert(page, (scale, generation));
        while let Ok(PdfRequest {
            page,
            scale,
            generation,
        }) = request_rx.try_recv()
        {
            queue.insert(page, (scale, generation));
        }
        for (page, (scale, generation)) in queue {
            if !render_and_send(
                &pdf,
                &render_cache,
                &interpreter_settings,
                response_tx,
                page,
                scale,
                generation,
            ) {
                return;
            }
        }
    }
}

fn render_and_send<'a>(
    pdf: &'a Pdf,
    render_cache: &hayro::RenderCache<'a>,
    interpreter_settings: &hayro::hayro_interpret::InterpreterSettings,
    response_tx: &futures::channel::mpsc::UnboundedSender<PdfResponse>,
    page: usize,
    scale: f32,
    generation: u64,
) -> bool {
    {
        let rendered = pdf.pages().get(page).map(|page| {
            let settings = hayro::RenderSettings {
                x_scale: scale,
                y_scale: scale,
                ..Default::default()
            };
            let pixmap = hayro::render(page, &render_cache, &interpreter_settings, &settings);
            pixmap_to_render_image(pixmap)
        });
        let Some(image) = rendered.flatten() else {
            return true;
        };
        response_tx
            .unbounded_send(PdfResponse::Rendered {
                page,
                generation,
                scale,
                image,
            })
            .is_ok()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Builds a minimal one-page PDF containing a filled rectangle, with a
    /// correct cross-reference table.
    pub(crate) fn minimal_pdf() -> Vec<u8> {
        let objects = [
            "<</Type/Catalog/Pages 2 0 R>>",
            "<</Type/Pages/Kids[3 0 R]/Count 1>>",
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 100]/Contents 4 0 R/Resources<<>>>>",
            "<</Length 26>>",
        ];
        let stream = "1 0 0 rg\n10 10 180 80 re\nf\n";
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
            pdf.extend_from_slice(object.as_bytes());
            if index == 3 {
                pdf.extend_from_slice(b"\nstream\n");
                pdf.extend_from_slice(stream.as_bytes());
                pdf.extend_from_slice(b"endstream\n");
            }
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref_offset = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 5\n{:010} 65535 f \n", 0).as_bytes());
        for offset in offsets {
            pdf.extend_from_slice(format!("{:010} 00000 n \n", offset).as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<</Size 5/Root 1 0 R>>\nstartxref\n{}\n%%EOF\n",
                xref_offset
            )
            .as_bytes(),
        );
        pdf
    }

    #[test]
    fn parses_and_renders_minimal_pdf() {
        let data = minimal_pdf();
        let pdf = Pdf::new(data).expect("minimal pdf should parse");
        assert_eq!(pdf.pages().len(), 1);
        let (width, height) = pdf.pages()[0].render_dimensions();
        assert_eq!((width, height), (200.0, 100.0));

        let render_cache = hayro::RenderCache::new();
        let settings = hayro::RenderSettings {
            x_scale: 1.0,
            y_scale: 1.0,
            ..Default::default()
        };
        let pixmap = hayro::render(
            &pdf.pages()[0],
            &render_cache,
            &hayro::hayro_interpret::InterpreterSettings::default(),
            &settings,
        );
        assert_eq!((pixmap.width(), pixmap.height()), (200, 100));
        let image = pixmap_to_render_image(pixmap).expect("pixmap should convert");
        assert_eq!(
            image.size(0),
            size(gpui::DevicePixels(200), gpui::DevicePixels(100))
        );
    }

    #[test]
    fn limits_render_scale_for_huge_pages() {
        // A0 poster at high zoom would exceed the pixel budget.
        let limited = limit_render_scale(10.0, 4000.0, 3000.0);
        let pixels = 4000.0 * limited * (3000.0 * limited);
        assert!(pixels <= MAX_PAGE_PIXELS * 1.001);
        // Small pages are left alone.
        assert_eq!(limit_render_scale(2.0, 600.0, 800.0), 2.0);
    }

    /// 源文件字节必须与 `DocumentItem.contents` 共享同一块分配；一旦退回
    /// `to_vec()` 或 `Arc<[u8]>`，整个 PDF 会多驻留一份，这里用引用计数把
    /// 这件事钉住。
    #[test]
    fn parses_from_a_shared_buffer_without_copying_it() {
        let bytes: Arc<Vec<u8>> = Arc::new(minimal_pdf());
        let shared = bytes.clone();
        assert_eq!(Arc::strong_count(&shared), 2);
        let pdf = Pdf::new(bytes).expect("minimal pdf should parse");
        assert!(
            Arc::strong_count(&shared) >= 2,
            "Pdf 应该持有调用方的 Arc，而不是复制成 Vec<u8>"
        );
        assert_eq!(pdf.pages().len(), 1);
    }

    /// 页位图缓存的内存预算；调高它必须是有意识的决定，而不是顺手改数字。
    #[test]
    fn page_cache_budget_stays_bounded() {
        const MIB: usize = 1024 * 1024;
        assert!(CACHE_MAX_BYTES <= 64 * MIB);
        assert!(CACHE_MAX_PAGES <= 12);
    }
}

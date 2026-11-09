//! Image cache for the markdown preview.
//!
//! The preview keeps decoded images and their atlas textures in a cache it
//! owns, so that switching documents or tabs releases them instead of holding
//! them for the lifetime of the process. Images that a frame no longer draws
//! stay decoded for [`IDLE_RETENTION`] and are then evicted; the budget only
//! limits images that are not drawn in the current frame, so a visible image is
//! never dropped mid-presentation.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, Entity, ImageCache, ImageCacheError, ImageCacheItem, RenderImage, Resource, WeakEntity,
    Window, hash, prelude::*,
};
use project::{Project, ProjectPath, image_store};

/// How long a decoded image survives after the preview stops drawing it.
pub const IDLE_RETENTION: Duration = Duration::from_secs(60);

/// Upper bound for decoded images the preview keeps while they are not drawn.
pub const DEFAULT_BUDGET_BYTES: usize = 256 * 1024 * 1024;

/// Identifies a cached image independently of its decoded pixels.
#[derive(Clone, PartialEq, Eq, Hash)]
enum ImageKey {
    Resource(u64),
    Project {
        project: WeakEntity<Project>,
        path: ProjectPath,
    },
}

struct Entry {
    item: ImageCacheItem,
    /// Decoded pixel bytes, or zero while the image is still loading.
    bytes: usize,
    /// Frame that last drew this image.
    used_generation: u64,
    /// Incremented every time the image is drawn, so stale eviction timers can
    /// tell whether the image was used again after they were scheduled.
    use_epoch: u64,
    /// Whether an idle eviction timer is already scheduled for this entry.
    evicting: bool,
}

/// A per-preview image cache that evicts images the preview no longer draws.
pub struct MarkdownImageCache {
    entries: HashMap<ImageKey, Entry>,
    total_bytes: usize,
    budget_bytes: usize,
    idle_retention: Duration,
    generation: u64,
    self_handle: WeakEntity<Self>,
}

impl MarkdownImageCache {
    pub fn new(cx: &mut App) -> Entity<Self> {
        let cache = cx.new(|cx| Self {
            entries: HashMap::default(),
            total_bytes: 0,
            budget_bytes: DEFAULT_BUDGET_BYTES,
            idle_retention: IDLE_RETENTION,
            generation: 0,
            self_handle: cx.weak_entity(),
        });
        cx.observe_release(&cache, |cache, cx| cache.clear(cx))
            .detach();
        cache
    }

    /// Starts a new frame. Images the previous frame did not draw begin their
    /// idle retention window here.
    pub fn begin_frame(&mut self, cx: &mut App) {
        self.generation += 1;
        let generation = self.generation;
        let idle_keys = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.used_generation + 1 < generation)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in idle_keys {
            self.schedule_idle_eviction(key, cx);
        }
        self.enforce_budget(cx);
    }

    /// Retires every image, for example because the preview was deactivated.
    /// Images that are drawn again before the retention window elapses are kept.
    pub fn retire_all(&mut self, cx: &mut App) {
        let keys = self.entries.keys().cloned().collect::<Vec<_>>();
        for key in keys {
            self.schedule_idle_eviction(key, cx);
        }
    }

    /// Drops every image, removing its atlas entries from all windows.
    pub fn clear(&mut self, cx: &mut App) {
        let keys = self.entries.keys().cloned().collect::<Vec<_>>();
        for key in keys {
            self.evict(&key, cx);
        }
    }

    /// Loads a project image, bypassing GPUI's global asset cache so that the
    /// preview can release it later.
    pub fn load_project_image(
        &mut self,
        project: WeakEntity<Project>,
        path: ProjectPath,
        window: &Window,
        cx: &mut App,
    ) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        let key = ImageKey::Project {
            project: project.clone(),
            path: path.clone(),
        };
        self.load_entry(key, window, cx, move |cx| {
            let load = image_store::load_project_image(project, path, cx);
            ImageCacheItem::from_load(load, cx)
        })
    }

    fn load_entry(
        &mut self,
        key: ImageKey,
        window: &Window,
        cx: &mut App,
        create: impl FnOnce(&mut App) -> ImageCacheItem,
    ) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        let generation = self.generation;
        let mut measured_bytes = None;
        let result = {
            let entry = self.entries.entry(key).or_insert_with(|| Entry {
                item: create(cx),
                bytes: 0,
                used_generation: generation,
                use_epoch: 0,
                evicting: false,
            });
            entry.used_generation = generation;
            entry.use_epoch += 1;
            entry.evicting = false;

            let result = entry.item.use_image(window);
            if entry.bytes == 0
                && let Some(Ok(image)) = &result
            {
                let bytes = decoded_image_bytes(image);
                entry.bytes = bytes;
                measured_bytes = Some(bytes);
            }
            result
        };

        if let Some(bytes) = measured_bytes {
            self.total_bytes += bytes;
        }
        self.enforce_budget(cx);
        result
    }

    /// Evicts images until the cache is within budget. Only images the current
    /// frame did not draw are candidates, so a visible image never disappears.
    fn enforce_budget(&mut self, cx: &mut App) {
        while self.total_bytes > self.budget_bytes {
            let candidate = self
                .entries
                .iter()
                .filter(|(_, entry)| !entry.is_displayed(self.generation))
                .min_by_key(|(_, entry)| (entry.used_generation, entry.use_epoch))
                .map(|(key, _)| key.clone());
            let Some(candidate) = candidate else {
                break;
            };
            self.evict(&candidate, cx);
        }
    }

    fn schedule_idle_eviction(&mut self, key: ImageKey, cx: &mut App) {
        let Some(entry) = self.entries.get_mut(&key) else {
            return;
        };
        if entry.evicting {
            return;
        }
        entry.evicting = true;
        let use_epoch = entry.use_epoch;
        let idle_retention = self.idle_retention;
        let cache = self.self_handle.clone();
        cx.spawn(async move |cx| {
            cx.background_executor().timer(idle_retention).await;
            if let Some(cache) = cache.upgrade() {
                cache.update(cx, |cache, cx| {
                    cache.evict_if_idle(&key, use_epoch, cx);
                });
            }
        })
        .detach();
    }

    /// Evicts an image if it has not been drawn since its eviction timer was
    /// scheduled.
    fn evict_if_idle(&mut self, key: &ImageKey, use_epoch: u64, cx: &mut App) {
        let is_idle = self
            .entries
            .get(key)
            .is_some_and(|entry| entry.evicting && entry.use_epoch == use_epoch);
        if is_idle {
            self.evict(key, cx);
        }
    }

    fn evict(&mut self, key: &ImageKey, cx: &mut App) {
        let Some(entry) = self.entries.remove(key) else {
            return;
        };
        self.total_bytes = self.total_bytes.saturating_sub(entry.bytes);
        if let Some(Ok(image)) = entry.item.get() {
            drop_image_later(image, cx);
        }
    }

    #[cfg(test)]
    fn contains(&self, key: &ImageKey) -> bool {
        self.entries.contains_key(key)
    }
}

impl Entry {
    /// Whether the current or the immediately preceding frame drew this image.
    /// A frame boundary must not make a visible image evictable.
    fn is_displayed(&self, generation: u64) -> bool {
        self.used_generation + 1 >= generation
    }
}

impl ImageCache for MarkdownImageCache {
    fn load(
        &mut self,
        resource: &Resource,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        self.load_entry(ImageKey::Resource(hash(resource)), window, cx, |cx| {
            ImageCacheItem::new(resource, cx)
        })
    }
}

/// Removes an image from every window's atlas after the current update.
///
/// Releases and evictions often run inside a window update, where that window is
/// temporarily removed from `App::windows` and `App::drop_image` would skip it;
/// deferring keeps the atlas entry from outliving the image.
fn drop_image_later(image: Arc<RenderImage>, cx: &mut App) {
    cx.defer(move |cx| cx.drop_image(image, None));
}

fn decoded_image_bytes(image: &RenderImage) -> usize {
    (0..image.frame_count())
        .map(|frame_index| {
            let size = image.size(frame_index);
            size.width.0.max(0) as usize * size.height.0.max(0) as usize * 4
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use fs::{FakeFs, Fs as _};
    use gpui::{
        App, Context, ImageSource, IntoElement, Render, TestAppContext, VisualTestContext, Window,
        div, img,
    };
    use project::Project;
    use serde_json::json;
    use settings::SettingsStore;
    use util::rel_path::rel_path;
    use util::test::TempTree;

    use super::*;

    const PNM_IMAGE: &str = "P3\n1 1\n255\n0 0 0\n";

    struct TestImageView {
        cache: Entity<MarkdownImageCache>,
        sources: Vec<ImageSource>,
    }

    impl Render for TestImageView {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.cache.update(cx, |cache, cx| cache.begin_frame(cx));
            div()
                .image_cache(self.cache.clone())
                .children(self.sources.iter().cloned().map(img))
        }
    }

    fn image_source(tree: &TempTree, name: &str) -> (ImageSource, ImageKey) {
        let path = tree.path().join(name);
        let resource = Resource::Path(Arc::from(path.as_path()));
        let key = ImageKey::Resource(hash(&resource));
        (ImageSource::Resource(resource), key)
    }

    fn draw_frame(cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
    }

    fn draw_loaded_frames(cx: &mut VisualTestContext) {
        draw_frame(cx);
        cx.run_until_parked();
        draw_frame(cx);
    }

    fn cached_image(
        cx: &VisualTestContext,
        cache: &Entity<MarkdownImageCache>,
        key: &ImageKey,
    ) -> Option<Arc<RenderImage>> {
        cx.read(|cx| {
            cache
                .read(cx)
                .entries
                .get(key)
                .and_then(|entry| entry.item.get())
                .and_then(|result| result.ok())
        })
    }

    fn cached(cx: &VisualTestContext, cache: &Entity<MarkdownImageCache>, key: &ImageKey) -> bool {
        cx.read(|cx| cache.read(cx).contains(key))
    }

    fn set_sources(
        cx: &mut VisualTestContext,
        view: &Entity<TestImageView>,
        sources: Vec<ImageSource>,
    ) {
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.sources = sources;
                cx.notify();
            });
        });
    }

    fn open_view(
        cx: &mut TestAppContext,
        sources: Vec<(ImageSource, ImageKey)>,
    ) -> (Entity<TestImageView>, Entity<MarkdownImageCache>, &mut VisualTestContext) {
        let (view, cx) = cx.add_window_view(|_, cx| TestImageView {
            cache: MarkdownImageCache::new(cx),
            sources: sources.iter().map(|(source, _)| source.clone()).collect(),
        });
        let cache = cx.read(|cx| view.read(cx).cache.clone());
        (view, cache, cx)
    }

    fn test_tree(names: &[&str]) -> TempTree {
        let mut files = serde_json::Map::default();
        for name in names {
            files.insert((*name).to_string(), json!(PNM_IMAGE));
        }
        TempTree::new(serde_json::Value::Object(files))
    }

    #[gpui::test]
    async fn evicts_an_image_the_preview_stops_drawing_after_retention(
        cx: &mut TestAppContext,
    ) {
        let tree = test_tree(&["first.ppm"]);
        let (source, key) = image_source(&tree, "first.ppm");
        let (view, cache, cx) = open_view(cx, vec![(source, key.clone())]);
        draw_loaded_frames(cx);

        let image = cached_image(cx, &cache, &key).expect("the rendered image should be decoded");
        assert!(
            cx.update(|window, _| window.has_image_atlas_entry(&image)),
            "a rendered image must be uploaded to the sprite atlas"
        );

        set_sources(cx, &view, Vec::new());
        draw_frame(cx);
        assert!(
            cached(cx, &cache, &key),
            "an image must not be evicted immediately after it stops being drawn"
        );
        draw_frame(cx);

        cx.background_executor.advance_clock(IDLE_RETENTION);
        cx.run_until_parked();

        assert!(
            !cached(cx, &cache, &key),
            "an image the preview stopped drawing must leave the cache after the retention window"
        );
        assert!(
            !cx.update(|window, _| window.has_image_atlas_entry(&image)),
            "evicting an image must remove its atlas entry"
        );
    }

    #[gpui::test]
    async fn keeps_an_image_drawn_again_within_retention(cx: &mut TestAppContext) {
        let tree = test_tree(&["again.ppm"]);
        let (source, key) = image_source(&tree, "again.ppm");
        let (view, cache, cx) = open_view(cx, vec![(source.clone(), key.clone())]);
        draw_loaded_frames(cx);
        assert!(cached(cx, &cache, &key));

        set_sources(cx, &view, Vec::new());
        draw_frame(cx);
        draw_frame(cx);

        set_sources(cx, &view, vec![source]);
        draw_frame(cx);
        cx.background_executor.advance_clock(IDLE_RETENTION);
        cx.run_until_parked();

        assert!(
            cached(cx, &cache, &key),
            "drawing an image again must keep it cached instead of reloading it"
        );
    }

    #[gpui::test]
    async fn evicts_images_the_current_frame_does_not_draw_when_over_budget(
        cx: &mut TestAppContext,
    ) {
        let tree = test_tree(&["first.ppm", "second.ppm"]);
        let (first_source, first) = image_source(&tree, "first.ppm");
        let (second_source, second) = image_source(&tree, "second.ppm");
        let (view, cache, cx) = open_view(
            cx,
            vec![
                (first_source, first.clone()),
                (second_source.clone(), second.clone()),
            ],
        );
        cx.update(|_, cx| {
            cache.update(cx, |cache, _| {
                cache.budget_bytes = 1;
            });
        });
        draw_loaded_frames(cx);
        assert!(cached(cx, &cache, &first));
        assert!(cached(cx, &cache, &second));

        set_sources(cx, &view, vec![second_source]);
        draw_frame(cx);
        draw_frame(cx);

        assert!(
            !cached(cx, &cache, &first),
            "the least recently drawn image must be evicted to stay within budget"
        );
        assert!(
            cached(cx, &cache, &second),
            "an image the current frame still draws must be kept"
        );
    }

    #[gpui::test]
    async fn retire_all_releases_images_after_retention(cx: &mut TestAppContext) {
        let tree = test_tree(&["deactivated.ppm"]);
        let (source, key) = image_source(&tree, "deactivated.ppm");
        let (_view, cache, cx) = open_view(cx, vec![(source, key.clone())]);
        draw_loaded_frames(cx);
        assert!(cached(cx, &cache, &key));

        cx.update(|_, cx| {
            cache.update(cx, |cache, cx| cache.retire_all(cx));
        });
        cx.background_executor.advance_clock(IDLE_RETENTION);
        cx.run_until_parked();

        assert!(
            !cached(cx, &cache, &key),
            "a deactivated preview must release its images after the retention window"
        );
    }

    #[gpui::test]
    async fn clear_drops_the_image_and_its_atlas_entry(cx: &mut TestAppContext) {
        let tree = test_tree(&["drawn.ppm"]);
        let (source, key) = image_source(&tree, "drawn.ppm");
        let (_view, cache, cx) = open_view(cx, vec![(source, key.clone())]);
        draw_loaded_frames(cx);
        let image = cached_image(cx, &cache, &key).expect("the rendered image should be decoded");
        assert!(cx.update(|window, _| window.has_image_atlas_entry(&image)));

        cx.update(|_, cx| {
            cache.update(cx, |cache, cx| cache.clear(cx));
        });

        assert!(!cached(cx, &cache, &key));
        assert!(
            !cx.update(|window, _| window.has_image_atlas_entry(&image)),
            "releasing a cached image must remove its atlas entry"
        );
    }

    #[gpui::test]
    async fn project_images_bypass_the_global_asset_cache(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let fs = FakeFs::new(cx.executor());
        fs.create_dir(Path::new("/project"))
            .await
            .expect("the test project root should be created");
        fs.insert_file("/project/image.ppm", PNM_IMAGE.as_bytes().to_vec())
            .await;
        let project = Project::test(fs, [Path::new("/project")], cx).await;
        let worktree_id = cx.update(|cx| {
            project
                .read(cx)
                .worktrees(cx)
                .next()
                .expect("the test project should contain a worktree")
                .read(cx)
                .id()
        });
        let project_path = ProjectPath {
            worktree_id,
            path: rel_path("image.ppm").into(),
        };
        let cache = cx.update(|cx| MarkdownImageCache::new(cx));
        let source = ImageSource::from({
            let cache = cache.clone();
            let project = project.clone();
            let project_path = project_path.clone();
            move |window: &mut Window, cx: &mut App| {
                cache.update(cx, |cache, cx| {
                    cache.load_project_image(project.downgrade(), project_path.clone(), window, cx)
                })
            }
        });
        let (_view, cx) = cx.add_window_view(|_, _| TestImageView {
            cache: cache.clone(),
            sources: vec![source],
        });
        let key = ImageKey::Project {
            project: project.downgrade(),
            path: project_path.clone(),
        };

        draw_frame(cx);
        cx.run_until_parked();
        draw_frame(cx);

        assert!(
            cached_image(cx, &cache, &key).is_some(),
            "the project image should be decoded"
        );
        assert!(
            !cx.read(|cx| image_store::is_project_image_cached(
                project.downgrade(),
                project_path,
                cx
            )),
            "a project image loaded by the preview must not stay in GPUI's global asset cache"
        );
    }
}

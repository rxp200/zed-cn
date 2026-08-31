//! Persistent on-disk cache for AI translations.
//!
//! Translations are stored in a single JSON file under Zed's data directory
//! and reused across sessions. The cache is bounded by a byte budget; when it
//! is full, entries are evicted by a weighted score (a GDSF-style policy) that
//! prefers recently-viewed, frequently-viewed and short entries, and evicts
//! entries that are old, rarely viewed and large.

use collections::HashMap;
use gpui::{AppContext as _, Context, SharedString, Task};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::hover_translation::TranslationService;

/// How long to wait after the last change before writing the cache file.
const SAVE_DEBOUNCE: Duration = Duration::from_secs(5);

/// Version of the on-disk cache format. Bump to invalidate old cache files.
const CACHE_FILE_VERSION: u32 = 1;

/// Approximate per-entry overhead (JSON keys, quotes, separators) added to the
/// byte accounting so the total stays close to the real file size.
const ENTRY_OVERHEAD: usize = 96;

/// Recency weight in the eviction score.
const RECENCY_WEIGHT: f64 = 1.0;
/// Frequency weight in the eviction score.
const FREQ_WEIGHT: f64 = 0.5;
/// Size-penalty weight in the eviction score.
const SIZE_WEIGHT: f64 = 2.0;
/// View counts above this cap are treated as equal, so an entry that was hot
/// long ago does not dominate the score forever (frequency aging).
const VIEW_COUNT_CAP: u32 = 20;
/// Entry byte size at which the size penalty saturates.
const MAX_ENTRY_BYTES: usize = 12_000;

/// One cached translation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TranslationCacheEntry {
    pub translation: SharedString,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub last_viewed_at: u64,
    #[serde(default)]
    pub view_count: u32,
}

impl TranslationCacheEntry {
    fn new(translation: SharedString, now: u64) -> Self {
        Self {
            translation,
            created_at: now,
            last_viewed_at: now,
            view_count: 1,
        }
    }
}

/// On-disk format of the translation cache.
#[derive(Serialize, Deserialize)]
struct TranslationCacheFile {
    version: u32,
    entries: HashMap<String, TranslationCacheEntry>,
}

/// The persistent translation cache: a JSON file loaded lazily and saved by a
/// debounced background writer. All state lives on the main thread; only the
/// file read/write happens in the background.
pub struct TranslationDiskCache {
    path: PathBuf,
    max_bytes: usize,
    enabled: bool,
    loaded: bool,
    /// One-time load task, started by the translation service on creation.
    load_task: Option<Task<()>>,
    entries: HashMap<String, TranslationCacheEntry>,
    total_bytes: usize,
    dirty: bool,
    /// Background save loop; `Some` while a save is pending.
    save_task: Option<Task<()>>,
}

/// Path of the on-disk translation cache file.
pub fn translation_cache_path() -> PathBuf {
    paths::data_dir().join("translation_cache.json")
}

impl TranslationDiskCache {
    pub fn new(path: PathBuf, max_bytes: usize, enabled: bool) -> Self {
        Self {
            path,
            max_bytes,
            enabled,
            loaded: false,
            load_task: None,
            entries: HashMap::default(),
            total_bytes: 0,
            dirty: false,
            save_task: None,
        }
    }

    /// Applies the current configuration, evicting entries if the budget
    /// shrank. Cheap enough to call on every translation.
    pub fn configure(&mut self, enabled: bool, max_bytes: usize) {
        self.enabled = enabled;
        self.max_bytes = max_bytes;
        if enabled {
            let before = self.entries.len();
            self.enforce_budget();
            if self.entries.len() < before {
                self.dirty = true;
            }
        }
    }

    /// Takes the one-time load task so the caller can await it before
    /// consulting the disk cache.
    pub fn take_load_task(&mut self) -> Option<Task<()>> {
        self.load_task.take()
    }

    /// Stores the one-time load task, started by the translation service on
    /// creation.
    pub fn set_load_task(&mut self, task: Task<()>) {
        self.load_task = Some(task);
    }

    /// Marks the cache as loaded even when there is nothing to load (e.g. the
    /// cache file does not exist yet).
    pub fn mark_loaded(&mut self) {
        self.loaded = true;
    }

    /// Parses the cache file contents and applies them. Corrupt or
    /// incompatible files are ignored (the cache starts fresh).
    pub fn load_from_bytes(&mut self, bytes: &[u8]) {
        match serde_json::from_slice::<TranslationCacheFile>(bytes) {
            Ok(file) if file.version == CACHE_FILE_VERSION => {
                for (key, entry) in file.entries {
                    if entry.translation.len() > self.max_bytes {
                        continue;
                    }
                    self.total_bytes = self.total_bytes.saturating_add(entry_bytes(&key, &entry));
                    self.entries.insert(key, entry);
                }
                self.enforce_budget();
            }
            _ => {}
        }
        self.loaded = true;
    }

    /// Returns the cached translation for `key`, if the cache is enabled,
    /// loaded and contains the key.
    pub fn get(&self, key: &str) -> Option<&TranslationCacheEntry> {
        if !self.enabled || !self.loaded {
            return None;
        }
        self.entries.get(key)
    }

    /// Stores a translation, evicting the least useful entries while the
    /// cache is over budget. Translations larger than the whole budget are
    /// not persisted (they stay in the in-memory cache only).
    pub fn store(&mut self, key: &str, translation: SharedString) {
        if !self.enabled {
            return;
        }
        if translation.len() > self.max_bytes {
            return;
        }
        let now = unix_seconds();
        if let Some(old) = self.entries.remove(key) {
            self.total_bytes = self.total_bytes.saturating_sub(entry_bytes(key, &old));
        }
        let entry = TranslationCacheEntry::new(translation, now);
        self.total_bytes = self.total_bytes.saturating_add(entry_bytes(key, &entry));
        self.entries.insert(key.to_string(), entry);
        self.enforce_budget();
        self.dirty = true;
    }

    /// Records a view of `key`, updating its recency and frequency for future
    /// eviction decisions.
    pub fn record_view(&mut self, key: &str) {
        if !self.enabled {
            return;
        }
        if let Some(entry) = self.entries.get_mut(key) {
            entry.last_viewed_at = unix_seconds();
            entry.view_count = entry.view_count.saturating_add(1);
            self.dirty = true;
        }
    }

    /// Schedules a debounced background save. Multiple changes within the
    /// debounce window are coalesced into a single write.
    pub fn schedule_save(&mut self, cx: &mut Context<TranslationService>) {
        if !self.enabled || self.save_task.is_some() {
            return;
        }
        let path = self.path.clone();
        let task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(SAVE_DEBOUNCE).await;
                let payload = this
                    .update(cx, |service, _| {
                        if !service.disk_cache.dirty {
                            return None;
                        }
                        service.disk_cache.dirty = false;
                        Some((path.clone(), service.disk_cache.serialize()))
                    })
                    .ok()
                    .flatten();
                match payload {
                    Some((path, bytes)) => {
                        let _ = cx
                            .background_spawn(async move { std::fs::write(path, bytes) })
                            .await;
                    }
                    None => break,
                }
            }
            this.update(cx, |service, _| service.disk_cache.save_task = None)
                .ok();
        });
        self.save_task = Some(task);
    }

    /// Returns a snapshot of all currently cached translations, for tests.
    #[cfg(test)]
    pub(crate) fn entries(&self) -> &HashMap<String, TranslationCacheEntry> {
        &self.entries
    }

    fn serialize(&self) -> Vec<u8> {
        serde_json::to_vec(&TranslationCacheFile {
            version: CACHE_FILE_VERSION,
            entries: self.entries.clone(),
        })
        .unwrap_or_default()
    }

    /// Evicts the lowest-scoring entries until the total is within budget.
    fn enforce_budget(&mut self) {
        let now = unix_seconds();
        while self.total_bytes > self.max_bytes && !self.entries.is_empty() {
            let victim = self
                .entries
                .iter()
                .min_by(|(ka, a), (kb, b)| {
                    score(a, now)
                        .partial_cmp(&score(b, now))
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| ka.cmp(kb))
                })
                .map(|(key, _)| key.clone());
            match victim {
                Some(key) => {
                    if let Some(entry) = self.entries.remove(&key) {
                        self.total_bytes =
                            self.total_bytes.saturating_sub(entry_bytes(&key, &entry));
                    }
                }
                None => break,
            }
        }
    }
}

fn entry_bytes(key: &str, entry: &TranslationCacheEntry) -> usize {
    key.len() + entry.translation.len() + ENTRY_OVERHEAD
}

/// Weighted eviction score: higher is more worth keeping.
fn score(entry: &TranslationCacheEntry, now: u64) -> f64 {
    let days_since_view = now.saturating_sub(entry.last_viewed_at) as f64 / 86_400.0;
    let recency = 1.0 / (1.0 + days_since_view);
    let frequency = entry.view_count.min(VIEW_COUNT_CAP) as f64 / VIEW_COUNT_CAP as f64;
    let size_penalty = (entry.translation.len() as f64 / MAX_ENTRY_BYTES as f64).min(1.0);
    RECENCY_WEIGHT * recency + FREQ_WEIGHT * frequency - SIZE_WEIGHT * size_penalty
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cache(max_bytes: usize) -> TranslationDiskCache {
        TranslationDiskCache::new(
            PathBuf::from("/tmp/translation_cache_test.json"),
            max_bytes,
            true,
        )
    }

    fn entry(translation: &str) -> TranslationCacheEntry {
        TranslationCacheEntry::new(translation.into(), unix_seconds())
    }

    #[test]
    fn test_round_trip_serialization() {
        let mut cache = test_cache(1024 * 1024);
        cache.store("provider:model:中文:abc", "你好世界".into());
        cache.store("provider:model:中文:def", "解析代码块".into());

        let mut loaded = test_cache(1024 * 1024);
        loaded.load_from_bytes(&cache.serialize());
        assert!(loaded.loaded);
        assert_eq!(
            loaded
                .entries
                .get("provider:model:中文:abc")
                .unwrap()
                .translation,
            "你好世界"
        );
        assert_eq!(
            loaded
                .entries
                .get("provider:model:中文:def")
                .unwrap()
                .translation,
            "解析代码块"
        );
    }

    #[test]
    fn test_corrupt_or_incompatible_file_starts_fresh() {
        let mut cache = test_cache(1024 * 1024);
        cache.load_from_bytes(b"not json at all");
        assert!(cache.loaded);
        assert!(cache.entries.is_empty());

        let mut cache = test_cache(1024 * 1024);
        let bytes = serde_json::to_vec(&TranslationCacheFile {
            version: CACHE_FILE_VERSION + 1,
            entries: HashMap::from_iter([("k".into(), entry("译"))]),
        })
        .unwrap();
        cache.load_from_bytes(&bytes);
        assert!(cache.loaded);
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn test_oversized_entry_is_not_persisted() {
        let mut cache = test_cache(10);
        cache.store("k", "这是一段非常长的翻译内容".into());
        assert!(cache.entries.is_empty());
        assert_eq!(cache.total_bytes, 0);
    }

    #[test]
    fn test_disabled_cache_does_not_store() {
        let mut cache = TranslationDiskCache::new(PathBuf::from("/tmp/x.json"), 1024, false);
        cache.store("k", "译".into());
        assert!(cache.entries.is_empty());
        cache.record_view("k");
        assert_eq!(cache.entries.len(), 0);
    }

    #[test]
    fn test_eviction_prefers_recent_entries() {
        // ~102 bytes per entry, so three entries exceed a 250-byte budget.
        let mut cache = test_cache(250);
        cache.store("old", "短".into());
        cache.store("recent", "短".into());
        // Age the first entry so it scores lower than the second.
        cache.entries.get_mut("old").unwrap().last_viewed_at = 0;
        cache.entries.get_mut("old").unwrap().view_count = 1;

        // Storing a third entry pushes the cache over budget; the old entry
        // must be evicted first.
        cache.store("new", "短".into());
        assert!(cache.entries.contains_key("new"));
        assert!(cache.entries.contains_key("recent"));
        assert!(!cache.entries.contains_key("old"));
    }

    #[test]
    fn test_eviction_prefers_small_entries() {
        // The large entry (~3100 bytes) fits alone but not together with the
        // two small entries; its size penalty makes it the first victim.
        let mut cache = test_cache(3200);
        let large = "大".repeat(1000);
        cache.store("large", large.into());
        cache.store("small1", "短".into());
        cache.store("small2", "短".into());

        // The large entry has the highest size penalty and must go first.
        assert!(!cache.entries.contains_key("large"));
        assert!(cache.entries.contains_key("small1"));
        assert!(cache.entries.contains_key("small2"));
        assert!(cache.total_bytes <= cache.max_bytes);
    }

    #[test]
    fn test_record_view_updates_frequency_and_recency() {
        let mut cache = test_cache(1024 * 1024);
        cache.store("k", "译".into());
        let before = cache.entries.get("k").unwrap().view_count;
        cache.record_view("k");
        assert_eq!(cache.entries.get("k").unwrap().view_count, before + 1);
        assert!(cache.dirty);
    }

    #[test]
    fn test_configure_evicts_when_budget_shrinks() {
        let mut cache = test_cache(1024 * 1024);
        cache.store("a", "翻译内容A".into());
        cache.store("b", "翻译内容B".into());
        cache.configure(true, 10);
        assert!(cache.entries.is_empty());
    }
}

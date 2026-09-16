//! Tool Result Cache
//!
//! Provides caching for tool execution results to avoid redundant operations.
//! This is particularly useful for read-only tools like `read` or `glob` where
//! the same inputs will produce the same outputs within a short time window.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// Configuration for the tool result cache
#[derive(Debug, Clone)]
pub struct CacheConfig {
    /// Maximum number of entries in the cache
    pub max_entries: usize,
    /// Time-to-live for cache entries
    pub ttl: Duration,
    /// Whether caching is enabled
    pub enabled: bool,
    /// Tools that should never be cached (e.g., bash, write)
    pub excluded_tools: Vec<String>,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_entries: 100,
            ttl: Duration::from_mins(1),
            enabled: true,
            excluded_tools: vec![
                "bash".to_string(),
                "write".to_string(),
                "edit".to_string(),
                "explore".to_string(),
                "repository_symbols".to_string(),
                "extract_document".to_string(),
                "spawn_subagent".to_string(),
                "coding_task".to_string(),
                "list_subagents".to_string(),
                "get_subagent".to_string(),
                "wait_subagent".to_string(),
                "resume_subagent".to_string(),
                "cancel_subagent".to_string(),
                "control_subagent".to_string(),
            ],
        }
    }
}

/// Key for cache entries
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CacheKey {
    /// Tool name
    pub tool_name: String,
    /// Hash of the arguments
    pub args_hash: u64,
}

fn canonicalize_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut normalized = serde_json::Map::new();
            for key in keys {
                if let Some(val) = map.get(key) {
                    normalized.insert(key.clone(), canonicalize_json(val));
                }
            }
            serde_json::Value::Object(normalized)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonicalize_json).collect::<Vec<_>>())
        }
        _ => value.clone(),
    }
}

impl CacheKey {
    /// Create a new cache key
    pub fn new(tool_name: impl Into<String>, args: &serde_json::Value) -> Self {
        Self::new_with_generation(tool_name, args, None)
    }

    pub(crate) fn for_generation(
        tool_name: impl Into<String>,
        args: &serde_json::Value,
        generation: u64,
    ) -> Self {
        Self::new_with_generation(tool_name, args, Some(generation))
    }

    fn new_with_generation(
        tool_name: impl Into<String>,
        args: &serde_json::Value,
        generation: Option<u64>,
    ) -> Self {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        let normalized_args = canonicalize_json(args);
        serde_json::to_string(&normalized_args)
            .unwrap_or_default()
            .hash(&mut hasher);
        generation.hash(&mut hasher);
        Self {
            tool_name: tool_name.into().to_lowercase(),
            args_hash: hasher.finish(),
        }
    }
}

/// Cached tool result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedResult {
    /// The tool result output
    pub output: String,
    /// Whether the result was an error
    pub is_error: bool,
    /// When the entry was created (runtime use, not serialized)
    #[serde(skip)]
    pub created_at: Option<Instant>,
    /// Timestamp for persistence (seconds since `UNIX_EPOCH`)
    #[serde(default)]
    pub created_timestamp: Option<u64>,
}

impl CachedResult {
    /// Create a new cached result
    pub fn new(output: impl Into<String>, is_error: bool) -> Self {
        let now = Instant::now();
        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .ok();
        Self {
            output: output.into(),
            is_error,
            created_at: Some(now),
            created_timestamp: timestamp,
        }
    }

    /// Check if the cache entry has expired
    #[must_use]
    pub fn is_expired(&self, ttl: Duration) -> bool {
        self.created_at.is_none_or(|t| t.elapsed() > ttl)
    }

    /// Check if the entry is expired based on timestamp (for persisted entries)
    #[must_use]
    pub fn is_expired_by_timestamp(&self, ttl: Duration) -> bool {
        let Some(created) = self.created_timestamp else {
            return true;
        };
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        now.saturating_sub(created) > ttl.as_secs()
    }

    /// Restore an approximate Instant from a persisted timestamp.
    fn restore_created_at_from_timestamp(&mut self) {
        let Some(created_ts) = self.created_timestamp else {
            return;
        };

        let created_system = SystemTime::UNIX_EPOCH + Duration::from_secs(created_ts);
        match SystemTime::now().duration_since(created_system) {
            Ok(elapsed) => {
                self.created_at = Instant::now().checked_sub(elapsed).or_else(|| {
                    // Fallback if elapsed is larger than the current Instant range.
                    Some(Instant::now())
                });
            }
            Err(_) => {
                // Clock skew or future timestamps: keep entry but start freshness window now.
                self.created_at = Some(Instant::now());
            }
        }
    }
}

/// A persistable cache entry that combines key and result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistableCacheEntry {
    /// The cache key
    pub key: CacheKey,
    /// The cached result
    pub result: CachedResult,
}

/// Tool result cache
#[derive(Debug)]
pub struct ToolResultCache {
    /// Configuration
    config: CacheConfig,
    /// Cached entries
    entries: HashMap<CacheKey, CachedResult>,
    /// Access order for LRU eviction (most recent at end)
    access_order: Vec<CacheKey>,
    /// File dependencies: maps file paths to cache keys that depend on them
    file_deps: HashMap<PathBuf, Vec<CacheKey>>,
    /// Cache hit count
    hits: u64,
    /// Cache miss count
    misses: u64,
}

impl Default for ToolResultCache {
    fn default() -> Self {
        Self::new(CacheConfig::default())
    }
}

impl ToolResultCache {
    /// Create a new cache with the given configuration
    #[must_use]
    pub fn new(mut config: CacheConfig) -> Self {
        // Normalize tool names for case-insensitive matching.
        config.excluded_tools = config
            .excluded_tools
            .into_iter()
            .map(|tool| tool.to_lowercase())
            .collect();
        Self {
            config,
            entries: HashMap::new(),
            access_order: Vec::new(),
            file_deps: HashMap::new(),
            hits: 0,
            misses: 0,
        }
    }

    /// Check if caching is enabled for a tool
    #[must_use]
    pub fn is_cacheable(&self, tool_name: &str) -> bool {
        let tool_name = tool_name.to_lowercase();
        self.config.enabled && !self.config.excluded_tools.contains(&tool_name)
    }

    /// Check cache provenance without mutating statistics or recency state.
    #[must_use]
    pub fn contains_fresh(&self, key: &CacheKey) -> bool {
        self.config.enabled
            && self
                .entries
                .get(key)
                .is_some_and(|entry| !entry.is_expired(self.config.ttl))
    }

    /// Get a cached result
    pub fn get(&mut self, key: &CacheKey) -> Option<&CachedResult> {
        if !self.config.enabled {
            self.misses += 1;
            return None;
        }

        // Check if entry exists and is not expired
        if let Some(entry) = self.entries.get(key) {
            if entry.is_expired(self.config.ttl) {
                // Remove expired entry
                self.entries.remove(key);
                self.access_order.retain(|k| k != key);
                self.misses += 1;
                return None;
            }

            // Update access order
            self.access_order.retain(|k| k != key);
            self.access_order.push(key.clone());

            self.hits += 1;
            return self.entries.get(key);
        }

        self.misses += 1;
        None
    }

    /// Store a result in the cache
    pub fn put(&mut self, key: CacheKey, result: CachedResult) {
        if !self.config.enabled {
            return;
        }

        // Evict old entries if at capacity
        if self.entries.contains_key(&key) {
            self.access_order.retain(|existing| existing != &key);
            self.remove_deps_for_key(&key);
            self.entries.remove(&key);
        }

        while self.entries.len() >= self.config.max_entries && !self.access_order.is_empty() {
            let oldest = self.access_order.remove(0);
            self.entries.remove(&oldest);
            self.remove_deps_for_key(&oldest);
        }

        self.entries.insert(key.clone(), result);
        self.access_order.push(key);
    }

    /// Store a result in the cache with file dependencies
    ///
    /// File dependencies allow targeted invalidation when files are modified.
    /// When any of the dependent files changes, this cache entry will be invalidated.
    pub fn put_with_deps(&mut self, key: CacheKey, result: CachedResult, deps: Vec<PathBuf>) {
        if !self.config.enabled {
            return;
        }

        // Evict old entries if at capacity
        if self.entries.contains_key(&key) {
            self.access_order.retain(|existing| existing != &key);
            self.remove_deps_for_key(&key);
            self.entries.remove(&key);
        }

        while self.entries.len() >= self.config.max_entries && !self.access_order.is_empty() {
            let oldest = self.access_order.remove(0);
            self.entries.remove(&oldest);
            self.remove_deps_for_key(&oldest);
        }

        // Track file dependencies
        for dep in deps {
            let canonical = dunce::canonicalize(&dep).unwrap_or_else(|_| dep.clone());
            self.file_deps
                .entry(dep.clone())
                .or_default()
                .push(key.clone());
            if canonical != dep {
                self.file_deps
                    .entry(canonical)
                    .or_default()
                    .push(key.clone());
            }
        }

        self.entries.insert(key.clone(), result);
        self.access_order.push(key);
    }

    /// Remove dependency tracking for a cache key
    fn remove_deps_for_key(&mut self, key: &CacheKey) {
        // Remove this key from all dependency lists
        for keys in self.file_deps.values_mut() {
            keys.retain(|k| k != key);
        }
        // Clean up empty dependency entries
        self.file_deps.retain(|_, keys| !keys.is_empty());
    }

    /// Invalidate cache entries that depend on a file
    ///
    /// Call this when a file is modified to ensure stale cache entries are removed.
    /// Returns the number of entries invalidated.
    pub fn invalidate_for_file(&mut self, path: &Path) -> usize {
        let canonical = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

        // Match exact file dependencies and directory dependencies that
        // contain the changed path. Directory dependencies are used by
        // searches/globs; exact dependencies are used by reads.
        let mut all_keys = self.file_deps.get(&canonical).cloned().unwrap_or_default();
        all_keys.extend(self.file_deps.get(path).cloned().unwrap_or_default());
        for (dependency, keys) in &self.file_deps {
            // Path::starts_with is component-aware, so treating every tracked
            // dependency as a prefix avoids filesystem stats and also invalidates
            // entries for directories that did not exist when they were cached.
            if canonical.starts_with(dependency) || path.starts_with(dependency) {
                all_keys.extend(keys.iter().cloned());
            }
        }
        all_keys.sort_unstable_by(|left, right| {
            left.tool_name
                .cmp(&right.tool_name)
                .then(left.args_hash.cmp(&right.args_hash))
        });
        all_keys.dedup();
        let count = all_keys.len();

        // Remove the entries
        for key in all_keys {
            self.entries.remove(&key);
            self.access_order.retain(|k| k != &key);
            self.remove_deps_for_key(&key);
        }

        count
    }

    /// Invalidate all cache entries that depend on files in a directory
    ///
    /// Useful when a directory or its contents are modified.
    /// Returns the number of entries invalidated.
    pub fn invalidate_for_directory(&mut self, dir: &Path) -> usize {
        let canonical_dir = dunce::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());

        // Find all files that start with this directory path
        let files_to_invalidate: Vec<PathBuf> = self
            .file_deps
            .keys()
            .filter(|p| p.starts_with(&canonical_dir) || p.starts_with(dir))
            .cloned()
            .collect();

        let mut total = 0;
        for file in files_to_invalidate {
            total += self.invalidate_for_file(&file);
        }
        total
    }

    /// Get the number of tracked file dependencies
    #[must_use]
    pub fn file_dep_count(&self) -> usize {
        self.file_deps.len()
    }

    /// Clear the cache
    pub fn clear(&mut self) {
        self.entries.clear();
        self.access_order.clear();
        self.file_deps.clear();
    }

    /// Remove expired entries
    pub fn evict_expired(&mut self) {
        let ttl = self.config.ttl;
        let expired: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, v)| v.is_expired(ttl))
            .map(|(k, _)| k.clone())
            .collect();

        for key in expired {
            self.entries.remove(&key);
            self.access_order.retain(|k| k != &key);
            self.remove_deps_for_key(&key);
        }
    }

    /// Get cache statistics
    #[must_use]
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            entries: self.entries.len(),
            max_entries: self.config.max_entries,
            hits: self.hits,
            misses: self.misses,
            hit_rate: if self.hits + self.misses > 0 {
                self.hits as f64 / (self.hits + self.misses) as f64
            } else {
                0.0
            },
        }
    }

    /// Get the configuration
    #[must_use]
    pub fn config(&self) -> &CacheConfig {
        &self.config
    }

    /// Update configuration
    pub fn set_config(&mut self, config: CacheConfig) {
        self.config = config;
    }

    // ─────────────────────────────────────────────────────────────────────────
    // PERSISTENCE METHODS
    // ─────────────────────────────────────────────────────────────────────────

    /// Get the default cache file path for a workspace
    #[must_use]
    pub fn default_cache_path(workspace: &Path) -> PathBuf {
        workspace.join(".composer").join("tool-cache.jsonl")
    }

    /// Save the cache to a JSONL file
    ///
    /// Each line is a JSON object with a key and result.
    /// Only saves entries that haven't expired.
    pub fn save_to_file(&self, path: &Path) -> std::io::Result<()> {
        // Create parent directories if needed
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let file = std::fs::File::create(path)?;
        let mut writer = std::io::BufWriter::new(file);

        // Write each non-expired entry as a JSON line
        for (key, result) in &self.entries {
            // Skip expired entries
            if result.is_expired(self.config.ttl) {
                continue;
            }

            let entry = PersistableCacheEntry {
                key: key.clone(),
                result: result.clone(),
            };

            if let Ok(json) = serde_json::to_string(&entry) {
                writeln!(writer, "{json}")?;
            }
        }

        writer.flush()?;
        Ok(())
    }

    /// Load the cache from a JSONL file
    ///
    /// Replaces the current cache contents with entries from the file.
    /// Expired entries are skipped during load.
    pub fn load_from_file(&mut self, path: &Path) -> std::io::Result<usize> {
        if !path.exists() {
            return Ok(0);
        }

        let file = std::fs::File::open(path)?;
        let reader = std::io::BufReader::new(file);

        // Clear current entries and deps
        self.entries.clear();
        self.access_order.clear();
        self.file_deps.clear();

        let mut loaded = 0;

        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }

            // Parse the entry
            let entry: PersistableCacheEntry = match serde_json::from_str(&line) {
                Ok(e) => e,
                Err(_) => continue, // Skip malformed lines
            };

            // Skip expired entries based on timestamp
            if entry.result.is_expired_by_timestamp(self.config.ttl) {
                continue;
            }

            // Respect max_entries limit
            if self.entries.len() >= self.config.max_entries {
                break;
            }

            // Convert timestamp back to Instant for runtime use
            let mut result = entry.result;
            if result.created_at.is_none() && result.created_timestamp.is_some() {
                result.restore_created_at_from_timestamp();
            }

            self.entries.insert(entry.key.clone(), result);
            self.access_order.push(entry.key);
            loaded += 1;
        }

        Ok(loaded)
    }

    /// Load cache from a file path, creating a new cache with default config
    #[must_use]
    pub fn load_or_create(path: &Path) -> Self {
        let mut cache = Self::new(CacheConfig::default());
        let _ = cache.load_from_file(path);
        cache
    }

    /// Save the cache and return the number of entries saved
    pub fn save(&self, path: &Path) -> std::io::Result<usize> {
        self.save_to_file(path)?;
        Ok(self.entries.len())
    }
}

/// Cache statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheStats {
    /// Current number of entries
    pub entries: usize,
    /// Maximum number of entries
    pub max_entries: usize,
    /// Number of cache hits
    pub hits: u64,
    /// Number of cache misses
    pub misses: u64,
    /// Hit rate (0.0 - 1.0)
    pub hit_rate: f64,
}

/// Thread-safe wrapper for ToolResultCache.
/// Use this when you need to share the cache across async tasks or threads.
///
/// Note: Currently only used in tests. Will be integrated into production
/// when tool caching is enabled across concurrent agent sessions.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct SharedCache {
    inner: std::sync::RwLock<ToolResultCache>,
}

#[cfg(test)]
impl SharedCache {
    /// Create a new shared cache with default configuration
    pub fn new() -> Self {
        Self {
            inner: std::sync::RwLock::new(ToolResultCache::default()),
        }
    }

    /// Create a new shared cache with the given configuration
    pub fn with_config(config: CacheConfig) -> Self {
        Self {
            inner: std::sync::RwLock::new(ToolResultCache::new(config)),
        }
    }

    /// Get a cached result (returns a clone)
    pub fn get(&self, key: &CacheKey) -> Option<CachedResult> {
        self.inner.write().ok()?.get(key).cloned()
    }

    /// Store a result in the cache
    pub fn put(&self, key: CacheKey, result: CachedResult) {
        if let Ok(mut cache) = self.inner.write() {
            cache.put(key, result);
        }
    }

    /// Get cache statistics
    pub fn stats(&self) -> Option<CacheStats> {
        self.inner.read().ok().map(|cache| cache.stats())
    }

    /// Clear all entries
    pub fn clear(&self) {
        if let Ok(mut cache) = self.inner.write() {
            cache.clear();
        }
    }

    /// Check if a tool is cacheable
    pub fn is_cacheable(&self, tool_name: &str) -> bool {
        self.inner
            .read()
            .map(|cache| cache.is_cacheable(tool_name))
            .unwrap_or(false)
    }
}

#[cfg(test)]
impl Default for SharedCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests;

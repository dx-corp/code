//! Incremental, revision-addressed Rust symbol localization.
//!
//! This index is a model-facing search projection, not a Rust compiler or an
//! authorization source. It recognizes common item definitions and records the
//! files containing identifier references. A refresh retains parsed records for
//! unchanged files and atomically swaps one complete snapshot for another.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime};

use regex::Regex;
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::files::{FileIndexer, IndexerConfig};

const MAX_QUERY_RESULTS: usize = 100;

#[derive(Clone, Debug)]
pub struct SymbolIndexConfig {
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub background_refresh_interval: Duration,
}

impl Default for SymbolIndexConfig {
    fn default() -> Self {
        Self {
            max_files: 50_000,
            max_file_bytes: 4 * 1024 * 1024,
            background_refresh_interval: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SymbolLocation {
    pub path: String,
    pub line: usize,
    pub kind: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolQueryResult {
    pub symbol: String,
    pub revision: String,
    pub definitions: Vec<SymbolLocation>,
    pub referencing_files: Vec<String>,
    pub total_definitions: usize,
    pub total_referencing_files: usize,
    pub indexed_files: usize,
    pub skipped_files: usize,
    pub index_truncated: bool,
    pub index_age_ms: u64,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolRefreshStats {
    pub revision: String,
    pub indexed_files: usize,
    pub parsed_files: usize,
    pub reused_files: usize,
    pub removed_files: usize,
    pub skipped_files: usize,
    pub index_truncated: bool,
    pub duration_ms: u64,
}

#[derive(Debug, Error)]
pub enum SymbolIndexError {
    #[error("repository root is not a directory: {0}")]
    InvalidRoot(String),
    #[error("symbol must be one Rust identifier")]
    InvalidSymbol,
    #[error("maxResults must be between 1 and {MAX_QUERY_RESULTS}")]
    InvalidLimit,
    #[error("repository symbol index has not been built")]
    NotReady,
    #[error("cannot read Rust source {path}: {source}")]
    ReadSource {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("Rust source is not UTF-8: {0}")]
    NonUtf8Source(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileFingerprint {
    len: u64,
    modified: Option<SystemTime>,
}

#[derive(Clone, Debug)]
struct FileRecord {
    relative_path: String,
    fingerprint: FileFingerprint,
    content_digest: [u8; 32],
    definitions: Vec<(String, SymbolLocation)>,
    identifiers: HashSet<String>,
}

struct BuiltGraph {
    definitions: HashMap<String, Vec<SymbolLocation>>,
    references: HashMap<String, Vec<String>>,
    revision: String,
}

#[derive(Clone, Debug, Default)]
struct SymbolSnapshot {
    files: HashMap<PathBuf, FileRecord>,
    definitions: HashMap<String, Vec<SymbolLocation>>,
    references: HashMap<String, Vec<String>>,
    revision: String,
    skipped_files: usize,
    index_truncated: bool,
    refreshed_at: Option<Instant>,
    indexed_generation: u64,
}

pub struct RepositorySymbolIndex {
    root: PathBuf,
    config: SymbolIndexConfig,
    invalidation_generation: AtomicU64,
    background_refreshing: AtomicBool,
    refresh_lock: Mutex<()>,
    snapshot: RwLock<SymbolSnapshot>,
}

impl RepositorySymbolIndex {
    #[must_use]
    pub fn new(root: impl AsRef<Path>, config: SymbolIndexConfig) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
            config,
            invalidation_generation: AtomicU64::new(1),
            background_refreshing: AtomicBool::new(false),
            refresh_lock: Mutex::new(()),
            snapshot: RwLock::new(SymbolSnapshot::default()),
        }
    }

    pub fn invalidate(&self) {
        self.invalidation_generation.fetch_add(1, Ordering::AcqRel);
    }

    pub fn begin_background_refresh(&self) -> bool {
        self.background_refreshing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub fn end_background_refresh(&self) {
        self.background_refreshing.store(false, Ordering::Release);
    }

    #[must_use]
    pub fn needs_blocking_refresh(&self) -> bool {
        let generation = self.invalidation_generation.load(Ordering::Acquire);
        let snapshot = self
            .snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        snapshot.refreshed_at.is_none() || snapshot.indexed_generation != generation
    }

    #[must_use]
    pub fn needs_background_refresh(&self) -> bool {
        if self.needs_blocking_refresh() {
            return false;
        }
        self.snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .refreshed_at
            .is_some_and(|instant| instant.elapsed() >= self.config.background_refresh_interval)
    }

    pub fn refresh(&self) -> Result<SymbolRefreshStats, SymbolIndexError> {
        let started = Instant::now();
        let _refresh_guard = self
            .refresh_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = dunce::canonicalize(&self.root)
            .map_err(|_| SymbolIndexError::InvalidRoot(self.root.to_string_lossy().into_owned()))?;
        if !root.is_dir() {
            return Err(SymbolIndexError::InvalidRoot(
                self.root.to_string_lossy().into_owned(),
            ));
        }
        let generation = self.invalidation_generation.load(Ordering::Acquire);
        let previous = self
            .snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .files
            .clone();

        let indexer = FileIndexer::new(
            IndexerConfig::default()
                .with_max_files(self.config.max_files.saturating_add(1))
                .include_only(&["rs"]),
        );
        let mut paths = indexer
            .index_sync(&root)
            .into_iter()
            .map(|file| file.path)
            .collect::<Vec<_>>();
        paths.sort_unstable();
        let file_limit_reached = paths.len() > self.config.max_files;
        paths.truncate(self.config.max_files);

        let mut files = HashMap::with_capacity(paths.len());
        let mut parsed_files = 0usize;
        let mut reused_files = 0usize;
        let mut skipped_files = 0usize;
        for path in paths {
            let metadata = std::fs::symlink_metadata(&path).map_err(|source| {
                SymbolIndexError::ReadSource {
                    path: display_relative(&root, &path),
                    source,
                }
            })?;
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || metadata.len() > self.config.max_file_bytes
            {
                skipped_files += 1;
                continue;
            }
            let fingerprint = FileFingerprint {
                len: metadata.len(),
                modified: metadata.modified().ok(),
            };
            let relative_path = display_relative(&root, &path);
            let data = std::fs::read(&path).map_err(|source| SymbolIndexError::ReadSource {
                path: relative_path,
                source,
            })?;
            let content_digest: [u8; 32] = Sha256::digest(&data).into();
            if let Some(record) = previous.get(&path).filter(|record| {
                record.fingerprint == fingerprint && record.content_digest == content_digest
            }) {
                files.insert(path, record.clone());
                reused_files += 1;
                continue;
            }
            let record = parse_file(&root, &path, fingerprint, data, content_digest)?;
            files.insert(path, record);
            parsed_files += 1;
        }

        let removed_files = previous
            .keys()
            .filter(|path| !files.contains_key(*path))
            .count();
        let index_truncated = file_limit_reached || skipped_files > 0;
        let BuiltGraph {
            definitions,
            references,
            revision,
        } = build_graph(&files, skipped_files, index_truncated);
        let indexed_files = files.len();
        let next = SymbolSnapshot {
            files,
            definitions,
            references,
            revision: revision.clone(),
            skipped_files,
            index_truncated,
            refreshed_at: Some(Instant::now()),
            indexed_generation: generation,
        };
        *self
            .snapshot
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = next;

        Ok(SymbolRefreshStats {
            revision,
            indexed_files,
            parsed_files,
            reused_files,
            removed_files,
            skipped_files,
            index_truncated,
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }

    pub fn query(
        &self,
        symbol: &str,
        max_results: usize,
    ) -> Result<SymbolQueryResult, SymbolIndexError> {
        if !identifier_regex().is_match(symbol) {
            return Err(SymbolIndexError::InvalidSymbol);
        }
        if !(1..=MAX_QUERY_RESULTS).contains(&max_results) {
            return Err(SymbolIndexError::InvalidLimit);
        }
        let snapshot = self
            .snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(refreshed_at) = snapshot.refreshed_at else {
            return Err(SymbolIndexError::NotReady);
        };
        let all_definitions = snapshot
            .definitions
            .get(symbol)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let all_references = snapshot
            .references
            .get(symbol)
            .map(Vec::as_slice)
            .unwrap_or_default();
        Ok(SymbolQueryResult {
            symbol: symbol.to_string(),
            revision: snapshot.revision.clone(),
            definitions: all_definitions.iter().take(max_results).cloned().collect(),
            referencing_files: all_references.iter().take(max_results).cloned().collect(),
            total_definitions: all_definitions.len(),
            total_referencing_files: all_references.len(),
            indexed_files: snapshot.files.len(),
            skipped_files: snapshot.skipped_files,
            index_truncated: snapshot.index_truncated,
            index_age_ms: refreshed_at.elapsed().as_millis() as u64,
            truncated: all_definitions.len() > max_results || all_references.len() > max_results,
        })
    }
}

fn parse_file(
    root: &Path,
    path: &Path,
    fingerprint: FileFingerprint,
    data: Vec<u8>,
    content_digest: [u8; 32],
) -> Result<FileRecord, SymbolIndexError> {
    let relative_path = display_relative(root, path);
    let text = std::str::from_utf8(&data)
        .map_err(|_| SymbolIndexError::NonUtf8Source(relative_path.clone()))?;
    let mut definitions = Vec::new();
    for (line_index, line) in text.lines().enumerate() {
        if let Some(captures) = definition_regex().captures(line) {
            let (Some(kind), Some(name)) = (captures.name("kind"), captures.name("name")) else {
                continue;
            };
            definitions.push((
                name.as_str().to_string(),
                SymbolLocation {
                    path: relative_path.clone(),
                    line: line_index + 1,
                    kind: kind.as_str().to_string(),
                },
            ));
        }
    }
    let identifiers = identifier_scan_regex()
        .find_iter(text)
        .map(|matched| matched.as_str().to_string())
        .collect();
    Ok(FileRecord {
        relative_path,
        fingerprint,
        content_digest,
        definitions,
        identifiers,
    })
}

fn build_graph(
    files: &HashMap<PathBuf, FileRecord>,
    skipped_files: usize,
    index_truncated: bool,
) -> BuiltGraph {
    let ordered = files
        .values()
        .map(|record| (record.relative_path.as_str(), record))
        .collect::<BTreeMap<_, _>>();
    let mut digest = Sha256::new();
    digest.update(skipped_files.to_le_bytes());
    digest.update([u8::from(index_truncated)]);
    let mut definitions: HashMap<String, Vec<SymbolLocation>> = HashMap::new();
    for (path, record) in &ordered {
        digest.update(path.as_bytes());
        digest.update([0]);
        digest.update(record.content_digest);
        digest.update([0]);
        for (symbol, location) in &record.definitions {
            definitions
                .entry(symbol.clone())
                .or_default()
                .push(location.clone());
        }
    }
    for locations in definitions.values_mut() {
        locations.sort_unstable_by(|left, right| {
            left.path.cmp(&right.path).then(left.line.cmp(&right.line))
        });
    }

    let known_symbols = definitions.keys().cloned().collect::<HashSet<_>>();
    let mut references: HashMap<String, Vec<String>> = HashMap::new();
    for (path, record) in ordered {
        for symbol in record.identifiers.intersection(&known_symbols) {
            references
                .entry(symbol.clone())
                .or_default()
                .push(path.to_string());
        }
    }
    for paths in references.values_mut() {
        paths.sort_unstable();
        paths.dedup();
    }
    BuiltGraph {
        definitions,
        references,
        revision: format!("{:x}", digest.finalize()),
    }
}

fn display_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn identifier_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").expect("valid identifier regex"))
}

fn identifier_scan_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").expect("valid identifier regex"))
}

fn definition_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:(?:async|unsafe|const)\s+)*(?P<kind>struct|enum|trait|fn|type|mod|const|static)\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)",
        )
        .expect("valid definition regex")
    })
}

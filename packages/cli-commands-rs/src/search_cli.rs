//! Native `maestro search <query>` full-text search over local session transcripts.
//!
//! Sessions are stored as JSONL under `~/.composer/agent/sessions/<project-slug>/`.
//! This module scans every session file, extracts searchable documents (user and
//! assistant message text plus tool result content) via [`SessionReader`], ranks
//! matches by term frequency, and prints a short highlighted snippet per match.
//!
//! Parsing is cached in `~/.composer/search-index.json`, keyed by absolute file
//! path and invalidated on mtime/size change, so repeat searches skip re-parsing
//! unchanged sessions. The cache is best-effort: a corrupt or unreadable cache
//! simply falls back to a fresh scan.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{Local, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::session::{SessionManager, SessionReader};

const DEFAULT_LIMIT: usize = 20;
/// Maximum characters kept per document so the cache stays small even for
/// sessions with very large tool outputs.
const MAX_DOCUMENT_CHARS: usize = 50_000;
/// Approximate snippet width in characters (grep-style context window).
const SNIPPET_WIDTH: usize = 160;
/// Context shown before the first match inside a snippet.
const SNIPPET_LEADING_CONTEXT: usize = 60;
const CACHE_SCHEMA_VERSION: u32 = 1;

const USAGE: &str =
    "Usage: maestro search <query> [--json] [--limit N] [--kind user|assistant|tool]";

/// Message-role filter for `--kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchKind {
    User,
    Assistant,
    Tool,
}

impl SearchKind {
    fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "user" => Some(Self::User),
            "assistant" => Some(Self::Assistant),
            "tool" | "toolresult" | "tool_result" => Some(Self::Tool),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

/// A single searchable unit extracted from a session message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchDocument {
    pub session_id: String,
    pub project: String,
    pub timestamp_ms: u64,
    pub kind: SearchKind,
    pub text: String,
}

/// A highlighted snippet with byte ranges of every matched term.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snippet {
    pub text: String,
    /// Byte offsets `(start, end)` into `text` for each matched term.
    pub highlights: Vec<(usize, usize)>,
}

/// One ranked search result.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchMatch {
    pub session_id: String,
    pub project: String,
    pub timestamp_ms: u64,
    pub kind: SearchKind,
    pub score: usize,
    pub snippet: Snippet,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchRequest {
    Help,
    Query {
        query: String,
        json: bool,
        kind: Option<SearchKind>,
        limit: usize,
    },
}

/// Parse `maestro search` arguments (after the `search` token).
pub fn parse_search_args(args: &[String]) -> std::result::Result<SearchRequest, String> {
    let mut json = false;
    let mut kind: Option<SearchKind> = None;
    let mut limit = DEFAULT_LIMIT;
    let mut positional: Vec<String> = Vec::new();

    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "--help" | "-h" | "help" => return Ok(SearchRequest::Help),
            "--json" => json = true,
            "--limit" => {
                i += 1;
                let value = args
                    .get(i)
                    .ok_or_else(|| "--limit requires a value".to_string())?;
                limit = parse_limit(value)?;
            }
            "--kind" => {
                i += 1;
                let value = args
                    .get(i)
                    .ok_or_else(|| "--kind requires a value".to_string())?;
                kind =
                    Some(SearchKind::parse(value).ok_or_else(|| {
                        format!("invalid --kind: {value} (use user|assistant|tool)")
                    })?);
            }
            _ if arg.starts_with("--limit=") => {
                limit = parse_limit(&arg["--limit=".len()..])?;
            }
            _ if arg.starts_with("--kind=") => {
                let value = &arg["--kind=".len()..];
                kind =
                    Some(SearchKind::parse(value).ok_or_else(|| {
                        format!("invalid --kind: {value} (use user|assistant|tool)")
                    })?);
            }
            _ if arg.starts_with('-') => return Err(format!("unknown flag: {arg}")),
            _ => positional.push(arg.clone()),
        }
        i += 1;
    }

    if positional.is_empty() {
        return Err("missing search query".to_string());
    }

    Ok(SearchRequest::Query {
        query: positional.join(" "),
        json,
        kind,
        limit,
    })
}

fn parse_limit(value: &str) -> std::result::Result<usize, String> {
    let limit = value
        .parse::<usize>()
        .map_err(|_| format!("invalid --limit: {value}"))?;
    if limit == 0 {
        return Err("--limit must be at least 1".to_string());
    }
    Ok(limit)
}

/// Split a query into lowercase search terms.
pub fn query_terms(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Score a document by total (case-insensitive) term occurrences.
pub fn score_text(text_lower: &str, terms: &[String]) -> usize {
    terms
        .iter()
        .map(|term| text_lower.matches(term.as_str()).count())
        .sum()
}

fn floor_char_boundary(s: &str, mut index: usize) -> usize {
    while index > 0 && !s.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(s: &str, mut index: usize) -> usize {
    while index < s.len() && !s.is_char_boundary(index) {
        index += 1;
    }
    index
}

/// Extract a grep-style snippet around the first term match.
///
/// Whitespace (including newlines) is collapsed so multi-line messages render
/// on one line. Returns `None` when no term matches.
pub fn extract_snippet(text: &str, terms: &[String], width: usize) -> Option<Snippet> {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let flat_lower = flat.to_lowercase();

    let first_match = terms
        .iter()
        .filter_map(|term| flat_lower.find(term.as_str()).map(|pos| (pos, term.len())))
        .min_by_key(|(pos, _)| *pos)?;

    let window_start =
        floor_char_boundary(&flat, first_match.0.saturating_sub(SNIPPET_LEADING_CONTEXT));
    let window_end = ceil_char_boundary(&flat, (window_start + width).min(flat.len()));

    let prefix = if window_start > 0 { "…" } else { "" };
    let suffix = if window_end < flat.len() { "…" } else { "" };
    let mut snippet_text =
        String::with_capacity(prefix.len() + (window_end - window_start) + suffix.len());
    snippet_text.push_str(prefix);
    snippet_text.push_str(&flat[window_start..window_end]);
    snippet_text.push_str(suffix);

    let body_offset = prefix.len();
    let body_lower = &flat_lower[window_start..window_end];
    let mut highlights: Vec<(usize, usize)> = Vec::new();
    for term in terms {
        for (pos, matched) in body_lower.match_indices(term.as_str()) {
            highlights.push((body_offset + pos, body_offset + pos + matched.len()));
        }
    }
    highlights.sort_unstable();

    Some(Snippet {
        text: snippet_text,
        highlights,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CachedFile {
    mtime_ms: u64,
    len: u64,
    documents: Vec<SearchDocument>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchIndexCache {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    files: HashMap<String, CachedFile>,
}

fn load_cache(path: &Path) -> SearchIndexCache {
    let Ok(raw) = fs::read_to_string(path) else {
        return SearchIndexCache::default();
    };
    match serde_json::from_str::<SearchIndexCache>(&raw) {
        Ok(cache) if cache.version == CACHE_SCHEMA_VERSION => cache,
        _ => SearchIndexCache::default(),
    }
}

fn save_cache(path: &Path, cache: &SearchIndexCache) {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = fs::create_dir_all(parent);
        }
    }
    if let Ok(raw) = serde_json::to_string(cache) {
        let _ = fs::write(path, raw);
    }
}

fn file_stamp(metadata: &fs::Metadata) -> (u64, u64) {
    let mtime_ms = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    (mtime_ms, metadata.len())
}

fn truncate_document(text: &str) -> String {
    if text.chars().count() <= MAX_DOCUMENT_CHARS {
        return text.to_string();
    }
    text.chars().take(MAX_DOCUMENT_CHARS).collect()
}

/// Extract searchable documents from one parsed session file.
fn documents_from_session(
    session: &crate::session::ParsedSession,
    project_fallback: &str,
) -> Vec<SearchDocument> {
    let project = if session.header.cwd.is_empty() {
        project_fallback.to_string()
    } else {
        session.header.cwd.clone()
    };
    session
        .messages
        .iter()
        .filter_map(|message| {
            let kind = match message.role() {
                "user" => SearchKind::User,
                "assistant" => SearchKind::Assistant,
                "toolResult" => SearchKind::Tool,
                _ => return None,
            };
            let text = message.text_content();
            if text.trim().is_empty() {
                return None;
            }
            Some(SearchDocument {
                session_id: session.header.id.clone(),
                project: project.clone(),
                timestamp_ms: message.timestamp(),
                kind,
                text: truncate_document(&text),
            })
        })
        .collect()
}

/// Collect documents for every session under `root`, using `cache_path` to
/// avoid re-parsing unchanged files. Unparseable files are skipped.
pub fn collect_documents(root: &Path, cache_path: Option<&Path>) -> Vec<SearchDocument> {
    let mut cache = cache_path.map(load_cache).unwrap_or_default();
    let mut documents = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    if root.is_dir() {
        collect_from_root(root, &mut cache, &mut seen, &mut documents);
    }

    // Prune cache entries for files that no longer exist.
    cache.files.retain(|path, _| seen.contains(path));
    if let Some(path) = cache_path {
        cache.version = CACHE_SCHEMA_VERSION;
        save_cache(path, &cache);
    }
    documents
}

fn collect_from_root(
    root: &Path,
    cache: &mut SearchIndexCache,
    seen: &mut HashSet<String>,
    documents: &mut Vec<SearchDocument>,
) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_from_root(&path, cache, seen, documents);
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            collect_from_file(&path, cache, seen, documents);
        }
    }
}

fn collect_from_file(
    path: &Path,
    cache: &mut SearchIndexCache,
    seen: &mut HashSet<String>,
    documents: &mut Vec<SearchDocument>,
) {
    let key = path.to_string_lossy().to_string();
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    let (mtime_ms, len) = file_stamp(&metadata);
    seen.insert(key.clone());

    if let Some(cached) = cache.files.get(&key) {
        if cached.mtime_ms == mtime_ms && cached.len == len {
            documents.extend(cached.documents.iter().cloned());
            return;
        }
    }

    let project_fallback = path
        .parent()
        .and_then(|dir| dir.file_name())
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();

    let Ok(session) = SessionReader::read_file(path) else {
        // Do not cache failures: a partially-written file should be retried.
        return;
    };
    let docs = documents_from_session(&session, &project_fallback);
    cache.files.insert(
        key,
        CachedFile {
            mtime_ms,
            len,
            documents: docs.clone(),
        },
    );
    documents.extend(docs);
}

/// Rank documents matching `query`, best first.
pub fn search_documents(
    documents: &[SearchDocument],
    query: &str,
    kind: Option<SearchKind>,
    limit: usize,
) -> Vec<SearchMatch> {
    let terms = query_terms(query);
    if terms.is_empty() {
        return Vec::new();
    }

    let mut matches: Vec<SearchMatch> = documents
        .iter()
        .filter(|doc| kind.is_none_or(|k| doc.kind == k))
        .filter_map(|doc| {
            let score = score_text(&doc.text.to_lowercase(), &terms);
            if score == 0 {
                return None;
            }
            let snippet = extract_snippet(&doc.text, &terms, SNIPPET_WIDTH)?;
            Some(SearchMatch {
                session_id: doc.session_id.clone(),
                project: doc.project.clone(),
                timestamp_ms: doc.timestamp_ms,
                kind: doc.kind,
                score,
                snippet,
            })
        })
        .collect();

    matches.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.timestamp_ms.cmp(&a.timestamp_ms))
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    matches.truncate(limit);
    matches
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonReport<'a> {
    query: &'a str,
    count: usize,
    matches: &'a [SearchMatch],
}

fn format_timestamp(timestamp_ms: u64) -> String {
    Utc.timestamp_millis_opt(timestamp_ms as i64)
        .single()
        .map(|dt| {
            dt.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "unknown-time".to_string())
}

fn apply_highlights(text: &str, highlights: &[(usize, usize)], open: &str, close: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for &(start, end) in highlights {
        if start < cursor || end > text.len() {
            continue;
        }
        out.push_str(&text[cursor..start]);
        out.push_str(open);
        out.push_str(&text[start..end]);
        out.push_str(close);
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    out
}

fn print_human(matches: &[SearchMatch], color: bool) {
    let (open, close) = if color {
        ("\x1b[1m", "\x1b[0m")
    } else {
        ("", "")
    };
    for (index, m) in matches.iter().enumerate() {
        if index > 0 {
            println!();
        }
        let id_short: String = m.session_id.chars().take(8).collect();
        println!(
            "{id_short} {:<9} {} {}",
            m.kind.as_str(),
            format_timestamp(m.timestamp_ms),
            m.project
        );
        println!(
            "  {}",
            apply_highlights(&m.snippet.text, &m.snippet.highlights, open, close)
        );
    }
}

/// Entry point for `maestro search`.
pub fn run_search(args: &[String]) -> Result<i32> {
    let request = match parse_search_args(args) {
        Ok(request) => request,
        Err(message) => {
            eprintln!("{message}");
            eprintln!("{USAGE}");
            return Ok(2);
        }
    };
    let SearchRequest::Query {
        query,
        json,
        kind,
        limit,
    } = request
    else {
        println!("{USAGE}");
        println!();
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "Search local session transcripts (user/assistant text and tool results).",
                &[]
            )
        );
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "Exit codes: 0 = matches found, 1 = no matches, 2 = error.",
                &[]
            )
        );
        return Ok(0);
    };

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let manager = SessionManager::new(cwd.to_string_lossy().to_string());
    // sessions_dir is `~/.composer/agent/sessions/<slug>`; search spans all slugs.
    let Some(root) = manager.sessions_dir().parent().map(Path::to_path_buf) else {
        eprintln!(
            "{}",
            crate::localization::cli_locale().format("could not resolve sessions directory", &[])
        );
        return Ok(2);
    };
    let cache_path = root
        .parent()
        .and_then(Path::parent)
        .map(|composer| composer.join("search-index.json"));

    let documents = collect_documents(&root, cache_path.as_deref());
    let matches = search_documents(&documents, &query, kind, limit);

    if json {
        let report = JsonReport {
            query: &query,
            count: matches.len(),
            matches: &matches,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if matches.is_empty() {
        println!(
            "{}",
            crate::localization::cli_locale()
                .format("No matches for \"{0}\".", std::slice::from_ref(&(query)))
        );
    } else {
        print_human(&matches, io::stdout().is_terminal());
    }

    Ok(i32::from(matches.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_session(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        let mut file = fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        path
    }

    fn fixture_root() -> TempDir {
        let root = TempDir::new().unwrap();
        let project_a = root.path().join("--home-dev-project-a--");
        write_session(
            &project_a,
            "session-a.jsonl",
            &[
                r#"{"type":"session","id":"session-a","timestamp":"2024-01-15T10:30:00Z","cwd":"/home/dev/project-a","model":"openai/gpt-5.2","thinkingLevel":"medium"}"#,
                r#"{"type":"message","timestamp":"2024-01-15T10:30:00Z","message":{"role":"user","content":"how do I fix the flaky search indexer test","timestamp":1000}}"#,
                r#"{"type":"message","timestamp":"2024-01-15T10:30:01Z","message":{"role":"assistant","content":[{"type":"text","text":"The indexer test flakes because the indexer cache is stale. Reset the indexer."}],"timestamp":1001}}"#,
                r#"{"type":"message","timestamp":"2024-01-15T10:30:02Z","message":{"role":"toolResult","toolCallId":"call-1","toolName":"bash","content":"running tests...\nindexer ok","isError":false,"timestamp":1002}}"#,
            ],
        );
        let project_b = root.path().join("--home-dev-project-b--");
        write_session(
            &project_b,
            "session-b.jsonl",
            &[
                r#"{"type":"session","id":"session-b","timestamp":"2024-01-16T09:00:00Z","cwd":"/home/dev/project-b","model":"openai/gpt-5.2","thinkingLevel":"medium"}"#,
                r#"{"type":"message","timestamp":"2024-01-16T09:00:00Z","message":{"role":"user","content":"refactor the login page","timestamp":2000}}"#,
                r#"{"type":"message","timestamp":"2024-01-16T09:00:01Z","message":{"role":"assistant","content":[{"type":"text","text":"indexer mentioned once here"}],"timestamp":2001}}"#,
            ],
        );
        root
    }

    #[test]
    fn parse_args_defaults_and_flags() {
        let request = parse_search_args(&["indexer".to_string()]).unwrap();
        assert_eq!(
            request,
            SearchRequest::Query {
                query: "indexer".into(),
                json: false,
                kind: None,
                limit: DEFAULT_LIMIT,
            }
        );

        let request = parse_search_args(&[
            "indexer".into(),
            "--json".into(),
            "--limit".into(),
            "5".into(),
            "--kind=tool".into(),
        ])
        .unwrap();
        assert_eq!(
            request,
            SearchRequest::Query {
                query: "indexer".into(),
                json: true,
                kind: Some(SearchKind::Tool),
                limit: 5,
            }
        );

        // Multi-word queries join positionals.
        let request = parse_search_args(&["flaky".into(), "test".into()]).unwrap();
        assert!(matches!(
            request,
            SearchRequest::Query { ref query, .. } if query == "flaky test"
        ));

        assert_eq!(
            parse_search_args(&["--help".into()]).unwrap(),
            SearchRequest::Help
        );
        assert!(parse_search_args(&[]).is_err());
        assert!(parse_search_args(&["q".into(), "--kind".into(), "robot".into()]).is_err());
        assert!(parse_search_args(&["q".into(), "--limit".into(), "0".into()]).is_err());
        assert!(parse_search_args(&["q".into(), "--nope".into()]).is_err());
    }

    #[test]
    fn query_terms_lowercase_and_split() {
        assert_eq!(
            query_terms("Flaky, Search-INDEXER!"),
            vec!["flaky", "search", "indexer"]
        );
        assert!(query_terms("  !!!  ").is_empty());
    }

    #[test]
    fn score_counts_occurrences() {
        let text = "indexer indexer indexer, said the indexer".to_lowercase();
        assert_eq!(score_text(&text, &["indexer".to_string()]), 4);
        assert_eq!(score_text(&text, &["missing".to_string()]), 0);
        assert_eq!(
            score_text(&text, &["indexer".to_string(), "said".to_string()]),
            5
        );
    }

    #[test]
    fn snippet_centers_first_match_and_highlights_all() {
        let text = "start of a very long line\nwith newlines and the keyword deep inside plus another keyword later";
        let terms = vec!["keyword".to_string()];
        let snippet = extract_snippet(text, &terms, 160).unwrap();
        assert!(!snippet.text.contains('\n'));
        assert_eq!(snippet.highlights.len(), 2);
        for (start, end) in &snippet.highlights {
            assert_eq!(&snippet.text[*start..*end], "keyword");
        }
        assert!(extract_snippet("nothing here", &terms, 160).is_none());
    }

    #[test]
    fn snippet_truncates_long_text_with_ellipsis() {
        let prefix = "a".repeat(500);
        let text = format!("{prefix} needle {}", "b".repeat(500));
        let terms = vec!["needle".to_string()];
        let snippet = extract_snippet(&text, &terms, 160).unwrap();
        assert!(snippet.text.starts_with('…'));
        assert!(snippet.text.ends_with('…'));
        assert!(snippet.text.contains("needle"));
        assert_eq!(snippet.highlights.len(), 1);
    }

    #[test]
    fn snippet_is_case_insensitive_but_preserves_original_case() {
        let snippet =
            extract_snippet("the INDEXER is Stale", &["indexer".to_string()], 160).unwrap();
        assert!(snippet.text.contains("INDEXER"));
        let (start, end) = snippet.highlights[0];
        assert_eq!(&snippet.text[start..end], "INDEXER");
    }

    #[test]
    fn end_to_end_search_ranks_and_filters() {
        let root = fixture_root();
        let cache = root.path().join("search-index.json");
        let docs = collect_documents(root.path(), Some(&cache));
        assert_eq!(docs.len(), 5);

        // "indexer" appears 3x in session-a assistant text, 1x in session-a user
        // text, 1x in session-a tool result, 1x in session-b assistant text.
        let matches = search_documents(&docs, "indexer", None, 20);
        assert_eq!(matches.len(), 4);
        assert_eq!(matches[0].session_id, "session-a");
        assert_eq!(matches[0].kind, SearchKind::Assistant);
        assert_eq!(matches[0].score, 3);
        assert_eq!(matches[0].project, "/home/dev/project-a");
        assert_eq!(matches[3].score, 1);

        // Kind filter keeps only tool results.
        let matches = search_documents(&docs, "indexer", Some(SearchKind::Tool), 20);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].kind, SearchKind::Tool);

        // Limit truncates.
        let matches = search_documents(&docs, "indexer", None, 1);
        assert_eq!(matches.len(), 1);

        // Multi-word query requires no single document to have every term.
        let matches = search_documents(&docs, "flaky login", None, 20);
        assert_eq!(matches.len(), 2);

        // No match.
        assert!(search_documents(&docs, "nonexistent-term", None, 20).is_empty());
    }

    #[test]
    fn cache_avoids_reparse_and_invalidates_on_change() {
        let root = fixture_root();
        let cache = root.path().join("search-index.json");

        let docs = collect_documents(root.path(), Some(&cache));
        assert_eq!(docs.len(), 5);
        assert!(cache.exists());

        // Second run reads from cache and yields identical results.
        let cached = load_cache(&cache);
        assert_eq!(cached.files.len(), 2);
        let docs_again = collect_documents(root.path(), Some(&cache));
        assert_eq!(docs_again.len(), 5);

        // Touching a file (content change bumps len) invalidates its entry.
        let session_b = root
            .path()
            .join("--home-dev-project-b--")
            .join("session-b.jsonl");
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&session_b)
            .unwrap();
        writeln!(
            file,
            r#"{{"type":"message","timestamp":"2024-01-16T09:00:02Z","message":{{"role":"user","content":"indexer follow-up","timestamp":2002}}}}"#
        )
        .unwrap();
        drop(file);
        let docs_updated = collect_documents(root.path(), Some(&cache));
        assert_eq!(docs_updated.len(), 6);
    }

    #[test]
    fn corrupt_cache_and_missing_root_are_tolerated() {
        let root = fixture_root();
        let cache = root.path().join("search-index.json");
        fs::write(&cache, "{ not json").unwrap();
        let docs = collect_documents(root.path(), Some(&cache));
        assert_eq!(docs.len(), 5);

        let missing = root.path().join("does-not-exist");
        assert!(collect_documents(&missing, Some(&cache)).is_empty());
    }

    #[test]
    fn unparseable_session_is_skipped_without_caching() {
        let root = TempDir::new().unwrap();
        let project = root.path().join("--proj--");
        write_session(
            &project,
            "bad.jsonl",
            &[r#"{"type":"message","message":broken"#],
        );
        let cache = root.path().join("search-index.json");
        assert!(collect_documents(root.path(), Some(&cache)).is_empty());
        assert!(load_cache(&cache).files.is_empty());
    }

    #[test]
    fn apply_highlights_wraps_matches() {
        let out = apply_highlights("a needle here", &[(2, 8)], "<b>", "</b>");
        assert_eq!(out, "a <b>needle</b> here");
    }
}

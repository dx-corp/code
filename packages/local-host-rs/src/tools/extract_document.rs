//! Document extraction tool helpers.
//!
//! This module provides functionality for extracting text content from various
//! document formats downloaded from URLs. It supports:
//!
//! - **PDF** - Portable Document Format
//! - **DOCX** - Microsoft Word Open XML documents
//! - **PPTX** - Microsoft PowerPoint Open XML presentations
//! - **XLSX** - Microsoft Excel Open XML spreadsheets
//! - **Text** - Plain text, Markdown, CSV, JSON, XML, YAML
//!
//! # Features
//!
//! - Automatic format detection based on file extension and MIME type
//! - Content-Disposition header parsing for filename extraction
//! - Structure-preserving Markdown: DOCX headings, lists, and tables; PPTX
//!   per-slide sections; XLSX per-sheet tables; PDF `## Page N` sections that
//!   name pages without a text layer (see [`ooxml`] for the Office formats)
//! - Size limits to prevent memory exhaustion (50MB max)
//! - Character-offset paging (`offset`/`maxChars`) that reports the total
//!   size and the next offset instead of silently truncating

use std::env;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{self, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering as AtomicOrdering};
use std::thread;
use std::time::{Duration, Instant};

use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::net_guard;
use crate::agent::ToolResult;

mod ooxml;

#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
#[cfg(windows)]
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
};

const MAX_DOWNLOAD_BYTES: usize = 50 * 1024 * 1024;
const MARKITDOWN_EXTRACT_TIMEOUT: Duration = Duration::from_secs(20);
/// Timeout for the document download request, matching `web_fetch`'s default.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);
static MARKITDOWN_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static NATIVE_TEST_EXTRACTION_ACTIVE: AtomicBool = AtomicBool::new(false);

struct ExtractionCancellation {
    cancelled: Arc<AtomicBool>,
}

impl ExtractionCancellation {
    fn new() -> (Self, Arc<AtomicBool>) {
        let cancelled = Arc::new(AtomicBool::new(false));
        (
            Self {
                cancelled: Arc::clone(&cancelled),
            },
            cancelled,
        )
    }
}

const EXTRACTION_NATIVE: u8 = 0;
const EXTRACTION_MARKITDOWN: u8 = 1;
const EXTRACTION_CANCELLED: u8 = 2;
const EXTRACTION_FINISHED: u8 = 3;

struct MarkitdownActivity<'a>(&'a AtomicU8);

impl<'a> MarkitdownActivity<'a> {
    fn begin(phase: &'a AtomicU8) -> Result<Self, String> {
        phase
            .compare_exchange(
                EXTRACTION_NATIVE,
                EXTRACTION_MARKITDOWN,
                AtomicOrdering::AcqRel,
                AtomicOrdering::Acquire,
            )
            .map(|_| Self(phase))
            .map_err(|_| "MarkItDown conversion cancelled".to_string())
    }
}

impl Drop for MarkitdownActivity<'_> {
    fn drop(&mut self) {
        let _ = self.0.compare_exchange(
            EXTRACTION_MARKITDOWN,
            EXTRACTION_NATIVE,
            AtomicOrdering::AcqRel,
            AtomicOrdering::Acquire,
        );
    }
}

impl Drop for ExtractionCancellation {
    fn drop(&mut self) {
        self.cancelled.store(true, AtomicOrdering::Release);
    }
}

#[cfg(windows)]
struct OwnedWindowsHandle(HANDLE);

#[cfg(windows)]
impl Drop for OwnedWindowsHandle {
    fn drop(&mut self) {
        // SAFETY: this type exclusively owns the valid handle returned by a
        // Win32 API call below.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
struct MarkitdownJobObject(OwnedWindowsHandle);

#[cfg(windows)]
impl MarkitdownJobObject {
    fn assign(child: &std::process::Child) -> std::io::Result<Self> {
        use std::os::windows::io::AsRawHandle;

        // SAFETY: null security attributes and name request an unnamed job
        // object with default security.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let job = OwnedWindowsHandle(job);

        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: limits has the exact layout and size required by this
        // information class, and job remains live for the call.
        if unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }

        // SAFETY: std::process::Child owns a live process handle until it is
        // dropped.
        if unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle() as HANDLE) } == 0 {
            return Err(std::io::Error::last_os_error());
        }

        Ok(Self(job))
    }
}

#[cfg(windows)]
fn resume_markitdown_process(child: &std::process::Child) -> std::io::Result<()> {
    let pid = child.id();
    // SAFETY: the snapshot has no caller-owned backing storage.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let snapshot = OwnedWindowsHandle(snapshot);
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..THREADENTRY32::default()
    };
    let mut resumed = 0usize;

    // SAFETY: entry is initialized with the required structure size and
    // remains valid while the snapshot is enumerated.
    let mut has_entry = unsafe { Thread32First(snapshot.0, &mut entry) };
    while has_entry != 0 {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: the thread id came from the live system snapshot.
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if thread.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let thread = OwnedWindowsHandle(thread);
            // SAFETY: thread has THREAD_SUSPEND_RESUME access.
            if unsafe { ResumeThread(thread.0) } == u32::MAX {
                return Err(std::io::Error::last_os_error());
            }
            resumed += 1;
        }
        // SAFETY: same initialized snapshot and entry as above.
        has_entry = unsafe { Thread32Next(snapshot.0, &mut entry) };
    }

    if resumed == 0 {
        return Err(std::io::Error::other(
            "spawned MarkItDown process had no resumable threads",
        ));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct ExtractDocumentArgs {
    url: String,
    /// Character offset into the extracted Markdown to start returning from.
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default, alias = "maxChars")]
    max_chars: Option<usize>,
}

/// Characters returned per call when the caller does not set `maxChars`.
/// Sized to stay below the model-facing tool-output spill threshold so a
/// page arrives inline; callers continue with the reported next offset.
const DEFAULT_PAGE_CHARS: usize = 30_000;
/// Largest page a single call may request.
const MAX_PAGE_CHARS: usize = 1_000_000;

/// Which window of the extracted text a call returns.
#[derive(Debug, Clone, Copy, Default)]
struct PageRequest {
    offset: usize,
    max_chars: Option<usize>,
}

struct DocumentExtraction {
    format: String,
    extractor: String,
    /// Returned window, including any continuation header and footer.
    text: String,
    /// Character offset of the first returned character.
    offset: usize,
    /// Characters of document content in this window.
    returned_chars: usize,
    /// Characters in the whole extracted document.
    total_chars: usize,
    /// Offset to pass on the next call, when more content remains.
    next_offset: Option<usize>,
    sections: Option<SectionSummary>,
}

/// A window of the extracted text, measured in characters.
#[derive(Debug, PartialEq, Eq)]
struct TextWindow<'a> {
    text: &'a str,
    end: usize,
    total_chars: usize,
}

/// Select up to `max_chars` characters starting at `offset`. When more text
/// remains, the window ends after the last line break in its final fifth so
/// a page does not split a line or table row; offsets stay deterministic
/// because the caller continues from the reported end.
fn text_window(full: &str, offset: usize, max_chars: usize) -> Result<TextWindow<'_>, String> {
    let total_chars = full.chars().count();
    if offset > 0 && offset >= total_chars {
        return Err(format!(
            "offset {offset} is past the end of the extracted document ({total_chars} characters)."
        ));
    }
    let start_byte = full
        .char_indices()
        .nth(offset)
        .map_or(full.len(), |(index, _)| index);
    let rest = &full[start_byte..];
    let Some((window_end_byte, _)) = rest.char_indices().nth(max_chars) else {
        return Ok(TextWindow {
            text: rest,
            end: total_chars,
            total_chars,
        });
    };
    let window = &rest[..window_end_byte];
    let snapped = window
        .rfind('\n')
        .map(|newline| &window[..=newline])
        .filter(|candidate| candidate.chars().count() >= max_chars - max_chars / 5);
    let text = snapped.unwrap_or(window);
    Ok(TextWindow {
        text,
        end: offset + text.chars().count(),
        total_chars,
    })
}

fn guess_filename_from_url(url: &reqwest::Url) -> String {
    url.path_segments()
        .and_then(|mut segments| segments.rfind(|s| !s.is_empty()))
        .map_or_else(|| "document".to_string(), std::string::ToString::to_string)
}

fn parse_content_disposition(header: Option<&str>) -> Option<String> {
    let header = header?;
    let patterns = [
        Regex::new(r"filename\*=UTF-8''([^;]+)").ok()?,
        Regex::new(r#"filename="([^"]+)""#).ok()?,
        Regex::new(r"filename=([^;]+)").ok()?,
    ];
    for pattern in patterns {
        if let Some(caps) = pattern.captures(header) {
            if let Some(name) = caps.get(1) {
                let raw = name.as_str().trim();
                if let Ok(decoded) = urlencoding::decode(raw) {
                    return Some(decoded.to_string());
                }
                return Some(raw.to_string());
            }
        }
    }
    None
}

/// Plain-text extraction result before paging, with optional per-section
/// accounting (PDF pages, PPTX slides, XLSX sheets).
struct Extracted {
    text: String,
    sections: Option<SectionSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SectionSummary {
    /// `page`, `slide`, or `sheet`.
    kind: &'static str,
    count: usize,
    /// One-based section numbers that produced no text.
    without_text: Vec<usize>,
}

impl Extracted {
    fn plain(text: String) -> Self {
        Self {
            text,
            sections: None,
        }
    }
}

/// Note written in place of a PDF page that has no extractable text layer.
/// Scanned pages need OCR, which this extractor does not perform.
const PDF_PAGE_WITHOUT_TEXT_NOTE: &str = "_(No extractable text layer on this page. It may be a scanned image or a figure; OCR is not performed by this tool.)_";

fn tidy_pdf_page_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0usize;
    for line in text.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank_run += 1;
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank_run > 0 { "\n\n" } else { "\n" });
        }
        blank_run = 0;
        out.push_str(line);
    }
    out
}

/// Render PDF text page by page as `## Page N` sections. Pages without a text
/// layer are reported explicitly rather than silently omitted.
fn pdf_pages_to_markdown(pages: &[String]) -> Option<Extracted> {
    if pages.is_empty() {
        return None;
    }
    let mut sections = Vec::with_capacity(pages.len());
    let mut without_text = Vec::new();
    for (index, page) in pages.iter().enumerate() {
        let number = index + 1;
        let body = tidy_pdf_page_text(page);
        if body.is_empty() {
            without_text.push(number);
            sections.push(format!("## Page {number}\n\n{PDF_PAGE_WITHOUT_TEXT_NOTE}"));
        } else {
            sections.push(format!("## Page {number}\n\n{body}"));
        }
    }
    Some(Extracted {
        text: sections.join("\n\n"),
        sections: Some(SectionSummary {
            kind: "page",
            count: pages.len(),
            without_text,
        }),
    })
}

fn extract_pdf(bytes: &[u8]) -> Option<Extracted> {
    let pages = pdf_extract::extract_text_from_mem_by_pages(bytes).ok()?;
    pdf_pages_to_markdown(&pages)
}

fn extract_text_file(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_string()
}

fn detect_format(file_name: &str, mime: Option<&str>) -> Option<&'static str> {
    let lower = file_name.to_lowercase();
    #[cfg(test)]
    if lower.ends_with(".slow-native-test.html") {
        return Some("slow_native_test");
    }
    if lower.ends_with(".pdf") || mime == Some("application/pdf") {
        return Some("pdf");
    }
    if lower.ends_with(".docx")
        || mime == Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document")
    {
        return Some("docx");
    }
    if lower.ends_with(".pptx")
        || mime == Some("application/vnd.openxmlformats-officedocument.presentationml.presentation")
    {
        return Some("pptx");
    }
    if lower.ends_with(".xlsx")
        || mime == Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet")
    {
        return Some("xlsx");
    }
    if lower.ends_with(".txt")
        || lower.ends_with(".md")
        || lower.ends_with(".csv")
        || lower.ends_with(".json")
        || lower.ends_with(".xml")
        || lower.ends_with(".yaml")
        || lower.ends_with(".yml")
        || mime.is_some_and(|m| m.starts_with("text/"))
    {
        return Some("text");
    }
    None
}

fn extract_from_format(format: &str, bytes: &[u8]) -> Option<Extracted> {
    match format {
        #[cfg(test)]
        "slow_native_test" => {
            NATIVE_TEST_EXTRACTION_ACTIVE.store(true, AtomicOrdering::Release);
            thread::sleep(Duration::from_secs(2));
            NATIVE_TEST_EXTRACTION_ACTIVE.store(false, AtomicOrdering::Release);
            Some(Extracted::plain(String::new()))
        }
        "pdf" => extract_pdf(bytes),
        "docx" => ooxml::docx_to_markdown(bytes),
        "pptx" => ooxml::pptx_to_markdown(bytes),
        "xlsx" => ooxml::xlsx_to_markdown(bytes),
        "text" => Some(Extracted::plain(extract_text_file(bytes))),
        _ => None,
    }
}

fn env_flag_enabled(name: &str) -> bool {
    env::var(name)
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "on" | "yes"
            )
        })
        .unwrap_or(false)
}

fn env_flag_disabled(name: &str) -> bool {
    env::var(name)
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            )
        })
        .unwrap_or(false)
}

fn markitdown_disabled() -> bool {
    env_flag_disabled("MAESTRO_MARKITDOWN")
}

fn markitdown_preferred() -> bool {
    env_flag_enabled("MAESTRO_MARKITDOWN_PREFER")
}

fn split_command_args(value: Option<String>) -> Result<Vec<String>, String> {
    let input = value.unwrap_or_default();
    let input = input.trim();
    if input.is_empty() {
        return Ok(Vec::new());
    }

    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut token_started = false;
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if quote == Some('\'') {
            if ch == '\'' {
                quote = None;
            } else {
                current.push(ch);
            }
            token_started = true;
            continue;
        }

        if quote == Some('"') {
            if ch == '"' {
                quote = None;
                token_started = true;
                continue;
            }
            if ch == '\\' {
                if let Some(&next) = chars.peek() {
                    if matches!(next, '"' | '\\' | '$' | '`' | '\n') {
                        current.push(next);
                        chars.next();
                    } else {
                        current.push(ch);
                    }
                } else {
                    current.push(ch);
                }
                token_started = true;
                continue;
            }
            current.push(ch);
            token_started = true;
            continue;
        }

        if ch == '\\' {
            if let Some(&next) = chars.peek() {
                if next.is_whitespace() || matches!(next, '\'' | '"' | '\\') {
                    current.push(next);
                    chars.next();
                } else {
                    current.push(ch);
                }
            } else {
                current.push(ch);
            }
            token_started = true;
            continue;
        }

        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            token_started = true;
            continue;
        }

        if ch.is_whitespace() {
            if token_started {
                args.push(std::mem::take(&mut current));
                token_started = false;
            }
            continue;
        }

        current.push(ch);
        token_started = true;
    }

    if quote.is_some() {
        return Err("Unterminated quote in MAESTRO_MARKITDOWN_ARGS".to_string());
    }

    if token_started {
        args.push(current);
    }
    Ok(args)
}

fn markitdown_candidates() -> Result<Vec<(String, Vec<String>)>, String> {
    if let Ok(command) = env::var("MAESTRO_MARKITDOWN_CMD") {
        let command = command.trim();
        if !command.is_empty() {
            return Ok(vec![(
                command.to_string(),
                split_command_args(env::var("MAESTRO_MARKITDOWN_ARGS").ok())?,
            )]);
        }
    }
    Ok(vec![
        ("markitdown".to_string(), Vec::new()),
        ("uvx".to_string(), vec!["markitdown".to_string()]),
    ])
}

fn should_try_markitdown(format: &str, file_name: &str, mime_type: Option<&str>) -> bool {
    if markitdown_disabled() {
        return false;
    }
    if markitdown_preferred() {
        return true;
    }
    let lower_name = file_name.to_ascii_lowercase();
    let mime_type = mime_type.unwrap_or("").to_ascii_lowercase();
    format == "unknown"
        || lower_name.ends_with(".html")
        || lower_name.ends_with(".htm")
        || mime_type.contains("text/html")
}

fn read_markitdown_pipe<R>(mut pipe: R) -> thread::JoinHandle<std::io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut output = Vec::new();
        pipe.read_to_end(&mut output)?;
        Ok(output)
    })
}

fn join_markitdown_pipe(
    name: &str,
    reader: thread::JoinHandle<std::io::Result<Vec<u8>>>,
) -> Result<Vec<u8>, String> {
    reader
        .join()
        .map_err(|_| format!("MarkItDown {name} reader panicked"))?
        .map_err(|error| format!("failed to read MarkItDown {name}: {error}"))
}

#[cfg(unix)]
fn configure_markitdown_process(command: &mut Command) {
    super::process_utils::set_std_process_group(command);
}

#[cfg(windows)]
fn configure_markitdown_process(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    // Suspend before user code can spawn descendants, assign the process to a
    // kill-on-close job, then resume it after spawn.
    command.creation_flags(CREATE_SUSPENDED);
}

#[cfg(not(any(unix, windows)))]
fn configure_markitdown_process(_command: &mut Command) {}

fn kill_markitdown_process(
    child: &mut std::process::Child,
    #[cfg_attr(not(unix), allow(unused_variables))] process_group_id: u32,
) {
    #[cfg(unix)]
    super::process_utils::kill_process_group(process_group_id);
    let _ = child.kill();
}

fn run_markitdown_command_with_cancellation(
    command: &str,
    args: &[String],
    cancelled: &AtomicBool,
) -> Result<String, String> {
    let mut process = Command::new(command);
    process
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_markitdown_process(&mut process);
    let mut child = process.spawn().map_err(|error| error.to_string())?;
    // Unix configures the child as its own process-group leader before exec,
    // so retain that identifier even after try_wait reaps the launcher.
    let process_group_id = child.id();
    #[cfg(unix)]
    let mut process_group = super::process_utils::ProcessGroupGuard::new(Some(process_group_id));
    #[cfg(windows)]
    let mut windows_job = Some({
        let job = match MarkitdownJobObject::assign(&child) {
            Ok(job) => job,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "failed to contain MarkItDown process tree: {error}"
                ));
            }
        };
        if let Err(error) = resume_markitdown_process(&child) {
            drop(job);
            let _ = child.wait();
            return Err(format!("failed to resume MarkItDown process: {error}"));
        }
        job
    });
    let stdout_reader = child
        .stdout
        .take()
        .map(read_markitdown_pipe)
        .ok_or_else(|| "failed to capture MarkItDown stdout".to_string())?;
    let stderr_reader = child
        .stderr
        .take()
        .map(read_markitdown_pipe)
        .ok_or_else(|| "failed to capture MarkItDown stderr".to_string())?;
    let mut stdout_reader = Some(stdout_reader);
    let mut stderr_reader = Some(stderr_reader);
    let started = Instant::now();
    loop {
        if cancelled.load(AtomicOrdering::Acquire) {
            kill_markitdown_process(&mut child, process_group_id);
            #[cfg(unix)]
            process_group.disarm();
            #[cfg(windows)]
            drop(windows_job.take());
            let _ = child.wait();
            return Err("MarkItDown conversion cancelled".to_string());
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("failed to wait for MarkItDown: {error}"))?
        {
            // A converter launcher can exit while descendants still inherit
            // its stdout/stderr. Terminate the retained Unix process group
            // before joining the pipe readers or they can block forever.
            #[cfg(unix)]
            process_group.terminate();
            #[cfg(windows)]
            drop(windows_job.take());
            let stdout = join_markitdown_pipe(
                "stdout",
                stdout_reader
                    .take()
                    .ok_or_else(|| "missing MarkItDown stdout reader".to_string())?,
            )?;
            let stderr = join_markitdown_pipe(
                "stderr",
                stderr_reader
                    .take()
                    .ok_or_else(|| "missing MarkItDown stderr reader".to_string())?,
            )?;
            if status.success() {
                return String::from_utf8(stdout)
                    .map_err(|_| "MarkItDown output was not UTF-8".to_string());
            }
            let stderr = String::from_utf8_lossy(&stderr);
            return Err(format!(
                "MarkItDown exited with {}{}",
                status,
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {}", stderr.chars().take(500).collect::<String>())
                }
            ));
        }
        if started.elapsed() > MARKITDOWN_EXTRACT_TIMEOUT {
            kill_markitdown_process(&mut child, process_group_id);
            #[cfg(unix)]
            process_group.disarm();
            #[cfg(windows)]
            drop(windows_job.take());
            let _ = child.wait();
            return Err("MarkItDown conversion timed out".to_string());
        }
        thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(test)]
fn run_markitdown_command(command: &str, args: &[String]) -> Result<String, String> {
    run_markitdown_command_with_cancellation(command, args, &AtomicBool::new(false))
}

fn extract_with_markitdown(
    bytes: &[u8],
    file_name: &str,
    mime_type: Option<&str>,
    cancelled: &AtomicBool,
    extraction_phase: &AtomicU8,
) -> Result<Option<String>, String> {
    if markitdown_disabled() {
        return Ok(None);
    }
    // Mark the whole external-converter boundary active before checking
    // cancellation. The async caller may then safely distinguish a native
    // parser (which owns only memory) from work that must be joined until its
    // subprocess tree and temporary files are cleaned up.
    let _activity = MarkitdownActivity::begin(extraction_phase)?;
    if cancelled.load(AtomicOrdering::Acquire) {
        return Err("MarkItDown conversion cancelled".to_string());
    }
    let counter = MARKITDOWN_TEMP_COUNTER.fetch_add(1, AtomicOrdering::SeqCst);
    let temp_dir =
        env::temp_dir().join(format!("maestro-markitdown-{}-{}", process::id(), counter));
    fs::create_dir_all(&temp_dir)
        .map_err(|error| format!("failed to create MarkItDown temp dir: {error}"))?;
    let extension = Path::new(file_name)
        .extension()
        .and_then(|extension| extension.to_str())
        .filter(|extension| !extension.is_empty())
        .unwrap_or("bin");
    let input_path = temp_dir.join(format!("input.{extension}"));
    let result = (|| {
        fs::write(&input_path, bytes)
            .map_err(|error| format!("failed to write MarkItDown input: {error}"))?;
        let mut last_error: Option<String> = None;
        for (command, prefix_args) in markitdown_candidates()? {
            if cancelled.load(AtomicOrdering::Acquire) {
                return Err("MarkItDown conversion cancelled".to_string());
            }
            let mut args = prefix_args;
            args.push(input_path.to_string_lossy().to_string());
            if let Some(mime_type) = mime_type {
                args.push("--mime-type".to_string());
                args.push(mime_type.to_string());
            }
            match run_markitdown_command_with_cancellation(&command, &args, cancelled) {
                Ok(output) => {
                    let text = output.trim().to_string();
                    if !text.is_empty() {
                        return Ok(Some(text));
                    }
                }
                Err(error) => {
                    last_error = Some(error);
                }
            }
        }
        if env::var("MAESTRO_MARKITDOWN_CMD")
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
        {
            return Err(format!(
                "MarkItDown extraction failed: {}",
                last_error.unwrap_or_else(|| "no output".to_string())
            ));
        }
        Ok(None)
    })();
    let _ = fs::remove_dir_all(&temp_dir);
    result
}

fn extract_document_bytes_with_cancellation(
    file_name: &str,
    mime_type: Option<&str>,
    bytes: &[u8],
    page: PageRequest,
    cancelled: &AtomicBool,
    extraction_phase: &AtomicU8,
) -> Result<DocumentExtraction, String> {
    if cancelled.load(AtomicOrdering::Acquire) {
        return Err("Document extraction cancelled".to_string());
    }
    let format = detect_format(file_name, mime_type);
    let format_label = format.unwrap_or("unknown").to_string();
    let prefer_markitdown = markitdown_preferred() && !markitdown_disabled();
    let mut extractor = "native".to_string();
    let mut sections = None;
    let mut extracted = if prefer_markitdown {
        match extract_with_markitdown(bytes, file_name, mime_type, cancelled, extraction_phase)? {
            Some(text) => {
                extractor = "markitdown".to_string();
                text
            }
            None => String::new(),
        }
    } else {
        String::new()
    };

    if extracted.is_empty() {
        if let Some(native) = format.and_then(|format| extract_from_format(format, bytes)) {
            extracted = native.text;
            sections = native.sections;
        }
    }

    if cancelled.load(AtomicOrdering::Acquire) {
        return Err("Document extraction cancelled".to_string());
    }
    if extractor != "markitdown" && should_try_markitdown(&format_label, file_name, mime_type) {
        if let Some(text) =
            extract_with_markitdown(bytes, file_name, mime_type, cancelled, extraction_phase)?
        {
            extracted = text;
            extractor = "markitdown".to_string();
            sections = None;
        }
    }

    if extracted.is_empty() && format.is_none() {
        return Err("Unsupported document format. Supported: PDF (.pdf), Word (.docx), Excel (.xlsx), PowerPoint (.pptx), and common text files.".to_string());
    }
    if extracted.is_empty() {
        return Err("Failed to extract document text.".to_string());
    }

    let max_chars = page
        .max_chars
        .unwrap_or(DEFAULT_PAGE_CHARS)
        .clamp(1, MAX_PAGE_CHARS);
    let window = text_window(&extracted, page.offset, max_chars)?;
    let next_offset = (window.end < window.total_chars).then_some(window.end);
    let mut text = String::with_capacity(window.text.len() + 256);
    if page.offset > 0 {
        text.push_str(&format!(
            "[extract_document: continuing at character {} of {}.]\n\n",
            page.offset, window.total_chars
        ));
    }
    text.push_str(window.text);
    if let Some(next) = next_offset {
        text.push_str(&format!(
            "\n\n[extract_document: returned characters {}-{} of {}. Call extract_document again with the same url and offset={} to continue.]",
            page.offset, window.end, window.total_chars, next
        ));
    }

    Ok(DocumentExtraction {
        format: format_label,
        extractor,
        text,
        offset: page.offset,
        returned_chars: window.end - page.offset,
        total_chars: window.total_chars,
        next_offset,
        sections,
    })
}

#[cfg(test)]
fn extract_document_bytes(
    file_name: &str,
    mime_type: Option<&str>,
    bytes: &[u8],
    page: PageRequest,
) -> Result<DocumentExtraction, String> {
    extract_document_bytes_with_cancellation(
        file_name,
        mime_type,
        bytes,
        page,
        &AtomicBool::new(false),
        &AtomicU8::new(EXTRACTION_NATIVE),
    )
}

#[cfg(test)]
async fn extract_document_bytes_async(
    file_name: String,
    mime_type: Option<String>,
    bytes: Vec<u8>,
    page: PageRequest,
) -> Result<DocumentExtraction, String> {
    extract_document_bytes_async_with_cancellation(file_name, mime_type, bytes, page, None).await
}

async fn extract_document_bytes_async_with_cancellation(
    file_name: String,
    mime_type: Option<String>,
    bytes: Vec<u8>,
    page: PageRequest,
    cancellation: Option<CancellationToken>,
) -> Result<DocumentExtraction, String> {
    let (cancel_on_drop, cancelled) = ExtractionCancellation::new();
    let mut cancel_on_drop = Some(cancel_on_drop);
    let extraction_phase = Arc::new(AtomicU8::new(EXTRACTION_NATIVE));
    let worker_extraction_phase = Arc::clone(&extraction_phase);
    let mut worker = tokio::task::spawn_blocking(move || {
        let result = extract_document_bytes_with_cancellation(
            &file_name,
            mime_type.as_deref(),
            &bytes,
            page,
            &cancelled,
            &worker_extraction_phase,
        );
        let _ = worker_extraction_phase.compare_exchange(
            EXTRACTION_NATIVE,
            EXTRACTION_FINISHED,
            AtomicOrdering::AcqRel,
            AtomicOrdering::Acquire,
        );
        result
    });
    let result = match cancellation {
        Some(token) => tokio::select! {
            biased;
            () = token.cancelled() => {
                let phase = extraction_phase.swap(
                    EXTRACTION_CANCELLED,
                    AtomicOrdering::AcqRel,
                );
                drop(cancel_on_drop.take());
                // Native PDF/OOXML parsing owns only its input buffer and can
                // finish harmlessly on Tokio's blocking pool. External
                // MarkItDown work must instead be joined so its process tree
                // and temporary files are reaped before shutdown continues.
                if matches!(phase, EXTRACTION_MARKITDOWN | EXTRACTION_FINISHED) {
                    let _ = worker
                        .await
                        .map_err(|error| format!("Document extraction task failed: {error}"))?;
                }
                return Err("Document extraction cancelled".to_string());
            }
            result = &mut worker => result,
        },
        None => worker.await,
    };
    drop(cancel_on_drop);
    result.map_err(|error| format!("Document extraction task failed: {error}"))?
}

#[cfg(test)]
pub async fn extract_document(args: Value) -> ToolResult {
    extract_document_with_cancellation(args, None).await
}

pub async fn extract_document_with_cancellation(
    args: Value,
    cancellation: Option<CancellationToken>,
) -> ToolResult {
    let parsed: ExtractDocumentArgs = match serde_json::from_value(args) {
        Ok(val) => val,
        Err(err) => {
            return ToolResult::failure(format!("Invalid extract_document arguments: {err}"));
        }
    };

    let url = match reqwest::Url::parse(parsed.url.trim()) {
        Ok(url) => url,
        Err(_) => return ToolResult::failure(format!("Invalid URL: {}", parsed.url)),
    };

    if let Err(err) = net_guard::validate_fetch_url(&url) {
        return ToolResult::failure(err);
    }

    // Resolve-then-pin and re-validate every redirect hop before connecting.
    // A bare `reqwest::Client` here (scheme check only, default redirect
    // policy) is what let this tool reach cloud metadata endpoints
    // (`169.254.169.254`) via an attacker-controlled URL; see the
    // `net_guard` module docs for why every hop must be validated, not just
    // the first.
    let fetch = net_guard::fetch_with_validated_redirects(
        url.clone(),
        DOWNLOAD_TIMEOUT,
        net_guard::DEFAULT_USER_AGENT,
    );
    let response = match cancellation.as_ref() {
        Some(token) => tokio::select! {
            biased;
            () = token.cancelled() => {
                return cancelled_extract_document_result();
            }
            response = fetch => response,
        },
        None => fetch.await,
    };
    let response = match response {
        Ok(resp) => resp,
        Err(err) => {
            return ToolResult::failure(format!("Failed to download document: {err}"));
        }
    };

    if !response.status().is_success() {
        return ToolResult::failure(format!(
            "Unable to download document ({} {})",
            response.status(),
            response
                .status()
                .canonical_reason()
                .unwrap_or("Unknown status")
        ));
    }

    if let Some(len) = response.content_length() {
        if len as usize > MAX_DOWNLOAD_BYTES {
            return ToolResult::failure(format!(
                "Document is too large ({:.1}MB). Maximum supported size is 50MB.",
                (len as f64) / (1024.0 * 1024.0)
            ));
        }
    }

    let headers = response.headers().clone();
    let content_type = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_string());
    let content_disposition = headers
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .map(std::string::ToString::to_string);

    let body = response.bytes();
    let bytes = match cancellation.as_ref() {
        Some(token) => tokio::select! {
            biased;
            () = token.cancelled() => {
                return cancelled_extract_document_result();
            }
            bytes = body => bytes,
        },
        None => body.await,
    };
    let bytes = match bytes {
        Ok(b) => b.to_vec(),
        Err(err) => {
            return ToolResult::failure(format!("Failed to read document bytes: {err}"));
        }
    };

    if bytes.len() > MAX_DOWNLOAD_BYTES {
        return ToolResult::failure(format!(
            "Document is too large ({:.1}MB). Maximum supported size is 50MB.",
            (bytes.len() as f64) / (1024.0 * 1024.0)
        ));
    }

    let file_name = parse_content_disposition(content_disposition.as_deref())
        .unwrap_or_else(|| guess_filename_from_url(&url));
    let size_bytes = bytes.len();

    let extraction = match extract_document_bytes_async_with_cancellation(
        file_name.clone(),
        content_type.clone(),
        bytes,
        PageRequest {
            offset: parsed.offset.unwrap_or(0),
            max_chars: parsed.max_chars,
        },
        cancellation.clone(),
    )
    .await
    {
        Ok(extraction) => extraction,
        Err(_)
            if cancellation
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled) =>
        {
            return cancelled_extract_document_result();
        }
        Err(error) => {
            return ToolResult::failure(error);
        }
    };

    let details = serde_json::json!({
        "url": url.to_string(),
        "fileName": file_name,
        "mimeType": content_type,
        "format": extraction.format,
        "extractor": extraction.extractor,
        "sizeBytes": size_bytes,
        "offset": extraction.offset,
        "returnedChars": extraction.returned_chars,
        "totalChars": extraction.total_chars,
        "nextOffset": extraction.next_offset,
        "truncated": extraction.next_offset.is_some(),
        "sections": extraction.sections.as_ref().map(|sections| serde_json::json!({
            "kind": sections.kind,
            "count": sections.count,
            "withoutText": sections.without_text,
        })),
    });

    ToolResult::success(extraction.text).with_details(details)
}

fn cancelled_extract_document_result() -> ToolResult {
    ToolResult::failure("extract_document cancelled")
        .with_details(serde_json::json!({"cancelled": true}))
}

#[cfg(test)]
mod tests {
    use super::*;

    static MARKITDOWN_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    // ========================================================================
    // ExtractDocumentArgs Deserialization Tests
    // ========================================================================

    #[test]
    fn test_args_deserialize_minimal() {
        let json = serde_json::json!({
            "url": "https://example.com/doc.pdf"
        });
        let args: ExtractDocumentArgs = serde_json::from_value(json).unwrap();
        assert_eq!(args.url, "https://example.com/doc.pdf");
        assert!(args.max_chars.is_none());
    }

    #[test]
    fn test_args_deserialize_with_max_chars() {
        let json = serde_json::json!({
            "url": "https://example.com/doc.pdf",
            "max_chars": 5000
        });
        let args: ExtractDocumentArgs = serde_json::from_value(json).unwrap();
        assert_eq!(args.max_chars, Some(5000));
    }

    #[test]
    fn test_args_deserialize_camel_case_alias() {
        let json = serde_json::json!({
            "url": "https://example.com/doc.pdf",
            "maxChars": 10000
        });
        let args: ExtractDocumentArgs = serde_json::from_value(json).unwrap();
        assert_eq!(args.max_chars, Some(10000));
    }

    // ========================================================================
    // guess_filename_from_url Tests
    // ========================================================================

    #[test]
    fn test_guess_filename_simple() {
        let url = reqwest::Url::parse("https://example.com/report.pdf").unwrap();
        assert_eq!(guess_filename_from_url(&url), "report.pdf");
    }

    #[test]
    fn test_guess_filename_with_path() {
        let url = reqwest::Url::parse("https://example.com/docs/2024/report.docx").unwrap();
        assert_eq!(guess_filename_from_url(&url), "report.docx");
    }

    #[test]
    fn test_guess_filename_trailing_slash() {
        let url = reqwest::Url::parse("https://example.com/files/").unwrap();
        assert_eq!(guess_filename_from_url(&url), "files");
    }

    #[test]
    fn test_guess_filename_no_path() {
        let url = reqwest::Url::parse("https://example.com/").unwrap();
        assert_eq!(guess_filename_from_url(&url), "document");
    }

    // ========================================================================
    // parse_content_disposition Tests
    // ========================================================================

    #[test]
    fn test_parse_content_disposition_quoted() {
        let header = r#"attachment; filename="report.pdf""#;
        assert_eq!(
            parse_content_disposition(Some(header)),
            Some("report.pdf".to_string())
        );
    }

    #[test]
    fn test_parse_content_disposition_unquoted() {
        let header = "attachment; filename=report.pdf";
        assert_eq!(
            parse_content_disposition(Some(header)),
            Some("report.pdf".to_string())
        );
    }

    #[test]
    fn test_parse_content_disposition_utf8() {
        let header = "attachment; filename*=UTF-8''report%20final.pdf";
        assert_eq!(
            parse_content_disposition(Some(header)),
            Some("report final.pdf".to_string())
        );
    }

    #[test]
    fn test_parse_content_disposition_none() {
        assert_eq!(parse_content_disposition(None), None);
    }

    #[test]
    fn test_parse_content_disposition_no_filename() {
        let header = "attachment";
        assert_eq!(parse_content_disposition(Some(header)), None);
    }

    // ========================================================================
    // decode_xml_entities Tests
    // ========================================================================

    #[test]
    fn test_decode_xml_entities_basic() {
        assert_eq!(ooxml::decode_entities("&lt;div&gt;"), "<div>");
    }

    #[test]
    fn test_decode_xml_entities_ampersand() {
        assert_eq!(ooxml::decode_entities("A &amp; B"), "A & B");
    }

    #[test]
    fn test_decode_xml_entities_quotes() {
        assert_eq!(
            ooxml::decode_entities("&quot;hello&quot; &apos;world&apos;"),
            "\"hello\" 'world'"
        );
    }

    #[test]
    fn test_decode_xml_entities_mixed() {
        assert_eq!(
            ooxml::decode_entities("x &lt; y &amp;&amp; y &gt; z"),
            "x < y && y > z"
        );
    }

    #[test]
    fn test_decode_xml_entities_none() {
        assert_eq!(ooxml::decode_entities("plain text"), "plain text");
    }

    // ========================================================================
    // detect_format Tests
    // ========================================================================

    #[test]
    fn test_detect_format_pdf_by_extension() {
        assert_eq!(detect_format("report.pdf", None), Some("pdf"));
    }

    #[test]
    fn test_detect_format_pdf_by_mime() {
        assert_eq!(
            detect_format("unknown", Some("application/pdf")),
            Some("pdf")
        );
    }

    #[test]
    fn test_detect_format_docx_by_extension() {
        assert_eq!(detect_format("document.docx", None), Some("docx"));
    }

    #[test]
    fn test_detect_format_docx_by_mime() {
        assert_eq!(
            detect_format(
                "doc",
                Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document")
            ),
            Some("docx")
        );
    }

    #[test]
    fn test_detect_format_pptx() {
        assert_eq!(detect_format("slides.pptx", None), Some("pptx"));
    }

    #[test]
    fn test_detect_format_xlsx() {
        assert_eq!(detect_format("data.xlsx", None), Some("xlsx"));
    }

    #[test]
    fn test_detect_format_text_extensions() {
        assert_eq!(detect_format("readme.txt", None), Some("text"));
        assert_eq!(detect_format("readme.md", None), Some("text"));
        assert_eq!(detect_format("data.csv", None), Some("text"));
        assert_eq!(detect_format("config.json", None), Some("text"));
        assert_eq!(detect_format("config.yaml", None), Some("text"));
        assert_eq!(detect_format("config.yml", None), Some("text"));
        assert_eq!(detect_format("data.xml", None), Some("text"));
    }

    #[test]
    fn test_detect_format_text_by_mime() {
        assert_eq!(detect_format("unknown", Some("text/plain")), Some("text"));
        assert_eq!(detect_format("unknown", Some("text/html")), Some("text"));
    }

    #[test]
    fn test_detect_format_unknown() {
        assert_eq!(detect_format("unknown.bin", None), None);
        assert_eq!(detect_format("image.png", Some("image/png")), None);
    }

    #[test]
    fn test_detect_format_case_insensitive() {
        assert_eq!(detect_format("DOC.PDF", None), Some("pdf"));
        assert_eq!(detect_format("DOC.DOCX", None), Some("docx"));
    }

    // ========================================================================
    // extract_text_file Tests
    // ========================================================================

    #[test]
    fn test_extract_text_file_utf8() {
        let bytes = b"Hello, World!";
        assert_eq!(extract_text_file(bytes), "Hello, World!");
    }

    #[test]
    fn test_extract_text_file_multiline() {
        let bytes = b"Line 1\nLine 2\nLine 3";
        assert_eq!(extract_text_file(bytes), "Line 1\nLine 2\nLine 3");
    }

    #[test]
    fn test_extract_text_file_unicode() {
        let text = "こんにちは World 🌍";
        let bytes = text.as_bytes();
        assert_eq!(extract_text_file(bytes), text);
    }

    // ========================================================================
    // extract_from_format Tests
    // ========================================================================

    #[test]
    fn test_extract_from_format_text() {
        let bytes = b"Hello from text file";
        let result = extract_from_format("text", bytes).map(|extracted| extracted.text);
        assert_eq!(result, Some("Hello from text file".to_string()));
    }

    #[test]
    fn test_extract_from_format_unknown() {
        let bytes = b"some data";
        let result = extract_from_format("unknown_format", bytes);
        assert!(result.is_none());
    }

    // ========================================================================
    // Structured extraction and paging Tests
    // ========================================================================

    /// Build a minimal single-font PDF. `None` pages have no content stream,
    /// standing in for a scanned page with no text layer.
    fn minimal_pdf(pages: &[Option<&str>]) -> Vec<u8> {
        let font_id = 3;
        let first_page_id = 4;
        let mut objects: Vec<String> = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            {
                let kids = (0..pages.len())
                    .map(|i| format!("{} 0 R", first_page_id + i * 2))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("<< /Type /Pages /Kids [{kids}] /Count {} >>", pages.len())
            },
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
                .to_string(),
        ];
        for (index, page) in pages.iter().enumerate() {
            let content_id = first_page_id + index * 2 + 1;
            objects.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 {font_id} 0 R >> >> /Contents {content_id} 0 R >>"
            ));
            let stream = page
                .map(|text| format!("BT /F1 12 Tf 72 720 Td ({text}) Tj ET"))
                .unwrap_or_default();
            objects.push(format!(
                "<< /Length {} >>\nstream\n{stream}\nendstream",
                stream.len()
            ));
        }
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::with_capacity(objects.len());
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
        }
        let xref_offset = pdf.len();
        let mut xref = format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1);
        for offset in offsets {
            xref.push_str(&format!("{offset:010} 00000 n \n"));
        }
        xref.push_str(&format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n",
            objects.len() + 1
        ));
        pdf.extend_from_slice(xref.as_bytes());
        pdf
    }

    #[test]
    fn test_pdf_extraction_marks_pages_and_pages_without_text() {
        let pdf = minimal_pdf(&[Some("Hello page one"), None, Some("Closing page")]);
        let extracted = extract_from_format("pdf", &pdf).expect("pdf extracts");
        let page_two = format!("## Page 2\n\n{PDF_PAGE_WITHOUT_TEXT_NOTE}");
        assert!(
            extracted.text.starts_with("## Page 1\n\n"),
            "{}",
            extracted.text
        );
        assert!(
            extracted.text.contains("Hello page one"),
            "{}",
            extracted.text
        );
        assert!(extracted.text.contains(&page_two), "{}", extracted.text);
        assert!(
            extracted.text.contains("## Page 3\n\nClosing page"),
            "{}",
            extracted.text
        );
        assert_eq!(
            extracted.sections,
            Some(SectionSummary {
                kind: "page",
                count: 3,
                without_text: vec![2],
            })
        );
    }

    #[test]
    fn test_pdf_pages_to_markdown_tidies_blank_runs() {
        let pages = vec![
            "  \n\nFirst line\nSecond line\n\n\n\nAfter gap\n\n".to_string(),
            "\u{c}\n  \n".to_string(),
        ];
        let extracted = pdf_pages_to_markdown(&pages).expect("pages render");
        assert_eq!(
            extracted.text,
            format!(
                "## Page 1\n\nFirst line\nSecond line\n\nAfter gap\n\n## Page 2\n\n{PDF_PAGE_WITHOUT_TEXT_NOTE}"
            )
        );
        assert!(pdf_pages_to_markdown(&[]).is_none());
    }

    #[test]
    fn test_office_formats_route_to_structured_markdown() {
        let docx = extract_from_format("docx", &ooxml::tests::sample_docx()).expect("docx");
        assert!(docx.text.starts_with("# Quarterly Report\n\n# Summary"));
        assert!(docx.text.contains("| Name | Qty |\n| --- | --- |"));
        let pptx = extract_from_format("pptx", &ooxml::tests::sample_pptx()).expect("pptx");
        assert!(pptx.text.starts_with("## Slide 1: Launch Plan"));
        let xlsx = extract_from_format("xlsx", &ooxml::tests::sample_xlsx(1, 1)).expect("xlsx");
        assert!(
            xlsx.text
                .starts_with("## Sheet: Totals & Notes\n\n| Region |")
        );
    }

    #[test]
    fn test_text_window_returns_everything_when_it_fits() {
        let window = text_window("abc\ndef", 0, 100).unwrap();
        assert_eq!(
            window,
            TextWindow {
                text: "abc\ndef",
                end: 7,
                total_chars: 7
            }
        );
        let tail = text_window("abc\ndef", 4, 100).unwrap();
        assert_eq!(tail.text, "def");
        assert_eq!(tail.end, 7);
    }

    #[test]
    fn test_text_window_accepts_empty_document_at_offset_zero() {
        let window = text_window("", 0, 100).unwrap();
        assert_eq!(
            window,
            TextWindow {
                text: "",
                end: 0,
                total_chars: 0
            }
        );
        assert!(text_window("", 1, 100).is_err());
    }

    #[test]
    fn test_text_window_snaps_to_line_break_near_the_limit() {
        let text = "0123456789\nabcdefghij\nKLMNOPQRST";
        // Limit 25 lands mid-third-line; the last break (after char 22) is
        // within the final fifth, so the window ends on the line boundary.
        let window = text_window(text, 0, 25).unwrap();
        assert_eq!(window.text, "0123456789\nabcdefghij\n");
        assert_eq!(window.end, 22);
        // A break far from the limit is not used; the window is cut exactly.
        let exact = text_window(text, 0, 9).unwrap();
        assert_eq!(exact.text, "012345678");
        assert_eq!(exact.end, 9);
    }

    #[test]
    fn test_text_window_counts_characters_not_bytes() {
        let text = "日本語のテキスト";
        let window = text_window(text, 2, 3).unwrap();
        assert_eq!(window.text, "語のテ");
        assert_eq!(window.end, 5);
        assert_eq!(window.total_chars, 8);
    }

    #[test]
    fn test_text_window_rejects_offset_past_end() {
        let error = text_window("short", 5, 10).unwrap_err();
        assert!(error.contains("offset 5 is past the end"), "{error}");
        assert!(error.contains("5 characters"), "{error}");
    }

    #[test]
    fn test_text_window_pages_reassemble_the_document() {
        let text = (0..500)
            .map(|i| format!("| row {i} | value {} |", i * 7))
            .collect::<Vec<_>>()
            .join("\n");
        let mut offset = 0;
        let mut rebuilt = String::new();
        let mut pages = 0;
        loop {
            let window = text_window(&text, offset, 1_000).unwrap();
            assert!(window.text.chars().count() <= 1_000);
            rebuilt.push_str(window.text);
            pages += 1;
            if window.end == window.total_chars {
                break;
            }
            assert!(window.text.ends_with('\n'), "pages end on row boundaries");
            offset = window.end;
        }
        assert_eq!(rebuilt, text);
        assert!(pages > 1);
    }

    #[tokio::test]
    async fn test_extract_document_bytes_pages_with_offset_and_reports_next_offset() {
        let _env_lock = MARKITDOWN_ENV_LOCK.lock().await;
        env::remove_var("MAESTRO_MARKITDOWN_PREFER");
        let xlsx = ooxml::tests::sample_xlsx(200, 3);

        let full = extract_document_bytes("data.xlsx", None, &xlsx, PageRequest::default())
            .expect("full extraction");
        assert_eq!(full.format, "xlsx");
        assert_eq!(full.offset, 0);
        assert!(full.next_offset.is_none());
        assert_eq!(full.returned_chars, full.total_chars);
        assert!(!full.text.contains("[extract_document:"));
        assert_eq!(
            full.sections.as_ref().map(|s| (s.kind, s.count)),
            Some(("sheet", 2))
        );

        let first = extract_document_bytes(
            "data.xlsx",
            None,
            &xlsx,
            PageRequest {
                offset: 0,
                max_chars: Some(1_000),
            },
        )
        .expect("first page");
        let next = first.next_offset.expect("more content remains");
        assert_eq!(first.total_chars, full.total_chars);
        assert_eq!(first.returned_chars, next);
        assert!(first.text.starts_with("## Sheet: Totals & Notes"));
        assert!(
            first.text.ends_with(&format!(
                "[extract_document: returned characters 0-{next} of {}. Call extract_document again with the same url and offset={next} to continue.]",
                full.total_chars
            )),
            "{}",
            first.text
        );

        let second = extract_document_bytes(
            "data.xlsx",
            None,
            &xlsx,
            PageRequest {
                offset: next,
                max_chars: Some(1_000_000),
            },
        )
        .expect("second page");
        assert!(second.next_offset.is_none());
        assert_eq!(second.offset, next);
        assert!(second.text.starts_with(&format!(
            "[extract_document: continuing at character {next} of {}.]\n\n",
            full.total_chars
        )));
        assert!(
            second
                .text
                .ends_with("## Sheet: Empty (hidden)\n\n_(Empty sheet.)_")
        );

        let past_end = extract_document_bytes(
            "data.xlsx",
            None,
            &xlsx,
            PageRequest {
                offset: full.total_chars,
                max_chars: None,
            },
        );
        assert!(past_end.is_err());
    }

    #[test]
    fn test_args_deserialize_offset() {
        let args: ExtractDocumentArgs = serde_json::from_value(serde_json::json!({
            "url": "https://example.com/doc.pdf",
            "offset": 30000,
            "maxChars": 5000
        }))
        .unwrap();
        assert_eq!(args.offset, Some(30_000));
        assert_eq!(args.max_chars, Some(5_000));
    }

    #[tokio::test]
    async fn test_extract_document_bytes_uses_configured_markitdown_for_html() {
        let _env_lock = MARKITDOWN_ENV_LOCK.lock().await;
        let script_path = env::temp_dir().join(format!(
            "maestro-tui-fake-markitdown-{}-{}.sh",
            process::id(),
            MARKITDOWN_TEMP_COUNTER.fetch_add(1, AtomicOrdering::SeqCst)
        ));
        fs::write(
            &script_path,
            "printf '# Converted by MarkItDown\\n\\nRust TUI body from fake CLI'",
        )
        .expect("fake MarkItDown script should be written");
        env::set_var("MAESTRO_MARKITDOWN_CMD", "sh");
        env::set_var(
            "MAESTRO_MARKITDOWN_ARGS",
            script_path.to_string_lossy().to_string(),
        );
        env::remove_var("MAESTRO_MARKITDOWN");
        env::remove_var("MAESTRO_MARKITDOWN_PREFER");

        let output = extract_document_bytes(
            "brief.html",
            Some("text/html"),
            b"<html><body><h1>Ignored native HTML</h1></body></html>",
            PageRequest::default(),
        )
        .expect("MarkItDown extraction should succeed");

        assert_eq!(output.format, "text");
        assert_eq!(output.extractor, "markitdown");
        assert!(output.text.contains("# Converted by MarkItDown"));

        env::remove_var("MAESTRO_MARKITDOWN_CMD");
        env::remove_var("MAESTRO_MARKITDOWN_ARGS");
        let _ = fs::remove_file(script_path);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_does_not_join_pure_native_extraction() {
        let _env_lock = MARKITDOWN_ENV_LOCK.lock().await;
        let suffix = MARKITDOWN_TEMP_COUNTER.fetch_add(1, AtomicOrdering::SeqCst);
        let script_path = env::temp_dir().join(format!(
            "maestro-tui-detached-native-markitdown-{}-{suffix}.sh",
            process::id()
        ));
        let pid_path = env::temp_dir().join(format!(
            "maestro-tui-detached-native-markitdown-{}-{suffix}.pid",
            process::id()
        ));
        fs::write(
            &script_path,
            format!("printf '%s' \"$$\" > '{}'\n", pid_path.to_string_lossy()),
        )
        .expect("fallback MarkItDown fixture should be written");
        env::set_var("MAESTRO_MARKITDOWN_CMD", "sh");
        env::set_var(
            "MAESTRO_MARKITDOWN_ARGS",
            script_path.to_string_lossy().to_string(),
        );
        env::remove_var("MAESTRO_MARKITDOWN");
        env::remove_var("MAESTRO_MARKITDOWN_PREFER");

        NATIVE_TEST_EXTRACTION_ACTIVE.store(false, AtomicOrdering::Release);
        let cancellation = CancellationToken::new();
        let extraction = tokio::spawn(extract_document_bytes_async_with_cancellation(
            "fixture.slow-native-test.html".to_string(),
            None,
            b"fixture".to_vec(),
            PageRequest::default(),
            Some(cancellation.clone()),
        ));

        tokio::time::timeout(Duration::from_secs(1), async {
            while !NATIVE_TEST_EXTRACTION_ACTIVE.load(AtomicOrdering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("native extraction must publish that it started");

        let started = Instant::now();
        cancellation.cancel();
        let result = tokio::time::timeout(Duration::from_millis(250), extraction)
            .await
            .expect("cancellation must not join pure native parsing")
            .expect("extraction task must not panic");
        assert!(result.is_err());
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "native parsing held cancellation open"
        );

        tokio::time::sleep(Duration::from_millis(2200)).await;
        assert!(
            !pid_path.exists(),
            "detached native extraction started MarkItDown after cancellation"
        );
        env::remove_var("MAESTRO_MARKITDOWN_CMD");
        env::remove_var("MAESTRO_MARKITDOWN_ARGS");
        let _ = fs::remove_file(script_path);
    }

    /// Poll a fake-MarkItDown pid file until it contains a complete,
    /// parseable pid. The scripts publish with `printf '%s' "$$" > file`,
    /// which makes the file visible to `exists()` before the pid is written
    /// into it, so waiting on existence alone can race an empty read.
    #[cfg(unix)]
    async fn read_pid_file_when_ready(pid_path: &Path) -> i32 {
        let started = Instant::now();
        loop {
            if let Ok(contents) = fs::read_to_string(pid_path) {
                if let Ok(pid) = contents.trim().parse() {
                    return pid;
                }
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "fake MarkItDown should record its pid in {}",
                pid_path.display()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn test_async_extraction_is_responsive_and_cancels_markitdown_process() {
        let _env_lock = MARKITDOWN_ENV_LOCK.lock().await;
        let suffix = MARKITDOWN_TEMP_COUNTER.fetch_add(1, AtomicOrdering::SeqCst);
        let script_path = env::temp_dir().join(format!(
            "maestro-tui-cancellable-markitdown-{}-{suffix}.sh",
            process::id()
        ));
        let pid_path = env::temp_dir().join(format!(
            "maestro-tui-cancellable-markitdown-{}-{suffix}.pid",
            process::id()
        ));
        fs::write(
            &script_path,
            format!(
                "printf '%s' \"$$\" > '{}'\nwhile :; do sleep 1; done\n",
                pid_path.to_string_lossy()
            ),
        )
        .expect("cancellable fake MarkItDown script should be written");
        env::set_var("MAESTRO_MARKITDOWN_CMD", "sh");
        env::set_var(
            "MAESTRO_MARKITDOWN_ARGS",
            script_path.to_string_lossy().to_string(),
        );
        env::remove_var("MAESTRO_MARKITDOWN");
        env::remove_var("MAESTRO_MARKITDOWN_PREFER");

        let extraction = tokio::spawn(extract_document_bytes_async(
            "brief.html".to_string(),
            Some("text/html".to_string()),
            b"<html><body>blocking conversion</body></html>".to_vec(),
            PageRequest::default(),
        ));

        let pid = read_pid_file_when_ready(&pid_path).await;
        // On this single-thread runtime, observing readiness while conversion
        // is pending proves extraction yielded to the caller. Keep the peer
        // blocked until cancellation; startup and OS scheduling are not part
        // of that invariant. The converter's watchdog bounds a regression
        // that performs conversion synchronously on the runtime thread.
        assert!(
            !extraction.is_finished(),
            "the runtime must observe readiness before conversion finishes"
        );

        extraction.abort();
        assert!(matches!(extraction.await, Err(error) if error.is_cancelled()));

        let cancellation_started = Instant::now();
        while unsafe { libc::kill(pid, 0) } == 0
            && cancellation_started.elapsed() < Duration::from_secs(1)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_ne!(
            unsafe { libc::kill(pid, 0) },
            0,
            "aborting extraction left the MarkItDown process running"
        );

        env::remove_var("MAESTRO_MARKITDOWN_CMD");
        env::remove_var("MAESTRO_MARKITDOWN_ARGS");
        let _ = fs::remove_file(script_path);
        let _ = fs::remove_file(pid_path);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_waits_for_markitdown_process_reaping() {
        let _env_lock = MARKITDOWN_ENV_LOCK.lock().await;
        let suffix = MARKITDOWN_TEMP_COUNTER.fetch_add(1, AtomicOrdering::SeqCst);
        let script_path = env::temp_dir().join(format!(
            "maestro-tui-reaped-markitdown-{}-{suffix}.sh",
            process::id()
        ));
        let pid_path = env::temp_dir().join(format!(
            "maestro-tui-reaped-markitdown-{}-{suffix}.pid",
            process::id()
        ));
        fs::write(
            &script_path,
            format!(
                "printf '%s' \"$$\" > '{}'\nsleep 60\n",
                pid_path.to_string_lossy()
            ),
        )
        .expect("cancellable fake MarkItDown script should be written");
        env::set_var("MAESTRO_MARKITDOWN_CMD", "sh");
        env::set_var(
            "MAESTRO_MARKITDOWN_ARGS",
            script_path.to_string_lossy().to_string(),
        );
        env::remove_var("MAESTRO_MARKITDOWN");
        env::remove_var("MAESTRO_MARKITDOWN_PREFER");

        let cancellation = CancellationToken::new();
        let extraction = tokio::spawn(extract_document_bytes_async_with_cancellation(
            "brief.html".to_string(),
            Some("text/html".to_string()),
            b"<html><body>blocking conversion</body></html>".to_vec(),
            PageRequest::default(),
            Some(cancellation.clone()),
        ));

        let pid = read_pid_file_when_ready(&pid_path).await;

        cancellation.cancel();
        let result = tokio::time::timeout(Duration::from_secs(2), extraction)
            .await
            .expect("cancellation must not wait for the conversion timeout")
            .expect("extraction task should join");

        assert!(
            result.is_err(),
            "cancelled extraction unexpectedly succeeded"
        );
        assert_ne!(
            unsafe { libc::kill(pid, 0) },
            0,
            "extraction returned before the MarkItDown process was reaped"
        );

        env::remove_var("MAESTRO_MARKITDOWN_CMD");
        env::remove_var("MAESTRO_MARKITDOWN_ARGS");
        let _ = fs::remove_file(script_path);
        let _ = fs::remove_file(pid_path);
    }

    #[cfg(unix)]
    #[test]
    fn exited_markitdown_launcher_closes_inherited_descendant_pipes() {
        let _env_lock = MARKITDOWN_ENV_LOCK.blocking_lock();
        let suffix = MARKITDOWN_TEMP_COUNTER.fetch_add(1, AtomicOrdering::SeqCst);
        let script_path = env::temp_dir().join(format!(
            "maestro-tui-descendant-markitdown-{}-{suffix}.sh",
            process::id()
        ));
        let pid_path = env::temp_dir().join(format!(
            "maestro-tui-descendant-markitdown-{}-{suffix}.pid",
            process::id()
        ));
        fs::write(
            &script_path,
            format!(
                "sleep 60 &\nprintf '%s' \"$!\" > '{}'\nexit 0\n",
                pid_path.to_string_lossy()
            ),
        )
        .expect("descendant fake MarkItDown script should be written");

        let started = Instant::now();
        let result = run_markitdown_command("sh", &[script_path.to_string_lossy().to_string()]);
        let elapsed = started.elapsed();
        let descendant_pid: i32 = fs::read_to_string(&pid_path)
            .expect("fake MarkItDown should record its descendant pid")
            .parse()
            .expect("recorded descendant pid should parse");

        assert!(
            result.is_ok(),
            "successful launcher should preserve its result: {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "inherited descendant pipes delayed completion for {elapsed:?}"
        );
        let stopped_started = Instant::now();
        while unsafe { libc::kill(descendant_pid, 0) } == 0
            && stopped_started.elapsed() < Duration::from_secs(1)
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(
            unsafe { libc::kill(descendant_pid, 0) },
            0,
            "exited launcher left inherited-pipe descendant {descendant_pid} running"
        );

        let _ = fs::remove_file(script_path);
        let _ = fs::remove_file(pid_path);
    }

    #[cfg(windows)]
    #[test]
    fn test_windows_markitdown_job_kills_descendant_on_close() {
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        let dir = tempfile::tempdir().expect("temporary directory should be created");
        let pid_file = dir.path().join("child.pid");
        let mut command = Command::new("powershell.exe");
        command
            .arg("-NoProfile")
            .arg("-Command")
            .arg(
                "$child = Start-Process powershell.exe -ArgumentList '-NoProfile', \
                 '-Command', 'Start-Sleep -Seconds 60' -PassThru; \
                 Set-Content -LiteralPath $env:MAESTRO_TEST_PID_FILE -Value $child.Id; \
                 $child.WaitForExit()",
            )
            .env("MAESTRO_TEST_PID_FILE", &pid_file)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_markitdown_process(&mut command);
        let mut child = command
            .spawn()
            .expect("suspended MarkItDown fixture should spawn");
        let job =
            MarkitdownJobObject::assign(&child).expect("fixture should enter kill-on-close job");
        resume_markitdown_process(&child).expect("fixture should resume after job assignment");

        for _ in 0..200 {
            if pid_file.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let descendant_pid: u32 = fs::read_to_string(&pid_file)
            .expect("launcher must publish its descendant pid")
            .trim()
            .parse()
            .expect("published descendant pid should parse");

        // Closing a JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE handle must terminate
        // the PowerShell descendant as well as the suspended launcher.
        drop(job);
        let mut descendant_stopped = false;
        for _ in 0..200 {
            // SAFETY: this opens a query-only handle to the pid published by
            // the test descendant. A null result means the process no longer
            // exists; any live handle is closed immediately.
            let handle =
                unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, descendant_pid) };
            if handle.is_null() {
                descendant_stopped = true;
                break;
            }
            drop(OwnedWindowsHandle(handle));
            thread::sleep(Duration::from_millis(10));
        }
        let _ = child.wait();
        assert!(
            descendant_stopped,
            "closing the MarkItDown job left descendant {descendant_pid} running"
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_windows_markitdown_helper_closes_job_before_joining_inherited_pipes() {
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        let _env_lock = MARKITDOWN_ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().expect("temporary directory should be created");
        let pid_file = dir.path().join("child.pid");
        env::set_var("MAESTRO_TEST_PID_FILE", &pid_file);
        let args = vec![
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "$child = Start-Process powershell.exe -ArgumentList '-NoProfile', \
             '-Command', 'Start-Sleep -Seconds 60' -NoNewWindow -PassThru; \
             Set-Content -LiteralPath $env:MAESTRO_TEST_PID_FILE -Value $child.Id; \
             exit 0"
                .to_string(),
        ];

        let started = Instant::now();
        let result = run_markitdown_command_with_cancellation(
            "powershell.exe",
            &args,
            &AtomicBool::new(false),
        );
        let elapsed = started.elapsed();
        env::remove_var("MAESTRO_TEST_PID_FILE");

        assert!(result.is_ok(), "unexpected MarkItDown result: {result:?}");
        assert!(
            elapsed < Duration::from_secs(2),
            "helper waited {elapsed:?} on pipes inherited by an exited launcher's descendant"
        );
        let descendant_pid: u32 = fs::read_to_string(&pid_file)
            .expect("launcher must publish its descendant pid")
            .trim()
            .parse()
            .expect("published descendant pid should parse");
        // SAFETY: this opens a query-only handle to the pid published by the
        // test descendant. A null result means the process no longer exists.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, descendant_pid) };
        if !handle.is_null() {
            drop(OwnedWindowsHandle(handle));
            panic!(
                "actual MarkItDown helper left inherited-pipe descendant {descendant_pid} running"
            );
        }
    }

    #[test]
    fn test_split_command_args_preserves_quoted_values() {
        let args = split_command_args(Some(
            r#"--script "/tmp/maestro markitdown/run.py" --flag='quoted value' --mime text/html"#
                .to_string(),
        ))
        .expect("quoted MarkItDown args should parse");

        assert_eq!(
            args,
            vec![
                "--script",
                "/tmp/maestro markitdown/run.py",
                "--flag=quoted value",
                "--mime",
                "text/html",
            ]
        );
    }

    #[test]
    fn test_split_command_args_preserves_windows_backslashes() {
        let quoted = split_command_args(Some(
            r#""C:\Program Files\markitdown\runner.py" --cache "C:\Users\me\Cache Dir""#
                .to_string(),
        ))
        .expect("quoted Windows MarkItDown args should parse");
        assert_eq!(
            quoted,
            vec![
                r"C:\Program Files\markitdown\runner.py",
                "--cache",
                r"C:\Users\me\Cache Dir",
            ]
        );

        let unquoted = split_command_args(Some(
            r"C:\tools\markitdown.py --flag C:\tmp\document.html".to_string(),
        ))
        .expect("unquoted Windows MarkItDown args should parse");
        assert_eq!(
            unquoted,
            vec![r"C:\tools\markitdown.py", "--flag", r"C:\tmp\document.html",]
        );
    }

    #[test]
    fn test_split_command_args_rejects_unterminated_quotes() {
        let error = split_command_args(Some(r#"--flag "unterminated"#.to_string()))
            .expect_err("unterminated MarkItDown args should fail");

        assert!(error.contains("Unterminated quote"));
    }

    #[test]
    fn test_run_markitdown_command_drains_large_stdout_before_waiting() {
        let script_path = env::temp_dir().join(format!(
            "maestro-tui-large-markitdown-{}-{}.sh",
            process::id(),
            MARKITDOWN_TEMP_COUNTER.fetch_add(1, AtomicOrdering::SeqCst)
        ));
        fs::write(
            &script_path,
            "i=0\nwhile [ \"$i\" -lt 12000 ]; do\n  printf '0123456789abcdef0123456789abcdef\\n'\n  i=$((i + 1))\ndone\nprintf 'MARKITDOWN_DONE\\n'\n",
        )
        .expect("large fake MarkItDown script should be written");

        let output = run_markitdown_command("sh", &[script_path.to_string_lossy().to_string()])
            .expect("large MarkItDown output should not deadlock");

        assert!(output.len() > 200_000);
        assert!(output.contains("MARKITDOWN_DONE"));

        let _ = fs::remove_file(script_path);
    }

    #[test]
    fn test_run_markitdown_command_timeout_does_not_wait_for_inherited_pipe_handles() {
        let script_path = env::temp_dir().join(format!(
            "maestro-tui-timeout-markitdown-{}-{}.sh",
            process::id(),
            MARKITDOWN_TEMP_COUNTER.fetch_add(1, AtomicOrdering::SeqCst)
        ));
        fs::write(&script_path, "sleep 28 &\nsleep 28\n")
            .expect("timeout fake MarkItDown script should be written");

        let started = Instant::now();
        let error = run_markitdown_command("sh", &[script_path.to_string_lossy().to_string()])
            .expect_err("timed-out MarkItDown command should fail");
        let elapsed = started.elapsed();

        assert_eq!(error, "MarkItDown conversion timed out");
        assert!(
            elapsed < MARKITDOWN_EXTRACT_TIMEOUT + Duration::from_secs(4),
            "timeout path waited for inherited pipe handles for {elapsed:?}"
        );

        let _ = fs::remove_file(script_path);
    }

    // ========================================================================
    // MAX_DOWNLOAD_BYTES Constant
    // ========================================================================

    #[test]
    fn test_max_download_bytes() {
        // 50MB = 50 * 1024 * 1024 = 52,428,800 bytes
        assert_eq!(MAX_DOWNLOAD_BYTES, 52_428_800);
    }

    // ========================================================================
    // extract_document SSRF regression tests
    //
    // Before this fix, `extract_document` validated only the URL scheme and
    // built a bare `reqwest::Client` with default redirect-following, so a
    // model- or content-supplied URL could reach cloud IAM credentials at
    // http://169.254.169.254/latest/meta-data/iam/security-credentials/
    // with zero prompts (requires_approval: false). These tests exercise
    // the full `extract_document` entry point (not just the shared
    // `net_guard` helpers) to prove the tool itself now fails closed.
    // ========================================================================

    #[tokio::test]
    async fn test_extract_document_rejects_cloud_metadata_ip() {
        let args = serde_json::json!({
            "url": "http://169.254.169.254/latest/meta-data/iam/security-credentials/"
        });
        let result = extract_document(args).await;
        assert!(!result.success);
        let error = result.error.unwrap_or_default();
        assert!(
            error.contains("blocked network target"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn test_extract_document_rejects_cgnat_address() {
        // 100.64.0.0/10 (RFC 6598 Shared Address Space / CGNAT). This
        // fleet's Tailscale network lives in this range, and it is not
        // covered by `Ipv4Addr::is_private()`.
        let args = serde_json::json!({
            "url": "http://100.64.0.5/internal-service"
        });
        let result = extract_document(args).await;
        assert!(!result.success);
        let error = result.error.unwrap_or_default();
        assert!(
            error.contains("blocked network target"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn test_extract_document_rejects_ipv4_mapped_ipv6_metadata_address() {
        // Classic bypass: encode the blocked IPv4 target as an IPv4-mapped
        // IPv6 literal to route around a naive IPv4-only blocklist.
        let args = serde_json::json!({
            "url": "http://[::ffff:169.254.169.254]/latest/meta-data/"
        });
        let result = extract_document(args).await;
        assert!(!result.success);
        let error = result.error.unwrap_or_default();
        assert!(
            error.contains("blocked network target"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn test_extract_document_rejects_non_http_scheme() {
        let args = serde_json::json!({
            "url": "file:///etc/passwd"
        });
        let result = extract_document(args).await;
        assert!(!result.success);
        let error = result.error.unwrap_or_default();
        assert!(
            error.contains("Unsupported URL scheme"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn test_extract_document_redirect_to_metadata_ip_is_rejected() {
        // Mirrors the ordering inside `net_guard::fetch_with_validated_redirects`:
        // a redirect `Location` pointing at cloud metadata parses fine as a
        // URL, but must be rejected on resolution, before it is ever
        // connected to. See `net_guard::tests` for the direct coverage of
        // that loop; this asserts the same invariant holds when reached
        // through `extract_document`'s public entry point.
        let current = reqwest::Url::parse("https://example.com/start").unwrap();
        let next = super::net_guard::redirect_target_url(
            &current,
            "http://169.254.169.254/latest/meta-data/iam/security-credentials/",
        )
        .expect("redirect target should parse: scheme/host validation doesn't resolve IPs");
        let error = super::net_guard::resolve_public_endpoint(&next)
            .await
            .expect_err("redirect target must be rejected before connecting");
        assert!(error.contains("blocked network target"));
    }
}

//! Structure-preserving Markdown conversion for Office Open XML documents.
//!
//! DOCX, PPTX, and XLSX packages are ZIP archives of XML parts. Earlier
//! extraction stripped every tag and returned a flat text dump, which lost
//! headings, list nesting, table cells, slide boundaries, and sheet names.
//! This module walks the XML parts with a small event tokenizer and renders:
//!
//! - DOCX: `#` headings from `Title`/`Heading N` paragraph styles (resolved
//!   through `word/styles.xml` when present), `-`/`1.` list items from
//!   `w:numPr` (ordered vs. bullet resolved through `word/numbering.xml`),
//!   and pipe tables for `w:tbl`.
//! - PPTX: one `## Slide N: Title` section per slide in presentation order,
//!   body placeholder paragraphs as nested bullets, DrawingML tables as pipe
//!   tables, and speaker notes.
//! - XLSX: one `## Sheet: Name` section per worksheet in workbook order, each
//!   rendered as a bounded pipe table with an explicit truncation note.
//!
//! The tokenizer only understands the subset of XML that OOXML writers emit
//! (elements, attributes, text, comments, processing instructions, CDATA).
//! It is not a validating parser and never resolves external entities.

use std::collections::HashMap;
use std::io::{Cursor, Read};

use zip::ZipArchive;

use super::{Extracted, SectionSummary};

/// Upper bound on the decompressed size of one XML part. Protects against
/// ZIP bombs; a part larger than this is parsed up to the bound.
const MAX_PART_BYTES: u64 = 128 * 1024 * 1024;
/// Maximum non-empty rows rendered per worksheet.
pub(super) const MAX_SHEET_ROWS: usize = 2_000;
/// Maximum columns rendered per worksheet.
pub(super) const MAX_SHEET_COLS: usize = 50;

type Archive<'a> = ZipArchive<Cursor<&'a [u8]>>;

// ---------------------------------------------------------------------------
// XML tokenizer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum XmlEvent<'a> {
    Start {
        name: &'a str,
        attrs: &'a str,
        self_closing: bool,
    },
    End {
        name: &'a str,
    },
    Text(&'a str),
}

struct XmlEvents<'a> {
    src: &'a str,
    pos: usize,
}

fn xml_events(src: &str) -> XmlEvents<'_> {
    XmlEvents { src, pos: 0 }
}

/// Element name without its namespace prefix (`w:p` -> `p`).
fn local_name(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

impl<'a> Iterator for XmlEvents<'a> {
    type Item = XmlEvent<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let rest = self.src.get(self.pos..)?;
            if rest.is_empty() {
                return None;
            }
            if !rest.starts_with('<') {
                let end = rest.find('<').unwrap_or(rest.len());
                self.pos += end;
                return Some(XmlEvent::Text(&rest[..end]));
            }
            if let Some(body) = rest.strip_prefix("<!--") {
                let end = body.find("-->").map_or(rest.len(), |i| 4 + i + 3);
                self.pos += end;
                continue;
            }
            if let Some(body) = rest.strip_prefix("<![CDATA[") {
                let len = body.find("]]>").unwrap_or(body.len());
                self.pos += 9 + len + 3.min(body.len() - len);
                return Some(XmlEvent::Text(&body[..len]));
            }
            let Some(close) = rest.find('>') else {
                self.pos = self.src.len();
                return None;
            };
            self.pos += close + 1;
            let tag = &rest[1..close];
            if tag.starts_with('?') || tag.starts_with('!') {
                continue;
            }
            if let Some(name) = tag.strip_prefix('/') {
                return Some(XmlEvent::End { name: name.trim() });
            }
            let self_closing = tag.ends_with('/');
            let tag = tag.trim_end_matches('/');
            let (name, attrs) = match tag.find(|c: char| c.is_ascii_whitespace()) {
                Some(split) => (&tag[..split], &tag[split..]),
                None => (tag, ""),
            };
            return Some(XmlEvent::Start {
                name,
                attrs,
                self_closing,
            });
        }
    }
}

/// Look up an attribute by its exact qualified name and decode its value.
fn attr(attrs: &str, wanted: &str) -> Option<String> {
    let mut rest = attrs;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return None;
        }
        let eq = rest.find('=')?;
        let name = rest[..eq].trim();
        rest = rest[eq + 1..].trim_start();
        let quote = rest.chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        let value_end = rest[1..].find(quote)? + 1;
        let value = &rest[1..value_end];
        rest = &rest[value_end + 1..];
        if name == wanted {
            return Some(decode_entities(value));
        }
    }
}

/// Decode the five predefined XML entities and numeric character references
/// in a single pass, so `&amp;lt;` stays the literal text `&lt;`.
pub(super) fn decode_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let decoded = rest.find(';').filter(|&semi| semi <= 12).and_then(|semi| {
            let entity = &rest[1..semi];
            let ch = match entity {
                "lt" => Some('<'),
                "gt" => Some('>'),
                "amp" => Some('&'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ => entity
                    .strip_prefix("#x")
                    .or_else(|| entity.strip_prefix("#X"))
                    .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                    .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                    .and_then(char::from_u32),
            }?;
            Some((ch, semi))
        });
        match decoded {
            Some((ch, semi)) => {
                out.push(ch);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn open_archive(bytes: &[u8]) -> Option<Archive<'_>> {
    ZipArchive::new(Cursor::new(bytes)).ok()
}

fn read_part(archive: &mut Archive<'_>, name: &str) -> Option<String> {
    let file = archive.by_name(name).ok()?;
    let mut raw = Vec::new();
    file.take(MAX_PART_BYTES).read_to_end(&mut raw).ok()?;
    Some(String::from_utf8_lossy(&raw).into_owned())
}

/// Parse a `.rels` part into `Id -> (Type, Target)`.
fn read_relationships(
    archive: &mut Archive<'_>,
    rels_path: &str,
) -> HashMap<String, (String, String)> {
    let mut rels = HashMap::new();
    let Some(xml) = read_part(archive, rels_path) else {
        return rels;
    };
    for event in xml_events(&xml) {
        if let XmlEvent::Start { name, attrs, .. } = event {
            if local_name(name) == "Relationship" {
                if let (Some(id), Some(target)) = (attr(attrs, "Id"), attr(attrs, "Target")) {
                    rels.insert(id, (attr(attrs, "Type").unwrap_or_default(), target));
                }
            }
        }
    }
    rels
}

/// Resolve a relationship target against the directory of its source part.
fn resolve_target(base_dir: &str, target: &str) -> String {
    if let Some(absolute) = target.strip_prefix('/') {
        return absolute.to_string();
    }
    let mut parts: Vec<&str> = base_dir.split('/').filter(|p| !p.is_empty()).collect();
    for segment in target.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Render rows as a GitHub-flavored Markdown pipe table. The first row is
/// the header; ragged rows are padded to the widest row.
pub(super) fn render_table(rows: &[Vec<String>]) -> String {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 {
        return String::new();
    }
    let render_row = |row: &[String]| {
        let mut line = String::from("|");
        for index in 0..columns {
            let cell = row.get(index).map_or("", String::as_str);
            let cell = cell
                .trim()
                .replace('\\', "\\\\")
                .replace('|', "\\|")
                .replace("\r\n", "<br>")
                .replace('\n', "<br>");
            line.push(' ');
            line.push_str(&cell);
            line.push_str(" |");
        }
        line
    };
    let mut lines = Vec::with_capacity(rows.len() + 1);
    lines.push(render_row(&rows[0]));
    lines.push(format!("|{}", " --- |".repeat(columns)));
    for row in &rows[1..] {
        lines.push(render_row(row));
    }
    lines.join("\n")
}

/// Rows of a table being assembled while walking XML.
#[derive(Default)]
struct TableBuilder {
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    cell: Vec<String>,
    span: usize,
}

impl TableBuilder {
    fn finish_cell(&mut self) {
        self.row.push(self.cell.join("\n"));
        for _ in 1..self.span.max(1) {
            self.row.push(String::new());
        }
        self.cell.clear();
        self.span = 1;
    }

    fn finish_row(&mut self) {
        if !self.row.is_empty() {
            self.rows.push(std::mem::take(&mut self.row));
        }
    }

    /// Flatten a nested table into one cell of its parent table.
    fn flatten(&self) -> String {
        self.rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| collapse_whitespace(cell))
                    .collect::<Vec<_>>()
                    .join("; ")
            })
            .collect::<Vec<_>>()
            .join(" / ")
    }
}

// ---------------------------------------------------------------------------
// DOCX
// ---------------------------------------------------------------------------

enum DocxBlock {
    Paragraph(String),
    Heading(usize, String),
    ListItem {
        level: usize,
        ordered: bool,
        text: String,
    },
    Table(Vec<Vec<String>>),
}

fn heading_level_from_name(name: &str) -> Option<usize> {
    let lower = name.trim().to_ascii_lowercase();
    if lower == "title" {
        return Some(1);
    }
    let digits = lower
        .strip_prefix("heading")?
        .trim_start_matches([' ', '_', '-']);
    digits
        .parse::<usize>()
        .ok()
        .filter(|level| *level >= 1)
        .map(|level| level.min(6))
}

/// Map paragraph style ids to Markdown heading levels from `styles.xml`.
fn docx_heading_styles(styles_xml: &str) -> HashMap<String, usize> {
    let mut levels = HashMap::new();
    let mut style_id: Option<String> = None;
    let mut style_name: Option<String> = None;
    let mut outline: Option<usize> = None;
    for event in xml_events(styles_xml) {
        match event {
            XmlEvent::Start { name, attrs, .. } => match local_name(name) {
                "style" => {
                    style_id = attr(attrs, "w:styleId");
                    style_name = None;
                    outline = None;
                }
                "name" if style_id.is_some() => style_name = attr(attrs, "w:val"),
                "outlineLvl" if style_id.is_some() => {
                    outline = attr(attrs, "w:val").and_then(|v| v.parse::<usize>().ok());
                }
                _ => {}
            },
            XmlEvent::End { name } if local_name(name) == "style" => {
                if let Some(id) = style_id.take() {
                    let level = style_name
                        .as_deref()
                        .and_then(heading_level_from_name)
                        .or_else(|| heading_level_from_name(&id))
                        .or_else(|| outline.filter(|l| *l < 6).map(|l| l + 1));
                    if let Some(level) = level {
                        levels.insert(id, level);
                    }
                }
            }
            _ => {}
        }
    }
    levels
}

/// Map `(numId, ilvl)` to whether the list level is ordered.
fn docx_ordered_levels(numbering_xml: &str) -> HashMap<(String, usize), bool> {
    let mut abstract_formats: HashMap<String, HashMap<usize, bool>> = HashMap::new();
    let mut num_to_abstract: HashMap<String, String> = HashMap::new();
    let mut current_abstract: Option<String> = None;
    let mut current_level: Option<usize> = None;
    let mut current_num: Option<String> = None;
    for event in xml_events(numbering_xml) {
        match event {
            XmlEvent::Start { name, attrs, .. } => match local_name(name) {
                "abstractNum" => current_abstract = attr(attrs, "w:abstractNumId"),
                "lvl" => current_level = attr(attrs, "w:ilvl").and_then(|v| v.parse().ok()),
                "numFmt" => {
                    if let (Some(abstract_id), Some(level), Some(format)) = (
                        current_abstract.as_ref(),
                        current_level,
                        attr(attrs, "w:val"),
                    ) {
                        abstract_formats
                            .entry(abstract_id.clone())
                            .or_default()
                            .insert(level, !matches!(format.as_str(), "bullet" | "none"));
                    }
                }
                "num" => current_num = attr(attrs, "w:numId"),
                "abstractNumId" => {
                    if let (Some(num), Some(abstract_id)) =
                        (current_num.as_ref(), attr(attrs, "w:val"))
                    {
                        num_to_abstract.insert(num.clone(), abstract_id);
                    }
                }
                _ => {}
            },
            XmlEvent::End { name } => match local_name(name) {
                "abstractNum" => current_abstract = None,
                "lvl" => current_level = None,
                "num" => current_num = None,
                _ => {}
            },
            XmlEvent::Text(_) => {}
        }
    }
    let mut ordered = HashMap::new();
    for (num, abstract_id) in num_to_abstract {
        if let Some(levels) = abstract_formats.get(&abstract_id) {
            for (level, is_ordered) in levels {
                ordered.insert((num.clone(), *level), *is_ordered);
            }
        }
    }
    ordered
}

fn docx_document_to_markdown(
    document_xml: &str,
    heading_styles: &HashMap<String, usize>,
    ordered_levels: &HashMap<(String, usize), bool>,
) -> String {
    let mut blocks: Vec<DocxBlock> = Vec::new();
    let mut tables: Vec<TableBuilder> = Vec::new();
    let mut para_depth = 0usize;
    let mut para_text = String::new();
    let mut para_style: Option<String> = None;
    let mut num_id: Option<String> = None;
    let mut list_level = 0usize;
    let mut in_text = false;
    let mut run_depth = 0usize;
    // mc:Fallback repeats mc:Choice content (e.g. text boxes); skip it.
    let mut fallback_depth = 0usize;

    for event in xml_events(document_xml) {
        if fallback_depth > 0 {
            match event {
                XmlEvent::Start {
                    name,
                    self_closing: false,
                    ..
                } if local_name(name) == "Fallback" => fallback_depth += 1,
                XmlEvent::End { name } if local_name(name) == "Fallback" => fallback_depth -= 1,
                _ => {}
            }
            continue;
        }
        match event {
            XmlEvent::Start {
                name,
                attrs,
                self_closing,
            } => match local_name(name) {
                "Fallback" if !self_closing => fallback_depth = 1,
                "p" if !self_closing => {
                    if para_depth == 0 {
                        para_text.clear();
                        para_style = None;
                        num_id = None;
                        list_level = 0;
                    } else {
                        // Text-box paragraph nested inside a run.
                        para_text.push('\n');
                    }
                    para_depth += 1;
                }
                "pStyle" if para_depth == 1 => para_style = attr(attrs, "w:val"),
                "numId" if para_depth == 1 => num_id = attr(attrs, "w:val"),
                "ilvl" if para_depth == 1 => {
                    list_level = attr(attrs, "w:val")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                }
                "r" if !self_closing => run_depth += 1,
                "t" if !self_closing => in_text = true,
                "tab" if run_depth > 0 => para_text.push('\t'),
                "br" | "cr" if run_depth > 0 => para_text.push('\n'),
                "noBreakHyphen" if run_depth > 0 => para_text.push('-'),
                "tbl" if !self_closing => tables.push(TableBuilder::default()),
                "tr" if !self_closing => {
                    if let Some(table) = tables.last_mut() {
                        table.row.clear();
                    }
                }
                "tc" if !self_closing => {
                    if let Some(table) = tables.last_mut() {
                        table.cell.clear();
                        table.span = 1;
                    }
                }
                "gridSpan" => {
                    if let Some(table) = tables.last_mut() {
                        table.span = attr(attrs, "w:val")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(1)
                            .clamp(1, MAX_SHEET_COLS);
                    }
                }
                _ => {}
            },
            XmlEvent::Text(text) if in_text && para_depth > 0 => {
                para_text.push_str(&decode_entities(text));
            }
            XmlEvent::Text(_) => {}
            XmlEvent::End { name } => match local_name(name) {
                "t" => in_text = false,
                "r" => run_depth = run_depth.saturating_sub(1),
                "p" if para_depth > 0 => {
                    para_depth -= 1;
                    if para_depth > 0 {
                        continue;
                    }
                    let text = para_text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    if let Some(table) = tables.last_mut() {
                        table.cell.push(text.to_string());
                        continue;
                    }
                    let heading = para_style.as_deref().and_then(|style| {
                        heading_styles
                            .get(style)
                            .copied()
                            .or_else(|| heading_level_from_name(style))
                    });
                    let list_num = num_id.as_deref().filter(|id| *id != "0");
                    blocks.push(match (heading, list_num) {
                        (Some(level), _) => DocxBlock::Heading(level, collapse_whitespace(text)),
                        (None, Some(id)) => DocxBlock::ListItem {
                            level: list_level.min(8),
                            ordered: ordered_levels
                                .get(&(id.to_string(), list_level))
                                .copied()
                                .unwrap_or(false),
                            text: collapse_whitespace(text),
                        },
                        (None, None) => DocxBlock::Paragraph(text.to_string()),
                    });
                }
                "tc" => {
                    if let Some(table) = tables.last_mut() {
                        table.finish_cell();
                    }
                }
                "tr" => {
                    if let Some(table) = tables.last_mut() {
                        table.finish_row();
                    }
                }
                "tbl" => {
                    if let Some(table) = tables.pop() {
                        if let Some(parent) = tables.last_mut() {
                            let flattened = table.flatten();
                            if !flattened.is_empty() {
                                parent.cell.push(flattened);
                            }
                        } else if !table.rows.is_empty() {
                            blocks.push(DocxBlock::Table(table.rows));
                        }
                    }
                }
                _ => {}
            },
        }
    }

    let mut out = String::new();
    let mut previous_was_list = false;
    for block in blocks {
        let is_list = matches!(block, DocxBlock::ListItem { .. });
        if !out.is_empty() {
            out.push_str(if is_list && previous_was_list {
                "\n"
            } else {
                "\n\n"
            });
        }
        match block {
            DocxBlock::Paragraph(text) => out.push_str(&text),
            DocxBlock::Heading(level, text) => {
                out.push_str(&"#".repeat(level));
                out.push(' ');
                out.push_str(&text);
            }
            DocxBlock::ListItem {
                level,
                ordered,
                text,
            } => {
                out.push_str(&"  ".repeat(level));
                out.push_str(if ordered { "1. " } else { "- " });
                out.push_str(&text);
            }
            DocxBlock::Table(rows) => out.push_str(&render_table(&rows)),
        }
        previous_was_list = is_list;
    }
    out
}

pub(super) fn docx_to_markdown(bytes: &[u8]) -> Option<Extracted> {
    let mut archive = open_archive(bytes)?;
    let document = read_part(&mut archive, "word/document.xml")?;
    let heading_styles = read_part(&mut archive, "word/styles.xml")
        .map(|xml| docx_heading_styles(&xml))
        .unwrap_or_default();
    let ordered_levels = read_part(&mut archive, "word/numbering.xml")
        .map(|xml| docx_ordered_levels(&xml))
        .unwrap_or_default();
    let text = docx_document_to_markdown(&document, &heading_styles, &ordered_levels);
    Some(Extracted {
        text,
        sections: None,
    })
}

// ---------------------------------------------------------------------------
// PPTX
// ---------------------------------------------------------------------------

enum SlideBlock {
    Bullets(Vec<(usize, String)>),
    Text(Vec<String>),
    Table(Vec<Vec<String>>),
}

#[derive(Default)]
struct SlideContent {
    title: Option<String>,
    blocks: Vec<SlideBlock>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ShapeRole {
    Title,
    Body,
    Text,
    Skip,
}

fn placeholder_role(attrs: &str) -> ShapeRole {
    match attr(attrs, "type").as_deref() {
        Some("title" | "ctrTitle") => ShapeRole::Title,
        None | Some("body" | "obj") => ShapeRole::Body,
        Some("sldNum" | "dt" | "ftr" | "hdr" | "sldImg") => ShapeRole::Skip,
        Some(_) => ShapeRole::Text,
    }
}

fn parse_slide(xml: &str) -> SlideContent {
    let mut slide = SlideContent::default();
    let mut role = ShapeRole::Text;
    let mut in_shape = false;
    let mut shape_paras: Vec<(usize, String)> = Vec::new();
    let mut para: Option<(usize, String)> = None;
    let mut in_text = false;
    let mut tables: Vec<TableBuilder> = Vec::new();

    for event in xml_events(xml) {
        match event {
            XmlEvent::Start {
                name,
                attrs,
                self_closing,
            } => match local_name(name) {
                "sp" if !self_closing => {
                    in_shape = true;
                    role = ShapeRole::Text;
                    shape_paras.clear();
                }
                "ph" if in_shape => role = placeholder_role(attrs),
                "p" if !self_closing => para = Some((0, String::new())),
                "pPr" => {
                    if let Some((level, _)) = para.as_mut() {
                        *level = attr(attrs, "lvl").and_then(|v| v.parse().ok()).unwrap_or(0);
                    }
                }
                "t" if !self_closing => in_text = true,
                "br" => {
                    if let Some((_, text)) = para.as_mut() {
                        text.push('\n');
                    }
                }
                "tbl" if !self_closing => tables.push(TableBuilder::default()),
                "tr" if !self_closing => {
                    if let Some(table) = tables.last_mut() {
                        table.row.clear();
                    }
                }
                "tc" if !self_closing => {
                    if let Some(table) = tables.last_mut() {
                        table.cell.clear();
                        table.span = attr(attrs, "gridSpan")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(1)
                            .clamp(1, MAX_SHEET_COLS);
                    }
                }
                _ => {}
            },
            XmlEvent::Text(text) if in_text => {
                if let Some((_, para_text)) = para.as_mut() {
                    para_text.push_str(&decode_entities(text));
                }
            }
            XmlEvent::Text(_) => {}
            XmlEvent::End { name } => match local_name(name) {
                "t" => in_text = false,
                "p" => {
                    let Some((level, text)) = para.take() else {
                        continue;
                    };
                    let text = text.trim().to_string();
                    if text.is_empty() {
                        continue;
                    }
                    if let Some(table) = tables.last_mut() {
                        table.cell.push(text);
                    } else if in_shape {
                        shape_paras.push((level, text));
                    } else {
                        slide.blocks.push(SlideBlock::Text(vec![text]));
                    }
                }
                "tc" => {
                    if let Some(table) = tables.last_mut() {
                        table.finish_cell();
                    }
                }
                "tr" => {
                    if let Some(table) = tables.last_mut() {
                        table.finish_row();
                    }
                }
                "tbl" => {
                    if let Some(table) = tables.pop() {
                        if !table.rows.is_empty() {
                            slide.blocks.push(SlideBlock::Table(table.rows));
                        }
                    }
                }
                "sp" if in_shape => {
                    in_shape = false;
                    let paras = std::mem::take(&mut shape_paras);
                    if paras.is_empty() {
                        continue;
                    }
                    match role {
                        ShapeRole::Title if slide.title.is_none() => {
                            let title = paras
                                .iter()
                                .map(|(_, text)| text.as_str())
                                .collect::<Vec<_>>()
                                .join(" ");
                            slide.title = Some(collapse_whitespace(&title));
                        }
                        ShapeRole::Body => slide.blocks.push(SlideBlock::Bullets(paras)),
                        ShapeRole::Title | ShapeRole::Text => slide.blocks.push(SlideBlock::Text(
                            paras.into_iter().map(|(_, t)| t).collect(),
                        )),
                        ShapeRole::Skip => {}
                    }
                }
                _ => {}
            },
        }
    }
    slide
}

fn render_slide_blocks(blocks: &[SlideBlock]) -> Vec<String> {
    blocks
        .iter()
        .map(|block| match block {
            SlideBlock::Bullets(items) => items
                .iter()
                .map(|(level, text)| {
                    format!(
                        "{}- {}",
                        "  ".repeat((*level).min(8)),
                        collapse_whitespace(text)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
            SlideBlock::Text(lines) => lines.join("\n"),
            SlideBlock::Table(rows) => render_table(rows),
        })
        .collect()
}

fn slide_number_from_name(name: &str) -> Option<u32> {
    name.rsplit('/')
        .next()?
        .strip_prefix("slide")?
        .strip_suffix(".xml")?
        .parse()
        .ok()
}

/// Slide part paths in presentation order, falling back to file numbering.
fn pptx_slide_paths(archive: &mut Archive<'_>) -> Vec<String> {
    let rels = read_relationships(archive, "ppt/_rels/presentation.xml.rels");
    let mut ordered = Vec::new();
    if let Some(presentation) = read_part(archive, "ppt/presentation.xml") {
        for event in xml_events(&presentation) {
            if let XmlEvent::Start { name, attrs, .. } = event {
                if local_name(name) == "sldId" {
                    if let Some((_, target)) = attr(attrs, "r:id").and_then(|id| rels.get(&id)) {
                        let path = resolve_target("ppt", target);
                        if archive.by_name(&path).is_ok() {
                            ordered.push(path);
                        }
                    }
                }
            }
        }
    }
    if !ordered.is_empty() {
        return ordered;
    }
    let mut names: Vec<String> = archive
        .file_names()
        .filter(|name| name.starts_with("ppt/slides/slide") && name.ends_with(".xml"))
        .map(str::to_string)
        .collect();
    names.sort_by_key(|name| {
        (
            slide_number_from_name(name).unwrap_or(u32::MAX),
            name.clone(),
        )
    });
    names
}

fn pptx_notes_path(archive: &mut Archive<'_>, slide_path: &str) -> Option<String> {
    let (dir, file) = slide_path.rsplit_once('/')?;
    let rels = read_relationships(archive, &format!("{dir}/_rels/{file}.rels"));
    rels.values()
        .find(|(kind, _)| kind.ends_with("/notesSlide"))
        .map(|(_, target)| resolve_target(dir, target))
}

pub(super) fn pptx_to_markdown(bytes: &[u8]) -> Option<Extracted> {
    let mut archive = open_archive(bytes)?;
    let slide_paths = pptx_slide_paths(&mut archive);
    if slide_paths.is_empty() {
        return None;
    }
    let mut sections = Vec::with_capacity(slide_paths.len());
    let mut without_text = Vec::new();
    for (index, path) in slide_paths.iter().enumerate() {
        let number = index + 1;
        let slide = read_part(&mut archive, path)
            .map(|xml| parse_slide(&xml))
            .unwrap_or_default();
        let notes = pptx_notes_path(&mut archive, path)
            .and_then(|notes_path| read_part(&mut archive, &notes_path))
            .map(|xml| parse_slide(&xml))
            .map(|notes| {
                notes
                    .blocks
                    .into_iter()
                    .filter(|block| matches!(block, SlideBlock::Bullets(_)))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let mut section = match &slide.title {
            Some(title) => format!("## Slide {number}: {title}"),
            None => format!("## Slide {number}"),
        };
        let body = render_slide_blocks(&slide.blocks);
        if slide.title.is_none() && body.is_empty() {
            without_text.push(number);
            section.push_str("\n\n_(No text on this slide.)_");
        }
        for block in body {
            section.push_str("\n\n");
            section.push_str(&block);
        }
        if !notes.is_empty() {
            let notes_text = notes
                .iter()
                .flat_map(|block| match block {
                    SlideBlock::Bullets(items) => items.iter().map(|(_, t)| t.clone()).collect(),
                    _ => Vec::new(),
                })
                .collect::<Vec<_>>()
                .join("\n");
            section.push_str("\n\n**Notes:** ");
            section.push_str(&notes_text);
        }
        sections.push(section);
    }
    Some(Extracted {
        text: sections.join("\n\n"),
        sections: Some(SectionSummary {
            kind: "slide",
            count: slide_paths.len(),
            without_text,
        }),
    })
}

// ---------------------------------------------------------------------------
// XLSX
// ---------------------------------------------------------------------------

fn shared_strings(xml: &str) -> Vec<String> {
    let mut strings = Vec::new();
    let mut current: Option<String> = None;
    let mut in_text = false;
    let mut phonetic_depth = 0usize;
    for event in xml_events(xml) {
        match event {
            XmlEvent::Start {
                name, self_closing, ..
            } => match local_name(name) {
                "si" if self_closing => strings.push(String::new()),
                "si" => current = Some(String::new()),
                "rPh" if !self_closing => phonetic_depth += 1,
                "t" if !self_closing => in_text = true,
                _ => {}
            },
            XmlEvent::End { name } => match local_name(name) {
                "si" => strings.push(current.take().unwrap_or_default()),
                "rPh" => phonetic_depth = phonetic_depth.saturating_sub(1),
                "t" => in_text = false,
                _ => {}
            },
            XmlEvent::Text(text) if in_text && phonetic_depth == 0 => {
                if let Some(current) = current.as_mut() {
                    current.push_str(&decode_entities(text));
                }
            }
            XmlEvent::Text(_) => {}
        }
    }
    strings
}

/// Zero-based column index from an `A1`-style cell reference.
fn column_index(cell_ref: &str) -> Option<usize> {
    let letters: String = cell_ref
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect();
    if letters.is_empty() {
        return None;
    }
    let mut index = 0usize;
    for ch in letters.chars() {
        index = index
            .checked_mul(26)?
            .checked_add((ch.to_ascii_uppercase() as u8 - b'A') as usize + 1)?;
    }
    Some(index - 1)
}

fn column_letters(mut index: usize) -> String {
    let mut letters = Vec::new();
    loop {
        letters.push((b'A' + (index % 26) as u8) as char);
        if index < 26 {
            break;
        }
        index = index / 26 - 1;
    }
    letters.iter().rev().collect()
}

struct SheetGrid {
    rows: Vec<Vec<(usize, String)>>,
    total_rows: usize,
    min_col: usize,
    max_col: usize,
}

fn parse_sheet(xml: &str, strings: &[String]) -> SheetGrid {
    let mut grid = SheetGrid {
        rows: Vec::new(),
        total_rows: 0,
        min_col: usize::MAX,
        max_col: 0,
    };
    let mut row: Vec<(usize, String)> = Vec::new();
    let mut next_col = 0usize;
    let mut cell: Option<(usize, Option<String>, String)> = None;
    let mut in_value = false;
    for event in xml_events(xml) {
        match event {
            XmlEvent::Start {
                name,
                attrs,
                self_closing,
            } => match local_name(name) {
                "row" if !self_closing => {
                    row.clear();
                    next_col = 0;
                }
                "c" => {
                    let col = attr(attrs, "r")
                        .and_then(|r| column_index(&r))
                        .unwrap_or(next_col);
                    next_col = col + 1;
                    if !self_closing {
                        cell = Some((col, attr(attrs, "t"), String::new()));
                    }
                }
                "v" | "t" if !self_closing && cell.is_some() => in_value = true,
                _ => {}
            },
            XmlEvent::Text(text) if in_value => {
                if let Some((_, _, value)) = cell.as_mut() {
                    value.push_str(&decode_entities(text));
                }
            }
            XmlEvent::Text(_) => {}
            XmlEvent::End { name } => match local_name(name) {
                "v" | "t" => in_value = false,
                "c" => {
                    let Some((col, kind, raw)) = cell.take() else {
                        continue;
                    };
                    let value = match kind.as_deref() {
                        Some("s") => raw
                            .trim()
                            .parse::<usize>()
                            .ok()
                            .and_then(|i| strings.get(i).cloned())
                            .unwrap_or_default(),
                        Some("b") => match raw.trim() {
                            "1" => "TRUE".to_string(),
                            "0" => "FALSE".to_string(),
                            other => other.to_string(),
                        },
                        _ => raw,
                    };
                    if !value.trim().is_empty() {
                        row.push((col, value));
                    }
                }
                "row" => {
                    if row.is_empty() {
                        continue;
                    }
                    grid.total_rows += 1;
                    for (col, _) in &row {
                        grid.min_col = grid.min_col.min(*col);
                        grid.max_col = grid.max_col.max(*col);
                    }
                    if grid.rows.len() < MAX_SHEET_ROWS {
                        grid.rows.push(std::mem::take(&mut row));
                    } else {
                        row.clear();
                    }
                }
                _ => {}
            },
        }
    }
    grid
}

fn render_sheet(name: &str, hidden: bool, grid: &SheetGrid) -> String {
    let mut out = format!("## Sheet: {name}");
    if hidden {
        out.push_str(" (hidden)");
    }
    if grid.rows.is_empty() {
        out.push_str("\n\n_(Empty sheet.)_");
        return out;
    }
    let first = grid.min_col;
    let total_cols = grid.max_col - first + 1;
    let shown_cols = total_cols.min(MAX_SHEET_COLS);
    let table: Vec<Vec<String>> = grid
        .rows
        .iter()
        .map(|cells| {
            let mut row = vec![String::new(); shown_cols];
            for (col, value) in cells {
                if let Some(slot) = col.checked_sub(first).and_then(|i| row.get_mut(i)) {
                    *slot = value.clone();
                }
            }
            row
        })
        .collect();
    out.push_str("\n\n");
    out.push_str(&render_table(&table));
    let mut notes = Vec::new();
    if grid.total_rows > grid.rows.len() {
        notes.push(format!(
            "showing the first {} of {} non-empty rows",
            grid.rows.len(),
            grid.total_rows
        ));
    }
    if total_cols > shown_cols {
        notes.push(format!(
            "showing columns {}–{} of {}–{}",
            column_letters(first),
            column_letters(first + shown_cols - 1),
            column_letters(first),
            column_letters(grid.max_col)
        ));
    }
    if !notes.is_empty() {
        out.push_str(&format!("\n\n_(Sheet truncated: {}.)_", notes.join("; ")));
    }
    out
}

pub(super) fn xlsx_to_markdown(bytes: &[u8]) -> Option<Extracted> {
    let mut archive = open_archive(bytes)?;
    let strings = read_part(&mut archive, "xl/sharedStrings.xml")
        .map(|xml| shared_strings(&xml))
        .unwrap_or_default();
    let rels = read_relationships(&mut archive, "xl/_rels/workbook.xml.rels");
    let mut sheets: Vec<(String, bool, String)> = Vec::new();
    if let Some(workbook) = read_part(&mut archive, "xl/workbook.xml") {
        for event in xml_events(&workbook) {
            if let XmlEvent::Start { name, attrs, .. } = event {
                if local_name(name) == "sheet" {
                    let sheet_name = attr(attrs, "name").unwrap_or_default();
                    let hidden = attr(attrs, "state").is_some_and(|s| s != "visible");
                    if let Some((_, target)) = attr(attrs, "r:id").and_then(|id| rels.get(&id)) {
                        sheets.push((sheet_name, hidden, resolve_target("xl", target)));
                    }
                }
            }
        }
    }
    if sheets.is_empty() {
        let mut names: Vec<String> = archive
            .file_names()
            .filter(|n| n.starts_with("xl/worksheets/") && n.ends_with(".xml"))
            .map(str::to_string)
            .collect();
        names.sort();
        sheets = names
            .into_iter()
            .map(|path| {
                let label = path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&path)
                    .trim_end_matches(".xml")
                    .to_string();
                (label, false, path)
            })
            .collect();
    }
    if sheets.is_empty() {
        return None;
    }
    let mut sections = Vec::with_capacity(sheets.len());
    let mut without_text = Vec::new();
    for (index, (name, hidden, path)) in sheets.iter().enumerate() {
        let grid = read_part(&mut archive, path)
            .map(|xml| parse_sheet(&xml, &strings))
            .unwrap_or(SheetGrid {
                rows: Vec::new(),
                total_rows: 0,
                min_col: 0,
                max_col: 0,
            });
        if grid.rows.is_empty() {
            without_text.push(index + 1);
        }
        sections.push(render_sheet(name, *hidden, &grid));
    }
    Some(Extracted {
        text: sections.join("\n\n"),
        sections: Some(SectionSummary {
            kind: "sheet",
            count: sheets.len(),
            without_text,
        }),
    })
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    /// Build an in-memory ZIP package from `(path, contents)` pairs.
    pub(in super::super) fn build_zip(parts: &[(&str, &str)]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (path, contents) in parts {
            writer
                .start_file(*path, SimpleFileOptions::default())
                .expect("start zip entry");
            writer
                .write_all(contents.as_bytes())
                .expect("write zip entry");
        }
        writer.finish().expect("finish zip").into_inner()
    }

    const W_NS: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;

    fn docx_paragraph(style: Option<&str>, num: Option<(&str, usize)>, text: &str) -> String {
        let mut ppr = String::new();
        if let Some(style) = style {
            ppr.push_str(&format!(r#"<w:pStyle w:val="{style}"/>"#));
        }
        if let Some((num_id, level)) = num {
            ppr.push_str(&format!(
                r#"<w:numPr><w:ilvl w:val="{level}"/><w:numId w:val="{num_id}"/></w:numPr>"#
            ));
        }
        format!(
            r#"<w:p><w:pPr>{ppr}<w:tabs><w:tab w:val="left" w:pos="720"/></w:tabs></w:pPr><w:r><w:t xml:space="preserve">{text}</w:t></w:r></w:p>"#
        )
    }

    pub(in super::super) fn sample_docx() -> Vec<u8> {
        let table = "<w:tbl><w:tblPr/><w:tr><w:tc><w:p><w:r><w:t>Name</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Qty</w:t></w:r></w:p></w:tc></w:tr><w:tr><w:tc><w:p><w:r><w:t>Widget | large</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>3</w:t></w:r></w:p><w:p><w:r><w:t>boxes</w:t></w:r></w:p></w:tc></w:tr></w:tbl>";
        let body = [
            docx_paragraph(Some("Title"), None, "Quarterly Report"),
            docx_paragraph(Some("Heading1"), None, "Summary"),
            docx_paragraph(None, None, "Revenue grew &amp; costs fell."),
            docx_paragraph(Some("ListParagraph"), Some(("1", 0)), "First bullet"),
            docx_paragraph(Some("ListParagraph"), Some(("1", 1)), "Nested bullet"),
            docx_paragraph(Some("ListParagraph"), Some(("2", 0)), "Step one"),
            docx_paragraph(Some("CustomHeading"), None, "Details"),
            table.to_string(),
            "<w:p><w:r><w:t>Deleted:</w:t></w:r><w:del><w:r><w:delText>gone</w:delText></w:r></w:del></w:p>".to_string(),
        ]
        .concat();
        let document = format!(
            r#"<?xml version="1.0"?><w:document {W_NS}><w:body>{body}</w:body></w:document>"#
        );
        let styles = format!(
            r#"<w:styles {W_NS}><w:style w:type="paragraph" w:styleId="CustomHeading"><w:name w:val="heading 2"/></w:style><w:style w:type="paragraph" w:styleId="ListParagraph"><w:name w:val="List Paragraph"/></w:style></w:styles>"#
        );
        let numbering = format!(
            r#"<w:numbering {W_NS}><w:abstractNum w:abstractNumId="10"><w:lvl w:ilvl="0"><w:numFmt w:val="bullet"/></w:lvl><w:lvl w:ilvl="1"><w:numFmt w:val="bullet"/></w:lvl></w:abstractNum><w:abstractNum w:abstractNumId="11"><w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="10"/></w:num><w:num w:numId="2"><w:abstractNumId w:val="11"/></w:num></w:numbering>"#
        );
        build_zip(&[
            ("word/document.xml", &document),
            ("word/styles.xml", &styles),
            ("word/numbering.xml", &numbering),
        ])
    }

    #[test]
    fn docx_preserves_headings_lists_and_tables() {
        let extracted = docx_to_markdown(&sample_docx()).expect("docx converts");
        let expected = "# Quarterly Report\n\n\
                        # Summary\n\n\
                        Revenue grew & costs fell.\n\n\
                        - First bullet\n  - Nested bullet\n1. Step one\n\n\
                        ## Details\n\n\
                        | Name | Qty |\n| --- | --- |\n| Widget \\| large | 3<br>boxes |\n\n\
                        Deleted:";
        assert_eq!(extracted.text, expected);
    }

    #[test]
    fn docx_heading_without_styles_part_uses_style_id() {
        let document = format!(
            "<w:document {W_NS}><w:body>{}{}</w:body></w:document>",
            docx_paragraph(Some("Heading3"), None, "Deep"),
            docx_paragraph(None, None, "Body")
        );
        let bytes = build_zip(&[("word/document.xml", &document)]);
        let extracted = docx_to_markdown(&bytes).expect("docx converts");
        assert_eq!(extracted.text, "### Deep\n\nBody");
    }

    pub(in super::super) fn sample_pptx() -> Vec<u8> {
        let p_ns = r#"xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;
        let presentation = format!(
            r#"<p:presentation {p_ns}><p:sldIdLst><p:sldId id="256" r:id="rId7"/><p:sldId id="257" r:id="rId3"/><p:sldId id="258" r:id="rId4"/></p:sldIdLst></p:presentation>"#
        );
        // Presentation order deliberately differs from file numbering.
        let rels = r#"<Relationships><Relationship Id="rId7" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide2.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/><Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="/ppt/slides/slide3.xml"/></Relationships>"#;
        let title_slide = format!(
            r#"<p:sld {p_ns}><p:cSld><p:spTree><p:sp><p:nvSpPr><p:nvPr><p:ph type="ctrTitle"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>Launch Plan</a:t></a:r></a:p></p:txBody></p:sp><p:sp><p:nvSpPr><p:nvPr><p:ph type="subTitle" idx="1"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>Q3 kickoff</a:t></a:r></a:p></p:txBody></p:sp><p:sp><p:nvSpPr><p:nvPr><p:ph type="sldNum" idx="12"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>1</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
        );
        let content_slide = format!(
            r#"<p:sld {p_ns}><p:cSld><p:spTree><p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>Milestones</a:t></a:r></a:p></p:txBody></p:sp><p:sp><p:nvSpPr><p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>Beta</a:t></a:r></a:p><a:p><a:pPr lvl="1"/><a:r><a:t>Invite 50 users</a:t></a:r></a:p></p:txBody></p:sp><p:graphicFrame><a:graphic><a:graphicData><a:tbl><a:tr><a:tc><a:txBody><a:p><a:r><a:t>Phase</a:t></a:r></a:p></a:txBody></a:tc><a:tc><a:txBody><a:p><a:r><a:t>Date</a:t></a:r></a:p></a:txBody></a:tc></a:tr><a:tr><a:tc><a:txBody><a:p><a:r><a:t>GA</a:t></a:r></a:p></a:txBody></a:tc><a:tc><a:txBody><a:p><a:r><a:t>Oct</a:t></a:r></a:p></a:txBody></a:tc></a:tr></a:tbl></a:graphicData></a:graphic></p:graphicFrame></p:spTree></p:cSld></p:sld>"#
        );
        let empty_slide = format!("<p:sld {p_ns}><p:cSld><p:spTree></p:spTree></p:cSld></p:sld>");
        let slide1_rels = r#"<Relationships><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide" Target="../notesSlides/notesSlide1.xml"/></Relationships>"#;
        let notes = format!(
            r#"<p:notes {p_ns}><p:cSld><p:spTree><p:sp><p:nvSpPr><p:nvPr><p:ph type="sldImg"/></p:nvPr></p:nvSpPr></p:sp><p:sp><p:nvSpPr><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>Mention the waitlist.</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:notes>"#
        );
        build_zip(&[
            ("ppt/presentation.xml", &presentation),
            ("ppt/_rels/presentation.xml.rels", rels),
            ("ppt/slides/slide1.xml", &content_slide),
            ("ppt/slides/slide2.xml", &title_slide),
            ("ppt/slides/slide3.xml", &empty_slide),
            ("ppt/slides/_rels/slide1.xml.rels", slide1_rels),
            ("ppt/notesSlides/notesSlide1.xml", &notes),
        ])
    }

    #[test]
    fn pptx_renders_slides_in_presentation_order() {
        let extracted = pptx_to_markdown(&sample_pptx()).expect("pptx converts");
        let expected = "## Slide 1: Launch Plan\n\n\
                        Q3 kickoff\n\n\
                        ## Slide 2: Milestones\n\n\
                        - Beta\n  - Invite 50 users\n\n\
                        | Phase | Date |\n| --- | --- |\n| GA | Oct |\n\n\
                        **Notes:** Mention the waitlist.\n\n\
                        ## Slide 3\n\n_(No text on this slide.)_";
        assert_eq!(extracted.text, expected);
        let sections = extracted.sections.expect("slide summary");
        assert_eq!(sections.kind, "slide");
        assert_eq!(sections.count, 3);
        assert_eq!(sections.without_text, vec![3]);
    }

    pub(in super::super) fn sample_xlsx(data_rows: usize, data_cols: usize) -> Vec<u8> {
        let workbook = r#"<workbook xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Totals &amp; Notes" sheetId="1" r:id="rId1"/><sheet name="Empty" sheetId="2" r:id="rId2" state="hidden"/></sheets></workbook>"#;
        let rels = r#"<Relationships><Relationship Id="rId1" Type="worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="worksheet" Target="worksheets/sheet2.xml"/></Relationships>"#;
        let shared = "<sst><si><t>Region</t></si><si><r><t>Rev</t></r><r><t>enue</t></r><rPh><t>ignored</t></rPh></si></sst>";
        let mut sheet = String::from(
            r#"<worksheet><sheetData><row r="1"><c r="B1" t="s"><v>0</v></c><c r="C1" t="s"><v>1</v></c></row>"#,
        );
        for row in 0..data_rows {
            let r = row + 2;
            sheet.push_str(&format!(r#"<row r="{r}">"#));
            for col in 0..data_cols {
                let letters = column_letters(col + 1);
                if col == 0 {
                    sheet.push_str(&format!(
                        r#"<c r="{letters}{r}" t="inlineStr"><is><t>R{row}</t></is></c>"#
                    ));
                } else {
                    sheet.push_str(&format!(r#"<c r="{letters}{r}"><v>{}</v></c>"#, row * col));
                }
            }
            sheet.push_str(&format!(
                r#"<c r="{}{r}" t="b"><v>1</v></c>"#,
                column_letters(data_cols + 1)
            ));
            sheet.push_str("</row>");
        }
        sheet.push_str("</sheetData></worksheet>");
        build_zip(&[
            ("xl/workbook.xml", workbook),
            ("xl/_rels/workbook.xml.rels", rels),
            ("xl/sharedStrings.xml", shared),
            ("xl/worksheets/sheet1.xml", &sheet),
            (
                "xl/worksheets/sheet2.xml",
                "<worksheet><sheetData/></worksheet>",
            ),
        ])
    }

    #[test]
    fn xlsx_renders_each_sheet_as_a_table() {
        let extracted = xlsx_to_markdown(&sample_xlsx(2, 2)).expect("xlsx converts");
        let expected = "## Sheet: Totals & Notes\n\n\
                        | Region | Revenue |  |\n| --- | --- | --- |\n\
                        | R0 | 0 | TRUE |\n| R1 | 1 | TRUE |\n\n\
                        ## Sheet: Empty (hidden)\n\n_(Empty sheet.)_";
        assert_eq!(extracted.text, expected);
        let sections = extracted.sections.expect("sheet summary");
        assert_eq!(sections.kind, "sheet");
        assert_eq!(sections.count, 2);
        assert_eq!(sections.without_text, vec![2]);
    }

    #[test]
    fn xlsx_bounds_rows_and_columns_with_a_note() {
        let extracted = xlsx_to_markdown(&sample_xlsx(MAX_SHEET_ROWS + 5, MAX_SHEET_COLS + 3))
            .expect("xlsx converts");
        let header_cols = extracted
            .text
            .lines()
            .find(|line| line.starts_with("| Region"))
            .expect("header row")
            .matches(" |")
            .count();
        assert_eq!(header_cols, MAX_SHEET_COLS);
        assert!(
            extracted.text.contains(&format!(
                "_(Sheet truncated: showing the first {MAX_SHEET_ROWS} of {} non-empty rows; showing columns B–{} of B–{}.)_",
                MAX_SHEET_ROWS + 6,
                column_letters(MAX_SHEET_COLS),
                column_letters(MAX_SHEET_COLS + 4),
            )),
            "missing truncation note in tail: {}",
            &extracted.text[extracted.text.len().saturating_sub(300)..]
        );
    }

    #[test]
    fn column_reference_round_trips() {
        for (letters, index) in [("A", 0), ("Z", 25), ("AA", 26), ("AZ", 51), ("ZZ", 701)] {
            assert_eq!(column_index(letters), Some(index));
            assert_eq!(column_letters(index), letters);
        }
        assert_eq!(column_index("B12"), Some(1));
        assert_eq!(column_index("12"), None);
    }

    #[test]
    fn decode_entities_handles_numeric_and_double_escaped_references() {
        assert_eq!(decode_entities("a &amp;lt; b"), "a &lt; b");
        assert_eq!(decode_entities("&#65;&#x42;&#X43;"), "ABC");
        assert_eq!(decode_entities("Tom & Jerry"), "Tom & Jerry");
        assert_eq!(decode_entities("&bogus;"), "&bogus;");
    }

    #[test]
    fn tokenizer_skips_comments_and_reads_cdata() {
        let events: Vec<_> =
            xml_events("<?xml?><!-- <x> --><a k='v &amp; w'><![CDATA[<raw>]]></a><b/>").collect();
        assert_eq!(
            events,
            vec![
                XmlEvent::Start {
                    name: "a",
                    attrs: " k='v &amp; w'",
                    self_closing: false
                },
                XmlEvent::Text("<raw>"),
                XmlEvent::End { name: "a" },
                XmlEvent::Start {
                    name: "b",
                    attrs: "",
                    self_closing: true
                },
            ]
        );
        assert_eq!(attr(" k='v &amp; w'", "k").as_deref(), Some("v & w"));
    }

    #[test]
    fn render_table_pads_ragged_rows() {
        let rows = vec![
            vec!["a".to_string(), "b".to_string()],
            vec!["only".to_string()],
        ];
        assert_eq!(render_table(&rows), "| a | b |\n| --- | --- |\n| only |  |");
    }
}

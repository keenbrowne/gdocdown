//! gdocdown engine: the novel core.
//!
//! Given the *current* state of a Google Doc and a *desired* markdown source,
//! produce the minimal set of Google Docs `batchUpdate` requests that morph the
//! doc into the markdown — preserving the doc's identity and only touching what
//! changed. No existing tool builds this incremental write-back.
//!
//! The engine is network-free so it can be unit-tested offline; `api` wraps it.
//!
//! ## Pipeline
//! `sync_requests(current, desired)` produces, in order:
//!   1. **Text pass** — minimal insert/delete to morph the (clean) body text.
//!      Handles the immovable final newline.
//!   2. **Paragraph-style pass** — heading levels and list bullets, for blocks
//!      whose kind/depth changed or that were inserted (lists rebuilt per run).
//!   3. **Inline-style pass** — bold/italic/underline/strikethrough/super/sub via
//!      `updateTextStyle` over character ranges, for paragraphs whose runs
//!      changed. Inserted/edited paragraphs are cleared first so they don't keep
//!      inherited marks.
//!
//! ## Inline markdown syntax
//! `**bold**`, `*italic*`, `~~strike~~`, and `[text](url)` links — exactly what
//! Google Docs emits on markdown export. Markers are stripped from the model's
//! clean text; link text keeps its own marks (`[**x**](url)`).
//!
//! Underline, superscript, and subscript are **deferred**: Google's markdown
//! export drops them to plain text, so there is no matching flavor to adopt.
//! We neither parse them nor touch those text-style fields, so any that already
//! exist in a doc are left alone. (Re-enabling them later is a superset decision.)
//!
//! ## Known limitations
//! - Diffs count Unicode scalar values; the Docs API counts UTF-16 code units.
//!   Identical for BMP text; astral characters need a UTF-16 pass.
//! - Links and tables aren't modeled. Inline emphasis parsing is a pragmatic
//!   toggling parser (balanced markers), not full CommonMark flanking rules.
//! - **Mixed-kind nesting** (a numbered parent with bulleted children, or vice
//!   versa, in one indented list) is not supported — and appears inexpressible
//!   through the public Docs API. Same-kind nesting is fully supported.

use serde::Serialize;
use similar::{Algorithm, DiffOp, TextDiff};

pub mod api;
pub mod apply;

// ---------------------------------------------------------------------------
// Document model
// ---------------------------------------------------------------------------

/// What a paragraph-level block is. A block is exactly one of these.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BlockKind {
    Normal,
    Heading(u8),
    Bullet,
    Number,
    /// A checkbox / task-list item (`- [ ]`). Checked state is not modeled (the
    /// API can't set it), so all serialize as unchecked.
    Checkbox,
}

/// Inline character styling. `Default` = unstyled. Underline / super / subscript
/// are deferred (Google's markdown export drops them), so they aren't modeled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct TextStyle {
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
}

impl TextStyle {
    pub fn is_default(&self) -> bool {
        *self == TextStyle::default()
    }
}

/// The sentinel character standing in for one inline object (image, …) in the
/// index-text. It is one UTF-16 unit, matching the object's one document index,
/// so offset→index math stays correct. Never appears in real document text.
pub const OBJECT_SENTINEL: char = '\u{FFFC}';

/// A contiguous span of text sharing one style and link, OR a single inline
/// object (when `object` is set — then `text` is one [`OBJECT_SENTINEL`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Run {
    pub text: String,
    pub style: TextStyle,
    pub link: Option<String>,
    /// Some(inlineObjectId) when this run is an inline object (e.g. an image).
    pub object: Option<String>,
}

impl Run {
    /// No text styling and no link (an inline object counts as plain — there's
    /// nothing to style).
    pub fn is_plain(&self) -> bool {
        self.style.is_default() && self.link.is_none()
    }
    /// An inline object placeholder run, identified by a `descriptor` such as
    /// `image/<id>`, `pagebreak`, or `footnote/<id>`.
    pub fn object(descriptor: &str) -> Run {
        Run { text: OBJECT_SENTINEL.to_string(), style: TextStyle::default(), link: None, object: Some(descriptor.into()) }
    }
}

/// One paragraph-level block. Its clean text (markdown markers, leading
/// indentation, and Docs glyphs excluded) is the concatenation of its runs.
/// `depth` is the list nesting level (0 = top); always 0 for non-list blocks.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Block {
    pub kind: BlockKind,
    pub depth: u8,
    pub runs: Vec<Run>,
}

impl Block {
    /// The block's clean text.
    pub fn text(&self) -> String {
        self.runs.iter().map(|r| r.text.as_str()).collect()
    }
    /// Whether any run carries a non-default style or a link.
    pub fn has_marks(&self) -> bool {
        self.runs.iter().any(|r| !r.is_plain())
    }
}

/// A top-level node: a paragraph, table, horizontal rule (`---`), or table of
/// contents. Rules and TOCs can't be created via the API (read/preserve/delete).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Para(Block),
    Table(Table),
    Rule,
    Toc,
}

impl Node {
    /// The paragraph block, if this node is a paragraph.
    pub fn as_para(&self) -> Option<&Block> {
        match self {
            Node::Para(b) => Some(b),
            _ => None,
        }
    }
    /// Whether this node is a "barrier" (table / rule / TOC) that splits paragraph runs.
    pub fn is_barrier(&self) -> bool {
        matches!(self, Node::Table(_) | Node::Rule | Node::Toc)
    }
}

/// A document as an ordered list of nodes.
pub type DocModel = Vec<Node>;

/// Max list nesting Google Docs supports (levels 0..=8).
const MAX_DEPTH: u8 = 8;

/// Placeholder line for a (preserved, auto-generated) table of contents.
const TOC_PLACEHOLDER: &str = "<!-- gdoc:toc -->";

/// Merge adjacent same-style runs and drop empties so two models with identical
/// content compare equal regardless of how runs were segmented.
pub fn normalize_runs(runs: Vec<Run>) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::new();
    for r in runs {
        if r.text.is_empty() {
            continue;
        }
        match out.last_mut() {
            // Never merge inline objects — each is its own distinct element.
            Some(last) if last.object.is_none() && r.object.is_none() && last.style == r.style && last.link == r.link => {
                last.text.push_str(&r.text)
            }
            _ => out.push(r),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Markdown -> model
// ---------------------------------------------------------------------------

/// Parse markdown into the block model (block structure + inline runs).
///
/// List nesting is **relative**: a stack of the indent widths of the currently
/// open levels, so each increase in indentation is one deeper level regardless
/// of how many columns it is. This matches Google Docs' export, which indents by
/// the *parent marker width* — 2 columns under `* `, 3 under `1. ` — rather than
/// a fixed unit, and also tolerates any consistent indent a user types.
pub fn markdown_to_model(md: &str) -> DocModel {
    let lines: Vec<&str> = md.lines().collect();
    let mut nodes = Vec::new();
    let mut open: Vec<usize> = Vec::new(); // indent widths of open list levels
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        if raw.trim().is_empty() {
            i += 1;
            continue;
        }

        // A thematic break (`---`, `***`, `___`, 3+ of one char) is a horizontal rule.
        if is_thematic_break(raw) {
            open.clear();
            nodes.push(Node::Rule);
            i += 1;
            continue;
        }

        // The table-of-contents placeholder.
        if raw.trim() == TOC_PLACEHOLDER {
            open.clear();
            nodes.push(Node::Toc);
            i += 1;
            continue;
        }

        // A GFM table consumes its header, separator, and following pipe rows.
        if is_table_start(&lines[i..]) {
            let mut j = i + 2;
            while j < lines.len() && split_row(lines[j]).is_some() {
                j += 1;
            }
            if let Some(table) = parse_table(&lines[i..j]) {
                nodes.push(Node::Table(table));
            }
            open.clear();
            i = j;
            continue;
        }

        let body = raw.trim_start_matches([' ', '\t']);
        let width = indent_width(&raw[..raw.len() - body.len()]);
        let content = body.trim_end();

        if let Some((level, rest)) = parse_heading(content) {
            open.clear();
            nodes.push(Node::Para(block(BlockKind::Heading(level), 0, rest)));
        } else if let Some((kind, rest)) = parse_list_marker(content) {
            while matches!(open.last(), Some(&top) if width < top) {
                open.pop();
            }
            if open.last().is_none_or(|&top| width > top) {
                open.push(width);
            }
            let depth = (open.len() - 1).min(MAX_DEPTH as usize) as u8;
            nodes.push(Node::Para(block(kind, depth, rest)));
        } else {
            open.clear();
            nodes.push(Node::Para(block(BlockKind::Normal, 0, content)));
        }
        i += 1;
    }
    nodes
}

/// A markdown thematic break: a line of 3+ of a single `-`/`*`/`_` (spaces allowed).
fn is_thematic_break(line: &str) -> bool {
    for marker in ['-', '*', '_'] {
        let count = line.chars().filter(|&c| c == marker).count();
        let only = line.chars().all(|c| c == marker || c == ' ' || c == '\t');
        if count >= 3 && only {
            return true;
        }
    }
    false
}

fn parse_heading(content: &str) -> Option<(u8, &str)> {
    for level in (1u8..=6).rev() {
        let hashes = "#".repeat(level as usize);
        if let Some(rest) = content.strip_prefix(&format!("{hashes} ")) {
            return Some((level, rest));
        }
        // An empty heading serializes as e.g. "# ", whose trailing space the line
        // parser trims to "#"; recognize bare hashes as an empty heading.
        if content == hashes {
            return Some((level, ""));
        }
    }
    None
}

fn parse_list_marker(content: &str) -> Option<(BlockKind, &str)> {
    if let Some(rest) = content.strip_prefix("- ").or_else(|| content.strip_prefix("* ")) {
        // Task-list item: `- [ ] ...` / `- [x] ...` (checked state ignored).
        for marker in ["[ ] ", "[x] ", "[X] "] {
            if let Some(text) = rest.strip_prefix(marker) {
                return Some((BlockKind::Checkbox, text));
            }
        }
        return Some((BlockKind::Bullet, rest));
    }
    strip_ordered_marker(content).map(|rest| (BlockKind::Number, rest))
}

fn block(kind: BlockKind, depth: u8, inline: &str) -> Block {
    Block { kind, depth, runs: parse_inline(inline) }
}

fn strip_ordered_marker(line: &str) -> Option<&str> {
    let digits: String = line.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let after = &line[digits.len()..];
    let after = after.strip_prefix('.').or_else(|| after.strip_prefix(')'))?;
    after.strip_prefix(' ')
}

/// Leading-whitespace width in columns (tab = 4), for relative nesting.
fn indent_width(indent: &str) -> usize {
    indent.chars().map(|c| if c == '\t' { 4 } else { 1 }).sum()
}

/// Parse inline markdown into styled runs. A pragmatic recursive parser: a
/// delimiter only takes effect when its matching closer exists, otherwise it's
/// literal text. Handles nesting (e.g. bold inside italic). Not full CommonMark.
pub fn parse_inline(s: &str) -> Vec<Run> {
    let mut runs = Vec::new();
    parse_into(s, TextStyle::default(), None, &mut runs);
    normalize_runs(runs)
}

/// Delimiters, longest-prefix first so `**`/`__` beat `*`/`_`. Each entry mutates
/// the active style for the wrapped span. The bool marks underscore-style
/// delimiters, which — unlike `*` — must not bind *intraword*, so identifiers like
/// `snake_case` and `foo_bar.md` stay literal. Underscores are input-only aliases;
/// the serializer always emits the `*` forms (matching Google's markdown export).
const WRAPS: &[(&str, &str, fn(&mut TextStyle), bool)] = &[
    ("***", "***", |s| {
        s.bold = true;
        s.italic = true;
    }, false),
    ("___", "___", |s| {
        s.bold = true;
        s.italic = true;
    }, true),
    ("**", "**", |s| s.bold = true, false),
    ("__", "__", |s| s.bold = true, true),
    ("~~", "~~", |s| s.strikethrough = true, false),
    ("*", "*", |s| s.italic = true, false),
    ("_", "_", |s| s.italic = true, true),
];

fn parse_into(s: &str, style: TextStyle, link: Option<&str>, out: &mut Vec<Run>) {
    let mut rest = s;
    let mut buf = String::new();
    'outer: while !rest.is_empty() {
        // Inline object placeholder: ![alt](gdoc:image/<id>).
        if rest.starts_with("![") {
            if let Some((desc, after)) = try_object(rest) {
                flush(&mut buf, style, link, out);
                out.push(Run::object(desc));
                rest = after;
                continue;
            }
        }
        // Link: [text](url). The inner text keeps its own marks, gains the link.
        if rest.starts_with('[') {
            if let Some((inner, url, after)) = try_link(rest) {
                flush(&mut buf, style, link, out);
                parse_into(inner, style, Some(url), out);
                rest = after;
                continue;
            }
        }
        // The character immediately before `rest` (for intraword checks).
        let prev = s[..s.len() - rest.len()].chars().last();
        for (open, close, apply, intraword) in WRAPS {
            let matched = if *intraword {
                try_wrap_word(rest, open, close, prev)
            } else {
                try_wrap(rest, open, close)
            };
            if let Some((inner, after)) = matched {
                flush(&mut buf, style, link, out);
                let mut inner_style = style;
                apply(&mut inner_style);
                parse_into(inner, inner_style, link, out);
                rest = after;
                continue 'outer;
            }
        }
        // Backslash escapes the next character.
        let ch = rest.chars().next().unwrap();
        if ch == '\\' {
            if let Some(next) = rest[1..].chars().next() {
                buf.push(next);
                rest = &rest[1 + next.len_utf8()..];
                continue;
            }
        }
        buf.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    flush(&mut buf, style, link, out);
}

/// Parse `[text](url)` at the start of `s`; returns (text, url, remainder).
fn try_link(s: &str) -> Option<(&str, &str, &str)> {
    let body = s.strip_prefix('[')?;
    let close = body.find(']')?;
    let (text, after_text) = (&body[..close], &body[close + 1..]);
    let paren = after_text.strip_prefix('(')?;
    let end = paren.find(')')?;
    let (url, after) = (&paren[..end], &paren[end + 1..]);
    if text.is_empty() || url.is_empty() {
        return None;
    }
    Some((text, url, after))
}

/// If `s` starts with `open` and a later `close` exists, return (inner, after).
/// Requires non-empty inner so empty `****` doesn't swallow text.
fn try_wrap<'a>(s: &'a str, open: &str, close: &str) -> Option<(&'a str, &'a str)> {
    let body = s.strip_prefix(open)?;
    let end = body.find(close)?;
    if end == 0 {
        return None;
    }
    Some((&body[..end], &body[end + close.len()..]))
}

/// Like `try_wrap`, but for underscore-style delimiters that must not bind
/// intraword: the opener can't follow an alphanumeric and the closer can't
/// precede one. So `snake_case`, `a_b_c`, and `foo_bar.md` stay literal, while
/// `_x_` (bounded by spaces / punctuation / ends) still emphasizes. Skips
/// intraword closers, searching on for a later valid one.
fn try_wrap_word<'a>(s: &'a str, open: &str, close: &str, prev: Option<char>) -> Option<(&'a str, &'a str)> {
    // A "word char" is alphanumeric or `_`, so a delimiter touching one is
    // intraword (e.g. `snake_case`, `my__dunder__name`) and stays literal.
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    if prev.is_some_and(is_word) {
        return None; // opener would bind intraword
    }
    let body = s.strip_prefix(open)?;
    let mut search = 0;
    while let Some(rel) = body[search..].find(close) {
        let end = search + rel;
        let after = &body[end + close.len()..];
        if end != 0 && !after.chars().next().is_some_and(is_word) {
            return Some((&body[..end], after));
        }
        search = end + close.len(); // empty inner or intraword closer — keep looking
    }
    None
}

fn flush(buf: &mut String, style: TextStyle, link: Option<&str>, out: &mut Vec<Run>) {
    if !buf.is_empty() {
        out.push(Run { text: std::mem::take(buf), style, link: link.map(String::from), object: None });
    }
}

/// Parse an inline object placeholder `![alt](gdoc:<descriptor>)` at the start of
/// `s`; returns (descriptor, remainder).
fn try_object<'a>(s: &'a str) -> Option<(&'a str, &'a str)> {
    let body = s.strip_prefix("![")?;
    let close = body.find("](")?;
    let after = &body[close + 2..];
    let end = after.find(')')?;
    let desc = after[..end].strip_prefix("gdoc:")?;
    if desc.is_empty() {
        return None;
    }
    Some((desc, &after[end + 1..]))
}

// ---------------------------------------------------------------------------
// GFM tables (Phase 1: model + markdown parsing)
// ---------------------------------------------------------------------------

/// A table cell's content: inline runs (one paragraph's worth).
pub type Cell = Vec<Run>;

/// A GFM table. `rows[0]` is the header row (as Google's export emits it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub rows: Vec<Vec<Cell>>,
}

impl Table {
    pub fn columns(&self) -> usize {
        self.rows.first().map_or(0, |r| r.len())
    }
}

/// Whether `lines` (consecutive non-blank lines) begin a GFM pipe table: a pipe
/// row followed by a separator row (`| :--- | --- |`).
pub fn is_table_start(lines: &[&str]) -> bool {
    lines.len() >= 2
        && split_row(lines[0]).is_some()
        && split_row(lines[1]).is_some_and(|cells| is_separator(&cells))
}

/// Parse a GFM table from `lines` (header, separator, then body rows). Returns
/// `None` if the first two lines aren't a header+separator. Short/long body rows
/// are padded/truncated to the header's column count.
pub fn parse_table(lines: &[&str]) -> Option<Table> {
    let header = split_row(lines.first()?)?;
    let sep = split_row(lines.get(1)?)?;
    if !is_separator(&sep) || sep.len() != header.len() {
        return None;
    }
    let cols = header.len();
    let mut rows = vec![header.iter().map(|c| parse_inline(c)).collect::<Vec<Cell>>()];
    for line in &lines[2..] {
        let Some(cells) = split_row(line) else { break };
        let row = (0..cols).map(|i| parse_inline(cells.get(i).copied().unwrap_or(""))).collect();
        rows.push(row);
    }
    Some(Table { rows })
}

/// Split `| a | b |` into trimmed cell strings, or `None` if not a pipe row.
fn split_row(line: &str) -> Option<Vec<&str>> {
    let t = line.trim();
    let inner = t.strip_prefix('|')?;
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    Some(inner.split('|').map(str::trim).collect())
}

/// A GFM separator row: each cell is dashes with optional leading/trailing colon.
fn is_separator(cells: &[&str]) -> bool {
    !cells.is_empty()
        && cells.iter().all(|c| {
            let c = c.trim();
            c.contains('-') && c.chars().all(|ch| ch == '-' || ch == ':')
        })
}

// ---------------------------------------------------------------------------
// Model -> markdown (the pull direction)
// ---------------------------------------------------------------------------

/// Serialize a document model back to markdown. The inverse of
/// [`markdown_to_model`]: `markdown_to_model(model_to_markdown(m)) == m`. Output
/// matches Google's export flavor (headings `#`, bullets `* `, numbers `1. `,
/// `**`/`*`/`~~`, `[text](url)`, GFM tables, `---` rules) with inline objects as
/// `![](gdoc:image/<id>)` placeholders.
pub fn model_to_markdown(model: &DocModel) -> String {
    let mut blocks: Vec<String> = Vec::new();
    let mut list: Vec<String> = Vec::new();
    for node in model {
        if let Node::Para(b) = node {
            if is_list(&b.kind) {
                list.push(list_item_md(b));
                continue;
            }
        }
        if !list.is_empty() {
            blocks.push(std::mem::take(&mut list).join("\n"));
        }
        blocks.push(node_md(node));
    }
    if !list.is_empty() {
        blocks.push(list.join("\n"));
    }
    let mut out = blocks.join("\n\n");
    out.push('\n');
    out
}

/// Whether a normal paragraph's text would be misparsed as a standalone block
/// marker (rule / TOC / empty heading) and so needs a leading backslash escape.
fn reparses_as_block_marker(s: &str) -> bool {
    is_thematic_break(s) || s == TOC_PLACEHOLDER || (1..=6).any(|n| s == "#".repeat(n))
}

fn node_md(node: &Node) -> String {
    match node {
        Node::Para(b) => match b.kind {
            BlockKind::Heading(n) => format!("{} {}", "#".repeat(n as usize), runs_md(&b.runs, false)),
            _ => {
                let s = runs_md(&b.runs, false);
                // Keep a normal paragraph from being reparsed as a block marker
                // (horizontal rule, TOC placeholder, or bare-hash empty heading).
                if reparses_as_block_marker(&s) {
                    format!("\\{s}")
                } else {
                    s
                }
            }
        },
        Node::Table(t) => table_md(t),
        Node::Rule => "---".to_string(),
        Node::Toc => TOC_PLACEHOLDER.to_string(),
    }
}

fn list_item_md(b: &Block) -> String {
    let (marker, unit) = match b.kind {
        BlockKind::Number => ("1. ", 3), // Google indents nested numbers by the marker width
        BlockKind::Checkbox => ("- [ ] ", 2),
        _ => ("* ", 2),
    };
    format!("{}{}{}", " ".repeat(b.depth as usize * unit), marker, runs_md(&b.runs, false))
}

fn table_md(t: &Table) -> String {
    let mut lines = Vec::new();
    for (i, row) in t.rows.iter().enumerate() {
        let cells: Vec<String> = row.iter().map(|c| runs_md(c, true)).collect();
        lines.push(format!("| {} |", cells.join(" | ")));
        if i == 0 {
            lines.push(format!("| {} |", vec!["---"; t.columns().max(1)].join(" | ")));
        }
    }
    lines.join("\n")
}

fn runs_md(runs: &[Run], in_table: bool) -> String {
    runs.iter().map(|r| run_md(r, in_table)).collect()
}

fn run_md(r: &Run, in_table: bool) -> String {
    if let Some(desc) = &r.object {
        return format!("![](gdoc:{desc})");
    }
    let mut s = escape_inline(&r.text, in_table);
    let st = r.style;
    s = if st.bold && st.italic {
        format!("***{s}***")
    } else if st.bold {
        format!("**{s}**")
    } else if st.italic {
        format!("*{s}*")
    } else {
        s
    };
    if st.strikethrough {
        s = format!("~~{s}~~");
    }
    match &r.link {
        Some(u) => format!("[{s}]({u})"),
        None => s,
    }
}

/// Backslash-escape characters our parser would otherwise interpret.
fn escape_inline(text: &str, in_table: bool) -> String {
    let mut s = String::new();
    for c in text.chars() {
        let special = matches!(c, '\\' | '*' | '~' | '[' | ']' | '!') || (in_table && c == '|');
        if special {
            s.push('\\');
        }
        s.push(c);
    }
    s
}

/// Render the model to the plain text a Google Doc body holds.
pub fn model_to_plain(model: &DocModel) -> String {
    let mut s = String::new();
    for node in model {
        if let Node::Para(b) = node {
            s.push_str(&b.text());
            s.push('\n');
        }
        // Tables contribute no flat text; they're handled by the real-index
        // sync (Phase 3). Until then, sync assumes a table-free document.
    }
    s
}

// ---------------------------------------------------------------------------
// batchUpdate request types (serialize to the exact Google JSON shape)
// ---------------------------------------------------------------------------

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub index: usize,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Range {
    pub start_index: usize,
    pub end_index: usize,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InsertText {
    pub location: Location,
    pub text: String,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DeleteContentRange {
    pub range: Range,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ParagraphStyle {
    pub named_style_type: String,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UpdateParagraphStyle {
    pub range: Range,
    pub paragraph_style: ParagraphStyle,
    pub fields: String,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CreateParagraphBullets {
    pub range: Range,
    pub bullet_preset: String,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DeleteParagraphBullets {
    pub range: Range,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Link {
    pub url: String,
}

/// Wire form of a text style for `updateTextStyle`. `link` is omitted when
/// `None`; since `fields` always lists `link`, an omitted link clears any
/// existing one.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TextStyleMsg {
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<Link>,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UpdateTextStyle {
    pub range: Range,
    pub text_style: TextStyleMsg,
    pub fields: String,
}

/// One entry in a `batchUpdate` `requests` array. Externally tagged so each
/// serializes as `{"insertText": {...}}`, etc., exactly as the Docs API expects.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Request {
    InsertText(InsertText),
    DeleteContentRange(DeleteContentRange),
    UpdateParagraphStyle(UpdateParagraphStyle),
    CreateParagraphBullets(CreateParagraphBullets),
    DeleteParagraphBullets(DeleteParagraphBullets),
    UpdateTextStyle(UpdateTextStyle),
}

const BULLET_PRESET: &str = "BULLET_DISC_CIRCLE_SQUARE";
const NUMBER_PRESET: &str = "NUMBERED_DECIMAL_ALPHA_ROMAN";
const CHECKBOX_PRESET: &str = "BULLET_CHECKBOX";
const TEXT_FIELDS: &str = "bold,italic,strikethrough,link";

/// Length of `s` in UTF-16 code units — the unit Google Docs indices count in
/// (an astral char like an emoji is 1 scalar but 2 units). All doc-index
/// arithmetic must use this, not `chars().count()`.
fn u16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Cumulative UTF-16 offset by character position: `prefix[i]` is the number of
/// UTF-16 units in the first `i` chars (length `chars + 1`). Lets a character-
/// space diff emit doc indices in UTF-16 units.
fn utf16_prefix(s: &str) -> Vec<usize> {
    let mut prefix = Vec::with_capacity(s.chars().count() + 1);
    let mut acc = 0;
    prefix.push(0);
    for c in s.chars() {
        acc += c.len_utf16();
        prefix.push(acc);
    }
    prefix
}

/// `off` is a UTF-16 offset (doc index = off + 1).
fn ins(off: usize, text: String) -> Request {
    Request::InsertText(InsertText { location: Location { index: off + 1 }, text })
}

/// `off` and `len16` are UTF-16 units (doc range = [off+1, off+len16+1)).
fn del(off: usize, len16: usize) -> Request {
    Request::DeleteContentRange(DeleteContentRange {
        range: Range { start_index: off + 1, end_index: off + len16 + 1 },
    })
}

fn update_text_style(range: Range, style: TextStyle, link: Option<&str>) -> Request {
    Request::UpdateTextStyle(UpdateTextStyle {
        range,
        text_style: TextStyleMsg {
            bold: style.bold,
            italic: style.italic,
            strikethrough: style.strikethrough,
            link: link.map(|url| Link { url: url.to_string() }),
        },
        fields: TEXT_FIELDS.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Text pass
// ---------------------------------------------------------------------------

/// Minimal text edits from `old` to `new`, in pure text space (doc index =
/// offset + 1), emitted highest-index-first. Final-newline-agnostic.
pub fn diff_to_requests(old: &str, new: &str) -> Vec<Request> {
    let new_chars: Vec<char> = new.chars().collect();
    let diff = TextDiff::from_chars(old, new);
    // The diff op positions are character indices into `old`; map them to UTF-16
    // offsets so the emitted doc indices count the unit Google Docs counts.
    let off = utf16_prefix(old);

    let mut reqs = Vec::new();
    for op in diff.ops().iter().rev() {
        match *op {
            DiffOp::Equal { .. } => {}
            DiffOp::Delete { old_index, old_len, .. } => {
                reqs.push(del(off[old_index], off[old_index + old_len] - off[old_index]))
            }
            DiffOp::Insert { old_index, new_index, new_len } => {
                let text: String = new_chars[new_index..new_index + new_len].iter().collect();
                reqs.push(ins(off[old_index], text));
            }
            DiffOp::Replace { old_index, old_len, new_index, new_len } => {
                reqs.push(del(off[old_index], off[old_index + old_len] - off[old_index]));
                let text: String = new_chars[new_index..new_index + new_len].iter().collect();
                reqs.push(ins(off[old_index], text));
            }
        }
    }
    reqs
}

/// Text edits for a real document body, respecting the immovable final newline.
pub fn doc_text_requests(old: &str, new: &str) -> Vec<Request> {
    let mut reqs = diff_to_requests(old, new);
    if !old.ends_with('\n') {
        return reqs;
    }
    let l = u16_len(old);
    for r in &mut reqs {
        match r {
            Request::InsertText(it) if it.location.index == l + 1 => {
                it.location.index = l;
                if let Some(body) = it.text.strip_suffix('\n') {
                    it.text = format!("\n{body}");
                }
            }
            Request::DeleteContentRange(d) if d.range.end_index == l + 1 => {
                if d.range.start_index > 1 {
                    d.range.start_index -= 1;
                    d.range.end_index -= 1;
                } else {
                    d.range.end_index = l;
                }
            }
            _ => {}
        }
    }
    reqs
}

// ---------------------------------------------------------------------------
// Shared: final paragraph ranges + block-level diff
// ---------------------------------------------------------------------------

/// Final paragraph ranges (post text pass), cumulative from index 1.
fn paragraph_ranges(desired: &[Block]) -> Vec<Range> {
    let mut ranges = Vec::with_capacity(desired.len());
    let mut idx = 1usize;
    for b in desired {
        let len = u16_len(&b.text());
        ranges.push(Range { start_index: idx, end_index: idx + len + 1 });
        idx += len + 1;
    }
    ranges
}

fn is_list(kind: &BlockKind) -> bool {
    matches!(kind, BlockKind::Bullet | BlockKind::Number | BlockKind::Checkbox)
}

// ---------------------------------------------------------------------------
// Paragraph-style pass (headings + list bullets)
// ---------------------------------------------------------------------------

/// Heading/list requests for blocks whose kind/depth changed or were inserted.
///
/// Non-list blocks are handled per paragraph. List items are handled per
/// *contiguous same-kind run*: if anything in a run changed, the whole run is
/// rebuilt — cleared, then each item gets `depth` leading tabs (tail-first so
/// clean indices stay valid), then one `createParagraphBullets` consumes the
/// tabs and assigns nesting levels. Each run's net text change is zero, so all
/// runs/blocks operate on the same clean coordinate space.
///
/// Runs are split by kind, so mixed-kind nesting is intentionally not produced
/// (a hard Docs API limit — see the crate docs).
pub fn style_requests(current: &[Block], desired: &[Block]) -> Vec<Request> {
    let ranges = paragraph_ranges(desired);

    let mut dirty = vec![false; desired.len()];
    for op in similar::capture_diff_slices(Algorithm::Myers, current, desired) {
        match op {
            DiffOp::Equal { .. } | DiffOp::Delete { .. } => {}
            DiffOp::Insert { new_index, new_len, .. } => {
                for t in 0..new_len {
                    dirty[new_index + t] = true;
                }
            }
            DiffOp::Replace { old_index, old_len, new_index, new_len } => {
                let paired = old_len.min(new_len);
                for t in 0..paired {
                    let c = &current[old_index + t];
                    let d = &desired[new_index + t];
                    if c.kind != d.kind || c.depth != d.depth {
                        dirty[new_index + t] = true;
                    }
                }
                for t in paired..new_len {
                    dirty[new_index + t] = true;
                }
            }
        }
    }

    let mut out = Vec::new();
    let mut i = 0;
    while i < desired.len() {
        let kind = &desired[i].kind;
        if is_list(kind) {
            let mut j = i + 1;
            while j < desired.len() && desired[j].kind == *kind {
                j += 1;
            }
            if dirty[i..j].iter().any(|&d| d) {
                rebuild_list_run(desired, &ranges, i, j, &mut out);
            }
            i = j;
        } else {
            if dirty[i] {
                style_block(&desired[i], ranges[i].clone(), &mut out);
            }
            i += 1;
        }
    }
    out
}

fn style_block(block: &Block, range: Range, out: &mut Vec<Request>) {
    out.push(Request::DeleteParagraphBullets(DeleteParagraphBullets { range: range.clone() }));
    let named = match block.kind {
        BlockKind::Heading(n) => format!("HEADING_{n}"),
        _ => "NORMAL_TEXT".to_string(),
    };
    out.push(Request::UpdateParagraphStyle(UpdateParagraphStyle {
        range,
        paragraph_style: ParagraphStyle { named_style_type: named },
        fields: "namedStyleType".to_string(),
    }));
}

fn rebuild_list_run(desired: &[Block], ranges: &[Range], i: usize, j: usize, out: &mut Vec<Request>) {
    let run = Range { start_index: ranges[i].start_index, end_index: ranges[j - 1].end_index };

    out.push(Request::DeleteParagraphBullets(DeleteParagraphBullets { range: run.clone() }));
    out.push(Request::UpdateParagraphStyle(UpdateParagraphStyle {
        range: run.clone(),
        paragraph_style: ParagraphStyle { named_style_type: "NORMAL_TEXT".to_string() },
        fields: "namedStyleType".to_string(),
    }));

    for k in (i..j).rev() {
        let depth = desired[k].depth as usize;
        if depth > 0 {
            out.push(Request::InsertText(InsertText {
                location: Location { index: ranges[k].start_index },
                text: "\t".repeat(depth),
            }));
        }
    }

    let total_tabs: usize = (i..j).map(|k| desired[k].depth as usize).sum();
    let preset = match desired[i].kind {
        BlockKind::Number => NUMBER_PRESET,
        BlockKind::Checkbox => CHECKBOX_PRESET,
        _ => BULLET_PRESET,
    };
    out.push(Request::CreateParagraphBullets(CreateParagraphBullets {
        range: Range { start_index: run.start_index, end_index: run.end_index + total_tabs },
        bullet_preset: preset.to_string(),
    }));
}

// ---------------------------------------------------------------------------
// Inline-style pass (bold / italic / underline / strike / super / sub)
// ---------------------------------------------------------------------------

/// `updateTextStyle` requests for paragraphs whose runs changed. For each such
/// paragraph the whole text range is cleared to default first (so removed marks
/// and inherited marks on inserted text are wiped), then each styled run is set.
pub fn inline_style_requests(current: &[Block], desired: &[Block]) -> Vec<Request> {
    let ranges = paragraph_ranges(desired);

    // A paragraph needs inline work if it gained/lost marks or was inserted.
    let mut dirty = vec![false; desired.len()];
    for op in similar::capture_diff_slices(Algorithm::Myers, current, desired) {
        match op {
            DiffOp::Equal { .. } | DiffOp::Delete { .. } => {}
            DiffOp::Insert { new_index, new_len, .. } => {
                for t in 0..new_len {
                    dirty[new_index + t] = true; // inserted text may inherit marks
                }
            }
            DiffOp::Replace { old_index, old_len, new_index, new_len } => {
                let paired = old_len.min(new_len);
                for t in 0..paired {
                    if current[old_index + t].has_marks() || desired[new_index + t].has_marks() {
                        dirty[new_index + t] = true;
                    }
                }
                for t in paired..new_len {
                    dirty[new_index + t] = true;
                }
            }
        }
    }

    let mut out = Vec::new();
    for (i, b) in desired.iter().enumerate() {
        if !dirty[i] {
            continue;
        }
        let s = ranges[i].start_index;
        let text_len: usize = u16_len(&b.text());
        if text_len == 0 {
            continue;
        }
        // Clear the whole paragraph's text (marks + links), then set each run
        // that carries any styling or a link.
        out.push(update_text_style(
            Range { start_index: s, end_index: s + text_len },
            TextStyle::default(),
            None,
        ));
        let mut off = 0usize;
        for run in &b.runs {
            let len = u16_len(&run.text);
            if len > 0 && !run.is_plain() {
                out.push(update_text_style(
                    Range { start_index: s + off, end_index: s + off + len },
                    run.style,
                    run.link.as_deref(),
                ));
            }
            off += len;
        }
    }
    out
}

/// A run of consecutive paragraphs (between tables / document ends), tagged with
/// its real start index in the document.
#[derive(Debug, Clone)]
pub struct ParaSegment {
    pub start_index: usize,
    pub blocks: Vec<Block>,
}

/// Split a model into paragraph runs separated by tables (`#tables + 1` runs).
pub fn split_paragraph_runs(model: &DocModel) -> Vec<Vec<Block>> {
    let mut runs = Vec::new();
    let mut cur = Vec::new();
    for node in model {
        match node {
            Node::Para(b) => cur.push(b.clone()),
            Node::Table(_) | Node::Rule | Node::Toc => runs.push(std::mem::take(&mut cur)),
        }
    }
    runs.push(cur);
    runs
}

fn blocks_to_plain(blocks: &[Block]) -> String {
    let mut s = String::new();
    for b in blocks {
        s.push_str(&b.text());
        s.push('\n');
    }
    s
}

/// Shift every index in a request by `d` (to rebase a segment's edits from
/// index-1 space into its real position in the document).
fn shift_request(r: &mut Request, d: usize) {
    match r {
        Request::InsertText(x) => x.location.index += d,
        Request::DeleteContentRange(x) => {
            x.range.start_index += d;
            x.range.end_index += d;
        }
        Request::UpdateParagraphStyle(x) => {
            x.range.start_index += d;
            x.range.end_index += d;
        }
        Request::CreateParagraphBullets(x) => {
            x.range.start_index += d;
            x.range.end_index += d;
        }
        Request::DeleteParagraphBullets(x) => {
            x.range.start_index += d;
            x.range.end_index += d;
        }
        Request::UpdateTextStyle(x) => {
            x.range.start_index += d;
            x.range.end_index += d;
        }
    }
}

/// One contiguous editable region — a paragraph segment or a single table cell
/// — diffed against its desired content and anchored at a real start index.
/// `final_newline` is true when the region owns an immovable trailing newline
/// (the document's last paragraph, or any cell's lone paragraph).
struct EditUnit {
    start_index: usize,
    cur: Vec<Block>,
    des: Vec<Block>,
    final_newline: bool,
}

fn sync_unit(u: &EditUnit) -> Vec<Request> {
    let cur_text = blocks_to_plain(&u.cur);
    let des_text = blocks_to_plain(&u.des);
    let mut reqs = if u.final_newline {
        doc_text_requests(&cur_text, &des_text)
    } else {
        diff_to_requests(&cur_text, &des_text)
    };
    // We can't create inline objects (e.g. an uploaded image has no insertable
    // URL), so drop any text insertion that carries the object sentinel.
    reqs.retain(|r| !matches!(r, Request::InsertText(it) if it.text.contains(OBJECT_SENTINEL)));
    // Drop no-op / invalid empty-range deletes (e.g. "clear a segment that's just
    // its immovable final newline").
    reqs.retain(|r| !matches!(r, Request::DeleteContentRange(d) if d.range.start_index >= d.range.end_index));
    reqs.extend(style_requests(&u.cur, &u.des));
    reqs.extend(inline_style_requests(&u.cur, &u.des));
    let delta = u.start_index - 1;
    if delta != 0 {
        for r in &mut reqs {
            shift_request(r, delta);
        }
    }
    reqs
}

/// Core incremental sync over paragraph segments anchored at real indices.
/// Segments pair positionally; tables between them are preserved. Segments are
/// processed highest-index-first so each segment's edits never invalidate the
/// (lower) real indices of segments before it; only the final segment owns the
/// immovable document newline.
pub fn sync_core(current: &[ParaSegment], desired: &[Vec<Block>]) -> Vec<Request> {
    let n = current.len().min(desired.len());
    let mut out = Vec::new();
    for i in (0..n).rev() {
        out.extend(sync_unit(&EditUnit {
            start_index: current[i].start_index,
            cur: current[i].blocks.clone(),
            des: desired[i].clone(),
            // Every segment is bounded by a barrier or the doc end, so its last
            // paragraph's newline is structurally immovable (not just the doc's).
            final_newline: true,
        }));
    }
    out
}

/// Full incremental sync from two in-memory models. Assumes a **table-free**
/// document (paragraph indices computed as `offset+1`); for documents that
/// contain tables, drive the sync from `documents.get` via the API layer
/// (`sync_doc`), which supplies real per-segment indices.
pub fn sync_requests(current: &DocModel, desired: &DocModel) -> Vec<Request> {
    let mut idx = 1usize;
    let cur_segs: Vec<ParaSegment> = split_paragraph_runs(current)
        .into_iter()
        .map(|blocks| {
            let start_index = idx;
            idx += blocks.iter().map(|b| u16_len(&b.text()) + 1).sum::<usize>();
            ParaSegment { start_index, blocks }
        })
        .collect();
    sync_core(&cur_segs, &split_paragraph_runs(desired))
}

// ---------------------------------------------------------------------------
// Table-aware node sync (Phase 4: edit inside tables)
// ---------------------------------------------------------------------------

/// A table cell in the *current* document: its content-paragraph real start
/// index plus the runs it holds.
#[derive(Debug, Clone)]
pub struct CellEdit {
    pub start_index: usize,
    pub runs: Vec<Run>,
}

/// A top-level node of the *current* document, positioned with real indices.
#[derive(Debug, Clone)]
pub enum CurNode {
    Paras(ParaSegment),
    Table { start_index: usize, end_index: usize, cells: Vec<Vec<CellEdit>> },
    Rule { start_index: usize, end_index: usize },
    Toc { start_index: usize, end_index: usize },
}

fn cell_block(runs: &[Run]) -> Block {
    Block { kind: BlockKind::Normal, depth: 0, runs: runs.to_vec() }
}

enum DesNode<'a> {
    Paras(Vec<Block>),
    Table(&'a Table),
    Rule,
    Toc,
}

/// Group a desired model into the same paragraph-run / barrier shape that
/// `CurNode`s have (so they pair index-by-index).
fn desired_sequence(model: &DocModel) -> Vec<DesNode<'_>> {
    let mut seq = Vec::new();
    let mut cur = Vec::new();
    for node in model {
        match node {
            Node::Para(b) => cur.push(b.clone()),
            Node::Table(t) => {
                seq.push(DesNode::Paras(std::mem::take(&mut cur)));
                seq.push(DesNode::Table(t));
            }
            Node::Rule => {
                seq.push(DesNode::Paras(std::mem::take(&mut cur)));
                seq.push(DesNode::Rule);
            }
            Node::Toc => {
                seq.push(DesNode::Paras(std::mem::take(&mut cur)));
                seq.push(DesNode::Toc);
            }
        }
    }
    seq.push(DesNode::Paras(cur));
    seq
}

/// Real-index sync over positioned current nodes vs the desired model. Handles
/// paragraph edits anywhere and **cell content edits** for tables whose
/// dimensions are unchanged. All edit regions are processed highest-index-first.
///
/// Not yet handled (Phase 4b): tables whose row/column count changed, and tables
/// added or removed (which change the node sequence). Those are skipped.
pub fn sync_nodes(current: &[CurNode], desired: &DocModel) -> Vec<Request> {
    let des_seq = desired_sequence(desired);
    let n = current.len().min(des_seq.len());

    let mut units: Vec<EditUnit> = Vec::new();
    for i in 0..n {
        match (&current[i], &des_seq[i]) {
            (CurNode::Paras(seg), DesNode::Paras(des_blocks)) => units.push(EditUnit {
                start_index: seg.start_index,
                cur: seg.blocks.clone(),
                des: des_blocks.clone(),
                final_newline: true, // every segment is barrier/doc-bounded
            }),
            (CurNode::Table { cells: cur_rows, .. }, DesNode::Table(des_table)) => {
                let cur_cols = cur_rows.first().map_or(0, |r| r.len());
                let same_dims = cur_rows.len() == des_table.rows.len() && cur_cols == des_table.columns();
                if same_dims {
                    for (r, row) in cur_rows.iter().enumerate() {
                        for (c, cell) in row.iter().enumerate() {
                            units.push(EditUnit {
                                start_index: cell.start_index,
                                cur: vec![cell_block(&cell.runs)],
                                des: vec![cell_block(&des_table.rows[r][c])],
                                final_newline: true, // a cell always keeps one paragraph
                            });
                        }
                    }
                }
            }
            _ => {} // table added/removed: node sequence diverges (Phase 4b)
        }
    }

    // Apply highest-index regions first so lower indices stay valid.
    units.sort_by(|a, b| b.start_index.cmp(&a.start_index));
    units.iter().flat_map(sync_unit).collect()
}

/// A structural "barrier" between paragraph runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BarrierKind {
    Table,
    Rule,
    Toc,
}

fn current_barriers(nodes: &[CurNode]) -> Vec<(BarrierKind, usize, usize)> {
    nodes
        .iter()
        .filter_map(|n| match n {
            CurNode::Table { start_index, end_index, .. } => Some((BarrierKind::Table, *start_index, *end_index)),
            CurNode::Rule { start_index, end_index } => Some((BarrierKind::Rule, *start_index, *end_index)),
            CurNode::Toc { start_index, end_index } => Some((BarrierKind::Toc, *start_index, *end_index)),
            CurNode::Paras(_) => None,
        })
        .collect()
}

fn desired_barrier_nodes(model: &DocModel) -> Vec<&Node> {
    model.iter().filter(|n| n.is_barrier()).collect()
}

/// Reconcile the *barrier* sequence (tables + rules) between the current document
/// and the desired model. Returns raw structural ops (whole-table inserts and
/// barrier deletes, ordered highest-index-first) plus an adjusted desired model
/// with un-creatable rules dropped (Google's API can't insert a horizontal
/// rule), so the later positional content pass stays aligned.
///
/// `append_pos` is where a new table goes when nothing follows it (the document's
/// final paragraph start). Returns `(ops, adjusted_desired, warnings)`.
pub fn reconcile_barriers(
    current: &[CurNode],
    desired: &DocModel,
    append_pos: usize,
) -> (Vec<serde_json::Value>, DocModel, Vec<String>) {
    use serde_json::json;
    let cur_b = current_barriers(current);
    let des_nodes = desired_barrier_nodes(desired);
    let cur_kinds: Vec<BarrierKind> = cur_b.iter().map(|(k, _, _)| *k).collect();
    let des_kinds: Vec<BarrierKind> = des_nodes
        .iter()
        .map(|n| match n {
            Node::Table(_) => BarrierKind::Table,
            Node::Toc => BarrierKind::Toc,
            _ => BarrierKind::Rule,
        })
        .collect();

    // Align the two barrier sequences.
    let mut des_match: Vec<Option<usize>> = vec![None; des_kinds.len()];
    let mut cur_deleted = vec![false; cur_kinds.len()];
    for op in similar::capture_diff_slices(Algorithm::Myers, &cur_kinds, &des_kinds) {
        match op {
            DiffOp::Equal { old_index, new_index, len } => {
                for k in 0..len {
                    des_match[new_index + k] = Some(old_index + k);
                }
            }
            DiffOp::Delete { old_index, old_len, .. } => {
                for k in 0..old_len {
                    cur_deleted[old_index + k] = true;
                }
            }
            DiffOp::Insert { .. } => {}
            DiffOp::Replace { old_index, old_len, .. } => {
                for k in 0..old_len {
                    cur_deleted[old_index + k] = true;
                }
            }
        }
    }

    let mut tagged: Vec<(usize, serde_json::Value)> = Vec::new();
    let mut uncreatable = Vec::new();
    let mut warnings = Vec::new();

    // Deletions (current barriers with no match).
    for (i, &(_, start, end)) in cur_b.iter().enumerate() {
        if cur_deleted[i] {
            tagged.push((start, json!({ "deleteContentRange": { "range": { "startIndex": start, "endIndex": end } } })));
        }
    }

    // Insertions (desired barriers with no match).
    for (j, kind) in des_kinds.iter().enumerate() {
        if des_match[j].is_some() {
            continue;
        }
        match kind {
            BarrierKind::Rule => {
                uncreatable.push(j);
                warnings.push("a horizontal rule (`---`) can't be created via the API; left out".into());
            }
            BarrierKind::Toc => {
                uncreatable.push(j);
                warnings.push("a table of contents can't be created via the API; left out".into());
            }
            BarrierKind::Table => {
                // Insert before the next matched barrier, else append at the end.
                let pos = (j + 1..des_kinds.len())
                    .find_map(|k| des_match[k].map(|ci| cur_b[ci].1))
                    .unwrap_or(append_pos);
                if let Node::Table(t) = des_nodes[j] {
                    tagged.push((pos, json!({ "insertTable": { "rows": t.rows.len(), "columns": t.columns(), "location": { "index": pos } } })));
                }
            }
        }
    }

    // Apply highest-index-first so earlier ops don't shift later indices.
    tagged.sort_by(|a, b| b.0.cmp(&a.0));
    let ops = tagged.into_iter().map(|(_, v)| v).collect();

    // Drop the un-creatable rule nodes from the desired model (merging the
    // paragraph runs around them) so the content pass aligns with what's achievable.
    let drop: std::collections::HashSet<usize> = uncreatable.into_iter().collect();
    let mut adjusted = Vec::with_capacity(desired.len());
    let mut bidx = 0;
    for node in desired {
        if node.is_barrier() {
            // `drop` only ever holds un-creatable barriers (rules / TOCs); tables
            // are creatable and never dropped.
            let keep = !drop.contains(&bidx);
            bidx += 1;
            if keep {
                adjusted.push(node.clone());
            }
        } else {
            adjusted.push(node.clone());
        }
    }

    (ops, adjusted, warnings)
}

/// Raw `batchUpdate` requests that reshape each positionally-matched table to the
/// desired row/column count (the structural pass of the multi-step apply). After
/// these run and the document is re-fetched, `sync_nodes` fills/edits the cells.
///
/// Tables are processed highest-start-first so a table's growth never invalidates
/// an earlier table's start index. Rows/columns are appended at (or trimmed from)
/// the end; cell *positions* are reconciled afterwards by the content pass.
///
/// Adding/removing whole tables (which changes the node count) isn't handled yet.
pub fn table_resize_requests(current: &[CurNode], desired: &DocModel) -> Vec<serde_json::Value> {
    use serde_json::json;
    let des_seq = desired_sequence(desired);
    let n = current.len().min(des_seq.len());

    // (start_index, cur_rows, cur_cols, des_rows, des_cols) for matched tables.
    let mut tables = Vec::new();
    for i in 0..n {
        if let (CurNode::Table { start_index, cells, .. }, DesNode::Table(t)) = (&current[i], &des_seq[i]) {
            let cur_cols = cells.first().map_or(0, |r| r.len());
            tables.push((*start_index, cells.len(), cur_cols, t.rows.len(), t.columns()));
        }
    }
    tables.sort_by(|a, b| b.0.cmp(&a.0));

    let mut ops = Vec::new();
    for (start, cur_rows, cur_cols, des_rows, des_cols) in tables {
        let loc = |r: usize, c: usize| json!({ "tableStartLocation": { "index": start }, "rowIndex": r, "columnIndex": c });
        // Append rows below the (growing) last row, or trim from the bottom.
        for i in 0..des_rows.saturating_sub(cur_rows) {
            ops.push(json!({ "insertTableRow": { "tableCellLocation": loc(cur_rows - 1 + i, 0), "insertBelow": true } }));
        }
        for i in 0..cur_rows.saturating_sub(des_rows) {
            ops.push(json!({ "deleteTableRow": { "tableCellLocation": loc(cur_rows - 1 - i, 0) } }));
        }
        // Append columns right of the (growing) last column, or trim from the right.
        for i in 0..des_cols.saturating_sub(cur_cols) {
            ops.push(json!({ "insertTableColumn": { "tableCellLocation": loc(0, cur_cols - 1 + i), "insertRight": true } }));
        }
        for i in 0..cur_cols.saturating_sub(des_cols) {
            ops.push(json!({ "deleteTableColumn": { "tableCellLocation": loc(0, cur_cols - 1 - i) } }));
        }
    }
    ops
}

/// Wrap requests into the full `batchUpdate` request body.
pub fn batch_update_body(reqs: &[Request]) -> serde_json::Value {
    serde_json::json!({ "requests": reqs })
}

// ---------------------------------------------------------------------------
// Optimistic-concurrency retry
// ---------------------------------------------------------------------------

/// How many times to retry a sync when the document is edited under us.
pub const MAX_CONFLICT_RETRIES: usize = 5;

/// True if `err` is Google's stale-`requiredRevisionId` rejection — i.e. the
/// document was modified between our read and our write.
pub fn is_revision_conflict(err: &str) -> bool {
    err.contains("does not match the latest revision")
}

/// Run a fetch→compute→write step, retrying when it's rejected because the
/// document moved under us. `op` must re-read the document each call (so the
/// retry recomputes against the latest revision). Non-conflict errors and the
/// final conflict (after [`MAX_CONFLICT_RETRIES`]) are returned as-is.
///
/// This is the tool's concurrency story: writes are revision-locked, so a
/// concurrent edit can only ever cause a clean rejection — never a lost update —
/// and the loop simply re-diffs against the new state.
pub fn retry_on_conflict<T, F>(mut op: F) -> Result<T, String>
where
    F: FnMut() -> Result<T, String>,
{
    let mut result = op();
    let mut attempts = 1;
    while attempts < MAX_CONFLICT_RETRIES && matches!(&result, Err(e) if is_revision_conflict(e)) {
        result = op();
        attempts += 1;
    }
    result
}

// ---------------------------------------------------------------------------
// Tests — offline, no Google credentials required.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn b(kind: BlockKind, text: &str) -> Block {
        bd(kind, text, 0)
    }
    fn bd(kind: BlockKind, text: &str, depth: u8) -> Block {
        Block { kind, depth, runs: vec![run(text, TextStyle::default())] }
    }
    // Node (paragraph) builders.
    fn p(kind: BlockKind, text: &str) -> Node {
        Node::Para(b(kind, text))
    }
    fn pd(kind: BlockKind, text: &str, depth: u8) -> Node {
        Node::Para(bd(kind, text, depth))
    }
    fn pruns(kind: BlockKind, runs: Vec<Run>) -> Node {
        Node::Para(Block { kind, depth: 0, runs })
    }
    fn para(n: &Node) -> &Block {
        n.as_para().unwrap()
    }
    fn run(text: &str, style: TextStyle) -> Run {
        Run { text: text.into(), style, link: None, object: None }
    }
    fn linked(text: &str, style: TextStyle, url: &str) -> Run {
        Run { text: text.into(), style, link: Some(url.into()), object: None }
    }
    fn bold() -> TextStyle {
        TextStyle { bold: true, ..Default::default() }
    }

    #[test]
    fn markdown_parses_structure_and_inline() {
        let model = markdown_to_model("# Title\n- a\n1. one\nplain **b** text");
        assert_eq!(model[0], p(BlockKind::Heading(1), "Title"));
        assert_eq!(model[1], p(BlockKind::Bullet, "a"));
        assert_eq!(model[2], p(BlockKind::Number, "one"));
        assert_eq!(
            para(&model[3]).runs,
            vec![run("plain ", TextStyle::default()), run("b", bold()), run(" text", TextStyle::default())]
        );
        assert_eq!(para(&model[3]).text(), "plain b text"); // markers stripped
    }

    #[test]
    fn markdown_parses_a_table_node_between_paragraphs() {
        let model = markdown_to_model("intro\n\n| a | b |\n| --- | --- |\n| 1 | 2 |\n\nafter");
        assert_eq!(model.len(), 3);
        assert_eq!(model[0], p(BlockKind::Normal, "intro"));
        assert_eq!(model[2], p(BlockKind::Normal, "after"));
        let Node::Table(t) = &model[1] else { panic!("expected a table node") };
        assert_eq!(t.columns(), 2);
        assert_eq!(t.rows.len(), 2); // header + one body row
    }

    #[test]
    fn parses_bold_italic_strike() {
        let r = parse_inline("a **b** *i* ~~s~~");
        let styles: Vec<TextStyle> = r.iter().map(|x| x.style).collect();
        assert!(styles.contains(&TextStyle { bold: true, ..Default::default() }));
        assert!(styles.contains(&TextStyle { italic: true, ..Default::default() }));
        assert!(styles.contains(&TextStyle { strikethrough: true, ..Default::default() }));
    }

    #[test]
    fn deferred_marks_stay_literal() {
        // <u>/<sup>/<sub> are not parsed; they pass through as text.
        let r = parse_inline("x<sup>2</sup>");
        assert_eq!(r, vec![run("x<sup>2</sup>", TextStyle::default())]);
    }

    #[test]
    fn parses_links_including_marked_text() {
        assert_eq!(
            parse_inline("see [the docs](https://example.com/x) now"),
            vec![
                run("see ", TextStyle::default()),
                linked("the docs", TextStyle::default(), "https://example.com/x"),
                run(" now", TextStyle::default()),
            ]
        );
        // A bold link keeps both attributes; clean text drops the markup.
        let r = parse_inline("[**bold**](u)");
        assert_eq!(r, vec![linked("bold", bold(), "u")]);
    }

    #[test]
    fn parses_and_handles_inline_image() {
        // Parse a placeholder into a 1-char object run (image and page break).
        let r = parse_inline("a ![](gdoc:image/kix.7gd) b ![](gdoc:pagebreak) c");
        assert_eq!(r[1], Run::object("image/kix.7gd"));
        assert_eq!(r[3], Run::object("pagebreak"));
        assert_eq!(r[1].text, OBJECT_SENTINEL.to_string());

        // Removing the placeholder deletes the image's single index (2: "a"[1] obj[2] "b"[3]).
        let with = vec![pruns(BlockKind::Normal, vec![run("a", TextStyle::default()), Run::object("img1"), run("b", TextStyle::default())])];
        let without = vec![pruns(BlockKind::Normal, vec![run("ab", TextStyle::default())])];
        let del = sync_requests(&with, &without);
        assert!(del.iter().any(|r| matches!(r, Request::DeleteContentRange(d) if d.range.start_index == 2 && d.range.end_index == 3)));

        // Can't create: adding a placeholder never inserts a literal sentinel.
        let create = sync_requests(&without, &with);
        assert!(create.iter().all(|r| !matches!(r, Request::InsertText(it) if it.text.contains(OBJECT_SENTINEL))));
    }

    #[test]
    fn adding_a_link_emits_clear_then_set() {
        let current = vec![p(BlockKind::Normal, "go home now")];
        let desired = vec![pruns(
            BlockKind::Normal,
            vec![run("go ", TextStyle::default()), linked("home", TextStyle::default(), "/h"), run(" now", TextStyle::default())],
        )];
        let links: Vec<_> = sync_requests(&current, &desired)
            .iter()
            .filter_map(|r| match r {
                Request::UpdateTextStyle(u) => Some((u.range.clone(), u.text_style.link.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            links,
            vec![
                (Range { start_index: 1, end_index: 12 }, None),                     // clear
                (Range { start_index: 4, end_index: 8 }, Some(Link { url: "/h".into() })), // set on "home"
            ]
        );
    }

    #[test]
    fn nested_marks_combine() {
        let r = parse_inline("***x***"); // bold + italic
        assert_eq!(r, vec![run("x", TextStyle { bold: true, italic: true, ..Default::default() })]);
    }

    #[test]
    fn unmatched_marker_stays_literal() {
        assert_eq!(parse_inline("2 * 3"), vec![run("2 * 3", TextStyle::default())]);
    }

    #[test]
    fn append_inserts_before_final_newline() {
        let reqs = doc_text_requests("a\n", "a\nb\n");
        assert_eq!(
            reqs,
            vec![Request::InsertText(InsertText {
                location: Location { index: 2 },
                text: "\nb".into(),
            })]
        );
        assert_eq!(apply::apply("a\n", &reqs), "a\nb\n");
    }

    #[test]
    fn trailing_delete_eats_preceding_newline() {
        let reqs = doc_text_requests("a\nb\n", "a\n");
        assert_eq!(apply::apply("a\nb\n", &reqs), "a\n");
    }

    #[test]
    fn heading_level_change_emits_paragraph_style_only() {
        let reqs = sync_requests(&vec![p(BlockKind::Heading(1), "Title")], &vec![p(BlockKind::Heading(2), "Title")]);
        assert!(reqs.iter().all(|r| matches!(
            r,
            Request::DeleteParagraphBullets(_) | Request::UpdateParagraphStyle(_)
        )));
        assert!(reqs.iter().any(|r| matches!(r, Request::UpdateParagraphStyle(u) if u.paragraph_style.named_style_type == "HEADING_2")));
    }

    #[test]
    fn text_only_edit_no_marks_emits_no_style() {
        let reqs = sync_requests(&vec![p(BlockKind::Normal, "hello world")], &vec![p(BlockKind::Normal, "hello brave world")]);
        assert!(reqs.iter().all(|r| matches!(r, Request::InsertText(_))));
        assert_eq!(reqs.len(), 1);
    }

    #[test]
    fn adding_bold_emits_clear_then_set() {
        let current = vec![p(BlockKind::Normal, "make me bold")];
        let desired = vec![pruns(BlockKind::Normal, vec![run("make me ", TextStyle::default()), run("bold", bold())])];
        let reqs = sync_requests(&current, &desired);
        // No text change (same clean text); just a clear over [1,13) and bold over [9,13).
        let styles: Vec<_> = reqs
            .iter()
            .filter_map(|r| match r {
                Request::UpdateTextStyle(u) => Some((u.range.clone(), u.text_style.bold)),
                _ => None,
            })
            .collect();
        assert_eq!(
            styles,
            vec![
                (Range { start_index: 1, end_index: 13 }, false), // clear
                (Range { start_index: 9, end_index: 13 }, true),  // set bold on "bold"
            ]
        );
    }

    #[test]
    fn turning_paragraph_into_bullet_keeps_inline_clean() {
        // Only the kind changes; runs are identical, so no inline requests.
        let reqs = sync_requests(&vec![p(BlockKind::Normal, "item")], &vec![p(BlockKind::Bullet, "item")]);
        assert!(reqs.iter().all(|r| !matches!(r, Request::UpdateTextStyle(_))));
    }

    #[test]
    fn nested_lists_parse_depth() {
        let m = markdown_to_model("- a\n  - b\n    - c");
        assert_eq!(m, vec![pd(BlockKind::Bullet, "a", 0), pd(BlockKind::Bullet, "b", 1), pd(BlockKind::Bullet, "c", 2)]);
    }

    #[test]
    fn relative_nesting_matches_google_export() {
        // Google indents numbered nesting by 3 columns/level, bullets by 2 — the
        // stack-based parser must read both as depths 0,1,2.
        let depths = |md: &str| markdown_to_model(md).iter().map(|n| para(n).depth).collect::<Vec<_>>();
        assert_eq!(depths("1. a\n   1. b\n      1. c"), vec![0, 1, 2]);
        assert_eq!(depths("* a\n  * b\n    * c"), vec![0, 1, 2]);
        assert_eq!(depths("- a\n  - b\n- c"), vec![0, 1, 0]); // dedent back to top
    }

    #[test]
    fn parses_gfm_table_with_inline_marks() {
        let lines = ["| Name | Role |", "| :---- | :---- |", "| Ada | **eng** |", "| Bob | [lead](/b) |"];
        assert!(is_table_start(&lines));
        let t = parse_table(&lines).unwrap();
        assert_eq!(t.columns(), 2);
        assert_eq!(t.rows.len(), 3);
        assert_eq!(t.rows[0], vec![vec![run("Name", TextStyle::default())], vec![run("Role", TextStyle::default())]]);
        assert_eq!(t.rows[1][1], vec![run("eng", bold())]);
        assert_eq!(t.rows[2][1], vec![linked("lead", TextStyle::default(), "/b")]);
    }

    #[test]
    fn non_table_is_rejected() {
        // A pipe row without a separator line is not a table.
        assert!(!is_table_start(&["| a | b |", "| c | d |"]));
        // Plain text isn't a table.
        assert!(!is_table_start(&["just text", "more text"]));
        // A short body row pads to the column count.
        let t = parse_table(&["| a | b | c |", "|---|---|---|", "| x |"]).unwrap();
        assert_eq!(t.rows[1], vec![vec![run("x", TextStyle::default())], vec![], vec![]]);
    }

    #[test]
    fn classifies_and_retries_revision_conflicts() {
        use std::cell::Cell;
        let conflict = || Err::<i32, String>("HTTP 400: ... does not match the latest revision.".into());
        assert!(is_revision_conflict(&conflict().unwrap_err()));
        assert!(!is_revision_conflict("HTTP 403: permission denied"));

        // Recovers on the second attempt.
        let n = Cell::new(0);
        let r = retry_on_conflict(|| {
            n.set(n.get() + 1);
            if n.get() == 1 { conflict() } else { Ok(7) }
        });
        assert_eq!(r, Ok(7));
        assert_eq!(n.get(), 2);

        // A non-conflict error is returned immediately (no retry).
        let m = Cell::new(0);
        let r2: Result<i32, String> = retry_on_conflict(|| {
            m.set(m.get() + 1);
            Err("boom".into())
        });
        assert_eq!(r2, Err("boom".into()));
        assert_eq!(m.get(), 1);

        // Persistent conflict gives up after MAX_CONFLICT_RETRIES.
        let k = Cell::new(0);
        let r3 = retry_on_conflict(|| {
            k.set(k.get() + 1);
            conflict()
        });
        assert!(is_revision_conflict(&r3.unwrap_err()));
        assert_eq!(k.get(), MAX_CONFLICT_RETRIES);
    }

    #[test]
    fn serialize_round_trips_through_parse() {
        let md = "# Title\n\nProse with **bold**, *italic*, ~~strike~~, ***both***, a [link](u), \
                  and ![](gdoc:image/x) here.\n\n* a\n  * b\n* c\n\n1. one\n   1. two\n\n\
                  | h1 | h2 |\n| --- | --- |\n| a | **b** |\n\n---\n\nlast line";
        let model = markdown_to_model(md);
        let out = model_to_markdown(&model);
        assert_eq!(markdown_to_model(&out), model, "round-trip mismatch; serialized:\n{out}");
    }

    #[test]
    fn serialize_escapes_inline_specials() {
        // Text containing markdown specials must round-trip, not become markup.
        let model = markdown_to_model("a \\*literal\\* star and a \\[bracket");
        let out = model_to_markdown(&model);
        assert_eq!(markdown_to_model(&out), model, "serialized: {out}");
        assert_eq!(model[0].as_para().unwrap().text(), "a *literal* star and a [bracket");
    }

    #[test]
    fn checkbox_list_parses_and_round_trips() {
        let m = markdown_to_model("- [ ] One\n- [x] Two\n  - [ ] Nested");
        assert_eq!(
            m,
            vec![
                pd(BlockKind::Checkbox, "One", 0),
                pd(BlockKind::Checkbox, "Two", 0), // checked state dropped
                pd(BlockKind::Checkbox, "Nested", 1),
            ]
        );
        // Serializes as unchecked, and round-trips through the model.
        let out = model_to_markdown(&m);
        assert!(out.contains("- [ ] One"), "{out}");
        assert_eq!(markdown_to_model(&out), m);
    }

    #[test]
    fn markdown_parses_horizontal_rule() {
        assert_eq!(markdown_to_model("a\n\n---\n\nb"), vec![p(BlockKind::Normal, "a"), Node::Rule, p(BlockKind::Normal, "b")]);
        assert_eq!(markdown_to_model("***"), vec![Node::Rule]);
        assert_eq!(markdown_to_model("___"), vec![Node::Rule]);
        // `***word***` is bold+italic inline, not a rule.
        assert!(matches!(markdown_to_model("***word***")[0], Node::Para(_)));
    }

    #[test]
    fn utf16_indices_for_astral_chars() {
        // 😀 and 𝐀 are 1 scalar but 2 UTF-16 units each, the unit Docs counts.
        assert_eq!(u16_len("A😀B𝐀C"), 7);
        // An edit *after* an emoji must land at the UTF-16 offset. "A😀X" -> "A😀Y":
        // 😀 ends at UTF-16 offset 3, so the replace is at doc index 4, not 3.
        let reqs = diff_to_requests("A\u{1F600}X", "A\u{1F600}Y");
        let dels: Vec<_> = reqs.iter().filter_map(|r| match r { Request::DeleteContentRange(d) => Some(d.range.clone()), _ => None }).collect();
        let inss: Vec<_> = reqs.iter().filter_map(|r| match r { Request::InsertText(i) => Some(i.location.index), _ => None }).collect();
        assert_eq!(dels, vec![Range { start_index: 4, end_index: 5 }], "delete X at UTF-16 doc index 4");
        assert_eq!(inss, vec![4], "insert Y at UTF-16 doc index 4");

        // paragraph_ranges accounts for astral width: "😀" para spans 2 units + \n.
        let r = paragraph_ranges(&[b(BlockKind::Normal, "\u{1F600}")]);
        assert_eq!(r[0], Range { start_index: 1, end_index: 4 }); // [1, 1+2+1)
    }

    #[test]
    fn underscore_emphasis_aliases() {
        let italic = TextStyle { italic: true, ..Default::default() };
        let bolditalic = TextStyle { bold: true, italic: true, ..Default::default() };
        // `_`, `__`, `___` behave like `*`, `**`, `***`.
        assert_eq!(parse_inline("_x_"), vec![run("x", italic)]);
        assert_eq!(parse_inline("__x__"), vec![run("x", bold())]);
        assert_eq!(parse_inline("___x___"), vec![run("x", bolditalic)]);
        // Bounded by spaces / punctuation.
        assert_eq!(
            parse_inline("a _b_ c"),
            vec![run("a ", TextStyle::default()), run("b", italic), run(" c", TextStyle::default())]
        );
        // The serializer normalizes to the `*` forms (Google's export flavor).
        assert_eq!(run_md(&run("x", italic), false), "*x*");
    }

    #[test]
    fn underscores_intraword_stay_literal() {
        // Identifiers and filenames must not become emphasis.
        for s in ["snake_case_name", "foo_bar.md", "a_b_c_d", "my__dunder__name"] {
            assert_eq!(parse_inline(s), vec![run(s, TextStyle::default())], "{s} should be literal");
        }
        // Asterisks are unaffected (still intraword-capable).
        assert_eq!(
            parse_inline("a*b*c"),
            vec![run("a", TextStyle::default()), run("b", TextStyle { italic: true, ..Default::default() }), run("c", TextStyle::default())]
        );
    }

    #[test]
    fn empty_heading_and_hash_paragraph_round_trip() {
        // An empty heading survives the trailing-space trim.
        let h = vec![pruns(BlockKind::Heading(1), vec![])];
        assert_eq!(markdown_to_model(&model_to_markdown(&h)), h);
        // A literal "#" paragraph is escaped so it isn't read back as a heading.
        let hp = vec![pruns(BlockKind::Normal, vec![run("#", TextStyle::default())])];
        assert_eq!(markdown_to_model(&model_to_markdown(&hp)), hp);
        // …and a literal TOC-placeholder paragraph stays a paragraph.
        let t = vec![pruns(BlockKind::Normal, vec![run(TOC_PLACEHOLDER, TextStyle::default())])];
        assert_eq!(markdown_to_model(&model_to_markdown(&t)), t);
    }

    #[test]
    fn toc_parses_serializes_and_reconciles() {
        let m = markdown_to_model("intro\n\n<!-- gdoc:toc -->\n\n# Heading");
        assert_eq!(m, vec![p(BlockKind::Normal, "intro"), Node::Toc, p(BlockKind::Heading(1), "Heading")]);
        assert!(model_to_markdown(&m).contains("<!-- gdoc:toc -->"));
        assert_eq!(markdown_to_model(&model_to_markdown(&m)), m);

        // A TOC dropped from the desired model is deleted by range.
        let cur = vec![
            CurNode::Paras(ParaSegment { start_index: 1, blocks: vec![] }),
            CurNode::Toc { start_index: 5, end_index: 30 },
            CurNode::Paras(ParaSegment { start_index: 30, blocks: vec![] }),
        ];
        let (ops, _adj, warns) = reconcile_barriers(&cur, &vec![p(BlockKind::Normal, "x")], 1);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0]["deleteContentRange"]["range"]["startIndex"], 5);
        assert_eq!(ops[0]["deleteContentRange"]["range"]["endIndex"], 30);
        assert!(warns.is_empty());

        // A TOC the API can't create is warned + dropped from the target.
        let cur2 = vec![CurNode::Paras(ParaSegment { start_index: 1, blocks: vec![] })];
        let (ops2, adj2, warns2) = reconcile_barriers(&cur2, &vec![p(BlockKind::Normal, "a"), Node::Toc, p(BlockKind::Normal, "b")], 1);
        assert!(ops2.is_empty());
        assert_eq!(warns2.len(), 1);
        assert_eq!(adj2, vec![p(BlockKind::Normal, "a"), p(BlockKind::Normal, "b")]);
    }

    #[test]
    fn reconcile_barriers_deletes_rules_and_warns_on_create() {
        // Delete a rule the desired model dropped.
        let cur = vec![
            CurNode::Paras(ParaSegment { start_index: 1, blocks: vec![] }),
            CurNode::Rule { start_index: 5, end_index: 7 },
            CurNode::Paras(ParaSegment { start_index: 7, blocks: vec![] }),
        ];
        let (ops, _adj, warns) = reconcile_barriers(&cur, &vec![p(BlockKind::Normal, "a")], 1);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0]["deleteContentRange"]["range"]["startIndex"], 5);
        assert_eq!(ops[0]["deleteContentRange"]["range"]["endIndex"], 7);
        assert!(warns.is_empty());

        // A rule the API can't create: warn + drop it from the target.
        let cur2 = vec![CurNode::Paras(ParaSegment { start_index: 1, blocks: vec![] })];
        let desired2 = vec![p(BlockKind::Normal, "a"), Node::Rule, p(BlockKind::Normal, "b")];
        let (ops2, adj2, warns2) = reconcile_barriers(&cur2, &desired2, 1);
        assert!(ops2.is_empty());
        assert_eq!(warns2.len(), 1);
        assert_eq!(adj2, vec![p(BlockKind::Normal, "a"), p(BlockKind::Normal, "b")]);
    }

    #[test]
    fn table_resize_emits_row_and_column_ops() {
        let ce = |i| CellEdit { start_index: i, runs: vec![] };
        let cur = vec![
            CurNode::Paras(ParaSegment { start_index: 1, blocks: vec![] }),
            CurNode::Table { start_index: 5, end_index: 20, cells: vec![vec![ce(7), ce(10)], vec![ce(13), ce(16)]] },
            CurNode::Paras(ParaSegment { start_index: 25, blocks: vec![] }),
        ];
        // Desired table is 3 rows x 1 col: add a row, drop a column (each row one cell).
        let desired = vec![Node::Table(Table { rows: vec![vec![vec![]], vec![vec![]], vec![vec![]]] })];

        let ops = table_resize_requests(&cur, &desired);
        // One row added (below row 1), one column removed (column 1).
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0]["insertTableRow"]["tableCellLocation"]["rowIndex"], 1);
        assert_eq!(ops[0]["insertTableRow"]["tableCellLocation"]["tableStartLocation"]["index"], 5);
        assert_eq!(ops[0]["insertTableRow"]["insertBelow"], true);
        assert_eq!(ops[1]["deleteTableColumn"]["tableCellLocation"]["columnIndex"], 1);
    }

    #[test]
    fn text_style_serializes_to_google_shape() {
        // A bold link: link present; fields always lists `link` so absent links clear.
        let reqs = vec![update_text_style(
            Range { start_index: 9, end_index: 13 },
            TextStyle { bold: true, ..Default::default() },
            Some("https://x.test"),
        )];
        assert_eq!(
            batch_update_body(&reqs),
            serde_json::json!({
                "requests": [
                    { "updateTextStyle": {
                        "range": { "startIndex": 9, "endIndex": 13 },
                        "textStyle": { "bold": true, "italic": false, "strikethrough": false,
                                       "link": { "url": "https://x.test" } },
                        "fields": "bold,italic,strikethrough,link" } }
                ]
            })
        );
    }
}

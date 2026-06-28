//! Minimal Google Docs API client for integration tests (and, later, the daemon).
//!
//! Auth reuses whatever Application Default Credentials are configured — we just
//! shell out to `gcloud auth application-default print-access-token` and send it
//! as a bearer token. The credentials need the `documents` scope (Docs
//! read/write). No OAuth flow of our own. Writes are revision-locked via
//! `writeControl.requiredRevisionId`, which is our optimistic-concurrency story:
//! if the doc moved under us, the write is rejected and the caller re-diffs.

use crate::{
    normalize_runs, reconcile_barriers, retry_on_conflict, sync_nodes, table_resize_requests, Block,
    BlockKind, CellEdit, CurNode, DocModel, Node, ParaSegment, Request, Run, Table, TextStyle,
};
use serde_json::{json, Value};
use std::process::Command;

const DOCS: &str = "https://docs.googleapis.com/v1/documents";

/// Mint an access token from Application Default Credentials. Never logged.
pub fn access_token() -> String {
    let out = Command::new("gcloud")
        .args(["auth", "application-default", "print-access-token"])
        .output()
        .expect("failed to launch gcloud");
    assert!(
        out.status.success(),
        "gcloud token failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

pub struct Docs {
    token: String,
}

impl Default for Docs {
    fn default() -> Self {
        Self::new()
    }
}

impl Docs {
    pub fn new() -> Self {
        Docs { token: access_token() }
    }

    fn bearer(&self) -> String {
        format!("Bearer {}", self.token)
    }

    /// `documents.get` — also our read-back oracle for assertions.
    pub fn get(&self, id: &str) -> Value {
        ureq::get(&format!("{DOCS}/{id}"))
            .set("Authorization", &self.bearer())
            .call()
            .expect("documents.get failed")
            .into_json()
            .expect("documents.get returned invalid JSON")
    }

    /// `documents.batchUpdate`, locked to `required_revision_id`. Returns the
    /// raw response on success, or a human-readable error (the API's message is
    /// preserved — e.g. a stale-revision conflict).
    pub fn batch_update(
        &self,
        id: &str,
        requests: &[Request],
        required_revision_id: &str,
    ) -> Result<Value, String> {
        let body = json!({
            "requests": requests,
            "writeControl": { "requiredRevisionId": required_revision_id },
        });
        self.post_batch(id, body)
    }

    /// Like `batch_update`, but with raw JSON requests (for requests our typed
    /// `Request` enum doesn't model yet, e.g. `insertTable`). Used by tests.
    pub fn batch_raw(&self, id: &str, requests: Value, required_revision_id: &str) -> Result<Value, String> {
        self.post_batch(id, json!({
            "requests": requests,
            "writeControl": { "requiredRevisionId": required_revision_id },
        }))
    }

    fn post_batch(&self, id: &str, body: Value) -> Result<Value, String> {
        match ureq::post(&format!("{DOCS}/{id}:batchUpdate"))
            .set("Authorization", &self.bearer())
            .send_json(body)
        {
            Ok(resp) => Ok(resp.into_json().unwrap_or(Value::Null)),
            Err(ureq::Error::Status(code, resp)) => Err(format!(
                "HTTP {code}: {}",
                resp.into_string().unwrap_or_default()
            )),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Create a blank document, returning its id. (`documents` scope.)
    pub fn create(&self, title: &str) -> String {
        let resp: Value = ureq::post(DOCS)
            .set("Authorization", &self.bearer())
            .send_json(json!({ "title": title }))
            .expect("documents.create failed")
            .into_json()
            .expect("documents.create returned invalid JSON");
        resp["documentId"].as_str().expect("no documentId").to_string()
    }
}

/// Convert a `documents.get` response into our block model. A paragraph with a
/// `bullet` is a list item — ordered if its list's nesting level defines a
/// `glyphType` (e.g. DECIMAL), otherwise unordered (a `glyphSymbol` like ●).
/// Headings come from `namedStyleType`; everything else is Normal.
pub fn document_to_model(doc: &Value) -> DocModel {
    let mut nodes = Vec::new();
    let empty = Vec::new();
    for el in doc["body"]["content"].as_array().unwrap_or(&empty) {
        if let Some(p) = el.get("paragraph") {
            if is_rule_paragraph(p) {
                nodes.push(Node::Rule);
            } else {
                nodes.push(Node::Para(para_block(p, doc)));
            }
        } else if el.get("table").is_some() {
            nodes.push(Node::Table(read_table(el)));
        } else if el.get("tableOfContents").is_some() {
            nodes.push(Node::Toc);
        }
    }
    nodes
}

fn opaque_label(key: &str) -> &'static str {
    match key {
        "inlineObjectElement" => "image",
        "footnoteReference" => "footnote",
        "pageBreak" => "page break",
        "columnBreak" => "column break",
        "equation" => "equation",
        "person" => "smart chip (person)",
        "richLink" => "smart chip (link)",
        "autoText" => "auto text",
        "tableOfContents" => "table of contents",
        "sectionBreak" => "section break",
        _ => "unsupported element",
    }
}

/// Detect index-occupying content the model doesn't represent (images, footnotes,
/// page/column/section breaks, equations, smart chips, TOC, …). Returns friendly
/// names of what was found. We can't edit such a document safely — the unmodeled
/// indices would shift our edits — so the sync refuses rather than corrupt it.
/// Tables, horizontal rules, and the document's leading section break are fine.
pub fn unsupported_features(doc: &Value) -> Vec<String> {
    use std::collections::BTreeSet;
    let empty = Vec::new();
    let content = doc["body"]["content"].as_array().unwrap_or(&empty);
    let mut found: BTreeSet<&'static str> = BTreeSet::new();
    for (i, el) in content.iter().enumerate() {
        if let Some(p) = el.get("paragraph") {
            for e in p["elements"].as_array().unwrap_or(&empty) {
                for key in e.as_object().into_iter().flat_map(|o| o.keys()) {
                    match key.as_str() {
                        // textRun, horizontal rules, and the inline objects we model.
                        "textRun" | "startIndex" | "endIndex" | "horizontalRule"
                        | "inlineObjectElement" | "footnoteReference" | "pageBreak"
                        | "columnBreak" | "autoText" => {}
                        other => {
                            found.insert(opaque_label(other));
                        }
                    }
                }
            }
        } else if el.get("table").is_some() || el.get("tableOfContents").is_some() {
            // supported (tables; TOC is a preserved barrier)
        } else if el.get("sectionBreak").is_some() {
            if i != 0 {
                found.insert("section break"); // the leading one is the document default
            }
        } else {
            for key in el.as_object().into_iter().flat_map(|o| o.keys()) {
                if key != "startIndex" && key != "endIndex" {
                    found.insert(opaque_label(key));
                }
            }
        }
    }
    found.into_iter().map(String::from).collect()
}

/// A paragraph that is a horizontal rule — it contains a `horizontalRule` element.
fn is_rule_paragraph(p: &Value) -> bool {
    p["elements"]
        .as_array()
        .is_some_and(|els| els.iter().any(|e| e.get("horizontalRule").is_some()))
}

/// Convert one paragraph JSON object into a block (kind + depth + runs).
fn para_block(p: &Value, doc: &Value) -> Block {
    let (kind, depth) = if let Some(bullet) = p.get("bullet") {
        (list_kind(doc, bullet), bullet["nestingLevel"].as_u64().unwrap_or(0) as u8)
    } else {
        let named = p["paragraphStyle"]["namedStyleType"].as_str().unwrap_or("NORMAL_TEXT");
        let kind = named
            .strip_prefix("HEADING_")
            .and_then(|n| n.parse().ok())
            .map(BlockKind::Heading)
            .unwrap_or(BlockKind::Normal);
        (kind, 0)
    };
    Block { kind, depth, runs: read_runs(p) }
}

/// Split `documents.get` into positioned nodes: paragraph segments (tagged with
/// their real start index) and tables (each cell tagged with its content
/// paragraph's real start index). This is what the real-index sync needs so
/// paragraph and cell edits land at their true positions.
pub fn current_nodes(doc: &Value) -> Vec<CurNode> {
    let empty = Vec::new();
    let mut nodes = Vec::new();
    let mut blocks: Vec<Block> = Vec::new();
    let mut start: Option<usize> = None;
    for el in doc["body"]["content"].as_array().unwrap_or(&empty) {
        let el_start = el["startIndex"].as_u64().unwrap_or(1) as usize;
        let el_end = el["endIndex"].as_u64().unwrap_or(el_start as u64) as usize;
        if let Some(p) = el.get("paragraph").filter(|p| !is_rule_paragraph(p)) {
            start.get_or_insert(el_start);
            blocks.push(para_block(p, doc));
        } else if el.get("paragraph").is_some_and(is_rule_paragraph) {
            // A horizontal rule is a barrier, like a table.
            nodes.push(CurNode::Paras(ParaSegment {
                start_index: start.take().unwrap_or(el_start),
                blocks: std::mem::take(&mut blocks),
            }));
            nodes.push(CurNode::Rule { start_index: el_start, end_index: el_end });
        } else if let Some(table) = el.get("table") {
            nodes.push(CurNode::Paras(ParaSegment {
                start_index: start.take().unwrap_or(el_start),
                blocks: std::mem::take(&mut blocks),
            }));
            let mut rows = Vec::new();
            for row in table["tableRows"].as_array().unwrap_or(&empty) {
                let mut cells = Vec::new();
                for c in row["tableCells"].as_array().unwrap_or(&empty) {
                    let content = &c["content"][0];
                    let cell_start = content["startIndex"].as_u64().unwrap_or(0) as usize;
                    cells.push(CellEdit { start_index: cell_start, runs: read_runs(&content["paragraph"]) });
                }
                rows.push(cells);
            }
            let end_index = el["endIndex"].as_u64().unwrap_or(el_start as u64) as usize;
            nodes.push(CurNode::Table { start_index: el_start, end_index, cells: rows });
        } else if el.get("tableOfContents").is_some() {
            // A table of contents is a preserved barrier, like a rule.
            nodes.push(CurNode::Paras(ParaSegment {
                start_index: start.take().unwrap_or(el_start),
                blocks: std::mem::take(&mut blocks),
            }));
            nodes.push(CurNode::Toc { start_index: el_start, end_index: el_end });
        }
    }
    nodes.push(CurNode::Paras(ParaSegment { start_index: start.unwrap_or(1), blocks }));
    nodes
}

/// Real-index incremental sync: diff `documents.get` against the desired model,
/// editing paragraphs and table cells at their true indices.
pub fn sync_doc(doc: &Value, desired: &DocModel) -> Vec<Request> {
    sync_nodes(&current_nodes(doc), desired)
}

/// Sync the desired model to the live document, retrying cleanly if a concurrent
/// edit moves the document under us. Returns any warnings (e.g. a `---` that
/// couldn't be created). See [`sync_apply_once`] for the steps.
pub fn sync_apply(docs: &Docs, id: &str, desired: &DocModel) -> Result<Vec<String>, String> {
    retry_on_conflict(|| sync_apply_once(docs, id, desired))
}

/// One attempt of the multi-step sync (no predicted indices):
///   A. reconcile the **barrier sequence** — insert/delete whole tables and
///      delete horizontal rules to match the desired shape (rules can't be
///      created, so they're dropped from the target with a warning);
///   B. re-fetch, reshape matched tables' row/column counts;
///   C. re-fetch, reconcile all content (paragraphs + cells).
/// Once the barrier sequence matches, the positional content pass yields exactly
/// the (achievable) desired document.
pub fn sync_apply_once(docs: &Docs, id: &str, desired: &DocModel) -> Result<Vec<String>, String> {
    // Safety gate: never edit a document with index-occupying content we don't
    // model — our edits would land at the wrong indices.
    let doc = docs.get(id);
    let unsupported = unsupported_features(&doc);
    if !unsupported.is_empty() {
        return Ok(vec![format!(
            "no changes applied: document contains unsupported content ({}) that could be corrupted by editing",
            unsupported.join(", ")
        )]);
    }

    // Phase A: structural barriers.
    let nodes = current_nodes(&doc);
    let append_pos = match nodes.last() {
        Some(CurNode::Paras(seg)) => seg.start_index,
        _ => 1,
    };
    let (ops, desired, warnings) = reconcile_barriers(&nodes, desired, append_pos);
    let doc = if ops.is_empty() {
        doc
    } else {
        let rev = doc["revisionId"].as_str().unwrap_or_default().to_string();
        docs.batch_raw(id, Value::Array(ops), &rev)?;
        docs.get(id)
    };

    // Phase B: reshape matched tables.
    let resize = table_resize_requests(&current_nodes(&doc), &desired);
    let doc = if resize.is_empty() {
        doc
    } else {
        let rev = doc["revisionId"].as_str().unwrap_or_default().to_string();
        docs.batch_raw(id, Value::Array(resize), &rev)?;
        docs.get(id)
    };

    // Phase C: content (paragraphs + cells).
    let content = sync_doc(&doc, &desired);
    if !content.is_empty() {
        let rev = doc["revisionId"].as_str().unwrap_or_default().to_string();
        docs.batch_update(id, &content, &rev)?;
    }
    Ok(warnings)
}

/// Extract the styled runs of a paragraph JSON object (text + marks + link).
/// Descriptor for a supported inline object element (image, page break, footnote,
/// column break, page-number field) — each occupies one document index, so it
/// maps to a single object sentinel. `None` for anything we don't model.
fn inline_object_descriptor(e: &Value) -> Option<String> {
    if let Some(id) = e["inlineObjectElement"]["inlineObjectId"].as_str() {
        return Some(format!("image/{id}"));
    }
    if let Some(id) = e["footnoteReference"]["footnoteId"].as_str() {
        return Some(format!("footnote/{id}"));
    }
    if e.get("pageBreak").is_some() {
        return Some("pagebreak".into());
    }
    if e.get("columnBreak").is_some() {
        return Some("columnbreak".into());
    }
    if e.get("autoText").is_some() {
        return Some("autotext".into());
    }
    None
}

fn read_runs(p: &Value) -> Vec<Run> {
    let mut runs = Vec::new();
    if let Some(elems) = p["elements"].as_array() {
        for e in elems {
            if let Some(desc) = inline_object_descriptor(e) {
                runs.push(Run::object(&desc));
                continue;
            }
            let Some(content) = e["textRun"]["content"].as_str() else { continue };
            // The paragraph's terminating newline isn't part of any run.
            let content = content.strip_suffix('\n').unwrap_or(content);
            if content.is_empty() {
                continue;
            }
            let ts = &e["textRun"]["textStyle"];
            let link = ts["link"]["url"].as_str().map(String::from);
            runs.push(Run { text: content.to_string(), style: text_style(ts), link, object: None });
        }
    }
    normalize_runs(runs)
}

/// Read a table structural element into our model. Each cell is its first
/// content paragraph's runs (multi-paragraph cells are flattened for now).
fn read_table(el: &Value) -> Table {
    let empty = Vec::new();
    let mut rows = Vec::new();
    for row in el["table"]["tableRows"].as_array().unwrap_or(&empty) {
        let mut cells = Vec::new();
        for cell in row["tableCells"].as_array().unwrap_or(&empty) {
            cells.push(read_runs(&cell["content"][0]["paragraph"]));
        }
        rows.push(cells);
    }
    Table { rows }
}

/// Read a textRun's style. Absent fields default to off (the API only returns
/// fields that are set). Underline/super/subscript are deferred, so unmodeled.
fn text_style(ts: &Value) -> TextStyle {
    let flag = |k: &str| ts[k].as_bool().unwrap_or(false);
    TextStyle { bold: flag("bold"), italic: flag("italic"), strikethrough: flag("strikethrough") }
}

/// Classify a bulleted paragraph as ordered (Number) or unordered (Bullet) by
/// looking up its list's glyph at the paragraph's nesting level.
fn list_kind(doc: &Value, bullet: &Value) -> BlockKind {
    let list_id = bullet["listId"].as_str().unwrap_or("");
    let level = bullet["nestingLevel"].as_u64().unwrap_or(0) as usize;
    let nesting = &doc["lists"][list_id]["listProperties"]["nestingLevels"][level];
    // A real glyphType (DECIMAL/ALPHA/…) is a numbered list; a glyphSymbol (●/○)
    // is a bullet; neither (GLYPH_TYPE_UNSPECIFIED, no symbol) is a checkbox list.
    if matches!(nesting["glyphType"].as_str(), Some(t) if t != "GLYPH_TYPE_UNSPECIFIED") {
        BlockKind::Number
    } else if nesting["glyphSymbol"].as_str().is_some() {
        BlockKind::Bullet
    } else {
        BlockKind::Checkbox
    }
}

/// Reset the fixture to `tests/baseline.md` via the reference Python tool.
/// (The reset logic will be ported to Rust once the engine emits heading styles
/// and bullets natively; reusing the verified script keeps tests honest now.)
pub fn reset_to_baseline(doc_id: &str) {
    let status = Command::new("python3")
        .args(["tools/create_fixture.py", doc_id])
        .status()
        .expect("failed to launch tools/create_fixture.py");
    assert!(status.success(), "reset_to_baseline failed");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Run, TextStyle};
    use serde_json::json;

    fn cell(text: &str, bold: bool) -> Value {
        json!({ "content": [{ "paragraph": { "elements": [
            { "textRun": { "content": format!("{text}\n"), "textStyle": { "bold": bold } } }
        ]}}]})
    }

    // A cell whose content paragraph carries a real start index.
    fn pcell(text: &str, start: u64) -> Value {
        json!({ "content": [{ "startIndex": start, "paragraph": { "elements": [
            { "textRun": { "content": format!("{text}\n"), "textStyle": {} } }
        ]}}]})
    }

    #[test]
    fn document_to_model_reads_paragraphs_and_tables() {
        let doc = json!({ "body": { "content": [
            { "paragraph": { "elements": [{ "textRun": { "content": "intro\n", "textStyle": {} } }] } },
            { "table": { "tableRows": [
                { "tableCells": [cell("Name", false), cell("Role", false)] },
                { "tableCells": [cell("Ada", false), cell("eng", true)] },
            ]}},
        ]}});

        let model = document_to_model(&doc);
        assert_eq!(model.len(), 2);
        assert_eq!(model[0], Node::Para(Block {
            kind: BlockKind::Normal,
            depth: 0,
            runs: vec![Run { text: "intro".into(), style: TextStyle::default(), link: None, object: None }],
        }));
        let Node::Table(t) = &model[1] else { panic!("expected a table node") };
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.columns(), 2);
        assert_eq!(t.rows[0][0], vec![Run { text: "Name".into(), style: TextStyle::default(), link: None, object: None }]);
        assert_eq!(t.rows[1][1], vec![Run {
            text: "eng".into(),
            style: TextStyle { bold: true, ..Default::default() },
            link: None,
            object: None,
        }]);
    }

    #[test]
    fn sync_doc_edits_paragraph_after_a_table_at_its_real_index() {
        use crate::{markdown_to_model, Request};
        // "intro\n" occupies [1,7); a table fills [7,19); "after\n" lives at [19,25).
        let doc = json!({ "body": { "content": [
            { "startIndex": 1, "paragraph": { "elements": [{ "textRun": { "content": "intro\n", "textStyle": {} } }] } },
            { "startIndex": 7, "table": { "tableRows": [{ "tableCells": [cell("x", false)] }] } },
            { "startIndex": 19, "paragraph": { "elements": [{ "textRun": { "content": "after\n", "textStyle": {} } }] } },
        ]}});

        // Edit the trailing paragraph; the table sits between the two paragraphs.
        let desired = markdown_to_model("intro\n\n| h |\n| --- |\n| x |\n\nafter edited");
        let reqs = sync_doc(&doc, &desired);

        let inserts: Vec<_> = reqs
            .iter()
            .filter_map(|r| match r {
                Request::InsertText(it) => Some((it.location.index, it.text.clone())),
                _ => None,
            })
            .collect();
        // "after" -> "after edited": insert " edited" before the newline at real
        // index 24 — NOT index 12, which the old flat offset+1 model would give.
        assert_eq!(inserts, vec![(24, " edited".to_string())]);
    }

    #[test]
    fn detects_unsupported_content() {
        let doc = json!({ "body": { "content": [
            { "sectionBreak": {} }, // leading default — ignored
            { "paragraph": { "elements": [
                { "textRun": { "content": "hi\n", "textStyle": {} } },
                { "pageBreak": {} },                              // modeled
                { "footnoteReference": { "footnoteId": "f1" } },  // modeled
            ]}},
            { "paragraph": { "elements": [{ "inlineObjectElement": { "inlineObjectId": "x" } }] } }, // modeled
            { "paragraph": { "elements": [{ "equation": {} }] } }, // NOT modeled
        ]}});
        let f = unsupported_features(&doc);
        assert!(f.contains(&"equation".to_string()), "{f:?}");
        assert!(!f.contains(&"image".to_string()), "image should be modeled: {f:?}");
        assert!(!f.contains(&"page break".to_string()), "page break should be modeled: {f:?}");
        assert!(!f.contains(&"footnote".to_string()), "footnote should be modeled: {f:?}");

        // Paragraphs, tables, horizontal rules, and the leading section break are fine.
        let clean = json!({ "body": { "content": [
            { "sectionBreak": {} },
            { "paragraph": { "elements": [{ "textRun": { "content": "hi\n", "textStyle": {} } }] } },
            { "paragraph": { "elements": [
                { "horizontalRule": {} },
                { "textRun": { "content": "\n", "textStyle": {} } },
            ]}},
        ]}});
        assert!(unsupported_features(&clean).is_empty(), "{:?}", unsupported_features(&clean));
    }

    #[test]
    fn sync_doc_edits_a_table_cell_at_its_real_index() {
        use crate::{markdown_to_model, Request};
        // A 2x2 table; cell (1,1) "eng" lives at content index 16.
        let doc = json!({ "body": { "content": [
            { "startIndex": 1, "paragraph": { "elements": [{ "textRun": { "content": "top\n", "textStyle": {} } }] } },
            { "startIndex": 5, "table": { "tableRows": [
                { "tableCells": [pcell("Name", 7), pcell("Role", 10)] },
                { "tableCells": [pcell("Ada", 13), pcell("eng", 16)] },
            ]}},
            { "startIndex": 25, "paragraph": { "elements": [{ "textRun": { "content": "end\n", "textStyle": {} } }] } },
        ]}});

        // Same dimensions; only cell (1,1) changes: "eng" -> "engineer".
        let desired = markdown_to_model("top\n\n| Name | Role |\n| --- | --- |\n| Ada | engineer |\n\nend");
        let inserts: Vec<_> = sync_doc(&doc, &desired)
            .iter()
            .filter_map(|r| match r {
                Request::InsertText(it) => Some((it.location.index, it.text.clone())),
                _ => None,
            })
            .collect();
        // Append "ineer" inside cell (1,1): real index 16 + offset 3 = 19. No other edits.
        assert_eq!(inserts, vec![(19, "ineer".to_string())]);
    }
}

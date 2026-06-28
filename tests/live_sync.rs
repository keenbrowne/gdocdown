//! Live round-trips against the real Google Docs fixture.
//!
//! `#[ignore]` so plain `cargo test` stays offline and fast. Run explicitly:
//!
//!   cargo test --test live_sync -- --ignored
//!
//! Requires: a populated fixture (tests/fixture.docid) and working ADC
//! (`gcloud auth application-default print-access-token`).

use gdocdown::api::{document_to_model, reset_to_baseline, sync_apply, sync_doc, Docs};
use gdocdown::{
    markdown_to_model, model_to_markdown, retry_on_conflict, BlockKind, DocModel, Node, Run, TextStyle,
};
use serde_json::{json, Value};
use std::fs;

fn fixture_id() -> String {
    fs::read_to_string("tests/fixture.docid")
        .expect("tests/fixture.docid missing — run `python3 tools/create_fixture.py`")
        .trim()
        .to_string()
}

fn baseline_md() -> String {
    fs::read_to_string("tests/baseline.md").unwrap()
}

/// Sync the live doc to `desired` (revision-locked) and return its new model.
fn sync_to(docs: &Docs, id: &str, desired: &DocModel) -> DocModel {
    sync_apply(docs, id, desired).expect("sync_apply failed"); // multi-step real-index path
    document_to_model(&docs.get(id))
}

/// Stand up a scratch doc with a populated 2x2 table (cells A/B/C/D). Returns id.
fn make_2x2_table_doc(docs: &Docs, title: &str) -> String {
    let id = docs.create(title);
    let rev = docs.get(&id)["revisionId"].as_str().unwrap().to_string();
    docs.batch_raw(&id, json!([{ "insertTable": { "rows": 2, "columns": 2, "location": { "index": 1 } } }]), &rev)
        .expect("insertTable");
    let doc = docs.get(&id);
    let mut starts: Vec<u64> = Vec::new();
    for el in doc["body"]["content"].as_array().unwrap() {
        if let Some(t) = el.get("table") {
            for row in t["tableRows"].as_array().unwrap() {
                for c in row["tableCells"].as_array().unwrap() {
                    starts.push(c["content"][0]["startIndex"].as_u64().unwrap());
                }
            }
        }
    }
    let fills: Vec<Value> = starts
        .iter()
        .zip(["A", "B", "C", "D"])
        .rev()
        .map(|(&idx, t)| json!({ "insertText": { "location": { "index": idx }, "text": t } }))
        .collect();
    docs.batch_raw(&id, json!(fills), doc["revisionId"].as_str().unwrap()).expect("populate");
    id
}

fn r(text: &str) -> Vec<Run> {
    vec![Run { text: text.into(), style: TextStyle::default(), link: None, object: None }]
}

fn find_table(model: &DocModel) -> &gdocdown::Table {
    model.iter().find_map(|n| match n {
        Node::Table(t) => Some(t),
        _ => None,
    }).expect("table node")
}

fn find_table_mut(model: &mut DocModel) -> &mut gdocdown::Table {
    model.iter_mut().find_map(|n| match n {
        Node::Table(t) => Some(t),
        _ => None,
    }).expect("table node")
}

/// After a reset, the doc parsed back from Docs must equal the markdown model —
/// proving headings and ordered/unordered lists round-trip in both directions.
#[test]
#[ignore = "hits the live Google Docs API; run with --ignored"]
fn live_baseline_roundtrips() {
    let id = fixture_id();
    reset_to_baseline(&id);
    let docs = Docs::new();
    assert_eq!(document_to_model(&docs.get(&id)), markdown_to_model(&baseline_md()));
}

/// Changing a heading's level rewrites only its paragraph style.
#[test]
#[ignore = "hits the live Google Docs API; run with --ignored"]
fn live_change_heading_level() {
    let id = fixture_id();
    reset_to_baseline(&id);
    let docs = Docs::new();
    let desired = markdown_to_model(&baseline_md().replace("## Paragraphs", "### Paragraphs"));
    assert_eq!(sync_to(&docs, &id, &desired), desired);
}

/// Edit a bullet's text and insert a new bullet mid-list; numbering/markers and
/// the surrounding structure must stay intact.
#[test]
#[ignore = "hits the live Google Docs API; run with --ignored"]
fn live_edit_and_add_bullet() {
    let id = fixture_id();
    reset_to_baseline(&id);
    let docs = Docs::new();
    let desired = markdown_to_model(
        &baseline_md().replace("- Second bullet", "- Second bullet (edited)\n- Inserted bullet"),
    );
    assert_eq!(sync_to(&docs, &id, &desired), desired);
}

/// Inline marks (bold/italic/strike — the ones Google's export emits) round-trip.
#[test]
#[ignore = "hits the live Google Docs API; run with --ignored"]
fn live_inline_marks_roundtrip() {
    let id = fixture_id();
    reset_to_baseline(&id);
    let docs = Docs::new();

    let md = baseline_md().replace(
        "The quick brown fox jumps over the lazy dog.",
        "Plain **bold** then *italic* and ~~struck~~ words here.",
    );
    let desired = markdown_to_model(&md);
    assert_eq!(sync_to(&docs, &id, &desired), desired);
}

/// The pull direction round-trips through a real doc: read it, serialize to
/// markdown, re-parse, and syncing back is a no-op (the generated markdown maps
/// exactly to the same document).
#[test]
#[ignore = "hits the live Google Docs API; run with --ignored"]
fn live_serialize_round_trips_through_doc() {
    let id = fixture_id();
    reset_to_baseline(&id);
    let docs = Docs::new();
    let doc = docs.get(&id);
    let md = model_to_markdown(&document_to_model(&doc));
    let reqs = sync_doc(&doc, &markdown_to_model(&md));
    assert!(reqs.is_empty(), "doc->markdown->parse->sync must be a no-op; got {reqs:?}\nmarkdown:\n{md}");
}

/// A markdown link round-trips: `[text](url)` becomes a real Docs hyperlink and
/// reads back with the same url.
#[test]
#[ignore = "hits the live Google Docs API; run with --ignored"]
fn live_link_roundtrip() {
    let id = fixture_id();
    reset_to_baseline(&id);
    let docs = Docs::new();

    let md = baseline_md().replace(
        "The quick brown fox jumps over the lazy dog.",
        "See the [project page](https://example.com/proj) and **[bold link](https://example.com/b)** here.",
    );
    let desired = markdown_to_model(&md);
    assert_eq!(sync_to(&docs, &id, &desired), desired);
}

/// Editing a table cell round-trips: build a real 2x2 table, edit one cell via
/// the markdown model, and confirm the doc reflects only that change.
#[test]
#[ignore = "hits the live Google Docs API; run with --ignored"]
fn live_table_cell_edit_roundtrips() {
    let docs = Docs::new();
    let id = make_2x2_table_doc(&docs, "gdocdown table cell test");

    // Edit cell (1,1): D -> Dee, via the model + real-index sync.
    let mut desired = document_to_model(&docs.get(&id));
    find_table_mut(&mut desired).rows[1][1] = r("Dee");
    let after = sync_to(&docs, &id, &desired);

    let t = find_table(&after);
    assert_eq!(t.rows[1][1], r("Dee"));
    assert_eq!(t.rows[0][0], r("A")); // other cells untouched
    assert_eq!(t.rows[1][0], r("C"));
}

/// Adding a row to a table: the multi-step apply inserts the row (batch 1), then
/// populates the new cells (batch 2).
#[test]
#[ignore = "hits the live Google Docs API; run with --ignored"]
fn live_table_add_row_roundtrips() {
    let docs = Docs::new();
    let id = make_2x2_table_doc(&docs, "gdocdown table row test");

    let mut desired = document_to_model(&docs.get(&id));
    find_table_mut(&mut desired).rows.push(vec![r("E"), r("F")]); // 2x2 -> 3x2
    let after = sync_to(&docs, &id, &desired);

    let t = find_table(&after);
    assert_eq!(t.rows.len(), 3);
    assert_eq!(t.rows[2][0], r("E"));
    assert_eq!(t.rows[2][1], r("F"));
    assert_eq!(t.rows[0][0], r("A")); // original rows intact
    assert_eq!(t.rows[1][1], r("D"));
}

/// Read-only check against a doc that already has horizontal rules (set
/// GDOCS_HR_DOC to its id). Confirms rules are read as `Node::Rule` and that
/// syncing the doc to its own model is a no-op — i.e. rules are preserved and
/// the rule handling introduces no spurious edits. Never writes.
#[test]
#[ignore = "needs GDOCS_HR_DOC pointing at a doc with horizontal rules"]
fn live_horizontal_rule_read_and_preserve() {
    let Ok(id) = std::env::var("GDOCS_HR_DOC") else { return };
    let docs = Docs::new();
    let doc = docs.get(&id);
    let model = document_to_model(&doc);
    let rules = model.iter().filter(|n| matches!(n, Node::Rule)).count();
    assert!(rules >= 1, "expected horizontal rules; got model {model:?}");
    // Syncing to its own model must produce no edits (preserve).
    let reqs = sync_doc(&doc, &model);
    assert!(reqs.is_empty(), "preserve should be a no-op, got {reqs:?}");
}

/// An inline image is preserved while surrounding text is edited, and removing
/// its placeholder run deletes it — all via the 1-char object sentinel.
#[test]
#[ignore = "hits the live Google Docs API; run with --ignored"]
fn live_image_preserve_then_delete() {
    let docs = Docs::new();
    let id = docs.create("gdocdown image test");
    docs.batch_raw(&id, json!([{ "insertText": { "location": { "index": 1 }, "text": "before after\n" } }]),
        docs.get(&id)["revisionId"].as_str().unwrap()).expect("seed");
    docs.batch_raw(&id, json!([{ "insertInlineImage": {
        "location": { "index": 8 },
        "uri": "https://www.google.com/images/branding/googlelogo/2x/googlelogo_color_272x92dp.png",
    }}]), docs.get(&id)["revisionId"].as_str().unwrap()).expect("insert image");

    let has_image = |m: &DocModel| m.iter().any(|n| n.as_para().is_some_and(|b| b.runs.iter().any(|r| r.object.is_some())));
    let before = document_to_model(&docs.get(&id));
    assert!(has_image(&before), "image should be read as an object run");

    // Edit a text run in the image's paragraph; keep the image run intact.
    let mut desired = before.clone();
    for n in &mut desired {
        if let Node::Para(b) = n {
            if b.runs.iter().any(|r| r.object.is_some()) {
                if let Some(t) = b.runs.iter_mut().find(|r| r.object.is_none() && !r.text.trim().is_empty()) {
                    t.text = "EDITED ".into();
                }
            }
        }
    }
    let after = sync_to(&docs, &id, &desired);
    assert!(has_image(&after), "image must survive a surrounding-text edit");
    assert!(after.iter().any(|n| n.as_para().is_some_and(|b| b.text().contains("EDITED"))), "text edit must land");

    // Now remove the image run -> the image is deleted.
    let mut no_image = after.clone();
    for n in &mut no_image {
        if let Node::Para(b) = n {
            b.runs.retain(|r| r.object.is_none());
        }
    }
    let after2 = sync_to(&docs, &id, &no_image);
    assert!(!has_image(&after2), "removing the placeholder must delete the image");
}

/// A checkbox / task list round-trips: read as `Checkbox`, and editing + adding
/// items keeps them checkboxes (not converted to disc bullets).
#[test]
#[ignore = "hits the live Google Docs API; run with --ignored"]
fn live_checkbox_list_roundtrips() {
    let docs = Docs::new();
    let id = docs.create("gdocdown checkbox test");
    docs.batch_raw(&id, json!([{ "insertText": { "location": { "index": 1 }, "text": "Buy milk\nWalk dog\n" } }]),
        docs.get(&id)["revisionId"].as_str().unwrap()).expect("seed");
    docs.batch_raw(&id, json!([{ "createParagraphBullets": { "range": { "startIndex": 1, "endIndex": 18 }, "bulletPreset": "BULLET_CHECKBOX" } }]),
        docs.get(&id)["revisionId"].as_str().unwrap()).expect("checkbox bullets");

    let checks = |m: &DocModel| m.iter().filter_map(|n| n.as_para()).filter(|b| b.kind == BlockKind::Checkbox).count();
    let before = document_to_model(&docs.get(&id));
    assert_eq!(checks(&before), 2, "both items should read as checkboxes; got {before:?}");

    // Edit one item and add a third — all must stay checkboxes.
    let desired = markdown_to_model("- [ ] Buy oat milk\n- [ ] Walk dog\n- [ ] Do laundry");
    let after = sync_to(&docs, &id, &desired);
    assert_eq!(checks(&after), 3, "all three must be checkboxes (not converted); got {after:?}");
    assert!(after.iter().any(|n| n.as_para().is_some_and(|b| b.text().contains("oat milk"))));
}

/// Astral-plane characters (emoji, math letters) are 2 UTF-16 units each — the
/// unit Docs indices count. Editing text *after* such a char must land at the
/// right index. Pre-fix (char-count math) this corrupted the doc.
#[test]
#[ignore = "hits the live Google Docs API; run with --ignored"]
fn live_astral_chars_keep_indices_aligned() {
    let docs = Docs::new();
    let id = docs.create("gdocdown utf16 test");
    // Two astral chars on line 1, plus following lines whose edits must align.
    let seed = "A\u{1F600}B\u{1D400}C\nedit me\nkeep me\n";
    docs.batch_raw(&id, json!([{ "insertText": { "location": { "index": 1 }, "text": seed } }]),
        docs.get(&id)["revisionId"].as_str().unwrap()).expect("seed");

    // Change a word on the line after the emoji line, and append a line. If index
    // math were char-based, these edits would land 2 units too early and mangle text.
    let desired = markdown_to_model("A\u{1F600}B\u{1D400}C\nedited!\nkeep me\nnew tail");
    let after = sync_to(&docs, &id, &desired);
    let text: Vec<String> = after.iter().filter_map(|n| n.as_para()).map(|b| b.text()).collect();
    assert_eq!(text, vec!["A\u{1F600}B\u{1D400}C", "edited!", "keep me", "new tail"], "got {text:?}");
}

/// Page breaks and footnotes (inline 1-index elements) are read as object runs,
/// preserved across a surrounding-text edit, and deletable like images.
#[test]
#[ignore = "hits the live Google Docs API; run with --ignored"]
fn live_pagebreak_and_footnote_preserved() {
    let docs = Docs::new();
    let id = docs.create("gdocdown inline-elements test");
    let rev = || docs.get(&id)["revisionId"].as_str().unwrap().to_string();
    docs.batch_raw(&id, json!([{ "insertText": { "location": { "index": 1 }, "text": "alpha beta gamma\n" } }]), &rev()).expect("seed");
    docs.batch_raw(&id, json!([{ "insertPageBreak": { "location": { "index": 3 } } }]), &rev()).expect("page break");
    docs.batch_raw(&id, json!([{ "createFootnote": { "location": { "index": 2 } } }]), &rev()).expect("footnote");

    let objs = |m: &DocModel| m.iter().filter_map(|n| n.as_para()).flat_map(|b| b.runs.clone()).filter(|r| r.object.is_some()).count();
    let before = document_to_model(&docs.get(&id));
    assert_eq!(objs(&before), 2, "expected page break + footnote object runs; got {before:?}");

    // Edit a text run; both inline elements must survive.
    let mut desired = before.clone();
    for n in &mut desired {
        if let Node::Para(b) = n {
            if let Some(t) = b.runs.iter_mut().find(|r| r.object.is_none() && r.text.contains("gamma")) {
                t.text = t.text.replace("gamma", "GAMMA");
            }
        }
    }
    let after = sync_to(&docs, &id, &desired);
    assert_eq!(objs(&after), 2, "page break + footnote must survive a text edit");
    assert!(after.iter().any(|n| n.as_para().is_some_and(|b| b.text().contains("GAMMA"))));

    // Remove one object run -> that element is deleted.
    let mut fewer = after.clone();
    for n in &mut fewer {
        if let Node::Para(b) = n {
            if let Some(pos) = b.runs.iter().position(|r| r.object.is_some()) {
                b.runs.remove(pos);
                break;
            }
        }
    }
    assert_eq!(objs(&sync_to(&docs, &id, &fewer)), 1, "removing a placeholder must delete that element");
}

/// A document with unsupported, index-occupying content (here a page break) is
/// refused, not corrupted: sync_apply makes no changes and returns a warning.
#[test]
#[ignore = "hits the live Google Docs API; run with --ignored"]
fn live_unsupported_content_is_refused() {
    let docs = Docs::new();
    let id = docs.create("gdocdown unsupported test");
    docs.batch_raw(&id, json!([{ "insertText": { "location": { "index": 1 }, "text": "hello world\n" } }]),
        docs.get(&id)["revisionId"].as_str().unwrap()).expect("seed");
    // A mid-document section break is structural and still unmodeled.
    docs.batch_raw(&id, json!([{ "insertSectionBreak": { "location": { "index": 4 }, "sectionType": "NEXT_PAGE" } }]),
        docs.get(&id)["revisionId"].as_str().unwrap()).expect("section break");

    let rev_before = docs.get(&id)["revisionId"].as_str().unwrap().to_string();
    let warnings = sync_apply(&docs, &id, &markdown_to_model("totally different text")).expect("sync_apply");
    assert!(warnings.iter().any(|w| w.contains("unsupported")), "expected a refusal warning: {warnings:?}");
    // Refused => no edits => revision unchanged.
    assert_eq!(docs.get(&id)["revisionId"].as_str().unwrap(), rev_before, "doc must be untouched");
}

/// The concurrency loop recovers from a real revision conflict: on the first
/// attempt we deliberately bump the revision after reading (so our write is
/// rejected), and the loop re-reads and succeeds on the second attempt.
#[test]
#[ignore = "hits the live Google Docs API; run with --ignored"]
fn live_concurrency_loop_recovers_from_conflict() {
    let docs = Docs::new();
    let id = docs.create("gdocdown conflict test");
    docs.batch_raw(&id, json!([{ "insertText": { "location": { "index": 1 }, "text": "base\n" } }]),
        docs.get(&id)["revisionId"].as_str().unwrap()).expect("seed");

    let attempt = std::cell::Cell::new(0);
    let result = retry_on_conflict(|| {
        let doc = docs.get(&id);
        let rev = doc["revisionId"].as_str().unwrap().to_string();
        if attempt.get() == 0 {
            // Concurrent edit: bump the revision so `rev` is now stale.
            docs.batch_raw(&id, json!([{ "insertText": { "location": { "index": 1 }, "text": "X" } }]), &rev)
                .expect("injected edit");
        }
        attempt.set(attempt.get() + 1);
        // Our "real" write, locked to `rev` — rejected on attempt 0, accepted on 1.
        docs.batch_raw(&id, json!([{ "insertText": { "location": { "index": 1 }, "text": "Y" } }]), &rev)
    });

    assert!(result.is_ok(), "loop should recover from the conflict: {result:?}");
    assert_eq!(attempt.get(), 2, "should take exactly two attempts");
}

/// Adding then removing a whole table: the count-reconciliation pass inserts /
/// deletes the table, and the content pass fills / heals the paragraphs.
#[test]
#[ignore = "hits the live Google Docs API; run with --ignored"]
fn live_table_add_and_remove_roundtrips() {
    let docs = Docs::new();
    let id = docs.create("gdocdown table add/remove");
    // Seed two paragraphs, no table.
    let rev = docs.get(&id)["revisionId"].as_str().unwrap().to_string();
    docs.batch_raw(&id, json!([{ "insertText": { "location": { "index": 1 }, "text": "before\nafter\n" } }]), &rev)
        .expect("seed");

    // Add a table between them.
    let with_table = markdown_to_model("before\n\n| h |\n| --- |\n| v |\n\nafter");
    let after = sync_to(&docs, &id, &with_table);
    let t = find_table(&after);
    assert_eq!(t.rows.len(), 2);
    assert_eq!(t.rows[0][0], r("h"));
    assert_eq!(t.rows[1][0], r("v"));
    assert!(after.iter().any(|n| n.as_para().is_some_and(|b| b.text() == "before")));
    assert!(after.iter().any(|n| n.as_para().is_some_and(|b| b.text() == "after")));

    // Remove it again.
    let no_table = markdown_to_model("before\nafter");
    let after2 = sync_to(&docs, &id, &no_table);
    assert!(after2.iter().all(|n| matches!(n, Node::Para(_))), "table should be gone");
    assert!(after2.iter().any(|n| n.as_para().is_some_and(|b| b.text() == "before")));
    assert!(after2.iter().any(|n| n.as_para().is_some_and(|b| b.text() == "after")));
}

/// Nested lists round-trip: indent some bullets and numbers, sync, and confirm
/// the doc reports the right nesting levels back.
#[test]
#[ignore = "hits the live Google Docs API; run with --ignored"]
fn live_nested_lists_roundtrip() {
    let id = fixture_id();
    reset_to_baseline(&id);
    let docs = Docs::new();

    let md = baseline_md()
        .replace("- Second bullet", "  - Second bullet")
        .replace("- Third bullet", "    - Third bullet")
        .replace("2. Second step", "   2. Second step")
        .replace("3. Third step", "    3. Third step");
    let desired = markdown_to_model(&md);
    // Sanity on the parse: nested depths 0/1/2 for bullets, 0/1/2 for numbers.
    let depths: Vec<u8> = desired
        .iter()
        .filter_map(|n| n.as_para())
        .filter(|b| matches!(b.kind, BlockKind::Bullet | BlockKind::Number))
        .map(|b| b.depth)
        .collect();
    assert_eq!(depths, vec![0, 1, 2, 0, 1, 2]);

    let after = sync_to(&docs, &id, &desired);
    assert_eq!(after, desired);
}

/// The combined #1+#2 case: append a paragraph after the numbered list. It must
/// land (final-newline handling) AND be Normal — not inherit the numbered list
/// above it (inheritance handling).
#[test]
#[ignore = "hits the live Google Docs API; run with --ignored"]
fn live_append_paragraph_is_normal_not_listed() {
    let id = fixture_id();
    reset_to_baseline(&id);
    let docs = Docs::new();
    let desired = markdown_to_model(&format!("{}\nA closing remark.", baseline_md().trim_end()));

    let after = sync_to(&docs, &id, &desired);
    assert_eq!(after, desired);
    assert_eq!(
        after.last().unwrap().as_para().unwrap().kind,
        BlockKind::Normal,
        "appended paragraph inherited the numbered list above it"
    );
}

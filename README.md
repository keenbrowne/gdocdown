# gdocdown

Edit Google Docs as markdown from any editor (vim, VS Code, …), pushing changes
into the live doc incrementally so collaborators see them appear in near-real-time.

## Why this doesn't already exist

Lots of tools convert Docs ↔ markdown, and a few sync a local file to Drive. But
they all either (a) only go one direction, or (b) re-upload the **whole document**
on every change — which clobbers concurrent edits and kills the collaborative feel.
The unsolved piece is **incremental write-back**: turning a markdown edit into the
*minimal* set of Google Docs `batchUpdate` requests, preserving the doc's identity
(comments, sharing, revision history). That engine is what this project is.

## The hard constraint

Google does **not** expose its real-time (operational-transform) backend to third
parties. So "real-time" here means fast push + poll/webhook pull, not keystroke OT:

- **Push (md → doc):** `documents.batchUpdate`, debounced (~1s) — feels live.
- **Pull (doc → md):** Drive `files.watch` webhook says *"file changed"* (not what);
  re-fetch via `documents.get` and diff. This asymmetry drives the design.

## Architecture

```
            ┌──────────────── engine (the novel core, offline-testable) ──────────────┐
trigger ───►│ md → block model → diff vs current doc text → batchUpdate requests       │──► Doc
(inotify    │ Doc (documents.get) → block model → md                                   │◄── Drive
 then FUSE) └──────────────────────────────────────────────────────────────────────────┘   watch
```

The trigger layer (how edits are intercepted) is swappable; the engine is not.
- **Phase A — mirror folder + inotify daemon:** real cached `.md` files, a watcher
  pushes diffs. Most robust, validates the whole loop.
- **Phase B — FUSE mount:** `~/gdocs/*.md` *are* the docs, conjured on demand. Nicer
  UX, harder (editor atomic-save quirks, network latency); calls the same engine.

## Markdown flavor (design rule)

**Our markdown flavor matches what Google Docs itself exports to markdown.**
Before choosing syntax for any construct (emphasis, headings, lists, quotes,
code, tables, …), probe Google's export of that construct (Drive
`files.export` with `text/markdown`) and match it exactly. Only where Google's
export is *lossy or undefined* (e.g. underline, super/subscript, which it drops)
do we add a documented superset — and even then, a Google-exported `.md` must
still parse unchanged. This keeps the round-trip symmetric and avoids inventing a
dialect that drifts from the one users already get from Docs.

Audited against Google's export so far: headings (`#`…`######`), emphasis
(`**`/`*`/`~~`), links (`[text](url)`), ordered marker (`1.`), and relative list
nesting all match (bare URLs are left as plain text, as Google does);
blank lines between blocks and trailing hard-break spaces are tolerated on parse.
One emit-direction note: Google uses `* ` for bullets where our fixtures use
`- ` — we parse both, so when the doc→markdown (pull) direction is built it
should *emit* `* ` to stay symmetric.

## Status

- [x] **Engine** — block model (headings, ordered/unordered lists, paragraphs);
      minimal **text pass** + block-diff-driven **style pass**. Emits
      `insertText`/`deleteContentRange`/`updateParagraphStyle`/`createParagraphBullets`/
      `deleteParagraphBullets`, highest-index-first. See `src/lib.rs`.
- [x] **Headings + lists round-trip** both directions, verified live.
- [x] **Checkbox / task lists** (`- [ ]`) ↔ Google's `BULLET_CHECKBOX` lists
      (detected by `glyphType=GLYPH_TYPE_UNSPECIFIED` + no glyph symbol; Google
      exports them as `- [ ]`). Read / create / preserve; editing or adding items
      keeps them checkboxes. Checked state (`- [x]`) isn't modeled — the API can't
      *set* it — so all serialize unchecked. Offline + live tested.
- [x] **Comments, headers/footers, page numbers** — comments live in Drive's
      separate layer (absent from `documents.get`, occupy no body index) and
      header/footer content (incl. page-number `autoText`) is outside `body`. We
      only read/edit `body`, so all of these are left untouched / preserved.
- [x] **Nested lists** — nesting is parsed *relatively* (a stack of indent
      widths), matching Google's export, which indents by parent marker width
      (2 cols under `* `, 3 under `1. `). The style pass rebuilds a contiguous
      list run by inserting `depth` leading tabs per item, then one
      `createParagraphBullets` consumes them and assigns levels. Verified live.
- [x] **Immovable final newline** — appends insert before it; trailing deletes
      eat the preceding newline. Inserted paragraphs are explicitly normalized so
      they don't inherit a neighbor's list/heading formatting.
- [x] **API client** (`src/api.rs`) — ADC token, `documents.get`, revision-locked
      `documents.batchUpdate` (optimistic concurrency). Offline simulator
      (`src/apply.rs`) + 50k-case fuzz prove the diff round-trip.
- [x] **Inline marks + links** — `**bold**`, `*italic*`, `~~strike~~`, and
      `[text](url)` (exactly what Google Docs' markdown export emits; link text
      keeps its own marks). Parsed into styled runs; applied via `updateTextStyle`
      (clear-then-set per changed paragraph). Verified live.
- [ ] Underline / superscript / subscript — **deferred** (Google's export drops
      them to plain text; no matching flavor to adopt yet).
- [~] **Tables** (GFM pipe tables, full-incremental, in progress):
      - [x] Phase 1 — table model + markdown parser (`parse_table`), inline marks
            and links inside cells. Offline-tested.
      - [x] Phase 2 — `Node = Para | Table`; `markdown_to_model` &
            `document_to_model` read tables. Sync still assumes table-free input.
      - [x] Phase 3 — **real-index sync** (`sync_doc`): the doc is split into
            paragraph segments separated by tables, each anchored at its real
            `documents.get` start index and processed highest-first. Tables are
            preserved untouched. Fixes the latent `offset+1` bug for any
            table-containing doc; table-free docs are byte-identical to before.
      - [x] Phase 4a — **edit inside tables**: a cell is an "edit unit" (a
            single-paragraph region owning its newline) at its real content
            index; cells reuse the text+inline passes. Matched-dimension tables
            only. Offline + live tested.
      - [x] Phase 4b — **row/column add & remove** via a multi-step apply
            (`sync_apply`): batch 1 reshapes matched tables
            (`insert/deleteTableRow/Column`, highest-table-first), re-fetch, then
            batch 2 is the normal content sync. Offline + live tested.
      - [x] Table add/remove — a count-reconciliation pass (`sync_apply` Phase A)
            inserts/deletes whole tables one at a time until the count matches;
            since an N-table doc has a fixed node shape, the positional content
            pass then yields the desired document. Correct always; inserted
            tables land near the end (non-minimal if they belong mid-document).
            Offline + live tested.
- [x] **Table of contents** — a `tableOfContents` is a structural **barrier**
      (like a rule): read / preserve / delete, never created (no insert API). It
      serializes to a `<!-- gdoc:toc -->` placeholder so you can edit *around* it;
      the TOC's entries are auto-generated by Google from the headings (you don't
      edit them — you regenerate when headings change), so the placeholder just
      marks its position. Offline + live (round-trip no-op, edit-around) tested.
- [x] **Every segment owns an immovable final newline** — not just the document's
      last paragraph. A paragraph region bounded by *any* barrier (table / rule /
      TOC) or the doc end can't be deleted down to nothing (Google forbids
      adjacent/leading structural elements), so appends insert before that newline
      and clears keep it. Empty-range deletes are dropped. This fixes an invalid
      `deleteContentRange` when round-tripping a doc with a structural empty
      paragraph (e.g. the one Google requires before a TOC). Empty headings (`# `)
      and literal `#`/placeholder paragraphs are escape-guarded to round-trip.
- [x] **Horizontal rules** (`---`/`***`/`___`) ↔ Google's `horizontalRule`
      (which exports as `---`). **Read / preserve / delete** only — the API can't
      *insert* a horizontal rule, so a newly-typed `---` is dropped with a warning.
      Rules are "barriers" like tables; `reconcile_barriers` aligns the barrier
      sequence (insert/delete tables, delete rules). Also closes a latent index
      bug (a `horizontalRule` is a 2-index element we used to skip). Offline +
      live (read/preserve) tested.
- [x] **Underscore emphasis aliases** (`_x_`/`__x__`/`___x___`) — accepted on
      input alongside `*`/`**`/`***`, with an intraword guard (a `_` touching a
      word char — alphanumeric or `_` — stays literal) so `snake_case`,
      `a_b_c`, `my__dunder__name`, and `foo_bar.md` are untouched. The serializer
      always emits the `*` forms, matching Google's export. Offline-tested.
- [ ] Full CommonMark emphasis (flanking) rules — the `*` parser is still a
      pragmatic toggling one (diverges on edge cases like `a * b * c`).
- [~] **Mixed-kind nesting** (numbered parent + bulleted child in one indented
      list) — **not implemented; appears inexpressible via the public API.**
      Empirically: `createParagraphBullets` only takes uniform glyph presets,
      re-skins the whole list any member belongs to, and normalizes a standalone
      sub-list back to nesting level 0 — so the single mixed-glyph list the Docs
      UI builds can't be reproduced. Same-kind nesting is fully supported.
- [x] **UTF-16 offset pass** — Docs indices count **UTF-16 code units**, not
      Unicode scalars. All doc-index arithmetic goes through `u16_len` /
      `utf16_prefix` (a char-space text diff maps its positions to UTF-16 offsets),
      so an edit *after* an astral char (emoji 😀, math letters 𝐀, rare CJK
      Ext-B — 2 units each) lands correctly instead of 1-per-astral-char early.
      BMP text (ASCII, accents, **common Chinese/Japanese/Korean**) is 1 unit =
      1 char, so it was already fine and is unchanged. The local `.md` file is
      UTF-8 (Rust strings); UTF-16 is used only to compute Docs indices, never for
      file I/O. Offline + live tested (edit text after 😀 and 𝐀).
- [x] **Pull direction — `model_to_markdown`** (the serializer): renders a
      document model back to markdown matching Google's export flavor (headings,
      `* `/`1. ` lists with Google's nesting widths, `**`/`*`/`~~`, links, GFM
      tables, `---`), with inline objects as `![](gdoc:image/<id>)` placeholders
      and backslash-escaping for round-trip safety. Invariant
      `markdown_to_model(model_to_markdown(m)) == m`. This is what *generates* the
      markdown you edit (and makes image placeholders visible). Offline round-trip
      + live `doc → markdown → doc` no-op tested. **The engine is now a complete
      round-trip: markdown ⇄ Google Doc.**
- [x] **Inline objects (placeholder editing)** — images, **page breaks,
      footnotes, column breaks, page-number fields** are each modeled as an object
      run: a 1-char sentinel (`U+FFFC`) in the index-text (so indexing stays
      correct) that serializes to `![](gdoc:<descriptor>)` (e.g. `image/<id>`,
      `pagebreak`, `footnote/<id>`). **Read / preserve / edit-around / delete**
      work (a removed placeholder deletes the element); **create is blocked**
      (sentinel inserts are dropped). Offline + live tested. Still refused by the
      safety gate (need probing / multi-index handling): section breaks, TOC,
      equations, smart chips.
- [x] **Safety gate** — `sync_apply` refuses (makes no changes, returns a
      warning) when the document contains index-occupying content we don't model
      (images, footnotes, page/column/section breaks, equations, smart chips,
      TOC). Prevents the latent corruption where unmodeled indices shift our
      edits. Tables, horizontal rules, and the leading section break are fine.
      Next step for real docs: per-element placeholders so content can be edited
      *around* (probe each element's Google export first).
- [x] **Concurrency loop** — writes are revision-locked, so a concurrent edit
      causes a clean rejection, never a lost update. `retry_on_conflict` wraps
      `sync_apply`: on Google's stale-`requiredRevisionId` 400 it re-reads and
      re-diffs against the new revision (up to `MAX_CONFLICT_RETRIES`). Offline +
      live tested (live test injects a real revision bump and recovers).
- [x] **Bidirectional file-watch daemon (Phase A)** — `gdocdown pull/push/watch`.
      `watch` seeds the file, then on each file-save or 3s poll runs a single
      `reconcile(base, local, remote)`: only-local → push, only-remote → pull,
      **both → 3-way merge** (`diffy`) — a clean merge converges file + doc; a
      conflicting merge writes git-style `<<<<<<<` markers to the **file only**
      (the doc is never polluted) for the user to resolve. Loop-free via a synced
      baseline; token re-mints every 30 min. Verified live: push, pull, no
      feedback loop, clean merge of non-overlapping edits, and conflict markers
      on overlapping edits.
- [ ] FUSE mount (Phase B) — needs `libfuse3-dev` + the `fuser` crate.

## Run

```
cargo test                          # engine unit + fuzz tests, fully offline
cargo test --test live_sync -- --ignored --test-threads=1   # live round-trips

# CLI (auth via gcloud Application Default Credentials):
gdocdown pull  <docId> <file.md>    # doc  -> markdown file
gdocdown push  <docId> <file.md>    # markdown file -> doc
gdocdown watch <docId> <file.md>    # seed the file, then push on every save
```

## You'll need (when we wire the real API)

A Google Cloud project with the **Docs API** and **Drive API** enabled, plus an
OAuth2 client (Desktop app) credentials JSON. This is the one step that can't be
automated — everything up to it is testable without it.

---
name: gdocdown
description: Edit a Google Doc as a local markdown file via the `gdocdown` CLI. Use when the user asks you to edit, rewrite, or update a Google Doc (given a doc URL/ID and a local .md file), especially when other people may be editing the same doc at the same time. Teaches the merge-safe pull→edit→sync loop and gdocdown's markdown flavor so your edits don't churn.
---

# gdocdown: edit a Google Doc as markdown

`gdocdown` mirrors a Google Doc as a local markdown file. You edit the markdown;
it pushes the *minimal* changes back into the live doc, so collaborators see only
what you changed and their concurrent edits are preserved.

You only ever touch the **local .md file**. Never call the Google API directly.

## The loop (do this)

Given a doc id (the long string in the doc URL, `/document/d/<ID>/edit`) and a
local file path:

1. **`gdocdown sync <docId> <file.md>`** — start here. On a fresh file it pulls the
   doc down; otherwise it merges. Now `<file.md>` holds the current doc.
2. **Edit `<file.md>`** to satisfy the user's request. Prefer **small, targeted
   edits** over full-file rewrites — smaller diffs mean fewer conflicts with
   collaborators and less reformatting churn.
3. **`gdocdown sync <docId> <file.md>`** again — this 3-way merges your edits with
   anything collaborators changed meanwhile, then pushes. Re-read the file
   afterward: a merge or a remote pull may have changed it.

That's it. `sync` is two-way and **safe against concurrent edits**.

### Commands

- `gdocdown sync <doc> <file>` — **merge** (two-way, concurrent-edit safe). Your
  default for everything. Use this; you should rarely need the others.
- `gdocdown pull <doc> <file>` — **take theirs**: overwrite the file with the doc.
  Refuses if the file has unsynced edits (would discard them).
- `gdocdown push <doc> <file>` — **take mine**: force the doc to equal the file.
  Refuses if the doc moved since the baseline (would clobber collaborators).
- `gdocdown watch <doc> <file>` — continuous background sync. If the user has this
  running, just edit the file and let it sync; still prefer small edits.

`pull`/`push` take `--force` to override their guard, but **don't reach for it**:
if one refuses, that means work would be lost — run `gdocdown sync` to merge
instead. Only use `--force` if the user explicitly tells you to overwrite a side.

## Conflicts

If `sync` prints `⚠ merge conflict`, the file now contains git-style markers:

```
<<<<<<< local
your edit
=======
the collaborator's edit
>>>>>>> remote
```

Resolve them in the file (combine both intents unless the user says otherwise),
remove the markers, then `sync` again. **The Google Doc never receives these
markers** — resolution happens entirely in the file.

## Markdown flavor — match it to avoid churn

gdocdown's flavor matches what Google Docs itself exports. Stay inside it or your
text will be silently rewritten on the next sync.

**Emphasis** — write `*italic*`, `**bold**`, `***bolditalic***`, `~~strike~~`.
`_underscores_` are accepted but normalized to `*` on the next pull, so don't be
surprised if they change. Underline / superscript / subscript are **dropped**
(Google's export drops them) — don't use them.

**Lists** — bullets are `* ` (a leading `- ` is accepted but becomes `* `).
Numbered are `1. `. Checkbox/task items are `- [ ] `. Note: a checkbox's *checked*
state can't be set via the API, so `- [x]` round-trips as unchecked — don't try to
toggle a checkbox by editing the file.

**Nested lists** — indent by the parent marker width: **2 spaces** under `* `,
**3 spaces** under `1. `. Mixed-kind nesting (e.g. a numbered parent with bulleted
children) is **not supported** — don't author it.

**Headings** — `#` through `######` (H1–H6). An empty heading is `# ` with a
trailing space.

**Links** — `[text](url)`. Bare URLs stay plain text (as Google does).

**Tables** — GFM pipe tables (`| a | b |` with a `| --- | --- |` separator row).

**Placeholders you must NOT touch** — these represent things only Google can
create. Leave them exactly where they are; deleting one deletes the underlying
element, and typing a new one does nothing (it's dropped):
- `<!-- gdoc:toc -->` — a table of contents. Its entries are auto-generated from
  the headings; the user regenerates it in the Docs UI after heading changes.
- `![](gdoc:image/…)`, `![](gdoc:pagebreak)`, `![](gdoc:footnote/…)`,
  `![](gdoc:columnbreak)`, `![](gdoc:autotext)` — inline images, page/column
  breaks, footnotes, page-number fields.
- `---` / `***` / `___` — horizontal rules. Existing ones are preserved; a
  **newly typed** rule is dropped with a warning (the API can't insert them).

**Not in the markdown at all** — comments (Drive's separate layer), and
headers/footers/page numbers (outside the doc body). You won't see them; don't try
to add or edit them. Deleting text that a comment is anchored to may orphan it.

**If sync refuses** — if a push is refused because of unsupported content
(equations, smart chips, mid-document section breaks), the doc has something
gdocdown can't safely model. Don't force it with `push`; tell the user.

## Don't touch

The `.<file>.gdocdown.json` sidecar next to the file is gdocdown's baseline state.
Don't edit, move, or commit it. Deleting it forces the next `sync` to re-pull.

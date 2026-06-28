#!/usr/bin/env python3
"""Create (or repopulate) the gdocdown test fixture from tests/baseline.md.

Auth reuses whatever Application Default Credentials are configured (they need
the `documents` scope, which grants Docs read/write), so this only shells out to
`gcloud auth application-default print-access-token` — no OAuth flow of our own.

Usage:
  python3 tools/create_fixture.py            # create a NEW doc, then populate
  python3 tools/create_fixture.py <DOC_ID>   # repopulate an EXISTING doc

This doubles as the reference implementation of "reset to baseline" that the
Rust harness will port: parse baseline.md -> one insertText -> heading styles ->
list bullets, all in a single revision-locked batchUpdate.
"""
import json
import pathlib
import re
import subprocess
import sys
import urllib.error
import urllib.request

DOCS = "https://docs.googleapis.com/v1/documents"
ROOT = pathlib.Path(__file__).resolve().parent.parent
BASELINE_MD = ROOT / "tests" / "baseline.md"
DOCID_FILE = ROOT / "tests" / "fixture.docid"

# Markdown heading level -> Docs named style; list kind -> bullet preset.
BULLET_PRESET = {
    "UL": "BULLET_DISC_CIRCLE_SQUARE",
    "OL": "NUMBERED_DECIMAL_ALPHA_ROMAN",
}


def token():
    return subprocess.check_output(
        ["gcloud", "auth", "application-default", "print-access-token"],
        text=True,
    ).strip()


def api(method, url, body, tok):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(
        url,
        data=data,
        method=method,
        headers={"Authorization": f"Bearer {tok}", "Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req) as r:
            return json.load(r)
    except urllib.error.HTTPError as e:
        sys.stderr.write(f"HTTP {e.code} on {method} {url}\n{e.read().decode()}\n")
        raise


def parse_baseline(md):
    """Trivial markdown subset -> [(text, kind)] where kind is ('H', level)|'UL'|'OL'|'P'."""
    blocks = []
    for raw in md.splitlines():
        line = raw.rstrip()
        if not line.strip():
            continue
        m = re.match(r"(#{1,6})\s+(.*)", line)
        if m:
            blocks.append((m.group(2), ("H", len(m.group(1)))))
        elif line.startswith("- "):
            blocks.append((line[2:], ("UL", None)))
        elif re.match(r"\d+\.\s+", line):
            blocks.append((re.sub(r"^\d+\.\s+", "", line), ("OL", None)))
        else:
            blocks.append((line, ("P", None)))
    return blocks


def build_requests(blocks):
    """One insertText for all text, then non-length-changing style/bullet requests.

    The whole body is inserted at index 1 WITHOUT a trailing newline so the doc's
    pre-existing final newline terminates the last paragraph (no stray empty para).
    Style and bullet requests don't change indices, so they all ride one batch.
    """
    texts = [t for t, _ in blocks]
    insert = "\n".join(texts)
    requests = [{"insertText": {"location": {"index": 1}, "text": insert}}]

    # Normalize EVERY paragraph back to a clean slate first. Google Docs
    # paragraphs inherit formatting, so inserting into a doc whose surviving
    # paragraph carried a bullet would splatter that list across all paragraphs.
    # Clearing bullets + forcing NORMAL_TEXT across the whole body fixes that
    # before we apply the specific heading/list styles below.
    full_end = len(insert) + 1  # index of the (immovable) final newline
    requests += [
        {"deleteParagraphBullets": {"range": {"startIndex": 1, "endIndex": full_end}}},
        {"updateParagraphStyle": {
            "range": {"startIndex": 1, "endIndex": full_end},
            "paragraphStyle": {"namedStyleType": "NORMAL_TEXT"},
            "fields": "namedStyleType",
        }},
    ]

    # Paragraph ranges in post-insert coordinates (body starts at index 1; each
    # paragraph owns its text plus a terminating newline).
    ranges, idx = [], 1
    for t in texts:
        start, end = idx, idx + len(t) + 1
        ranges.append((start, end))
        idx = end

    for (text, kind), (s, e) in zip(blocks, ranges):
        if kind[0] == "H":
            requests.append({
                "updateParagraphStyle": {
                    "range": {"startIndex": s, "endIndex": e},
                    "paragraphStyle": {"namedStyleType": f"HEADING_{kind[1]}"},
                    "fields": "namedStyleType",
                }
            })

    # Apply bullets across each run of consecutive same-kind list items.
    i = 0
    while i < len(blocks):
        kind = blocks[i][1][0]
        if kind in BULLET_PRESET:
            j = i
            while j < len(blocks) and blocks[j][1][0] == kind:
                j += 1
            requests.append({
                "createParagraphBullets": {
                    "range": {"startIndex": ranges[i][0], "endIndex": ranges[j - 1][1]},
                    "bulletPreset": BULLET_PRESET[kind],
                }
            })
            i = j
        else:
            i += 1
    return requests


def main():
    tok = token()
    clear = []
    if len(sys.argv) > 1:
        doc_id = sys.argv[1]
        doc = api("GET", f"{DOCS}/{doc_id}", None, tok)
        rev = doc["revisionId"]
        # Wipe existing content first (everything but the immovable final newline)
        # so reset-to-baseline is idempotent rather than appending.
        end = doc["body"]["content"][-1]["endIndex"]
        if end - 1 > 1:
            clear = [{"deleteContentRange": {"range": {"startIndex": 1, "endIndex": end - 1}}}]
    else:
        doc = api("POST", DOCS, {"title": "gdocdown test fixture"}, tok)
        doc_id, rev = doc["documentId"], doc["revisionId"]

    reqs = clear + build_requests(parse_baseline(BASELINE_MD.read_text()))
    api(
        "POST",
        f"{DOCS}/{doc_id}:batchUpdate",
        {"requests": reqs, "writeControl": {"requiredRevisionId": rev}},
        tok,
    )

    DOCID_FILE.write_text(doc_id + "\n")
    print(doc_id)
    sys.stderr.write(f"Fixture ready: https://docs.google.com/document/d/{doc_id}/edit\n")


if __name__ == "__main__":
    main()

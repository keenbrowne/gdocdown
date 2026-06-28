//! Offline simulator of Google Docs `batchUpdate` text semantics.
//!
//! Applies a request list to a document body string so we can verify
//! `diff_to_requests` without touching the network. This mirrors Google's real
//! behavior: requests apply **sequentially**, and each request's index refers
//! to the document *as it stands at that point* in the batch. Because
//! `diff_to_requests` emits edits tail-first (highest index first), applying
//! them in array order never disturbs an index a later request still needs.
//!
//! Index convention matches [`crate::model_to_plain`]: doc index `N` is the
//! character at string offset `N - 1` (index 0 is reserved; the body starts at
//! index 1).

use crate::Request;

/// Apply `requests` to `text` and return the resulting body text.
///
/// Panics (intentionally — a test signal) if a request references an
/// out-of-range index, which would mean `diff_to_requests` produced bad math.
pub fn apply(text: &str, requests: &[Request]) -> String {
    let mut chars: Vec<char> = text.chars().collect();
    for req in requests {
        match req {
            Request::InsertText(it) => {
                let off = it.location.index - 1;
                chars.splice(off..off, it.text.chars());
            }
            Request::DeleteContentRange(d) => {
                chars.drain((d.range.start_index - 1)..(d.range.end_index - 1));
            }
            // Style / bullet requests don't change the body text, so the text
            // simulator ignores them. Structural fidelity is verified live.
            Request::UpdateParagraphStyle(_)
            | Request::CreateParagraphBullets(_)
            | Request::DeleteParagraphBullets(_)
            | Request::UpdateTextStyle(_) => {}
        }
    }
    chars.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DeleteContentRange, InsertText, Location, Range};

    #[test]
    fn applies_insert_at_doc_index() {
        let reqs = vec![Request::InsertText(InsertText {
            location: Location { index: 7 },
            text: "brave ".into(),
        })];
        assert_eq!(apply("Hello world\n", &reqs), "Hello brave world\n");
    }

    #[test]
    fn applies_delete_range() {
        let reqs = vec![Request::DeleteContentRange(DeleteContentRange {
            range: Range { start_index: 8, end_index: 15 },
        })];
        assert_eq!(apply("line a\nline b\n", &reqs), "line a\n");
    }
}

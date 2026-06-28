//! The core correctness property, fuzzed offline:
//!
//!   for all old, new:  apply(old, diff_to_requests(old -> new)) == new
//!
//! If this holds across thousands of random edit pairs, the index arithmetic in
//! `diff_to_requests` is sound — without a single API call. No external rng
//! crate: a small deterministic LCG keeps failures reproducible.

use gdocdown::{apply::apply, diff_to_requests, markdown_to_model, model_to_plain};

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn rand_str(rng: &mut Lcg, alphabet: &[char], max_len: usize) -> String {
    let len = rng.below(max_len as u64 + 1) as usize;
    (0..len)
        .map(|_| alphabet[rng.below(alphabet.len() as u64) as usize])
        .collect()
}

#[test]
fn fuzz_roundtrip_apply_equals_new() {
    // A small alphabet with '\n' maximizes overlap, so the diff produces dense
    // interleavings of insert/delete/replace — the cases most likely to expose
    // index-shift bugs.
    let alphabet = ['a', 'b', 'c', '\n'];
    let mut rng = Lcg(0x1234_5678_9abc_def0);

    for i in 0..50_000 {
        let old = rand_str(&mut rng, &alphabet, 14);
        let new = rand_str(&mut rng, &alphabet, 14);
        let reqs = diff_to_requests(&old, &new);
        let got = apply(&old, &reqs);
        assert_eq!(
            got, new,
            "case {i}: old={old:?} new={new:?} reqs={reqs:?}"
        );
    }
}

#[test]
fn realistic_markdown_edit_through_full_pipeline() {
    // What the daemon actually does: current doc text -> diff against the model
    // parsed from edited markdown -> apply -> must equal the desired text.
    let current = model_to_plain(&markdown_to_model(
        "# Project Plan\nWe ship in Q3.\n## Risks\nNone identified.",
    ));
    let edited_md = "# Project Plan\nWe ship in early Q3.\n## Risks\nNone identified yet.\nReview weekly.";

    let desired = model_to_plain(&markdown_to_model(edited_md));
    let reqs = diff_to_requests(&current, &desired);

    assert_eq!(apply(&current, &reqs), desired);
    // And it's idempotent: re-diffing the result yields no work.
    assert!(diff_to_requests(&desired, &desired).is_empty());
}

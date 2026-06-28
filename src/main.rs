//! gdocdown CLI: edit a Google Doc as a local markdown file.
//!
//!   gdocdown pull  <docId> <file.md>   doc -> markdown file (records a baseline)
//!   gdocdown sync  <docId> <file.md>   merge-safe one-shot: 3-way merge vs the
//!                                       baseline, then push (concurrent-edit safe)
//!   gdocdown push  <docId> <file.md>   force: make the doc equal the file (NOT
//!                                       merge-safe — overwrites concurrent edits)
//!   gdocdown watch <docId> <file.md>   continuous: seed, then reconcile on save/poll
//!
//! For an editor/agent working alongside live collaborators, use the controlled
//! loop: `pull` once, edit the file, `sync` to merge-and-push. `sync` reuses the
//! same 3-way merge as `watch`; conflicts are written to the file (never the doc).
//!
//! Auth reuses Application Default Credentials (a fresh token is minted per
//! operation, so a long-running `watch` survives token expiry).

use gdocdown::api::{document_to_model, sync_apply, Docs};
use gdocdown::{markdown_to_model, model_to_markdown};
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::time::Duration;

fn main() {
    let raw: Vec<String> = std::env::args().collect();
    let force = raw.iter().any(|a| a == "--force" || a == "-f");
    let args: Vec<&str> = raw.iter().map(String::as_str).filter(|a| *a != "--force" && *a != "-f").collect();
    let result = match args.as_slice() {
        [_, "pull", doc, file] => pull(doc, file, force),
        [_, "sync", doc, file] => sync(doc, file),
        [_, "push", doc, file] => push(doc, file, force),
        [_, "watch", doc, file] => watch(doc, file),
        _ => {
            eprintln!("usage: gdocdown <sync|pull|push|watch> [--force] <docId> <file.md>");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

// --- Baseline store -------------------------------------------------------
//
// A one-shot `sync` needs the *common ancestor* (the markdown where the file and
// doc last agreed) to do a 3-way merge. We persist it as the last-synced baseline:
// one small JSON file per (doc, file) pair, kept under `~/.gdocdown/` so it stays
// out of the user's working directory. (FUTURE: use per-OS state dirs — see README.)

/// `~/.gdocdown/`, created on demand.
fn baseline_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("HOME not set")?;
    let dir = PathBuf::from(home).join(".gdocdown");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

/// Absolute path of `file`, stable even before it exists (the parent dir must
/// exist; we canonicalize that and append the name). Used as part of the key.
fn abs_path(file: &Path) -> PathBuf {
    if let Ok(c) = file.canonicalize() {
        return c;
    }
    let name = file.file_name().map(PathBuf::from).unwrap_or_default();
    let parent = file.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    parent.canonicalize().map(|p| p.join(name)).unwrap_or_else(|_| file.to_path_buf())
}

/// Stable 64-bit FNV-1a hash, hex-encoded — deterministic across runs/machines
/// (unlike `DefaultHasher`), so a baseline file name is reproducible.
fn key_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

fn baseline_path(doc: &str, file: &Path) -> Result<PathBuf, String> {
    let key = format!("{doc}\0{}", abs_path(file).display());
    Ok(baseline_dir()?.join(format!("{}.json", key_hash(&key))))
}

fn write_baseline(doc: &str, file: &Path, rev: &str, md: &str) -> Result<(), String> {
    let v = serde_json::json!({ "doc": doc, "path": abs_path(file).display().to_string(), "rev": rev, "md": md });
    std::fs::write(baseline_path(doc, file)?, serde_json::to_vec_pretty(&v).unwrap()).map_err(|e| e.to_string())
}

fn read_baseline(doc: &str, file: &Path) -> Result<(String, String), String> {
    let raw = std::fs::read_to_string(baseline_path(doc, file)?)
        .map_err(|_| format!("no baseline for {} — run `gdocdown pull` first", file.display()))?;
    let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    // Defensive: the key already includes the doc id, but verify the stored record
    // matches so a hash collision or hand-edit can't merge against the wrong base.
    if v["doc"].as_str() != Some(doc) {
        return Err(format!("baseline for {} belongs to a different doc — run `gdocdown pull`", file.display()));
    }
    Ok((
        v["rev"].as_str().unwrap_or_default().to_string(),
        v["md"].as_str().unwrap_or_default().to_string(),
    ))
}

/// Take theirs: overwrite the file with the doc and record a baseline. Refuses if
/// the file has unsynced local edits (use `sync` to merge, or `--force`). Only the
/// local file is at risk — the doc is never touched — so this guard is soft.
fn pull(doc: &str, file: &str, force: bool) -> Result<(), String> {
    let path = Path::new(file);
    if !force {
        match read_baseline(doc, path) {
            Ok((_, base_md)) if std::fs::read_to_string(path).unwrap_or_default() != base_md => {
                return Err(format!(
                    "{} has local edits that pull would discard — `gdocdown sync` to merge them, \
                     or `gdocdown pull --force` to discard them",
                    path.display()
                ));
            }
            Err(_) if std::fs::read_to_string(path).map(|s| !s.trim().is_empty()).unwrap_or(false) => {
                return Err(format!(
                    "{} has local content and no baseline — `gdocdown pull --force` to overwrite it",
                    path.display()
                ));
            }
            _ => {}
        }
    }
    let docs = Docs::new();
    let d = docs.get(doc);
    let md = model_to_markdown(&document_to_model(&d));
    std::fs::write(path, &md).map_err(|e| e.to_string())?;
    write_baseline(doc, path, &rev_of(&d), &md)?;
    println!("pulled {doc} -> {file}");
    Ok(())
}

/// Merge-safe one-shot: 3-way merge the file against the baseline and the live doc
/// (same logic as `watch`), then push the result. Safe against concurrent edits;
/// conflicts are written to the file only. Requires a prior `pull`.
fn sync(doc: &str, file: &str) -> Result<(), String> {
    let path = Path::new(file);
    let (base_rev, base_md) = match read_baseline(doc, path) {
        Ok(b) => b,
        // No baseline yet: if there's nothing local to lose, bootstrap by pulling;
        // otherwise refuse rather than guess (use `pull`/`push` to pick a side).
        Err(_) => {
            if std::fs::read_to_string(path).unwrap_or_default().trim().is_empty() {
                return pull(doc, file, false);
            }
            return Err(format!(
                "{} has local content but no baseline — run `gdocdown pull` to take the doc \
                 (discards local), or `gdocdown push` to overwrite the doc with the file",
                path.display()
            ));
        }
    };
    let docs = Docs::new();
    match reconcile(&docs, doc, path, &base_rev, &base_md)? {
        Some((rev, md)) => write_baseline(doc, path, &rev, &md)?,
        None => println!("nothing to sync (file and doc match the baseline)"),
    }
    Ok(())
}

/// Take mine: force the document to equal the file. This can overwrite *other
/// people's* concurrent edits — high blast radius — so by default it refuses if the
/// doc has moved since the baseline (force-with-lease semantics); `--force` to
/// override regardless. Records a fresh baseline so later `sync`s stay aligned.
fn push(doc: &str, file: &str, force: bool) -> Result<(), String> {
    let md = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
    let path = Path::new(file);
    let docs = Docs::new();
    let before = docs.get(doc);
    if !force {
        match read_baseline(doc, path) {
            Ok((base_rev, _)) if base_rev == rev_of(&before) => {} // doc hasn't moved — safe
            Ok(_) => {
                return Err(
                    "the doc changed since your last sync — `gdocdown sync` to merge, \
                     or `gdocdown push --force` to overwrite collaborators' edits"
                        .to_string(),
                )
            }
            Err(_) => {
                return Err(format!(
                    "no baseline for {} — `gdocdown sync` first, or `gdocdown push --force` to overwrite the doc",
                    path.display()
                ))
            }
        }
    }
    let warnings = sync_apply(&docs, doc, &markdown_to_model(&md))?;
    for w in &warnings {
        println!("  \u{26a0} {w}");
    }
    write_baseline(doc, path, &rev_of(&docs.get(doc)), &md)?; // doc now equals the file
    println!("pushed {file} -> {doc}");
    Ok(())
}

fn rev_of(doc: &serde_json::Value) -> String {
    doc["revisionId"].as_str().unwrap_or_default().to_string()
}

/// Reconcile the file and the doc against the last-synced baseline. Returns the
/// new (revision, markdown) baseline if anything changed, else `None`.
///
///   only local changed  -> push the file to the doc
///   only remote changed -> pull the doc into the file
///   both changed        -> 3-way merge (base, local, remote):
///                            clean      -> write merged + push it
///                            conflicting -> write conflict markers, don't push
fn reconcile(docs: &Docs, doc: &str, path: &Path, base_rev: &str, base_md: &str) -> Result<Option<(String, String)>, String> {
    let cur = docs.get(doc);
    let rev = rev_of(&cur);
    let local = std::fs::read_to_string(path).unwrap_or_else(|_| base_md.to_string());
    let local_changed = local != base_md;
    let remote_changed = rev != base_rev;

    match (local_changed, remote_changed) {
        (false, false) => Ok(None),
        (true, false) => {
            push_model(docs, doc, &local)?;
            println!("\u{2192} pushed local edits");
            Ok(Some((rev_of(&docs.get(doc)), local)))
        }
        (false, true) => {
            let remote = model_to_markdown(&document_to_model(&cur));
            if remote != base_md {
                std::fs::write(path, &remote).map_err(|e| e.to_string())?;
                println!("\u{2190} pulled remote edits");
            }
            Ok(Some((rev, remote)))
        }
        (true, true) => {
            let remote = model_to_markdown(&document_to_model(&cur));
            match diffy::merge(base_md, &local, &remote) {
                Ok(merged) => {
                    std::fs::write(path, &merged).map_err(|e| e.to_string())?;
                    push_model(docs, doc, &merged)?;
                    println!("\u{21c4} merged local + remote edits");
                    Ok(Some((rev_of(&docs.get(doc)), merged)))
                }
                Err(conflicted) => {
                    // Keep the conflict markers in the file for the user to
                    // resolve; leave the doc as-is (don't push markers).
                    std::fs::write(path, &conflicted).map_err(|e| e.to_string())?;
                    eprintln!("\u{26a0} merge conflict — resolve the <<< markers in {} and save", path.display());
                    Ok(Some((rev, conflicted)))
                }
            }
        }
    }
}

fn push_model(docs: &Docs, doc: &str, md: &str) -> Result<(), String> {
    for w in sync_apply(docs, doc, &markdown_to_model(md))? {
        eprintln!("  \u{26a0} {w}");
    }
    Ok(())
}

/// One reconcile step: refresh the token if stale, then reconcile and update the
/// in-memory baseline.
fn tick(docs: &mut Docs, minted: &mut std::time::Instant, doc: &str, path: &Path, rev: &mut String, md: &mut String) {
    if minted.elapsed() > Duration::from_secs(30 * 60) {
        *docs = Docs::new();
        *minted = std::time::Instant::now();
    }
    match reconcile(docs, doc, path, rev, md) {
        Ok(Some((r, m))) => {
            *rev = r;
            *md = m;
        }
        Ok(None) => {}
        Err(e) => eprintln!("  \u{2717} {e}"),
    }
}

/// Bidirectional sync: seed the file from the doc, then push local saves to the
/// doc AND pull remote edits back into the file (detected by polling the doc's
/// revision). Loop-free: our own pull-writes match the synced baseline and are
/// ignored; after a push the new revision is recorded so the next poll is quiet.
/// Conflicts (both sides changed) keep the local copy — the next save pushes it.
fn watch(doc: &str, file: &str) -> Result<(), String> {
    use notify::{RecursiveMode, Watcher};
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Instant;

    let path = Path::new(file);
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let target = path.file_name().ok_or("invalid file path")?.to_os_string();

    let mut docs = Docs::new();
    let mut minted = Instant::now();

    // Seed the file from the doc — establishes the synced baseline.
    let seed = docs.get(doc);
    let mut synced_rev = rev_of(&seed);
    let mut synced_md = model_to_markdown(&document_to_model(&seed));
    std::fs::write(path, &synced_md).map_err(|e| e.to_string())?;
    println!("seeded {file} from {doc}");

    let (tx, rx) = channel();
    let mut watcher =
        notify::recommended_watcher(move |res| { let _ = tx.send(res); }).map_err(|e| e.to_string())?;
    watcher.watch(&dir, RecursiveMode::NonRecursive).map_err(|e| e.to_string())?;
    let poll = Duration::from_secs(3);
    println!("watching {file} \u{2194} {doc} (Ctrl-C to stop)");

    loop {
        match rx.recv_timeout(poll) {
            // A file event (local save) or the poll timeout both trigger a full
            // reconcile of file vs doc against the synced baseline.
            Ok(Ok(ev)) => {
                if !ev.paths.iter().any(|p| p.file_name() == Some(target.as_os_str())) {
                    continue;
                }
                while rx.recv_timeout(Duration::from_millis(300)).is_ok() {} // debounce
                tick(&mut docs, &mut minted, doc, path, &mut synced_rev, &mut synced_md);
            }
            Err(RecvTimeoutError::Timeout) => {
                tick(&mut docs, &mut minted, doc, path, &mut synced_rev, &mut synced_md);
            }
            Ok(Err(_)) => continue,                       // watcher hiccup
            Err(RecvTimeoutError::Disconnected) => break, // sender dropped
        }
    }
    Ok(())
}

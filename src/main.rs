//! gdocdown CLI: edit a Google Doc as a local markdown file.
//!
//!   gdocdown pull  <docId> <file.md>   doc  -> markdown file
//!   gdocdown push  <docId> <file.md>   markdown file -> doc
//!   gdocdown watch <docId> <file.md>   seed the file, then push on every save
//!
//! Auth reuses Application Default Credentials (a fresh token is minted per
//! operation, so a long-running `watch` survives token expiry).

use gdocdown::api::{document_to_model, sync_apply, Docs};
use gdocdown::{markdown_to_model, model_to_markdown};
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [_, "pull", doc, file] => pull(doc, file),
        [_, "push", doc, file] => push(doc, file),
        [_, "watch", doc, file] => watch(doc, file),
        _ => {
            eprintln!("usage: gdocdown <pull|push|watch> <docId> <file.md>");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

/// Generate the markdown file from the current document.
fn pull(doc: &str, file: &str) -> Result<(), String> {
    let docs = Docs::new();
    let md = model_to_markdown(&document_to_model(&docs.get(doc)));
    std::fs::write(file, md).map_err(|e| e.to_string())?;
    println!("pulled {doc} -> {file}");
    Ok(())
}

/// Parse the markdown file and sync it into the document.
fn push(doc: &str, file: &str) -> Result<(), String> {
    let md = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
    let docs = Docs::new();
    let warnings = sync_apply(&docs, doc, &markdown_to_model(&md))?;
    for w in &warnings {
        println!("  \u{26a0} {w}");
    }
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

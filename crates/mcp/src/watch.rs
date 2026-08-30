//! OS-file-watch-backed freshness for the daemon (§7.5): a recursive watch (FSEvents on
//! macOS, inotify on Linux) over the source root marks a dirty flag, and queries revalidate
//! lazily — so the steady-state freshness check is one atomic load instead of a stat sweep.
//!
//! The watch is a **necessary-condition filter** in the §3.4 sense: it may only skip
//! revalidation when nothing relevant can have changed, and every doubt fails open to
//! revalidation — the flag starts dirty (changes between the last index and daemon startup
//! produced no events), watcher errors and event overflows mark dirty, and a failed rebuild
//! re-marks dirty so the next query retries. Filtering only ever *keeps* the flag clean for
//! provably irrelevant events: reads, hidden trees (`.vorpal`, `.git` — which also breaks the
//! rebuild→index-write→event cycle), and gitignored paths (`target/` build churn).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::event::{Event, EventKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

/// Bound on the captured change set: past this, the set degrades to "unknown" and the next
/// revalidation full-scans (a change this large re-parses enough that the sweep is noise).
const MAX_CAPTURED_CHANGES: usize = 4096;

/// A live recursive watch over one source root, feeding a dirty flag and — when the event
/// stream permits certainty — the exact set of changed file paths.
pub(crate) struct SourceWatch {
  src: PathBuf,
  dirty: Arc<AtomicBool>,
  /// `Some(set)` = every relevant change since the last take is in the set (a complete
  /// capture, safe to hint the manifest with). `None` = certainty was lost (startup gap,
  /// watcher error, overflow, a directory-level event, or set-size cap) and the next
  /// revalidation must full-scan. Doubt always degrades to `None`, never to a wrong set —
  /// the same necessary-condition contract the dirty flag keeps.
  changed: Arc<Mutex<Option<HashSet<PathBuf>>>>,
  /// Held for the daemon's lifetime; dropping it stops event delivery.
  _watcher: RecommendedWatcher,
}

impl SourceWatch {
  /// Start watching `src`. `None` means watching could not be established — the caller then
  /// behaves exactly as an unwatched daemon (explicit `index` calls only), never staler.
  pub(crate) fn start(src: &Path) -> Option<SourceWatch> {
    let src = src.canonicalize().ok()?;
    // Root .gitignore only: nested ignore files would need a full walk to honor, and a missed
    // ignore merely costs one cheap fast-path revalidation, never staleness.
    let (ignore, _err) = GitignoreBuilder::new(&src).build_global();
    let ignore = {
      let mut builder = GitignoreBuilder::new(&src);
      builder.add(src.join(".gitignore"));
      builder.build().unwrap_or(ignore)
    };

    let dirty = Arc::new(AtomicBool::new(true));
    // Startup gap: changes before the daemon existed produced no events → unknown.
    let changed: Arc<Mutex<Option<HashSet<PathBuf>>>> = Arc::new(Mutex::new(None));
    let flag = Arc::clone(&dirty);
    let capture = Arc::clone(&changed);
    let root = src.clone();
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
      match result {
        Ok(event) => {
          if event_is_relevant(&root, &ignore, &event) {
            let mut capture = capture.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(set) = capture.as_mut() {
              // Only individual FILE paths can be captured with certainty: a directory
              // event (rename/move) can imply changes the stream never itemizes.
              let mut certain = true;
              for path in &event.paths {
                if path.is_file() {
                  set.insert(path.clone());
                } else {
                  certain = false;
                  break;
                }
              }
              if !certain || set.len() > MAX_CAPTURED_CHANGES {
                *capture = None;
              }
            }
            flag.store(true, Ordering::Release);
          }
        }
        // A watcher error means events may have been lost: assume the worst.
        Err(_) => {
          *capture
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
          flag.store(true, Ordering::Release);
        }
      }
    })
    .ok()?;
    watcher.watch(&src, RecursiveMode::Recursive).ok()?;
    Some(SourceWatch {
      src,
      dirty,
      changed,
      _watcher: watcher,
    })
  }

  pub(crate) fn src(&self) -> &Path {
    &self.src
  }

  /// Consume the dirty flag: `true` means something relevant may have changed since the last
  /// take (or since startup) and the caller must revalidate.
  pub(crate) fn take_dirty(&self) -> bool {
    self.dirty.swap(false, Ordering::AcqRel)
  }

  /// Consume the captured change set. `Some(paths)` is a COMPLETE set of every relevant file
  /// change since the previous take — safe to hint a manifest patch with; `None` means
  /// certainty was lost and the caller must full-scan. Either way capture restarts complete.
  pub(crate) fn take_changes(&self) -> Option<HashSet<PathBuf>> {
    self
      .changed
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .replace(HashSet::new())
  }

  /// Re-arm the flag — used when a revalidation attempt failed, so the next query retries
  /// instead of serving the pre-failure graph as if it were fresh. Capture certainty is
  /// poisoned too: the failed attempt consumed the set.
  pub(crate) fn mark_dirty(&self) {
    *self
      .changed
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    self.dirty.store(true, Ordering::Release);
  }
}

/// Whether an event can possibly affect index contents.
fn event_is_relevant(root: &Path, ignore: &Gitignore, event: &Event) -> bool {
  // Overflow/rescan: events were dropped — anything may have changed.
  if event.need_rescan() {
    return true;
  }
  // Reads can't change sources — and the daemon's own rebuilds read every source file, which
  // on access-reporting backends would otherwise re-dirty the flag they just cleared.
  if matches!(event.kind, EventKind::Access(_)) {
    return false;
  }
  event
    .paths
    .iter()
    .any(|path| path_is_relevant(root, ignore, path))
}

/// Whether a changed path can possibly affect index contents: not inside a hidden tree (the
/// walker never indexes those — this also covers `.vorpal` itself and `.git`), and not
/// gitignored (`target/` churn from builds). Everything else — including extensionless paths
/// and directory-level events, whose reach we can't bound — counts as relevant.
fn path_is_relevant(root: &Path, ignore: &Gitignore, path: &Path) -> bool {
  let rel = path.strip_prefix(root).unwrap_or(path);
  let hidden = rel.components().any(|component| {
    let name = component.as_os_str().to_string_lossy();
    name.starts_with('.') && name != "." && name != ".."
  });
  if hidden {
    return false;
  }
  !ignore
    .matched_path_or_any_parents(path, path.is_dir())
    .is_ignore()
}

#[cfg(test)]
mod tests {
  use super::*;
  use notify::event::{AccessKind, CreateKind, EventAttributes, Flag, ModifyKind};

  fn matcher(root: &Path, gitignore: &str) -> Gitignore {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(root.join(".gitignore"), gitignore).unwrap();
    let mut builder = GitignoreBuilder::new(root);
    builder.add(root.join(".gitignore"));
    builder.build().unwrap()
  }

  #[test]
  fn filters_hidden_ignored_and_reads_but_keeps_sources_and_overflow() {
    let root = std::env::temp_dir().join(format!("vorpal-watch-filter-{}", std::process::id()));
    let ignore = matcher(&root, "/target\n*.log\n");

    let event = |kind: EventKind, path: &Path| Event {
      kind,
      paths: vec![path.to_path_buf()],
      attrs: EventAttributes::new(),
    };
    let modify = EventKind::Modify(ModifyKind::Any);

    // A source edit is relevant; so is an extensionless or directory-level path.
    assert!(event_is_relevant(
      &root,
      &ignore,
      &event(modify, &root.join("src/lib.rs"))
    ));
    assert!(event_is_relevant(
      &root,
      &ignore,
      &event(modify, &root.join("Makefile"))
    ));
    assert!(event_is_relevant(
      &root,
      &ignore,
      &event(EventKind::Create(CreateKind::Folder), &root.join("newdir"))
    ));

    // Hidden trees (the index's own writes live under `.vorpal`) and gitignored churn do not.
    assert!(!event_is_relevant(
      &root,
      &ignore,
      &event(modify, &root.join(".vorpal/index/nodes.vseg"))
    ));
    assert!(!event_is_relevant(
      &root,
      &ignore,
      &event(modify, &root.join(".git/HEAD"))
    ));
    assert!(!event_is_relevant(
      &root,
      &ignore,
      &event(modify, &root.join("target/debug/build/out.rs"))
    ));
    assert!(!event_is_relevant(
      &root,
      &ignore,
      &event(modify, &root.join("build.log"))
    ));

    // Reads never dirty.
    assert!(!event_is_relevant(
      &root,
      &ignore,
      &event(EventKind::Access(AccessKind::Any), &root.join("src/lib.rs"))
    ));

    // Overflow/rescan dirties unconditionally, even for an otherwise-ignored path.
    let mut attrs = EventAttributes::new();
    attrs.set_flag(Flag::Rescan);
    let overflow = Event {
      kind: EventKind::Other,
      paths: vec![root.join("target/whatever")],
      attrs,
    };
    assert!(event_is_relevant(&root, &ignore, &overflow));

    let _ = std::fs::remove_dir_all(&root);
  }
}

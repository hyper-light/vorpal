//! Change-impact: git-diff-seeded reverse reachability (ADOPTION B1).
//!
//! `impact --since <ref>` answers "what could my changes break?": the changed files'
//! definitions become BFS seeds, and everything transitively reaching them — through the
//! chosen relations, at or above the chosen grade — is the blast radius, each node with its
//! minimum hop distance (the multi-seed BFS computes min-hop by construction).
//!
//! Git is invoked as a subprocess (`git -C <src> …`) — no libgit2 dependency; failures
//! surface git's own stderr verbatim (a wrong ref should read exactly like git said it).

use std::path::Path;
use std::process::Command;

/// The changed-path set for `since`:
/// - `Some(ref)` → `git diff --name-only <merge-base(ref, HEAD)>` plus worktree changes —
///   "everything this branch/worktree changes relative to ref", the review question;
/// - `None` → uncommitted changes only (`git status --porcelain`), the pre-commit question.
///
/// Paths come back relative to the repo root; callers join them onto the indexed root.
pub fn changed_paths(src: &Path, since: Option<&str>) -> Result<Vec<String>, String> {
  let git = |args: &[&str]| -> Result<String, String> {
    let out = Command::new("git")
      .arg("-C")
      .arg(src)
      .args(args)
      .output()
      .map_err(|err| format!("running git: {err}"))?;
    if !out.status.success() {
      return Err(format!(
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
      ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
  };

  let mut paths: Vec<String> = Vec::new();
  match since {
    Some(reference) => {
      let base = git(&["merge-base", "HEAD", reference])?;
      let base = base.trim();
      paths.extend(
        git(&["diff", "--name-only", base])?
          .lines()
          .map(str::to_string),
      );
    }
    None => {
      for line in git(&["status", "--porcelain"])?.lines() {
        // Format: `XY path` (or `XY old -> new` for renames — take the new side).
        let path = line.get(3..).unwrap_or("");
        let path = path.rsplit(" -> ").next().unwrap_or(path);
        if !path.is_empty() {
          paths.push(path.trim_matches('"').to_string());
        }
      }
    }
  }
  paths.sort();
  paths.dedup();
  Ok(paths)
}

/// Resolve changed repo-relative paths to seed node ids: each path's File node plus every
/// definition it `defines`. Paths not in the index (deleted files, non-indexed types) are
/// counted, not errors — the caller reports them.
pub fn seeds_for_paths(
  kg: &vorpal_kg::Kg,
  root: &Path,
  changed: &[String],
) -> (Vec<vorpal_kg::NodeId>, usize) {
  let mut seeds: Vec<vorpal_kg::NodeId> = Vec::new();
  let mut missing = 0usize;
  // The index stores paths under the CANONICAL root spelling (the build canonicalizes its
  // src so every producer keys identically); a caller's verbatim root (symlinked /tmp,
  // relative spellings) must resolve the same way before joining, or every lookup misses.
  let root = &root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
  for rel in changed {
    // File nodes carry their path as their NAME, so the name index resolves them.
    let full = root.join(rel);
    let spelled = full.to_string_lossy();
    let file_nodes = kg.nodes_named(&spelled);
    if file_nodes.is_empty() {
      missing += 1;
      continue;
    }
    for file in file_nodes {
      seeds.push(file);
      for (child, edge) in kg.out_neighbors(file) {
        if edge.base() == vorpal_kg::EdgeType::DEFINES {
          seeds.push(child);
        }
      }
    }
  }
  seeds.sort_unstable_by_key(|id| id.raw());
  seeds.dedup();
  (seeds, missing)
}

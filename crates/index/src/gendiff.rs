//! Generation diff (ADOPTION B2): what changed between two immutable generations of one
//! index — files added/removed/changed, node-level adds/removes/modifications aligned by
//! durable external id, and per-relation edge-count deltas.
//!
//! Both generations stay open simultaneously (content-addressed dirs are immutable); the
//! comparison rides [`crate::annfiles::file_runs_of`]: runs arrive path-sorted (manifest
//! order), so one merge-join classifies files, and equal run digests skip a file's nodes
//! entirely — a kernel-scale diff touches only the files that actually differ.
//!
//! **What `modified` means.** Alignment is by durable eid, and eids fold the signature into
//! overloadable identities (Function/Method/Constructor) — so a *signature* change on a
//! function is an identity transition and reads `removed + added`, exactly as the identity
//! system defines it. `modified` (same eid, new content hash) fires for non-overloadable
//! identities whose signature changed in place: a field's type, a struct's declaration.
//! Body-only edits change neither identity nor signature, therefore neither the node set
//! nor the graph's semantic content — such files diff as unchanged, by design.

use std::path::{Path, PathBuf};

use vorpal_kg::{Kg, NodeId};

use crate::annfiles::{FileRun, file_runs_of};

/// Resolve a generation spec against an index root:
/// - `CURRENT` (default `to`) → the live generation;
/// - `prev` (default `from`) → the retained non-live generation (GC keeps exactly one);
/// - a 32-hex content id → `root/gen/<id>`;
/// - any existing path → itself.
pub fn resolve_generation(root: &Path, spec: &str) -> Result<PathBuf, String> {
  match spec {
    "CURRENT" | "current" => Ok(vorpal_kg::resolve_index_dir(root)),
    "prev" | "previous" => {
      let live = vorpal_kg::resolve_index_dir(root);
      let gen_root = root.join("gen");
      let mut others: Vec<PathBuf> = std::fs::read_dir(&gen_root)
        .map_err(|err| format!("no generation dir under {}: {err}", root.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && *path != live && !path.ends_with(".staging"))
        .filter(|path| path.join("manifest.bin").exists())
        .collect();
      others.sort();
      match others.len() {
        0 => Err("no previous generation is retained (GC keeps live + prior only)".to_string()),
        1 => Ok(others.remove(0)),
        _ => Err(format!(
          "several non-live generations exist — name one: {}",
          others
            .iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect::<Vec<_>>()
            .join(", ")
        )),
      }
    }
    spec => {
      let by_id = root.join("gen").join(spec);
      if by_id.is_dir() {
        return Ok(by_id);
      }
      let as_path = Path::new(spec);
      if as_path.is_dir() {
        return Ok(as_path.to_path_buf());
      }
      Err(format!("no generation '{spec}' under {}", root.display()))
    }
  }
}

/// One node-level difference, located in whichever generation holds the node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeChange {
  /// In `to`, not in `from` (by eid).
  Added(NodeId),
  /// In `from`, not in `to` (by eid).
  Removed(NodeId),
  /// Same eid in both, different content hash — the definition changed. Carries the `to` id.
  Modified(NodeId),
}

/// The whole diff, node changes kept as cheap ids for page-materialization by the caller.
pub struct GenDiff {
  pub from_generation: String,
  pub to_generation: String,
  pub files_unchanged: usize,
  pub files_added: usize,
  pub files_removed: usize,
  pub files_changed: usize,
  pub changes: Vec<NodeChange>,
  /// (relation name, from-count, to-count) for every relation present in either.
  pub relation_deltas: Vec<(String, u64, u64)>,
}

fn eids_of(kg: &Kg, run: &FileRun) -> Vec<(u128, u64, u64)> {
  // (eid, content_hash, id) for every non-File node in the run — identity columns only,
  // zero heap-string reads (whole-file adds/removes walk entire runs through this). Nodes
  // without eids are skipped (pre-eid segments align as all-added/removed, visible from
  // the generation ages).
  let mut rows = Vec::with_capacity(run.len as usize);
  for at in run.start..run.start + run.len as u64 {
    let id = NodeId::new(at);
    if kg.node_kind(id) == Some(vorpal_kg::SymbolKind::File) {
      continue;
    }
    if let Some((Some(eid), content_hash)) = kg.node_identity(id) {
      rows.push((eid, content_hash, at));
    }
  }
  rows.sort_unstable_by_key(|&(eid, ..)| eid);
  rows
}

/// Diff two open generations. Deterministic: files in path order, node changes in
/// (file, eid) order.
pub fn diff(from: &Kg, to: &Kg, from_label: &str, to_label: &str) -> GenDiff {
  let runs_from = file_runs_of(from);
  let runs_to = file_runs_of(to);
  let mut out = GenDiff {
    from_generation: from_label.to_string(),
    to_generation: to_label.to_string(),
    files_unchanged: 0,
    files_added: 0,
    files_removed: 0,
    files_changed: 0,
    changes: Vec::new(),
    relation_deltas: Vec::new(),
  };

  let mut ai = 0usize;
  let mut bi = 0usize;
  while ai < runs_from.len() || bi < runs_to.len() {
    let order = match (runs_from.get(ai), runs_to.get(bi)) {
      (Some(a), Some(b)) => a.path.cmp(&b.path),
      (Some(_), None) => std::cmp::Ordering::Less,
      (None, Some(_)) => std::cmp::Ordering::Greater,
      (None, None) => break,
    };
    match order {
      std::cmp::Ordering::Less => {
        // Whole file removed: every eid-bearing node in the run is a removal.
        let run = &runs_from[ai];
        out.files_removed += 1;
        out
          .changes
          .extend(eids_of(from, run).into_iter().map(|(_, _, id)| NodeChange::Removed(NodeId::new(id))));
        ai += 1;
      }
      std::cmp::Ordering::Greater => {
        let run = &runs_to[bi];
        out.files_added += 1;
        out
          .changes
          .extend(eids_of(to, run).into_iter().map(|(_, _, id)| NodeChange::Added(NodeId::new(id))));
        bi += 1;
      }
      std::cmp::Ordering::Equal => {
        let (a, b) = (&runs_from[ai], &runs_to[bi]);
        if a.digest == b.digest && a.len == b.len {
          out.files_unchanged += 1;
        } else {
          out.files_changed += 1;
          // eid merge-join within the two runs.
          let (rows_a, rows_b) = (eids_of(from, a), eids_of(to, b));
          let (mut i, mut j) = (0usize, 0usize);
          while i < rows_a.len() || j < rows_b.len() {
            match (rows_a.get(i), rows_b.get(j)) {
              (Some(&(ea, ha, _)), Some(&(eb, hb, idb))) if ea == eb => {
                if ha != hb {
                  out.changes.push(NodeChange::Modified(NodeId::new(idb)));
                }
                i += 1;
                j += 1;
              }
              (Some(&(ea, _, ida)), Some(&(eb, ..))) if ea < eb => {
                out.changes.push(NodeChange::Removed(NodeId::new(ida)));
                i += 1;
              }
              (Some(_), Some(&(_, _, idb))) => {
                out.changes.push(NodeChange::Added(NodeId::new(idb)));
                j += 1;
              }
              (Some(&(_, _, ida)), None) => {
                out.changes.push(NodeChange::Removed(NodeId::new(ida)));
                i += 1;
              }
              (None, Some(&(_, _, idb))) => {
                out.changes.push(NodeChange::Added(NodeId::new(idb)));
                j += 1;
              }
              (None, None) => break,
            }
          }
        }
        ai += 1;
        bi += 1;
      }
    }
  }

  // Relation deltas from the two one-pass bucket counts.
  let (from_counts, to_counts) = (from.edge_count_by_type(), to.edge_count_by_type());
  let mut names: Vec<String> = from_counts
    .iter()
    .chain(to_counts.iter())
    .map(|(edge, _)| edge.name().to_string())
    .collect();
  names.sort();
  names.dedup();
  for name in names {
    let of = |counts: &[(vorpal_kg::EdgeType, u64)]| {
      counts
        .iter()
        .find(|(edge, _)| edge.name() == name)
        .map_or(0, |&(_, count)| count)
    };
    let (from_count, to_count) = (of(&from_counts), of(&to_counts));
    out.relation_deltas.push((name, from_count, to_count));
  }
  out
}

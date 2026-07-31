//! Graph-predicate evaluation (IMPROVEMENTS #5) — the index side of rule `graph:` sections.
//!
//! A structural rule match hands us `(file, capture byte span)`; we answer whether the
//! repository facts hold: the reference at that span resolves to a selected symbol, two
//! captures bind the same definition, or the enclosing definition/file carries a
//! `calls`/`imports`/`implements` edge to a selected target. Everything is answered from the
//! evidence sidecar — the exact per-occurrence record resolution emitted — so a predicate can
//! never claim more than resolution proved, and every verdict carries a human-readable
//! justification for audit output.
//!
//! Three-valued outcomes, because the assessment requires the distinction: a predicate that
//! *evaluated* and failed is a non-match (with the near-miss reported), while facts that
//! could not be obtained at all (file not indexed, generation pinned to something else) are
//! **unavailable** and the rule's `require` policy decides what happens. A below-floor
//! (heuristic) edge is always the former: an explicit, auditable candidate — never an error,
//! never silently rewritten.
//!
//! This module is deliberately independent of the rule-engine crate: the CLI translates the
//! serde schema into these plain types, keeping the index layer rule-format-agnostic.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use vorpal_ingest::{Confidence, ResolveReason};
use vorpal_kg::{EdgeType, EvidenceOutcome, EvidenceRow, Kg, NodeId, SymbolKind};

/// Selects the definition a predicate must reach. Every given field must hold; the caller
/// guarantees at least one is set (schema validation).
#[derive(Debug, Clone, Default)]
pub struct TargetSpec {
  /// Definition name, exact.
  pub name: Option<String>,
  /// Defining file path suffix.
  pub path_suffix: Option<String>,
  /// Durable external id (128-bit).
  pub external_id: Option<u128>,
}

impl TargetSpec {
  fn describe(&self) -> String {
    let mut parts = Vec::new();
    if let Some(name) = &self.name {
      parts.push(format!("name `{name}`"));
    }
    if let Some(suffix) = &self.path_suffix {
      parts.push(format!("path …{suffix}"));
    }
    if let Some(eid) = self.external_id {
      parts.push(format!("eid:{eid:032x}"));
    }
    parts.join(", ")
  }
}

/// One predicate, anchored at a capture span (translated from the rule schema).
#[derive(Debug, Clone)]
pub enum PredicateKind {
  /// The reference at the span must resolve (at or above the floor) to a matching target.
  ResolvesTo(TargetSpec),
  /// The references at this span and at `other` must resolve to the same definition.
  SameBindingAs { other: (u32, u32) },
  /// The definition enclosing the span must have a `calls` edge to a matching target.
  Calls(TargetSpec),
  /// The span's file must have an `imports` edge to a matching target.
  Imports(TargetSpec),
  /// The definition enclosing the span must have an `implements` edge to a matching target.
  Implements(TargetSpec),
}

/// The verdict for one predicate at one match site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PredicateOutcome {
  /// The fact holds; the string cites the edge that proves it.
  Holds(String),
  /// The fact was evaluated and does not hold; the string explains the near-miss (wrong
  /// target, below-floor grade, masked/external reference, no reference at the span).
  Fails(String),
  /// The facts could not be obtained here (file not in the index). Policy decides.
  Unavailable(String),
}

/// Index facts prepared for predicate evaluation: per-path node lists over a borrowed [`Kg`],
/// plus a canonical-path bridge so scanned paths match as-ingested paths regardless of how
/// either side spelled them.
pub struct GraphFacts {
  kg: Kg,
  generation: String,
  /// Stored path → ids of nodes defined in that file (including the File node itself).
  nodes_by_path: HashMap<String, Vec<u32>>,
  /// Canonicalized filesystem path → stored path key (built once; stored paths whose files
  /// no longer exist keep only their stored spelling).
  canonical: HashMap<PathBuf, String>,
}

impl GraphFacts {
  /// Open an index root for predicate evaluation: load the live generation's graph, learn its
  /// content id, and enforce an optional generation pin. Every failure — missing index,
  /// unreadable graph, pin mismatch — is an *unavailability* string for the rule's `require`
  /// policy to act on; there is no partially-available success.
  pub fn open(index_root: &Path, pinned_generation: Option<&str>) -> Result<GraphFacts, String> {
    let generation = match std::fs::read_to_string(index_root.join("CURRENT")) {
      Ok(current) => current.trim().trim_start_matches("gen/").to_string(),
      // Legacy flat index: no generations to pin against.
      Err(_) => String::new(),
    };
    if let Some(pin) = pinned_generation {
      let pin = pin.trim().trim_start_matches("gen/");
      if pin != generation {
        return Err(format!(
          "index {} is at generation {generation:?} but the rule pins {pin:?}",
          index_root.display()
        ));
      }
    }
    let kg = Kg::load(index_root).map_err(|err| {
      format!(
        "cannot load index at {}: {err} (build one with `vorpal index`)",
        index_root.display()
      )
    })?;
    Ok(Self::build(kg, generation))
  }

  /// Group the graph's nodes by defining file and bridge canonical paths. `generation` is
  /// the content id of the generation `kg` was loaded from (empty for legacy flat indexes);
  /// the caller checks any rule pin against it before evaluating.
  pub fn build(kg: Kg, generation: String) -> Self {
    let mut nodes_by_path: HashMap<String, Vec<u32>> = HashMap::new();
    for id in 0..kg.node_count() as u64 {
      if let Some(view) = kg.node(NodeId::new(id)) {
        if let Some(ids) = nodes_by_path.get_mut(view.path) {
          ids.push(id as u32);
        } else {
          nodes_by_path.insert(view.path.to_string(), vec![id as u32]);
        }
      }
    }
    let mut canonical = HashMap::new();
    for stored in nodes_by_path.keys() {
      if let Ok(canon) = std::fs::canonicalize(stored) {
        canonical.insert(canon, stored.clone());
      }
    }
    Self {
      kg,
      generation,
      nodes_by_path,
      canonical,
    }
  }

  /// The generation content id these facts came from.
  pub fn generation(&self) -> &str {
    &self.generation
  }

  /// Evaluate one predicate anchored at `span` (byte offsets) in `file`. `floor` is the
  /// minimum packed confidence an edge must carry (see `min_confidence_for_grade`).
  pub fn evaluate(
    &self,
    file: &Path,
    span: (u32, u32),
    kind: &PredicateKind,
    floor: u8,
  ) -> PredicateOutcome {
    let Some(key) = self.stored_key(file) else {
      return PredicateOutcome::Unavailable(format!(
        "file {} is not part of the index",
        file.display()
      ));
    };
    match kind {
      PredicateKind::ResolvesTo(spec) => match self.resolved_target(key, span, floor) {
        Ok((row, target)) => {
          if self.target_matches(target, spec) {
            PredicateOutcome::Holds(self.cite(&row, target))
          } else {
            PredicateOutcome::Fails(format!(
              "resolves to {} — not the selected target ({})",
              self.name_of(target),
              spec.describe()
            ))
          }
        }
        Err(fail) => PredicateOutcome::Fails(fail),
      },
      PredicateKind::SameBindingAs { other } => {
        let first = self.resolved_target(key, span, floor);
        let second = self.resolved_target(key, *other, floor);
        match (first, second) {
          (Ok((row_a, a)), Ok((_, b))) => {
            if a == b {
              PredicateOutcome::Holds(format!("both bind {}", self.cite(&row_a, a)))
            } else {
              PredicateOutcome::Fails(format!(
                "bindings differ: {} vs {}",
                self.name_of(a),
                self.name_of(b)
              ))
            }
          }
          (Err(fail), _) | (_, Err(fail)) => PredicateOutcome::Fails(fail),
        }
      }
      PredicateKind::Calls(spec) => self.edge_from_enclosing(key, span, EdgeType::CALLS, spec, floor),
      PredicateKind::Implements(spec) => {
        self.edge_from_enclosing(key, span, EdgeType::IMPLEMENTS, spec, floor)
      }
      PredicateKind::Imports(spec) => {
        let Some(file_node) = self.file_node(key) else {
          return PredicateOutcome::Unavailable(format!("no file node indexed for {key}"));
        };
        self.edge_from(file_node, EdgeType::IMPORTS, spec, floor, key)
      }
    }
  }

  fn stored_key(&self, file: &Path) -> Option<&str> {
    if let Some((stored, _)) = file.to_str().and_then(|s| self.nodes_by_path.get_key_value(s)) {
      return Some(stored.as_str());
    }
    let canon = std::fs::canonicalize(file).ok()?;
    self.canonical.get(&canon).map(String::as_str)
  }

  fn name_of(&self, id: u32) -> String {
    match self.kg.node(NodeId::new(id as u64)) {
      Some(view) => format!("`{}` ({})", view.name, view.path),
      None => format!("<missing node {id}>"),
    }
  }

  fn cite(&self, row: &EvidenceRow, target: u32) -> String {
    format!(
      "{} [{}; {}]",
      self.name_of(target),
      Confidence(row.confidence).grade().label(),
      ResolveReason::from_tag(row.reason).label()
    )
  }

  fn target_matches(&self, id: u32, spec: &TargetSpec) -> bool {
    let Some(view) = self.kg.node(NodeId::new(id as u64)) else {
      return false;
    };
    if let Some(name) = &spec.name {
      if view.name != name {
        return false;
      }
    }
    if let Some(suffix) = &spec.path_suffix {
      if !view.path.ends_with(suffix.as_str()) {
        return false;
      }
    }
    if let Some(eid) = spec.external_id {
      if view.external_id != Some(eid) {
        return false;
      }
    }
    true
  }

  /// The evidence row whose recorded reference span most tightly covers `span`, across every
  /// node defined in `key`'s file. Deterministic: smallest width, then lowest from-id, then
  /// earliest start.
  fn covering_row(&self, key: &str, span: (u32, u32)) -> Option<EvidenceRow> {
    let ids = self.nodes_by_path.get(key)?;
    let mut best: Option<EvidenceRow> = None;
    for &id in ids {
      for row in self.kg.evidence_from(NodeId::new(id as u64)) {
        if row.span_start <= span.0 && span.1 <= row.span_end {
          let better = match &best {
            None => true,
            Some(current) => {
              let width = row.span_end - row.span_start;
              let current_width = current.span_end - current.span_start;
              (width, row.from, row.span_start) < (current_width, current.from, current.span_start)
            }
          };
          if better {
            best = Some(row);
          }
        }
      }
    }
    best
  }

  /// The edge target the reference at `span` resolved to, or the audit reason there is none.
  fn resolved_target(
    &self,
    key: &str,
    span: (u32, u32),
    floor: u8,
  ) -> Result<(EvidenceRow, u32), String> {
    let Some(row) = self.covering_row(key, span) else {
      return Err(format!(
        "no reference recorded at bytes {}..{} — nothing to prove against",
        span.0, span.1
      ));
    };
    match row.outcome {
      EvidenceOutcome::Edge => {
        if row.confidence < floor {
          Err(format!(
            "resolution is {} ({}; {} candidates) — below the required grade",
            Confidence(row.confidence).grade().label(),
            ResolveReason::from_tag(row.reason).label(),
            row.candidates
          ))
        } else {
          let to = row.to;
          Ok((row, to))
        }
      }
      EvidenceOutcome::External => Err(format!(
        "reference is external (no definition in the corpus; {} candidates)",
        row.candidates
      )),
      EvidenceOutcome::Masked => Err(format!(
        "reference is masked (visibility/qualifier refused every candidate; {} candidates)",
        row.candidates
      )),
    }
  }

  /// The smallest definition whose recorded span covers `span` in `key`'s file.
  fn enclosing_definition(&self, key: &str, span: (u32, u32)) -> Option<u32> {
    let ids = self.nodes_by_path.get(key)?;
    let mut best: Option<(u32, u32)> = None; // (width, id)
    for &id in ids {
      let Some(view) = self.kg.node(NodeId::new(id as u64)) else {
        continue;
      };
      let (start, end) = view.span;
      if (start, end) == (0, 0) || view.kind == SymbolKind::File {
        continue;
      }
      if start <= span.0 && span.1 <= end {
        let width = end - start;
        if best.is_none_or(|(w, _)| width < w) {
          best = Some((width, id));
        }
      }
    }
    best.map(|(_, id)| id)
  }

  fn file_node(&self, key: &str) -> Option<u32> {
    self.nodes_by_path.get(key)?.iter().copied().find(|&id| {
      self
        .kg
        .node(NodeId::new(id as u64))
        .is_some_and(|view| view.kind == SymbolKind::File)
    })
  }

  fn edge_from_enclosing(
    &self,
    key: &str,
    span: (u32, u32),
    etype: EdgeType,
    spec: &TargetSpec,
    floor: u8,
  ) -> PredicateOutcome {
    let Some(from) = self.enclosing_definition(key, span) else {
      return PredicateOutcome::Fails(format!(
        "no definition encloses bytes {}..{} in {key}",
        span.0, span.1
      ));
    };
    self.edge_from(from, etype, spec, floor, key)
  }

  /// Does `from` carry an `etype` edge (at or above `floor`) to a target matching `spec`?
  /// Cites the proving edge; on failure reports the closest near-miss (a matching target
  /// below the floor beats "none at all" in the audit trail).
  fn edge_from(
    &self,
    from: u32,
    etype: EdgeType,
    spec: &TargetSpec,
    floor: u8,
    key: &str,
  ) -> PredicateOutcome {
    let mut below_floor: Option<EvidenceRow> = None;
    for row in self.kg.evidence_from(NodeId::new(from as u64)) {
      if EdgeType(row.etype) != etype || row.outcome != EvidenceOutcome::Edge {
        continue;
      }
      if !self.target_matches(row.to, spec) {
        continue;
      }
      if row.confidence >= floor {
        return PredicateOutcome::Holds(format!(
          "{} —{}→ {}",
          self.name_of(from),
          etype.name(),
          self.cite(&row, row.to)
        ));
      }
      if below_floor.is_none() {
        below_floor = Some(row);
      }
    }
    match below_floor {
      Some(row) => PredicateOutcome::Fails(format!(
        "only a below-floor candidate: {} —{}→ {}",
        self.name_of(from),
        etype.name(),
        self.cite(&row, row.to)
      )),
      None => PredicateOutcome::Fails(format!(
        "no {} edge from {} matches {} (searched {key})",
        etype.name(),
        self.name_of(from),
        spec.describe()
      )),
    }
  }
}

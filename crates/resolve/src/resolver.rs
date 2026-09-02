//! The resolution algorithm (§3.3): scope-aware, confidence-scored, never faking edges.

use vorpal_kg::{EdgeType, NodeId, SymbolKind};

use crate::intern::{Interner, NameId};
use crate::reach::IncludeReach;
use crate::reference::{RefForm, RefKind, Reference};
use crate::table::{Symbol, SymbolTable};

/// How sure a resolution is (0–100). Ordered so `<= AMBIGUOUS` flags approximate edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Confidence(pub u8);

impl Confidence {
  /// No definition is visible — unresolved.
  pub const NONE: Confidence = Confidence(0);
  /// Multiple candidates; the edge is approximate (labeled, not faked).
  pub const AMBIGUOUS: Confidence = Confidence(40);
  /// Typed-receiver resolution (G-M2): the candidate set was narrowed by the receiver's
  /// inferred type — stronger than a bare-name pick, weaker than an explicit local binding.
  pub const TYPE_BOUND: Confidence = Confidence(85);
  /// A single visible exported definition in another file.
  pub const CROSS_FILE: Confidence = Confidence(90);
  /// A single definition in the same file — the strongest binding.
  pub const LOCAL: Confidence = Confidence(100);

  /// The categorical [`ResolutionGrade`] of this confidence.
  pub fn grade(self) -> ResolutionGrade {
    ResolutionGrade::from_confidence(self)
  }
}

/// The explicit, ordinal grade of a resolution — the categorical view of [`Confidence`] a
/// consumer branches on to tell a proven binding from a best guess, without interpreting a raw
/// score. This is the single grade vocabulary shared by the CLI, MCP, and bindings (§3.3, §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResolutionGrade {
  /// No definition was visible: no edge (or a name-only membership).
  Unresolved,
  /// Several definitions carried the name; the target is a deterministic best guess, not a bound
  /// reference — approximate, and labeled as such.
  Heuristic,
  /// A single visible exported definition in another file: constrained to one candidate, but not
  /// a lexical binding (no import/scope proof).
  Constrained,
  /// A single definition in the same file: an exact lexical binding.
  Exact,
}

impl ResolutionGrade {
  /// Map a confidence score to its grade using the [`Confidence`] tier constants.
  pub fn from_confidence(c: Confidence) -> Self {
    if c >= Confidence::LOCAL {
      Self::Exact
    } else if c >= Confidence::TYPE_BOUND {
      // The constrained floor moved 90 → 85 when TYPE_BOUND landed (G-M0). No existing edge
      // lives in (40, 90), so no historical label shifts.
      Self::Constrained
    } else if c > Confidence::NONE {
      Self::Heuristic
    } else {
      Self::Unresolved
    }
  }

  /// A stable lowercase label for output and machine consumers.
  pub fn label(self) -> &'static str {
    match self {
      Self::Exact => "exact",
      Self::Constrained => "constrained",
      Self::Heuristic => "heuristic",
      Self::Unresolved => "unresolved",
    }
  }
}

/// *Why* a resolution chose its target — the exact resolver branch that produced the edge,
/// persisted per edge occurrence so every relation can answer "why does this exist?" (§5).
/// Stored on disk as its `u8` tag; unknown future tags render as `unknown`, never fail.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveReason {
  /// No edge was produced (unresolved/masked) — never persisted.
  None = 0,
  /// A path-form import matched an indexed file node exactly.
  ImportPath = 1,
  /// A grammar-provided qualifier corroborated a single candidate.
  Qualified = 2,
  /// A grammar-provided qualifier corroborated several; deterministic tie pick (approximate).
  QualifiedTie = 3,
  /// A single definition in the referencing file (the strongest binding).
  Local = 4,
  /// Several same-file definitions; deterministic tie pick (approximate).
  LocalTie = 5,
  /// A single cross-file definition visible here (exported, or structurally private-visible).
  VisibleExport = 6,
  /// Several visible cross-file candidates; deterministic tie pick (approximate).
  VisibleTie = 7,
  /// A bare name bound through the referencing file's own import: the file's qualified import
  /// of this name resolved to a single corroborated target, and no local definition shadows
  /// it. The strongest cross-file evidence a bare use can carry.
  ImportBound = 8,
  /// Typed-receiver resolution (G-M2): the receiver's type came from an explicit annotation.
  ReceiverAnnotated = 9,
  /// The receiver's type came from a constructor-shaped initializer.
  ReceiverConstructed = 10,
  /// The receiver's type came from a typed parameter binding.
  ReceiverParamTyped = 11,
  /// The receiver's type came from a typed field on the enclosing type.
  ReceiverFieldTyped = 12,
  /// Type narrowing left several candidates; deterministic tie pick (approximate).
  ReceiverTypedTie = 13,
  /// Chained-call typing (G-M5): the receiver was bound from a call whose callee's declared
  /// return type uniquely narrowed the candidates (`let x = make(); x.render()`).
  ReceiverChained = 14,
}

impl ResolveReason {
  pub fn from_tag(tag: u8) -> Self {
    match tag {
      1 => Self::ImportPath,
      2 => Self::Qualified,
      3 => Self::QualifiedTie,
      4 => Self::Local,
      5 => Self::LocalTie,
      6 => Self::VisibleExport,
      7 => Self::VisibleTie,
      8 => Self::ImportBound,
      9 => Self::ReceiverAnnotated,
      10 => Self::ReceiverConstructed,
      11 => Self::ReceiverParamTyped,
      12 => Self::ReceiverFieldTyped,
      13 => Self::ReceiverTypedTie,
      14 => Self::ReceiverChained,
      _ => Self::None,
    }
  }

  /// Stable lowercase label for output and machine consumers.
  pub fn label(self) -> &'static str {
    match self {
      Self::None => "unknown",
      Self::ImportPath => "import-path",
      Self::Qualified => "qualifier-match",
      Self::QualifiedTie => "qualifier-tie",
      Self::Local => "same-file",
      Self::LocalTie => "same-file-tie",
      Self::VisibleExport => "visible-export",
      Self::VisibleTie => "visible-tie",
      Self::ImportBound => "import-bound",
      Self::ReceiverAnnotated => "receiver-annotated",
      Self::ReceiverConstructed => "receiver-constructed",
      Self::ReceiverParamTyped => "receiver-param-typed",
      Self::ReceiverFieldTyped => "receiver-field-typed",
      Self::ReceiverTypedTie => "receiver-typed-tie",
      Self::ReceiverChained => "receiver-chained",
    }
  }
}

/// The chained-call return ledger (G-M5): function name → declared return type, both as
/// interned ids. Built once at link from the per-file capture rows; a name bound to
/// DISAGREEING return types across the corpus is poisoned out (absent) — conservative by
/// design, exactly the receiver-typing discipline.
pub struct ChainReturns<'i> {
  map: std::collections::HashMap<NameId<'i>, Option<NameId<'i>>>,
}

impl<'i> ChainReturns<'i> {
  pub fn build(
    interner: &'i Interner,
    rows: impl IntoIterator<Item = (impl AsRef<str>, impl AsRef<str>)>,
  ) -> Self {
    let mut map: std::collections::HashMap<NameId<'i>, Option<NameId<'i>>> =
      std::collections::HashMap::new();
    for (name, ret) in rows {
      let name = interner.intern(name.as_ref());
      let ret = interner.intern(ret.as_ref());
      map
        .entry(name)
        .and_modify(|slot| {
          if *slot != Some(ret) {
            *slot = None; // disagreement → poisoned, forever
          }
        })
        .or_insert(Some(ret));
    }
    Self { map }
  }

  pub fn is_empty(&self) -> bool {
    self.map.is_empty()
  }

  pub fn get(&self, name: NameId<'i>) -> Option<NameId<'i>> {
    *self.map.get(&name)?
  }
}

/// Cap on retained alternative-candidate identities per occurrence: enough to explain any
/// realistic tie, bounded so a 500-way `init` collision cannot bloat the sidecar.
pub const MAX_RETAINED_ALTERNATIVES: usize = 8;

/// The outcome of resolving one reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
  /// The chosen definition, if any (approximate resolutions still pick a deterministic target).
  pub target: Option<NodeId>,
  pub edge: EdgeType,
  pub confidence: Confidence,
  /// How many definitions carried the referenced name (for transparency).
  pub candidates: usize,
  /// The resolver branch that produced (or declined) the target.
  pub reason: ResolveReason,
  /// The alternatives the chosen target beat *in the final set* (tie picks retain the tie
  /// set minus the target, capped at [`MAX_RETAINED_ALTERNATIVES`]); unique picks have none —
  /// their eliminations are already explained by `reason`. `(ids, count)`.
  pub alternatives: ([u32; MAX_RETAINED_ALTERNATIVES], u8),
}

/// A resolved reference ready to become a graph edge, carrying its evidence: the source span
/// of the referencing occurrence, the resolver branch that bound it, and how many candidates
/// carried the name — everything the persisted evidence sidecar retains per edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedEdge {
  pub from: NodeId,
  pub to: NodeId,
  pub edge: EdgeType,
  pub confidence: u8,
  /// Byte span of the referencing occurrence within `from`'s file.
  pub span: (u32, u32),
  pub reason: ResolveReason,
  /// Candidate count at resolve time (saturated to `u32`).
  pub candidates: u32,
  /// Low 32 bits of xxh3 of the referenced name — the absence-query key, carried on edges too.
  pub name_hash: u32,
  /// Retained final-set alternatives (tie picks), `(ids, count)` — see [`Resolution`].
  pub alternatives: ([u32; MAX_RETAINED_ALTERNATIVES], u8),
  /// Interned bits of the referencing file's path — the retained daemon's bucketing key
  /// (per-file resolution buckets; scoped rederive). Process-private, like every NameId.
  pub from_path_bits: u32,
  /// Interned bits of the referenced name — the scoped-rederive postings key.
  pub name_bits: u32,
}

/// A reference that produced **no** edge, retained as evidence (IMPROVEMENTS 07-29 §4): the
/// honest-resolution story completed — "why is there no edge here?" is answerable from the
/// sidecar instead of only aggregate counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnresolvedEvidence {
  pub from: NodeId,
  /// Low 32 bits of xxh3 of the referenced name (the name itself lives in source, not here).
  pub name_hash: u32,
  /// The edge type this reference *would* have produced.
  pub etype: EdgeType,
  pub span: (u32, u32),
  pub candidates: u32,
  /// `true` = external (no definition anywhere in the tree); `false` = masked (definitions
  /// exist but none is safely attributable).
  pub external: bool,
  /// Interned bits of the referencing file's path (see [`ResolvedEdge::from_path_bits`]).
  pub from_path_bits: u32,
  /// Interned bits of the referenced name (see [`ResolvedEdge::name_bits`]).
  pub name_bits: u32,
}

/// Counts from a resolution batch. `external + masked` is the total left without an edge.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ResolveStats {
  /// Confidently resolved (single visible definition).
  pub resolved: u64,
  /// Resolved but approximate (multiple candidates; labeled edge).
  pub ambiguous: u64,
  /// No definition with this name exists in the indexed tree — the reference targets the
  /// standard library or a dependency outside the corpus. Honest, expected, and not a defect.
  pub external: u64,
  /// Definitions with the name exist, but none is safely attributable: not visible from the
  /// use site, or the reference is a member access whose name evidence is non-unique.
  pub masked: u64,
}

impl ResolveStats {
  /// References that produced no edge (external + masked).
  pub fn unresolved(&self) -> u64 {
    self.external + self.masked
  }
}

impl ResolveStats {
  /// Every reference this stats block accounts for, across all four outcomes.
  pub fn total(&self) -> u64 {
    self.resolved + self.ambiguous + self.external + self.masked
  }
}

impl std::ops::AddAssign for ResolveStats {
  fn add_assign(&mut self, other: ResolveStats) {
    self.resolved += other.resolved;
    self.ambiguous += other.ambiguous;
    self.external += other.external;
    self.masked += other.masked;
  }
}

/// The reference resolver. Stateless today; a home for future config (per-language visibility).
#[derive(Debug, Default, Clone, Copy)]
pub struct Resolver;

impl Resolver {
  pub fn new() -> Self {
    Self
  }

  /// Resolve one reference against the table (§3.3):
  /// 1. path-form imports resolve against indexed file nodes (exact path match — cannot fake);
  /// 2. a grammar-provided qualifier (`Kg::load`, `self.helper()`) restricts candidates to
  ///    members of that owner (or, for static paths, definitions in the module of that name) —
  ///    and when the qualifier matches nothing, [`RefForm::Static`]/[`RefForm::Method`]
  ///    references never fall back to a blind multi-candidate guess;
  /// 3. same-file definitions win (local binding);
  /// 4. across files, exported definitions are visible — plus, for Rust, private definitions
  ///    in an ancestor module file (child modules see parent privates);
  /// 5. a single visible definition binds; multiple bind approximately only where the form
  ///    tolerates it (bare names); none is unresolved.
  pub fn resolve<'i>(
    &self,
    interner: &'i Interner,
    table: &SymbolTable<'i>,
    reference: &Reference<'i>,
    reach: Option<&IncludeReach<'i>>,
  ) -> Resolution {
    self.resolve_with(
      interner,
      table,
      reference,
      &mut ResolveScratch::default(),
      None,
      reach,
    )
  }

  /// [`Resolver::resolve`] with caller-owned scratch buffers — batch resolution reuses one
  /// scratch across a whole chunk instead of allocating fresh candidate vectors per
  /// reference (~4 allocations × millions of references otherwise).
  fn resolve_with<'i>(
    &self,
    interner: &'i Interner,
    table: &SymbolTable<'i>,
    reference: &Reference<'i>,
    scratch: &mut ResolveScratch<'i>,
    chain: Option<&ChainReturns<'i>>,
    reach: Option<&IncludeReach<'i>>,
  ) -> Resolution {
    let ResolveScratch {
      reachable,
      refined,
      local,
      visible,
      path_buf,
    } = scratch;
    let edge = reference.kind.edge();
    if reference.kind == RefKind::Import {
      if let Some((target, _)) = resolve_import_path(interner, table, reference, path_buf) {
        return Resolution {
          target: Some(target),
          edge,
          confidence: Confidence::CROSS_FILE,
          candidates: 1,
          reason: ResolveReason::ImportPath,
          alternatives: ([0; MAX_RETAINED_ALTERNATIVES], 0),
        };
      }
    }
    let unfiltered = table.candidates(reference.name);
    // The candidate law's macro gate (see `SymbolKind::is_resolution_candidate`):
    // macros bind by INCLUSION, not name-globality — a macro candidate survives
    // only when its defining file is the reference's own file or include-reachable
    // from it. Same-named per-arch/vendored duplicates thereby resolve to exactly
    // the copy the includer reaches instead of minting global ambiguity.
    let candidates: &[Symbol<'i>] =
      if unfiltered.iter().any(|s| s.kind == SymbolKind::Macro) {
        reachable.clear();
        reachable.extend(unfiltered.iter().copied().filter(|s| {
          s.kind != SymbolKind::Macro
            || s.path == reference.from_path
            || reach.is_some_and(|r| r.reaches(reference.from_path, s.path))
        }));
        if reachable.is_empty() {
          // Macro definitions exist but none is include-visible here — the same
          // masked outcome as private-to-other-files, never a fake binding.
          return Resolution {
            target: None,
            edge,
            confidence: Confidence::NONE,
            candidates: unfiltered.len(),
            reason: ResolveReason::None,
            alternatives: ([0; MAX_RETAINED_ALTERNATIVES], 0),
          };
        }
        reachable
      } else {
        unfiltered
      };
    // A bare name in a file whose own import provably bound it (the qualified import
    // resolved to a single corroborated target — ties never seed bindings, so ambiguity
    // cannot launder through here) resolves to that import's target, unless a local
    // definition shadows the import. This check precedes the empty-candidates return
    // because an ALIASED import's local name has no candidates of its own (`from x import
    // y as z`: nothing anywhere defines `z`), yet `z()` is exactly the call the binding
    // proves. Confidence stays cross-file: the use inherits the import's certainty.
    if reference.form == RefForm::Bare {
      if let Some(target) = table.import_binding(reference.from_path, reference.name) {
        if !candidates.iter().any(|s| s.path == reference.from_path) {
          let candidates = candidates.len();
          return Resolution {
            target: Some(target),
            edge,
            confidence: Confidence::CROSS_FILE,
            candidates,
            reason: ResolveReason::ImportBound,
            alternatives: ([0; MAX_RETAINED_ALTERNATIVES], 0),
          };
        }
      }
    }
    if candidates.is_empty() {
      return Resolution {
        target: None,
        edge,
        confidence: Confidence::NONE,
        candidates: 0,
        reason: ResolveReason::None,
        alternatives: ([0; MAX_RETAINED_ALTERNATIVES], 0),
      };
    }

    if let Some(qualifier) = reference.qualifier {
      refined.clear();
      refined.extend(
        candidates
          .iter()
          .filter(|s| qualifier_matches(interner, s, qualifier, reference.form))
          .copied(),
      );
      if !refined.is_empty() {
        // The qualifier corroborates these candidates; among them, a multi-way tie (e.g. two
        // `impl Kg` blocks defining `load`) is genuine ambiguity — labeled, tolerated.
        return finish(
          interner,
          refined,
          reference,
          edge,
          candidates.len(),
          true,
          true,
          local,
          visible,
        );
      }
      if reference.form == RefForm::MethodHinted {
        // The receiver text corroborated no owner — it was an opaque value name after all.
        // Fall through to plain Method semantics below: the hint may upgrade a resolution,
        // never veto one.
      } else if reference.form != RefForm::Bare {
        // The grammar names an owner/namespace and nothing in the tree matches it: the target
        // is outside the corpus (e.g. `Vec::new`). Falling back to bare-name candidates would
        // fake an edge to a coincidentally-named definition.
        return Resolution {
          target: None,
          edge,
          confidence: Confidence::NONE,
          candidates: candidates.len(),
          reason: ResolveReason::None,
          alternatives: ([0; MAX_RETAINED_ALTERNATIVES], 0),
        };
      }
    }

    // Typed-receiver narrowing (G-M2): the receiver's file-locally bound type refines the
    // candidate set by OWNER. A hint that upgrades, never vetoes: a type that matches no
    // in-tree owner (external type, spelling drift) falls through to untyped semantics.
    if matches!(reference.form, RefForm::Method | RefForm::MethodHinted) {
      if let Some(receiver_type) = reference.receiver_type {
        refined.clear();
        refined.extend(
          candidates
            .iter()
            .filter(|s| s.owner == Some(receiver_type))
            .copied(),
        );
        match refined.len() {
          0 => {
            // Chained-call fallback (G-M5): the "type" may really be a CALLEE NAME
            // (`let x = make()` records `make`). If the corpus-wide return ledger maps it
            // to exactly one return type that uniquely narrows, that is the edge; any
            // ambiguity refuses (inference on inference earns no tie picks).
            if let Some(ret) = chain.and_then(|c| c.get(receiver_type)) {
              refined.extend(
                candidates.iter().filter(|s| s.owner == Some(ret)).copied(),
              );
              if refined.len() == 1 {
                let target = refined[0];
                return Resolution {
                  target: Some(target.id),
                  edge,
                  confidence: Confidence::TYPE_BOUND,
                  candidates: candidates.len(),
                  reason: ResolveReason::ReceiverChained,
                  alternatives: ([0; MAX_RETAINED_ALTERNATIVES], 0),
                };
              }
              refined.clear();
            }
          }
          1 => {
            let target = refined[0];
            return Resolution {
              target: Some(target.id),
              edge,
              confidence: typed_receiver_confidence(
                interner,
                reference,
                target.path == reference.from_path,
              ),
              candidates: candidates.len(),
              reason: typed_receiver_reason(reference.receiver_type_origin),
              alternatives: ([0; MAX_RETAINED_ALTERNATIVES], 0),
            };
          }
          _ => {
            // Several methods on the SAME type (e.g. duplicate impl blocks): genuinely
            // ambiguous, but the type narrowed the field — a labeled deterministic pick
            // with the beaten set retained, exactly the QualifiedTie discipline.
            let target = refined.iter().min_by_key(|s| s.id.raw()).map(|s| s.id);
            let mut alts = [0u32; MAX_RETAINED_ALTERNATIVES];
            let mut alt_count = 0u8;
            for symbol in refined.iter() {
              if Some(symbol.id) == target || (alt_count as usize) >= MAX_RETAINED_ALTERNATIVES {
                continue;
              }
              alts[alt_count as usize] = symbol.id.raw() as u32;
              alt_count += 1;
            }
            return Resolution {
              target,
              edge,
              confidence: Confidence::AMBIGUOUS,
              candidates: candidates.len(),
              reason: ResolveReason::ReceiverTypedTie,
              alternatives: (alts, alt_count),
            };
          }
        }
      }
    }

    // Bare names may take a labeled approximate pick on a tie; member accesses on untyped
    // values (hinted or not) carry no proof beyond the name, so only a unique match binds.
    let guess_on_tie = !matches!(reference.form, RefForm::Method | RefForm::MethodHinted);
    finish(
      interner,
      candidates,
      reference,
      edge,
      candidates.len(),
      guess_on_tie,
      false,
      local,
      visible,
    )
  }
}

/// Reusable candidate buffers for [`Resolver::resolve_with`] — cleared per reference, sized
/// by the largest candidate set a chunk encounters.
#[derive(Default)]
struct ResolveScratch<'i> {
  /// Include-gated candidate list (the macro gate's output).
  reachable: Vec<Symbol<'i>>,
  refined: Vec<Symbol<'i>>,
  local: Vec<Symbol<'i>>,
  visible: Vec<Symbol<'i>>,
  /// Reused path-probe buffer for [`resolve_import_path`]'s join/extension
  /// probes — one allocation per chunk lifetime instead of two-plus per import.
  path_buf: String,
}

/// Shared tail of resolution: local-first, then cross-file visibility, then pick.
#[allow(clippy::too_many_arguments)] // resolution kernel: scratch buffers ride as args by design
fn finish<'i>(
  interner: &'i Interner,
  set: &[Symbol<'i>],
  reference: &Reference<'i>,
  edge: EdgeType,
  candidates: usize,
  guess_on_tie: bool,
  via_qualifier: bool,
  local: &mut Vec<Symbol<'i>>,
  visible: &mut Vec<Symbol<'i>>,
) -> Resolution {
  local.clear();
  local.extend(
    set
      .iter()
      .filter(|s| s.path == reference.from_path)
      .copied(),
  );
  if !local.is_empty() {
    return pick(local, edge, Confidence::LOCAL, candidates, guess_on_tie, via_qualifier);
  }

  // Resolve the reference's path text once per reference, not once per candidate.
  let from_path_text = interner.text_of(reference.from_path);
  visible.clear();
  visible.extend(
    set
      .iter()
      .filter(|s| s.exported || privately_visible(interner.text_of(s.path), from_path_text))
      .copied(),
  );
  if visible.is_empty() {
    // Definitions exist, but all are private to other files → not visible here.
    return Resolution {
      target: None,
      edge,
      confidence: Confidence::NONE,
      candidates,
      reason: ResolveReason::None,
      alternatives: ([0; MAX_RETAINED_ALTERNATIVES], 0),
    };
  }
  pick(
    visible,
    edge,
    Confidence::CROSS_FILE,
    candidates,
    guess_on_tie,
    via_qualifier,
  )
}

/// Whether `symbol` is a plausible target for qualifier `q`: its containing definition carries
/// that name, or — for static paths only — it is a top-level definition in a module file named
/// `q` (`util::helper` → `…/util.rs`). Method receivers never module-match: a variable that
/// happens to share a file's name is coincidence, not namespace evidence.
/// The confidence a typed-receiver unique match earns. An explicit annotation in a
/// statically-checked language is compiler-grade evidence — local/cross-file strength; a
/// constructor-inferred, param-typed, or field-typed binding — and ANY binding in Python,
/// whose annotations are unenforced hints — caps at `TYPE_BOUND`.
fn typed_receiver_confidence<'i>(
  interner: &'i Interner,
  reference: &Reference<'i>,
  local: bool,
) -> Confidence {
  let annotated = reference.receiver_type_origin == 0; // typefacts BindOrigin::Annotated
  let path = interner.text_of(reference.from_path);
  let python = path.ends_with(".py") || path.ends_with(".pyi");
  if annotated && !python {
    if local { Confidence::LOCAL } else { Confidence::CROSS_FILE }
  } else {
    Confidence::TYPE_BOUND
  }
}

/// The reason tag for a typed-receiver unique match, by binding origin.
fn typed_receiver_reason(origin: u8) -> ResolveReason {
  match origin {
    0 => ResolveReason::ReceiverAnnotated,
    1 => ResolveReason::ReceiverConstructed,
    2 => ResolveReason::ReceiverParamTyped,
    3 => ResolveReason::ReceiverFieldTyped,
    _ => ResolveReason::ReceiverAnnotated,
  }
}

fn qualifier_matches<'i>(
  interner: &'i Interner,
  symbol: &Symbol<'i>,
  q: NameId<'i>,
  form: RefForm,
) -> bool {
  match symbol.owner {
    Some(owner) => owner == q,
    None => {
      form == RefForm::Static
        && module_stem_matches(interner.text_of(symbol.path), interner.text_of(q))
    }
  }
}

/// Whether `path` is the file of module `q`: its stem is `q`, or it is a directory-carrier file
/// (`mod.rs` / `lib.rs` / `main.rs` / `index.*` / `__init__.py`) inside a directory named `q`.
fn module_stem_matches(path: &str, q: &str) -> bool {
  let file = path.rsplit('/').next().unwrap_or(path);
  let stem = file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file);
  if stem == q {
    return true;
  }
  matches!(stem, "mod" | "lib" | "main" | "index" | "__init__")
    && parent_dir(path).rsplit('/').next() == Some(q)
}

/// Language-structural visibility for non-exported definitions — only where the language's
/// privacy domain is derivable from the file tree:
///
/// - **Rust**: a private item is visible to its module and all descendant modules, and the
///   module tree mirrors the file tree — `dir/name.rs` owns `dir/name/**`, and the carrier
///   files `mod.rs`/`lib.rs`/`main.rs` own their directory's subtree.
/// - **Java**: package-private means visible throughout the package, and a package is a
///   directory — same-directory `.java` files see each other's package-private definitions.
///
/// Everything else stays file-local: JS/TS module-locals genuinely end at the file, C `static`
/// is translation-unit-local, and module/assembly-scoped visibilities (Kotlin `internal`,
/// C# `internal`, Swift `internal`) are not derivable from paths — masking those is the
/// conservative, honest outcome.
fn privately_visible(def_path: &str, from_path: &str) -> bool {
  if let Some(stem_path) = def_path.strip_suffix(".rs") {
    if !from_path.ends_with(".rs") {
      return false;
    }
    let subtree_root = match stem_path.rsplit('/').next().unwrap_or(stem_path) {
      "mod" | "lib" | "main" => parent_dir(def_path),
      _ => stem_path,
    };
    return !subtree_root.is_empty()
      && from_path
        .strip_prefix(subtree_root)
        .is_some_and(|rest| rest.starts_with('/'));
  }
  if def_path.ends_with(".java") && from_path.ends_with(".java") {
    return parent_dir(def_path) == parent_dir(from_path);
  }
  false
}

/// Path-form import resolution: `./util` imported from `src/a.ts` tries the indexed file nodes
/// at `src/util` and `src/util.ts` (the importer's own extension). Exact matches only — a miss
/// falls through to symbol-name resolution and, failing that, stays honestly unresolved.
///
/// Only path-shaped names (containing `/` or `.`) are attempted: a bare symbol import like
/// Java's `Helper` must not be hijacked to a coincidentally-named `Helper.java` file node when
/// precise symbol resolution is available.
fn resolve_import_path<'i>(
  interner: &'i Interner,
  table: &SymbolTable<'i>,
  reference: &Reference<'i>,
  buf: &mut String,
) -> Option<(NodeId, NameId<'i>)> {
  let name = interner.text_of(reference.name);
  if !name.contains(['/', '.']) {
    return None;
  }
  if let Some(id) = table.file(interner, name) {
    return Some((id, reference.name));
  }
  let from_path = interner.text_of(reference.from_path);
  join_normalize_into(parent_dir(from_path), name, buf);
  if let Some(id) = table.file(interner, buf) {
    return Some((id, interner.intern(buf)));
  }
  if let Some(ext) = extension(from_path) {
    buf.push('.');
    buf.push_str(ext);
    if let Some(id) = table.file(interner, buf) {
      return Some((id, interner.intern(buf)));
    }
  }
  // Root-relative includes (`#include <linux/export.h>`, absolute-style module paths):
  // the exact and importer-relative probes cannot see them, so fall through to the
  // suffix map with nearest-prefix + corpus-support disambiguation (the `-I` set
  // inferred from the corpus itself — see `SymbolTable::file_by_suffix`).
  table.file_by_suffix(interner, name, reference.from_path)
}

/// Build the include-reachability oracle from the import references: every
/// path-form import that resolves to an indexed file contributes a file→file
/// edge. Language-agnostic — C/C++ `#include`, JS/TS path imports, anything the
/// path resolver matches; symbol-form imports contribute nothing here.
pub fn build_include_reach<'i>(
  interner: &'i Interner,
  table: &SymbolTable<'i>,
  imports: &[Reference<'i>],
) -> IncludeReach<'i> {
  IncludeReach::from_edges(&include_edges(interner, table, imports))
}

/// The resolved `(includer, included)` file edges behind [`build_include_reach`] —
/// exposed so full builds can persist the graph (`reach.bin`) for scoped composes.
pub fn include_edges<'i>(
  interner: &'i Interner,
  table: &SymbolTable<'i>,
  imports: &[Reference<'i>],
) -> Vec<(NameId<'i>, NameId<'i>)> {
  // Edge collection resolves every path-form import — embarrassingly parallel
  // (the ledger measured this loop 0.27 s single-threaded at kernel scale).
  // `from_edges` is order-invariant (pinned by test), so chunked collection
  // changes nothing about the oracle.
  use rayon::prelude::*;
  let threads = rayon::current_num_threads().max(1);
  let chunk = imports.len().div_ceil(threads * 2).max(1);
  let edges: Vec<(NameId<'i>, NameId<'i>)> = imports
    .par_chunks(chunk)
    .flat_map_iter(|chunk| {
      let mut buf = String::new();
      chunk.iter().filter_map(move |reference| {
        if reference.kind != RefKind::Import {
          return None;
        }
        resolve_import_path(interner, table, reference, &mut buf)
          .map(|(_, target_path)| (reference.from_path, target_path))
      })
    })
    .collect();
  edges
}

fn parent_dir(path: &str) -> &str {
  path.rfind('/').map(|i| &path[..i]).unwrap_or("")
}

/// Join a relative segment path onto a directory, resolving `.` and `..` textually, into
/// `out` (cleared first). An absolute `rel` ignores `dir`; an absolute `dir` keeps its
/// leading slash through the split/join. Exactly the historical Vec-collect-and-join
/// semantics — including empty segments inherited verbatim from `dir` (`a//b` stays
/// `a//b`; `..` over an empty segment truncates to the previous `/`). The probe paths call
/// this with a reused scratch `String`: the Vec<&str> + join + format chain it replaced
/// allocated two-to-three times per import probe, ~a million times per kernel-scale link.
fn join_normalize_into(dir: &str, rel: &str, out: &mut String) {
  out.clear();
  let absolute = rel.starts_with('/');
  if !absolute && !dir.is_empty() {
    out.push_str(dir);
  }
  for segment in rel.split('/') {
    match segment {
      "" | "." => {}
      ".." => match out.rfind('/') {
        Some(at) => out.truncate(at),
        None => out.clear(),
      },
      other => {
        if !out.is_empty() {
          out.push('/');
        }
        out.push_str(other);
      }
    }
  }
  if absolute {
    out.insert(0, '/');
  }
}

fn extension(path: &str) -> Option<&str> {
  path
    .rsplit('/')
    .next()
    .and_then(|file| file.rsplit_once('.').map(|(_, ext)| ext))
}

/// Choose a target from a visible set: unique → the given confidence; a tie takes a
/// deterministic min-id target at `AMBIGUOUS` when the reference's form tolerates it, and no
/// edge at all when it does not.
/// The deterministic ambiguous pick is the FIRST tied candidate in RUN order. Run order is
/// the canonical BUILD SEQUENCE: the bulk and retained table builders insert path-major in
/// the same canonical walk, the retained splice maintenance preserves it ("runs are
/// path-major by invariant"), and post-build import-binding seeds append in the same
/// canonical seeding sequence on every linker — so run order is identical in every id
/// space, where raw ids are not (the retained writer re-appends an edited file's rows at
/// its tail; a lowest-raw-id pick flipped between spaces — kernel repro:
/// slab_is_available, 41 flipped evidence rows). Filtered sets built by iterating the run
/// keep its order, so `first()` IS the canonical pick, at zero comparisons — this also
/// removed a measured ~1.3s of hub-name tie-break comparisons per kernel link.
fn pick<'i>(
  set: &[Symbol<'i>],
  edge: EdgeType,
  unique: Confidence,
  candidates: usize,
  guess_on_tie: bool,
  via_qualifier: bool,
) -> Resolution {
  if set.len() == 1 {
    // The reason names the branch that bound the target: qualifier corroboration when it
    // narrowed the set, else the visibility tier the unique candidate came from.
    let reason = if via_qualifier {
      ResolveReason::Qualified
    } else if unique >= Confidence::LOCAL {
      ResolveReason::Local
    } else {
      ResolveReason::VisibleExport
    };
    Resolution {
      target: Some(set[0].id),
      edge,
      confidence: unique,
      candidates,
      reason,
      alternatives: ([0; MAX_RETAINED_ALTERNATIVES], 0),
    }
  } else if guess_on_tie {
    let reason = if via_qualifier {
      ResolveReason::QualifiedTie
    } else if unique >= Confidence::LOCAL {
      ResolveReason::LocalTie
    } else {
      ResolveReason::VisibleTie
    };
    let target = set.first().map(|s| s.id);
    // Retain the tie set the pick beat — the alternatives a "why this target?" answer names.
    let mut alts = [0u32; MAX_RETAINED_ALTERNATIVES];
    let mut alt_count = 0u8;
    for symbol in set {
      if Some(symbol.id) == target || (alt_count as usize) >= MAX_RETAINED_ALTERNATIVES {
        continue;
      }
      alts[alt_count as usize] = symbol.id.raw() as u32;
      alt_count += 1;
    }
    Resolution {
      target,
      edge,
      confidence: Confidence::AMBIGUOUS,
      candidates,
      reason,
      alternatives: (alts, alt_count),
    }
  } else {
    Resolution {
      target: None,
      edge,
      confidence: Confidence::NONE,
      candidates,
      reason: ResolveReason::None,
      alternatives: ([0; MAX_RETAINED_ALTERNATIVES], 0),
    }
  }
}

/// Resolve the qualifier-carrying import references and install, per importing file, the
/// bindings they prove: `(file path, imported name) → target node`. Only single-target,
/// constrained-or-better, symbol-form resolutions seed a binding — a tied import proves
/// nothing (so ambiguity can never launder into a binding), and a path-form match targets a
/// *file* node, which a bare name must not inherit. When one file imports the same name twice
/// the later import wins, matching rebinding semantics in the languages that allow it. Runs
/// serially: qualified imports are ~0.1% of references, and the main pass re-resolves them
/// identically for edge emission (bindings never alter `RefForm::Static` resolution).
///
/// Call between [`SymbolTable::finalize`] and the main resolution pass; returns the number of
/// bindings installed.
pub fn seed_import_bindings<'i>(
  interner: &'i Interner,
  table: &mut SymbolTable<'i>,
  qualified_imports: &[Reference<'i>],
  resolver: &Resolver,
) -> usize {
  // Per-import resolution is independent — parallel chunks (the ledger measured
  // this loop 0.28 s single-threaded at kernel scale). The fold below applies
  // results in original import order, so duplicate keys keep exactly the serial
  // last-write-wins outcome.
  use rayon::prelude::*;
  let table_ref: &SymbolTable<'i> = table;
  let threads = rayon::current_num_threads().max(1);
  let chunk_size = qualified_imports.len().div_ceil(threads * 2).max(1);
  let resolved: Vec<Vec<((NameId<'i>, NameId<'i>), NodeId)>> = qualified_imports
    .par_chunks(chunk_size)
    .map(|chunk| {
      let mut scratch = ResolveScratch::default();
      let mut out = Vec::new();
      for reference in chunk {
        let resolution =
          resolver.resolve_with(interner, table_ref, reference, &mut scratch, None, None);
        let Some(target) = resolution.target else {
          continue;
        };
        if resolution.confidence < Confidence::CROSS_FILE {
          continue;
        }
        if !matches!(
          resolution.reason,
          ResolveReason::Qualified | ResolveReason::Local | ResolveReason::VisibleExport
        ) {
          continue;
        }
        // An aliased import rebinds under its LOCAL name — that is what bare uses in the
        // file say, so the binding keys on the alias when one exists.
        let bound_name = reference.alias.unwrap_or(reference.name);
        out.push(((reference.from_path, bound_name), target));
      }
      out
    })
    .collect();
  let mut bindings: rustc_hash::FxHashMap<(NameId<'i>, NameId<'i>), NodeId> =
    rustc_hash::FxHashMap::default();
  for chunk in resolved {
    for (key, target) in chunk {
      bindings.insert(key, target);
    }
  }
  let count = bindings.len();
  table.set_import_bindings(bindings);
  count
}

/// Below this many references the fan-out overhead outweighs the win and the batch resolves
/// serially.
const MIN_REFS_PER_SHARD: usize = 4096;

/// Resolve a batch of references, emitting a labeled edge per resolvable reference.
///
/// Large batches shard across threads (§7.5): resolution of one reference is a pure read of
/// the immutable table, so contiguous chunks resolve independently and their edge lists
/// concatenate in chunk order — the output is identical to the serial loop, edge for edge, in
/// order (pinned by test).
pub fn resolve_all<'i>(
  interner: &'i Interner,
  table: &SymbolTable<'i>,
  references: &[Reference<'i>],
  resolver: &Resolver,
  reach: Option<&IncludeReach<'i>>,
) -> (Vec<ResolvedEdge>, ResolveStats) {
  if references.len() <= MIN_REFS_PER_SHARD {
    let (edges, _unresolved, stats) =
      resolve_chunk(interner, table, references, resolver, None, reach);
    return (edges, stats);
  }
  use rayon::prelude::*;
  let threads = rayon::current_num_threads().max(1);
  let chunk_size = references
    .len()
    .div_ceil(threads * 2)
    .max(MIN_REFS_PER_SHARD);
  let shards: Vec<(Vec<ResolvedEdge>, Vec<UnresolvedEvidence>, ResolveStats)> = references
    .par_chunks(chunk_size)
    .map(|chunk| resolve_chunk(interner, table, chunk, resolver, None, reach))
    .collect();
  // Reserve what actually resolved, not one slot per reference — at kernel scale roughly
  // half of all references yield edges, and the difference is ~80 MB of dead reservation.
  let total: usize = shards.iter().map(|(edges, _, _)| edges.len()).sum();
  let mut edges = Vec::with_capacity(total);
  let mut stats = ResolveStats::default();
  for (shard_edges, _unresolved, shard_stats) in shards {
    edges.extend(shard_edges);
    stats += shard_stats;
  }
  (edges, stats)
}

/// [`resolve_all`] over a [`crate::RefSpill`] instead of an in-RAM slice: chunks stream off
/// disk through a bounded channel into a worker pool, and edge lists concatenate in chunk
/// order — output identical to `resolve_all` on the same references (chunking is invisible:
/// resolution is a pure per-reference read of the immutable table). In-flight memory is a
/// few chunks, not the whole reference stream.
pub fn resolve_all_spilled<'i>(
  interner: &'i Interner,
  table: &SymbolTable<'i>,
  spill: &crate::RefSpill<'i>,
  resolver: &Resolver,
  reach: Option<&IncludeReach<'i>>,
) -> std::io::Result<(Vec<ResolvedEdge>, ResolveStats)> {
  let mut edges = Vec::new();
  let stats = resolve_all_spilled_into(
    interner,
    table,
    spill,
    resolver,
    None,
    reach,
    |resolved, _unresolved| resolved,
    |mut chunk: Vec<ResolvedEdge>| edges.append(&mut chunk),
  )?;
  Ok((edges, stats))
}

/// [`resolve_all_spilled`] delivering edges through `sink` — in exactly the order the
/// collected form would have held them — instead of materializing the edge vector (~90 MB
/// alive under the seal at kernel scale). Chunks stream to a worker pool through a bounded
/// channel; the sink runs on the calling thread, fed by a rolling in-order drain of
/// finished chunks (the absorb-holdback pattern).
/// `map_chunk` runs ON THE WORKERS, right after resolution, turning each chunk's raw
/// outcomes into whatever the caller's drain consumes (edge triples, evidence rows,
/// per-file runs) — the ordered `consume` then does bulk appends only. The former
/// per-edge sink pair made the single drain thread construct every row itself: ~9M
/// indirect calls + row builds at kernel scale, the link's serial floor.
#[allow(clippy::too_many_arguments)] // streaming API: two closures plus two optional oracles.
pub fn resolve_all_spilled_into<'i, T: Send>(
  interner: &'i Interner,
  table: &SymbolTable<'i>,
  spill: &crate::RefSpill<'i>,
  resolver: &Resolver,
  chain: Option<&ChainReturns<'i>>,
  reach: Option<&IncludeReach<'i>>,
  map_chunk: impl Fn(Vec<ResolvedEdge>, Vec<UnresolvedEvidence>) -> T + Sync,
  consume: impl FnMut(T),
) -> std::io::Result<ResolveStats> {
  let raw_chunks = spill.raw_chunks()?;
  resolve_chunks_into(
    interner,
    table,
    resolver,
    raw_chunks,
    |bytes| spill.decode_chunk(bytes),
    chain,
    reach,
    map_chunk,
    consume,
  )
}

/// [`resolve_all_spilled_into`] over a retained [`crate::RefStore`] — the memory-primary
/// daemon's link path. Identical pump; only the chunk source differs (alive ranges instead
/// of the whole file).
#[allow(clippy::too_many_arguments)] // the retained-path resolve entry: chain ledger rides with the drains
#[allow(clippy::too_many_arguments)] // streaming API mirrors resolve_all_spilled_into.
pub fn resolve_all_store_into<'i, T: Send>(
  interner: &'i Interner,
  table: &SymbolTable<'i>,
  store: &mut crate::RefStore,
  order: impl IntoIterator<Item = u32>,
  resolver: &Resolver,
  chain: Option<&ChainReturns<'i>>,
  reach: Option<&IncludeReach<'i>>,
  map_chunk: impl Fn(Vec<ResolvedEdge>, Vec<UnresolvedEvidence>) -> T + Sync,
  consume: impl FnMut(T),
) -> std::io::Result<ResolveStats> {
  let raw_chunks = store.raw_chunks(order)?;
  let store = &*store;
  resolve_chunks_into(
    interner,
    table,
    resolver,
    raw_chunks,
    |bytes| store.decode_chunk(interner, bytes),
    chain,
    reach,
    map_chunk,
    consume,
  )
}

/// The shared feed→decode→resolve→ordered-drain pump behind the spilled and retained link
/// paths. Chunk provenance is invisible to resolution (a pure per-reference read of the
/// immutable table), so both sources produce output identical to an in-RAM `resolve_all`
/// over the same reference sequence.
#[allow(clippy::too_many_arguments)] // the shared chunk pump: decode hook + chain ledger + map/consume are load-bearing
#[allow(clippy::too_many_arguments)] // the one shared worker-scope body behind both drains.
fn resolve_chunks_into<'i, T: Send>(
  interner: &'i Interner,
  table: &SymbolTable<'i>,
  resolver: &Resolver,
  raw_chunks: impl Iterator<Item = std::io::Result<Vec<u8>>> + Send,
  decode: impl Fn(&[u8]) -> Vec<Reference<'i>> + Sync,
  chain: Option<&ChainReturns<'i>>,
  reach: Option<&IncludeReach<'i>>,
  map_chunk: impl Fn(Vec<ResolvedEdge>, Vec<UnresolvedEvidence>) -> T + Sync,
  mut consume: impl FnMut(T),
) -> std::io::Result<ResolveStats> {
  type ChunkOut<T> = (usize, T, ResolveStats);
  let threads = std::thread::available_parallelism()
    .map(|n| n.get())
    .unwrap_or(1);
  // Raw record bytes travel to the workers; decode happens beside resolution on the worker
  // threads. The feeder below is pure sequential file reads, and the calling thread does
  // nothing but the ordered drain into the sinks — three formerly-competing roles
  // (read+decode / drain / sink) on one thread become three threads.
  let (work_tx, work_rx) = crossbeam_channel::bounded::<(usize, Vec<u8>)>(threads * 2);
  let (out_tx, out_rx) = crossbeam_channel::unbounded::<ChunkOut<T>>();

  let mut stats = ResolveStats::default();
  let mut feed_error: Option<std::io::Error> = None;
  {
    let consume = &mut consume;
    let stats = &mut stats;
    let feed_error = &mut feed_error;
    let decode = &decode;
    let map_chunk = &map_chunk;
    std::thread::scope(|scope| {
      for _ in 0..threads {
        let work_rx = work_rx.clone();
        let out_tx = out_tx.clone();
        scope.spawn(move || {
          while let Ok((index, bytes)) = work_rx.recv() {
            let chunk = decode(&bytes);
            drop(bytes);
            let (edges, unresolved, stats) =
              resolve_chunk(interner, table, &chunk, resolver, chain, reach);
            let mapped = map_chunk(edges, unresolved);
            if out_tx.send((index, mapped, stats)).is_err() {
              break;
            }
          }
        });
      }
      drop(work_rx);
      drop(out_tx);

      // Feeder: sequential reads only. Its first IO error stops the feed and is surfaced
      // after the scope joins (the drain below still consumes everything already sent).
      scope.spawn(move || {
        for (sent, bytes) in raw_chunks.enumerate() {
          let bytes = match bytes {
            Ok(bytes) => bytes,
            Err(err) => {
              *feed_error = Some(err);
              break;
            }
          };
          if work_tx.send((sent, bytes)).is_err() {
            break;
          }
        }
      });

      let mut holdback: std::collections::BTreeMap<usize, (T, ResolveStats)> =
        std::collections::BTreeMap::new();
      let mut next_out = 0usize;
      // Superlinearity probe over drained references (D7): chunk granularity, one tick
      // per drained chunk — far off the per-reference hot path.
      let mut scaling = vorpal_kg::ScalingProbe::new("link");
      let mut refs_done: u64 = 0;
      while let Ok((index, mapped, chunk_stats)) = out_rx.recv() {
        holdback.insert(index, (mapped, chunk_stats));
        while let Some((mapped, chunk_stats)) = holdback.remove(&next_out) {
          consume(mapped);
          refs_done += chunk_stats.total();
          scaling.tick(refs_done);
          *stats += chunk_stats;
          next_out += 1;
        }
      }
      scaling.finish(refs_done);
    });
  }
  if let Some(err) = feed_error {
    return Err(err);
  }
  Ok(stats)
}

/// The serial kernel: resolve one contiguous run of references in order.
/// One in-RAM batch resolved on the calling thread: the (edges, unresolved, stats) triple
/// the spilled drain hands its chunk callback, for callers whose reference set is small and
/// already materialized — the scoped compose (SUBSECOND.md P4.5c-2) re-resolving ONE file's
/// references against a prior generation. Same code path as every chunk (`resolve_chunk`),
/// so outcomes cannot drift from the pipeline's.
pub fn resolve_batch<'i>(
  interner: &'i Interner,
  table: &SymbolTable<'i>,
  references: &[Reference<'i>],
  resolver: &Resolver,
  chain: Option<&ChainReturns<'i>>,
  reach: Option<&IncludeReach<'i>>,
) -> (Vec<ResolvedEdge>, Vec<UnresolvedEvidence>, ResolveStats) {
  resolve_chunk(interner, table, references, resolver, chain, reach)
}

fn resolve_chunk<'i>(
  interner: &'i Interner,
  table: &SymbolTable<'i>,
  references: &[Reference<'i>],
  resolver: &Resolver,
  chain: Option<&ChainReturns<'i>>,
  reach: Option<&IncludeReach<'i>>,
) -> (Vec<ResolvedEdge>, Vec<UnresolvedEvidence>, ResolveStats) {
  let mut edges = Vec::new();
  let mut unresolved = Vec::new();
  let mut stats = ResolveStats::default();
  let mut scratch = ResolveScratch::default();
  for reference in references {
    let resolution =
      resolver.resolve_with(interner, table, reference, &mut scratch, chain, reach);
    let name_hash =
      xxhash_rust::xxh3::xxh3_64(interner.text_of(reference.name).as_bytes()) as u32;
    match resolution.target {
      Some(to) => {
        if resolution.confidence <= Confidence::AMBIGUOUS {
          stats.ambiguous += 1;
        } else {
          stats.resolved += 1;
        }
        edges.push(ResolvedEdge {
          from: reference.from,
          to,
          edge: resolution.edge,
          confidence: resolution.confidence.0,
          span: reference.evidence,
          reason: resolution.reason,
          candidates: resolution.candidates.min(u32::MAX as usize) as u32,
          name_hash,
          alternatives: resolution.alternatives,
          from_path_bits: reference.from_path.to_bits(),
          name_bits: reference.name.to_bits(),
        });
      }
      None => {
        let external = resolution.candidates == 0;
        if external {
          stats.external += 1;
        } else {
          stats.masked += 1;
        }
        unresolved.push(UnresolvedEvidence {
          from: reference.from,
          name_hash,
          etype: resolution.edge,
          span: reference.evidence,
          candidates: resolution.candidates.min(u32::MAX as usize) as u32,
          external,
          from_path_bits: reference.from_path.to_bits(),
          name_bits: reference.name.to_bits(),
        });
      }
    }
  }
  (edges, unresolved, stats)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::intern::Interner;
  use crate::reference::{RefForm, RefKind, Reference};
  use crate::table::{Symbol, SymbolTable};
  use vorpal_kg::SymbolKind;

  /// One shared session for the whole test binary: tests only ever intern a bounded
  /// vocabulary, and `'static` ids keep the assertions free of lifetime plumbing.
  fn itn() -> &'static Interner {
    static INTERNER: std::sync::OnceLock<Interner> = std::sync::OnceLock::new();
    INTERNER.get_or_init(Interner::new)
  }

  fn symbol(id: u64, path: &str, exported: bool, owner: Option<&str>) -> Symbol<'static> {
    Symbol {
      id: NodeId::new(id),
      kind: SymbolKind::Function,
      path: itn().intern(path),
      exported,
      owner: owner.map(|o| itn().intern(o)),
    }
  }

  fn call(name: &str, from_path: &str) -> Reference<'static> {
    Reference::new(itn(), NodeId::new(0), from_path, name, RefKind::Call)
  }

  #[test]
  fn qualifier_binds_to_owner_and_never_falls_back_to_a_guess() {
    let mut table = SymbolTable::new();
    table.insert(itn(), "new", symbol(1, "a.rs", true, Some("Kg")));
    table.insert(itn(), "new", symbol(2, "b.rs", true, Some("Manifest")));

    // `Kg::new()` → exactly the Kg member, despite two visible `new`s.
    table.finalize();
    let r = Resolver::new().resolve(
      itn(),
      &table,
      &call("new", "c.rs")
        .with_qualifier(itn(), Some("Kg".into()))
        .with_form(RefForm::Static),
      None,
    );
    assert_eq!(r.target, Some(NodeId::new(1)));
    assert_eq!(r.confidence, Confidence::CROSS_FILE);

    // `Vec::new()` → Vec is not in the tree; a static path must NOT guess by bare name.
    let r = Resolver::new().resolve(
      itn(),
      &table,
      &call("new", "c.rs")
        .with_qualifier(itn(), Some("Vec".into()))
        .with_form(RefForm::Static),
      None,
    );
    assert_eq!(r.target, None);
    assert_eq!(r.candidates, 2, "candidates reported for transparency");
  }

  #[test]
  fn static_path_matches_module_file_stem() {
    let mut table = SymbolTable::new();
    table.insert(itn(), "helper", symbol(1, "src/util.rs", true, None));
    table.insert(itn(), "helper", symbol(2, "src/other.rs", true, None));

    table.finalize();
    let r = Resolver::new().resolve(
      itn(),
      &table,
      &call("helper", "src/a.rs")
        .with_qualifier(itn(), Some("util".into()))
        .with_form(RefForm::Static),
      None,
    );
    assert_eq!(r.target, Some(NodeId::new(1)));

    // Method receivers must not module-match: a variable named `util` is not evidence.
    let r = Resolver::new().resolve(
      itn(),
      &table,
      &call("helper", "src/a.rs")
        .with_qualifier(itn(), Some("util".into()))
        .with_form(RefForm::Method),
      None,
    );
    assert_eq!(r.target, None, "receiver text is not namespace evidence");
  }

  #[test]
  fn import_bindings_bind_bare_uses_but_ties_never_seed() {
    // `helper` is exported by BOTH util.rs and other.rs; the importing file's
    // `use crate::util::helper` disambiguates.
    let mut table = SymbolTable::new();
    table.insert(itn(), "helper", symbol(1, "src/util.rs", true, None));
    table.insert(itn(), "helper", symbol(2, "src/other.rs", true, None));
    table.finalize();

    let import = |from_path: &str, qualifier: &str| {
      Reference::new(itn(), NodeId::new(9), from_path, "helper", RefKind::Import)
        .with_qualifier(itn(), Some(qualifier.into()))
        .with_form(RefForm::Static)
    };
    let seeded = seed_import_bindings(
      itn(),
      &mut table,
      &[import("src/a.rs", "util")],
      &Resolver::new(),
    );
    assert_eq!(seeded, 1);

    // The bare call in the importing file inherits the import-proven target...
    let r = Resolver::new().resolve(itn(), &table, &call("helper", "src/a.rs"), None);
    assert_eq!(r.target, Some(NodeId::new(1)));
    assert_eq!(r.confidence, Confidence::CROSS_FILE);
    assert_eq!(r.reason, ResolveReason::ImportBound);
    // ...while the same call in a file with no import stays a labelled blind tie.
    let r = Resolver::new().resolve(itn(), &table, &call("helper", "src/b.rs"), None);
    assert_eq!(r.reason, ResolveReason::VisibleTie);
    assert_eq!(r.confidence, Confidence::AMBIGUOUS);

    // A local definition shadows the file's own import.
    let mut shadowed = SymbolTable::new();
    shadowed.insert(itn(), "helper", symbol(1, "src/util.rs", true, None));
    shadowed.insert(itn(), "helper", symbol(3, "src/a.rs", false, None));
    shadowed.finalize();
    seed_import_bindings(itn(), &mut shadowed, &[import("src/a.rs", "util")], &Resolver::new());
    let r = Resolver::new().resolve(itn(), &shadowed, &call("helper", "src/a.rs"), None);
    assert_eq!(r.target, Some(NodeId::new(3)));
    assert_eq!(r.reason, ResolveReason::Local);

    // An aliased import rebinds under its LOCAL name: the binding keys on the alias, and a
    // bare use of that alias — a name with ZERO candidates of its own — still resolves
    // import-bound (the consult precedes the empty-candidates return).
    let mut aliased = SymbolTable::new();
    aliased.insert(itn(), "helper", symbol(1, "src/util.rs", true, None));
    aliased.finalize();
    let aliased_import = Reference::new(itn(), NodeId::new(9), "src/a.rs", "helper", RefKind::Import)
      .with_qualifier(itn(), Some("util".into()))
      .with_form(RefForm::Static)
      .with_alias_ref(itn(), Some("h"));
    let seeded = seed_import_bindings(itn(), &mut aliased, &[aliased_import], &Resolver::new());
    assert_eq!(seeded, 1);
    let r = Resolver::new().resolve(itn(), &aliased, &call("h", "src/a.rs"), None);
    assert_eq!(r.target, Some(NodeId::new(1)), "alias call binds the original");
    assert_eq!(r.reason, ResolveReason::ImportBound);
    assert_eq!(r.candidates, 0, "the alias itself is defined nowhere");
    // The ORIGINAL name did not get a binding in the aliased form — `helper()` in that file
    // resolves through normal visibility, not the alias binding.
    let r = Resolver::new().resolve(itn(), &aliased, &call("helper", "src/a.rs"), None);
    assert_eq!(r.reason, ResolveReason::VisibleExport);

    // Two module files share the qualifier's stem: the import itself is a qualifier TIE, and
    // a tied import must seed nothing — ambiguity never launders into a binding.
    let mut tied = SymbolTable::new();
    tied.insert(itn(), "helper", symbol(1, "src/util.rs", true, None));
    tied.insert(itn(), "helper", symbol(2, "vendor/util.rs", true, None));
    tied.finalize();
    let seeded = seed_import_bindings(itn(), &mut tied, &[import("src/a.rs", "util")], &Resolver::new());
    assert_eq!(seeded, 0, "a tied import proves nothing");
    let r = Resolver::new().resolve(itn(), &tied, &call("helper", "src/a.rs"), None);
    assert_eq!(r.reason, ResolveReason::VisibleTie, "no binding, so the tie stays labelled");
  }

  #[test]
  fn receiver_hints_upgrade_but_never_veto() {
    let mut table = SymbolTable::new();
    table.insert(itn(), "draw", symbol(1, "a.rs", true, Some("Chart")));
    table.insert(itn(), "draw", symbol(2, "b.rs", true, Some("Grid")));
    table.insert(itn(), "render", symbol(3, "a.rs", true, Some("Chart")));
    table.finalize();

    // The hint names an owner: corroborated exactly like a static qualifier, even among
    // several same-named members.
    let r = Resolver::new().resolve(
      itn(),
      &table,
      &call("draw", "c.rs")
        .with_qualifier(itn(), Some("Grid".into()))
        .with_form(RefForm::MethodHinted),
      None,
    );
    assert_eq!(r.target, Some(NodeId::new(2)));
    assert_eq!(r.reason, ResolveReason::Qualified);

    // The hint names nothing (an opaque variable): fall back to Method semantics — a unique
    // member binds…
    let r = Resolver::new().resolve(
      itn(),
      &table,
      &call("render", "c.rs")
        .with_qualifier(itn(), Some("mystery".into()))
        .with_form(RefForm::MethodHinted),
      None,
    );
    assert_eq!(r.target, Some(NodeId::new(3)), "hint must not veto the unique member");
    assert_eq!(r.reason, ResolveReason::VisibleExport);

    // …and a non-unique one stays unbound (no blind guessing), exactly like plain Method.
    let r = Resolver::new().resolve(
      itn(),
      &table,
      &call("draw", "c.rs")
        .with_qualifier(itn(), Some("mystery".into()))
        .with_form(RefForm::MethodHinted),
      None,
    );
    assert_eq!(r.target, None);

    // A hint must never module-stem match: a variable sharing a file's name is coincidence.
    let mut files = SymbolTable::new();
    files.insert(itn(), "helper", symbol(7, "src/util.rs", true, None));
    files.insert(itn(), "helper", symbol(8, "src/other.rs", true, None));
    files.finalize();
    let r = Resolver::new().resolve(
      itn(),
      &files,
      &call("helper", "c.rs")
        .with_qualifier(itn(), Some("util".into()))
        .with_form(RefForm::MethodHinted),
      None,
    );
    assert_eq!(r.target, None, "hinted receivers get owner matching only");
  }

  #[test]
  fn method_form_requires_a_unique_visible_candidate() {
    let mut table = SymbolTable::new();
    table.insert(itn(), "map", symbol(1, "a.rs", true, Some("Chart")));
    table.insert(itn(), "map", symbol(2, "b.rs", true, Some("Grid")));

    // `x.map()` with two visible candidates: no edge, counted as masked.
    table.finalize();
    let r =
      Resolver::new().resolve(itn(), &table, &call("map", "c.rs").with_form(RefForm::Method), None);
    assert_eq!(r.target, None);

    // Bare `map()` keeps the labeled approximate pick.
    let r = Resolver::new().resolve(itn(), &table, &call("map", "c.rs"), None);
    assert_eq!(r.target, Some(NodeId::new(1)));
    assert_eq!(r.confidence, Confidence::AMBIGUOUS);

    // A unique candidate binds for methods too.
    let mut unique = SymbolTable::new();
    unique.insert(itn(), "map", symbol(7, "a.rs", true, Some("Chart")));
    unique.finalize();
    let r = Resolver::new().resolve(
      itn(),
      &unique,
      &call("map", "c.rs").with_form(RefForm::Method),
      None,
    );
    assert_eq!(r.target, Some(NodeId::new(7)));
  }

  #[test]
  fn rust_child_modules_see_parent_file_privates() {
    let mut table = SymbolTable::new();
    table.insert(
      itn(),
      "print_text_to",
      symbol(1, "src/outline/output.rs", false, None),
    );

    // `output/tests.rs` is a descendant module of `output.rs` → private is visible.
    table.finalize();
    let r = Resolver::new().resolve(
      itn(),
      &table,
      &call("print_text_to", "src/outline/output/tests.rs"),
      None,
    );
    assert_eq!(r.target, Some(NodeId::new(1)));
    assert_eq!(r.confidence, Confidence::CROSS_FILE);

    // A sibling file is not.
    table.finalize();
    let r =
      Resolver::new().resolve(itn(), &table, &call("print_text_to", "src/outline/other.rs"), None);
    assert_eq!(r.target, None);

    // Carrier files own their directory subtree.
    let mut lib = SymbolTable::new();
    lib.insert(itn(), "internal", symbol(2, "crates/x/src/lib.rs", false, None));
    lib.finalize();
    let r =
      Resolver::new().resolve(itn(), &lib, &call("internal", "crates/x/src/deep/nested.rs"), None);
    assert_eq!(r.target, Some(NodeId::new(2)));
    let r = Resolver::new().resolve(itn(), &lib, &call("internal", "crates/y/src/a.rs"), None);
    assert_eq!(r.target, None);
  }

  #[test]
  fn java_package_privates_are_visible_within_their_directory() {
    let mut table = SymbolTable::new();
    table.insert(itn(), "Helper", symbol(1, "src/com/x/Helper.java", false, None));

    // Same package (directory) → visible.
    table.finalize();
    let r = Resolver::new().resolve(itn(), &table, &call("Helper", "src/com/x/Main.java"), None);
    assert_eq!(r.target, Some(NodeId::new(1)));

    // Another package → package-private stays masked.
    let r = Resolver::new().resolve(itn(), &table, &call("Helper", "src/com/y/Main.java"), None);
    assert_eq!(r.target, None);

    // Directory scoping never leaks across languages.
    let r = Resolver::new().resolve(itn(), &table, &call("Helper", "src/com/x/main.rs"), None);
    assert_eq!(r.target, None);
  }

  #[test]
  fn sharded_resolve_all_matches_the_serial_loop_edge_for_edge() {
    // A table with every outcome class: unique resolutions, ambiguous ties, masked privates,
    // owner-qualified members, and plenty of external misses.
    let mut table = SymbolTable::new();
    for i in 0..64u64 {
      table.insert(itn(), &format!("unique_{i}"), symbol(i, "defs.rs", true, None));
      table.insert(itn(), &format!("dup_{i}"), symbol(1000 + i, "a.rs", true, None));
      table.insert(itn(), &format!("dup_{i}"), symbol(2000 + i, "b.rs", true, None));
      table.insert(
        itn(),
        &format!("hidden_{i}"),
        symbol(3000 + i, "priv.rs", false, None),
      );
      table.insert(
        itn(),
        &format!("method_{i}"),
        symbol(4000 + i, "owners.rs", true, Some("Owner")),
      );
    }

    // Well past the sharding threshold, mixing every reference shape.
    let mut references = Vec::new();
    for round in 0..1500u64 {
      let i = round % 64;
      references.push(call(&format!("unique_{i}"), "use.rs"));
      references.push(call(&format!("dup_{i}"), "use.rs"));
      references.push(call(&format!("hidden_{i}"), "use.rs"));
      references.push(call(&format!("nowhere_{round}"), "use.rs"));
      references.push(
        call(&format!("method_{i}"), "use.rs")
          .with_qualifier(itn(), Some("Owner".into()))
          .with_form(RefForm::Static),
      );
    }
    assert!(references.len() > 4096, "must exercise the sharded path");

    let resolver = Resolver::new();
    table.finalize();
    let (sharded_edges, sharded_stats) = resolve_all(itn(), &table, &references, &resolver, None);

    // The serial specification: one resolver call per reference, in order.
    let mut serial_edges = Vec::new();
    let mut serial_stats = ResolveStats::default();
    for reference in &references {
      let resolution = resolver.resolve(itn(), &table, reference, None);
      match resolution.target {
        Some(to) => {
          if resolution.confidence <= Confidence::AMBIGUOUS {
            serial_stats.ambiguous += 1;
          } else {
            serial_stats.resolved += 1;
          }
          serial_edges.push(ResolvedEdge {
            from: reference.from,
            to,
            edge: resolution.edge,
            confidence: resolution.confidence.0,
            span: reference.evidence,
            reason: resolution.reason,
            candidates: resolution.candidates.min(u32::MAX as usize) as u32,
            name_hash: xxhash_rust::xxh3::xxh3_64(itn().text_of(reference.name).as_bytes())
              as u32,
            alternatives: resolution.alternatives,
            from_path_bits: reference.from_path.to_bits(),
            name_bits: reference.name.to_bits(),
          });
        }
        None => {
          if resolution.candidates == 0 {
            serial_stats.external += 1;
          } else {
            serial_stats.masked += 1;
          }
        }
      }
    }

    assert_eq!(sharded_stats, serial_stats);
    assert_eq!(sharded_edges, serial_edges, "edges must match in order");
  }

  #[test]
  fn stats_split_external_from_masked() {
    let mut table = SymbolTable::new();
    table.insert(itn(), "hidden", symbol(1, "a.rs", false, None));
    let refs = vec![
      call("nowhere", "b.rs"), // no candidates at all → external
      call("hidden", "b.rs"),  // exists, not visible → masked
      call("hidden", "a.rs"),  // same file → resolved
    ];
    table.finalize();
    let (edges, stats) = resolve_all(itn(), &table, &refs, &Resolver::new(), None);
    assert_eq!(edges.len(), 1);
    assert_eq!(stats.resolved, 1);
    assert_eq!(stats.external, 1);
    assert_eq!(stats.masked, 1);
    assert_eq!(stats.unresolved(), 2);
  }
}

#[cfg(test)]
mod grade_tests {
  use super::{Confidence, ResolutionGrade};

  #[test]
  fn grade_maps_each_confidence_tier() {
    assert_eq!(Confidence::LOCAL.grade(), ResolutionGrade::Exact);
    assert_eq!(Confidence::CROSS_FILE.grade(), ResolutionGrade::Constrained);
    assert_eq!(Confidence::AMBIGUOUS.grade(), ResolutionGrade::Heuristic);
    assert_eq!(Confidence::NONE.grade(), ResolutionGrade::Unresolved);
    // Ordered strongest-last so `>=` comparisons rank grades.
    assert!(ResolutionGrade::Exact > ResolutionGrade::Heuristic);
    assert_eq!(ResolutionGrade::Exact.label(), "exact");
  }
}

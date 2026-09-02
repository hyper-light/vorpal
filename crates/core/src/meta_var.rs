use crate::match_tree::does_node_match_exactly;
use crate::matcher::Matcher;
use crate::source::Content;
use crate::{Doc, Node};
use std::borrow::Cow;
use std::collections::HashMap;

use crate::replacer::formatted_slice;

pub type MetaVariableID = String;

pub type Underlying<D> = Vec<<<D as Doc>::Source as Content>::Underlying>;

/// a dictionary that stores metavariable instantiation
/// const a = 123 matched with const a = $A will produce env: $A => 123
pub struct MetaVarEnv<'tree, D: Doc> {
  // Insertion-ordered association vectors, not HashMaps: an env holds a
  // handful of variables, and the matcher clones it copy-on-write PER
  // CANDIDATE — the map form spent ~29 % of kernel-scale ingest allocations
  // on String keys, table allocations, and rehashes (ledger-sampled). Linear
  // scans over ≤~8 entries beat hashing String keys, a clone is three Vec
  // memcpys, and iteration order becomes deterministic (insertion order).
  // Keys are INTERNED (`intern_var`): the name universe is compile-bounded
  // (rule meta-vars and labels), so rows carry `&'static str` — no key
  // `String` per capture (was 16 % of post-pass-10 stream allocation
  // samples) and clones copy flat rows.
  single_matched: Vec<(&'static str, Node<'tree, D>)>,
  multi_matched: Vec<(&'static str, Vec<Node<'tree, D>>)>,
  transformed_var: Vec<(&'static str, Underlying<D>)>,
  /// The `"secondary"` diagnostic label — the ONLY label the workspace ever
  /// adds (every relational rule match pushes one). A dedicated field costs
  /// no key `String` per env and clones as a plain Vec; the read paths below
  /// reconstruct the exact old surface (labels listing, JSON export,
  /// cross-thread re-adoption).
  secondary: Vec<Node<'tree, D>>,
  /// Undo log for trial matching (`mark`/`rollback_to`). While a trial is
  /// open (`trial_depth > 0`), any write that is not a plain append — an
  /// overwrite of an existing slot, a push into an existing label's node
  /// list — records how to undo itself here; appends need no entry because
  /// rollback truncates each vec to its marked length. Empty whenever no
  /// trial is open. EVERY future mutating method must either be append-only
  /// or journal its non-append effect, or rollback stops being exact.
  journal: Vec<Undo<'tree, D>>,
  /// Number of currently open trials (marks not yet rolled back). Trials
  /// nest (an ellipsis probe inside an ellipsis probe) and are strictly
  /// LIFO — each `mark` is paired with exactly one `rollback_to`.
  trial_depth: u32,
}

/// A snapshot taken by [`MetaVarEnv::mark`]: the four vec lengths plus the
/// journal watermark. [`MetaVarEnv::rollback_to`] restores the env to this
/// point byte-exactly — the matcher brackets speculative match attempts
/// (ellipsis lookahead probes) with a mark/rollback pair instead of cloning
/// the whole env per probed candidate.
#[derive(Clone, Copy)]
pub(crate) struct EnvMark {
  single: usize,
  multi: usize,
  transformed: usize,
  secondary: usize,
  journal: usize,
}

/// One reversible non-append write, replayed in reverse by `rollback_to`.
/// Old values are MOVED in (`mem::replace`) — journaling allocates nothing
/// beyond the journal vec's own capacity.
enum Undo<'tree, D: Doc> {
  /// `single_matched[i].1` held this node before an overwrite.
  Single(usize, Node<'tree, D>),
  /// `multi_matched[i].1` held this list before an overwrite.
  Multi(usize, Vec<Node<'tree, D>>),
  /// `transformed_var[i].1` held these bytes before an overwrite.
  Transformed(usize, Underlying<D>),
  /// `add_label` pushed one node into `multi_matched[i].1`.
  MultiInnerPush(usize),
}

impl<'t, D: Doc> Clone for Undo<'t, D> {
  fn clone(&self) -> Self {
    match self {
      Undo::Single(i, n) => Undo::Single(*i, n.clone()),
      Undo::Multi(i, ns) => Undo::Multi(*i, ns.clone()),
      Undo::Transformed(i, v) => Undo::Transformed(*i, v.clone()),
      Undo::MultiInnerPush(i) => Undo::MultiInnerPush(*i),
    }
  }
}

/// The copy-on-write clone preserves each vector's CAPACITY, not just its
/// length: `derive(Clone)` produced exact-capacity vectors, so the very next
/// push after every clone reallocated — relational sub-matches clone per
/// candidate and immediately push, which sampled at ~30 % of post-pass-10
/// stream allocations across the `secondary`/assoc growth sites. Capacity is
/// the source's own high-water mark — data-derived slack, no constants.
impl<D: Doc> Clone for MetaVarEnv<'_, D> {
  fn clone(&self) -> Self {
    fn keep_high_water<T: Clone>(src: &[T], capacity: usize) -> Vec<T> {
      let mut out = Vec::with_capacity(capacity.max(src.len()));
      out.extend_from_slice(src);
      out
    }
    Self {
      single_matched: keep_high_water(&self.single_matched, self.single_matched.capacity()),
      multi_matched: keep_high_water(&self.multi_matched, self.multi_matched.capacity()),
      transformed_var: keep_high_water(&self.transformed_var, self.transformed_var.capacity()),
      secondary: keep_high_water(&self.secondary, self.secondary.capacity()),
      // Envs are cloned between trials (relational sub-matches, Cow
      // materialization), so the journal is virtually always empty here —
      // cloned anyway so a mid-trial clone stays exactly rollback-able.
      journal: self.journal.clone(),
      trial_depth: self.trial_depth,
    }
  }
}

/// Intern a meta-variable name. The universe is compile-bounded — rule
/// pattern meta-vars and rule labels, a few hundred short strings across all
/// rulesets — leaked once each. The per-thread cache makes the hot path (one
/// probe per fresh capture) a thread-local hash lookup with zero shared-memory
/// traffic; the global set is consulted only the first time a thread meets a
/// name. Locks recover from poisoning instead of panicking (no-panics law).
fn intern_var(name: &str) -> &'static str {
  use std::cell::RefCell;
  use std::collections::HashSet;
  use std::sync::{OnceLock, RwLock};
  thread_local! {
    static LOCAL: RefCell<HashMap<String, &'static str>> = RefCell::new(HashMap::new());
  }
  static GLOBAL: OnceLock<RwLock<HashSet<&'static str>>> = OnceLock::new();
  LOCAL.with(|cache| {
    if let Some(interned) = cache.borrow().get(name) {
      return *interned;
    }
    let global = GLOBAL.get_or_init(|| RwLock::new(HashSet::new()));
    let interned = {
      let readable = global.read().unwrap_or_else(|e| e.into_inner());
      readable.get(name).copied()
    };
    let interned = interned.unwrap_or_else(|| {
      let mut writable = global.write().unwrap_or_else(|e| e.into_inner());
      match writable.get(name) {
        Some(&found) => found,
        None => {
          let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
          writable.insert(leaked);
          leaked
        }
      }
    });
    cache.borrow_mut().insert(name.to_string(), interned);
    interned
  })
}

/// Trial snapshot for a `Cow<MetaVarEnv>`, matching its two states at mark
/// time. Shared by every speculative-match site — the ellipsis lookahead
/// probe in the pattern matcher and the composite combinators
/// (`All`/`Any`/`Or`/`Not`) — which used to isolate branches by cloning the
/// env per attempt.
pub(crate) enum CowEnvMark<'c, 'tree, D: Doc> {
  /// Still borrowed when the trial opened: nothing to journal — if a trial
  /// write materializes an owned clone (`to_mut`), rollback just restores
  /// the borrow and drops that clone, exactly the old discard-the-clone
  /// cost, still paid only when a trial actually writes.
  Borrowed(&'c MetaVarEnv<'tree, D>),
  /// Already owned: undo in place via the env's journal — no clone at all.
  Owned(EnvMark),
}

/// Mark/rollback/commit on a `Cow`-held env. Marks nest and MUST be closed
/// LIFO, exactly once each, by either `env_rollback` (undo the trial
/// byte-exactly) or `env_commit` (keep the trial's writes) — leaving a mark
/// open leaks the trial depth and the env journals forever after.
pub(crate) trait CowEnvExt<'c, 'tree, D: Doc> {
  fn env_mark(&mut self) -> CowEnvMark<'c, 'tree, D>;
  fn env_rollback(&mut self, mark: CowEnvMark<'c, 'tree, D>);
  fn env_commit(&mut self, mark: CowEnvMark<'c, 'tree, D>);
}

impl<'c, 'tree, D: Doc> CowEnvExt<'c, 'tree, D> for Cow<'c, MetaVarEnv<'tree, D>> {
  fn env_mark(&mut self) -> CowEnvMark<'c, 'tree, D> {
    match self {
      Cow::Borrowed(orig) => CowEnvMark::Borrowed(orig),
      Cow::Owned(env) => CowEnvMark::Owned(env.mark()),
    }
  }
  fn env_rollback(&mut self, mark: CowEnvMark<'c, 'tree, D>) {
    match mark {
      CowEnvMark::Borrowed(orig) => *self = Cow::Borrowed(orig),
      CowEnvMark::Owned(m) => {
        // A Cow never reverts to Borrowed while an Owned mark is open
        // (marks pair LIFO), so the env is still the one that was marked.
        if let Cow::Owned(env) = self {
          env.rollback_to(m);
        }
      }
    }
  }
  fn env_commit(&mut self, mark: CowEnvMark<'c, 'tree, D>) {
    match mark {
      // Marked while borrowed: no trial was opened on the env — whatever
      // state the Cow is in now (still borrowed, or owned via a write) IS
      // the committed state.
      CowEnvMark::Borrowed(_) => {}
      CowEnvMark::Owned(m) => {
        if let Cow::Owned(env) = self {
          env.commit_to(m);
        }
      }
    }
  }
}

fn assoc_get<'a, V>(list: &'a [(&'static str, V)], key: &str) -> Option<&'a V> {
  list.iter().find(|(k, _)| *k == key).map(|(_, v)| v)
}

/// Set `key` in an assoc vec, journaling the displaced value when a trial is
/// open. An insert of a NEW key is a plain append — rollback's truncate
/// undoes it — so only the overwrite arm records an entry; `wrap` names the
/// `Undo` variant for the vec being written. The old value is moved into the
/// journal, so no allocation happens on either arm.
fn assoc_set_undoable<'t, D: Doc, V>(
  list: &mut Vec<(&'static str, V)>,
  trial_depth: u32,
  journal: &mut Vec<Undo<'t, D>>,
  key: &str,
  value: V,
  wrap: fn(usize, V) -> Undo<'t, D>,
) {
  let found = list
    .iter_mut()
    .enumerate()
    .find(|(_, (k, _))| *k == key);
  match found {
    Some((index, (_, slot))) => {
      let old = std::mem::replace(slot, value);
      if trial_depth > 0 {
        journal.push(wrap(index, old));
      }
    }
    None => list.push((intern_var(key), value)),
  }
}

impl<'t, D: Doc> MetaVarEnv<'t, D> {
  pub fn new() -> Self {
    Self {
      single_matched: Vec::new(),
      multi_matched: Vec::new(),
      transformed_var: Vec::new(),
      secondary: Vec::new(),
      journal: Vec::new(),
      trial_depth: 0,
    }
  }

  /// Open a trial: snapshot the env so a speculative match attempt can be
  /// undone byte-exactly by [`rollback_to`](Self::rollback_to). Marks nest
  /// and MUST be rolled back LIFO, exactly once each — while any trial is
  /// open, non-append writes journal their undo.
  pub(crate) fn mark(&mut self) -> EnvMark {
    self.trial_depth += 1;
    EnvMark {
      single: self.single_matched.len(),
      multi: self.multi_matched.len(),
      transformed: self.transformed_var.len(),
      secondary: self.secondary.len(),
      journal: self.journal.len(),
    }
  }

  /// Close the most recent open trial KEEPING its writes. Journal entries
  /// above the mark are retained while any outer trial is still open — the
  /// outer rollback must be able to undo this trial's committed writes too —
  /// and dropped once no trial is open (nothing can replay them, and they
  /// pin tree handles).
  pub(crate) fn commit_to(&mut self, _mark: EnvMark) {
    self.trial_depth = self.trial_depth.saturating_sub(1);
    if self.trial_depth == 0 {
      self.journal.clear();
    }
  }

  /// Close the most recent open trial: replay the journal down to the mark
  /// (restoring overwritten slots and popping label pushes, newest first),
  /// then truncate every vec to its marked length. Appends vanish with the
  /// truncate; everything else is restored from the journal — the env is
  /// byte-identical to the moment `mark` returned.
  pub(crate) fn rollback_to(&mut self, mark: EnvMark) {
    while self.journal.len() > mark.journal {
      match self.journal.pop() {
        Some(Undo::Single(i, old)) => {
          if let Some((_, slot)) = self.single_matched.get_mut(i) {
            *slot = old;
          }
        }
        Some(Undo::Multi(i, old)) => {
          if let Some((_, slot)) = self.multi_matched.get_mut(i) {
            *slot = old;
          }
        }
        Some(Undo::Transformed(i, old)) => {
          if let Some((_, slot)) = self.transformed_var.get_mut(i) {
            *slot = old;
          }
        }
        Some(Undo::MultiInnerPush(i)) => {
          if let Some((_, nodes)) = self.multi_matched.get_mut(i) {
            nodes.pop();
          }
        }
        None => break,
      }
    }
    self.single_matched.truncate(mark.single);
    self.multi_matched.truncate(mark.multi);
    self.transformed_var.truncate(mark.transformed);
    self.secondary.truncate(mark.secondary);
    self.trial_depth = self.trial_depth.saturating_sub(1);
  }

  /// Reset for reuse as a match scratch: every binding, label,
  /// transformation, and trial record is dropped while every buffer's
  /// capacity is kept. The outline matching loop recycles one env across
  /// failed candidate attempts — each attempt used to buy its vectors
  /// afresh (the two largest stream-phase allocation sites at kernel scale,
  /// ledger-sampled); successful envs depart into their `NodeMatch`
  /// instead, which is exactly the live data.
  pub fn reset_for_reuse(&mut self) {
    self.single_matched.clear();
    self.multi_matched.clear();
    self.transformed_var.clear();
    self.secondary.clear();
    self.journal.clear();
    self.trial_depth = 0;
  }

  /// Run a speculative match against this env and DISCARD every write it
  /// makes, returning only the closure's verdict. The closure sees the env's
  /// current bindings (metavar consistency holds) through a `Cow` it may
  /// write freely; afterwards the env is byte-identical to before — appends
  /// truncate away, overwrites restore from the undo journal. This replaces
  /// the borrow-the-env-and-let-the-first-write-clone-it probe protocol
  /// (predicate rules, sibling probes): no clone on any path.
  pub fn probe<R>(&mut self, run: impl FnOnce(&mut Cow<MetaVarEnv<'t, D>>) -> R) -> R {
    let mut taken = std::mem::take(self);
    let mark = taken.mark();
    let mut env = Cow::Owned(taken);
    let ret = run(&mut env);
    // The Cow stays Owned (nothing reverts an Owned Cow whose marks pair
    // LIFO), so this is a move, not a clone.
    let mut recovered = env.into_owned();
    recovered.rollback_to(mark);
    *self = recovered;
    ret
  }

  pub fn insert(&mut self, id: &str, ret: Node<'t, D>) -> Option<&mut Self> {
    if self.match_variable(id, &ret) {
      assoc_set_undoable(
        &mut self.single_matched,
        self.trial_depth,
        &mut self.journal,
        id,
        ret,
        Undo::Single,
      );
      Some(self)
    } else {
      None
    }
  }

  pub fn insert_multi(&mut self, id: &str, ret: Vec<Node<'t, D>>) -> Option<&mut Self> {
    if self.match_multi_var(id, &ret) {
      assoc_set_undoable(
        &mut self.multi_matched,
        self.trial_depth,
        &mut self.journal,
        id,
        ret,
        Undo::Multi,
      );
      Some(self)
    } else {
      None
    }
  }

  pub fn get_match(&self, var: &str) -> Option<&'_ Node<'t, D>> {
    assoc_get(&self.single_matched, var)
  }

  pub fn get_multiple_matches(&self, var: &str) -> Vec<Node<'t, D>> {
    assoc_get(&self.multi_matched, var).cloned().unwrap_or_default()
  }

  pub fn add_label(&mut self, label: &str, node: Node<'t, D>) {
    if label == "secondary" {
      // The hot path: relational rules label every sub-match, tens of
      // millions of times per large index — no key String, no assoc scan.
      self.secondary.push(node);
      return;
    }
    let found = self
      .multi_matched
      .iter_mut()
      .enumerate()
      .find(|(_, (k, _))| *k == label);
    match found {
      Some((index, (_, nodes))) => {
        nodes.push(node);
        if self.trial_depth > 0 {
          self.journal.push(Undo::MultiInnerPush(index));
        }
      }
      None => self.multi_matched.push((intern_var(label), vec![node])),
    }
  }

  pub fn get_labels(&self, label: &str) -> Option<&Vec<Node<'t, D>>> {
    if label == "secondary" {
      return (!self.secondary.is_empty()).then_some(&self.secondary);
    }
    assoc_get(&self.multi_matched, label)
  }

  pub fn get_matched_variables(&self) -> impl Iterator<Item = MetaVariable> + use<'_, 't, D> {
    let single = self
      .single_matched
      .iter()
      .map(|(n, _)| MetaVariable::Capture((*n).to_string(), false));
    let transformed = self
      .transformed_var
      .iter()
      .map(|(n, _)| MetaVariable::Capture((*n).to_string(), false));
    let multi = self
      .multi_matched
      .iter()
      .map(|(n, _)| MetaVariable::MultiCapture((*n).to_string()))
      .chain(
        // Parity with the map storage: a labeled env used to surface
        // "secondary" as a multi capture here.
        (!self.secondary.is_empty())
          .then(|| MetaVariable::MultiCapture("secondary".to_string())),
      );
    single.chain(multi).chain(transformed)
  }

  fn match_variable(&self, id: &str, candidate: &Node<'t, D>) -> bool {
    if let Some(m) = assoc_get(&self.single_matched, id) {
      return does_node_match_exactly(m, candidate);
    }
    true
  }
  fn match_multi_var(&self, id: &str, cands: &[Node<'t, D>]) -> bool {
    let Some(nodes) = assoc_get(&self.multi_matched, id) else {
      return true;
    };
    let mut named_nodes = nodes.iter().filter(|n| n.is_named());
    let mut named_cands = cands.iter().filter(|n| n.is_named());
    loop {
      if let Some(node) = named_nodes.next() {
        let Some(cand) = named_cands.next() else {
          // cand is done but node is not
          break false;
        };
        if !does_node_match_exactly(node, cand) {
          break false;
        }
      } else if named_cands.next().is_some() {
        // node is done but cand is not
        break false;
      } else {
        // both None, matches
        break true;
      }
    }
  }

  pub fn match_constraints<M: Matcher>(
    &mut self,
    var_matchers: &HashMap<MetaVariableID, M>,
  ) -> bool {
    if var_matchers.is_empty() {
      return true;
    }
    // Snapshot the constrained bindings first: constraint patterns may append
    // rows or equality-overwrite them while matching, and each constraint
    // must see its binding as it stood when checking began — the exact reads
    // the old iterate-self-write-a-copy protocol performed. The snapshot is
    // a handful of node handles; the old protocol cloned the WHOLE env on a
    // constraint's first write (every vector, at high-water capacity once
    // the outline loop began recycling envs — ledger-sampled at ~5 % of
    // kernel-scale stream allocations).
    let pairs: Vec<(&'static str, Node<'t, D>)> = self
      .single_matched
      .iter()
      .filter(|(id, _)| var_matchers.contains_key(*id))
      .map(|(id, node)| (*id, node.clone()))
      .collect();
    if pairs.is_empty() {
      return true;
    }
    // Run the constraints on the live env under a trial: failure restores
    // this env byte-exactly (the old protocol dropped the written copy),
    // success commits in place (the old protocol moved the copy over self).
    let mut taken = std::mem::take(self);
    let mark = taken.mark();
    let mut env = Cow::Owned(taken);
    let mut ok = true;
    for (var_id, candidate) in pairs {
      if let Some(m) = var_matchers.get(var_id)
        && m.match_node_with_env(candidate, &mut env).is_none()
      {
        ok = false;
        break;
      }
    }
    // The Cow stayed Owned throughout (nothing reverts an Owned Cow whose
    // marks all pair LIFO), so this is a move, not a clone.
    let mut recovered = env.into_owned();
    if ok {
      recovered.commit_to(mark);
    } else {
      recovered.rollback_to(mark);
    }
    *self = recovered;
    ok
  }

  pub fn insert_transformation(&mut self, var: &MetaVariable, name: &str, slice: Underlying<D>) {
    let node = match var {
      MetaVariable::Capture(v, _) => assoc_get(&self.single_matched, v),
      MetaVariable::MultiCapture(vs) => {
        assoc_get(&self.multi_matched, vs).and_then(|vs| vs.first())
      }
      _ => None,
    };
    let deindented = if let Some(v) = node {
      // Borrowed means "unchanged": keep the caller's buffer instead of copying the
      // whole value a second time (most single-line transforms take this arm).
      match formatted_slice(&slice, v.get_doc().get_source(), v.range().start) {
        std::borrow::Cow::Owned(v) => v,
        std::borrow::Cow::Borrowed(_) => slice,
      }
    } else {
      slice
    };
    assoc_set_undoable(
      &mut self.transformed_var,
      self.trial_depth,
      &mut self.journal,
      name,
      deindented,
      Undo::Transformed,
    );
  }

  pub fn get_transformed(&self, var: &str) -> Option<&Underlying<D>> {
    assoc_get(&self.transformed_var, var)
  }
  pub fn get_var_bytes<'s>(
    &'s self,
    var: &MetaVariable,
  ) -> Option<&'s [<D::Source as Content>::Underlying]> {
    get_var_bytes_impl(self, var)
  }
}

impl<D: Doc> MetaVarEnv<'_, D> {
  /// internal for readopt NodeMatch in pinned.rs
  /// readopt node and env when sending them to other threads
  pub(crate) fn visit_nodes<F>(&mut self, mut f: F)
  where
    F: FnMut(&mut Node<'_, D>),
  {
    for (_, n) in self.single_matched.iter_mut() {
      f(n)
    }
    for (_, ns) in self.multi_matched.iter_mut() {
      for n in ns {
        f(n)
      }
    }
    // Secondary label nodes must re-adopt across threads exactly like every
    // other captured node — missing them would leave dangling tree handles.
    for n in self.secondary.iter_mut() {
      f(n)
    }
    // The journal is empty whenever no trial is open (envs cross threads
    // only at rest), but any node it holds is a tree handle all the same.
    for undo in self.journal.iter_mut() {
      match undo {
        Undo::Single(_, n) => f(n),
        Undo::Multi(_, ns) => {
          for n in ns {
            f(n)
          }
        }
        Undo::Transformed(..) | Undo::MultiInnerPush(_) => {}
      }
    }
  }
}

fn get_var_bytes_impl<'e, 't, C, D>(
  env: &'e MetaVarEnv<'t, D>,
  var: &MetaVariable,
) -> Option<&'e [C::Underlying]>
where
  D: Doc<Source = C> + 't,
  C: Content + 't,
{
  match var {
    MetaVariable::Capture(n, _) => {
      if let Some(node) = env.get_match(n) {
        let bytes = node.get_doc().get_source().get_range(node.range());
        Some(bytes)
      } else if let Some(bytes) = env.get_transformed(n) {
        Some(bytes)
      } else {
        None
      }
    }
    MetaVariable::MultiCapture(n) => {
      let nodes = env.get_multiple_matches(n);
      if nodes.is_empty() {
        None
      } else {
        // NOTE: start_byte is not always index range of source's slice.
        // e.g. start_byte is still byte_offset in utf_16 (napi). start_byte
        // so we need to call source's get_range method
        let start = nodes[0].range().start;
        let end = nodes[nodes.len() - 1].range().end;
        Some(nodes[0].get_doc().get_source().get_range(start..end))
      }
    }
    _ => None,
  }
}

impl<D: Doc> Default for MetaVarEnv<'_, D> {
  fn default() -> Self {
    Self::new()
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MetaVariable {
  /// $A for captured meta var
  Capture(MetaVariableID, bool),
  /// $_ for non-captured meta var
  Dropped(bool),
  /// $$$ for non-captured multi var
  Multiple,
  /// $$$A for captured ellipsis
  MultiCapture(MetaVariableID),
}

pub(crate) fn extract_meta_var(src: &str, meta_char: char) -> Option<MetaVariable> {
  use MetaVariable::*;
  let ellipsis: String = std::iter::repeat_n(meta_char, 3).collect();
  if src == ellipsis {
    return Some(Multiple);
  }
  if let Some(trimmed) = src.strip_prefix(&ellipsis) {
    if !trimmed.chars().all(is_valid_meta_var_char) {
      return None;
    }
    if trimmed.starts_with('_') {
      return Some(Multiple);
    } else {
      return Some(MultiCapture(trimmed.to_owned()));
    }
  }
  if !src.starts_with(meta_char) {
    return None;
  }
  let trimmed = &src[meta_char.len_utf8()..];
  let (trimmed, named) = if let Some(t) = trimmed.strip_prefix(meta_char) {
    (t, false)
  } else {
    (trimmed, true)
  };
  if !trimmed.starts_with(is_valid_first_char) || // empty or started with number
    !trimmed.chars().all(is_valid_meta_var_char)
  // not in form of $A or $_
  {
    return None;
  }
  if trimmed.starts_with('_') {
    Some(Dropped(named))
  } else {
    Some(Capture(trimmed.to_owned(), named))
  }
}

#[inline]
fn is_valid_first_char(c: char) -> bool {
  matches!(c, 'A'..='Z' | '_')
}

#[inline]
pub(crate) fn is_valid_meta_var_char(c: char) -> bool {
  is_valid_first_char(c) || c.is_ascii_digit()
}

impl<'tree, D: Doc> From<MetaVarEnv<'tree, D>> for HashMap<String, String> {
  fn from(env: MetaVarEnv<'tree, D>) -> Self {
    let mut ret = HashMap::new();
    for (id, node) in env.single_matched {
      ret.insert(id.to_string(), node.text().into());
    }
    for (id, bytes) in env.transformed_var {
      ret.insert(id.to_string(), <D::Source as Content>::encode_bytes(&bytes).to_string());
    }
    for (id, nodes) in env.multi_matched {
      let s: Vec<_> = nodes.iter().map(|n| n.text()).collect();
      let s = s.join(", ");
      ret.insert(id.to_string(), format!("[{s}]"));
    }
    if !env.secondary.is_empty() {
      let s: Vec<_> = env.secondary.iter().map(|n| n.text()).collect();
      let s = s.join(", ");
      ret.insert("secondary".to_string(), format!("[{s}]"));
    }
    ret
  }
}

#[cfg(test)]
mod test {
  use super::*;
  use crate::Pattern;
  use crate::language::Tsx;
  use crate::tree_sitter::LanguageExt;

  fn extract_var(s: &str) -> Option<MetaVariable> {
    extract_meta_var(s, '$')
  }
  #[test]
  fn test_match_var() {
    use MetaVariable::*;
    assert_eq!(extract_var("$$$"), Some(Multiple));
    assert_eq!(extract_var("$ABC"), Some(Capture("ABC".into(), true)));
    assert_eq!(extract_var("$$ABC"), Some(Capture("ABC".into(), false)));
    assert_eq!(extract_var("$MATCH1"), Some(Capture("MATCH1".into(), true)));
    assert_eq!(extract_var("$$$ABC"), Some(MultiCapture("ABC".into())));
    assert_eq!(extract_var("$_"), Some(Dropped(true)));
    assert_eq!(extract_var("$_123"), Some(Dropped(true)));
    assert_eq!(extract_var("$$_"), Some(Dropped(false)));
  }

  #[test]
  fn test_not_meta_var() {
    assert_eq!(extract_var("$123"), None);
    assert_eq!(extract_var("$"), None);
    assert_eq!(extract_var("$$"), None);
    assert_eq!(extract_var("abc"), None);
    assert_eq!(extract_var("$abc"), None);
  }

  fn match_constraints(pattern: &str, node: &str) -> bool {
    let mut matchers = HashMap::new();
    matchers.insert("A".to_string(), Pattern::new(pattern, Tsx));
    let mut env = MetaVarEnv::new();
    let root = Tsx.grep(node);
    let node = root.root().child(0).unwrap().child(0).unwrap();
    env.insert("A", node);
    env.match_constraints(&matchers)
  }

  #[test]
  fn test_non_ascii_meta_var() {
    let extract = |s| extract_meta_var(s, 'µ');
    use MetaVariable::*;
    assert_eq!(extract("µµµ"), Some(Multiple));
    assert_eq!(extract("µABC"), Some(Capture("ABC".into(), true)));
    assert_eq!(extract("µµABC"), Some(Capture("ABC".into(), false)));
    assert_eq!(extract("µµµABC"), Some(MultiCapture("ABC".into())));
    assert_eq!(extract("µ_"), Some(Dropped(true)));
    assert_eq!(extract("abc"), None);
    assert_eq!(extract("µabc"), None);
  }

  #[test]
  fn test_match_constraints() {
    assert!(match_constraints("a + b", "a + b"));
  }

  // Trial rollback must be a byte-exact undo: appends vanish, and an
  // overwrite of a pre-mark binding restores the ORIGINAL node (same
  // node id), not merely an exactly-matching one — the ellipsis probe in
  // the matcher relies on this instead of cloning the env per candidate.
  #[test]
  fn test_mark_rollback_exact() {
    let root = Tsx.grep("foo; foo; bar;");
    let root = root.root();
    let mut stmts = root.children();
    let (foo1, foo2, bar) = (
      stmts.next().expect("has stmt"),
      stmts.next().expect("has stmt"),
      stmts.next().expect("has stmt"),
    );
    assert_ne!(foo1.node_id(), foo2.node_id());

    let mut env = MetaVarEnv::new();
    env.insert("A", foo1.clone()).expect("fresh bind");
    env.add_label("L", bar.clone());
    let mark = env.mark();
    // Overwrite below the mark (equality-checked: foo2 matches foo1 exactly),
    // append a new var, push into an existing label, push a secondary.
    env.insert("A", foo2.clone()).expect("exact re-bind");
    assert_eq!(
      env.get_match("A").expect("bound").node_id(),
      foo2.node_id()
    );
    env.insert("B", bar.clone()).expect("fresh bind");
    env.add_label("L", foo2.clone());
    env.add_label("secondary", foo2.clone());
    env.rollback_to(mark);

    let a = env.get_match("A").expect("A survives rollback");
    assert_eq!(a.node_id(), foo1.node_id(), "original binding restored");
    assert!(env.get_match("B").is_none(), "trial append truncated");
    let labels = env.get_labels("L").expect("label survives");
    assert_eq!(labels.len(), 1, "trial label push undone");
    assert_eq!(labels[0].node_id(), bar.node_id());
    assert!(env.get_labels("secondary").is_none(), "trial secondary undone");
  }

  // Nested trials roll back LIFO, each to its own mark.
  #[test]
  fn test_mark_rollback_nested() {
    let root = Tsx.grep("foo; foo; bar;");
    let root = root.root();
    let mut stmts = root.children();
    let (foo1, foo2, bar) = (
      stmts.next().expect("has stmt"),
      stmts.next().expect("has stmt"),
      stmts.next().expect("has stmt"),
    );

    let mut env = MetaVarEnv::new();
    env.insert("A", foo1.clone()).expect("fresh bind");
    let outer = env.mark();
    env.insert("B", bar.clone()).expect("fresh bind");
    let inner = env.mark();
    env.insert("A", foo2.clone()).expect("exact re-bind");
    env.insert("C", bar.clone()).expect("fresh bind");
    env.rollback_to(inner);
    // Inner trial undone; outer trial's append still live.
    assert_eq!(
      env.get_match("A").expect("bound").node_id(),
      foo1.node_id()
    );
    assert!(env.get_match("B").is_some());
    assert!(env.get_match("C").is_none());
    env.rollback_to(outer);
    assert!(env.get_match("B").is_none());
    assert_eq!(
      env.get_match("A").expect("bound").node_id(),
      foo1.node_id()
    );
  }

  #[test]
  fn test_match_not_constraints() {
    assert!(!match_constraints("a - b", "a + b"));
  }

  #[test]
  fn test_multi_var_match() {
    let grep = Tsx.grep("if (true) { a += 1; b += 1 } else { a += 1; b += 1 }");
    let node = grep.root();
    let found = node.find("if (true) { $$$A } else { $$$A }");
    assert!(found.is_some());
    let grep = Tsx.grep("if (true) { a += 1 } else { b += 1 }");
    let node = grep.root();
    let not_found = node.find("if (true) { $$$A } else { $$$A }");
    assert!(not_found.is_none());
  }

  #[test]
  fn test_multi_var_match_with_trailing() {
    let grep = Tsx.grep("if (true) { a += 1; } else { a += 1; b += 1 }");
    let node = grep.root();
    let not_found = node.find("if (true) { $$$A } else { $$$A }");
    assert!(not_found.is_none());
    let grep = Tsx.grep("if (true) { a += 1; b += 1; } else { a += 1 }");
    let node = grep.root();
    let not_found = node.find("if (true) { $$$A } else { $$$A }");
    assert!(not_found.is_none());
  }
}

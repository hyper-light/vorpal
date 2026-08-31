//! Edge types and the append-only edge log (write form, §9.3).

/// A code-graph edge label (§3.3). A small dense integer so it packs into a CSR payload column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeType(pub u16);

impl EdgeType {
  /// The label with resolution confidence packed into the high byte (`0` = structural /
  /// unlabeled). The low byte remains the edge *type*; every type comparison must go
  /// through [`EdgeType::base`].
  pub fn with_confidence(self, confidence: u8) -> EdgeType {
    EdgeType((self.0 & 0x00FF) | ((confidence as u16) << 8))
  }

  /// The bare edge type, confidence stripped — what type dispatch compares.
  pub fn base(self) -> EdgeType {
    EdgeType(self.0 & 0x00FF)
  }

  /// The packed resolution confidence (`0` for structural edges).
  pub fn confidence(self) -> u8 {
    (self.0 >> 8) as u8
  }

  pub const DEFINES: EdgeType = EdgeType(0);
  pub const CALLS: EdgeType = EdgeType(1);
  pub const REFERENCES: EdgeType = EdgeType(2);
  pub const IMPORTS: EdgeType = EdgeType(3);
  pub const IMPLEMENTS: EdgeType = EdgeType(4);
  pub const OF_TYPE: EdgeType = EdgeType(5);
  pub const OVERRIDES: EdgeType = EdgeType(6);
  pub const HAS_METHOD: EdgeType = EdgeType(7);
  pub const HAS_FIELD: EdgeType = EdgeType(8);
  /// Static argument→parameter flow recorded at extraction (G-M3). Reserved here first
  /// (G-M0) so every surface speaks the name before any edge exists; id 9 stays free for a
  /// future structural relation to keep the semantic block contiguous from 10.
  pub const DATA_FLOWS: EdgeType = EdgeType(10);
  /// Temporal coupling from version control: the two files changed together in the
  /// indexed repository's recent history (symmetric; confidence encodes the co-change
  /// count, ×20 capped at 100). Derived at seal from `git log`, never from source text.
  pub const CHANGES_WITH: EdgeType = EdgeType(11);
  /// Near-clone: the two definitions' token shingle sets are estimated ≥ 0.7 Jaccard-similar
  /// by their MinHash sketches (symmetric; confidence = estimated similarity × 100). Derived
  /// at link from extraction-time signatures, never from name resolution.
  pub const SIMILAR_TO: EdgeType = EdgeType(12);
  /// An HTTP client call site whose literal URL uniquely matches a `Route` node's template
  /// (`fetch("/api/users")` → `GET /api/users`): caller → route, directional. Confidence 95
  /// for a literal-exact path, 85 when template parameters absorbed segments. Derived at
  /// link from extraction-time request records; ambiguity refuses, never guesses.
  pub const REQUESTS: EdgeType = EdgeType(13);
  /// An event/message emitter whose literal topic matches a `Channel` node's registration
  /// (`emit("user.created")` → `EVENT user.created`): emitter → channel, directional, one
  /// edge per matching listener registration (pub/sub is one-to-many by design; the
  /// fan-out is capped and counted, never silently truncated). Confidence 90.
  pub const NOTIFIES: EdgeType = EdgeType(14);

  /// The user-facing relation name (the spelling accepted by [`EdgeType::from_name`] and shown
  /// in traversal results). Confidence is ignored — this reports the base type.
  pub fn name(self) -> &'static str {
    match self.base() {
      EdgeType::DEFINES => "defines",
      EdgeType::CALLS => "calls",
      EdgeType::REFERENCES => "references",
      EdgeType::IMPORTS => "imports",
      EdgeType::IMPLEMENTS => "implements",
      EdgeType::OF_TYPE => "of_type",
      EdgeType::OVERRIDES => "overrides",
      EdgeType::HAS_METHOD => "has_method",
      EdgeType::HAS_FIELD => "has_field",
      EdgeType::DATA_FLOWS => "data_flows",
      EdgeType::CHANGES_WITH => "changes_with",
      EdgeType::SIMILAR_TO => "similar_to",
      EdgeType::REQUESTS => "requests",
      EdgeType::NOTIFIES => "notifies",
      _ => "unknown",
    }
  }

  /// Parse a relation name (case-insensitive) into its edge type — the vocabulary a
  /// relation-filtered traversal accepts. `None` for an unknown name.
  pub fn from_name(name: &str) -> Option<EdgeType> {
    match name.to_ascii_lowercase().as_str() {
      "defines" => Some(EdgeType::DEFINES),
      "calls" => Some(EdgeType::CALLS),
      "references" => Some(EdgeType::REFERENCES),
      "imports" => Some(EdgeType::IMPORTS),
      "implements" => Some(EdgeType::IMPLEMENTS),
      "of_type" => Some(EdgeType::OF_TYPE),
      "overrides" => Some(EdgeType::OVERRIDES),
      "has_method" => Some(EdgeType::HAS_METHOD),
      "has_field" => Some(EdgeType::HAS_FIELD),
      "data_flows" => Some(EdgeType::DATA_FLOWS),
      "changes_with" => Some(EdgeType::CHANGES_WITH),
      "similar_to" => Some(EdgeType::SIMILAR_TO),
      "requests" => Some(EdgeType::REQUESTS),
      "notifies" => Some(EdgeType::NOTIFIES),
      _ => None,
    }
  }
}

/// Append-only edge log: struct-of-arrays so a compaction pass streams columns (§9.3, §11.2).
/// Node ids are dense `u32` locators.
#[derive(Debug, Default, Clone)]
pub struct EdgeLog {
  srcs: Vec<u32>,
  dsts: Vec<u32>,
  etypes: Vec<u16>,
}

impl EdgeLog {
  pub fn new() -> Self {
    Self::default()
  }

  /// O(1) append — the write path never buffers the whole repo.
  pub fn push(&mut self, src: u32, dst: u32, etype: EdgeType) {
    self.srcs.push(src);
    self.dsts.push(dst);
    self.etypes.push(etype.0);
  }

  /// Pre-size for a known batch (the bulk drain's per-chunk extend).
  pub fn reserve(&mut self, additional: usize) {
    self.srcs.reserve(additional);
    self.dsts.reserve(additional);
    self.etypes.reserve(additional);
  }

  pub fn len(&self) -> usize {
    self.srcs.len()
  }

  pub fn is_empty(&self) -> bool {
    self.srcs.is_empty()
  }

  pub fn clear(&mut self) {
    self.srcs.clear();
    self.dsts.clear();
    self.etypes.clear();
  }

  /// Truncate to the first `len` edges — the retained-writer link path rolls the log back
  /// to its containment watermark before re-appending fresh resolution edges.
  pub fn truncate(&mut self, len: usize) {
    self.srcs.truncate(len);
    self.dsts.truncate(len);
    self.etypes.truncate(len);
  }

  /// One `(src, dst, etype)` triple by index — random access for range-gathering seals.
  pub fn triple(&self, index: usize) -> (u32, u32, EdgeType) {
    (self.srcs[index], self.dsts[index], EdgeType(self.etypes[index]))
  }

  /// Release growth slack (capacity beyond length) back to the allocator.
  pub fn shrink_to_fit(&mut self) {
    self.srcs.shrink_to_fit();
    self.dsts.shrink_to_fit();
    self.etypes.shrink_to_fit();
  }

  pub(crate) fn srcs(&self) -> &[u32] {
    &self.srcs
  }

  pub(crate) fn dsts(&self) -> &[u32] {
    &self.dsts
  }

  pub(crate) fn etypes(&self) -> &[u16] {
    &self.etypes
  }

  /// Iterate `(src, dst, etype)` triples.
  pub fn iter(&self) -> impl Iterator<Item = (u32, u32, EdgeType)> + '_ {
    (0..self.len()).map(move |i| (self.srcs[i], self.dsts[i], EdgeType(self.etypes[i])))
  }
}

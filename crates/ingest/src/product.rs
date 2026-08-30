//! Per-file extraction products — the incremental-rebuild cache unit (§3.4).
//!
//! A [`FileProduct`] is everything extraction learns from one file: its outline items and its
//! references, the latter keyed by the *entity path* of their enclosing definition (stable
//! across runs) rather than a `NodeId` (assigned per run). Re-indexing re-parses only changed
//! files and replays cached products for unchanged ones; the graph is always re-linked from the
//! complete product set, so removals and renames cannot leave stale nodes or edges behind.
//!
//! The on-disk form is a hand-rolled little-endian binary (`.vpb`), matching the repo's other
//! formats (manifest, edges, ANN): no field names, no escaping, no text numbers — the JSON it
//! replaced carried ~5.7× write amplification and a String-heavy parse on every replay. Every
//! decode is bounds-checked and versioned; any mismatch is an error the incremental path treats
//! as a cache miss.

use std::fs;
use std::io;
use std::path::Path;

use vorpal_outline::model::{
  EntryRole, OutlineEntry, OutlineItem, OutlineMember, SourcePosition, SourceRange, SymbolType,
};
use vorpal_resolve::{RefForm, RefKind};

/// The extraction-product format generation. Bumped whenever extraction output changes shape or
/// semantics (fields, suppression rules, qualifier capture, encoding), so stale caches replay
/// as cache misses and re-parse — staleness is structural, never silent (§3.4). v8 adds the
/// grammar-generation digest to the header; v9 widens the parse-error flag to an error-node
/// count; v10 captures source-module qualifiers on import references (Python `from X import y`,
/// Rust `use a::b::c`); v11 adds affected-byte counts and representative merged error spans
/// (parse health beyond a scalar, IMPROVEMENTS #11); v12 carries the aliased-import local
/// rebinding (`from x import y as z` → alias `z`), so import bindings can key on the name
/// bare uses actually say; v13 adds the MethodHinted form — member-access receivers ride as
/// owner hints (`Foo.bar()` corroborates class Foo) instead of being discarded.
/// v14 (G-M1): refs carry receiver/receiver-type/args; products carry per-entity params.
/// The version check itself is this bump's invalidation — v13 bytes never decode, so every
/// file re-extracts exactly once.
pub const PRODUCT_FORMAT_VERSION: u32 = 16;

/// `(local entity index, [(param name, type text?)])` — see `FileProduct::entity_params`.
pub type EntityParams = Vec<(u32, Vec<(String, Option<String>)>)>;
/// Borrowed twin of [`EntityParams`] for the zero-copy view.
pub type EntityParamsView<'a> = Vec<(u32, Vec<(&'a str, Option<&'a str>)>)>;

/// One file's extraction output, serializable for the on-disk product cache.
#[derive(Debug, Clone)]
pub struct FileProduct {
  /// Format generation this product was extracted under.
  pub version: u32,
  /// Stat identity (size, mtime) of the source bytes this product was extracted from — set by
  /// whoever persists the product. A product replays only while the file's current stat still
  /// matches, making every product **self-validating**: a full index run, an interrupted one,
  /// and a search that banked its matches all produce equally trustworthy cache entries. The
  /// `extract_product` default of `(0, 0)` matches no real file, so an unstamped product can
  /// never replay.
  pub source_size: u64,
  pub source_mtime_ns: u64,
  /// xxh3 of the exact source bytes this product was extracted from — the content identity
  /// behind staged cache validation (stat is the cheap hint; this digest is the truth used
  /// in the racy-mtime window and under `VORPAL_VERIFY_CACHE=1`).
  pub source_xxh3: u64,
  /// The extraction-identity digest this product was produced under: the language's grammar
  /// generation folded with the outline-rule digest (see [`crate::extraction_identity`]). A
  /// product replays only while it still matches, so editing *either* the grammar or the
  /// extraction rules invalidates exactly its stale products. (Field name is historical — it
  /// began as grammar-only in v8.)
  pub grammar_digest: u64,
  /// How many tree-sitter ERROR nodes the parse produced: `0` = clean, higher = worse (a rough
  /// "how bad" signal, not just "did it fail"). Some definitions in this file may be missing from
  /// the graph. Language-agnostic graceful-degradation telemetry — surfaced in
  /// `IndexReport::{error_files, error_nodes}`, never acted on (parsing is tree-sitter's job).
  pub error_nodes: u32,
  /// Total bytes covered by ERROR nodes, after merging nested/overlapping error ranges —
  /// with `source_size`, the covered-byte ratio a health policy thresholds on.
  pub error_bytes: u64,
  /// Up to eight representative merged error spans, document order — enough to LOOK at the
  /// damage without re-parsing.
  pub error_spans: Vec<(u32, u32)>,
  pub items: Vec<OutlineItem<'static>>,
  pub refs: Vec<ProductRef>,
  /// Per-entity parameter lists (G-M1): `(local entity index, [(name, type_text?)])`, sorted
  /// by entity index; captured for the typefacts launch languages, empty elsewhere.
  pub entity_params: EntityParams,
  /// `(function name, declared return type)` — v15, the chained-call return ledger. Name-
/// v16: near-clone signatures — per callable definition a 64-byte MinHash sketch + shingle
/// count (trailing section, absent for definitions under the 32-token floor).
  /// keyed on purpose: link joins it against receiver "types" that are really callee names
  /// (`let x = make(); x.render()`), poisoning same-named functions with disagreeing
  /// returns. Only capture languages with a return annotation produce rows.
  pub returns: Vec<(String, String)>,
  /// Near-clone sketches per signed callable definition (v16), entity-ordered.
  pub signatures: Vec<ProductSignature>,
}

/// A reference keyed by its enclosing definition's position in the file's local layout
/// (index 0 = the file node, then items and members in extraction order — see
/// `local_layout`). An index, not an entity-path string: the strings already live in `items`,
/// and repeating them per reference dominated both the encode size and the apply-time
/// allocation count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductRef {
  pub from_entity_index: u32,
  pub name: String,
  pub kind: u8,
  /// Aliased-import local rebinding (`as z`), when the grammar provides one.
  pub alias: Option<String>,
  pub start: u32,
  pub end: u32,
  /// Grammar-provided qualifier evidence (owner/namespace), when any.
  pub qualifier: Option<String>,
  /// Syntactic form tag (see [`refform_tag`]).
  pub form: u8,
  /// Method-call receiver's simple spelling (`x.helper()` → `x`), when it is a bare name.
  pub receiver: Option<String>,
  /// The receiver's file-locally bound type text, when exactly one unpoisoned binding names
  /// it (G-M1 capture; consumed by typed-receiver resolution in G-M2).
  pub receiver_type: Option<String>,
  /// [`crate::typefacts::BindOrigin`] tag for `receiver_type`; `0xFF` when absent.
  pub receiver_type_origin: u8,
  /// Call-site arguments (G-M1 capture; consumed by data-flow in G-M3).
  pub args: Vec<ProductArg>,
}

/// One definition's near-clone sketch (see `signature.rs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductSignature {
  pub entity_index: u32,
  pub shingles: u32,
  pub sketch: [u8; crate::signature::BINS],
}

/// Borrowed twin of [`ProductSignature`].
#[derive(Clone, Copy)]
pub struct SignatureView<'a> {
  pub entity_index: u32,
  pub shingles: u32,
  pub sketch: &'a [u8],
}

/// One persisted call-site argument (see `references::RawArg`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductArg {
  pub index: u16,
  /// `references::ArgClass` discriminant.
  pub class: u8,
  pub kw_name: Option<String>,
  /// Expression text (≤64 bytes), traceable classes only.
  pub expr: Option<String>,
}

pub(crate) fn refkind_tag(kind: RefKind) -> u8 {
  match kind {
    RefKind::Call => 0,
    RefKind::Type => 1,
    RefKind::Import => 2,
    RefKind::Implements => 3,
    RefKind::Use => 4,
  }
}

pub(crate) fn tag_refkind(tag: u8) -> RefKind {
  match tag {
    1 => RefKind::Type,
    2 => RefKind::Import,
    3 => RefKind::Implements,
    4 => RefKind::Use,
    _ => RefKind::Call,
  }
}

pub(crate) fn refform_tag(form: RefForm) -> u8 {
  match form {
    RefForm::Bare => 0,
    RefForm::Static => 1,
    RefForm::Method => 2,
    RefForm::MethodHinted => 3,
  }
}

pub(crate) fn tag_refform(tag: u8) -> RefForm {
  match tag {
    1 => RefForm::Static,
    2 => RefForm::Method,
    3 => RefForm::MethodHinted,
    _ => RefForm::Bare,
  }
}

/// Detach an extracted item from its parse tree (owned strings) for caching.
pub(crate) fn own_item(item: OutlineItem<'_>) -> OutlineItem<'static> {
  OutlineItem {
    entry: own_entry(item.entry),
    is_import: item.is_import,
    is_exported: item.is_exported,
    members: item
      .members
      .into_iter()
      .map(|member| OutlineMember {
        entry: own_entry(member.entry),
        is_public: member.is_public,
      })
      .collect(),
  }
}

fn own_entry(entry: OutlineEntry<'_>) -> OutlineEntry<'static> {
  OutlineEntry {
    role: entry.role,
    symbol_type: entry.symbol_type,
    name: entry.name.into_owned().into(),
    range: entry.range,
    signature: entry.signature.into_owned().into(),
    ast_kind: std::borrow::Cow::Borrowed(intern_kind(&entry.ast_kind)),
  }
}

/// Intern a grammar node-kind name. Kinds are `'static` data inside tree-sitter grammars, but
/// the type system ties `node.kind()` to the tree lifetime — so detaching an entry used to
/// heap-copy the same few hundred names once per extracted definition. The interner is bounded
/// by the union of kind names across the compiled grammars (a few thousand short strings,
/// leaked once each) and is read-mostly after warmup.
fn intern_kind(name: &str) -> &'static str {
  use std::collections::HashSet;
  use std::sync::{OnceLock, RwLock};
  static KINDS: OnceLock<RwLock<HashSet<&'static str>>> = OnceLock::new();
  let kinds = KINDS.get_or_init(|| RwLock::new(HashSet::new()));
  if let Some(interned) = kinds.read().unwrap().get(name) {
    return interned;
  }
  let mut writable = kinds.write().unwrap();
  if let Some(interned) = writable.get(name) {
    return interned;
  }
  let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
  writable.insert(leaked);
  leaked
}

/// Filename-safe cache key for a source path (`.vpb` = vorpal product binary; files with
/// superseded extensions are swept by the cache-hygiene pass).
pub fn cache_file_name(path: &str) -> String {
  format!("{}.vpb", blake3::hash(path.as_bytes()).to_hex())
}

pub fn save_product(path: &Path, product: &FileProduct) -> io::Result<()> {
  fs::write(path, encode_product(product))
}

/// [`save_product`] with a caller-owned encode buffer (cleared, filled, written) — streaming
/// workers reuse one buffer across every file they process instead of allocating per file
/// (§7.5 per-worker scratch).
pub fn save_product_with(path: &Path, product: &FileProduct, buf: &mut Vec<u8>) -> io::Result<()> {
  buf.clear();
  encode_product_into(product, buf);
  fs::write(path, buf)
}

/// Load a cached product; a product from any other format generation — or any malformed byte
/// stream — is an error, which the incremental path treats as a cache miss (the file
/// re-parses under the current format).
pub fn load_product(path: &Path) -> io::Result<FileProduct> {
  decode_product(&fs::read(path)?)
}

// --- binary codec -----------------------------------------------------------------------

const PRODUCT_MAGIC: &[u8; 4] = b"VPRD";

fn symbol_type_tag(sym: SymbolType) -> u8 {
  // Stable on-disk tags: append-only. Adding a `SymbolType` variant makes this match
  // non-exhaustive, forcing a new tag AND a `PRODUCT_FORMAT_VERSION` bump here.
  match sym {
    SymbolType::File => 0,
    SymbolType::Module => 1,
    SymbolType::Namespace => 2,
    SymbolType::Package => 3,
    SymbolType::Class => 4,
    SymbolType::Method => 5,
    SymbolType::Property => 6,
    SymbolType::Field => 7,
    SymbolType::Constructor => 8,
    SymbolType::Enum => 9,
    SymbolType::Interface => 10,
    SymbolType::Function => 11,
    SymbolType::Variable => 12,
    SymbolType::Constant => 13,
    SymbolType::String => 14,
    SymbolType::Number => 15,
    SymbolType::Boolean => 16,
    SymbolType::Array => 17,
    SymbolType::Object => 18,
    SymbolType::Key => 19,
    SymbolType::Null => 20,
    SymbolType::EnumMember => 21,
    SymbolType::Struct => 22,
    SymbolType::Event => 23,
    SymbolType::Operator => 24,
    SymbolType::TypeParameter => 25,
  }
}

fn tag_symbol_type(tag: u8) -> io::Result<SymbolType> {
  Ok(match tag {
    0 => SymbolType::File,
    1 => SymbolType::Module,
    2 => SymbolType::Namespace,
    3 => SymbolType::Package,
    4 => SymbolType::Class,
    5 => SymbolType::Method,
    6 => SymbolType::Property,
    7 => SymbolType::Field,
    8 => SymbolType::Constructor,
    9 => SymbolType::Enum,
    10 => SymbolType::Interface,
    11 => SymbolType::Function,
    12 => SymbolType::Variable,
    13 => SymbolType::Constant,
    14 => SymbolType::String,
    15 => SymbolType::Number,
    16 => SymbolType::Boolean,
    17 => SymbolType::Array,
    18 => SymbolType::Object,
    19 => SymbolType::Key,
    20 => SymbolType::Null,
    21 => SymbolType::EnumMember,
    22 => SymbolType::Struct,
    23 => SymbolType::Event,
    24 => SymbolType::Operator,
    25 => SymbolType::TypeParameter,
    other => return Err(corrupt(format!("unknown symbol type tag {other}"))),
  })
}

fn role_tag(role: EntryRole) -> u8 {
  match role {
    EntryRole::Item => 0,
    EntryRole::Member => 1,
  }
}

fn tag_role(tag: u8) -> io::Result<EntryRole> {
  Ok(match tag {
    0 => EntryRole::Item,
    1 => EntryRole::Member,
    other => return Err(corrupt(format!("unknown entry role tag {other}"))),
  })
}

fn corrupt(msg: impl Into<String>) -> io::Error {
  io::Error::other(msg.into())
}

fn push_u32(buf: &mut Vec<u8>, v: u32) {
  buf.extend_from_slice(&v.to_le_bytes());
}

fn push_str(buf: &mut Vec<u8>, s: &str) {
  push_u32(buf, s.len() as u32);
  buf.extend_from_slice(s.as_bytes());
}

/// Positions are `usize` in the model but tree-sitter itself is u32-bounded; encode with a
/// checked cast so an unrepresentable in-memory value can never truncate silently.
fn push_pos(buf: &mut Vec<u8>, v: usize) -> io::Result<()> {
  let v = u32::try_from(v).map_err(|_| corrupt("position exceeds u32"))?;
  push_u32(buf, v);
  Ok(())
}

fn push_entry(buf: &mut Vec<u8>, entry: &OutlineEntry<'_>) -> io::Result<()> {
  buf.push(role_tag(entry.role));
  buf.push(symbol_type_tag(entry.symbol_type));
  push_str(buf, &entry.name);
  push_pos(buf, entry.range.byte_offset.start)?;
  push_pos(buf, entry.range.byte_offset.end)?;
  push_pos(buf, entry.range.start.line)?;
  push_pos(buf, entry.range.start.column)?;
  push_pos(buf, entry.range.end.line)?;
  push_pos(buf, entry.range.end.column)?;
  push_str(buf, &entry.signature);
  push_str(buf, &entry.ast_kind);
  Ok(())
}

/// Encode a product into its `.vpb` byte form. Position casts cannot fail for real
/// extractions (tree-sitter is u32-bounded); a defensively caught overflow yields an empty
/// buffer, which can never decode — corruption is impossible, silent truncation more so.
fn encode_product(product: &FileProduct) -> Vec<u8> {
  let mut buf = Vec::with_capacity(256 + product.refs.len() * 48);
  encode_product_into(product, &mut buf);
  buf
}

/// [`encode_product`] into a caller-owned buffer (appended; clear first for a fresh encoding).
pub fn encode_product_into(product: &FileProduct, buf: &mut Vec<u8>) {
  let rollback = buf.len();
  buf.extend_from_slice(PRODUCT_MAGIC);
  push_u32(buf, product.version);
  buf.extend_from_slice(&product.source_size.to_le_bytes());
  buf.extend_from_slice(&product.source_mtime_ns.to_le_bytes());
  buf.extend_from_slice(&product.source_xxh3.to_le_bytes());
  buf.extend_from_slice(&product.grammar_digest.to_le_bytes());
  push_u32(buf, product.error_nodes);
  buf.extend_from_slice(&product.error_bytes.to_le_bytes());
  push_u32(buf, product.error_spans.len() as u32);
  for &(start, end) in &product.error_spans {
    push_u32(buf, start);
    push_u32(buf, end);
  }
  push_u32(buf, product.items.len() as u32);
  for item in &product.items {
    if push_entry(buf, &item.entry).is_err() {
      buf.truncate(rollback);
      return;
    }
    buf.push(u8::from(item.is_import) | (u8::from(item.is_exported) << 1));
    push_u32(buf, item.members.len() as u32);
    for member in &item.members {
      if push_entry(buf, &member.entry).is_err() {
        buf.truncate(rollback);
        return;
      }
      buf.push(u8::from(member.is_public));
    }
  }
  push_u32(buf, product.refs.len() as u32);
  for r in &product.refs {
    push_u32(buf, r.from_entity_index);
    push_str(buf, &r.name);
    buf.push(r.kind);
    push_u32(buf, r.start);
    push_u32(buf, r.end);
    buf.push(r.form);
    match &r.qualifier {
      Some(q) => {
        buf.push(1);
        push_str(buf, q);
      }
      None => buf.push(0),
    }
    match &r.alias {
      Some(a) => {
        buf.push(1);
        push_str(buf, a);
      }
      None => buf.push(0),
    }
    // Extras presence flags: bit0 receiver, bit1 receiver_type (+origin byte), bit2 args
    // (+u16 count). The overwhelmingly common no-extras ref costs ONE byte, not five.
    let flags = u8::from(r.receiver.is_some())
      | (u8::from(r.receiver_type.is_some()) << 1)
      | (u8::from(!r.args.is_empty()) << 2);
    buf.push(flags);
    if let Some(v) = &r.receiver {
      push_str(buf, v);
    }
    if let Some(v) = &r.receiver_type {
      push_str(buf, v);
      buf.push(r.receiver_type_origin);
    }
    if !r.args.is_empty() {
      buf.extend_from_slice(&(r.args.len() as u16).to_le_bytes());
    }
    for arg in &r.args {
      buf.extend_from_slice(&arg.index.to_le_bytes());
      buf.push(arg.class);
      match &arg.kw_name {
        Some(v) => {
          buf.push(1);
          push_str(buf, v);
        }
        None => buf.push(0),
      }
      match &arg.expr {
        Some(v) => {
          buf.push(1);
          push_str(buf, v);
        }
        None => buf.push(0),
      }
    }
  }
  push_u32(buf, product.entity_params.len() as u32);
  for (entity, params) in &product.entity_params {
    push_u32(buf, *entity);
    push_u32(buf, params.len() as u32);
    for (name, ty) in params {
      push_str(buf, name);
      match ty {
        Some(t) => {
          buf.push(1);
          push_str(buf, t);
        }
        None => buf.push(0),
      }
    }
  }
  push_u32(buf, product.returns.len() as u32);
  for (name, ret) in &product.returns {
    push_str(buf, name);
    push_str(buf, ret);
  }
  push_u32(buf, product.signatures.len() as u32);
  for sig in &product.signatures {
    push_u32(buf, sig.entity_index);
    push_u32(buf, sig.shingles);
    buf.extend_from_slice(&sig.sketch);
  }
}

struct Reader<'a> {
  bytes: &'a [u8],
  off: usize,
}

impl<'a> Reader<'a> {
  fn take(&mut self, len: usize) -> io::Result<&'a [u8]> {
    let end = self
      .off
      .checked_add(len)
      .ok_or_else(|| corrupt("truncated product"))?;
    let slice = self
      .bytes
      .get(self.off..end)
      .ok_or_else(|| corrupt("truncated product"))?;
    self.off = end;
    Ok(slice)
  }

  fn u8(&mut self) -> io::Result<u8> {
    Ok(self.take(1)?[0])
  }

  fn u16(&mut self) -> io::Result<u16> {
    Ok(u16::from_le_bytes(self.take(2)?.try_into().expect("2B")))
  }

  fn u32(&mut self) -> io::Result<u32> {
    Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4B")))
  }

  fn u64(&mut self) -> io::Result<u64> {
    Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("8B")))
  }

  fn str(&mut self) -> io::Result<String> {
    Ok(self.str_borrowed()?.to_owned())
  }

  /// Borrowed variant for strings that get interned rather than owned (no temp allocation).
  fn str_borrowed(&mut self) -> io::Result<&'a str> {
    let len = self.u32()? as usize;
    let bytes = self.take(len)?;
    std::str::from_utf8(bytes).map_err(|_| corrupt("product string is not UTF-8"))
  }

  /// Bounded element count for `Vec::with_capacity` — a corrupt count must not allocate
  /// unboundedly before the per-element reads fail.
  fn count(&mut self) -> io::Result<usize> {
    let n = self.u32()? as usize;
    if n > self.bytes.len().saturating_sub(self.off) {
      return Err(corrupt("product count exceeds remaining bytes"));
    }
    Ok(n)
  }

  fn entry(&mut self) -> io::Result<OutlineEntry<'static>> {
    let entry = self.entry_view()?;
    let ast_kind: &'static str = match entry.ast_kind {
      std::borrow::Cow::Borrowed(kind) => intern_kind(kind),
      std::borrow::Cow::Owned(kind) => intern_kind(&kind),
    };
    Ok(OutlineEntry {
      role: entry.role,
      symbol_type: entry.symbol_type,
      name: entry.name.into_owned().into(),
      range: entry.range,
      signature: entry.signature.into_owned().into(),
      ast_kind: std::borrow::Cow::Borrowed(ast_kind),
    })
  }

  /// [`Reader::entry`] borrowing name/signature straight from the encoded bytes — the
  /// pack-replay decode, which materializes no strings at all (`ast_kind` was already an
  /// interned static).
  fn entry_view(&mut self) -> io::Result<OutlineEntry<'a>> {
    let role = tag_role(self.u8()?)?;
    let symbol_type = tag_symbol_type(self.u8()?)?;
    let name = self.str_borrowed()?;
    let byte_start = self.u32()? as usize;
    let byte_end = self.u32()? as usize;
    let start = SourcePosition {
      line: self.u32()? as usize,
      column: self.u32()? as usize,
    };
    let end = SourcePosition {
      line: self.u32()? as usize,
      column: self.u32()? as usize,
    };
    let signature = self.str_borrowed()?;
    let ast_kind = intern_kind(self.str_borrowed()?);
    Ok(OutlineEntry {
      role,
      symbol_type,
      name: std::borrow::Cow::Borrowed(name),
      range: SourceRange {
        byte_offset: byte_start..byte_end,
        start,
        end,
      },
      signature: std::borrow::Cow::Borrowed(signature),
      ast_kind: std::borrow::Cow::Borrowed(ast_kind),
    })
  }
}

/// A product decoded as **views into its encoded bytes** — the pack-replay form. No owned
/// strings: names, signatures, and qualifiers borrow the mapped pack; only the small view
/// vectors allocate. Item strings ride in `OutlineItem<'a>`'s existing `Cow` lifetime.
pub struct ProductView<'a> {
  pub source_size: u64,
  pub source_mtime_ns: u64,
  pub source_xxh3: u64,
  pub grammar_digest: u64,
  pub error_nodes: u32,
  pub error_bytes: u64,
  pub error_spans: Vec<(u32, u32)>,
  pub items: Vec<OutlineItem<'a>>,
  pub refs: Vec<RefView<'a>>,
  /// Per-entity parameter lists, borrowed (see `FileProduct::entity_params`).
  pub entity_params: EntityParamsView<'a>,
  /// Borrowed twin of `FileProduct::returns` (v15).
  pub returns: Vec<(&'a str, &'a str)>,
  /// Borrowed twin of `FileProduct::signatures` (v16).
  pub signatures: Vec<SignatureView<'a>>,
}

/// One reference occurrence as a borrowed view (see [`ProductRef`] for field semantics).
/// Receiver typing is decoded eagerly (two option-tagged slices — no allocation); the
/// argument records stay as raw encoded bytes decoded on demand via [`RefView::args`] — the
/// replay path applies millions of refs and must never pay for records it doesn't read.
#[derive(Clone, Copy)]
pub struct RefView<'a> {
  pub from_entity_index: u32,
  pub name: &'a str,
  pub kind: u8,
  pub start: u32,
  pub end: u32,
  pub qualifier: Option<&'a str>,
  pub form: u8,
  pub alias: Option<&'a str>,
  pub receiver: Option<&'a str>,
  pub receiver_type: Option<&'a str>,
  pub receiver_type_origin: u8,
  /// Argument records: encoded bytes (the replay path, decoded lazily) or a borrow of the
  /// owned records (the just-parsed bridge) — one accessor serves both.
  args: ArgsSrc<'a>,
}

#[derive(Clone, Copy)]
enum ArgsSrc<'a> {
  Encoded { bytes: &'a [u8], count: u16 },
  Owned(&'a [ProductArg]),
}

/// One decoded argument view.
#[derive(Clone, Copy)]
pub struct ArgView<'a> {
  pub index: u16,
  pub class: u8,
  pub kw_name: Option<&'a str>,
  pub expr: Option<&'a str>,
}

impl<'a> RefView<'a> {
  /// Bridge an owned ref into the shared apply path, argument records included (borrowed).
  #[allow(clippy::too_many_arguments)]
  pub(crate) fn bridge(
    from_entity_index: u32,
    name: &'a str,
    kind: u8,
    start: u32,
    end: u32,
    qualifier: Option<&'a str>,
    form: u8,
    alias: Option<&'a str>,
    receiver: Option<&'a str>,
    receiver_type: Option<&'a str>,
    receiver_type_origin: u8,
    args: &'a [ProductArg],
  ) -> Self {
    Self {
      from_entity_index,
      name,
      kind,
      start,
      end,
      qualifier,
      form,
      alias,
      receiver,
      receiver_type,
      receiver_type_origin,
      args: ArgsSrc::Owned(args),
    }
  }

  /// Decode the argument records on demand. Errors surface as an early end (the encoder and
  /// the eager decoder guarantee well-formed bytes; a torn read yields fewer records, never
  /// junk).
  pub fn args(&self) -> impl Iterator<Item = ArgView<'a>> + 'a {
    let (bytes, count, owned): (&'a [u8], u16, &'a [ProductArg]) = match self.args {
      ArgsSrc::Encoded { bytes, count } => (bytes, count, &[]),
      ArgsSrc::Owned(records) => (&[], 0, records),
    };
    let mut r = Reader { bytes, off: 0 };
    let encoded = (0..count).map_while(move |_| {
      let index = r.u16().ok()?;
      let class = r.u8().ok()?;
      let kw_name = if r.u8().ok()? != 0 {
        Some(r.str_borrowed().ok()?)
      } else {
        None
      };
      let expr = if r.u8().ok()? != 0 {
        Some(r.str_borrowed().ok()?)
      } else {
        None
      };
      Some(ArgView {
        index,
        class,
        kw_name,
        expr,
      })
    });
    encoded.chain(owned.iter().map(|arg| ArgView {
      index: arg.index,
      class: arg.class,
      kw_name: arg.kw_name.as_deref(),
      expr: arg.expr.as_deref(),
    }))
  }

  pub fn args_len(&self) -> usize {
    match self.args {
      ArgsSrc::Encoded { count, .. } => count as usize,
      ArgsSrc::Owned(records) => records.len(),
    }
  }
}

/// The stat stamp of an encoded product, read from its fixed header — magic and format
/// version checked, nothing decoded. `None` for foreign or torn bytes (treat as cache miss).
pub fn peek_product_stamps(bytes: &[u8]) -> Option<(u64, u64)> {
  if bytes.len() < 44 || &bytes[0..4] != PRODUCT_MAGIC {
    return None;
  }
  if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != PRODUCT_FORMAT_VERSION {
    return None;
  }
  Some((
    u64::from_le_bytes(bytes[8..16].try_into().ok()?),
    u64::from_le_bytes(bytes[16..24].try_into().ok()?),
  ))
}

/// The content digest (`xxh3` of the source bytes) from the product header — the identity
/// staged validation compares when stat alone cannot be trusted.
pub fn peek_product_digest(bytes: &[u8]) -> Option<u64> {
  if bytes.len() < 44 || &bytes[0..4] != PRODUCT_MAGIC {
    return None;
  }
  if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != PRODUCT_FORMAT_VERSION {
    return None;
  }
  Some(u64::from_le_bytes(bytes[24..32].try_into().ok()?))
}

/// The grammar-generation digest from a v8 product header (offset 32) — the identity that
/// invalidates products whose grammar has since been edited/bumped.
pub fn peek_product_grammar_digest(bytes: &[u8]) -> Option<u64> {
  if bytes.len() < 44 || &bytes[0..4] != PRODUCT_MAGIC {
    return None;
  }
  if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != PRODUCT_FORMAT_VERSION {
    return None;
  }
  Some(u64::from_le_bytes(bytes[32..40].try_into().ok()?))
}

/// The residual parse-error node count a cached product recorded (v9 header, offset 40..44).
/// `0` = clean; the replay paths sum it into `IndexReport::error_nodes` and count files with
/// any errors into `error_files`.
pub fn peek_product_error_nodes(bytes: &[u8]) -> Option<u32> {
  if bytes.len() < 44 || &bytes[0..4] != PRODUCT_MAGIC {
    return None;
  }
  if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != PRODUCT_FORMAT_VERSION {
    return None;
  }
  Some(u32::from_le_bytes(bytes[40..44].try_into().ok()?))
}

/// Validate an encoded product without materializing anything: the exact walk
/// [`decode_product_view`] performs — every bounds check, tag check, and UTF-8 check —
/// with zero allocations. The pack-replay producer runs this so a corrupt entry still
/// falls back to a re-parse, while the (validated) real decode happens once, at apply.
/// The affected-byte count from a v11 header (offset 44..52) — replay paths sum it into
/// `IndexReport::error_bytes` beside the node count.
pub fn peek_product_error_bytes(bytes: &[u8]) -> Option<u64> {
  if bytes.len() < 52 || &bytes[0..4] != PRODUCT_MAGIC {
    return None;
  }
  if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != PRODUCT_FORMAT_VERSION {
    return None;
  }
  Some(u64::from_le_bytes(bytes[44..52].try_into().ok()?))
}

pub fn validate_product(bytes: &[u8]) -> bool {
  // ONE decoder. This used to be a hand-rolled byte walker mirroring the layout — and it
  // silently fell behind the v14/v15 reference extras (flags byte, args, entity params,
  // returns): every product with references failed validation and re-parsed, so incremental
  // builds re-extracted ~80% of the tree for weeks while cold builds looked fine. The view
  // decoder is exact by construction and allocates only the item/ref vectors.
  decode_product_view(bytes).is_ok()
}

/// Decode a product as views over `bytes` (see [`ProductView`]). Same validation as
/// [`decode_product`]; allocates only the item/ref vectors.
pub fn decode_product_view(bytes: &[u8]) -> io::Result<ProductView<'_>> {
  let mut r = Reader { bytes, off: 0 };
  if r.take(4)? != PRODUCT_MAGIC {
    return Err(corrupt("bad product magic"));
  }
  let version = r.u32()?;
  if version != PRODUCT_FORMAT_VERSION {
    return Err(corrupt("product from a different format generation"));
  }
  let source_size = r.u64()?;
  let source_mtime_ns = r.u64()?;
  let source_xxh3 = r.u64()?;
  let grammar_digest = r.u64()?;
  let error_nodes = r.u32()?;
  let error_bytes = r.u64()?;
  let span_count = r.count()?;
  let mut error_spans = Vec::with_capacity(span_count.min(16));
  for _ in 0..span_count {
    error_spans.push((r.u32()?, r.u32()?));
  }
  let item_count = r.count()?;
  let mut items = Vec::with_capacity(item_count);
  for _ in 0..item_count {
    let entry = r.entry_view()?;
    let flags = r.u8()?;
    let member_count = r.count()?;
    let mut members = Vec::with_capacity(member_count);
    for _ in 0..member_count {
      let entry = r.entry_view()?;
      let is_public = r.u8()? & 1 != 0;
      members.push(OutlineMember { entry, is_public });
    }
    items.push(OutlineItem {
      entry,
      is_import: flags & 1 != 0,
      is_exported: flags & 2 != 0,
      members,
    });
  }
  let ref_count = r.count()?;
  let mut refs = Vec::with_capacity(ref_count);
  for _ in 0..ref_count {
    let from_entity_index = r.u32()?;
    let name = r.str_borrowed()?;
    let kind = r.u8()?;
    let start = r.u32()?;
    let end = r.u32()?;
    let form = r.u8()?;
    let qualifier = if r.u8()? != 0 {
      Some(r.str_borrowed()?)
    } else {
      None
    };
    let alias = if r.u8()? != 0 {
      Some(r.str_borrowed()?)
    } else {
      None
    };
    let flags = r.u8()?;
    let receiver = if flags & 1 != 0 {
      Some(r.str_borrowed()?)
    } else {
      None
    };
    let (receiver_type, receiver_type_origin) = if flags & 2 != 0 {
      (Some(r.str_borrowed()?), r.u8()?)
    } else {
      (None, 0xFF)
    };
    // Args stay encoded: remember the region, skip past it.
    let args_count = if flags & 4 != 0 { r.u16()? } else { 0 };
    let args_start = r.off;
    for _ in 0..args_count {
      r.u16()?;
      r.u8()?;
      if r.u8()? != 0 {
        r.str_borrowed()?;
      }
      if r.u8()? != 0 {
        r.str_borrowed()?;
      }
    }
    let args_bytes = &bytes[args_start..r.off];
    refs.push(RefView {
      from_entity_index,
      name,
      kind,
      start,
      end,
      qualifier,
      form,
      alias,
      receiver,
      receiver_type,
      receiver_type_origin,
      args: ArgsSrc::Encoded {
        bytes: args_bytes,
        count: args_count,
      },
    });
  }
  let entity_param_count = r.count()?;
  let mut entity_params = Vec::with_capacity(entity_param_count.min(1024));
  for _ in 0..entity_param_count {
    let entity = r.u32()?;
    let param_count = r.count()?;
    let mut params = Vec::with_capacity(param_count.min(64));
    for _ in 0..param_count {
      let name = r.str_borrowed()?;
      let ty = if r.u8()? != 0 {
        Some(r.str_borrowed()?)
      } else {
        None
      };
      params.push((name, ty));
    }
    entity_params.push((entity, params));
  }
  let return_count = r.count()?;
  let mut returns = Vec::with_capacity(return_count.min(1024));
  for _ in 0..return_count {
    let name = r.str_borrowed()?;
    let ret = r.str_borrowed()?;
    returns.push((name, ret));
  }
  let signature_count = r.count()?;
  let mut signatures = Vec::with_capacity(signature_count.min(1024));
  for _ in 0..signature_count {
    let entity_index = r.u32()?;
    let shingles = r.u32()?;
    let sketch = r.take(crate::signature::BINS)?;
    signatures.push(SignatureView {
      entity_index,
      shingles,
      sketch,
    });
  }
  if r.off != bytes.len() {
    return Err(corrupt("trailing bytes after product"));
  }
  Ok(ProductView {
    source_size,
    source_mtime_ns,
    source_xxh3,
    grammar_digest,
    error_nodes,
    error_bytes,
    error_spans,
    items,
    refs,
    entity_params,
    returns,
    signatures,
  })
}

pub fn decode_product(bytes: &[u8]) -> io::Result<FileProduct> {
  let mut r = Reader { bytes, off: 0 };
  if r.take(4)? != PRODUCT_MAGIC {
    return Err(corrupt("bad product magic"));
  }
  let version = r.u32()?;
  if version != PRODUCT_FORMAT_VERSION {
    return Err(corrupt("product from a different format generation"));
  }
  let source_size = r.u64()?;
  let source_mtime_ns = r.u64()?;
  let source_xxh3 = r.u64()?;
  let grammar_digest = r.u64()?;
  let error_nodes = r.u32()?;
  let error_bytes = r.u64()?;
  let span_count = r.count()?;
  let mut error_spans = Vec::with_capacity(span_count.min(16));
  for _ in 0..span_count {
    error_spans.push((r.u32()?, r.u32()?));
  }
  let item_count = r.count()?;
  let mut items = Vec::with_capacity(item_count);
  for _ in 0..item_count {
    let entry = r.entry()?;
    let flags = r.u8()?;
    let member_count = r.count()?;
    let mut members = Vec::with_capacity(member_count);
    for _ in 0..member_count {
      let entry = r.entry()?;
      let is_public = r.u8()? & 1 != 0;
      members.push(OutlineMember { entry, is_public });
    }
    items.push(OutlineItem {
      entry,
      is_import: flags & 1 != 0,
      is_exported: flags & 2 != 0,
      members,
    });
  }
  let ref_count = r.count()?;
  let mut refs = Vec::with_capacity(ref_count);
  for _ in 0..ref_count {
    let from_entity_index = r.u32()?;
    let name = r.str()?;
    let kind = r.u8()?;
    let start = r.u32()?;
    let end = r.u32()?;
    let form = r.u8()?;
    let qualifier = if r.u8()? != 0 { Some(r.str()?) } else { None };
    let alias = if r.u8()? != 0 { Some(r.str()?) } else { None };
    let flags = r.u8()?;
    let receiver = if flags & 1 != 0 { Some(r.str()?) } else { None };
    let (receiver_type, receiver_type_origin) = if flags & 2 != 0 {
      (Some(r.str()?), r.u8()?)
    } else {
      (None, 0xFF)
    };
    let arg_count = if flags & 4 != 0 { r.u16()? as usize } else { 0 };
    let mut args = Vec::with_capacity(arg_count.min(64));
    for _ in 0..arg_count {
      let index = r.u16()?;
      let class = r.u8()?;
      let kw_name = if r.u8()? != 0 { Some(r.str()?) } else { None };
      let expr = if r.u8()? != 0 { Some(r.str()?) } else { None };
      args.push(ProductArg {
        index,
        class,
        kw_name,
        expr,
      });
    }
    refs.push(ProductRef {
      from_entity_index,
      name,
      kind,
      start,
      end,
      qualifier,
      form,
      alias,
      receiver,
      receiver_type,
      receiver_type_origin,
      args,
    });
  }
  let entity_param_count = r.count()?;
  let mut entity_params = Vec::with_capacity(entity_param_count.min(1024));
  for _ in 0..entity_param_count {
    let entity = r.u32()?;
    let param_count = r.count()?;
    let mut params = Vec::with_capacity(param_count.min(64));
    for _ in 0..param_count {
      let name = r.str()?;
      let ty = if r.u8()? != 0 { Some(r.str()?) } else { None };
      params.push((name, ty));
    }
    entity_params.push((entity, params));
  }
  let return_count = r.count()?;
  let mut returns = Vec::with_capacity(return_count.min(1024));
  for _ in 0..return_count {
    let name = r.str()?;
    let ret = r.str()?;
    returns.push((name, ret));
  }
  let signature_count = r.count()?;
  let mut signatures = Vec::with_capacity(signature_count.min(1024));
  for _ in 0..signature_count {
    let entity_index = r.u32()?;
    let shingles = r.u32()?;
    let sketch: [u8; crate::signature::BINS] = r
      .take(crate::signature::BINS)?
      .try_into()
      .map_err(|_| corrupt("signature sketch width"))?;
    signatures.push(ProductSignature {
      entity_index,
      shingles,
      sketch,
    });
  }
  if r.off != bytes.len() {
    return Err(corrupt("trailing bytes after product"));
  }
  Ok(FileProduct {
    version,
    source_size,
    source_mtime_ns,
    source_xxh3,
    grammar_digest,
    error_nodes,
    error_bytes,
    error_spans,
    items,
    refs,
    entity_params,
    returns,
    signatures,
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::OutlineExtractor;
  use vorpal_language::SupportLang;

  /// Structural equality for products (the model types don't derive `PartialEq`).
  fn assert_products_equal(a: &FileProduct, b: &FileProduct) {
    assert_eq!(a.version, b.version);
    assert_eq!(a.source_size, b.source_size);
    assert_eq!(a.source_mtime_ns, b.source_mtime_ns);
    assert_eq!(a.refs, b.refs);
    assert_eq!(a.items.len(), b.items.len());
    for (x, y) in a.items.iter().zip(&b.items) {
      assert_entries_equal(&x.entry, &y.entry);
      assert_eq!(x.is_import, y.is_import);
      assert_eq!(x.is_exported, y.is_exported);
      assert_eq!(x.members.len(), y.members.len());
      for (m, n) in x.members.iter().zip(&y.members) {
        assert_entries_equal(&m.entry, &n.entry);
        assert_eq!(m.is_public, n.is_public);
      }
    }
  }

  fn assert_entries_equal(a: &OutlineEntry<'_>, b: &OutlineEntry<'_>) {
    assert_eq!(a.role, b.role);
    assert_eq!(a.symbol_type, b.symbol_type);
    assert_eq!(a.name, b.name);
    assert_eq!(a.range, b.range);
    assert_eq!(a.signature, b.signature);
    assert_eq!(a.ast_kind, b.ast_kind);
  }

  #[test]
  fn round_trips_real_extractions_bit_exactly() {
    let extractor = OutlineExtractor::new().expect("rules compile");
    let sources: &[(&str, &str)] = &[
      (
        "a.rs",
        "impl<T: Clone> Kg<T> {\n  pub fn löad(&self) -> Self {\n    self.hélper();\n    Vec::new()\n  }\n}\nuse std::fs;\n",
      ),
      (
        "b.ts",
        "import { x } from \"./util\";\nexport class Ünïcode extends Base { m() { this.n(); } }\n",
      ),
      (
        "c.py",
        "from util import helper\nclass A:\n  def m(self):\n    helper()\n",
      ),
      ("d.json", "{\"key\": [1, 2]}\n"),
      ("empty.rs", ""),
    ];
    let _ = SupportLang::Rust; // anchor the language crate for the extractor's dispatch
    for (path, source) in sources {
      let Some(mut product) = extractor.extract_product(path, source) else {
        continue;
      };
      // Stamp as a persisting caller would; extreme values must survive the trip.
      product.source_size = source.len() as u64;
      product.source_mtime_ns = u64::MAX - 1;
      let bytes = encode_product(&product);
      let decoded = decode_product(&bytes).expect("round trip decodes");
      assert_products_equal(&product, &decoded);
      // Encoding is deterministic.
      assert_eq!(bytes, encode_product(&decoded), "{path}");
    }
  }

  #[test]
  fn rejects_corruption_truncation_and_foreign_versions() {
    let extractor = OutlineExtractor::new().expect("rules compile");
    let product = extractor
      .extract_product("t.rs", "pub fn a() { b(); }\nstruct S { f: Vec<u8> }\n")
      .expect("extracts");
    let bytes = encode_product(&product);
    assert!(decode_product(&bytes).is_ok());

    // Every strict prefix must error, never panic.
    for len in 0..bytes.len() {
      assert!(
        decode_product(&bytes[..len]).is_err(),
        "prefix {len} decoded"
      );
    }
    // Trailing garbage is rejected (a torn write can duplicate tails).
    let mut long = bytes.clone();
    long.push(0);
    assert!(decode_product(&long).is_err());
    // Wrong magic and wrong version are cache misses, not panics.
    let mut wrong_magic = bytes.clone();
    wrong_magic[0] ^= 0xFF;
    assert!(decode_product(&wrong_magic).is_err());
    let mut wrong_version = bytes.clone();
    wrong_version[4] ^= 0xFF;
    assert!(decode_product(&wrong_version).is_err());
    // A corrupt huge count fails fast instead of allocating unboundedly. The error-span
    // count sits after magic(4) + version(4) + stat stamp(16) + digest(8) +
    // grammar_digest(8) + error_nodes(4) + error_bytes(8) — v11 header layout.
    let mut huge_count = bytes.clone();
    huge_count[52..56].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(decode_product(&huge_count).is_err());
  }
}

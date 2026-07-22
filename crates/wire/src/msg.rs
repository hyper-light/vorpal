//! The message set carried in frame payloads (postcard-encoded).
//!
//! Every message is `Serialize`/`Deserialize` and, crucially, **byte-stable**: no `HashMap` appears
//! in any type whose bytes are hashed or compared — ordered collections (`Vec`, `BTreeMap`) only —
//! so encodings are reproducible across builds and machines (I3).
//!
//! The rule set is carried as **canonicalized opaque bytes + a `blake3` digest**, not as the
//! `vorpal-config` rule structs. Those structs are std-bound and live in a crate that depends on
//! this one; embedding them would invert the dependency graph. Shipping bytes also makes the digest
//! exactly the bytes that were shipped — the coordinator hashes once, the agent verifies against the
//! received bytes, and neither re-serializes (which is what makes the digest trustworthy given the
//! `HashMap`s inside the rule types).
//!
//! Fidelity note: every field that a printer *bakes into rendered output* is shipped **resolved**
//! (booleans, not "auto") — the coordinator owns the terminal, so tty/env-dependent decisions are
//! made there and the agent reproduces them bit-for-bit (§1 `PrinterSpec`).

use serde::{Deserialize, Serialize};

use crate::frame::Frame;
use crate::hash::digest32;

/// Discriminants mirrored into `FrameHeader.msg_type` so a receiver can route — or skip — a frame
/// without decoding its body. Stable: append only, never renumber.
pub mod msg_type {
  pub const HELLO: u16 = 1;
  pub const WELCOME: u16 = 2;
  pub const JOB: u16 = 3;
  pub const ASSIGN: u16 = 4;
  pub const RESULT: u16 = 5;
  pub const CONTROL: u16 = 6;
  pub const TELEMETRY: u16 = 7;
  pub const DONE: u16 = 8;
  pub const BYE: u16 = 9;
}

/// A protocol message. One variant per `msg_type`; the tag is redundant with the header's
/// `msg_type` but kept so a `Message` is self-describing off the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Message {
  Hello(Hello),
  Welcome(Welcome),
  Job(JobSpec),
  Assign(Assign),
  Result(ResultFrame),
  Control(Control),
  Telemetry(Telemetry),
  Done(Done),
  Bye(RemoteError),
}

impl Message {
  /// The `msg_type` discriminant for this message (mirrored into the frame header).
  pub fn msg_type(&self) -> u16 {
    match self {
      Message::Hello(_) => msg_type::HELLO,
      Message::Welcome(_) => msg_type::WELCOME,
      Message::Job(_) => msg_type::JOB,
      Message::Assign(_) => msg_type::ASSIGN,
      Message::Result(_) => msg_type::RESULT,
      Message::Control(_) => msg_type::CONTROL,
      Message::Telemetry(_) => msg_type::TELEMETRY,
      Message::Done(_) => msg_type::DONE,
      Message::Bye(_) => msg_type::BYE,
    }
  }

  /// Encode as a postcard body.
  pub fn encode(&self) -> Result<Vec<u8>, crate::WireError> {
    postcard::to_stdvec(self).map_err(|e| crate::WireError::Codec(e.to_string()))
  }

  /// Decode a postcard body.
  pub fn decode(bytes: &[u8]) -> Result<Message, crate::WireError> {
    postcard::from_bytes(bytes).map_err(|e| crate::WireError::Codec(e.to_string()))
  }

  /// Encode into a control-channel [`Frame`] ready for the wire.
  pub fn to_frame(&self, channel: u16) -> Result<Frame, crate::WireError> {
    Ok(Frame::new(channel, self.msg_type(), self.encode()?))
  }
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

/// A `major.minor.patch` triple, compared exactly for the protocol/version gate (I2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemVer {
  pub major: u16,
  pub minor: u16,
  pub patch: u16,
}

impl SemVer {
  pub const fn new(major: u16, minor: u16, patch: u16) -> Self {
    Self { major, minor, patch }
  }

  /// Parse `"0.1.0"`-style versions (as produced by `CARGO_PKG_VERSION`).
  ///
  /// Pre-release/build suffixes on the patch component are tolerated — including dotted ones
  /// like `0.2.0-rc.1` — because a parse failure here would force callers into a fallback value,
  /// and a shared fallback would make the exact-match handshake gate vacuously pass between
  /// genuinely different builds (I2). Callers must still treat `None` as a hard error, never as
  /// a sentinel version.
  pub fn parse(s: &str) -> Option<Self> {
    let mut it = s.splitn(3, '.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    // The rest may carry a pre-release/build suffix (`0-rc.1`, `3-alpha+b5`).
    let patch_raw = it.next()?;
    let patch_digits: String = patch_raw.chars().take_while(|c| c.is_ascii_digit()).collect();
    if patch_digits.is_empty() {
      return None;
    }
    let patch = patch_digits.parse().ok()?;
    Some(Self { major, minor, patch })
  }
}

impl core::fmt::Display for SemVer {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
  }
}

/// Coordinator → node, first frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
  pub protocol: u8,
  pub coordinator_version: SemVer,
  /// Opaque session id (128-bit), used to correlate frames and fence retries.
  pub session: u128,
  pub wanted: Caps,
}

/// Node → coordinator, in reply to `Hello`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Welcome {
  pub protocol: u8,
  pub agent_version: SemVer,
  /// Stable logical identity of this node (I4) — never an ephemeral instance id.
  pub node_id: String,
  pub caps: Caps,
  pub host: HostInfo,
}

/// Capabilities/limits exchanged in the handshake. `grammar_fingerprint` is the load-bearing field
/// for extraction parity (I2): a mismatch means the node's grammars differ from the coordinator's,
/// so agent-mode matches could differ — the coordinator must then refuse or demote to streaming.
///
/// The `Welcome` fingerprint covers the agent's *built-in* grammars; custom languages register per
/// job, so the job carries `expected_grammar_fingerprint` verified again after `LangEnv` applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caps {
  /// The node can execute a pushed binary (false ⇒ streaming mode only).
  pub can_exec: bool,
  /// The node can emit already-rendered result fragments (agent mode).
  pub structured_results: bool,
  /// zstd payloads are supported.
  pub zstd: bool,
  /// Per-frame ceiling this side will accept (resource-safety bound on the read path).
  pub max_frame: u32,
  /// blake3 over the grammar inventory (names, ABI, node kinds, fields) + core version (I2).
  pub grammar_fingerprint: [u8; 32],
}

impl Default for Caps {
  fn default() -> Self {
    Self {
      can_exec: true,
      structured_results: true,
      zstd: false,
      max_frame: crate::DEFAULT_MAX_FRAME,
      grammar_fingerprint: [0; 32],
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInfo {
  pub arch: String,
  pub os: String,
  pub nproc: u32,
  pub hostname: String,
}

// ---------------------------------------------------------------------------
// Job spec
// ---------------------------------------------------------------------------

/// What to run, with its kind-specific payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum JobKind {
  /// `vorpal scan` — a rule set.
  Scan(ScanJob),
  /// `vorpal run` — a pattern/kind matcher.
  Run(RunJob),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobSpec {
  pub job_id: u64,
  pub kind: JobKind,
  pub walk: WalkParams,
  pub printer: PrinterSpec,
  pub result_encoding: ResultEncoding,
  pub limits: Limits,
  pub flow: FlowParams,
  /// Project language environment (custom languages, language globs, injections) as canonical
  /// JSON bytes of a coordinator/agent-shared struct; opaque to the wire. Applied on the agent
  /// *before* the rule payload is deserialized (`SgLang::Custom` needs registration).
  pub lang_env: Option<Vec<u8>>,
  /// Post-`lang_env` grammar fingerprint the coordinator computed for itself. The agent recomputes
  /// after applying `lang_env` and refuses on mismatch (I2).
  pub expected_grammar_fingerprint: [u8; 32],
}

/// The scan job payload: the coordinator's **post-overwrite** rule set (rules *and* severities are
/// exactly what the coordinator resolved from `--error/--warning/--filter/--off`), so the agent
/// needs no project directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanJob {
  pub rules: RulePayload,
  /// Resolved severity for the synthesized `unused-suppression` rule (stable name, e.g. "hint").
  pub unused_suppression_severity: String,
  /// Resolved severity for the synthesized `no-suppress-all` rule.
  pub no_suppress_all_severity: String,
  /// The project directory result paths are normalized against for rule `files:`/`ignores:` glob
  /// matching. R0/loopback ships it verbatim (shared filesystem); later phases map it per-realm.
  pub proj_dir: String,
}

/// The run job payload — mirrors the CLI matcher surface; the agent rebuilds the same `RunArg`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunJob {
  pub pattern: Option<String>,
  pub selector: Option<String>,
  /// Stable strictness name ("cst"|"smart"|"ast"|"relaxed"|"signature"|"template").
  pub strictness: Option<String>,
  /// ESQuery-style kind selector (conflicts with pattern, as on the CLI).
  pub kind: Option<String>,
  pub rewrite: Option<String>,
  /// Language name as the CLI accepts it; `None` = infer per file.
  pub lang: Option<String>,
}

/// The rule set for a scan job, as canonicalized opaque bytes + a `blake3` digest over *exactly
/// those bytes*. The agent verifies the digest against what it received and never re-serializes
/// (see the module docs). The interpretation of the bytes is a coordinator/agent contract
/// (canonical JSON: `{"globals": [raw util YAML strings], "rules": [rule config values]}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulePayload {
  pub bytes: Vec<u8>,
  pub digest: [u8; 32],
}

impl RulePayload {
  /// Build a payload, computing the digest over the provided (already-canonical) bytes.
  pub fn new(bytes: Vec<u8>) -> Self {
    let digest = digest32(&bytes);
    RulePayload { bytes, digest }
  }

  /// Verify the carried digest matches the carried bytes.
  pub fn verify(&self) -> bool {
    digest32(&self.bytes) == self.digest
  }
}

/// `--no-ignore` selectors, mirrored from the CLI's `IgnoreFile` with stable wire names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoIgnore {
  Hidden,
  Dot,
  Exclude,
  Global,
  Parent,
  Vcs,
}

/// Walk parameters mirroring the CLI `InputArgs`, so the agent's `WalkParallel` reproduces the
/// coordinator's discovery exactly (I1 in agent mode). The language restriction is *not* shipped:
/// both sides derive it from the same job payload (scan: rule languages; run: the pattern lang).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalkParams {
  /// Search roots exactly as given on the CLI, relative to the job root (= agent cwd).
  pub paths: Vec<String>,
  pub globs: Vec<String>,
  pub no_ignore: Vec<NoIgnore>,
  pub follow_links: bool,
  pub threads: u32,
}

/// JSON output style (mirrors the CLI `JsonStyle`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JsonStyleSpec {
  Pretty,
  Stream,
  Compact,
}

/// Diagnostic report style (mirrors the CLI `ReportStyle`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReportStyleSpec {
  Rich,
  Medium,
  Short,
}

/// Cloud output platform (mirrors the CLI `Platform`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlatformSpec {
  GitHub,
  Sarif,
}

/// How to reconstruct the exact processor on the agent so rendered fragments match local output.
/// All tty/env-dependent decisions arrive **resolved** — the coordinator owns the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrinterSpec {
  Json { style: JsonStyleSpec, context: (u16, u16), include_metadata: bool },
  Colored {
    /// Rendered fragments carry ANSI codes (the coordinator's `writer.supports_color()`).
    buffer_color: bool,
    /// Highlight styles are colored (the coordinator's `should_use_color(choice)`).
    styles_colored: bool,
    /// Heading mode, resolved from `Heading::Auto` + tty on the coordinator.
    heading: bool,
    report_style: ReportStyleSpec,
    context: (u16, u16),
  },
  FileName { buffer_color: bool, styles_colored: bool },
  Cloud { platform: PlatformSpec },
}

/// How results come back: opaque already-rendered fragments (default, printer-transparent) or
/// neutral structured records (for cross-node dedup / merge / re-render).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResultEncoding {
  Rendered,
  Structured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
  /// Per-node result cap: the agent runs the same truncation a local `--max-results` would. In
  /// R0 (one node per unique target) this IS the enforcement; the coordinator-side global claim
  /// via `Rendered.match_count` arrives with multi-node fan-out in R2.
  pub local_max_results: Option<u32>,
  pub max_file_bytes: u64,
  pub max_lines: u32,
  pub deadline_ms: Option<u64>,
}

impl Default for Limits {
  fn default() -> Self {
    Self { local_max_results: None, max_file_bytes: 3_000_000, max_lines: 200_000, deadline_ms: None }
  }
}

/// Credit-window parameters for end-to-end backpressure. R0's loopback pipes provide natural
/// backpressure; explicit `Control::Credit` engages on multiplexed transports (R1+).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowParams {
  pub max_in_flight_frames: u32,
  pub max_in_flight_bytes: u64,
  pub chunk_bytes: u32,
}

impl Default for FlowParams {
  fn default() -> Self {
    Self { max_in_flight_frames: 256, max_in_flight_bytes: 16 * 1024 * 1024, chunk_bytes: 256 * 1024 }
  }
}

// ---------------------------------------------------------------------------
// Work assignment
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Assign {
  /// The node walks its own filesystem under `walk.paths` (the disjoint-disk default).
  SelfEnumerate,
  /// Shared corpus: process paths where `xxh3(path, seed) % of == index`.
  Shard { index: u32, of: u32, seed: u64 },
  /// Pull-based work-stealing: an explicit path batch for the given epoch.
  PathBatch { epoch: u32, paths: Vec<String> },
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ResultFrame {
  /// Opaque already-rendered `P::Processed` bytes. `match_count` exists so a coordinator can
  /// enforce a global `--max-results` across nodes without decoding payloads; that global claim
  /// (and per-match counting on the agent) lands with multi-node fan-out in R2 — R0 agents send
  /// one rendered item per frame with `match_count = 1` and the cap is enforced per node.
  Rendered { seq: u64, epoch: u32, match_count: u32, bytes: Vec<u8> },
  /// A neutral structured record (cross-node dedup/merge/re-render).
  Structured { seq: u64, epoch: u32, record: MatchRecord },
  /// Bulk binary (index products / streamed source content), content-addressed and chunked.
  Bulk { seq: u64, kind: BulkKind, chunk_index: u32, last: bool, bytes: Vec<u8> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BulkKind {
  /// Streamed source content for one path (streaming mode).
  SourceContent { path: String },
  /// A serialized `.vpb` `FileProduct` (index mode A).
  IndexProduct { path: String },
}

/// Neutral structured match record. Byte-stable and self-contained; `content_hash` is `xxh3` over
/// canonicalized identity fields for cross-node dedup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchRecord {
  /// Realm-qualified display path (`realm:/abs/path`) — one identity scheme across scan + index.
  pub path: String,
  pub language: String,
  pub start_byte: u32,
  pub end_byte: u32,
  pub start_row: u32,
  pub start_col: u32,
  pub end_row: u32,
  pub end_col: u32,
  pub text: String,
  pub rule_id: Option<String>,
  pub severity: Option<String>,
  pub message: Option<String>,
  pub replacement: Option<String>,
  pub content_hash: u128,
}

// ---------------------------------------------------------------------------
// Flow / telemetry / lifecycle
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Control {
  /// Credit grant: the receiver may send up to this many more frames / bytes on `channel`.
  Credit { channel: u16, frames: u32, bytes: u64 },
  /// Stop producing (global max-results reached, user interrupt, or deadline).
  Stop { reason: StopReason },
  Ping { seq: u64 },
  Pong { seq: u64 },
  /// Work-stealing pull request.
  RequestWork { max_paths: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
  MaxResults,
  UserInterrupt,
  Deadline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Telemetry {
  Progress { scanned: u64, matched: u64, skipped: u64, bytes_read: u64, files_total: Option<u64> },
  Heartbeat { seq: u64, monotonic_ms: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Done {
  pub epoch: u32,
  pub outcome: Outcome,
  pub stats: FinalStats,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
  Complete,
  Aborted(StopReason),
  Failed,
}

/// Terminal per-node stats. `error_count` (error-severity matches) funnels into the coordinator's
/// exit-code decision exactly as a local scan's would (§3.4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalStats {
  pub scanned: u64,
  pub skipped: u64,
  /// Count of result *items* produced (drives run's has-matches exit code).
  pub matched: u64,
  pub error_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteError {
  Io { path: Option<String>, msg: String },
  Parse { path: String, msg: String },
  RuleDigestMismatch { expected: [u8; 32], got: [u8; 32] },
  GrammarMismatch { expected: [u8; 32], got: [u8; 32] },
  VersionMismatch { coordinator: SemVer, agent: SemVer },
  UnsupportedAssign,
  Internal(String),
  Fatal(String),
}

impl core::fmt::Display for RemoteError {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    match self {
      RemoteError::Io { path: Some(p), msg } => write!(f, "remote io error at {p}: {msg}"),
      RemoteError::Io { path: None, msg } => write!(f, "remote io error: {msg}"),
      RemoteError::Parse { path, msg } => write!(f, "remote parse error at {path}: {msg}"),
      RemoteError::RuleDigestMismatch { .. } => write!(f, "rule payload digest mismatch"),
      RemoteError::GrammarMismatch { .. } => {
        write!(f, "grammar fingerprint mismatch (agent grammars differ from coordinator)")
      }
      RemoteError::VersionMismatch { coordinator, agent } => {
        write!(f, "version mismatch: coordinator {coordinator} vs agent {agent}")
      }
      RemoteError::UnsupportedAssign => write!(f, "agent does not support this work assignment"),
      RemoteError::Internal(m) => write!(f, "remote internal error: {m}"),
      RemoteError::Fatal(m) => write!(f, "remote fatal error: {m}"),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::frame::{DEFAULT_MAX_FRAME, parse_frame};

  fn round_trip(m: &Message) {
    let frame = m.to_frame(0).unwrap();
    let bytes = frame.to_vec();
    let (got_frame, _) = parse_frame(&bytes, DEFAULT_MAX_FRAME).unwrap();
    assert_eq!(got_frame.msg_type, m.msg_type());
    let got = Message::decode(&got_frame.payload).unwrap();
    assert_eq!(&got, m);
  }

  fn scan_job_spec() -> JobSpec {
    JobSpec {
      job_id: 1,
      kind: JobKind::Scan(ScanJob {
        rules: RulePayload::new(b"{\"globals\":[],\"rules\":[]}".to_vec()),
        unused_suppression_severity: "hint".into(),
        no_suppress_all_severity: "off".into(),
        proj_dir: "/repo".into(),
      }),
      walk: WalkParams {
        paths: vec![".".into()],
        globs: vec![],
        no_ignore: vec![NoIgnore::Hidden],
        follow_links: false,
        threads: 0,
      },
      printer: PrinterSpec::Json {
        style: JsonStyleSpec::Stream,
        context: (0, 0),
        include_metadata: false,
      },
      result_encoding: ResultEncoding::Rendered,
      limits: Limits::default(),
      flow: FlowParams::default(),
      lang_env: None,
      expected_grammar_fingerprint: [7; 32],
    }
  }

  #[test]
  fn all_message_variants_round_trip() {
    round_trip(&Message::Hello(Hello {
      protocol: crate::PROTOCOL_VERSION,
      coordinator_version: SemVer::new(0, 1, 0),
      session: 0xdead_beef_cafe,
      wanted: Caps::default(),
    }));
    round_trip(&Message::Welcome(Welcome {
      protocol: crate::PROTOCOL_VERSION,
      agent_version: SemVer::new(0, 1, 0),
      node_id: "web-0".into(),
      caps: Caps::default(),
      host: HostInfo { arch: "x86_64".into(), os: "linux".into(), nproc: 8, hostname: "pod".into() },
    }));
    round_trip(&Message::Job(scan_job_spec()));
    round_trip(&Message::Job(JobSpec {
      kind: JobKind::Run(RunJob {
        pattern: Some("Some($A)".into()),
        selector: None,
        strictness: Some("smart".into()),
        kind: None,
        rewrite: None,
        lang: Some("rust".into()),
      }),
      printer: PrinterSpec::Colored {
        buffer_color: true,
        styles_colored: true,
        heading: false,
        report_style: ReportStyleSpec::Rich,
        context: (2, 2),
      },
      ..scan_job_spec()
    }));
    round_trip(&Message::Assign(Assign::Shard { index: 1, of: 4, seed: 99 }));
    round_trip(&Message::Result(ResultFrame::Rendered {
      seq: 5,
      epoch: 0,
      match_count: 3,
      bytes: b"rendered-json-lines".to_vec(),
    }));
    round_trip(&Message::Control(Control::Credit { channel: 1, frames: 10, bytes: 4096 }));
    round_trip(&Message::Telemetry(Telemetry::Heartbeat { seq: 2, monotonic_ms: 12345 }));
    round_trip(&Message::Done(Done {
      epoch: 0,
      outcome: Outcome::Complete,
      stats: FinalStats { scanned: 100, skipped: 5, matched: 3, error_count: 1 },
    }));
    round_trip(&Message::Bye(RemoteError::Internal("boom".into())));
  }

  #[test]
  fn rule_payload_digest_verifies_and_detects_tampering() {
    let p = RulePayload::new(b"rule-set-v1".to_vec());
    assert!(p.verify());
    let tampered = RulePayload { bytes: b"rule-set-v2".to_vec(), digest: p.digest };
    assert!(!tampered.verify(), "digest must not match altered bytes");
  }

  #[test]
  fn msg_type_matches_header_for_all_variants() {
    // The self-describing tag must equal the header discriminant, or skip-by-len routing breaks.
    let m = Message::Control(Control::Ping { seq: 1 });
    let frame = m.to_frame(0).unwrap();
    assert_eq!(frame.msg_type, msg_type::CONTROL);
  }

  #[test]
  fn semver_parses_cargo_style_versions() {
    assert_eq!(SemVer::parse("0.1.0"), Some(SemVer::new(0, 1, 0)));
    assert_eq!(SemVer::parse("12.34.56"), Some(SemVer::new(12, 34, 56)));
    assert_eq!(SemVer::parse("1.2"), None);
    assert_eq!(SemVer::parse("1.2.3-alpha"), Some(SemVer::new(1, 2, 3)));
    // Dotted pre-release suffixes must parse (a None here would push callers toward a fallback
    // sentinel that makes the exact-match version gate vacuous).
    assert_eq!(SemVer::parse("0.2.0-rc.1"), Some(SemVer::new(0, 2, 0)));
    assert_eq!(SemVer::parse("1.2.x"), None);
  }
}

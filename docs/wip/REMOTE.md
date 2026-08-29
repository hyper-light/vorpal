# Vorpal Fleet — Parallel Remote Search / Scan / Index (no-install)

## Context

Vorpal today is a single-machine engine: `scan`/`run` walk a local filesystem via the `ignore`
crate and match with tree-sitter; `index` builds a knowledge-graph + vector index from local
files. This expansion lets one vorpal invocation **connect to many remote nodes — Kubernetes
pods, VMs of any stripe, sandboxes, Firecracker/Cloud-Hypervisor microVMs — and perform
search/scan/index work across all of them in parallel, without installing vorpal on the nodes.**

The mandate is **maximally robust, correct, and performant — complexity is not a constraint.**
The design is grounded in five verified architectural facts about the current code (not
assumptions):

1. **Core parsing/matching is already filesystem-agnostic.** `LanguageExt::grep(source)` and
   `StrDoc::try_new(src, lang)` take in-memory strings; the CLI is the only layer that touches
   the disk, through exactly one chokepoint: `read_file` (`crates/cli/src/utils/mod.rs:111`).
2. **The scan/search engine is already a pluggable producer→consumer.** `run_worker`
   (`crates/cli/src/utils/worker.rs:118`) spawns a producer that feeds a `std::sync::mpsc`
   channel drained by a single-threaded consumer that drives the printer.
3. **Every non-interactive printer's `Processed` type is already a fully-rendered, relocatable
   value** (`Vec<u8>` for JSON, `termcolor::Buffer` for colored/file-name, `CloudOutput` for
   SARIF/GitHub), rendered *off the main thread* on the worker. This is the seam the whole
   remote design pivots on.
4. **The index pipeline is already streaming and deterministic.**
   `OutlineExtractor::extract_product(path, source)` is a pure function; `FileProduct`
   serializes to a versioned, self-validating `.vpb` binary; `apply_products_sharded` +
   `KgWriter::absorb` + `link_writer` produce **byte-identical-to-serial** output for any
   path-sorted disjoint partition (pinned by `crates/ingest/tests/{sharded,streamed}.rs`).
5. **Rules already serialize.** `SerializableRuleConfig<SgLang>` is `Serialize`/`Deserialize`
   (`crates/config/src/rule_config.rs`), so shipping a job's rule set needs no new format.

The consequence: a remote node only ever contributes **bytes** (raw content) or **products**
(rendered result fragments / `.vpb` extraction products). Both converge on the *existing*
sync engine. The remote layer is additive; the hot paths are essentially untouched.

### Load-bearing invariants (violate any of these and results silently diverge)

These are called out up front because several are subtle and each has a dedicated mechanism below:

- **I1 — Discovery parity.** The set of files examined remotely must equal what a local
  `vorpal scan`/`index` would examine, i.e. full `ignore`-crate semantics (`.gitignore`/`.ignore`,
  hidden, parent, global, VCS). Agent mode gets this for free (it runs the real `WalkParallel`);
  streaming mode must *reconstruct* it (§3.3).
- **I2 — Extraction parity.** The tree-sitter grammars, ABI, and rule set that run on a node must
  be bit-identical to the orchestrator's, or matches/products differ. Enforced by an exact
  version + grammar-fingerprint handshake; mismatch → refuse or demote to streaming (§2, §4).
- **I3 — Version-stable identity hashing.** Every hash that crosses the wire or feeds node
  identity/dedup must be a version-stable algorithm (`blake3`/`xxh3`), never
  `std::hash::DefaultHasher` (whose output std does **not** guarantee across Rust releases) and
  never the iteration order of a `HashMap` (§1, §4).
- **I4 — Stable logical node identity.** A node's realm/namespace key must be a *stable logical*
  identity (workload name, StatefulSet ordinal, explicit `--node-id`), never an ephemeral
  instance id (pod UID, VM boot id), or incremental indexing reuses nothing (§4).
- **I5 — Scoped resolution.** Cross-node reference edges are created only where they are real
  (an actual import path), never by bare-name coincidence across unrelated realms (§4).

## Decisions (locked)

| # | Decision | Choice |
|---|----------|--------|
| D1 | Execution model | **Hybrid, auto-negotiated** — push an ephemeral static agent where a node can exec; byte-stream where it can't. |
| D2 | Transports (all first-class) | **SSH, Kubernetes, Docker/containerd, vsock/microVM + generic command-transport.** |
| D3 | Distributed-index result model | **Both** — unified merged index by default; federated per-node indexes with query-time RRF fusion also supported. |
| D4 | Fleet identity (same path on two nodes) | **Selectable per run** — `--fleet-mode {namespace,dedup}`, default `namespace`. |

**Recommend-and-proceed defaults** (chosen per the robustness mandate; all overridable): SSH via
`russh` (pure-Rust, static-musl-clean) with `openssh` as an opt-in alternative for
`~/.ssh/config`/ControlMaster reuse; `rustls` everywhere (never OpenSSL); zero-residue =
`memfd`-preferred with open-then-unlink fallback (guaranteed zero *persistent* residue); a
dedicated grammar-sliced `vorpal-agent` binary (not the 51 MB CLI); auto-negotiation on by
default with per-target policy gates; SSH+k8s implemented first, vsock/containerd last.

---

## Architecture overview

The topology is a **coordinator tree**, not a single hub. A small fleet degenerates to one
orchestrator talking directly to nodes; a large fleet (thousands of nodes, 10⁹-LOC scale) fans
out through **sub-coordinators**, each owning a region of nodes — this is what keeps binary
distribution, connection fan-out, and (for indexing) merge memory from bottlenecking on one
process (§4 *Scaling the merge*).

```
                       ┌──────────────── top coordinator (local vorpal) ────────────────┐
                       │   fuses regional results / seals regional index partitions       │
                       └───────────────┬───────────────────────────┬─────────────────────┘
                          sub-coordinator A                 sub-coordinator B     … (tree, N levels)
                          │  relays agent, owns region's        │
                          │  connections, merges its shard      │
        ┌─────────────────┼──────────────┐              ┌───────┴────────┐
      node             node            node            node            node
   (ssh/k8s/docker/vsock/cmd)  ──►  Negotiation  ──►  ExecMode::{EphemeralAgent, ByteStream}
        │                                                        │
        │  agent mode:  rendered P::Processed fragments (opaque) │  stream mode: (path, content) bytes
        └────────────────────────────────────────────────────────  → coordinator runs produce_item
                              │
      per printer, the fragment is ALREADY the wire output → SAME mpsc → consume_items → printer (UNCHANGED)

  vorpal-wire  = 16-byte LE frame header (bytemuck) + postcard message bodies + opaque .vpb/fragment payloads
  vorpal-agent = grammar-sliced static-musl binary: runs the REAL sync engine, streams fragments/products,
                 self-limited to its cgroup share so it never disrupts the node's primary workload
  vorpal-loader= tiny stage-0: verify Ed25519+blake3 → decompress into memfd → fexecve (agent never hits disk)
```

**Two orthogonal data paths, one protocol:**

- **Search / scan** (streams results live, always): the agent runs the real `produce_item::<P>`
  and ships already-rendered `P::Processed` fragments; the coordinator decodes them into the
  same channel the printer drains. Or, in stream mode, the node ships `(path, content)` and the
  coordinator runs `produce_item`.
- **Index** (builds an index): the node ships `.vpb` `FileProduct` batches (or raw content); each
  coordinator folds its region via the existing `stream_apply → link_writer → build_ann` path
  (streaming to sealed segments, §4), or keeps per-node indexes for federated query.

## New crates & touched code

| Crate | Kind | Purpose |
|-------|------|---------|
| **`vorpal-wire`** | new lib, `no_std + alloc` | Frame header (`bytemuck::Pod`) + postcard `Message` set + neutral `MatchRecord`. Canonicalizes all hashed/identity payloads to `BTreeMap` + version-stable hashes (I3). Shared by coordinator, agent, and (for `MatchRecord`) the JSON printer. |
| **`vorpal-transport`** | new lib | `Transport`/`Connection`/`Channel` traits + feature-gated backends (`ssh`,`k8s`,`docker`,`containerd`,`vsock`,`cmd`) + `negotiate()` + provisioner + reconnect supervisor + `RemotePolicy`. tokio lives here only. |
| **`vorpal-agent`** | new bin (static musl) | The pushed binary: reads `Hello`/`Job`/`Assign`, runs the existing sync rayon engine under cgroup-aware self-limits, streams framed results/products under credit. Grammars behind cargo features → slim per-job variants. |
| **`vorpal-loader`** | new bin (tiny, C-free) | Stage-0: `libc` + `ruzstd` + `ed25519-dalek`; verify + decompress agent into `memfd_create` and `execveat` — zero disk residue. |
| **`vorpal-remote`** | new lib | Coordinator: `RemoteProducer` (search/scan fan-out) + `build_index_distributed` driver + the coordinator-tree fabric wiring transport ↔ merge. |

**Surgical touches to existing crates** (detailed below): `crates/cli/src/utils/worker.rs`
(pluggable producer), `crates/cli/src/print/mod.rs` (one `WireFragment` bound + 3 impls),
`crates/cli/src/utils/mod.rs` (`Source::{Fs,Memory}` refactor of `read_file`),
`crates/cli/src/{scan,run}.rs` + `lib.rs` + `utils/args.rs` (CLI surface),
`crates/kg/src/writer.rs` (swap `DefaultHasher`→`xxh3` for `content_hash`, add a realm column),
`crates/resolve` (realm-scoped candidate visibility), `crates/ingest/src/product_batch.rs` (new
wire batch), `crates/index/src/distributed.rs` (new merge driver), `Cargo.toml` + `release.yml`
(members, deps, agent/loader musl build+sign+embed).

---

## 1. Wire protocol (`vorpal-wire`)

**Serialization: `postcard` message bodies inside a hand-rolled 16-byte LE frame header.**
serde is already a workspace dep, so message types reuse existing serde types (`SgLang`,
`Severity`, `SerializableRuleConfig`, the JSON printer's `Range`/`Position`/`MetaVariables`);
postcard is `no_std+alloc`, tiny (keeps the pushed agent small), and its number/struct encoding
is byte-stable. Rejected: `bincode` (less wire-stable across versions), `rkyv` (archived layout
is fragile across heterogeneous arch — arm64 pod ↔ amd64 coordinator), `prost` (needs codegen;
codebase is serde-native). Bulk payloads (`.vpb` products, source content, index shards) travel
as **raw byte ranges**, never re-encoded.

```rust
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct FrameHeader {
    pub magic: u16,     // 0x5650 "VP"
    pub version: u8,    // protocol major
    pub flags: u8,      // bit0 zstd, bit1 structured, bit2 checksum present
    pub channel: u16,   // 0 = control; N = per-shard result stream (multiplex)
    pub msg_type: u16,  // Message discriminant (mirrored so unknown frames skip by len)
    pub len: u32,       // payload length
    pub checksum: u32,  // xxh3-derived payload integrity (optional per flags)
}
// wire = [FrameHeader 16B LE] ++ [postcard(Message) | raw bytes]
```

Length-delimited over *any* byte pipe (SSH stdout, k8s exec stream, docker attach, vsock).
Unknown `msg_type` → skip by `len` (forward-compat). `zstd` gated by a capability bit + header
flag, applied to bulk/source/index frames; small control frames stay raw.

**Determinism of hashed payloads (I3).** postcard is byte-stable *only if the value is* — and the
reused serde types are not: `SerializableRuleConfig` carries `labels: Option<HashMap<..>>` and
`Metadata(HashMap<..>)`, and `SerializableRuleCore` carries `constraints`/`utils`/`transform:
Option<HashMap<..>>`, all of which serialize in nondeterministic iteration order. Therefore any
value whose bytes are hashed or content-addressed is first **canonicalized** (recursively, not
just at the top level: every `HashMap` → sorted `BTreeMap`/`Vec<(K,V)>`) before encoding. Two
consequences enforced in code: the rule-set `digest` is computed **once, over the exact bytes the
coordinator ships**, and the agent verifies against *those* bytes — it never re-serializes and
re-hashes; and `MatchRecord.content_hash` (cross-node dedup) is `xxh3` over canonicalized fields.

**Resource-safety on the read path (a node is authenticated but not trusted).** `len` is
validated against the negotiated `Caps.max_frame` *before* any allocation — an oversized frame is
rejected, never speculatively allocated — and decompression enforces both an absolute
output-size cap and a max expansion ratio, so a malformed `len` or a zstd bomb from a buggy or
compromised node cannot OOM the coordinator. `checksum` is verified before a payload is decoded.

**Message set** (postcard bodies): `Hello`/`Welcome` (handshake + `Caps` incl.
`grammar_fingerprint` = blake3 over the exact compiled grammar blobs + tree-sitter ABI + core
version — I2), `Job` (`JobKind::{Search,Scan,Index}` + `RulePayload` + `WalkParams` +
`PrinterSpec` + `ResultEncoding` + `Limits` + `FlowParams`), `Assign` (`SelfEnumerate` |
`Shard{index,of,seed}` | `PathBatch{epoch,paths}`), `Result`
(`Rendered{seq,epoch,match_count,bytes}` | `Structured{record}` | `Bulk{kind,chunk,last,bytes}`),
`Control` (`Credit`/`Stop`/`Ping`/`Pong`/`RequestWork`), `Telemetry` (`Progress`/`Heartbeat`),
`Done{outcome,stats}`, `Bye(RemoteError)`.

**Shipping rules:** primary `RulePayload::Resolved { rules: Vec<SerializableRuleConfig<SgLang>>,
globals, digest: [u8;32] }` — the coordinator ships its **post-overwrite** rule set (after
`--error/--warning/--filter/--off`), so rules *and* severities are identical (severity drives
the scan exit code) and the agent needn't have the project dir. The agent runs
`RuleCollection::try_new(configs)` exactly as the coordinator does; the blake3 `digest` (over the
shipped bytes, per above) is verified (`RuleDigestMismatch` otherwise). Fallback
`RulePayload::Yaml{bundle,overwrites,digest}`; search/run uses `RulePayload::Pattern(PatternSpec)`.
Compiled `RuleConfig` is *not* shipped (it holds compiled matchers/tree-sitter state and the agent
must recompile against its grammars anyway).

**`PrinterSpec`** reproduces the exact processor on the agent (color/heading/context/metadata
bake into the rendered fragment); the color/TTY decision is resolved on the coordinator (it owns
the terminal) and shipped. `MatchRecord` (neutral structured record) lifts json_print.rs's
`Range`/`Position`/`MetaVariables`/`MatchLabel`/`CharCount` into `vorpal-wire` and adds
`content_hash: u128` (`xxh3` of canonicalized path+range+text[+rule_id]) for cross-node dedup.

---

## 2. Transport, provisioning & negotiation (`vorpal-transport`)

**Async on a dedicated tokio runtime** (three of four backends are async-tokio-native with no
sync equivalent; the layer multiplexes I/O over hundreds of nodes × several streams). It mirrors
the existing `crates/cli/src/lsp.rs` pattern: the remote command builds a multi-thread runtime
and `block_on`s the coordinator. **The sync scan/index hot path is never touched; the agent
reuses the sync rayon engine verbatim** with only a thin async I/O shell at its edge.

**Exec-shaped trait trio.** Everything reduces to "run argv, pipe bytes, get exit code";
`push_file`/`pull_file` default over `exec`. `async_trait` at the `dyn` boundary (coarse,
control-plane granularity); the per-byte copy runs through monomorphized `AsyncRead/AsyncWrite`
halves handed back from `exec`, so boxing is amortized over whole streams.

```rust
#[async_trait] pub trait Transport: Send + Sync + 'static {
    fn descriptor(&self) -> &NodeDescriptor;   // redacted identity, never secrets
    fn hints(&self) -> TransportHints;          // a-priori (distroless, noexec, arch) from control plane
    async fn exec(&self, spec: &ExecSpec) -> Result<RemoteProcess, TransportError>;
    async fn exec_capture(&self, spec: &ExecSpec) -> Result<Output, TransportError> { /* default via exec */ }
    async fn push_file(&self, dest: &RemotePath, mode: u32, len: u64,
                       body: &mut (dyn AsyncRead+Send+Unpin)) -> Result<PushOutcome, TransportError> { /* default */ }
    async fn pull_file(&self, src: &RemotePath) -> Result<Box<dyn AsyncRead+Send+Unpin>, TransportError>;
    async fn health_check(&self) -> Result<(), TransportError>;
    async fn shutdown(&self) -> Result<(), TransportError>;
}
```
`RemoteProcess` carries `stdin/stdout/stderr` halves, a `wait` future, and an
`Arc<dyn RemoteKiller>` used by the Drop guard. A blanket `CommandTransport<C: CommandChannel>`
turns *any* `exec`-shaped CLI (`gcloud compute ssh`, custom brokers) into a `Transport` by
spawning `tokio::process::Command` — the generic escape hatch (D2).

**Per-backend:**
- **SSH — `russh`** (+ `russh-keys`/`russh-agent`, `russh-sftp`): one TCP → one `client::Handle`;
  each `exec` = a session channel with `exec(false, cmd)` (**no PTY** — a TTY line-discipline
  corrupts binary streams); SSH channels give native multiplexing + window backpressure; push via
  SFTP or `dd`/`cat` into `/dev/shm`; host-key verification in `Handler::check_server_key`
  (reject-unknown by default, TOFU opt-in); auth prefers ssh-agent. `openssh` (drives system ssh,
  reuses `~/.ssh/config`+ControlMaster) is an opt-in alt behind the trait.
- **Kubernetes — `kube` + `k8s-openapi`.** *Preferred:* **ephemeral-container injection** (patch
  `pod/ephemeralcontainers`, kubectl-debug style) with the agent image — works on distroless, no
  bytes pushed, no residue; gated on RBAC + a pullable agent image. **Its trust anchor differs
  from the pushed-binary path and is reconciled explicitly (§6):** the ephemeral-container image
  is pinned by digest (`@sha256:…`), and where the threat model demands the same guarantee as the
  signed binary, the container entrypoint is the same `vorpal-loader` stage-0 verifying the agent.
  *Fallback:* `Api::<Pod>::exec` (`tty:false`) over **WebSocket** (`v5.channel.k8s.io`; SPDY
  legacy); exit code arrives on channel 3 as a `metav1.Status` JSON (parse `ExitCode` cause);
  push = `exec tar -xmf -` / `cat >/dev/shm/x` (no daemon-side copy). Namespace allowlist +
  label-selector target expansion.
- **Docker — `bollard`.** `create_exec`+`start_exec` (`Tty:false` → 8-byte stream-framed
  stdout/stderr, bollard demuxes); **`put_archive`/`upload_to_container` is daemon-served → lands
  bytes even into distroless**; exit via `inspect_exec`. Unix socket (uid-gated) or mutual TLS
  (rustls).
- **containerd — `containerd-client`** *co-located only* (its exec wires stdio through host-local
  FIFOs; there is no network hijack). Remote containerd → go through the k8s/CRI layer or a
  `ctr`-shaped `CommandChannel`. Image `platform` (arch/os) read without executing.
- **vsock/microVM — `tokio-vsock`** (+ `CommandChannel` fallback over serial/VMM CLI). Preferred:
  a tiny **stage-0 vsock-loader in the initramfs** (rootfs stays pristine); coordinator connects
  `CID:port`, streams `zstd(agent)`, loader `memfd+fexecve`s it. vsock has no TLS → an **inner
  authenticated session** (Noise-IK / TLS-PSK keyed by the per-job token) provides
  confidentiality/integrity; exit status rides a trailer frame.

**Negotiation** — `negotiate(&dyn Transport)` fuses control-plane metadata first, live probe
second; a one-round-trip distroless-aware shell probe reports arch/libc, tmpfs+noexec (from
`/proc/mounts`), primitive inventory (`tar`/`dd`/`cat`/`base64`), memfd usability, cgroup budget,
seccomp hints. Decision tree (first match wins):

1. **Arch/OS** from control-plane where possible (k8s Node `nodeInfo`, docker `/info`, OCI image
   `platform`; else `uname -sm`). No static agent build for `(os,arch)` → **ByteStream**.
2. **Can we run any process?** k8s+RBAC → ephemeral container (done, distroless included); docker
   → always `put_archive`; else need shell/exec/ssh. None → **ByteStream**.
3. **Writable+executable landing spot**, probed in order `memfd ▸ /dev/shm ▸ $XDG_RUNTIME_DIR ▸
   writable mount ▸ /tmp`; write-ok-but-exec-`EACCES` ⇒ noexec, try next; none executable →
   **ByteStream**.
4. **Residue-minimal launch:** `memfd_create`+`execveat(AT_EMPTY_PATH)` (never touches disk) ▸
   `/dev/shm` stream-in + `chmod 0700` + **exec-then-unlink** (running inode holds it; nothing
   remains).

Output `ExecMode::{EphemeralAgent{arch,libc,transfer,launch}, ByteStream{prim,reader}}`, with
**runtime demotion** (agent `--selftest` fails → fall back to ByteStream) and a
fingerprint-keyed cache. Honest dead-ends surfaced, never silently mishandled: distroless without
tar under exec-only k8s (→ ephemeral-container path or explicit escalation); true zero-residue
guaranteed only with exec-capable tmpfs/memfd (else unlink-after-launch = zero *persistent*
residue).

**Two-stage signed delivery + hierarchical distribution.** Ship a ~50–100 KB **stage-0 loader**
(C-free: `libc` + `ruzstd` + `ed25519-dalek`) that lands cheaply (base64 here-doc /
`put_archive` / vsock), reads `zstd(agent)` from a pipe, verifies **Ed25519 signature + blake3
hash**, decompresses into a **memfd**, and `fexecve`s it → the multi-MB agent never hits disk.
The agent is **grammar-sliced by cargo feature**; the coordinator picks the slimmest variant
covering the job's language set; `{(os,arch) → blake3, bytes}` is embedded via
`build.rs`+`include_bytes!` (the `outline/default_rule.rs` precedent); **exact version+protocol
match or refuse** (mirrors the `.vseg` reject-on-mismatch rule, and upholds I2). At fleet scale
the blob is **not** fanned out from one hub: each **sub-coordinator relays** the (content-addressed)
agent into its region, and where a registry is reachable the digest-pinned image/OCI blob is
pulled by the node itself — so binary distribution and connection fan-out scale with the tree, not
with a single uplink.

**Agent self-limiting (robustness — it runs on live nodes).** Scanning a production pod/VM must
not disrupt that node's primary workload. The agent enforces the budget negotiation discovered:
threads capped to the cgroup CPU share, its `ByteBudget` wired to the cgroup memory limit,
`nice`/`ionice` applied by default, and a self-watchdog that exits+unlinks if the control stream
or heartbeat is lost. These are defaults, tunable via `--remote-concurrency` and policy.

---

## 3. Search / scan orchestration

**Pluggable producer** — extract the producer side of `run_worker` behind one trait; keep
`consume_items`, `Items<T>`, every printer, and the exit-code logic **verbatim**. Because
`Worker::consume_items<P>` is a *generic method* (so `dyn Worker` is not object-safe), the driver
stays generic over the worker type, exactly like today's `run_worker`:

```rust
// worker.rs
pub trait Produce<P: Printer>: Send + 'static {
    fn produce(self: Box<Self>, tx: mpsc::SyncSender<P::Processed>, processor: P::Processor) -> Result<()>;
}
fn run_producer<W, P>(worker: Arc<W>, producer: Box<dyn Produce<P>>, printer: P) -> Result<ExitCode>
where W: Worker + ?Sized + 'static, P: Printer + 'static {
    let (tx, rx) = mpsc::sync_channel(printer_window());   // bounded ⇒ printer paces producer
    let processor = printer.get_processor();
    std::thread::spawn(move || { let _ = producer.produce(tx, processor); });
    worker.consume_items(Items(rx), printer)               // existing consumer + printer, untouched
}
```
`LocalWalkProducer` = the current spawn body (worker.rs:127-156) moved verbatim ⇒
behavior-preserving for every existing command (the local path keeps its effectively-unbounded
channel; only the remote path uses a real window). The **only** printer-facing change:

```rust
// print/mod.rs — one bound + three impls, no concrete printer logic changes
pub trait WireFragment: Sized + Send + 'static {
    fn encode(&self, out: &mut Vec<u8>);            // on the agent
    fn decode(bytes: &[u8]) -> anyhow::Result<Self>; // on the coordinator
}
impl WireFragment for Vec<u8> { /* identity — JSON */ }
impl WireFragment for termcolor::Buffer { /* bytes in/out — colored/file-name */ }
impl WireFragment for CloudOutput { /* GitHub=bytes; Sarif=serde_json of Vec<sarif::Result> */ }
// Printer::Processed gains `: WireFragment`. Interactive is excluded from --remote.
```

### 3.1 The producer bridge

`RemoteProducer` builds a tokio runtime on the producer thread (lsp.rs pattern), spawns one task
per target (connect → handshake → agent-vs-stream by `Caps.can_exec` + grammar-fingerprint match
→ `Job`/`Assign`), decodes inbound frames, and funnels fragments to a **single forwarder** that
is the only writer to the sync `tx`. Backpressure is end-to-end: tokio bounded mpsc → blocking
forwarder (`spawn_blocking`, so `SyncSender::send` blocking never stalls the reactor) → std
`sync_channel`; the printer's drain rate throttles the network and nothing buffers the whole
corpus. Global `--max-results` reuses the exact `MaxItemCounter`: each `Rendered.match_count`
`claim`s; on `reached_max()` broadcast `Control::Stop{MaxResults}`, which agents wire into the
same atomic their `PathWorker::should_stop()` already checks (`WalkState::Quit`). Multiplexing is
correct because each fragment is a self-contained rendered block and the single-threaded consumer
writes them in arrival order; cross-file printer state (JSON separators, SARIF accumulation) lives
only in the sequential consumer.

### 3.2 Agent mode — split at `produce_item`

The pushed agent runs the real pipeline on its own `WalkParallel` built from the shipped
`WalkParams`, so `.gitignore`/glob/hidden semantics are byte-identical to local (**I1 for free**),
does its own `canonicalize`/`proj_dir` handling locally, and ships only rendered fragments — the
large network saving.

### 3.3 Streaming mode — split at `read_file`, and reconstructing discovery

For nodes that cannot exec our binary, the remote side is a *generic* enumerator+reader (tar,
`kubectl cp`, `docker cp`, `find`+`cat`). **A naïve enumerator does not reproduce `ignore`
semantics** — a raw `tar` of a tree includes vendored deps, build output, and everything
gitignored — so streaming discovery would examine a *different file set* than local `vorpal scan`,
silently changing results. To uphold **I1**, streaming mode reconstructs the walk on the
coordinator:

1. The remote side streams the tree's **ignore-relevant metadata** — the file path list plus the
   contents of every `.gitignore`/`.ignore`/`.git/info/exclude` and the relevant global-ignore
   files (all cheap, and already present in a `tar` of the tree).
2. The coordinator runs the **`ignore` crate's own matcher** (`ignore::gitignore`/`overrides`,
   the same types `WalkParallel` uses) over that path list with the reconstructed ignore stack,
   producing exactly the set `WalkParallel` would have yielded — same precedence, nested
   `.gitignore`, hidden/parent/VCS rules, and CLI `--globs`/`--no-ignore`.
3. Only for the surviving paths does it request content (`Bulk::SourceContent`, with the
   3 MB/200k-line cap applied *on the remote reader* before shipping), then run
   `produce_item_from_content`.

The content path itself is a small, surgical refactor of the single `read_file` chokepoint:

```rust
// utils/mod.rs
pub enum Source<'a> { Fs, Memory(&'a str) }
pub fn filter_source_rule(path: &Path, src: Source, ...) -> Result<SmallVec<[Vorpal;1]>>;
// filter_file_rule(path,..) == filter_source_rule(path, Source::Fs, ..)
// produce_item_from_content(display_path, content, processor): Source::Memory
```
`produce_item_from_content` does no filesystem call, so it cannot `canonicalize()`. Because rules
carry `files:`/`ignores:` globs and `proj_dir`-relative behavior, remote paths are shipped
**pre-normalized relative to the remote job root**, and the coordinator maps `remote job root →
canonical prefix` (the same mapping the index path uses, §4) so rule path-matching and diagnostic
paths are identical to a local run rooted there. Symlinks are resolved on the side that owns the
files: agent mode via `WalkParallel` (honoring `--follow`); stream mode normalizes the tar's link
entries to the same policy (default: do not follow, matching local default) and never escapes the
job root. Streaming thus reuses ~100% of the matching/printing pipeline; only byte-acquisition and
discovery-reconstruction differ.

Interactive/rewrite-apply is rejected for `--remote` (it edits local files); remote fixes run on
the agent under `--remote-apply`, returning a diff summary as `Structured` records.

### 3.4 Fault tolerance & exit codes

Heartbeats + transport EOF detect death (miss K → `Failed`, `Ping`/`Pong` probes
idle-but-alive). At-least-once delivery; `Structured` dedups by `content_hash`, `Rendered` fences
by `(node,shard,epoch,seq)`. A dead node with a private disk is **irrecoverably partial** and is
*surfaced, never silently dropped*: per-node `FinalStats.error_count` funnels into the same
`AtomicUsize` the scan worker reads (error-severity matches still fail the build), and a reserved
**exit code `4 = REMOTE_INCOMPLETE`** (precedence `REMOTE_INCOMPLETE > DiagnosticError > no-match
> success`, unless `--remote-allow-partial`) makes "clean, no error matches" provably distinct
from "a node died." JSON/SARIF gain a top-level `remote`/`invocations` section
(`executionSuccessful=false` on partial).

### 3.5 Work distribution

Default `SelfEnumerate` (disjoint disks — each pod/VM scans what it sees; the union is the result;
intrinsically load-balanced by streaming, no barrier). Shared-corpus (NFS/replicated) →
`Shard{index,of,seed}` (deterministic `xxh3(path,seed)%of`) or pull-based work-stealing
(`RequestWork`→`PathBatch`) with speculative re-execution of straggler batches fenced by `epoch`.

---

## 4. Distributed indexing

**The correctness crux, proven.** Distributed indexing reduces to **canonical path assignment +
disjoint partition + global byte-lexicographic sort**, after which each coordinator runs the
*existing, determinism-pinned* `stream_apply → link_writer → build_ann` path. Determinism is
**inherited**, not re-engineered.

**Version-stable identity hashing (I3), and the one required change to `crates/kg`.** The
byte-identity proof assumes every hash feeding a `NodeId`/dedup is machine- and
version-independent. Today `KgWriter::content_hash` uses `std::collections::hash_map::DefaultHasher`
(writer.rs:332) — fixed-seed within a rustc version, but **std explicitly does not guarantee it
across Rust releases**, so a cached or differently-built agent could produce divergent hashes and
break byte-identity without tripping the version check. **Change:** replace it with the in-tree
`xxh3` (or `blake3`) so identity hashing is version-stable, and pin it with a cross-build vector
in tests. (This also removes the last reason two same-version builds could ever disagree.)

**What crosses the wire — `.vpb` products, never sealed segments.** `seal()` discards references
and the canonical index, and resolves edges against a *node-local* symbol table — so a call from
node A's file to a definition on node B would have been scored `external` and **no edge exists**,
with the evidence to re-resolve it gone. There is (by design) no on-disk sealed-segment merge.
Therefore nodes ship `FileProduct` batches (mode A) or raw content (mode B); merge happens
**pre-seal**. Products are finer-grained (reuse the per-file `.vpb` incremental cache), carry
owned portable strings, are versioned + self-validating, and their refs are keyed by *position
index* (not process-local NodeId), so they rebase cleanly.

**Global determinism (argued).** With A1 (product portability: `extract_product` is a pure
function of `(path,content)` given a fixed rule set + `PRODUCT_FORMAT_VERSION`, and `.vpb`
encode/decode is a bijection — pinned), Lemma 1 (`apply_products_sharded ∘ link_writer` depends
only on the path-sorted product list — the `sharded.rs` invariant), and Lemma 2 (`stream_apply`
== batch under any budget — the `streamed.rs` invariant): **for any partition where each `(p,c)`
is produced by exactly one node under a canonical path `p`, and the coordinator forms the global
entry list = canonical paths sorted by `str::cmp` and yields `extract_product(p,c)` per entry,
the committed index equals a single-machine `build_index` over the same file set byte-for-byte,
including `ann.bin`.** Output depends only on the sorted product list, never on which node built
each product (partition-scheme independence). A per-batch **extractor fingerprint** (blake3 over
rules+specs+version+lang dispatch) enforces A1 across a heterogeneous fleet — mismatches are
rejected, never merged (I2).

**Fleet identity (D4 — selectable) feeds the same machinery, on a STABLE key (I4).** The
coordinator owns the authoritative global manifest and assigns each node a canonical prefix
(reusing the `WarmRoot` prefix logic): `key = canonical_prefix + (file relative to local_root)`.
The prefix is a **stable logical identity** — workload/Deployment name, StatefulSet ordinal, VM
role, or explicit `--node-id` — **never** the ephemeral pod UID / VM boot id, because an
identity that changes each run would re-key every canonical path and defeat all incrementality
(§ *Incremental*). Realm is stored as a small **realm column** on the node segment (a minor
additive change to `vorpal-kg`/`vorpal-segment`), so query results carry node provenance and
resolution can scope by realm (below) — cleaner than smuggling the realm into the path string.
- `--fleet-mode namespace` (default): each node gets a **distinct** realm, so all files coexist
  under distinct canonical paths → disjoint by construction, nothing dropped. Federation of
  distinct filesystems folded into **one** graph.
- `--fleet-mode dedup`: nodes **share** the realm/prefix (partition of one corpus); the
  coordinator deduplicates the manifest union to one entry per canonical path (deterministic
  lowest-node tie-break + diagnostic) *before* applying (mandatory: `absorb` requires disjoint
  rows).

Both modes produce a globally-path-sorted disjoint product stream → the Theorem holds for both.

**Scoped reference resolution (I5) — required for a correct unified+namespace default.** The
resolver builds a *global* `by_name` candidate map, and for a bare-name reference with no local
match it falls back to **any `exported` definition with that name** anywhere in the table
(`resolver.rs:148-188`, `finish`). In a flat cross-node table that means node-A's `parse()` would
bind to an unrelated node-B's exported `parse()` — a **fabricated cross-node edge**. Fix: make
candidate visibility **realm-aware**.
- Bare/unqualified references resolve only among candidates **in the same realm** (plus the
  file-local and same-directory rules already there). This keeps within-node resolution
  byte-identical to a single-machine build.
- A **genuine** cross-node edge is created only where evidence crosses realms: a path-form import
  whose exact target path resolves into another realm (e.g. a shared module actually imported),
  which the exact-path matcher already gates. Such edges are labeled cross-realm and
  confidence-scored, never guessed.
- `dedup` mode is one logical corpus, so resolution is global as today (its "cross-node" edges are
  ordinary within-corpus edges).

This is a small change to `crates/resolve` (thread the referrer's realm into candidate filtering)
and preserves the "approximate edges are labeled, never faked" invariant across the fleet.

**Result model (D3 — both).**
- **Unified merged (default):** gather products → global sort → `build_index_distributed`
  (`stream_apply` over the global manifest with a spool-replay-or-fetch closure → `link_writer` →
  seal → lazy `build_ann`). Cross-node reference resolution is **central and realm-scoped** (above):
  nodes ship *unresolved* refs; the coordinator runs resolution over the merged, realm-tagged
  definitions. One queryable `nodes.vseg`/`strings.heap`/`edges.bin`/`ann.bin`.
- **Federated (opt-in):** each node (or region) keeps its own index; the existing hybrid-RRF query
  coordinator (`search_index`) fans out across per-node indexes and fuses with Reciprocal Rank
  Fusion at query time. No cross-node edges; scales query by fan-out. To preserve recall, each
  shard is queried with an **over-fetch `k' ≫ k`** before fusion (the true global top-k may cluster
  in one shard), and results are keyed by `CanonicalKey` for cross-shard dedup.

**Scaling the merge (the single-coordinator ceiling, addressed).** The in-process
`absorb`→`seal` path accumulates the entire merged `KgWriter` in RAM before sealing;
`ByteBudget` bounds only transient *products*, not the growing graph. Indexing a fleet larger than
one machine's RAM into **one** unified index would therefore reproduce the whole-graph-in-RAM
failure the architecture doc warns against, and `link_writer`/`resolve`/`build_ann`/`seal` would
all run on one machine. Two mechanisms lift this ceiling (implementing the aspirational
ARCHITECTURE.md §3.4 path):
1. **Hierarchical merge over the coordinator tree.** Each sub-coordinator merges its region into a
   sealed **partition** (path-range or realm partitioned); the top coordinator merges partition
   *directories* + cross-partition resolved edges, not raw columns. Work and memory divide by the
   tree's fan-out.
2. **Segment-streaming commit.** Within a coordinator, seal path-sorted **segments** incrementally
   to disk (the `.vseg` store is already segmented) and hash-partition edges into per-shard delta
   logs compacted to CSR/CSC in the background, so the coordinator never holds the whole graph —
   only a bounded window. A top-level manifest maps shard → head and is published last (the commit
   point), preserving crash-safety.

For small fleets both collapse to the current single-writer path (identical bytes); they engage
only past a size threshold. Determinism is preserved because the *global path-sorted product
sequence* is unchanged — only *where* each segment is sealed moves.

**ANN — embed after the graph merge, once, over the full vector set** (the ANN tier never runs on
nodes). Since the merged `Kg` is byte-identical to single-machine, `build_ann` (lazy, off the
commit hot path) yields a byte-identical `ann.bin`; the Vamana graph is built once over the full
merged matrix (already deterministic at any thread count — never merge per-node subgraphs). In the
segment-streaming/hierarchical case the embed still runs over the fully merged node set (vectors
are a pure function of the sealed rows). Per-node vector shipping is reserved only for an expensive
*neural* embedder (re-key `CanonicalKey → NodeId` after merge).

**Incremental & crash-safe across the cluster (two-level cache, content-addressed).** Nodes own a
local `products/*.vpb` cache; the coordinator owns the authoritative product **spool** + committed
index + prior global manifest, keyed by the *stable* canonical path (I4). Cache validity is
checked by **blake3 content hash first**, mtime only as a fast-path hint — because ephemeral pods
and fresh checkouts re-stamp mtimes on identical content, and mtime-only validation would
re-parse the whole fleet every run (and an ephemeral node's local cache dies with it, so the
coordinator spool is the durable cache that must survive node churn). Per run: nodes ship cheap
scoped manifests → `reconcile_manifests` → whole-tree-unchanged fast path (`Kg::peek_node_count`,
near-instant) or fetch only changed products (unchanged replay from the spool). Full re-link every
run (removals/renames can't leave stale nodes); the manifest is published **last** = commit point.
Node dies mid-ship → spool holds what arrived (durable, self-validating); resume re-requests only
the missing products (idempotent per A1). Coordinator dies mid-merge → prior manifest still points
at the last good epoch; resume recomputes from spool + deltas.

**New merge API** (in `vorpal-ingest` `product_batch.rs` + `vorpal-index` `distributed.rs`):
```rust
pub const PRODUCT_BATCH_VERSION: u32 = 1;   const PRODUCT_BATCH_MAGIC: &[u8;4] = b"VPBB";
pub struct ProductBatch { pub realm: RealmId, pub extractor_fingerprint: u64,
                          pub products: Vec<(String, FileProduct)> }        // reuses .vpb codec; realm = stable id
pub struct PathScope { pub local_root: PathBuf, pub canonical_prefix: String, pub realm: RealmId }
pub fn scan_scoped(scope: &PathScope, handled: impl Fn(&str)->bool + Sync) -> io::Result<Manifest>;

pub trait ProductSource: Sync { fn fetch_product(&self, s: &FileStat) -> io::Result<Option<FileProduct>>; }
pub trait ContentSource: Sync { fn fetch_content<'s>(&self, s: &FileStat, sc: &'s mut ExtractScratch)
                                    -> io::Result<Option<&'s str>>; }
pub enum IndexMode<'a> { RemoteShards(&'a dyn ProductSource), StreamContent(&'a dyn ContentSource) }
pub fn reconcile_manifests(node: &[(RealmId, Manifest)]) -> (Manifest, HashMap<String,RealmId>);
pub fn build_index_distributed(global: &Manifest, out: &Path, mode: IndexMode<'_>) -> Result<IndexReport>;
```

---

## 5. CLI surface

Flags on existing subcommands (reuse `--json`, `--format sarif`, `--max-results`, globs, ignore
flags unchanged) plus a management umbrella:
```
vorpal scan  --remote <targets...> [flags unchanged]
vorpal run -p '<pat>' -l rs --remote <targets...>
vorpal index --remote <targets...> [--fleet-mode namespace|dedup] [--federated]
vorpal cluster ping|caps|probe <targets...>          # handshake / exec-probe / grammar-fingerprint
```
Targets: inline URIs `ssh://user@host:port`, `k8s://ns/pod[/ctr]`, `docker://name`,
`containerd://ns/ctr`, `vsock://cid:port`, `cmd://<template>`; `@targets.yaml`; k8s label
selectors (`--remote k8s: --selector app=web -n prod`). Knobs: `--remote-mode {auto|agent|stream}`,
`--agent-binary <path>`, `--node-id <template>` (the **stable** realm/namespace key, I4 — e.g.
`{{workload}}` or `{{statefulset}}-{{ordinal}}`; a sensible per-transport default is derived, never
the ephemeral instance id), `--remote-retry {none|reconnect|reschedule}`, `--remote-allow-partial`,
`--remote-deadline <dur>`, `--remote-concurrency <n>` (per-node in-flight window *and* the agent's
self-imposed thread cap). **One identity scheme:** the realm that namespaces a canonical path in
the index is the same value that prefixes a result path in scan output (`realm:/app/src/main.rs`),
so a path means the same thing in search results and in the graph; `--no-remote-tag` strips the
display prefix for single-realm or `dedup` runs. Output/exit aggregate per §3.4.

---

## 6. Security (load-bearing)

- **Transport trust:** SSH host-key verification (reject-unknown default; TOFU opt-in, persisted);
  k8s/docker via rustls (kubeconfig CA / mutual TLS; never `insecure-skip-tls-verify` unless
  explicit); vsock/cmd inner Noise-IK/TLS-PSK keyed by the per-job token.
- **Provisioning trust anchors are consistent across paths.** The pushed-binary path is
  Ed25519+blake3-verified in a memfd. The k8s ephemeral-container path is **not** silently weaker:
  the image is pinned by digest, and when the threat model requires the same guarantee its
  entrypoint is the same `vorpal-loader` stage-0 that verifies the agent — so "we ran our code, not
  something the registry swapped" holds on both paths, and the asymmetry (registry/admission trust
  vs. signature trust) is documented, not hidden.
- **No secrets on remote disk, ever:** the per-job 256-bit capability token is delivered via env
  or stdin only.
- **Result authenticity — honest threat model.** In agent mode the node owns its disk and is the
  scan *target*, so signing results proves *provenance* (this authenticated agent produced them),
  **not** that they reflect some other ground truth — a compromised node can only misreport its
  own files, which is inherent to scanning it. Result frames are therefore HMAC'd with the job key
  to defend an authenticated-but-untrusted **relay/sub-coordinator** in the tree from forging or
  tampering with a downstream node's results; end-to-end transport encryption covers the wire. The
  spend that actually matters is the *input* trust anchors above (host-key pinning, image digests),
  not per-result crypto theater.
- **Agent integrity:** Ed25519-signed at release, public key embedded in stage-0, which verifies
  signature **and** blake3 before `fexecve`; coordinator pins expected blake3 per `(os,arch)`; ELF
  `e_machine` checked against probed arch; **refuse unexpected arch** (never "try anyway").
- **Non-disruption is a security property here too:** the agent's cgroup-aware self-limits (§2)
  bound its blast radius on a shared production node.
- **Least-privilege `RemotePolicy`** consulted before every connection/provision step: separate
  grants for `allow_push_exec` (run a binary) vs byte-read (streaming only needs read);
  `allowed_backends`, host allow/deny, `k8s_namespaces`, `max_nodes`, `max_streams_per_node`.
  Any out-of-policy node is refused before a byte moves. `Redacted<T>` newtype for logs/errors.
- **Authorization context:** this is dual-use remote execution; gate behind explicit target
  specification + policy, and document that it is for operator-authorized fleets only.

---

## 7. Execution & phasing

Each phase is independently useful and gated on a **differential test** (remote result ≡
local result over the same corpus — see §8 for the equivalence oracle).

- **R0 — wire + loopback + agent skeleton.** `vorpal-wire` (frame + `Message` + postcard +
  canonicalization/version-stable hashing, I3); `vorpal-agent` running the real `produce_item` for
  `scan`/`run`; a `loopback`/`subprocess` transport (spawn the agent as a local child). Pluggable
  `Produce`/`run_producer` (generic-over-`W`), `WireFragment` + 3 impls, `Source::{Fs,Memory}` +
  the streaming-discovery reconstruction (I1). **Gate:** `vorpal scan --remote loopback://` ≡
  `vorpal scan` (normalized) for JSON + colored + SARIF, on a repo *with* a `.gitignore`.
- **R1 — SSH + negotiation + push.** `russh` backend, `negotiate()`, stage-0 `vorpal-loader`
  (memfd/dev-shm), signed agent embed in `release.yml` (musl matrix already exists), agent
  self-limits. **Gate:** scan/run over SSH to a VM, both agent and forced-stream modes ≡ local.
- **R2 — parallel fleet + unified index merge.** `RemoteProducer` fan-out; global `--max-results`,
  exit-code aggregation, `REMOTE_INCOMPLETE`; `content_hash`→`xxh3`; realm column + realm-scoped
  resolution; `ProductBatch` + `build_index_distributed` + `reconcile_manifests`; `--fleet-mode`,
  stable `--node-id`. **Gate:** the new determinism + no-false-edge tests (§8) + N-node scan ≡
  local.
- **R3 — Kubernetes + Docker.** `kube` (digest-pinned ephemeral-container preferred, exec
  fallback) + `bollard` (`put_archive` distroless reader); label-selector targets; sub-coordinator
  relay for binary distribution. **Gate:** scan+index across a multi-pod cluster; distroless pod
  via ephemeral container.
- **R4 — vsock/containerd + federated query + scale-out + hardening.** `tokio-vsock` + initramfs
  stage-0; containerd co-located / `ctr` CommandChannel; federated per-node index + over-fetch RRF
  query fan-out; hierarchical coordinator tree + segment-streaming commit; full security pass
  (host-key store, image-digest pinning, policy, redaction). **Gate:** microVM run; federated query
  ≡ unified query top-k on a fixture; a merge that exceeds one coordinator's RAM budget completes
  via segment streaming.

---

## 8. Verification

**The equivalence oracle.** Local output order is nondeterministic (parallel `WalkParallel` →
mpsc), and remote adds N-node interleaving, so "≡ local" is defined as **equal after
canonicalization**, never raw byte compare: JSON/`Structured` results sorted by `(realm, path,
range)`; SARIF results sorted; colored/file-name compared as the sorted set of per-file blocks. A
`--sorted` output mode (buffer + sort before print) provides the same determinism for CI and for
users regardless of remoting.

- **New index determinism tests** (mirror `crates/ingest/tests/{sharded,streamed}.rs`):
  `distributed_merge_is_byte_identical_to_single_machine` (partition N ∈ {1,3,7,#files} by hash
  *and* arbitrary assignment; assert `nodes.vseg`/`strings.heap`/`edges.bin` **and `ann.bin`** equal
  single-machine `build_index`); `arrival_order_independence`; `mode_a_equals_mode_b`;
  `overlapping_paths_dedup_deterministically`; `incremental_reparses_only_changed_but_matches_full`;
  `crash_resume_partial_products`; `product_batch_codec_round_trips_bit_exactly` +
  `rejects_corruption_truncation_and_foreign_fingerprints`;
  `segment_streaming_merge_equals_in_process_merge` (the scale path is byte-identical to the simple
  path).
- **New correctness-of-the-fixes tests:**
  `content_hash_is_stable_across_builds` (a pinned vector — guards I3 against a rustc/hasher
  change); `namespace_mode_creates_no_bare_name_cross_realm_edges` and
  `dedup_mode_resolves_globally` (guard I5 against regression, both directions);
  `streaming_discovery_matches_walkparallel` (feed a tree with nested `.gitignore`/`.ignore` +
  hidden files; the reconstructed stream-mode file set equals `WalkParallel`'s — guards I1);
  `ephemeral_identity_reuses_spool` (same stable `--node-id` across two "pod incarnations" with
  changed mtimes but identical content → near-zero re-parse — guards I4).
- **Search/scan differential tests:** for each printer, `--remote loopback://` ≡ local (normalized);
  `--max-results` global cutoff; partial-node exit-code precedence.
- **Robustness/adversarial tests:** oversized-`len` frame and zstd-bomb are rejected without
  allocation blowup (§1); an agent capped to a 1-core/256 MB cgroup completes without exceeding it.
- **End-to-end manual:** `vorpal cluster probe <targets>` to inspect negotiation; run against a
  real SSH VM, a kind/minikube cluster (incl. a distroless pod), and a Firecracker microVM;
  confirm zero residue (`ls /dev/shm`, `/tmp` post-run) and clean teardown on `SIGINT`.
- **Perf:** agent vs stream mode wall-clock + bytes-on-wire over a large corpus; bounded coordinator
  RAM under segment-streaming at a synthetic multi-node load exceeding one coordinator's memory.

---

## Critical files

- `crates/cli/src/utils/worker.rs` — `Produce`/`run_producer` (generic over `W`), `RemoteProducer`
  bridge; reuse `MaxItemCounter`; keep `consume_items`/`Items` intact.
- `crates/cli/src/print/mod.rs` — `WireFragment` bound + 3 impls (json_print.rs types promoted to
  `vorpal-wire`, canonicalized for hashing).
- `crates/cli/src/utils/mod.rs` — `read_file` behind `Source::{Fs,Memory}`; `filter_source_*` +
  `produce_item_from_content`; streaming-discovery reconstruction via the `ignore` matcher.
- `crates/cli/src/{scan,run}.rs`, `lib.rs`, `utils/args.rs` — `--remote`/`--fleet-mode`/`--node-id`/…,
  `cluster` subcommand; shared exit-code helper.
- `crates/config/src/{rule_config.rs,rule_core.rs}` — `SerializableRuleConfig<SgLang>` is the rule
  wire payload (already serde); canonicalize its internal `HashMap`s at the wire boundary (I3).
- `crates/kg/src/writer.rs` — swap `content_hash` `DefaultHasher`→`xxh3` (I3); add the realm column;
  `absorb`/`seal` invariants the proof rests on.
- `crates/resolve/{resolver.rs,table.rs}` — realm-scoped candidate visibility (I5).
- `crates/ingest/src/{pipeline.rs,product.rs,manifest.rs}` — reuse `stream_apply`/
  `apply_products_sharded`/`link_writer`; add `product_batch.rs` (realm-tagged) + `scan_scoped`.
- `crates/index/src/lib.rs` — `build_index` commit tail + `WarmRoot` prefix logic; home of
  `build_index_distributed`/`reconcile_manifests` + the segment-streaming/hierarchical merge
  (new `distributed.rs`).
- `crates/segment/*` — the realm column + the segment-streaming commit target (already segmented).
- `Cargo.toml` + `.github/workflows/release.yml` — new members/deps; agent+loader musl build,
  Ed25519-sign, blake3-record, `include_bytes!` embed.

//! The ONE GPU path for the doc-side encoder fill: the six encoder GEMMs as a
//! `wgpu` compute shader (`gemm_nt.wgsl`) over Metal / Vulkan / DX12 — NVIDIA,
//! AMD, Intel, and Apple through the OS driver alone, no vendor SDK, nothing
//! beside the binary. Doc-side ONLY ([`super::GemmPath::Gpu`]): the query-side
//! rerank keeps the fixed-order lanes everywhere.
//!
//! Shape of the contract:
//! * Weights become RESIDENT once per model open ([`GpuGemm::make_resident`] —
//!   every layer matrix, ≈453 MB for CodeRankEmbed), keyed by the host slice's
//!   address; a GEMM whose weight is not resident is a typed error, never a
//!   silent per-call upload. Activations round-trip per GEMM through reusable
//!   scratch buffers (grown to the largest batch seen) — the copy bytes and
//!   wall-clock are ledgered ([`GpuGemm::transfer_report`]) so the host↔device
//!   share is a measurement, not a guess.
//! * Every size is derived from `adapter.limits()` ([`Tile::derive`] for the
//!   workgroup/tile geometry, the row chunking from the binding/buffer limits);
//!   an adapter whose limits cannot hold the shapes is a typed refusal.
//! * Device selection: discrete over integrated over virtual/other by
//!   `adapter.get_info()`; software rasterizers (`DeviceType::Cpu`, e.g.
//!   lavapipe) only under `VORPAL_ENCODER_GPU=software` — they exist for
//!   correctness runs, and would lose to the CPU rungs on throughput.
//! * NO panics: `wgpu`'s default uncaptured-error handler aborts the process, so
//!   the device installs a recording handler and a device-lost callback, and
//!   every submission runs under validation / OOM / internal error scopes. Any
//!   fault RETIRES the rung ([`GpuGemm::fault`]) and the caller's ladder
//!   degrades to the next rung with the stated reason — the fill never fails
//!   because of the GPU.
//!
//! Determinism: the shader fixes each output's summation order, so two
//! dispatches of the same compiled pipeline agree bitwise (measured and recorded
//! by the gated `gpu_path_*` oracles); a different driver, device, or `wgpu`
//! release may compile a different order, which is why the sidecar record names
//! the rung that built it (the stamp-gated sidecar admits that variance; the
//! query side never sees this path).

use std::collections::HashMap;
use std::future::Future;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// The GEMM shader with `{{NAME}}` holes for the tile geometry.
const SHADER_TEMPLATE: &str = include_str!("gemm_nt.wgsl");
/// The fused MLP's SwiGLU gate, `{{THREADS}}` hole for the workgroup width.
const GATE_TEMPLATE: &str = include_str!("swiglu.wgsl");

/// Bytes per f32 — the only unit the sizing arithmetic needs.
const F32: u64 = 4;

/// Faults `wgpu` reports outside an error scope (its default handler would
/// panic) and device-loss notices: process-global because the handlers must be
/// `'static`; a rung reads it after every submission and retires on it.
static UNCAPTURED: Mutex<Option<String>> = Mutex::new(None);

fn record_uncaptured(message: String) {
  if let Ok(mut slot) = UNCAPTURED.lock()
    && slot.is_none()
  {
    *slot = Some(message);
  }
}

fn take_uncaptured() -> Option<String> {
  UNCAPTURED.lock().ok().and_then(|mut slot| slot.take())
}

/// Drive a `wgpu` future to completion on this thread. Native `wgpu` futures
/// (adapter/device requests, error-scope pops) resolve without an executor
/// once the device has been polled; readbacks poll explicitly before awaiting.
fn block_on<F: Future>(future: F) -> F::Output {
  let mut future = std::pin::pin!(future);
  let mut context = std::task::Context::from_waker(std::task::Waker::noop());
  loop {
    match future.as_mut().poll(&mut context) {
      std::task::Poll::Ready(value) => return value,
      std::task::Poll::Pending => std::thread::yield_now(),
    }
  }
}

/// Workgroup / tile geometry of one compiled pipeline. `bm × bn` is the C block
/// a workgroup owns (the workgroup is `bm/4 × bn/4` invocations, each owning a
/// 4 × 4 micro-tile — see [`Tile::MICRO`]), `bk4` the vec4-columns of K staged
/// per step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tile {
  pub bm: u32,
  pub bn: u32,
  pub bk4: u32,
}

impl Tile {
  /// Per-invocation micro-tile side — a structural constant of the kernel like
  /// the CPU path's `GEMM_LANES`: four vec4 accumulators (4 rows × 4 columns)
  /// plus eight staged vec4 per K step, written as named registers in the
  /// shader; every staged vec4 feeds four FMAs, the register-bound regime.
  pub const MICRO: u32 = 4;

  /// Derive the geometry from the adapter's limits: the largest SQUARE workgroup
  /// the invocation limit and the per-axis size limit admit (square keeps the
  /// A and B tile traffic balanced), capped at the recorded sweep's winner,
  /// then the deepest K stage the workgroup-memory limit holds, up to the
  /// sweep's winning depth. The caps come from `examples/gpu_gemm_probe.rs`
  /// on the M5 Max at three row scales (364 / 4,690 / 21,853 tokens — the
  /// 26-, 256- and 1,024-surface batches; `BENCHMARKS.md`, "GPU tile sweep"):
  /// 16 × 16 invocations (a 64 × 64 block) with 8 vec4 of K (16 KiB staged)
  /// is the best or within noise of the best at every scale (6.0 TFLOPS at
  /// 21,853 rows vs 5.1 for 128 × 128 and 5.0 for 32 × 32), while 16 vec4 at
  /// 64 × 64 (32 KiB, one workgroup per core) collapses to 1.4 TFLOPS — the
  /// occupancy cliff the memory cap keeps the derivation away from.
  pub fn derive(limits: &wgpu::Limits) -> Result<Tile, String> {
    const SWEEP_SIDE: u32 = 16;
    const SWEEP_BK4: u32 = 8;
    let side_by_count = (limits.max_compute_invocations_per_workgroup as f64).sqrt() as u32;
    let side = side_by_count
      .min(limits.max_compute_workgroup_size_x)
      .min(limits.max_compute_workgroup_size_y)
      .min(SWEEP_SIDE);
    // Power of two so tile strides stay aligned.
    let side = 1u32 << (31 - side.max(1).leading_zeros());
    let (bm, bn) = (side * Self::MICRO, side * Self::MICRO);
    let bytes_per_column = u64::from(bm + bn) * 4 * F32;
    let bk4_by_memory = (u64::from(limits.max_compute_workgroup_storage_size) / bytes_per_column) as u32;
    let bk4 = bk4_by_memory.min(SWEEP_BK4);
    let tile = Tile { bm, bn, bk4 };
    tile.validate(limits)?;
    Ok(tile)
  }

  fn workgroup(&self) -> Result<(u32, u32), String> {
    if self.bm == 0 || self.bn == 0 || self.bm % Self::MICRO != 0 || self.bn % Self::MICRO != 0 {
      return Err(format!("gpu gemm: tile {self:?} is not a whole number of 4×4 micro-tiles"));
    }
    Ok((self.bm / Self::MICRO, self.bn / Self::MICRO))
  }

  /// Workgroup memory the tile stages per K step.
  fn workgroup_bytes(&self) -> u64 {
    u64::from(self.bm + self.bn) * u64::from(self.bk4) * 4 * F32
  }

  /// Typed refusal when the adapter cannot run this geometry.
  pub fn validate(&self, limits: &wgpu::Limits) -> Result<(), String> {
    let (wx, wy) = self.workgroup()?;
    if self.bk4 == 0 {
      return Err(format!(
        "gpu gemm: workgroup memory limit {} B holds no K stage for a {}×{} block",
        limits.max_compute_workgroup_storage_size, self.bm, self.bn
      ));
    }
    if wx > limits.max_compute_workgroup_size_x || wy > limits.max_compute_workgroup_size_y {
      return Err(format!(
        "gpu gemm: workgroup {wx}×{wy} exceeds the per-axis limits {}×{}",
        limits.max_compute_workgroup_size_x, limits.max_compute_workgroup_size_y
      ));
    }
    if wx * wy > limits.max_compute_invocations_per_workgroup {
      return Err(format!(
        "gpu gemm: workgroup {wx}×{wy} exceeds the invocation limit {}",
        limits.max_compute_invocations_per_workgroup
      ));
    }
    if self.workgroup_bytes() > u64::from(limits.max_compute_workgroup_storage_size) {
      return Err(format!(
        "gpu gemm: tile stages {} B of workgroup memory, limit {} B",
        self.workgroup_bytes(),
        limits.max_compute_workgroup_storage_size
      ));
    }
    Ok(())
  }

  fn shader(&self) -> Result<String, String> {
    let (wx, wy) = self.workgroup()?;
    Ok(
      SHADER_TEMPLATE
        .replace("{{BM}}", &self.bm.to_string())
        .replace("{{BN}}", &self.bn.to_string())
        .replace("{{BK4}}", &self.bk4.to_string())
        .replace("{{WX}}", &wx.to_string())
        .replace("{{WY}}", &wy.to_string()),
    )
  }
}

/// Host↔device traffic ledger (bytes and host-observed wall-clock) since the
/// last [`GpuGemm::reset_transfer`] — the bench's transfer-share figure.
#[derive(Clone, Copy, Debug, Default)]
pub struct TransferReport {
  /// Activation bytes written host → device.
  pub bytes_up: u64,
  /// Result bytes read device → host.
  pub bytes_down: u64,
  /// Wall-clock inside `queue.write_buffer` (the host-side staging copy).
  pub upload_secs: f64,
  /// Wall-clock from submit to the readback being mapped — GPU compute plus
  /// the device-side blit into the staging buffer (not separable without
  /// timestamp queries; the bench isolates it with [`GpuGemm::dispatch_only`]).
  pub device_secs: f64,
  /// Wall-clock copying the mapped readback into the caller's slice.
  pub download_secs: f64,
  /// GEMM calls ledgered.
  pub calls: u64,
}

#[derive(Default)]
struct Ledger {
  bytes_up: AtomicU64,
  bytes_down: AtomicU64,
  upload_ns: AtomicU64,
  device_ns: AtomicU64,
  download_ns: AtomicU64,
  calls: AtomicU64,
}

impl Ledger {
  fn add(slot: &AtomicU64, value: u64) {
    slot.fetch_add(value, Ordering::Relaxed);
  }
}

/// One growable device buffer plus its byte capacity.
struct Grown {
  buffer: wgpu::Buffer,
  bytes: u64,
}

/// A GEMM's place in a submission — which scratch buffers it reads and writes
/// and which uniform carries its shape; the bind-group cache key's third
/// component.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Role {
  /// `x → out` under `params` (a stand-alone GEMM).
  Plain,
  /// Fused MLP `x → y` under `params`.
  Fc11,
  /// Fused MLP `x → g` under `params`.
  Fc12,
  /// Fused MLP `y → out` under `params_fc2`.
  Fc2,
}

/// Per-call activation buffers — grown to the largest shape seen, never shrunk
/// (the fill's batches are uniform). Bind groups reference these buffers, so
/// the cache is dropped whenever one regrows. `params` serves the first GEMM of
/// a submission, `params_fc2` the fused MLP's differently-shaped third one, and
/// `gate_extent` the gate (`queue.write_buffer` lands at submission start, so
/// two shapes in one submission need two uniform buffers).
struct Scratch {
  params: wgpu::Buffer,
  params_fc2: wgpu::Buffer,
  gate_extent: wgpu::Buffer,
  x: Option<Grown>,
  out: Option<Grown>,
  staging: Option<Grown>,
  /// Fused-MLP intermediates (`rows × inner` each), device-only.
  y: Option<Grown>,
  g: Option<Grown>,
  groups: HashMap<(usize, usize, Role), wgpu::BindGroup>,
  gate_group: Option<wgpu::BindGroup>,
}

/// A compiled GEMM pipeline on one adapter with the model's weights resident.
pub struct GpuGemm {
  device: wgpu::Device,
  queue: wgpu::Queue,
  pipeline: wgpu::ComputePipeline,
  layout: wgpu::BindGroupLayout,
  gate_pipeline: wgpu::ComputePipeline,
  gate_layout: wgpu::BindGroupLayout,
  /// Invocations per gate workgroup — the GEMM workgroup's count (validated
  /// against the same limits).
  gate_threads: u32,
  tile: Tile,
  /// `max_storage_buffer_binding_size` ∧ `max_buffer_size` — the byte ceiling
  /// of any one bound buffer (weights and activation chunks alike).
  max_binding: u64,
  max_groups_per_dim: u32,
  label: String,
  resident: HashMap<(usize, usize), wgpu::Buffer>,
  resident_bytes: u64,
  scratch: Mutex<Scratch>,
  fault: Mutex<Option<String>>,
  ledger: Ledger,
}

impl std::fmt::Debug for GpuGemm {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("GpuGemm").field("label", &self.label).field("tile", &self.tile).finish()
  }
}

/// Adapter preference: the ordering [`GpuGemm::open`] sorts candidates by.
fn adapter_rank(kind: wgpu::DeviceType) -> u8 {
  match kind {
    wgpu::DeviceType::DiscreteGpu => 0,
    wgpu::DeviceType::IntegratedGpu => 1,
    wgpu::DeviceType::VirtualGpu => 2,
    wgpu::DeviceType::Other => 3,
    wgpu::DeviceType::Cpu => 4,
  }
}

fn backend_name(backend: wgpu::Backend) -> &'static str {
  match backend {
    wgpu::Backend::Metal => "metal",
    wgpu::Backend::Vulkan => "vulkan",
    wgpu::Backend::Dx12 => "dx12",
    wgpu::Backend::Gl => "gl",
    wgpu::Backend::BrowserWebGpu => "webgpu",
    wgpu::Backend::Noop => "noop",
  }
}

impl GpuGemm {
  /// Open the preferred adapter and compile the pipeline for `dims_in` (every
  /// `dim_in` the caller will pass — each must be a multiple of 4, the vec4
  /// read width) with the tile derived from the adapter's limits. Every
  /// failure is a typed refusal naming the adapter (or the absence of one).
  pub fn open(dims_in: &[usize]) -> Result<GpuGemm, String> {
    Self::open_with(dims_in, None)
  }

  /// [`GpuGemm::open`] with an explicit tile — the bench sweep's seam (the
  /// derived tile when `None`).
  pub fn open_with(dims_in: &[usize], tile: Option<Tile>) -> Result<GpuGemm, String> {
    if let Some(odd) = dims_in.iter().find(|d| **d == 0 || **d % 4 != 0) {
      return Err(format!("gpu gemm: dim_in {odd} is not a positive multiple of 4 (vec4 reads)"));
    }
    let allow_software = std::env::var_os("VORPAL_ENCODER_GPU").is_some_and(|v| v == "software");
    // Compute only — no window, no display handle.
    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    descriptor.backends = wgpu::Backends::PRIMARY;
    let instance = wgpu::Instance::new(descriptor);
    let mut adapters: Vec<(u8, wgpu::Adapter)> = block_on(instance.enumerate_adapters(wgpu::Backends::PRIMARY))
      .into_iter()
      .map(|adapter| (adapter_rank(adapter.get_info().device_type), adapter))
      .filter(|(rank, _)| allow_software || *rank < adapter_rank(wgpu::DeviceType::Cpu))
      .collect();
    if adapters.is_empty() {
      return Err(if allow_software {
        "gpu gemm: no Metal/Vulkan/DX12 adapter is present".to_string()
      } else {
        "gpu gemm: no hardware Metal/Vulkan/DX12 adapter is present (software adapters need VORPAL_ENCODER_GPU=software)".to_string()
      });
    }
    adapters.sort_by_key(|(rank, _)| *rank);
    let mut refusals = Vec::new();
    for (_, adapter) in adapters {
      match Self::open_adapter(&adapter, tile) {
        Ok(gemm) => return Ok(gemm),
        Err(reason) => refusals.push(reason),
      }
    }
    Err(refusals.join("; "))
  }

  fn open_adapter(adapter: &wgpu::Adapter, tile: Option<Tile>) -> Result<GpuGemm, String> {
    let info = adapter.get_info();
    let label = format!("wgpu-{}:{}", backend_name(info.backend), info.name);
    let limits = adapter.limits();
    let tile = match tile {
      Some(explicit) => {
        explicit.validate(&limits).map_err(|e| format!("{label}: {e}"))?;
        explicit
      }
      None => Tile::derive(&limits).map_err(|e| format!("{label}: {e}"))?,
    };
    if limits.max_storage_buffers_per_shader_stage < 3 {
      return Err(format!(
        "{label}: {} storage buffers per stage, the kernel binds 3",
        limits.max_storage_buffers_per_shader_stage
      ));
    }
    let max_binding = limits.max_storage_buffer_binding_size.min(limits.max_buffer_size);
    let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
      label: Some("vorpal encoder gemm"),
      required_features: wgpu::Features::empty(),
      required_limits: limits.clone(),
      ..Default::default()
    }))
    .map_err(|e| format!("{label}: device request failed: {e}"))?;
    device.on_uncaptured_error(std::sync::Arc::new(|error: wgpu::Error| {
      record_uncaptured(format!("uncaptured: {error}"));
    }));
    device.set_device_lost_callback(|reason, message| {
      record_uncaptured(format!("device lost ({reason:?}): {message}"));
    });
    let source = tile.shader()?;
    let (wx, wy) = tile.workgroup()?;
    let gate_threads = wx * wy;
    let gate_source = GATE_TEMPLATE.replace("{{THREADS}}", &gate_threads.to_string());
    let build = |name: &'static str, source: String, entries: &[wgpu::BindGroupLayoutEntry]| {
      let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(name),
        source: wgpu::ShaderSource::Wgsl(source.into()),
      });
      let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some(name), entries });
      let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(name),
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
      });
      let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(name),
        layout: Some(&pipeline_layout),
        module: &module,
        entry_point: Some(name),
        compilation_options: Default::default(),
        cache: None,
      });
      (pipeline, layout)
    };
    let buffer = |binding: u32, ty: wgpu::BufferBindingType| wgpu::BindGroupLayoutEntry {
      binding,
      visibility: wgpu::ShaderStages::COMPUTE,
      ty: wgpu::BindingType::Buffer { ty, has_dynamic_offset: false, min_binding_size: None },
      count: None,
    };
    let (pipeline, layout, gate_pipeline, gate_layout) = scoped(&device, || {
      let (pipeline, layout) = build(
        "gemm_nt",
        source,
        &[
          buffer(0, wgpu::BufferBindingType::Uniform),
          buffer(1, wgpu::BufferBindingType::Storage { read_only: true }),
          buffer(2, wgpu::BufferBindingType::Storage { read_only: true }),
          buffer(3, wgpu::BufferBindingType::Storage { read_only: false }),
        ],
      );
      let (gate_pipeline, gate_layout) = build(
        "swiglu",
        gate_source,
        &[
          buffer(0, wgpu::BufferBindingType::Uniform),
          buffer(1, wgpu::BufferBindingType::Storage { read_only: false }),
          buffer(2, wgpu::BufferBindingType::Storage { read_only: true }),
        ],
      );
      Ok((pipeline, layout, gate_pipeline, gate_layout))
    })
    .map_err(|e| format!("{label}: pipeline: {e}"))?;
    let uniform = |name: &'static str| {
      device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(name),
        size: 4 * F32,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
      })
    };
    let scratch = Scratch {
      params: uniform("gemm params"),
      params_fc2: uniform("gemm params fc2"),
      gate_extent: uniform("gate extent"),
      x: None,
      out: None,
      staging: None,
      y: None,
      g: None,
      groups: HashMap::new(),
      gate_group: None,
    };
    Ok(GpuGemm {
      device,
      queue,
      pipeline,
      layout,
      gate_pipeline,
      gate_layout,
      gate_threads,
      tile,
      max_binding,
      max_groups_per_dim: limits.max_compute_workgroups_per_dimension,
      label,
      resident: HashMap::new(),
      resident_bytes: 0,
      scratch: Mutex::new(scratch),
      fault: Mutex::new(None),
      ledger: Ledger::default(),
    })
  }

  /// `wgpu-<backend>:<adapter name>` — the provenance label a sidecar records.
  pub fn label(&self) -> &str {
    &self.label
  }

  /// The compiled geometry.
  pub fn tile(&self) -> Tile {
    self.tile
  }

  /// Bytes of weights resident on the device.
  pub fn resident_bytes(&self) -> u64 {
    self.resident_bytes
  }

  /// The runtime fault that retired this rung, if any (device lost, OOM,
  /// validation) — the ladder's stated reason for degrading.
  pub fn fault(&self) -> Option<String> {
    self.fault.lock().ok().and_then(|slot| slot.clone())
  }

  /// Retire the rung with `reason`: every later [`GpuGemm::gemm`] returns it
  /// immediately (a lost device would otherwise fail slowly, call after call).
  pub fn retire(&self, reason: String) {
    if let Ok(mut slot) = self.fault.lock()
      && slot.is_none()
    {
      *slot = Some(reason);
    }
  }

  /// Upload one weight matrix (row-major `[rows_out][dim_in]`, as the caller
  /// will later pass it by reference) so GEMMs against it read device memory.
  pub fn make_resident(&mut self, w: &[f32]) -> Result<(), String> {
    let key = (w.as_ptr() as usize, w.len());
    if self.resident.contains_key(&key) {
      return Ok(());
    }
    let bytes = w.len() as u64 * F32;
    if bytes > self.max_binding {
      return Err(format!(
        "{}: weight of {bytes} B exceeds the binding limit {} B",
        self.label, self.max_binding
      ));
    }
    let buffer = scoped(&self.device, || {
      let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("gemm weight"),
        size: aligned(bytes),
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
      });
      self.queue.write_buffer(&buffer, 0, bytemuck::cast_slice(w));
      self.queue.submit(std::iter::empty());
      self
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|e| format!("poll after weight upload: {e}"))?;
      Ok(buffer)
    })
    .map_err(|e| format!("{}: weight upload: {e}", self.label))?;
    self.resident.insert(key, buffer);
    self.resident_bytes += bytes;
    Ok(())
  }

  /// The resident device buffer for a host weight slice, or the typed miss.
  fn weight(&self, w: &[f32]) -> Result<&wgpu::Buffer, String> {
    self
      .resident
      .get(&(w.as_ptr() as usize, w.len()))
      .ok_or_else(|| format!("{}: weight matrix is not resident on the device", self.label))
  }

  /// Rows per submission for a block whose widest activation row is `widest`
  /// f32s: bounded by the binding limit, the dispatch grid, and `rows` itself.
  fn rows_per_chunk(&self, widest: usize, rows: usize) -> Result<usize, String> {
    let row_bytes = widest as u64 * F32;
    let chunk = (self.max_binding / row_bytes)
      .min(u64::from(self.max_groups_per_dim) * u64::from(self.tile.bm))
      .min(rows as u64) as usize;
    if chunk == 0 {
      return Err(format!(
        "{}: one row of {row_bytes} B exceeds the binding limit {} B",
        self.label, self.max_binding
      ));
    }
    Ok(chunk)
  }

  fn grid_admits(&self, rows_out: usize) -> Result<(), String> {
    if u64::from(self.max_groups_per_dim) * u64::from(self.tile.bn) < rows_out as u64 {
      return Err(format!("{}: rows_out {rows_out} exceeds the dispatch grid", self.label));
    }
    Ok(())
  }

  /// Populate the bind-group cache for `(weight, role)` against the current
  /// scratch buffers.
  fn bind(&self, scratch: &mut Scratch, role: Role, w: &[f32]) -> Result<(), String> {
    let key = (w.as_ptr() as usize, w.len(), role);
    if scratch.groups.contains_key(&key) {
      return Ok(());
    }
    let weight = self.weight(w)?;
    let missing = || format!("{}: scratch buffers missing after growth (invariant)", self.label);
    let (params, input, output) = match role {
      Role::Plain => (&scratch.params, scratch.x.as_ref(), scratch.out.as_ref()),
      Role::Fc11 => (&scratch.params, scratch.x.as_ref(), scratch.y.as_ref()),
      Role::Fc12 => (&scratch.params, scratch.x.as_ref(), scratch.g.as_ref()),
      Role::Fc2 => (&scratch.params_fc2, scratch.y.as_ref(), scratch.out.as_ref()),
    };
    let (Some(input), Some(output)) = (input, output) else {
      return Err(missing());
    };
    let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
      label: Some("gemm_nt"),
      layout: &self.layout,
      entries: &[
        wgpu::BindGroupEntry { binding: 0, resource: params.as_entire_binding() },
        wgpu::BindGroupEntry { binding: 1, resource: input.buffer.as_entire_binding() },
        wgpu::BindGroupEntry { binding: 2, resource: weight.as_entire_binding() },
        wgpu::BindGroupEntry { binding: 3, resource: output.buffer.as_entire_binding() },
      ],
    });
    scratch.groups.insert(key, group);
    Ok(())
  }

  /// One GEMM dispatch in its own pass (a pass boundary orders it after the
  /// previous dispatch's writes on every backend).
  fn encode_gemm(&self, encoder: &mut wgpu::CommandEncoder, group: &wgpu::BindGroup, rows: usize, rows_out: usize) {
    let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("gemm_nt"), timestamp_writes: None });
    pass.set_pipeline(&self.pipeline);
    pass.set_bind_group(0, group, &[]);
    pass.dispatch_workgroups((rows as u32).div_ceil(self.tile.bm), (rows_out as u32).div_ceil(self.tile.bn), 1);
  }

  /// Submit `encoder` with the blit of `out_bytes` from `out_buf` into
  /// `staging` appended, wait, and copy the mapped result into `out`; ledger
  /// the call's phases from `started` (before the uploads) and `uploaded`.
  #[allow(clippy::too_many_arguments)]
  fn finish_chunk(
    &self,
    mut encoder: wgpu::CommandEncoder,
    out_buf: &Grown,
    staging: &Grown,
    out: &mut [f32],
    bytes_up: u64,
    started: std::time::Instant,
    uploaded: std::time::Instant,
  ) -> Result<(), String> {
    let out_bytes = out.len() as u64 * F32;
    encoder.copy_buffer_to_buffer(&out_buf.buffer, 0, &staging.buffer, 0, aligned(out_bytes));
    self.queue.submit(Some(encoder.finish()));
    let (sender, receiver) = std::sync::mpsc::channel();
    staging
      .buffer
      .slice(0..aligned(out_bytes))
      .map_async(wgpu::MapMode::Read, move |result| {
        let _ = sender.send(result);
      });
    self
      .device
      .poll(wgpu::PollType::wait_indefinitely())
      .map_err(|e| format!("poll: {e}"))?;
    receiver
      .recv()
      .map_err(|_| "readback callback never fired".to_string())?
      .map_err(|e| format!("readback map: {e}"))?;
    let computed = std::time::Instant::now();
    {
      let view = staging
        .buffer
        .slice(0..aligned(out_bytes))
        .get_mapped_range()
        .map_err(|e| format!("readback range: {e}"))?;
      let bytes: &[u8] = &view[..out_bytes as usize];
      match bytemuck::try_cast_slice::<u8, f32>(bytes) {
        Ok(values) => out.copy_from_slice(values),
        Err(_) => {
          for (slot, chunk) in out.iter_mut().zip(bytes.chunks_exact(4)) {
            *slot = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
          }
        }
      }
    }
    staging.buffer.unmap();
    let finished = std::time::Instant::now();
    Ledger::add(&self.ledger.bytes_up, bytes_up);
    Ledger::add(&self.ledger.bytes_down, out_bytes);
    Ledger::add(&self.ledger.upload_ns, (uploaded - started).as_nanos() as u64);
    Ledger::add(&self.ledger.device_ns, (computed - uploaded).as_nanos() as u64);
    Ledger::add(&self.ledger.download_ns, (finished - computed).as_nanos() as u64);
    Ledger::add(&self.ledger.calls, 1);
    Ok(())
  }

  /// `out[rows × rows_out] = x[rows × dim_in] · wᵀ`, `w` resident. Shapes were
  /// validated by the caller; here the rows are chunked to the binding limit
  /// and each chunk is one submission (upload, dispatch, blit, readback).
  pub fn gemm(
    &self,
    x: &[f32],
    dim_in: usize,
    w: &[f32],
    rows_out: usize,
    rows: usize,
    out: &mut [f32],
  ) -> Result<(), String> {
    if let Some(fault) = self.fault() {
      return Err(fault);
    }
    if dim_in % 4 != 0 {
      return Err(format!("{}: dim_in {dim_in} is not a multiple of 4", self.label));
    }
    self.grid_admits(rows_out)?;
    let rows_per_chunk = self.rows_per_chunk(dim_in.max(rows_out), rows)?;
    let mut scratch = self.scratch.lock().map_err(|_| format!("{}: scratch lock poisoned", self.label))?;
    let x_bytes = rows_per_chunk as u64 * dim_in as u64 * F32;
    let out_bytes = rows_per_chunk as u64 * rows_out as u64 * F32;
    self.ensure_scratch(&mut scratch, x_bytes, out_bytes, None)?;
    self.bind(&mut scratch, Role::Plain, w)?;
    let scratch = &*scratch;
    let (Some(x_buf), Some(out_buf), Some(staging)) = (&scratch.x, &scratch.out, &scratch.staging) else {
      return Err(format!("{}: scratch buffers missing after growth (invariant)", self.label));
    };
    let Some(group) = scratch.groups.get(&(w.as_ptr() as usize, w.len(), Role::Plain)) else {
      return Err(format!("{}: bind group missing after insert (invariant)", self.label));
    };
    let result = scoped(&self.device, || {
      for first in (0..rows).step_by(rows_per_chunk) {
        let count = rows_per_chunk.min(rows - first);
        let x_chunk = &x[first * dim_in..(first + count) * dim_in];
        let out_chunk = &mut out[first * rows_out..(first + count) * rows_out];
        let started = std::time::Instant::now();
        let header = [count as u32, rows_out as u32, (dim_in / 4) as u32, 0u32];
        self.queue.write_buffer(&scratch.params, 0, bytemuck::cast_slice(&header));
        self.queue.write_buffer(&x_buf.buffer, 0, bytemuck::cast_slice(x_chunk));
        let uploaded = std::time::Instant::now();
        let mut encoder = self
          .device
          .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("gemm_nt") });
        self.encode_gemm(&mut encoder, group, count, rows_out);
        self.finish_chunk(encoder, out_buf, staging, out_chunk, x_chunk.len() as u64 * F32, started, uploaded)?;
      }
      Ok(())
    });
    if let Some(uncaptured) = take_uncaptured() {
      return Err(format!("{}: {uncaptured}", self.label));
    }
    result.map_err(|e| format!("{}: {e}", self.label))
  }

  /// The fused MLP block: `out = (x·fc11ᵀ ⊙ silu(x·fc12ᵀ)) · fc2ᵀ` with the
  /// two `rows × inner` intermediates and the gate on the device — only `x`
  /// goes up and `out` comes down (the encoder's MLP is ~3/4 of its GEMM
  /// FLOPs and, un-fused, ~2/3 of its host↔device bytes). All three weights
  /// resident; one submission per row chunk.
  #[allow(clippy::too_many_arguments)]
  pub fn mlp(
    &self,
    x: &[f32],
    dim: usize,
    fc11: &[f32],
    fc12: &[f32],
    fc2: &[f32],
    inner: usize,
    rows: usize,
    out: &mut [f32],
  ) -> Result<(), String> {
    if let Some(fault) = self.fault() {
      return Err(fault);
    }
    if dim % 4 != 0 || inner % 4 != 0 {
      return Err(format!("{}: dim {dim} / inner {inner} not multiples of 4", self.label));
    }
    if x.len() < rows * dim
      || out.len() != rows * dim
      || fc11.len() < inner * dim
      || fc12.len() < inner * dim
      || fc2.len() < dim * inner
    {
      return Err(format!("{}: MLP operand shapes disagree", self.label));
    }
    self.grid_admits(inner)?;
    self.grid_admits(dim)?;
    let rows_per_chunk = self.rows_per_chunk(dim.max(inner), rows)?;
    let mut scratch = self.scratch.lock().map_err(|_| format!("{}: scratch lock poisoned", self.label))?;
    let x_bytes = rows_per_chunk as u64 * dim as u64 * F32;
    let mid_bytes = rows_per_chunk as u64 * inner as u64 * F32;
    self.ensure_scratch(&mut scratch, x_bytes, x_bytes, Some(mid_bytes))?;
    self.bind(&mut scratch, Role::Fc11, fc11)?;
    self.bind(&mut scratch, Role::Fc12, fc12)?;
    self.bind(&mut scratch, Role::Fc2, fc2)?;
    if scratch.gate_group.is_none() {
      let (Some(y), Some(g)) = (&scratch.y, &scratch.g) else {
        return Err(format!("{}: MLP scratch missing after growth (invariant)", self.label));
      };
      scratch.gate_group = Some(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("swiglu"),
        layout: &self.gate_layout,
        entries: &[
          wgpu::BindGroupEntry { binding: 0, resource: scratch.gate_extent.as_entire_binding() },
          wgpu::BindGroupEntry { binding: 1, resource: y.buffer.as_entire_binding() },
          wgpu::BindGroupEntry { binding: 2, resource: g.buffer.as_entire_binding() },
        ],
      }));
    }
    let scratch = &*scratch;
    let (Some(x_buf), Some(out_buf), Some(staging), Some(gate_group)) =
      (&scratch.x, &scratch.out, &scratch.staging, &scratch.gate_group)
    else {
      return Err(format!("{}: scratch buffers missing after growth (invariant)", self.label));
    };
    let group_of = |w: &[f32], role: Role| {
      scratch
        .groups
        .get(&(w.as_ptr() as usize, w.len(), role))
        .ok_or_else(|| format!("{}: bind group missing after insert (invariant)", self.label))
    };
    let (g11, g12, g2) = (group_of(fc11, Role::Fc11)?, group_of(fc12, Role::Fc12)?, group_of(fc2, Role::Fc2)?);
    let result = scoped(&self.device, || {
      for first in (0..rows).step_by(rows_per_chunk) {
        let count = rows_per_chunk.min(rows - first);
        let x_chunk = &x[first * dim..(first + count) * dim];
        let out_chunk = &mut out[first * dim..(first + count) * dim];
        // Gate grid: n4 vec4s over a 2-D dispatch (x capped by the per-dimension limit).
        let n4 = (count * inner / 4) as u64;
        let groups = n4.div_ceil(u64::from(self.gate_threads));
        let groups_x = groups.min(u64::from(self.max_groups_per_dim));
        let groups_y = groups.div_ceil(groups_x.max(1));
        if groups_y > u64::from(self.max_groups_per_dim) {
          return Err(format!("gate grid {groups} workgroups exceeds the dispatch limits"));
        }
        let started = std::time::Instant::now();
        let fc1 = [count as u32, inner as u32, (dim / 4) as u32, 0u32];
        let fc2_header = [count as u32, dim as u32, (inner / 4) as u32, 0u32];
        let extent = [n4 as u32, groups_x as u32, 0u32, 0u32];
        self.queue.write_buffer(&scratch.params, 0, bytemuck::cast_slice(&fc1));
        self.queue.write_buffer(&scratch.params_fc2, 0, bytemuck::cast_slice(&fc2_header));
        self.queue.write_buffer(&scratch.gate_extent, 0, bytemuck::cast_slice(&extent));
        self.queue.write_buffer(&x_buf.buffer, 0, bytemuck::cast_slice(x_chunk));
        let uploaded = std::time::Instant::now();
        let mut encoder = self
          .device
          .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("mlp") });
        self.encode_gemm(&mut encoder, g11, count, inner);
        self.encode_gemm(&mut encoder, g12, count, inner);
        {
          let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("swiglu"), timestamp_writes: None });
          pass.set_pipeline(&self.gate_pipeline);
          pass.set_bind_group(0, gate_group, &[]);
          pass.dispatch_workgroups(groups_x as u32, groups_y as u32, 1);
        }
        self.encode_gemm(&mut encoder, g2, count, dim);
        self.finish_chunk(encoder, out_buf, staging, out_chunk, x_chunk.len() as u64 * F32, started, uploaded)?;
      }
      Ok(())
    });
    if let Some(uncaptured) = take_uncaptured() {
      return Err(format!("{}: {uncaptured}", self.label));
    }
    result.map_err(|e| format!("{}: {e}", self.label))
  }

  /// Measurement seam: the dispatch alone for a `rows × dim_in · wᵀ` shape —
  /// no upload, no readback (whatever the scratch buffers hold) — so the bench
  /// can subtract device compute from the round trip. Returns the wall-clock of
  /// `reps` back-to-back submissions.
  pub fn dispatch_only(
    &self,
    dim_in: usize,
    w: &[f32],
    rows_out: usize,
    rows: usize,
    reps: usize,
  ) -> Result<f64, String> {
    let x_bytes = rows as u64 * dim_in as u64 * F32;
    let out_bytes = rows as u64 * rows_out as u64 * F32;
    if x_bytes > self.max_binding || out_bytes > self.max_binding {
      return Err(format!("{}: shape exceeds one binding", self.label));
    }
    let mut scratch = self.scratch.lock().map_err(|_| format!("{}: scratch lock poisoned", self.label))?;
    self.ensure_scratch(&mut scratch, x_bytes, out_bytes, None)?;
    self.bind(&mut scratch, Role::Plain, w)?;
    let scratch = &*scratch;
    let Some(group) = scratch.groups.get(&(w.as_ptr() as usize, w.len(), Role::Plain)) else {
      return Err(format!("{}: bind group missing after insert (invariant)", self.label));
    };
    let header = [rows as u32, rows_out as u32, (dim_in / 4) as u32, 0u32];
    self.queue.write_buffer(&scratch.params, 0, bytemuck::cast_slice(&header));
    scoped(&self.device, || {
      self.queue.submit(std::iter::empty());
      self.device.poll(wgpu::PollType::wait_indefinitely()).map_err(|e| format!("poll: {e}"))?;
      let started = std::time::Instant::now();
      for _ in 0..reps {
        let mut encoder = self
          .device
          .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("gemm_nt bench") });
        self.encode_gemm(&mut encoder, group, rows, rows_out);
        self.queue.submit(Some(encoder.finish()));
      }
      self.device.poll(wgpu::PollType::wait_indefinitely()).map_err(|e| format!("poll: {e}"))?;
      Ok(started.elapsed().as_secs_f64())
    })
    .map_err(|e| format!("{}: {e}", self.label))
  }

  /// Grow the activation buffers to hold `x_bytes` / `out_bytes` (and, for
  /// the fused MLP, `mid_bytes` for each intermediate), dropping the bind-group
  /// caches when any buffer is replaced.
  fn ensure_scratch(&self, scratch: &mut Scratch, x_bytes: u64, out_bytes: u64, mid_bytes: Option<u64>) -> Result<(), String> {
    let mut regrown = false;
    let mut grow = |slot: &mut Option<Grown>, bytes: u64, label: &'static str, usage: wgpu::BufferUsages| {
      if slot.as_ref().is_some_and(|g| g.bytes >= bytes) {
        return Ok(());
      }
      let buffer = scoped(&self.device, || {
        Ok(self.device.create_buffer(&wgpu::BufferDescriptor {
          label: Some(label),
          size: aligned(bytes),
          usage,
          mapped_at_creation: false,
        }))
      })?;
      *slot = Some(Grown { buffer, bytes });
      regrown = true;
      Ok::<(), String>(())
    };
    grow(&mut scratch.x, x_bytes, "gemm x", wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST)?;
    grow(&mut scratch.out, out_bytes, "gemm out", wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC)?;
    grow(&mut scratch.staging, out_bytes, "gemm readback", wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST)?;
    if let Some(mid) = mid_bytes {
      grow(&mut scratch.y, mid, "mlp y", wgpu::BufferUsages::STORAGE)?;
      grow(&mut scratch.g, mid, "mlp gate", wgpu::BufferUsages::STORAGE)?;
    }
    if regrown {
      scratch.groups.clear();
      scratch.gate_group = None;
    }
    Ok(())
  }

  /// The traffic ledger since the last reset.
  pub fn transfer_report(&self) -> TransferReport {
    TransferReport {
      bytes_up: self.ledger.bytes_up.load(Ordering::Relaxed),
      bytes_down: self.ledger.bytes_down.load(Ordering::Relaxed),
      upload_secs: self.ledger.upload_ns.load(Ordering::Relaxed) as f64 * 1e-9,
      device_secs: self.ledger.device_ns.load(Ordering::Relaxed) as f64 * 1e-9,
      download_secs: self.ledger.download_ns.load(Ordering::Relaxed) as f64 * 1e-9,
      calls: self.ledger.calls.load(Ordering::Relaxed),
    }
  }

  /// Zero the traffic ledger (bench: between warm-up and the timed reps).
  pub fn reset_transfer(&self) {
    for slot in [
      &self.ledger.bytes_up,
      &self.ledger.bytes_down,
      &self.ledger.upload_ns,
      &self.ledger.device_ns,
      &self.ledger.download_ns,
      &self.ledger.calls,
    ] {
      slot.store(0, Ordering::Relaxed);
    }
  }
}

/// Round a byte count up to the copy/map alignment `wgpu` requires of buffer
/// sizes and mapped ranges.
fn aligned(bytes: u64) -> u64 {
  let unit = wgpu::MAP_ALIGNMENT.max(wgpu::COPY_BUFFER_ALIGNMENT);
  bytes.div_ceil(unit) * unit
}

/// Run `work` under validation / out-of-memory / internal error scopes and
/// turn any captured error into a typed `Err` — the no-panic contract.
fn scoped<T>(device: &wgpu::Device, work: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
  let internal = device.push_error_scope(wgpu::ErrorFilter::Internal);
  let memory = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
  let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
  let out = work();
  let mut errors: Vec<String> = Vec::new();
  for guard in [validation, memory, internal] {
    if let Some(error) = block_on(guard.pop()) {
      errors.push(error.to_string());
    }
  }
  match (out, errors.is_empty()) {
    (Ok(value), true) => Ok(value),
    (Err(reason), true) => Err(reason),
    (Ok(_), false) => Err(errors.join("; ")),
    (Err(reason), false) => Err(format!("{reason}; {}", errors.join("; "))),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn tile_derivation_respects_every_limit() {
    let mut limits = wgpu::Limits {
      max_compute_invocations_per_workgroup: 256,
      max_compute_workgroup_size_x: 256,
      max_compute_workgroup_size_y: 256,
      max_compute_workgroup_storage_size: 16384,
      ..Default::default()
    };
    let tile = Tile::derive(&limits).unwrap();
    assert_eq!((tile.bm, tile.bn), (64, 64));
    assert_eq!(tile.bk4, 8);
    // A tighter workgroup-memory limit shrinks the K stage, never the block.
    limits.max_compute_workgroup_storage_size = 4096;
    let tile = Tile::derive(&limits).unwrap();
    assert_eq!((tile.bm, tile.bn, tile.bk4), (64, 64, 2));
    // A smaller invocation limit shrinks the block (square, power of two).
    limits.max_compute_invocations_per_workgroup = 64;
    let tile = Tile::derive(&limits).unwrap();
    assert_eq!((tile.bm, tile.bn), (32, 32));
    // Too little workgroup memory for even one K stage is a typed refusal.
    limits.max_compute_workgroup_storage_size = 512;
    assert!(Tile::derive(&limits).is_err());
  }

  #[test]
  fn shader_template_is_fully_substituted() {
    let tile = Tile { bm: 64, bn: 64, bk4: 8 };
    let source = tile.shader().unwrap();
    assert!(!source.contains("{{"), "unsubstituted hole in the shader");
    assert!(source.contains("const WX: u32 = 16u;") && source.contains("const WY: u32 = 16u;"));
  }

  /// Runs only where an adapter exists (hardware, or software under
  /// `VORPAL_ENCODER_GPU=software`); otherwise states the refusal and passes.
  #[test]
  fn gemm_matches_a_scalar_reference_on_ragged_shapes() {
    let mut gpu = match GpuGemm::open(&[20]) {
      Ok(gpu) => gpu,
      Err(reason) => {
        eprintln!("skipped: {reason}");
        return;
      }
    };
    // rows 37, dim_in 20, rows_out 70: every edge is a partial tile.
    let (rows, dim_in, rows_out) = (37usize, 20usize, 70usize);
    let x: Vec<f32> = (0..rows * dim_in).map(|i| ((i * 7919) % 97) as f32 / 31.0 - 1.5).collect();
    let w: Vec<f32> = (0..rows_out * dim_in).map(|i| ((i * 104729) % 89) as f32 / 29.0 - 1.4).collect();
    gpu.make_resident(&w).unwrap();
    let mut out = vec![0.0f32; rows * rows_out];
    gpu.gemm(&x, dim_in, &w, rows_out, rows, &mut out).unwrap();
    for r in 0..rows {
      for o in 0..rows_out {
        let expect: f64 = (0..dim_in).map(|d| x[r * dim_in + d] as f64 * w[o * dim_in + d] as f64).sum();
        let got = out[r * rows_out + o] as f64;
        assert!((got - expect).abs() <= 1e-4 * expect.abs().max(1.0), "({r},{o}): {got} vs {expect}");
      }
    }
    // Two dispatches of the same pipeline agree bitwise.
    let mut again = vec![0.0f32; rows * rows_out];
    gpu.gemm(&x, dim_in, &w, rows_out, rows, &mut again).unwrap();
    assert_eq!(bytemuck::cast_slice::<f32, u32>(&out), bytemuck::cast_slice::<f32, u32>(&again));
    // A non-resident weight is a typed error, not an upload.
    let other = vec![0.5f32; rows_out * dim_in];
    assert!(gpu.gemm(&x, dim_in, &other, rows_out, rows, &mut out).is_err());
    eprintln!("gpu gemm on {} tile {:?}: ok", gpu.label(), gpu.tile());
  }
}

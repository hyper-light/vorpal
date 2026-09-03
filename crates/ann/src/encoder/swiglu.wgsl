// SwiGLU gate, element-wise on the device-resident MLP intermediates:
// y[i] = y[i] · silu(g[i]) = y[i] · g[i] / (1 + exp(−g[i])), vec4-wide (the
// host refuses inner % 4 ≠ 0). Part of the fused MLP (gemm_wgpu.rs `mlp`):
// fc11 and fc12 outputs stay on the device, this gate runs between them and
// fc2, so only the block's input and output cross the host boundary. The
// throughput CPU path uses a polynomial `exp`; here it is the device's — both
// are inside the parity oracle's bound, neither is the query-side law.
// The invocation count per workgroup and the 2-D grid split are substituted by
// the host from the adapter's limits.

struct Extent {
  n4: u32,
  groups_x: u32,
  pad0: u32,
  pad1: u32,
}

@group(0) @binding(0) var<uniform> extent: Extent;
@group(0) @binding(1) var<storage, read_write> y: array<vec4<f32>>;
@group(0) @binding(2) var<storage, read> g: array<vec4<f32>>;

const THREADS: u32 = {{THREADS}}u;

@compute @workgroup_size(THREADS, 1, 1)
fn swiglu(
  @builtin(workgroup_id) group: vec3<u32>,
  @builtin(local_invocation_index) thread: u32,
) {
  let i = (group.y * extent.groups_x + group.x) * THREADS + thread;
  if (i < extent.n4) {
    let gate = g[i];
    y[i] = y[i] * (gate / (vec4<f32>(1.0) + exp(-gate)));
  }
}

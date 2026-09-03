// Tiled f32 GEMM, "NT" form: C[m][n] = Σ_k A[m][k] · B[n][k] — both operands
// row-major with K innermost (the encoder's `x · Wᵀ` with W stored [out][in]),
// read as vec4 along K (the host refuses K % 4 ≠ 0). One workgroup owns a
// BM × BN block of C; each invocation owns a 4 × 4 micro-tile held in FOUR
// vec4 register accumulators (row i → acc_i, its four columns in the lanes —
// named registers, never a runtime-indexed private array, which naga would
// bounds-check on every access); the K walk stages BK4 vec4-columns of both
// operands in workgroup memory per step. Every double-braced NAME hole is
// substituted by the host from the tile it derived from `adapter.limits()`
// (gemm_wgpu.rs — nothing here is a fixed size).
//
// Tile layouts are chosen so that neighbouring invocations read neighbouring
// words: A is staged k-major as [kk][row] and invocation x owns rows
// x, x+WX, x+2WX, x+3WX (its four reads per kk are consecutive across x); B is
// staged TRANSPOSED as [k lane][column quad] so one vec4 read yields four
// columns' values at one k (the same address for every x — a broadcast).
//
// Summation order per output element is FIXED by this source (k ascending, the
// four lanes of each vec4 in x,y,z,w order through `fma`), so two dispatches of
// the same compiled pipeline agree bitwise; the compiled code may differ across
// drivers/devices, which is why the sidecar records the rung that built it.

struct Params {
  m: u32,
  n: u32,
  k4: u32,
  pad: u32,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> a: array<vec4<f32>>;
@group(0) @binding(2) var<storage, read> b: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read_write> c: array<f32>;

const BM: u32 = {{BM}}u;
const BN: u32 = {{BN}}u;
const BK4: u32 = {{BK4}}u;
const WX: u32 = {{WX}}u;   // BM / 4
const WY: u32 = {{WY}}u;   // BN / 4 (column quads)
const THREADS: u32 = WX * WY;

var<workgroup> tile_a: array<vec4<f32>, BM * BK4>;
var<workgroup> tile_bt: array<vec4<f32>, BK4 * 4u * WY>;

@compute @workgroup_size(WX, WY, 1)
fn gemm_nt(
  @builtin(workgroup_id) group: vec3<u32>,
  @builtin(local_invocation_id) local: vec3<u32>,
  @builtin(local_invocation_index) thread: u32,
) {
  let a_base = group.x * BM;
  let b_base = group.y * BN;
  let x = local.x;
  let q = local.y;
  var acc0 = vec4<f32>(0.0);
  var acc1 = vec4<f32>(0.0);
  var acc2 = vec4<f32>(0.0);
  var acc3 = vec4<f32>(0.0);
  for (var k0 = 0u; k0 < params.k4; k0 += BK4) {
    // Cooperative loads: consecutive invocations read consecutive vec4s of one
    // row (coalesced); out-of-range → 0.
    for (var idx = thread; idx < BM * BK4; idx += THREADS) {
      let r = idx / BK4;
      let kk = idx % BK4;
      let gr = a_base + r;
      let gk = k0 + kk;
      var v = vec4<f32>(0.0);
      if (gr < params.m && gk < params.k4) {
        v = a[gr * params.k4 + gk];
      }
      // Row r of the block belongs to invocation x = r % WX as its i = r / WX.
      tile_a[kk * BM + r] = v;
    }
    // B: one invocation per (column quad, kk) loads the quad's four rows at that
    // k-vec4 and writes them transposed — four whole vec4s, one per k lane.
    for (var idx = thread; idx < WY * BK4; idx += THREADS) {
      let quad = idx / BK4;
      let kk = idx % BK4;
      let gk = k0 + kk;
      let r0 = b_base + quad * 4u;
      var v0 = vec4<f32>(0.0);
      var v1 = vec4<f32>(0.0);
      var v2 = vec4<f32>(0.0);
      var v3 = vec4<f32>(0.0);
      if (gk < params.k4) {
        if (r0 < params.n) { v0 = b[r0 * params.k4 + gk]; }
        if (r0 + 1u < params.n) { v1 = b[(r0 + 1u) * params.k4 + gk]; }
        if (r0 + 2u < params.n) { v2 = b[(r0 + 2u) * params.k4 + gk]; }
        if (r0 + 3u < params.n) { v3 = b[(r0 + 3u) * params.k4 + gk]; }
      }
      let base = kk * 4u * WY + quad;
      tile_bt[base] = vec4<f32>(v0.x, v1.x, v2.x, v3.x);
      tile_bt[base + WY] = vec4<f32>(v0.y, v1.y, v2.y, v3.y);
      tile_bt[base + 2u * WY] = vec4<f32>(v0.z, v1.z, v2.z, v3.z);
      tile_bt[base + 3u * WY] = vec4<f32>(v0.w, v1.w, v2.w, v3.w);
    }
    workgroupBarrier();
    for (var kk = 0u; kk < BK4; kk++) {
      let av0 = tile_a[kk * BM + 0u * WX + x];
      let av1 = tile_a[kk * BM + 1u * WX + x];
      let av2 = tile_a[kk * BM + 2u * WX + x];
      let av3 = tile_a[kk * BM + 3u * WX + x];
      let bq = kk * 4u * WY + q;
      let b0 = tile_bt[bq];
      let b1 = tile_bt[bq + WY];
      let b2 = tile_bt[bq + 2u * WY];
      let b3 = tile_bt[bq + 3u * WY];
      acc0 = fma(vec4<f32>(av0.x), b0, acc0);
      acc0 = fma(vec4<f32>(av0.y), b1, acc0);
      acc0 = fma(vec4<f32>(av0.z), b2, acc0);
      acc0 = fma(vec4<f32>(av0.w), b3, acc0);
      acc1 = fma(vec4<f32>(av1.x), b0, acc1);
      acc1 = fma(vec4<f32>(av1.y), b1, acc1);
      acc1 = fma(vec4<f32>(av1.z), b2, acc1);
      acc1 = fma(vec4<f32>(av1.w), b3, acc1);
      acc2 = fma(vec4<f32>(av2.x), b0, acc2);
      acc2 = fma(vec4<f32>(av2.y), b1, acc2);
      acc2 = fma(vec4<f32>(av2.z), b2, acc2);
      acc2 = fma(vec4<f32>(av2.w), b3, acc2);
      acc3 = fma(vec4<f32>(av3.x), b0, acc3);
      acc3 = fma(vec4<f32>(av3.y), b1, acc3);
      acc3 = fma(vec4<f32>(av3.z), b2, acc3);
      acc3 = fma(vec4<f32>(av3.w), b3, acc3);
    }
    workgroupBarrier();
  }
  let col0 = b_base + q * 4u;
  store_row(a_base + 0u * WX + x, col0, acc0);
  store_row(a_base + 1u * WX + x, col0, acc1);
  store_row(a_base + 2u * WX + x, col0, acc2);
  store_row(a_base + 3u * WX + x, col0, acc3);
}

fn store_row(r: u32, col0: u32, v: vec4<f32>) {
  if (r >= params.m) {
    return;
  }
  let base = r * params.n;
  if (col0 + 3u < params.n) {
    c[base + col0] = v.x;
    c[base + col0 + 1u] = v.y;
    c[base + col0 + 2u] = v.z;
    c[base + col0 + 3u] = v.w;
    return;
  }
  if (col0 < params.n) { c[base + col0] = v.x; }
  if (col0 + 1u < params.n) { c[base + col0 + 1u] = v.y; }
  if (col0 + 2u < params.n) { c[base + col0 + 2u] = v.z; }
}

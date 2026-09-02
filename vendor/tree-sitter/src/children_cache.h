// VORPAL VENDORED ADDITION (see docs/wip/UPSTREAM.md): a thread-local,
// size-classed free cache for internal-node "children blocks" — the single
// allocation per internal AST node that holds `[children..., SubtreeHeapData]`.
//
// Upstream mallocs one such block per reduction (born in `stack__iter`,
// claimed in place by `ts_subtree_new_node`) and frees it raw in
// `ts_subtree_release`; `ts_tree_delete` uses a throwaway SubtreePool, so
// NOTHING recycles across parses. Ledger-profiled across all 45 vendored
// grammar corpora, this one callsite family is 24–93 % of every grammar's
// C-side allocations (~240 M of 273 M at kernel scale). Leaf headers already
// recycle through SubtreePool; this cache gives children blocks the same
// treatment, and — because it is thread-local and survives the parser — the
// blocks a dropped tree returns are reused by the NEXT file parsed on that
// worker thread.
//
// Invariants (the audited no-overflow argument):
// * The cache hands out class-sized blocks (its malloc fallback included).
// * Frees come in two modes, both incapable of over-promising: `_exact`
//   takes the block's physical size (`capacity * sizeof(Subtree)` on every
//   array-flavored path) and bins by FLOOR class, bypassing sub-class raw
//   blocks; `_node` takes `ts_subtree_alloc_size(child_count)` — a lower
//   bound — and bins by ROUND-UP class, which reconstructs the birth class
//   because every block a node can claim is cache-born or array-doubled to
//   a power of two ≥ that class.
// * Requests beyond the largest class bypass the cache in both directions.
// * Per-class depth is capped (`TS_CHILDREN_CACHE_CAP`, swept — see
//   BENCHMARKS); overflow frees go to the real allocator. A pthread key
//   destructor drains the cache at thread exit.
// * On targets without pthreads (wasm, MSVC) the cache compiles away and the
//   entry points degrade to plain `ts_malloc`/`ts_free`.
#ifndef TREE_SITTER_CHILDREN_CACHE_H_
#define TREE_SITTER_CHILDREN_CACHE_H_

#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include "./alloc.h"

#if defined(_WIN32) || defined(__wasm__) || defined(__EMSCRIPTEN__)

static inline void *ts_children_alloc(size_t *bytes) {
  return ts_malloc(*bytes);
}

static inline void ts_children_free_exact(void *ptr, size_t bytes) {
  (void)bytes;
  if (ptr) ts_free(ptr);
}

static inline void ts_children_free_node(void *ptr, size_t bytes) {
  (void)bytes;
  if (ptr) ts_free(ptr);
}

static inline bool ts_children_node_block_ok(size_t physical, size_t node_bytes) {
  (void)physical;
  (void)node_bytes;
  return true; /* no binning without the cache — every block is claim-safe */
}

static inline void *ts_leaf_cache_alloc(size_t bytes) {
  return ts_malloc(bytes);
}

static inline void ts_leaf_cache_free(void *ptr) {
  if (ptr) ts_free(ptr);
}

#else

#include <pthread.h>

#define TS_CC_CLASS_COUNT 8
#define TS_CC_MIN_SHIFT 7 /* smallest class = 128 bytes */
#define TS_CC_MAX_BYTES ((size_t)1 << (TS_CC_MIN_SHIFT + TS_CC_CLASS_COUNT - 1)) /* 16384 */
// Swept on the kernel corpus (40 M LOC) and cpython (BENCHMARKS: caps
// 512 / 8192 / 65536, re-run interleaved on a quiet machine): 65536 is the
// optimum — an earlier "deep lists go cold" reading was ambient noise; at
// depth 65536 the kernel keeps the same user CPU as 8192 while allocator
// calls drop a further 3.5× (RSS +~0.2 GB). `TS_CHILDREN_CACHE_CAP` and
// `TS_LEAF_CACHE_CAP` override per corpus.
#define TS_CC_DEFAULT_CAP 65536

// Intrusive per-class freelists: a free block's first word is the next
// pointer (every cached block is ≥ 128 bytes), so the cache itself occupies
// no memory beyond two small TLS arrays — depth is bounded by the swept
// per-class cap alone.
static _Thread_local void *ts_cc_head[TS_CC_CLASS_COUNT];
static _Thread_local uint32_t ts_cc_len[TS_CC_CLASS_COUNT];
static _Thread_local bool ts_cc_armed = false;

static pthread_key_t ts_cc_key;
static pthread_once_t ts_cc_once = PTHREAD_ONCE_INIT;
static long ts_cc_cap = TS_CC_DEFAULT_CAP;

// Leaf-header list: `SubtreePool` recycles leaf `SubtreeHeapData` structs
// within one parser, but the pool dies with the parser and `ts_tree_delete`
// drains through a throwaway pool — so leaves, like children blocks before
// pass 19, never recycled across files. One fixed-size intrusive list closes
// that (`sizeof(SubtreeHeapData)` ≥ one pointer; cap `TS_LEAF_CACHE_CAP`).
static _Thread_local void *ts_lc_head;
static _Thread_local uint32_t ts_lc_len;
static long ts_lc_cap = TS_CC_DEFAULT_CAP;

static void ts_cc_thread_drain(void *unused) {
  (void)unused;
  for (unsigned c = 0; c < TS_CC_CLASS_COUNT; c++) {
    void *block = ts_cc_head[c];
    while (block) {
      void *next = *(void **)block;
      ts_free(block);
      block = next;
    }
    ts_cc_head[c] = NULL;
    ts_cc_len[c] = 0;
  }
  void *leaf = ts_lc_head;
  while (leaf) {
    void *next = *(void **)leaf;
    ts_free(leaf);
    leaf = next;
  }
  ts_lc_head = NULL;
  ts_lc_len = 0;
  ts_cc_armed = false;
}

static void ts_cc_global_init(void) {
  (void)pthread_key_create(&ts_cc_key, ts_cc_thread_drain);
  const char *cap = getenv("TS_CHILDREN_CACHE_CAP");
  if (cap) {
    long v = strtol(cap, NULL, 10);
    if (v >= 0) ts_cc_cap = v;
  }
  const char *leaf_cap = getenv("TS_LEAF_CACHE_CAP");
  if (leaf_cap) {
    long v = strtol(leaf_cap, NULL, 10);
    if (v >= 0) ts_lc_cap = v;
  }
}

static inline void ts_cc_arm(void) {
  if (!ts_cc_armed) {
    (void)pthread_once(&ts_cc_once, ts_cc_global_init);
    (void)pthread_setspecific(ts_cc_key, (void *)1);
    ts_cc_armed = true;
  }
}

// Class index covering `bytes` (callers pre-check the bypass threshold).
static inline unsigned ts_cc_class(size_t bytes) {
  unsigned c = 0;
  size_t class_bytes = (size_t)1 << TS_CC_MIN_SHIFT;
  while (class_bytes < bytes) {
    class_bytes <<= 1;
    c++;
  }
  return c;
}

// Allocate a children block of at least `*bytes`; `*bytes` is rounded up to
// the class actually provided so callers can set array capacity honestly.
static inline void *ts_children_alloc(size_t *bytes) {
  if (*bytes > TS_CC_MAX_BYTES) {
    return ts_malloc(*bytes);
  }
  ts_cc_arm();
  unsigned c = ts_cc_class(*bytes);
  *bytes = (size_t)1 << (TS_CC_MIN_SHIFT + c);
  void *block = ts_cc_head[c];
  if (block) {
    ts_cc_head[c] = *(void **)block;
    ts_cc_len[c]--;
    return block;
  }
  return ts_malloc(*bytes);
}

// Largest class whose size is ≤ `bytes` (caller guarantees
// `bytes >= 1 << TS_CC_MIN_SHIFT`).
static inline unsigned ts_cc_class_floor(size_t bytes) {
  unsigned c = 0;
  while (c + 1 < TS_CC_CLASS_COUNT &&
         ((size_t)1 << (TS_CC_MIN_SHIFT + c + 1)) <= bytes) {
    c++;
  }
  return c;
}

static inline void ts_cc_push(unsigned c, void *ptr) {
  ts_cc_arm();
  if ((long)ts_cc_len[c] < ts_cc_cap) {
    *(void **)ptr = ts_cc_head[c];
    ts_cc_head[c] = ptr;
    ts_cc_len[c]++;
  } else {
    ts_free(ptr);
  }
}

// Free with `bytes` == the block's EXACT physical size (array-flavored frees:
// `capacity * sizeof(Subtree)`). Bins by FLOOR class, so reuse can never
// exceed the physical size; blocks below the smallest class (raw sub-128-byte
// reserves) bypass the cache entirely.
static inline void ts_children_free_exact(void *ptr, size_t bytes) {
  if (!ptr) return;
  if (bytes < ((size_t)1 << TS_CC_MIN_SHIFT) || bytes > TS_CC_MAX_BYTES) {
    ts_free(ptr);
    return;
  }
  ts_cc_push(ts_cc_class_floor(bytes), ptr);
}

// Free a CLAIMED node block with `bytes` == `ts_subtree_alloc_size(count)` —
// a lower bound on the physical size. Bins by ROUND-UP class, reconstructing
// the birth class: every block a node can claim is cache-born (class-sized,
// and alloc_size ≥ the header keeps round-up ≥ the smallest class) or
// array-grown to a power of two ≥ `8 * (count + header/8)` — in both audited
// flows physical ≥ the round-up class, so reuse cannot overflow.
static inline void ts_children_free_node(void *ptr, size_t bytes) {
  if (!ptr) return;
  if (bytes > TS_CC_MAX_BYTES) {
    ts_free(ptr);
    return;
  }
  ts_cc_push(ts_cc_class(bytes), ptr);
}

// Claim-safety check for `ts_subtree_new_node`: may a node claim a block of
// `physical` bytes and later be freed via `ts_children_free_node(node_bytes)`?
// Safe iff the round-up bin that free will choose does not exceed the block's
// physical size. Cache-born and po2-grown blocks always pass; the one shape
// that fails is an exact-reserved array (`array_reserve(n)` → `ts_realloc`,
// physical = n*8, not class-shaped) big enough to skip the grow branch —
// C++/Rust parse flows produce those, and binning such a block one class up
// hands out more bytes than it has (heap corruption; found on
// llvm/include/llvm/CodeGen/SelectionDAG.h). Callers migrate failing blocks
// through `ts_children_alloc` instead of claiming them.
static inline bool ts_children_node_block_ok(size_t physical, size_t node_bytes) {
  if (node_bytes > TS_CC_MAX_BYTES) return true; /* free_node raw-frees these */
  return physical >= ((size_t)1 << (TS_CC_MIN_SHIFT + ts_cc_class(node_bytes)));
}

// Fixed-size leaf-header alloc/free (`bytes` is always
// `sizeof(SubtreeHeapData)` — passed for the malloc fallback only).
static inline void *ts_leaf_cache_alloc(size_t bytes) {
  ts_cc_arm();
  void *block = ts_lc_head;
  if (block) {
    ts_lc_head = *(void **)block;
    ts_lc_len--;
    return block;
  }
  return ts_malloc(bytes);
}

static inline void ts_leaf_cache_free(void *ptr) {
  if (!ptr) return;
  ts_cc_arm();
  if ((long)ts_lc_len < ts_lc_cap) {
    *(void **)ptr = ts_lc_head;
    ts_lc_head = ptr;
    ts_lc_len++;
  } else {
    ts_free(ptr);
  }
}

#endif // threads available

#endif // TREE_SITTER_CHILDREN_CACHE_H_

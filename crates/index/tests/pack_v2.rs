//! Bucketed pack (P4.1) end-to-end gates, driven through the real `build_index` pipeline
//! under `VORPAL_FORMAT=next`:
//!
//!  1. a bucketed generation carries `products/<k>.pack` + `products/toc.bin` and no flat
//!     pack;
//!  2. determinism ×2 — two scratch builds of the same tree commit the same content id
//!     (bucket files and TOC included in the fold);
//!  3. the single-bucket rewrite law — an incremental edit rewrites exactly the edited
//!     file's bucket, hard-links every other bucket from the prior generation (inode
//!     oracle), and converges to the scratch content id of the edited tree;
//!  4. the stamp-only cutoff stays live under v2 — a touch-only change commits via the
//!     carried-graph path (patched buckets copied, digests respliced) and its generation
//!     equals the scratch build of the touched tree;
//!  5. the v1→v2 migration is one pack publish, not a re-extract, and still converges;
//!  6. query surfaces that hold only the index dir (parse-health, coverage) resolve the
//!     bucketed pack through the exact manifest-derived root.
//!
//! One test fn: the format env var is process-global, and this file being its own test
//! binary plus a single #[test] makes the sequencing deterministic.

use std::fs;
use std::path::{Path, PathBuf};

use vorpal_index::build_index;

fn live(root: &Path) -> PathBuf {
  vorpal_kg::resolve_index_dir(root)
}

fn write_fixture(src: &Path) {
  fs::create_dir_all(src.join("core")).unwrap();
  fs::create_dir_all(src.join("util")).unwrap();
  for i in 0..12 {
    fs::write(
      src.join("core").join(format!("mod_{i:02}.rs")),
      format!(
        "pub fn helper_{i}(value: i32) -> i32 {{\n    value + {i}\n}}\n\npub fn \
         entry_{i}(seed: i32) -> i32 {{\n    helper_{i}(seed)\n}}\n"
      ),
    )
    .unwrap();
    fs::write(
      src.join("util").join(format!("tool_{i:02}.py")),
      format!("def tool_{i}(count):\n    return count + {i}\n\n\ndef run_{i}(count):\n    return tool_{i}(count)\n"),
    )
    .unwrap();
  }
  fs::write(
    src.join("main.go"),
    "package main\n\nfunc compute(n int) int {\n\treturn n * 2\n}\n\nfunc main() {\n\t_ = compute(21)\n}\n",
  )
  .unwrap();
}

/// (bucket file name → (inode, len)) for every bucket file of a generation.
fn bucket_stats(generation: &Path) -> Vec<(String, u64, u64)> {
  let mut rows: Vec<(String, u64, u64)> = fs::read_dir(generation.join("products"))
    .unwrap()
    .flatten()
    .filter_map(|entry| {
      let name = entry.file_name().into_string().ok()?;
      if !name.ends_with(".pack") {
        return None;
      }
      let meta = entry.metadata().ok()?;
      #[cfg(unix)]
      let ino = {
        use std::os::unix::fs::MetadataExt;
        meta.ino()
      };
      #[cfg(not(unix))]
      let ino = 0u64;
      Some((name, ino, meta.len()))
    })
    .collect();
  rows.sort();
  rows
}

fn content_id(generation: &Path) -> String {
  generation.file_name().unwrap().to_string_lossy().into_owned()
}

#[test]
fn bucketed_pack_end_to_end() {
  let base = std::env::temp_dir().join(format!("vorpal-pack-v2-e2e-{}", std::process::id()));
  let _ = fs::remove_dir_all(&base);
  let src = base.join("tree");
  write_fixture(&src);
  // Process-global by design: this file is its own test binary with exactly one test.
  unsafe { std::env::set_var("VORPAL_FORMAT", "next") };

  // 1+2: scratch determinism and the artifact set — the pack family (P4.1) AND the node
  // store family (P4.2).
  let out_a = base.join("index-a");
  let report = build_index(&src, &out_a).unwrap();
  assert!(!report.reused);
  assert_eq!(report.indexed, 25, "12 rs + 12 py + 1 go");
  let gen_a = live(&out_a);
  assert!(gen_a.join("products/toc.bin").is_file(), "v2 pack TOC missing");
  assert!(!gen_a.join("products.pack").exists(), "flat pack must not ride along");
  assert!(!gen_a.join("products.idx").exists(), "flat sidecar must not ride along");
  assert!(gen_a.join("nodes/toc.bin").is_file(), "v2 node-store TOC missing");
  assert!(!gen_a.join("nodes.vseg").exists(), "flat node segment must not ride along");
  assert!(!gen_a.join("strings.heap").exists(), "flat heap must not ride along");
  assert!(gen_a.join("evidence/toc.bin").is_file(), "v2 evidence TOC missing");
  assert!(!gen_a.join("evidence.bin").exists(), "flat evidence must not ride along");
  assert!(gen_a.join("edges/toc.bin").is_file(), "v2 edge-store TOC missing");
  assert!(
    gen_a.join("graph.bin").is_file() && gen_a.join("graph.stamp").is_file(),
    "the derived CSR cache is written eagerly"
  );
  let buckets = bucket_stats(&gen_a);
  assert_eq!(buckets.len(), 16, "25 files land in the clamped minimum bucket count");
  let node_slabs = fs::read_dir(gen_a.join("nodes"))
    .unwrap()
    .flatten()
    .filter(|e| e.file_name().to_string_lossy().ends_with(".vseg"))
    .count();
  assert_eq!(node_slabs, 16, "one node slab per bucket");

  // Cross-format truth oracle: a flat and a bucketed generation of the SAME tree describe
  // the same graph — every node's every field, compared as canonical sets (dense ids
  // legitimately differ: bucket-major vs path order).
  unsafe { std::env::set_var("VORPAL_FORMAT", "") };
  let out_flat = base.join("index-flat");
  build_index(&src, &out_flat).unwrap();
  unsafe { std::env::set_var("VORPAL_FORMAT", "next") };
  let node_universe = |root: &std::path::Path| -> Vec<String> {
    let kg = vorpal_kg::Kg::load(root).unwrap();
    let mut rows: Vec<String> = (0..kg.node_count() as u64)
      .map(|id| {
        let view = kg.node(vorpal_kg::NodeId::new(id)).expect("dense id resolves");
        format!(
          "{:?}|{}|{}|{}|{:?}|{}|{}|{:?}|{:?}",
          view.kind,
          view.name,
          view.path,
          view.signature,
          view.span,
          view.content_hash,
          view.exported,
          view.external_id,
          kg.scc_size(vorpal_kg::NodeId::new(id)),
        )
      })
      .collect();
    rows.sort_unstable();
    rows
  };
  assert_eq!(
    node_universe(&out_flat),
    node_universe(&out_a),
    "flat and bucketed generations must describe the identical node universe"
  );
  // …and the identical EDGE + EVIDENCE universe: every relation and every retained
  // occurrence, keyed by names (dense ids legitimately differ), compared as sets.
  let relation_universe = |root: &std::path::Path| -> Vec<String> {
    let kg = vorpal_kg::Kg::load(root).unwrap();
    let name_of = |id: vorpal_kg::NodeId| {
      kg.node(id)
        .map(|view| format!("{}#{}", view.path, view.name))
        .unwrap_or_default()
    };
    let mut rows: Vec<String> = Vec::new();
    for id in 0..kg.node_count() as u64 {
      let id = vorpal_kg::NodeId::new(id);
      for (target, etype) in kg.out_neighbors(id) {
        rows.push(format!("E|{}|{}|{}", name_of(id), etype.base().0, name_of(target)));
      }
      for row in kg.evidence_from(id) {
        let target = if row.to == vorpal_kg::NO_EDGE {
          "<none>".to_string()
        } else {
          name_of(vorpal_kg::NodeId::new(row.to as u64))
        };
        let mut alts: Vec<String> = row
          .alternatives
          .iter()
          .map(|&alt| name_of(vorpal_kg::NodeId::new(alt as u64)))
          .collect();
        alts.sort_unstable();
        rows.push(format!(
          "V|{}|{}|{}|{}|{}|{}|{}|{:?}",
          name_of(id),
          target,
          row.etype,
          row.reason,
          row.confidence,
          row.candidates,
          row.span_start,
          alts,
        ));
      }
    }
    rows.sort_unstable();
    rows
  };
  assert_eq!(
    relation_universe(&out_flat),
    relation_universe(&out_a),
    "flat and bucketed generations must describe the identical relation/evidence universe"
  );
  // The derived CSR cache is never load-bearing: delete it, load again (forces the
  // slab rebuild), answers hold, and the lazy write-back restores the cache files.
  fs::remove_file(gen_a.join("graph.bin")).unwrap();
  fs::remove_file(gen_a.join("graph.stamp")).unwrap();
  assert_eq!(
    relation_universe(&out_flat),
    relation_universe(&out_a),
    "slab-rebuilt graph must answer identically to the cached one"
  );
  assert!(
    gen_a.join("graph.bin").is_file() && gen_a.join("graph.stamp").is_file(),
    "the rebuild lazily re-caches"
  );
  let out_b = base.join("index-b");
  build_index(&src, &out_b).unwrap();
  assert_eq!(
    content_id(&gen_a),
    content_id(&live(&out_b)),
    "two scratch v2 builds must commit the same content id"
  );
  // P4.4 agreement oracle, equal side: generations equal under the Merkle id are equal
  // under the full-rehash fold too.
  assert_eq!(
    vorpal_index::generation_content_id_full(&gen_a).unwrap(),
    vorpal_index::generation_content_id_full(&live(&out_b)).unwrap(),
    "Merkle-equal generations must be full-rehash-equal"
  );

  // 6: query surfaces resolve the bucketed pack from the index dir alone (exact
  // manifest-derived root — parse-health walks kg File paths, coverage sweeps the bank).
  let health = vorpal_index::parse_health_report(&out_a).unwrap();
  assert!(!health.is_empty());
  let coverage = vorpal_index::records::coverage_records(Some(&gen_a));
  assert_eq!(coverage.total_files, 25, "coverage sweep sees every packed product");

  // 3: the single-bucket rewrite law.
  let edited_rel = "core/mod_07.rs";
  fs::write(
    src.join(edited_rel),
    "pub fn helper_7(value: i32) -> i32 {\n    value * 7\n}\n\npub fn entry_7(seed: i32) -> \
     i32 {\n    helper_7(seed + 1)\n}\n",
  )
  .unwrap();
  let report = build_index(&src, &out_a).unwrap();
  assert!(!report.reused && !report.graph_reused, "content edit takes the full pipeline");
  let gen_a2 = live(&out_a);
  assert_ne!(gen_a.file_name(), gen_a2.file_name());
  let before: std::collections::HashMap<String, (u64, u64)> = bucket_stats(&gen_a)
    .into_iter()
    .map(|(name, ino, len)| (name, (ino, len)))
    .collect();
  let expected_bucket =
    (vorpal_kg::identity::FileKey::of(edited_rel).0 & (before.len() as u64 - 1)) as u32;
  let mut rewritten: Vec<String> = Vec::new();
  #[cfg(unix)]
  for (name, ino, _) in bucket_stats(&gen_a2) {
    if before[&name].0 != ino {
      rewritten.push(name);
    }
  }
  #[cfg(unix)]
  assert_eq!(
    rewritten,
    vec![format!("{expected_bucket:04}.pack")],
    "an edit must rewrite exactly the edited file's bucket and hard-link the rest"
  );
  // The node store obeys the same law (P4.2): exactly the edited file's node/heap slabs
  // rewrite; every other bucket's slabs hard-link (this fixture has no cross-file cycles,
  // so no scc_size ripple).
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    let mut node_rewritten: Vec<String> = Vec::new();
    for entry in fs::read_dir(gen_a.join("nodes")).unwrap().flatten() {
      let name = entry.file_name().into_string().unwrap();
      if !(name.ends_with(".vseg") || name.ends_with(".heap")) {
        continue;
      }
      let prior_ino = entry.metadata().unwrap().ino();
      let new_ino = fs::metadata(gen_a2.join("nodes").join(&name)).unwrap().ino();
      if prior_ino != new_ino {
        node_rewritten.push(name);
      }
    }
    node_rewritten.sort();
    assert_eq!(
      node_rewritten,
      vec![
        format!("{expected_bucket:04}.heap"),
        format!("{expected_bucket:04}.vseg"),
      ],
      "an edit must rewrite exactly the edited file's node slabs and hard-link the rest"
    );
    // Evidence follows the same law (spans in the edited file moved)…
    let mut evidence_rewritten: Vec<String> = Vec::new();
    for entry in fs::read_dir(gen_a.join("evidence")).unwrap().flatten() {
      let name = entry.file_name().into_string().unwrap();
      if !name.ends_with(".bin") || name == "toc.bin" {
        continue;
      }
      let prior_ino = entry.metadata().unwrap().ino();
      let new_ino = fs::metadata(gen_a2.join("evidence").join(&name)).unwrap().ino();
      if prior_ino != new_ino {
        evidence_rewritten.push(name);
      }
    }
    assert_eq!(
      evidence_rewritten,
      vec![format!("{expected_bucket:04}.bin")],
      "an edit must rewrite exactly the edited file's evidence slab"
    );
    // …and the EDGE slabs rewrite AT MOST the edited file's bucket (this edit changes the
    // call-argument shape, so its DATA_FLOWS rows legitimately move); every other bucket's
    // endpoints are position-independent (bucket, local) coordinates and must hard-link.
    for entry in fs::read_dir(gen_a.join("edges")).unwrap().flatten() {
      let name = entry.file_name().into_string().unwrap();
      if !name.ends_with(".bin") || name == "toc.bin" {
        continue;
      }
      if name == format!("{expected_bucket:04}.bin") {
        continue;
      }
      assert_eq!(
        entry.metadata().unwrap().ino(),
        fs::metadata(gen_a2.join("edges").join(&name)).unwrap().ino(),
        "unedited edge slab {name} must hard-link across a single-file edit"
      );
    }
  }
  let out_c = base.join("index-c");
  build_index(&src, &out_c).unwrap();
  assert_eq!(
    content_id(&gen_a2),
    content_id(&live(&out_c)),
    "incremental v2 build must converge to the scratch id of the edited tree"
  );
  // P4.4 agreement oracle, unequal side: differing trees differ under BOTH folds.
  assert_ne!(content_id(&gen_a), content_id(&gen_a2));
  assert_ne!(
    vorpal_index::generation_content_id_full(&gen_a).unwrap(),
    vorpal_index::generation_content_id_full(&gen_a2).unwrap(),
    "Merkle-distinct generations must be full-rehash-distinct"
  );

  // 4: stamp-only cutoff under v2 — same bytes, fresh mtime.
  let touched_rel = "util/tool_03.py";
  let bytes = fs::read(src.join(touched_rel)).unwrap();
  fs::write(src.join(touched_rel), &bytes).unwrap();
  let report = build_index(&src, &out_a).unwrap();
  assert!(
    report.graph_reused && !report.reused,
    "touch-only change must take the stamp-only cutoff: {report:?}"
  );
  let gen_a3 = live(&out_a);
  let touched_bucket =
    (vorpal_kg::identity::FileKey::of(touched_rel).0 & (before.len() as u64 - 1)) as u32;
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    let after_edit: std::collections::HashMap<String, (u64, u64)> = bucket_stats(&gen_a2)
      .into_iter()
      .map(|(name, ino, len)| (name, (ino, len)))
      .collect();
    let mut repatched: Vec<String> = Vec::new();
    for (name, ino, _) in bucket_stats(&gen_a3) {
      if after_edit[&name].0 != ino {
        repatched.push(name);
      }
    }
    assert_eq!(
      repatched,
      vec![format!("{touched_bucket:04}.pack")],
      "the cutoff must copy-patch exactly the touched file's bucket and link the rest"
    );
    // The node store, evidence, and edge families never carry stamps: the cutoff
    // hard-links EVERY member + TOC of all three.
    for family in ["nodes", "evidence", "edges"] {
      for entry in fs::read_dir(gen_a2.join(family)).unwrap().flatten() {
        let name = entry.file_name().into_string().unwrap();
        assert_eq!(
          entry.metadata().unwrap().ino(),
          fs::metadata(gen_a3.join(family).join(&name)).unwrap().ino(),
          "cutoff must hard-link {family} member {name}"
        );
      }
    }
  }
  let out_d = base.join("index-d");
  build_index(&src, &out_d).unwrap();
  assert_eq!(
    content_id(&gen_a3),
    content_id(&live(&out_d)),
    "cutoff generation must equal the scratch build of the touched tree (stamps included)"
  );

  // 5: v1 → v2 migration — a flat prior migrates through body reuse on the next edit.
  unsafe { std::env::set_var("VORPAL_FORMAT", "") };
  let out_e = base.join("index-e");
  build_index(&src, &out_e).unwrap();
  let gen_e = live(&out_e);
  assert!(gen_e.join("products.pack").is_file(), "flat build still writes v1");
  assert!(!gen_e.join("products").exists(), "no bucketed members in a flat generation");
  assert!(gen_e.join("nodes.vseg").is_file(), "flat build still writes the flat node store");
  assert!(!gen_e.join("nodes").exists(), "no bucketed node members in a flat generation");
  assert!(gen_e.join("evidence.bin").is_file(), "flat build still writes flat evidence");
  assert!(!gen_e.join("evidence").exists());
  assert!(!gen_e.join("edges").exists(), "no edge slabs in a flat generation");
  unsafe { std::env::set_var("VORPAL_FORMAT", "next") };
  // A structural edit (span-shifting), so the stamp-only cutoff — which correctly carries
  // the PRIOR's pack format byte-for-byte — cannot swallow the migration build.
  fs::write(
    src.join("main.go"),
    "package main\n\nfunc doubled(n int) int {\n\treturn n + n\n}\n\nfunc compute(n int) int \
     {\n\treturn doubled(n)\n}\n\nfunc main() {\n\t_ = compute(21)\n}\n",
  )
  .unwrap();
  let report = build_index(&src, &out_e).unwrap();
  assert!(
    !report.reused && !report.graph_reused,
    "structural migration edit must run the full pipeline: {report:?}"
  );
  let gen_e2 = live(&out_e);
  assert!(gen_e2.join("products/toc.bin").is_file(), "migration publishes v2");
  assert!(!gen_e2.join("products.pack").exists());
  assert!(gen_e2.join("nodes/toc.bin").is_file(), "migration publishes the v2 node store");
  assert!(!gen_e2.join("nodes.vseg").exists());
  assert!(gen_e2.join("evidence/toc.bin").is_file(), "migration publishes v2 evidence");
  assert!(!gen_e2.join("evidence.bin").exists());
  assert!(gen_e2.join("edges/toc.bin").is_file(), "migration publishes the v2 edge store");
  let out_f = base.join("index-f");
  build_index(&src, &out_f).unwrap();
  assert_eq!(
    content_id(&gen_e2),
    content_id(&live(&out_f)),
    "migrated generation must equal the scratch v2 build of the same tree"
  );

  let _ = fs::remove_dir_all(&base);
}

//! A definition-adding edit in a C file whose includes need corpus-wide include-root
//! support (`<linux/thing.h>` with a main-tree and a `tools/` copy) must take the
//! defs-changed compose — the prior generation's support rides in `reach.bin` (v2) — and
//! converge byte for byte with a scratch build. Before 0.8.5 the scoped compose could
//! resolve only the includes that need no support, saw its first-hop rows diverge from the
//! carried graph, and declined every such edit to the full pipeline (4.7 s on the kernel).
use std::fs;
use std::path::{Path, PathBuf};

fn live(root: &Path) -> PathBuf {
  vorpal_kg::resolve_index_dir(root)
}

fn content_id(generation: &Path) -> String {
  generation.file_name().unwrap().to_string_lossy().into_owned()
}

fn assert_converged(out_live: &Path, src: &Path, base: &Path, tag: &str) {
  let scratch_out = base.join(format!("scratch-{tag}"));
  vorpal_index::build_index(src, &scratch_out).expect("scratch build");
  let (live_gen, scratch_gen) = (live(out_live), live(&scratch_out));
  assert_eq!(
    content_id(&live_gen),
    content_id(&scratch_gen),
    "{tag}: composed generation must equal the scratch build (Merkle id)"
  );
  assert_eq!(
    vorpal_index::generation_content_id_full(&live_gen).unwrap(),
    vorpal_index::generation_content_id_full(&scratch_gen).unwrap(),
    "{tag}: …and under the full-rehash fold"
  );
}

fn compose_fired(report: &vorpal_index::IndexReport) -> bool {
  report
    .cochange_note
    .as_deref()
    .is_some_and(|note| note.contains("defs-changed compose"))
}

/// Main tree: `include/linux/{thing,other}.h`; a `tools/` shadow of `thing.h` only. Three
/// main-tree files include both headers, so `include/` accumulates twice the support of
/// `tools/include/`; `tools/t.c` binds its own copy by nearest prefix. Every `<linux/…>`
/// include here needs the support rung — exactly the kernel's shape in miniature.
fn write_fixture(src: &Path, with_added: bool) {
  fs::create_dir_all(src.join("include/linux")).unwrap();
  fs::create_dir_all(src.join("tools/include/linux")).unwrap();
  fs::create_dir_all(src.join("tools")).unwrap();
  fs::write(src.join("include/linux/thing.h"), "int thing_main(int v);\n").unwrap();
  fs::write(src.join("include/linux/other.h"), "int other_main(void);\n").unwrap();
  fs::write(src.join("tools/include/linux/thing.h"), "int thing_tools(int v);\n").unwrap();
  let a = if with_added {
    "#include <linux/thing.h>\n#include <linux/other.h>\n\nint alpha(int v)\n{\n\treturn thing_main(v) + other_main();\n}\n\nint fresh_probe(void)\n{\n\treturn alpha(1);\n}\n"
  } else {
    "#include <linux/thing.h>\n#include <linux/other.h>\n\nint alpha(int v)\n{\n\treturn thing_main(v) + other_main();\n}\n"
  };
  fs::write(src.join("a.c"), a).unwrap();
  fs::write(
    src.join("b.c"),
    "#include <linux/thing.h>\n#include <linux/other.h>\n\nint beta(void)\n{\n\treturn thing_main(2) + other_main();\n}\n",
  )
  .unwrap();
  fs::write(
    src.join("c.c"),
    "#include <linux/thing.h>\n#include <linux/other.h>\n\nint gamma(void)\n{\n\treturn thing_main(3);\n}\n",
  )
  .unwrap();
  fs::write(
    src.join("tools/t.c"),
    "#include <linux/thing.h>\n\nint tool_main(void)\n{\n\treturn thing_tools(4);\n}\n",
  )
  .unwrap();
}

#[test]
fn a_definition_added_to_a_c_file_with_suffix_includes_composes_and_converges() {
  unsafe { std::env::set_var("VORPAL_FORMAT", "next") };
  let base = std::env::temp_dir().join(format!("vorpal-dc-inc-{}", std::process::id()));
  let _ = fs::remove_dir_all(&base);
  let src = base.join("repo");
  fs::create_dir_all(&src).unwrap();
  write_fixture(&src, false);
  let src = src.canonicalize().unwrap();

  let out = base.join("index");
  vorpal_index::build_index(&src, &out).expect("initial build");
  // The prior generation persists the learned support with its reach graph.
  let reach = fs::read(live(&out).join(vorpal_ingest::REACH_GRAPH_FILE)).expect("reach.bin");
  let graph = vorpal_ingest::ReachGraph::decode(&reach).expect("decodes");
  let support = graph.include_root_support().expect("v2 carries support");
  let main_root = support.iter().find(|(r, _)| r.ends_with("/include") && !r.contains("/tools/"));
  let tools_root = support.iter().find(|(r, _)| r.ends_with("/tools/include"));
  assert!(
    main_root.map(|r| r.1) > tools_root.map(|r| r.1),
    "main-tree include root must out-support the tools shadow: {support:?}"
  );

  write_fixture(&src, true);
  let report = vorpal_index::build_index(&src, &out).expect("incremental build");
  assert!(
    compose_fired(&report),
    "the definition-adding edit must take the defs-changed compose: {report:?}"
  );
  assert_converged(&out, &src, &base, "c-include-add");

  // And back: removing it composes and converges too.
  write_fixture(&src, false);
  let report = vorpal_index::build_index(&src, &out).expect("incremental build (remove)");
  assert!(compose_fired(&report), "the removal must take the compose: {report:?}");
  assert_converged(&out, &src, &base, "c-include-remove");

  let _ = fs::remove_dir_all(&base);
}

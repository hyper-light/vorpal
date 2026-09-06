//! Installed include-root support must drive suffix disambiguation exactly like learned
//! support: the scoped compose installs the prior generation's map instead of learning
//! from its own handful of imports.
use vorpal_resolve::{Interner, NodeId, SymbolTable};

#[test]
fn installed_support_breaks_the_suffix_tie_like_learned_support() {
  let interner = Interner::new();
  let mut table = SymbolTable::new();
  table.insert_file(&interner, "include/linux/fs.h", NodeId::new(1));
  table.insert_file(&interner, "tools/include/linux/fs.h", NodeId::new(2));
  table.finalize();
  let from = interner.intern("fs/read_write.c");
  // No support: both candidates share zero leading components with the importer and
  // carry zero support — an honest tie, resolved to nothing.
  assert!(table.file_by_suffix(&interner, "linux/fs.h", from).is_none());
  // The corpus's learned support (persisted with the reach graph) decides it. Roots are the
  // path up to the suffix, without the trailing slash — what `learn_include_roots` keys.
  table.set_include_root_support(&interner, [("include", 900u32), ("tools/include", 40u32)]);
  let (id, path) = table.file_by_suffix(&interner, "linux/fs.h", from).expect("resolves");
  assert_eq!((id, interner.text_of(path)), (NodeId::new(1), "include/linux/fs.h"));
  // Export round-trips what was installed, sorted by root.
  assert_eq!(table.include_root_support(), vec![("include", 900u32), ("tools/include", 40u32)]);
  // Reversed support flips the answer — the map is the decision, not the path shape.
  table.set_include_root_support(&interner, [("include", 4u32), ("tools/include", 5u32)]);
  let (id, _) = table.file_by_suffix(&interner, "linux/fs.h", from).expect("resolves");
  assert_eq!(id, NodeId::new(2));
}

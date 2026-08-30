//! Allocation-attribution probe: parse a tree of C files WITHOUT extraction, so jemalloc's
//! exit stats separate parser-side allocation bins from extraction-side ones. Scratch
//! tooling for profiling sessions — not part of any product surface.
use vorpal_ingest::SupportLang;

#[cfg(feature = "jemalloc")]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
  let Ok(entries) = std::fs::read_dir(dir) else { return };
  for entry in entries.flatten() {
    let path = entry.path();
    if path.is_dir() {
      if !path.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')) {
        walk(&path, out);
      }
    } else if path.extension().is_some_and(|e| e == "c" || e == "h") {
      out.push(path);
    }
  }
}

fn main() {
  let root = std::env::args().nth(1).expect("usage: parse_probe <dir>");
  let mut files = Vec::new();
  walk(std::path::Path::new(&root), &mut files);
  files.sort();
  eprintln!("{} files", files.len());
  use rayon::prelude::*;
  let nodes: u64 = files
    .par_iter()
    .map(|path| {
      let Ok(source) = std::fs::read_to_string(path) else { return 0 };
      // The probe walks .c/.h trees only (see `walk`), so the grammar is always C.
      let lang = SupportLang::C;
      let Ok(doc) = vorpal_core::tree_sitter::StrDoc::try_new(&source, lang) else { return 0 };
      let n = doc.tree.root_node().descendant_count() as u64;
      drop(doc);
      n
    })
    .sum();
  eprintln!("total nodes: {nodes}");
}

//! Diagnostic: structural parse coverage per file — how much of each source the
//! parser actually reached before its last top-level node ended. A file whose tree
//! ends far before EOF has been swallowed by a non-resyncing ERROR subtree; the
//! byte-ratio health policy cannot see this. Usage:
//!   structural_coverage <root> [ext ...]   (default exts: c h)
//! Prints one line per file below full coverage, then a summary histogram.
use std::io::Write;
use vorpal_core::Language as _;

fn main() {
  let mut args = std::env::args().skip(1);
  let root = std::path::PathBuf::from(args.next().expect("root"));
  let exts: Vec<String> = { let v: Vec<String> = args.collect(); if v.is_empty() { vec!["c".into(), "h".into()] } else { v } };
  let mut files = Vec::new();
  let mut stack = vec![root.clone()];
  while let Some(dir) = stack.pop() {
    let Ok(rd) = std::fs::read_dir(&dir) else { continue };
    for e in rd.flatten() {
      let p = e.path();
      if p.is_dir() { if p.file_name().is_some_and(|n| n == ".git") { continue; } stack.push(p); }
      else if p.extension().and_then(|x| x.to_str()).is_some_and(|x| exts.iter().any(|e| e == x)) { files.push(p); }
    }
  }
  files.sort();
  let mut buckets = [0usize; 10];
  let mut swallowed_bytes = 0u64;
  let mut total_bytes = 0u64;
  let mut below = 0usize;
  let out = std::io::stdout();
  let mut out = out.lock();
  for f in &files {
    let Ok(src) = std::fs::read_to_string(f) else { continue };
    if src.is_empty() { continue; }
    let path = f.to_string_lossy().into_owned();
    let Some(lang) = vorpal_lang_registry::SgLang::from_path(path.as_str()) else { continue };
    let parsed = vorpal_core::tree_sitter::LanguageExt::grep(&lang, &src);
    let r = parsed.root();
    total_bytes += src.len() as u64;
    // SWALLOWING, not mere errors: tree-sitter recovers from most errors and
    // resyncs at the next top-level construct, so "first child with an error
    // descendant" over-counts massively (kernel/sched/fair.c indexes thousands
    // of definitions past its first error). The failure mode that loses
    // definitions is a top-level node whose span runs to (or nearly to) EOF
    // while carrying errors inside — the parser kept it open and absorbed the
    // rest of the file. Coverage = the start of the FIRST such swallowing node
    // (the tail from there is inside it), else full.
    let mut reached = src.len();
    let eof = src.len();
    for c in r.children() {
      let range = c.range();
      // A node that ends within 1% of EOF (or at it) and has errors inside,
      // while starting well before EOF, is the swallowing shape.
      let ends_at_eof = range.end + eof / 100 >= eof;
      let spans_tail = (eof - range.start) as f64 / eof as f64 > 0.05;
      if ends_at_eof && spans_tail && (c.is_error() || c.has_error()) {
        reached = range.start;
        break;
      }
    }
    let cov = reached as f64 / src.len() as f64;
    let b = ((cov * 10.0) as usize).min(9);
    buckets[b] += 1;
    if cov < 0.999 {
      below += 1;
      swallowed_bytes += (src.len() - reached) as u64;
      let line = src[..reached].matches('\n').count() + 1;
      let total = src.matches('\n').count() + 1;
      let _ = writeln!(out, "{:.3}\t{}\t{}\t{}", cov, line, total, path.strip_prefix(root.to_string_lossy().as_ref()).unwrap_or(&path));
    }
  }
  let _ = writeln!(out, "SUMMARY files={} below_full={} swallowed_bytes={} of {} ({:.2}%)", files.len(), below, swallowed_bytes, total_bytes, swallowed_bytes as f64 * 100.0 / total_bytes.max(1) as f64);
  let _ = writeln!(out, "HISTOGRAM(coverage decile: files) {:?}", buckets);
}

//! `vorpal-index` binary: index a directory, then query the persisted knowledge graph.

use std::path::Path;
use std::process::ExitCode;

/// jemalloc with prompt page return: at kernel scale, roughly 45% of the default macOS
/// allocator's peak footprint was freed-but-retained magazine pages (2.05 GB observed vs a
/// ~0.95 GB live set). jemalloc's decay returns those pages while running, and its
/// thread-local caches are also simply faster under the pipeline's multithreaded churn.
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// Compiled-in jemalloc tuning (overridable at runtime via `_RJEM_MALLOC_CONF`): zero decay
/// returns freed pages to the OS immediately — a bulk pipeline's phases hand memory back
/// instead of stacking retained garbage under the next phase's live set — and a bounded
/// arena count stops 4×ncpu arenas from each holding a retention tail (~140 MB spread at
/// kernel scale). Measured on the Linux tree: default malloc 2.05 GB peak → this config
/// 1.13 GB, equal wall time.
#[cfg(not(target_env = "msvc"))]
mod jemalloc_conf {
  #[repr(transparent)]
  pub struct SyncPtr(#[allow(dead_code)] *const u8);
  unsafe impl Sync for SyncPtr {}
  #[unsafe(export_name = "_rjem_malloc_conf")]
  pub static MALLOC_CONF: SyncPtr = SyncPtr(
    c"narenas:8,dirty_decay_ms:0,muzzy_decay_ms:0"
      .as_ptr()
      .cast(),
  );
}

use vorpal_index::{build_index, search_index};

const USAGE: &str = "usage:
  vorpal-index index        <src-dir> <index-dir> [--verify]  build + persist a knowledge graph
  vorpal-index callers      <index-dir> <name>      direct callers of a symbol
  vorpal-index refs         <index-dir> <name>      direct referrers of a symbol
  vorpal-index importers    <index-dir> <name>      files importing a symbol
  vorpal-index implementors <index-dir> <name>      types implementing/extending a symbol
  vorpal-index typeusers    <index-dir> <name>      definitions using a type
  vorpal-index node         <index-dir> <name>      nodes matching a name
  vorpal-index why          <index-dir> <from-id> <to-id>  evidence for the edge(s) from → to
  vorpal-index search       <index-dir> <query> [k] hybrid search (name + semantic + graph, RRF)";

/// Route tree-sitter's C-side allocations through jemalloc too. Without this the parser's
/// tree memory lives in the macOS default zone — outside jemalloc's decay policy — and
/// freed-but-retained tree pages (~150–250 MB at kernel scale) ride under the link phase's
/// peak. One allocator, one policy.
#[cfg(not(target_env = "msvc"))]
fn unify_parser_allocator() {
  unsafe {
    tree_sitter::set_allocator(
      Some(tikv_jemalloc_sys::malloc),
      Some(tikv_jemalloc_sys::calloc),
      Some(tikv_jemalloc_sys::realloc),
      Some(tikv_jemalloc_sys::free),
    );
  }
}

fn main() -> ExitCode {
  #[cfg(not(target_env = "msvc"))]
  unify_parser_allocator();
  // Detached-warm re-entry + spawn permission — before any argument handling.
  vorpal_index::autowarm::run_if_sentinel();
  vorpal_index::autowarm::register();
  match run() {
    Ok(()) => ExitCode::SUCCESS,
    Err(err) => {
      eprintln!("vorpal-index: {err}");
      ExitCode::FAILURE
    }
  }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
  let args: Vec<String> = std::env::args().skip(1).collect();
  let argv: Vec<&str> = args.iter().map(String::as_str).collect();
  match argv.as_slice() {
    ["index", src, out] | ["index", src, out, "--verify"] => {
      let mode = if argv.last() == Some(&"--verify") {
        vorpal_index::CacheMode::Verified
      } else {
        vorpal_index::CacheMode::default()
      };
      let report = vorpal_index::build_index_with(Path::new(src), Path::new(out), mode)?;
      if report.reused {
        println!("unchanged — reused existing index ({} nodes)", report.nodes);
      } else {
        println!(
          "parsed {} files ({} replayed from cache) → {} nodes; refs: {} resolved, {} ambiguous, {} external, {} masked",
          report.indexed,
          report.skipped,
          report.nodes,
          report.resolved,
          report.ambiguous,
          report.external,
          report.masked
        );
        if report.error_files > 0 {
          println!(
            "note: {} files had parse errors ({} ERROR nodes total; some definitions may be missing)",
            report.error_files, report.error_nodes
          );
        }
      }
      Ok(())
    }
    [
      verb @ ("callers" | "refs" | "importers" | "implementors" | "typeusers" | "node"),
      index,
      name,
      rest @ ..,
    ] => {
      // Optional trailing selector flag: `--all` merges across same-named definitions
      // (the historical union); the richer selector surface lives on the `vorpal` CLI.
      let target = vorpal_index::GraphTarget {
        name: (*name).to_string(),
        merge_all: rest == ["--all"],
        ..vorpal_index::GraphTarget::default()
      };
      if !rest.is_empty() && rest != ["--all"] {
        eprintln!("{USAGE}");
        return Err("invalid arguments".into());
      }
      print!(
        "{}",
        vorpal_index::graph_query_selected(Path::new(index), verb, &target)?
      );
      Ok(())
    }
    ["why", index, from_id, to_id] => {
      print!(
        "{}",
        vorpal_index::explain_edge(Path::new(index), from_id.parse()?, to_id.parse()?)?
      );
      Ok(())
    }
    ["search", index, query] => print_search(index, query, 10),
    ["search", index, query, k] => print_search(index, query, k.parse()?),
    _ => {
      eprintln!("{USAGE}");
      Err("invalid arguments".into())
    }
  }
}

fn print_search(index: &str, query: &str, k: usize) -> Result<(), Box<dyn std::error::Error>> {
  let rendered = search_index(Path::new(index), query, k)?;
  if rendered.is_empty() {
    println!("(no results for '{query}')");
  } else {
    print!("{rendered}");
  }
  Ok(())
}

//! `vorpal-index` binary: index a directory, then query the persisted knowledge graph.

use std::path::Path;
use std::process::ExitCode;

use vorpal_index::{build_index, graph_query, search_index};

const USAGE: &str = "usage:
  vorpal-index index        <src-dir> <index-dir>   build + persist a knowledge graph
  vorpal-index callers      <index-dir> <name>      direct callers of a symbol
  vorpal-index refs         <index-dir> <name>      direct referrers of a symbol
  vorpal-index importers    <index-dir> <name>      files importing a symbol
  vorpal-index implementors <index-dir> <name>      types implementing/extending a symbol
  vorpal-index typeusers    <index-dir> <name>      definitions using a type
  vorpal-index node         <index-dir> <name>      nodes matching a name
  vorpal-index search       <index-dir> <query> [k] hybrid search (name + semantic + graph, RRF)";

fn main() -> ExitCode {
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
    ["index", src, out] => {
      let report = build_index(Path::new(src), Path::new(out))?;
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
      }
      Ok(())
    }
    [
      verb @ ("callers" | "refs" | "importers" | "implementors" | "typeusers" | "node"),
      index,
      name,
    ] => {
      print!("{}", graph_query(Path::new(index), verb, name)?);
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

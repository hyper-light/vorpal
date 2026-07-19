//! `vorpal-index` binary: index a directory, then query the persisted knowledge graph.

use std::path::Path;
use std::process::ExitCode;

use vorpal_index::{build_index, format_nodes};
use vorpal_kg::Kg;

const USAGE: &str = "usage:
  vorpal-index index   <src-dir> <index-dir>   build + persist a knowledge graph
  vorpal-index callers <index-dir> <name>      direct callers of a symbol
  vorpal-index refs    <index-dir> <name>      direct referrers of a symbol
  vorpal-index node    <index-dir> <name>      nodes matching a name";

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
          "indexed {} files ({} skipped) → {} nodes, {} calls resolved, {} unresolved",
          report.indexed, report.skipped, report.nodes, report.resolved, report.unresolved
        );
      }
      Ok(())
    }
    [verb @ ("callers" | "refs" | "node"), index, name] => {
      let kg = Kg::load(Path::new(index))?;
      let ids = match *verb {
        "callers" => kg.callers_of(name),
        "refs" => kg.references_to(name),
        _ => kg.nodes_named(name),
      };
      if ids.is_empty() {
        println!("(no results for '{name}')");
      } else {
        print!("{}", format_nodes(&kg, &ids));
      }
      Ok(())
    }
    _ => {
      eprintln!("{USAGE}");
      Err("invalid arguments".into())
    }
  }
}

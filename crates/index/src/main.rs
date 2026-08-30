//! `vorpal-index` binary: index a directory, then query the persisted knowledge graph.

use std::path::Path;
use std::process::ExitCode;

/// jemalloc with prompt page return: at kernel scale, roughly 45% of the default macOS
/// allocator's peak footprint was freed-but-retained magazine pages (2.05 GB observed vs a
/// ~0.95 GB live set). jemalloc's decay returns those pages while running, and its
/// thread-local caches are also simply faster under the pipeline's multithreaded churn.
#[cfg(all(feature = "jemalloc", not(any(target_env = "msvc", all(target_env = "musl", target_arch = "aarch64")))))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// Compiled-in jemalloc tuning (overridable at runtime via `_RJEM_MALLOC_CONF`): zero decay
/// returns freed pages to the OS immediately — a bulk pipeline's phases hand memory back
/// instead of stacking retained garbage under the next phase's live set — and a bounded
/// arena count stops 4×ncpu arenas from each holding a retention tail (~140 MB spread at
/// kernel scale). Measured on the Linux tree: default malloc 2.05 GB peak → this config
/// 1.13 GB, equal wall time.
#[cfg(all(feature = "jemalloc", not(any(target_env = "msvc", all(target_env = "musl", target_arch = "aarch64")))))]
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

use vorpal_index::search_index;

const USAGE: &str = "usage:
  vorpal-index index        <src-dir> <index-dir> [--verify] [--parse-health warn|exclude|fail] [--max-error-ratio F]
                                                    build + persist a knowledge graph
  vorpal-index export       <index-root> <file.vidx>  pack the live generation into one shareable artifact
  vorpal-index import       <file.vidx> <index-root>  verify + install an exported generation (atomic CURRENT swap)
  vorpal-index health       <index-dir>             per-file parse damage: byte ratios, error spans, affected entities
  vorpal-index schema       <index-dir>             kinds, relations, grades, tier state — with counts
  vorpal-index dead         <index-dir> [kind]      definitions with no semantic in-edges (suppression-honest)
  vorpal-index coverage     <index-dir>             per-file parse-coverage overview (worst first)
  vorpal-index diff         <index-root> [from] [to]  generation diff (defaults: prev → CURRENT)
  vorpal-index architecture <index-dir> [top]        module mass, hubs, entry-point candidates
  vorpal-index callers      <index-dir> <name>      direct callers of a symbol
  vorpal-index refs         <index-dir> <name>      direct referrers of a symbol
  vorpal-index importers    <index-dir> <name>      files importing a symbol
  vorpal-index implementors <index-dir> <name>      types implementing/extending a symbol
  vorpal-index typeusers    <index-dir> <name>      definitions using a type
  vorpal-index node         <index-dir> <name>      nodes matching a name (append --pattern for regex)
  vorpal-index why          <index-dir> <from-id> <to-id|name>  edge evidence, or why no edge to <name>
  vorpal-index snippet      <index-dir> <name> [context-lines] [--all]  defining source, digest-verified
  vorpal-index search       <index-dir> <query> [k] hybrid search (name + semantic + graph, RRF)";

/// Route tree-sitter's C-side allocations through jemalloc too. Without this the parser's
/// tree memory lives in the macOS default zone — outside jemalloc's decay policy — and
/// freed-but-retained tree pages (~150–250 MB at kernel scale) ride under the link phase's
/// peak. One allocator, one policy.
#[cfg(all(feature = "jemalloc", not(any(target_env = "msvc", all(target_env = "musl", target_arch = "aarch64")))))]
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
  #[cfg(all(feature = "jemalloc", not(any(target_env = "msvc", all(target_env = "musl", target_arch = "aarch64")))))]
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
    ["index", src, out, rest @ ..] => {
      let mut mode = vorpal_index::CacheMode::default();
      let mut policy = vorpal_index::ParseHealthPolicy::default();
      let mut flags = rest.iter();
      while let Some(flag) = flags.next() {
        match *flag {
          "--verify" => mode = vorpal_index::CacheMode::Verified,
          "--parse-health" => {
            policy.mode = match flags.next().copied() {
              Some("warn") => vorpal_index::ParseHealthMode::Warn,
              Some("exclude") => vorpal_index::ParseHealthMode::Exclude,
              Some("fail") => vorpal_index::ParseHealthMode::Fail,
              other => {
                return Err(
                  format!("--parse-health wants warn|exclude|fail, got {other:?}").into(),
                );
              }
            };
          }
          "--max-error-ratio" => {
            policy.max_error_ratio = flags
              .next()
              .and_then(|v| v.parse().ok())
              .ok_or("--max-error-ratio wants a number in [0,1]")?;
          }
          other => return Err(format!("unknown flag '{other}'\n{USAGE}").into()),
        }
      }
      let report =
        vorpal_index::build_index_full(Path::new(src), Path::new(out), mode, policy)?;
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
            "note: {} files had parse errors ({} ERROR nodes across {} bytes; some definitions may be missing — see the 'health' verb)",
            report.error_files, report.error_nodes, report.error_bytes
          );
        }
        if !report.unverified_langs.is_empty() {
          println!(
            "note: {} dynamic language(s) indexed without a canary (best-effort, unverified): {}",
            report.unverified_langs.len(),
            report.unverified_langs.join(", ")
          );
        }
        match &report.cochange_note {
          Some(note) => println!("note: {note}"),
          None => println!("co-change: {} file pairs from git history", report.cochange_edges),
        }
        match &report.similar_note {
          Some(note) => println!("near-clones: {note}"),
          None => println!("near-clones: {} similar_to pairs from token sketches", report.similar_edges),
        }
        if report.request_sites > 0 {
          println!(
            "requests: {} of {} request/emit sites linked to routes/channels",
            report.request_edges, report.request_sites
          );
        }
        if let Some(note) = &report.request_note {
          println!("requests: {note}");
        }
        if report.excluded_files > 0 {
          println!(
            "note: {} unhealthy files excluded from the graph (parse-health policy)",
            report.excluded_files
          );
        }
      }
      Ok(())
    }
    ["export", index, out] => {
      let report = vorpal_index::artifact::export_generation(Path::new(index), Path::new(out))
        .map_err(std::io::Error::other)?;
      println!(
        "exported generation {} ({} artifacts, {} bytes) → {}",
        report.content_id, report.artifacts, report.bytes, out
      );
      Ok(())
    }
    ["import", vidx, index] => {
      let report = vorpal_index::artifact::import_generation(Path::new(vidx), Path::new(index))
        .map_err(std::io::Error::other)?;
      println!(
        "imported generation {} into {} (exporter recorded {})",
        report.installed_id, index, report.exported_id
      );
      if let Some(note) = report.fold_note {
        println!("note: {note}");
      }
      Ok(())
    }
    ["ingest-traces", index, folded] => {
      // Folded stacks (perf/py-spy/inferno collapsed format) → observed.bin sidecar.
      let report = vorpal_index::traces::ingest_traces(
        std::path::Path::new(index),
        std::path::Path::new(folded),
      )?;
      print!("{}", vorpal_index::traces::render_trace_report(&report));
      Ok(())
    }
    ["health", index] => {
      print!("{}", vorpal_index::parse_health_report(Path::new(index))?);
      Ok(())
    }
    ["schema", index] => {
      let dir = vorpal_kg::resolve_index_dir(Path::new(index));
      let kg = vorpal_kg::Kg::load(&dir)?;
      let report = vorpal_index::records::schema_report(&kg, Some(&dir));
      print!("{}", vorpal_index::records::render_schema(&report));
      Ok(())
    }
    [
      verb @ ("callers" | "refs" | "importers" | "implementors" | "typeusers" | "node"),
      index,
      name,
      rest @ ..,
    ] => {
      // Optional trailing selector flags: `--all` merges across same-named definitions
      // (the historical union); `node <regex> --pattern` lists regex name matches. The
      // richer selector surface lives on the `vorpal` CLI.
      if *verb == "node" && rest == ["--pattern"] {
        let dir = vorpal_kg::resolve_index_dir(Path::new(index));
        let kg = vorpal_kg::Kg::load(&dir)?;
        print!("{}", vorpal_index::pattern_query_on(&kg, name, 200)?);
        return Ok(());
      }
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
    ["why", index, from_id, to_or_name] => {
      // Numeric third arg = edge form ("why does this edge exist?"); anything else is a
      // name = absence form ("why is there no edge to anything named X?").
      let rendered = match to_or_name.parse::<u64>() {
        Ok(to_id) => vorpal_index::explain_edge(Path::new(index), from_id.parse()?, to_id)?,
        Err(_) => {
          let dir = vorpal_kg::resolve_index_dir(Path::new(index));
          let kg = vorpal_kg::Kg::load(&dir)?;
          vorpal_index::explain_absence_on(&kg, from_id.parse()?, to_or_name)?
        }
      };
      print!("{rendered}");
      Ok(())
    }
    ["coverage", index] => {
      let dir = vorpal_kg::resolve_index_dir(Path::new(index));
      let report = vorpal_index::records::coverage_records(Some(&dir));
      print!("{}", vorpal_index::records::render_coverage(&report));
      Ok(())
    }
    ["architecture", index, rest @ ..] => {
      let top = match rest {
        [] => 20usize,
        [n] => n.parse().map_err(|_| format!("bad top '{n}'"))?,
        _ => {
          eprintln!("{USAGE}");
          return Err("invalid arguments".into());
        }
      };
      let dir = vorpal_kg::resolve_index_dir(Path::new(index));
      let kg = vorpal_kg::Kg::load(&dir)?;
      let report = vorpal_index::records::architecture_report(&kg, Some(&dir), top.clamp(1, 500));
      print!("{}", vorpal_index::records::render_architecture(&report));
      Ok(())
    }
    ["diff", root, rest @ ..] => {
      let (from, to) = match rest {
        [] => ("prev", "CURRENT"),
        [from] => (*from, "CURRENT"),
        [from, to] => (*from, *to),
        _ => {
          eprintln!("{USAGE}");
          return Err("invalid arguments".into());
        }
      };
      let root = Path::new(root);
      let from_dir = vorpal_index::gendiff::resolve_generation(root, from)?;
      let to_dir = vorpal_index::gendiff::resolve_generation(root, to)?;
      let from_kg = vorpal_kg::Kg::load(&from_dir)?;
      let to_kg = vorpal_kg::Kg::load(&to_dir)?;
      let label = |dir: &Path| {
        dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
      };
      let diff = vorpal_index::gendiff::diff(&from_kg, &to_kg, &label(&from_dir), &label(&to_dir));
      let report = vorpal_index::records::diff_page(
        &from_kg,
        &to_kg,
        diff,
        vorpal_index::records::PageRequest { cursor: None, limit: Some(200) },
      )?;
      print!("{}", vorpal_index::records::render_diff(&report));
      Ok(())
    }
    ["dead", index, rest @ ..] => {
      let kind = match rest {
        [] => None,
        [kind] => Some((*kind).to_string()),
        _ => {
          eprintln!("{USAGE}");
          return Err("invalid arguments".into());
        }
      };
      let dir = vorpal_kg::resolve_index_dir(Path::new(index));
      let kg = vorpal_kg::Kg::load(&dir)?;
      let filter = vorpal_index::records::DeadFilter {
        kind,
        ..Default::default()
      };
      let report = vorpal_index::records::dead_records_page(
        &kg,
        Some(&dir),
        &filter,
        vorpal_index::records::PageRequest { cursor: None, limit: Some(200) },
      )?;
      print!("{}", vorpal_index::records::render_dead(&report));
      Ok(())
    }
    ["snippet", index, name, rest @ ..] => {
      let mut context_lines = 0usize;
      let mut merge_all = false;
      for arg in rest {
        match *arg {
          "--all" => merge_all = true,
          other => context_lines = other.parse().map_err(|_| format!("bad context '{other}'"))?,
        }
      }
      let dir = vorpal_kg::resolve_index_dir(Path::new(index));
      let kg = vorpal_kg::Kg::load(&dir)?;
      let target = vorpal_index::GraphTarget {
        name: (*name).to_string(),
        merge_all,
        ..vorpal_index::GraphTarget::default()
      };
      let rendered =
        vorpal_index::snippet_query_on(&kg, Some(&dir), &target, context_lines, 262_144)
          .map_err(|err| match err {
            vorpal_index::records::SnippetError::Stale(m)
            | vorpal_index::records::SnippetError::Other(m) => m,
          })?;
      print!("{rendered}");
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

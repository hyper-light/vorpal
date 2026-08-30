//! `vorpal grammars`: list the tree-sitter grammars compiled into this binary — the parsers it
//! will actually use, with the generation digest the product cache keys on. Everything printed
//! is read from the linked parsers at runtime, so it reflects the real binary, not a manifest.
//! Provenance (vendored vs. crates.io, upstream commit, local patches) is tracked by hand in
//! `docs/UPSTREAM.md`; this command is the runtime counterpart used to confirm the two agree.

use std::process::ExitCode;

use anyhow::Result;
use vorpal_language::{SupportLang, grammar_info};

#[derive(clap::Args)]
pub struct GrammarsArg {
  /// Show only this language (case-insensitive name, e.g. `python`); omit to list every grammar.
  lang: Option<String>,
}

pub fn run_grammars(arg: GrammarsArg) -> Result<ExitCode> {
  let filter = arg.lang.as_deref().map(str::to_ascii_lowercase);
  let mut shown = 0usize;
  println!("LANGUAGE      ABI    SEMVER   NODES   STATES  DIGEST");
  for lang in SupportLang::all_langs() {
    let name = format!("{lang}");
    if let Some(f) = &filter {
      if name.to_ascii_lowercase() != *f {
        continue;
      }
    }
    let info = grammar_info(*lang);
    let semver = info
      .semver
      .map(|(a, b, c)| format!("{a}.{b}.{c}"))
      .unwrap_or_else(|| "-".into());
    println!(
      "{:<12} {:>4} {:>9} {:>7} {:>8}  {:016x}",
      name, info.abi_version, semver, info.node_kinds, info.parse_states, info.digest
    );
    shown += 1;
  }
  if let Some(f) = &filter {
    if shown == 0 {
      return Err(anyhow::anyhow!("no compiled-in grammar named '{f}'"));
    }
  } else {
    // The registry stamp — the value index manifests actually record — so what this command
    // prints always matches what the whole-tree fast path compares (dynamic langs included).
    println!(
      "\nglobal grammar stamp: {:016x}",
      vorpal_lang_registry::global_grammar_stamp()
    );
    println!("provenance (vendored / upstream commit / local patches): docs/UPSTREAM.md");
  }
  Ok(ExitCode::SUCCESS)
}

//! The differential harness (SUBSECOND.md Phase 3 correctness spine): a seeded random edit
//! sequence over a synthetic corpus, where after EVERY step the incrementally-updated index
//! must converge to a from-scratch build — pinned two ways:
//!
//!   1. generation content-id equality (the total oracle over every artifact byte), and
//!   2. a rendered-answer battery across the graph verbs and hybrid search (the oracle that
//!      survives into the overlay era, where a live view's bytes are allowed to differ but
//!      its ANSWERS are not).
//!
//! Everything is seeded and deterministic: same binary, same failures.

use std::fs;
use std::path::{Path, PathBuf};

/// Tiny deterministic generator (splitmix64) — the harness must not depend on ambient entropy.
struct Rng(u64);
impl Rng {
  fn next(&mut self) -> u64 {
    self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = self.0;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
  }
  fn below(&mut self, bound: usize) -> usize {
    (self.next() % bound as u64) as usize
  }
}

struct Corpus {
  src: PathBuf,
  /// fns[file][slot] = current callee ("none" = leaf) — the ground truth we mutate.
  files: Vec<Vec<String>>,
  next_fn: usize,
}

impl Corpus {
  fn fn_name(id: usize) -> String {
    format!("df_fn_{id}")
  }

  fn render(&self, file: usize) -> String {
    let mut out = String::new();
    for name in &self.files[file] {
      out.push_str(&format!("extern int {name}(int);\n"));
    }
    for (slot, name) in self.files[file].iter().enumerate() {
      // Each function calls the "next" function in the same file when one exists — plus the
      // cross-file callee wiring lives in the shared header of externs above.
      let body = match self.files[file].get(slot + 1) {
        Some(callee) => format!("return {callee}(v) + 1;"),
        None => "return v + 1;".to_string(),
      };
      out.push_str(&format!("int {name}(int v) {{ {body} }}\n"));
    }
    out
  }

  fn write(&self, file: usize) {
    fs::write(self.src.join(format!("f{file}.c")), self.render(file)).unwrap();
  }

  fn write_all(&self) {
    for file in 0..self.files.len() {
      if !self.files[file].is_empty() {
        self.write(file);
      }
    }
  }
}

fn gen_id(out: &Path) -> String {
  fs::read_to_string(out.join("CURRENT")).expect("CURRENT exists")
}

/// Every rendered answer the battery compares. Probes cover live and deleted names.
fn battery(index: &Path, probes: &[String]) -> String {
  let mut out = String::new();
  for name in probes {
    for verb in ["node", "callers", "refs", "importers", "implementors", "typeusers"] {
      out.push_str(&format!("== {verb} {name}\n"));
      match vorpal_index::graph_query(index, verb, name) {
        Ok(rendered) => out.push_str(&rendered),
        Err(err) => out.push_str(&format!("error: {err}\n")),
      }
    }
  }
  for query in ["df fn call chain", "leaf return value", "df_fn_3"] {
    out.push_str(&format!("== search {query}\n"));
    match vorpal_index::search_index(index, query, 5) {
      Ok(rendered) => out.push_str(&rendered),
      Err(err) => out.push_str(&format!("error: {err}\n")),
    }
  }
  out
}

#[test]
fn random_edit_sequences_converge_to_scratch_builds() {
  let root = std::env::temp_dir().join(format!("vorpal-differential-{}", std::process::id()));
  let _ = fs::remove_dir_all(&root);
  let src = root.join("src");
  fs::create_dir_all(&src).unwrap();

  // Seeded corpus: 8 files × 3 functions.
  let mut corpus = Corpus {
    src: src.clone(),
    files: Vec::new(),
    next_fn: 0,
  };
  for _ in 0..8 {
    let mut file = Vec::new();
    for _ in 0..3 {
      file.push(Corpus::fn_name(corpus.next_fn));
      corpus.next_fn += 1;
    }
    corpus.files.push(file);
  }
  corpus.write_all();

  let incremental = root.join("idx");
  vorpal_index::build_index(&src, &incremental).unwrap();

  let mut rng = Rng(0xD1FF_E2E4_71A5_EED0);
  let mut deleted_names: Vec<String> = Vec::new();

  for step in 0..24 {
    // One seeded edit per step, spanning every class the pipeline distinguishes.
    match rng.below(7) {
      // Body edit: rewrite one file with a bumped constant (semantic, node-count stable).
      0 => {
        let file = rng.below(corpus.files.len());
        if !corpus.files[file].is_empty() {
          let mut text = corpus.render(file);
          text.push_str(&format!("/* step {step} */\nint df_extra_{step}(void) {{ return {step}; }}\n"));
          fs::write(src.join(format!("f{file}.c")), text).unwrap();
        }
      }
      // Add a function to a file (changes node counts + candidate sets).
      1 => {
        let file = rng.below(corpus.files.len());
        corpus.files[file].push(Corpus::fn_name(corpus.next_fn));
        corpus.next_fn += 1;
        if !corpus.files[file].is_empty() {
          corpus.write(file);
        }
      }
      // Remove a function (retractions; shadowing candidates shrink).
      2 => {
        let file = rng.below(corpus.files.len());
        if corpus.files[file].len() > 1 {
          let removed = corpus.files[file].pop().unwrap();
          deleted_names.push(removed);
          corpus.write(file);
        }
      }
      // New file (adds a File node + path-form landscape change).
      3 => {
        let file = corpus.files.len();
        corpus.files.push(vec![Corpus::fn_name(corpus.next_fn)]);
        corpus.next_fn += 1;
        corpus.write(file);
      }
      // Delete a file entirely.
      4 => {
        let file = rng.below(corpus.files.len());
        if corpus.files.len() > 2 && !corpus.files[file].is_empty() {
          for name in corpus.files[file].drain(..) {
            deleted_names.push(name);
          }
          let _ = fs::remove_file(src.join(format!("f{file}.c")));
        }
      }
      // Comment-only edit (the stamp-cutoff class).
      5 => {
        let file = rng.below(corpus.files.len());
        if !corpus.files[file].is_empty() {
          let path = src.join(format!("f{file}.c"));
          let mut text = fs::read_to_string(&path).unwrap();
          text.push_str(&format!("// restamp step {step}\n"));
          fs::write(&path, text).unwrap();
        }
      }
      // Pure touch (mtime only).
      _ => {
        let file = rng.below(corpus.files.len());
        if !corpus.files[file].is_empty() {
          let path = src.join(format!("f{file}.c"));
          let text = fs::read_to_string(&path).unwrap();
          fs::write(&path, text).unwrap();
        }
      }
    }

    vorpal_index::build_index(&src, &incremental)
      .unwrap_or_else(|err| panic!("incremental build failed at step {step}: {err}"));
    let scratch = root.join(format!("scratch-{step}"));
    vorpal_index::build_index(&src, &scratch)
      .unwrap_or_else(|err| panic!("scratch build failed at step {step}: {err}"));

    // Oracle 1: total byte convergence.
    assert_eq!(
      gen_id(&incremental),
      gen_id(&scratch),
      "step {step}: incremental generation diverged from scratch"
    );

    // Oracle 2: the rendered-answer battery (the one the overlay era will rely on). Probe a
    // seeded sample of live names plus recently deleted ones.
    let mut probes: Vec<String> = Vec::new();
    let live: Vec<&String> = corpus.files.iter().flatten().collect();
    for _ in 0..4 {
      if !live.is_empty() {
        probes.push(live[rng.below(live.len())].clone());
      }
    }
    if let Some(dead) = deleted_names.last() {
      probes.push(dead.clone());
    }
    assert_eq!(
      battery(&incremental, &probes),
      battery(&scratch, &probes),
      "step {step}: query battery diverged"
    );
    let _ = fs::remove_dir_all(&scratch);
  }

  let _ = fs::remove_dir_all(&root);
}

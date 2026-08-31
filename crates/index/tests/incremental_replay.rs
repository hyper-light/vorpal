//! The replay contract, pinned by COUNTS: after one file changes, exactly that file
//! re-extracts and every other product replays from the prior generation's pack. Output
//! identity was always tested (`incremental_generation_converges_to_from_scratch`); the
//! counts were not — which is how a stale product validator re-parsed ~80% of every
//! incremental build for weeks without a single test noticing.

use std::fs;

use vorpal_index::build_index;

#[test]
fn one_touched_file_reparses_exactly_one_product() {
  let base = std::env::temp_dir().join(format!("vorpal-replay-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  // Every product shape the validator must understand: references with receivers, typed
  // receivers, call arguments (kwargs included), entity params, returns, injections, and a
  // near-clone sketch (a body over the 32-token signing floor).
  fs::write(
    src.join("a.rs"),
    "pub struct Widget;\nimpl Widget {\n  pub fn render(&self, n: u32) -> u32 { n }\n}\n\
     pub fn make() -> Widget { Widget }\n\
     pub fn run(w: &Widget) -> u32 {\n    let m = make();\n    w.render(1) + m.render(2)\n}\n\
     pub fn big(a: u32, b: u32) -> u32 {\n    let mut s = a + b;\n    if s > 10 { s = s / 2; } \
     else { s = s * 3; }\n    while s < 100 { s += a; }\n    s - b\n}\n",
  )
  .unwrap();
  fs::write(
    src.join("b.py"),
    "def sink(value, other=None):\n    return value\n\n\
     class P:\n    def draw(self, x, y=0):\n        return x\n\n\
     def source(k):\n    p = P()\n    p.draw(k, y=k)\n    return sink(k, other=k)\n",
  )
  .unwrap();
  fs::write(src.join("c.c"), "int helper(int x) { return x; }\nint main(void) { return helper(1); }\n")
    .unwrap();
  fs::write(src.join("d.go"), "package m\n\ntype T struct{}\n\nfunc (t T) Go() int { return 1 }\n\nfunc use(t T) int { return t.Go() }\n")
    .unwrap();
  fs::write(src.join("e.ts"), "export class W { render(): number { return 1 } }\nexport function f(w: W) { return w.render() }\n")
    .unwrap();
  fs::write(src.join("f.html"), "<html><script>function g() { return 1 } g();</script></html>\n").unwrap();
  fs::write(src.join("g.json"), "{\"k\": {\"n\": 1}}\n").unwrap();

  let cold = build_index(&src, &out).unwrap();
  assert!(cold.indexed >= 7, "cold indexes every file: {cold:?}");
  assert_eq!(cold.skipped, 0, "nothing replays on a cold build");
  let total = cold.indexed;

  // Unchanged tree: the whole-tree fast path reuses without touching products.
  let warm = build_index(&src, &out).unwrap();
  assert!(warm.reused, "{warm:?}");

  // Touch one file (mtime changes, content does not): exactly one re-extract, the rest replay.
  let touched = src.join("b.py");
  let bytes = fs::read(&touched).unwrap();
  std::thread::sleep(std::time::Duration::from_millis(1100)); // past mtime granularity
  fs::write(&touched, &bytes).unwrap();
  let one = build_index(&src, &out).unwrap();
  assert!(!one.reused);
  assert_eq!(one.indexed, 1, "exactly the touched file re-extracts: {one:?}");
  assert_eq!(one.skipped, total - 1, "every other product replays: {one:?}");

  // Same again with a different language's file, from the generation the touch produced.
  let touched = src.join("a.rs");
  let bytes = fs::read(&touched).unwrap();
  std::thread::sleep(std::time::Duration::from_millis(1100));
  fs::write(&touched, &bytes).unwrap();
  let two = build_index(&src, &out).unwrap();
  assert_eq!(two.indexed, 1, "{two:?}");
  assert_eq!(two.skipped, total - 1, "{two:?}");

  let _ = fs::remove_dir_all(&base);
}

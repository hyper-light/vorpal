#!/usr/bin/env python3
"""Kernel edit classes through the CLI on the scratch copy, quiet-gated, 3 reps each:
body edit (constant inside a function; defs unchanged), comment-only edit, add a function."""
import sys, os, time, json, statistics, subprocess, pathlib, importlib.util
BIN, COPY, OUT = sys.argv[1], pathlib.Path(sys.argv[2]), pathlib.Path(sys.argv[3])
# reuse the driver's gate without running its phases
src = open(str(pathlib.Path(__file__).resolve().with_name("readme_bench.py"))).read().split("# ---------------- index17")[0]
src = src.replace('BIN = sys.argv[1]; OUT = pathlib.Path(sys.argv[2]); OUT.mkdir(parents=True, exist_ok=True)', 'BIN = sys.argv[1]; OUT = pathlib.Path(sys.argv[3]); OUT.mkdir(parents=True, exist_ok=True)')
src = src.replace('PHASES = sys.argv[sys.argv.index("--phases") + 1].split(",") if "--phases" in sys.argv else ["index17", "edit", "tiers", "giant", "scan"]', 'PHASES = []')
g = {}; exec(compile(src, "rb-gate", "exec"), g)
run, wait_quiet, log = g["run"], g["wait_quiet"], g["log"]
target = COPY / "fs" / "read_write.c"; original = target.read_text()
assert "ret = rw_verify_area(READ, file, pos, count);" in original
_anchor = "ret = rw_verify_area(READ, file, pos, count);"
_at = original.index(_anchor, original.index("ssize_t vfs_read(struct file *file"))  # inside vfs_read, not the earlier namesake statement
body_edit = original[:_at + len(_anchor)] + " /* body edit */" + original[_at + len(_anchor):]
comment_edit = original + "\n/* trailing comment */\n"
add_fn = original + "\nint vorpal_bench_probe(void) { return 1; }\n"
rows = []
run([BIN, "index", str(COPY)])  # bring current, drain any bank
for rep in range(3):
    for label, text in (("body edit", body_edit), ("comment only", comment_edit), ("add a function", add_fn)):
        quiet = wait_quiet()
        target.write_text(text)
        env = {**os.environ, "VORPAL_PHASE_TRACE": "1"}
        wall, p = run([BIN, "index", str(COPY)], env=env)
        lane = "full pipeline" if "stream: start" in p.stderr else ("cutoff" if "cutoff" in p.stderr.lower() and "declin" not in p.stderr.lower() else "compose")
        stamps = [l.split("] ", 1)[1].split(" [minflt")[0] for l in p.stderr.splitlines() if "compose" in l or "cutoff" in l or "respan" in l or "defs-" in l or "stream: start" in l][:6]
        target.write_text(original)
        wall_r, pr = run([BIN, "index", str(COPY)])
        row = {"class": label, "rep": rep, "wall_s": round(wall, 2), "restore_s": round(wall_r, 2), "rc": (p.returncode, pr.returncode), "lane": lane, "stamps": stamps, "quiet": quiet}
        rows.append(row); log(row)
        json.dump(rows, open(OUT / "edit_classes.json", "w"), indent=1)
for label in ("body edit", "comment only", "add a function"):
    ws = [r["wall_s"] for r in rows if r["class"] == label]
    log(label, "median", round(statistics.median(ws), 2), "all", ws)

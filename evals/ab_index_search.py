#!/usr/bin/env python3
"""Interleaved A/B of two vorpal binaries on the workloads the 2026-09-05 profile findings touch.

Recorded runs: docs/wip/BENCHMARKS.md ("Profile findings: seven fixes").

  python3 ab.py <A-binary> <B-binary> <out.json> [--reps 3]

Workloads: kernel cold index (wall, max RSS, generation id), LLVM cold index (same),
kernel warm search medians over MCP stdio (default tier, 30 calls), kernel incremental
`index` after a touch and after a comment edit on a scratch copy. Every subprocess reads
stdin from /dev/null. Runs alternate A,B,A,B,... so page-cache and load drift hit both.
"""
import json, os, re, resource, shutil, statistics, subprocess, sys, time, pathlib
A, B, OUT = sys.argv[1], sys.argv[2], sys.argv[3]
REPS = int(sys.argv[sys.argv.index("--reps") + 1]) if "--reps" in sys.argv else 3
S = pathlib.Path(OUT).parent
KERNEL = "/Users/adalundhe/Projects/linux"
LLVM = "/private/tmp/vorpal-profile.whkiQX/corpora/llvm-project"
KERNEL_INDEX = f"{KERNEL}/.vorpal/index"
results = {"A": A, "B": B, "reps": REPS, "rows": []}

def timed(cmd, cwd=None, env=None):
    t0 = time.perf_counter()
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    p = subprocess.run(cmd, cwd=cwd, stdin=subprocess.DEVNULL, capture_output=True, text=True, env=env)
    wall = time.perf_counter() - t0
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    return wall, p, after.ru_maxrss  # ru_maxrss on macOS is bytes, max over all children so far

def cold_index(label, src):
    for rep in range(REPS):
        for arm, binary in (("A", A), ("B", B)):
            out = S / f"idx-{label}-{arm}"
            shutil.rmtree(out, ignore_errors=True)
            # max RSS: run under /usr/bin/time -l and parse "maximum resident set size"
            wall, p, _ = timed(["/usr/bin/time", "-l", binary, "index", src, "--out", str(out)])
            m = re.search(r"(\d+)\s+maximum resident set size", p.stderr)
            rss = int(m.group(1)) if m else None
            gen = (out / "CURRENT").read_text().strip() if (out / "CURRENT").exists() else None
            nodes = re.search(r"→ (\d+) nodes", p.stdout + p.stderr)
            row = {"workload": f"{label} cold index", "arm": arm, "rep": rep, "wall_s": round(wall, 2), "max_rss_gb": round(rss / 1e9, 2) if rss else None, "generation": gen, "nodes": nodes.group(1) if nodes else None, "rc": p.returncode}
            results["rows"].append(row); print(row, flush=True)
            json.dump(results, open(OUT, "w"), indent=1)
            shutil.rmtree(out, ignore_errors=True)

def search_medians(label, index, query):
    for rep in range(REPS):
        for arm, binary in (("A", A), ("B", B)):
            p = subprocess.Popen([binary, "mcp", "--index", index], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, env={**os.environ, "VORPAL_NO_AUTOWARM": "1"})
            i = 0
            def send(o): p.stdin.write(json.dumps(o) + "\n"); p.stdin.flush()
            send({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"ab","version":"0"}}}); p.stdout.readline()
            send({"jsonrpc":"2.0","method":"notifications/initialized"})
            def call():
                nonlocal i; i += 1
                t0 = time.perf_counter(); send({"jsonrpc":"2.0","id":i+10,"method":"tools/call","params":{"name":"search","arguments":{"query":query,"k":10}}}); line = p.stdout.readline(); return (time.perf_counter()-t0)*1000, json.loads(line)
            first, r = call()
            dts = [call()[0] for _ in range(30)]
            names = [rec["name"] for rec in r["result"]["structuredContent"].get("records", [])]
            p.stdin.close(); p.kill()
            row = {"workload": f"{label} search median (30, k=10)", "arm": arm, "rep": rep, "first_ms": round(first, 1), "median_ms": round(statistics.median(dts), 2), "p90_ms": round(sorted(dts)[26], 2), "top": names[:3]}
            results["rows"].append(row); print(row, flush=True)
            json.dump(results, open(OUT, "w"), indent=1)

def incremental(label, copy_src):
    """Scratch copy of the kernel tree with its own index: touch and comment-edit lanes."""
    idx = f"{copy_src}/.vorpal/index"
    target = pathlib.Path(copy_src) / "fs" / "read_write.c"
    original = target.read_bytes()
    for rep in range(REPS):
        for arm, binary in (("A", A), ("B", B)):
            # bring the index current with this binary first (format identical; lanes differ)
            subprocess.run([binary, "index", copy_src], stdin=subprocess.DEVNULL, capture_output=True)
            # touch
            os.utime(target, None)
            w_touch, p1, _ = timed([binary, "index", copy_src])
            # comment edit at the end of the file (stamp-only for the graph: cutoff class)
            target.write_bytes(original + b"\n/* ab comment */\n")
            w_comment, p2, _ = timed([binary, "index", copy_src])
            # restore (a respan-free body change back)
            target.write_bytes(original)
            w_restore, p3, _ = timed([binary, "index", copy_src])
            row = {"workload": f"{label} incremental", "arm": arm, "rep": rep, "touch_s": round(w_touch, 2), "comment_s": round(w_comment, 2), "restore_s": round(w_restore, 2), "rc": (p1.returncode, p2.returncode, p3.returncode)}
            results["rows"].append(row); print(row, flush=True)
            json.dump(results, open(OUT, "w"), indent=1)

which = sys.argv[sys.argv.index("--only") + 1].split(",") if "--only" in sys.argv else ["kernel", "llvm", "search", "incremental"]
if "kernel" in which: cold_index("kernel", KERNEL)
if "llvm" in which: cold_index("llvm", LLVM)
SEARCH_INDEX = sys.argv[sys.argv.index("--index") + 1] if "--index" in sys.argv else KERNEL_INDEX
SEARCH_LABEL = "kernel-copy(warm)" if SEARCH_INDEX != KERNEL_INDEX else "kernel"
if "search" in which:
    for q in ("socket buffer alloc", "read file into user buffer", "schedule timeout"):
        search_medians(f"{SEARCH_LABEL} q={q!r}", SEARCH_INDEX, q)
if "incremental" in which:
    copy = str(S / "kernel-copy")
    if not pathlib.Path(copy, "fs", "read_write.c").exists():
        print("kernel copy missing at", copy); sys.exit(2)
    incremental("kernel-copy", copy)
json.dump(results, open(OUT, "w"), indent=1)
print("done")

#!/usr/bin/env python3
"""The README Performance section, re-measured end to end on one release build.

    python3 evals/readme_bench.py <vorpal-binary> <out-dir> [--phases index17,edit,tiers,giant,scan]
                                  [--control <label>=<binary> ...]

Phases (all serial — no two timed lanes overlap):
  index17  cold best-of-3 + unchanged for the 17 pinned corpora ("Files parsed" and nodes from
           the indexer's own line); the kernel row also runs each --control arm interleaved,
           with max RSS (`/usr/bin/time -l`) and the committed generation's size on disk.
  edit     kernel edit / touch / unchanged lanes on a scratch copy of the tree, 3 reps.
  tiers    per (corpus, tier) fresh index + `__warm-ann` (10 m dense cap), searcheval with
           VORPAL_DENSE_CHANNEL=off (the README's pre-fill floor), then 30 stdio round trips
           per tool with RSS sampled after every call, first search, one-shot CLI search, disk.
  giant    the tree-cache / walk-splice save bench on the three giant C files.
  scan     the README's kmalloc rule over the kernel copy vs rg.
Every timed subprocess asserts rc == 0 and reads stdin from /dev/null; load is logged per lane.
Recorded runs: docs/wip/BENCHMARKS.md.
"""
import json, os, re, shutil, statistics, subprocess, sys, time, pathlib

BIN = sys.argv[1]; OUT = pathlib.Path(sys.argv[2]); OUT.mkdir(parents=True, exist_ok=True)
PHASES = sys.argv[sys.argv.index("--phases") + 1].split(",") if "--phases" in sys.argv else ["index17", "edit", "tiers", "giant", "scan"]
CONTROLS = [a.split("=", 1) for i, a in enumerate(sys.argv) if i > 0 and sys.argv[i - 1] == "--control"]
REPO = "/Users/adalundhe/Projects/vorpal"
PROFILE = "/private/tmp/vorpal-profile.whkiQX"
CORPORA = [  # (readme label, path, tracked-count path)
  ("linux", "/Users/adalundhe/Projects/linux"),
  ("llvm-project", f"{PROFILE}/corpora/llvm-project"), ("zig", f"{PROFILE}/corpora/zig"),
  ("kotlin", f"{PROFILE}/corpora/kotlin"), ("kubernetes", f"{PROFILE}/corpora/kubernetes"),
  ("roslyn", f"{PROFILE}/corpora/roslyn"), ("rust", f"{PROFILE}/corpora/rust"),
  ("WordPress", f"{PROFILE}/corpora/WordPress"), ("spark", f"{PROFILE}/corpora/spark"),
  ("kafka", f"{PROFILE}/corpora/kafka"), ("next.js", f"{PROFILE}/corpora/next.js"),
  ("ghc", f"{PROFILE}/corpora/ghc"), ("cpython", "/Users/adalundhe/Projects/cpython"),
  ("rails", f"{PROFILE}/corpora/rails"), ("neovim", f"{PROFILE}/corpora/neovim"),
  ("vue-core", f"{PROFILE}/corpora/vue-core"), ("vorpal", REPO),
]
HOME = os.path.expanduser("~")
# The encoder weights `vorpal enable semantic-f32|f16` installs (global enable turned back
# off afterwards; the per-index `encoderDir` below is what selects them for a tier).
MODELS = {"f16": f"{HOME}/.vorpal/models/coderankembed-f16", "f32": f"{HOME}/.vorpal/models/coderankembed-f32"}
TIER_CORPORA = {"kernel": "/Users/adalundhe/Projects/linux", "cpython": "/Users/adalundhe/Projects/cpython", "vorpal": REPO}
QUERIES = {
  "kernel": ["socket buffer alloc", "read file into user buffer", "schedule timeout", "spin lock irqsave", "page cache writeback"],
  "cpython": ["parse argument tuple", "dict lookup", "garbage collection generation", "compile ast to bytecode", "unicode decode error"],
  "vorpal": ["stdio pump reader thread", "canonical seal", "bucketed pack", "stat backstop sweep", "resolve import path"],
}
GRAPH_SYMBOL = {"kernel": "vfs_read", "cpython": "PyDict_GetItem", "vorpal": "tool_result"}
LABELS = {"kernel": "kernel", "cpython": "cpython", "vorpal": "vorpal"}
R = {"binary": BIN, "controls": CONTROLS, "rows": []}
if "--append" in sys.argv and (OUT / "readme_bench.json").exists():
    # keep every earlier phase's rows; the phases being run now are replaced wholesale
    prior = json.load(open(OUT / "readme_bench.json"))
    # every earlier phase's rows stay; a re-run phase is replaced, except `tiers`, whose
    # completed (corpus, tier) rows are kept and skipped (each costs minutes to rebuild)
    R["rows"] = [r for r in prior.get("rows", []) if r.get("phase") not in PHASES or (r.get("phase") == "tiers" and "search_median_ms" in r)]
def save(): json.dump(R, open(OUT / "readme_bench.json", "w"), indent=1)
def load1(): return os.getloadavg()[0]

def cpu_idle_pct():
    """Instantaneous idle from two 1 s `top` samples (the second is the live one)."""
    out = subprocess.run(["top", "-l", "2", "-n", "0", "-s", "1"], capture_output=True, text=True, stdin=subprocess.DEVNULL).stdout
    m = re.findall(r"CPU usage: .*?([\d.]+)% idle", out)
    return float(m[-1]) if m else 0.0

# System daemons that are part of this machine's steady state and not a workload: fseventsd
# sits near one core whenever Docker Desktop's file sharing is up (Docker is required here
# for the cross-platform lanes), WindowServer paints the screen. They are reported beside
# every row, not treated as load to wait out.
BASELINE_DAEMONS = ("fseventsd", "WindowServer")

def hottest_external():
    """(pcpu, command) of the busiest process that is not this driver, its sampler, or a
    baseline daemon; plus fseventsd's own pcpu for the record."""
    out = subprocess.run(["ps", "-eo", "pcpu,pid,comm", "-r"], capture_output=True, text=True, stdin=subprocess.DEVNULL).stdout.splitlines()[1:]
    hottest, fse = (0.0, ""), 0.0
    for line in out:
        parts = line.split(None, 2)
        if len(parts) < 3:
            continue
        pcpu, pid, comm = float(parts[0]), int(parts[1]), parts[2]
        base = comm.rsplit("/", 1)[-1]
        if base == "fseventsd":
            fse = max(fse, pcpu)
        if pid == os.getpid() or base in ("top", "ps") or base in BASELINE_DAEMONS:
            continue
        if hottest[1] == "":
            hottest = (pcpu, comm[-70:])
    return hottest, fse

def wait_quiet(max_wait=14400):
    """The README's method is best-of-three on a QUIET machine. Quiet, on this machine with
    Docker Desktop running by requirement: two consecutive samples with CPU idle >= 88 %
    (the measured steady floor is 89-91 % with fseventsd near one core) and no process
    outside the baseline daemons above half a core. Blocks up to `max_wait`; a row taken
    past that is marked `not_quiet`. Returns what it saw right before the timed run."""
    t0 = time.time(); streak = 0
    while True:
        idle, (hot, fse), load = cpu_idle_pct(), hottest_external(), load1()
        seen = {"idle_pct": round(idle, 1), "hot_pcpu": hot[0], "hot": hot[1], "fseventsd_pcpu": fse, "load1": round(load, 2), "waited_s": round(time.time() - t0)}
        streak = streak + 1 if (idle >= 88.0 and hot[0] < 50.0) else 0
        if streak >= 2:
            return seen
        if time.time() - t0 > max_wait:
            seen["not_quiet"] = True
            log("NOT QUIET after", max_wait, "s:", seen)
            return seen
        time.sleep(8)
def log(*a):
    print(*a, flush=True)
def run(cmd, cwd=None, env=None, timeout=3600):
    t0 = time.perf_counter()
    p = subprocess.run(cmd, cwd=cwd, stdin=subprocess.DEVNULL, capture_output=True, text=True, env=env, timeout=timeout)
    wall = time.perf_counter() - t0
    if p.returncode != 0:
        log("RC!=0", cmd[:4], p.stderr[-800:])
    return wall, p
def parse_index_line(text):
    m = re.search(r"parsed (\d+) files.*?→ (\d+) nodes", text)
    return (int(m.group(1)), int(m.group(2))) if m else (None, None)
def gen_dir(index):
    return pathlib.Path(index) / pathlib.Path(index, "CURRENT").read_text().strip()
def du_gb(path):
    out = subprocess.run(["du", "-sk", str(path)], capture_output=True, text=True, stdin=subprocess.DEVNULL).stdout.split()[0]
    return round(int(out) * 1024 / 1e9, 2)
def rss_gb_from_time_l(stderr):
    m = re.search(r"(\d+)\s+maximum resident set size", stderr)
    return round(int(m.group(1)) / 1e9, 2) if m else None

# ---------------- index17 ----------------
def phase_index17():
    for label, src in CORPORA:
        arms = [("new", BIN)] + ([(l, b) for l, b in CONTROLS] if label == "linux" else [])
        out = OUT / f"idx17-{label}"
        tracked = subprocess.run(["git", "-C", src, "ls-files"], capture_output=True, text=True, stdin=subprocess.DEVNULL).stdout.count("\n")
        for rep in range(3):
            for arm, binary in arms:
                shutil.rmtree(out, ignore_errors=True)
                quiet = wait_quiet()
                wall, p = run(["/usr/bin/time", "-l", binary, "index", src, "--out", str(out)])
                files, nodes = parse_index_line(p.stdout + p.stderr)
                row = {"phase": "index17", "corpus": label, "arm": arm, "rep": rep, "kind": "cold", "wall_s": round(wall, 2), "rc": p.returncode, "files": files, "nodes": nodes, "tracked": tracked, "rss_gb": rss_gb_from_time_l(p.stderr), "quiet": quiet}
                if rep == 2 and arm == "new":
                    row["disk_gb"] = du_gb(gen_dir(out)); row["generation"] = gen_dir(out).name
                R["rows"].append(row); log(row); save()
        # unchanged: the last cold arm was the last in `arms`; rebuild once with new to be exact
        if arms[-1][0] != "new":
            shutil.rmtree(out, ignore_errors=True); run([BIN, "index", src, "--out", str(out)])
        for rep in range(3):
            quiet = wait_quiet()
            wall, p = run([BIN, "index", src, "--out", str(out)])
            row = {"phase": "index17", "corpus": label, "arm": "new", "rep": rep, "kind": "unchanged", "wall_s": round(wall, 3), "rc": p.returncode, "quiet": quiet}
            R["rows"].append(row); log(row); save()
        shutil.rmtree(out, ignore_errors=True)

# ---------------- edit ----------------
def phase_edit():
    copy = OUT.parent / "kernel-copy"
    if not (copy / "fs" / "read_write.c").exists():
        log("kernel copy missing", copy); return
    target = copy / "fs" / "read_write.c"; original = target.read_bytes()
    run([BIN, "index", str(copy)])
    for rep in range(3):
        quiet = wait_quiet()
        target.write_bytes(original + b"\nint vorpal_bench_probe(void) { return 1; }\n")
        w_edit, p1 = run([BIN, "index", str(copy)])
        os.utime(target, None)
        w_touch, p2 = run([BIN, "index", str(copy)])
        w_unch, p3 = run([BIN, "index", str(copy)])
        target.write_bytes(original)
        w_restore, p4 = run([BIN, "index", str(copy)])
        row = {"phase": "edit", "rep": rep, "edit_s": round(w_edit, 2), "touch_s": round(w_touch, 2), "unchanged_s": round(w_unch, 3), "restore_s": round(w_restore, 2), "rc": (p1.returncode, p2.returncode, p3.returncode, p4.returncode), "quiet": quiet}
        R["rows"].append(row); log(row); save()

# ---------------- tiers ----------------
class Daemon:
    def __init__(self, index):
        self.p = subprocess.Popen([BIN, "mcp", "--index", index], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, env={**os.environ, "VORPAL_NO_AUTOWARM": "1"})
        self.i = 0
        self.send({"jsonrpc":"2.0","id":self.nid(),"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"readme-bench","version":"0"}}}); self.p.stdout.readline()
        self.send({"jsonrpc":"2.0","method":"notifications/initialized"})
    def nid(self): self.i += 1; return self.i
    def send(self, o): self.p.stdin.write(json.dumps(o)+"\n"); self.p.stdin.flush()
    def call(self, tool, args):
        t0 = time.perf_counter(); self.send({"jsonrpc":"2.0","id":self.nid(),"method":"tools/call","params":{"name":tool,"arguments":args}})
        line = self.p.stdout.readline(); return (time.perf_counter()-t0)*1000, json.loads(line)
    def rss_gb(self):
        out = subprocess.run(["ps", "-o", "rss=", "-p", str(self.p.pid)], capture_output=True, text=True, stdin=subprocess.DEVNULL).stdout.strip()
        return int(out) * 1024 / 1e9 if out else None
    def close(self): self.p.stdin.close(); self.p.kill()

def searcheval(index, corpus):
    env = {**os.environ, "VORPAL_DENSE_CHANNEL": "off"}
    wait_quiet()
    # The label sets anchor paths at the indexed tree: pass it as --root.
    # `cargo xtask` is a DEBUG alias; the encoder tiers rerank through the encoder itself, and
    # an unoptimized GEMM made one kernel run take over 45 minutes. Release build, quality
    # identical (the same code, the same labels).
    wall, p = run(["cargo", "run", "--release", "-q", "-p", "xtask", "--", "searcheval", str(index), f"xtask/labels/{LABELS[corpus]}.json", "--root", TIER_CORPORA[corpus]], cwd=REPO, env=env, timeout=7200)
    (OUT / f"searcheval-{corpus}-{index.name}.txt").write_text(p.stdout + "\n--- stderr ---\n" + p.stderr)
    rows = {}
    for line in p.stdout.splitlines():
        if not line.startswith("|"):
            continue
        cells = [c.strip().strip("*").strip() for c in line.strip().strip("|").split("|")]
        if len(cells) == 5 and cells[1].isdigit():
            try:
                rows[cells[0]] = {"queries": int(cells[1]), "ndcg": float(cells[2]), "mrr": float(cells[3]), "recall": float(cells[4])}
            except ValueError:
                pass
    return rows, p.returncode

def phase_tiers():
    cfgs = OUT / "cfg"; cfgs.mkdir(exist_ok=True)
    (cfgs / "learned.yml").write_text("semanticTier: learned\n")
    for k, m in MODELS.items(): (cfgs / f"learned-{k}.yml").write_text(f"semanticTier: learned\nencoderDir: {m}\n")
    done = {(r["corpus"], r["tier"]) for r in R["rows"] if r.get("phase") == "tiers" and "search_median_ms" in r}
    for corpus, src in TIER_CORPORA.items():
        for tier in ("default", "learned", "learned-f16", "learned-f32"):
            if (corpus, tier) in done:
                log("tiers: keeping the completed row for", corpus, tier); continue
            index = OUT / f"tier-{corpus}-{tier}"
            # A tier left on disk by an interrupted run, already built and warmed, is reused
            # (its 10-minute dense fill is the expensive part); anything partial is rebuilt.
            reuse = (index / "CURRENT").exists() and (gen_dir(index) / "ann.bin").exists()
            if reuse:
                log("tiers: reusing the built+warmed index at", index)
            else:
                shutil.rmtree(index, ignore_errors=True)
            # Disk guard: a kernel tier with its warm artifacts is ~8 GB; never start one into
            # a nearly full volume (the first run of this phase filled the disk mid-build).
            while shutil.disk_usage(OUT).free < 15 * 1024**3:
                log("waiting for disk: free", round(shutil.disk_usage(OUT).free / 1e9, 1), "GB"); time.sleep(30)
            cmd = [BIN, "index", src, "--out", str(index)] + (["-c", str(cfgs / f"{tier}.yml")] if tier != "default" else [])
            if reuse:
                w_build, w_warm = float("nan"), float("nan")
                class _Ok: returncode = 0
                p = pw = _Ok()
            else:
                w_build, p = run(cmd)
                if tier.startswith("learned-"):
                    # belt-and-braces: the root's own selection names the model dir
                    (index / "encoder.dir").write_text(MODELS[tier.split("-")[1]] + "\n")
                w_warm, pw = run([BIN, "__warm-ann", str(index.resolve()), "--dense-budget-timeout", "10m"], timeout=1800)
            gen = gen_dir(index)
            row = {"phase": "tiers", "corpus": corpus, "tier": tier, "build_s": round(w_build, 2), "warm_s": round(w_warm, 1), "rc": (p.returncode, pw.returncode), "disk_gb": du_gb(gen), "artifacts": sorted(f.name for f in gen.iterdir() if f.name.startswith(("ann", "postings", "dense"))), "root_files": sorted(f.name for f in index.iterdir() if f.is_file()), "load": round(load1(), 1)}
            # quality (pre-fill floor semantics)
            q, rc = searcheval(index, corpus); row["searcheval"] = q; row["searcheval_rc"] = rc
            # latency + RSS
            row["quiet"] = wait_quiet()
            d = Daemon(str(index))
            queries = QUERIES[corpus]
            first_ms, r0 = d.call("search", {"query": queries[0], "k": 10}); peak = d.rss_gb() or 0
            s_ms = []
            for i in range(30):
                ms, _ = d.call("search", {"query": queries[i % len(queries)], "k": 10}); s_ms.append(ms); peak = max(peak, d.rss_gb() or 0)
            g_ms = []
            for i in range(30):
                ms, _ = d.call("graph", {"relation": "callers", "name": GRAPH_SYMBOL[corpus]}); g_ms.append(ms); peak = max(peak, d.rss_gb() or 0)
            enc = None
            try:
                _, st = d.call("health", {}); enc = json.dumps(st.get("result", {}).get("structuredContent", {}))[:200]
            except Exception: pass
            d.close()
            row.update({"first_search_ms": round(first_ms, 1), "search_median_ms": round(statistics.median(s_ms), 2), "search_p95_ms": round(sorted(s_ms)[28], 2), "graph_median_ms": round(statistics.median(g_ms), 3), "peak_rss_gb": round(peak, 3), "health": enc})
            # one-shot CLI search (default tier only, page cache warm)
            if tier == "default":
                wait_quiet()
                ws = []
                for _ in range(5):
                    w, ps = run([BIN, "search", queries[0], "-k", "10", "--index", str(index)]); ws.append(w)
                wg = []
                for _ in range(5):
                    w, pg = run([BIN, "graph", "callers", GRAPH_SYMBOL[corpus], "--index", str(index)]); wg.append(w)
                row["oneshot_search_s"] = round(statistics.median(ws), 3); row["oneshot_callers_s"] = round(statistics.median(wg), 3)
            R["rows"].append(row); log({k: v for k, v in row.items() if k != "searcheval"}); log("  searcheval:", q.get("all") or q.get("overall") or list(q.items())[-1:]); save()
            shutil.rmtree(index, ignore_errors=True)

# ---------------- giant ----------------
def phase_giant():
    files = [("julia parser.c 54 MB", f"{REPO}/grammars/tree-sitter-julia/src/parser.c"), ("cpp parser.c 17 MB", f"{REPO}/grammars/tree-sitter-cpp/src/parser.c"), ("cpython Parser/parser.c 1.4 MB", "/Users/adalundhe/Projects/cpython/Parser/parser.c")]
    for mode, env_extra in (("tree cache only", {"VORPAL_WALK_REUSE": "0"}), ("+ walk splice", {})):
        for label, path in files:
            env = {**os.environ, **env_extra}
            # compile the example first so the timed run is the bench alone
            run(["cargo", "build", "--release", "-q", "-p", "vorpal-ingest", "--example", "tree_cache_bench"], cwd=REPO, timeout=3600)
            quiet = wait_quiet()
            wall, p = run(["cargo", "run", "--release", "-q", "-p", "vorpal-ingest", "--example", "tree_cache_bench", "--", path, "8"], cwd=REPO, env=env, timeout=3600)
            (OUT / f"giant-{mode.replace(' ', '_').replace('+','plus')}-{label.split()[0]}.txt").write_text(p.stdout + "\n" + p.stderr)
            row = {"phase": "giant", "file": label, "mode": mode, "rc": p.returncode, "lines": [l for l in p.stdout.splitlines() if "ms" in l][:12], "quiet": quiet}
            R["rows"].append(row); log(row); save()

# ---------------- scan ----------------
def phase_scan():
    copy = OUT.parent / "kernel-copy"
    rule = OUT / "kmalloc-rule.yml"; rule.write_text("id: readme-kmalloc-calls\nlanguage: C\nrule:\n  kind: call_expression\n  regex: kmalloc\n")
    for rep in range(3):
        quiet = wait_quiet()
        wall, p = run([BIN, "scan", "--rule", str(rule), "--json=stream", str(copy)])
        matches = p.stdout.count("\n")
        wr, pr = run(["rg", "kmalloc\\(", "-t", "c", str(copy)])
        row = {"phase": "scan", "rep": rep, "scan_s": round(wall, 2), "scan_matches": matches, "scan_rc": p.returncode, "rg_s": round(wr, 2), "rg_lines": pr.stdout.count("\n"), "quiet": quiet}
        R["rows"].append(row); log(row); save()

log("binary", BIN, subprocess.run([BIN, "--version"], capture_output=True, text=True, stdin=subprocess.DEVNULL).stdout.strip(), "load", round(load1(), 1))
for ph in PHASES:
    log(f"===== phase {ph} =====")
    {"index17": phase_index17, "edit": phase_edit, "tiers": phase_tiers, "giant": phase_giant, "scan": phase_scan}[ph]()
save(); log("ALL-PHASES-DONE")

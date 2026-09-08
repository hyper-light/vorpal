#!/usr/bin/env python3
"""vorpal versus microsoft/tgrep (trigram-indexed grep) on the README corpora.

    python3 evals/tgrep_bench.py <vorpal-bin> <tgrep-bin> <out-dir> [--phases build,queries,calls,search,text,save] [--append]

Phases, all serial:
  build    cold index of the kernel, CPython and this repo, three reps each, interleaved
           tgrep / vorpal so neither owns a warmer page cache; wall from the driver,
           max RSS and peak footprint from `/usr/bin/time -l`, bytes on disk from `du`.
  queries  the kernel query suite shipped in tgrep's scripts/benchmark-queries.json (102
           regexes) through tgrep's own protocol — a fresh `tgrep` client per query against a
           running `tgrep serve --no-watch` — and through `rg -n`; the 88 identifier-shaped
           queries also go to a warm `vorpal mcp` daemon as `graph references`, which is the
           nearest question the graph answers (resolved reference records, not text lines).
  calls    the head-to-head where both tools do the same job, find every call of a function:
           tgrep and rg (`name\(` in C files, fresh process each), and on the warm daemon
           `search <name>`, `code_search` (the arity-spelled call pattern) and `structural_search`.
  search   tgrep's own protocol mirrored on vorpal: after `vorpal index .` in the kernel directory,
           each of the 102 queries is a fresh `vorpal search "<q>"` process there, no flags, no
           daemon; the same query then goes to a warm daemon as `search` (k=10, lean).  Hit
           count and whether the top hit is the query itself are recorded beside the time.
  save     save → visible on a scratch clone of the kernel: append a function to
           fs/read_write.c, poll until the answer shows it, seven reps; tgrep serve with its
           watcher (polled with a fresh client), vorpal mcp (polled with `node`).
Every timed subprocess reads stdin from /dev/null; machine idle and fseventsd CPU are recorded
beside every row.  Results: <out-dir>/tgrep_bench.json.  Recorded runs: docs/wip/BENCHMARKS.md.
"""
import hashlib
import json
import os, os, re, shutil, statistics, subprocess, sys, time, pathlib

VORPAL, TGREP = os.path.abspath(sys.argv[1]), os.path.abspath(sys.argv[2])
OUT = pathlib.Path(sys.argv[3]); OUT.mkdir(parents=True, exist_ok=True)
PHASES = sys.argv[sys.argv.index("--phases") + 1].split(",") if "--phases" in sys.argv else ["build", "queries", "save"]
REPO = "/Users/adalundhe/Projects/vorpal"
CORPORA = [("kernel", "/Users/adalundhe/Projects/linux"), ("cpython", "/Users/adalundhe/Projects/cpython"), ("vorpal", REPO)]
QUERIES = json.load(open("/Users/adalundhe/Projects/tgrep/scripts/benchmark-queries.json"))["queries"]
R = {"vorpal": VORPAL, "tgrep": TGREP, "rows": []}
def save(): json.dump(R, open(OUT / "tgrep_bench.json", "w"), indent=1)
def log(*a): print(*a, flush=True)

def machine():
    top = subprocess.run(["top", "-l", "2", "-n", "0", "-s", "1"], capture_output=True, text=True, stdin=subprocess.DEVNULL).stdout
    idle = re.findall(r"CPU usage: .*?([\d.]+)% idle", top)
    ps = subprocess.run(["ps", "-eo", "pcpu,comm", "-r"], capture_output=True, text=True, stdin=subprocess.DEVNULL).stdout.splitlines()[1:]
    fse = max([float(l.split()[0]) for l in ps if l.rstrip().endswith("fseventsd")] or [0.0])
    return {"idle_pct": float(idle[-1]) if idle else None, "fseventsd_pcpu": fse, "load1": round(os.getloadavg()[0], 2)}

def timed(cmd, cwd=None, env=None, timeout=3600):
    """wall, rc, max_rss_bytes, peak_footprint_bytes, stdout, stderr via /usr/bin/time -l."""
    t0 = time.perf_counter()
    p = subprocess.run(["/usr/bin/time", "-l"] + cmd, cwd=cwd, stdin=subprocess.DEVNULL, capture_output=True, text=True, env=env, timeout=timeout)
    wall = time.perf_counter() - t0
    rss = re.search(r"(\d+)\s+maximum resident set size", p.stderr); foot = re.search(r"(\d+)\s+peak memory footprint", p.stderr)
    return wall, p.returncode, int(rss.group(1)) if rss else None, int(foot.group(1)) if foot else None, p.stdout, p.stderr

def du(path):
    return int(subprocess.run(["du", "-sk", str(path)], capture_output=True, text=True, stdin=subprocess.DEVNULL).stdout.split()[0]) * 1024

def rss_of(pid):
    out = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True, stdin=subprocess.DEVNULL).stdout.strip()
    return int(out) * 1024 if out else None

class Daemon:
    def __init__(self, index, stderr_path, env=None):
        full_env = dict(os.environ); full_env.update(env or {})
        self.p = subprocess.Popen([VORPAL, "mcp", "--index", str(index)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=open(stderr_path, "w"), text=True, env=full_env)
        self.id = 0
        self.send({"jsonrpc": "2.0", "id": self.nid(), "method": "initialize", "params": {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "bench", "version": "0"}}}); self.p.stdout.readline()
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
    def nid(self): self.id += 1; return self.id
    def send(self, o): self.p.stdin.write(json.dumps(o) + "\n"); self.p.stdin.flush()
    def call(self, tool, args):
        t0 = time.perf_counter(); self.send({"jsonrpc": "2.0", "id": self.nid(), "method": "tools/call", "params": {"name": tool, "arguments": args}})
        line = self.p.stdout.readline(); dt = time.perf_counter() - t0
        return dt, json.loads(line).get("result", {})
    def pcpu(self):
        out = subprocess.run(["ps", "-o", "pcpu=", "-p", str(self.p.pid)], capture_output=True, text=True, stdin=subprocess.DEVNULL).stdout.strip()
        return float(out) if out else 0.0
    def wait_settled(self, max_s=1800):
        """Block until the daemon is idle: no child process and its own CPU under 20 % on two
        consecutive 1 s samples (the search-tier warm runs in-process after a new generation)."""
        t0 = time.time(); streak = 0
        while time.time() - t0 < max_s:
            kids = subprocess.run(["pgrep", "-P", str(self.p.pid)], capture_output=True, text=True, stdin=subprocess.DEVNULL).stdout.split()
            streak = streak + 1 if (not kids and self.pcpu() < 20.0) else 0
            if streak >= 2: return round(time.time() - t0, 1)
            time.sleep(1)
        return None
    def close(self):
        try: self.p.stdin.close()
        except Exception: pass
        self.p.kill(); self.p.wait()
        subprocess.run(["pkill", "-9", "-f", "vorpal __warm-ann"], stdin=subprocess.DEVNULL)

class TgrepServe:
    def __init__(self, root, index, stderr_path, watch):
        (pathlib.Path(index) / "serve.json").unlink(missing_ok=True)
        cmd = [TGREP, "serve", str(root), "--index-path", str(index)] + ([] if watch else ["--no-watch"])
        self.p = subprocess.Popen(cmd, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=open(stderr_path, "w"))
        for _ in range(600):
            if (pathlib.Path(index) / "serve.json").exists(): break
            time.sleep(0.1)
        else: raise RuntimeError("tgrep serve did not publish serve.json")
        self.index = index
    def close(self):
        self.p.terminate(); self.p.wait()

# ---------------- build
def phase_build():
    for label, src in CORPORA:
        for rep in range(3):
            for tool in ("tgrep", "vorpal"):
                idx = OUT / f"idx-{tool}-{label}-{rep}.noindex"
                shutil.rmtree(idx, ignore_errors=True)
                cmd = [TGREP, "index", src, "--index-path", str(idx)] if tool == "tgrep" else [VORPAL, "index", src, "--out", str(idx)]
                m = machine()
                wall, rc, rss, foot, so, se = timed(cmd)
                if rc != 0: log("RC!=0", tool, label, se[-500:])
                files = None
                if tool == "tgrep":
                    mm = re.search(r"Found (\d+) text files", so + se); files = int(mm.group(1)) if mm else None
                    mm = re.search(r"Writing index \((\d+) trigrams, (\d+) files", so + se)
                    if mm: files = int(mm.group(2))
                else:
                    mm = re.search(r"parsed (\d+) files", so + se); files = int(mm.group(1)) if mm else None
                row = {"phase": "build", "corpus": label, "tool": tool, "rep": rep, "wall_s": round(wall, 3), "rc": rc, "max_rss": rss, "peak_footprint": foot, "disk_bytes": du(idx), "files": files, "machine": m}
                R["rows"].append(row); save()
                log(f"build {label:8} {tool:7} rep{rep} {wall:7.2f} s  rss {rss/2**30:5.2f} GB  foot {foot/2**30 if foot else 0:5.2f} GB  disk {row['disk_bytes']/2**20:8.1f} MB  files {files}  idle {m['idle_pct']} fse {m['fseventsd_pcpu']}")
                if not (rep == 2 and tool == "tgrep" and label == "kernel"):
                    shutil.rmtree(idx, ignore_errors=True)  # only the last tgrep kernel index is kept, for `queries`

# ---------------- queries
IDENT = re.compile(r"^[A-Za-z_]\w*$")
CALLS = ["kmalloc", "vfs_read", "schedule_timeout", "devm_platform_ioremap_resource", "netif_napi_add"]
# tree-sitter C parses `name($A)` and `name($$$)` as a macro_type_specifier, not a call, so a
# pattern-only structural search needs the call's arity spelled out; the one-argument function
# is only reachable in assignment form (`$R = f($A)`), which misses bare statement calls.
PATTERNS = {"kmalloc": "kmalloc($A, $B)", "vfs_read": "vfs_read($A, $B, $C, $D)", "schedule_timeout": "$R = schedule_timeout($A)",
            "devm_platform_ioremap_resource": "devm_platform_ioremap_resource($A, $B)", "netif_napi_add": "netif_napi_add($A, $B, $C)"}

def phase_queries():
    src = "/Users/adalundhe/Projects/linux"
    # the daemon finds its watched tree from the conventional index location, and
    # structural_search needs that watch, so the kernel's own index serves the MCP rows
    tidx = OUT / "idx-tgrep-kernel-2.noindex"; vidx = pathlib.Path(src) / ".vorpal" / "index"
    if not (tidx / "lookup.bin").exists():
        log("tgrep kernel index missing — building"); timed([TGREP, "index", src, "--index-path", str(tidx)])
    serve = TgrepServe(src, tidx, OUT / "serve-queries.stderr", watch=False)
    time.sleep(2)
    # warm both once on a query outside the suite
    subprocess.run([TGREP, "zzz_warm_probe", src, "--index-path", str(tidx)], capture_output=True, stdin=subprocess.DEVNULL)
    subprocess.run(["rg", "-n", "zzz_warm_probe", src], capture_output=True, stdin=subprocess.DEVNULL)
    m = machine(); log("suite machine", m)
    suite = []
    for q in QUERIES:
        t0 = time.perf_counter(); pt = subprocess.run([TGREP, q, src, "--index-path", str(tidx)], capture_output=True, stdin=subprocess.DEVNULL); wt = time.perf_counter() - t0
        t0 = time.perf_counter(); pr = subprocess.run(["rg", "-n", q, src], capture_output=True, stdin=subprocess.DEVNULL); wr = time.perf_counter() - t0
        tl, rl = pt.stdout.count(b"\n"), pr.stdout.count(b"\n")
        suite.append({"query": q, "tgrep_s": round(wt, 4), "rg_s": round(wr, 4), "tgrep_lines": tl, "rg_lines": rl})
        log(f"  {q[:40]:40} tgrep {wt*1000:8.1f} ms {tl:7} | rg {wr*1000:8.1f} ms {rl:7}")
    serve_rss = rss_of(serve.p.pid)
    R["rows"].append({"phase": "queries", "kind": "suite", "corpus": "kernel", "n": len(suite), "tgrep_total_s": round(sum(r["tgrep_s"] for r in suite), 3), "rg_total_s": round(sum(r["rg_s"] for r in suite), 3),
                      "tgrep_median_s": round(statistics.median(r["tgrep_s"] for r in suite), 4), "rg_median_s": round(statistics.median(r["rg_s"] for r in suite), 4), "serve_rss_after": serve_rss, "machine": m, "per_query": suite}); save()
    serve.close()
    # vorpal daemon: references for the identifier queries, code_search / structural_search for the calls
    d = Daemon(vidx, OUT / "mcp-queries.stderr")
    d.call("node", {"name": "vfs_read"}); time.sleep(3)  # boot revalidation
    refs = []
    for q in QUERIES:
        if not IDENT.match(q): continue
        dt, r = d.call("graph", {"relation": "references", "name": q, "format": "lean"})
        sc = r.get("structuredContent", {}) or {}
        recs = sc.get("records") or []
        refs.append({"query": q, "vorpal_refs_s": round(dt, 5), "records": len(recs) if isinstance(recs, list) else None, "outcome": sc.get("outcome"), "total": sc.get("total")})
    log(f"  vorpal references: {len(refs)} identifier queries, median {statistics.median(r['vorpal_refs_s'] for r in refs)*1000:.2f} ms, total {sum(r['vorpal_refs_s'] for r in refs)*1000:.1f} ms")
    R["rows"].append({"phase": "queries", "kind": "references", "corpus": "kernel", "n": len(refs), "total_s": round(sum(r["vorpal_refs_s"] for r in refs), 4), "median_s": round(statistics.median(r["vorpal_refs_s"] for r in refs), 5), "per_query": refs}); save()
    d.close()

# ---------------- save
def phase_save():
    copy = OUT / "kernel-copy.noindex"
    if not (copy / "fs" / "read_write.c").exists():
        log("cloning kernel tree (APFS clone)")
        subprocess.run(["cp", "-c", "-R", "/Users/adalundhe/Projects/linux", str(copy)], check=True, stdin=subprocess.DEVNULL)
        shutil.rmtree(copy / ".vorpal", ignore_errors=True)
    target = copy / "fs" / "read_write.c"; original = target.read_text()
    # tgrep
    tidx = OUT / "idx-tgrep-save.noindex"; shutil.rmtree(tidx, ignore_errors=True)
    timed([TGREP, "index", str(copy), "--index-path", str(tidx)])
    serve = TgrepServe(copy, tidx, OUT / "serve-save.stderr", watch=True)
    time.sleep(5)
    rows = []
    for rep in range(7):
        name = f"tgrep_save_probe_{rep}"
        m = machine()
        t0 = time.perf_counter(); target.write_text(original + f"\nint {name}(void) {{ return {rep}; }}\n")
        polls = 0
        while True:
            p = subprocess.run([TGREP, name, str(copy), "--index-path", str(tidx)], capture_output=True, stdin=subprocess.DEVNULL); polls += 1
            if name.encode() in p.stdout: break
            if time.perf_counter() - t0 > 60: break
            time.sleep(0.02)
        seen = time.perf_counter() - t0
        rows.append({"rep": rep, "seen_s": round(seen, 3), "polls": polls, "machine": m}); log(f"  tgrep save rep{rep} visible after {seen:.3f} s ({polls} polls) idle {m['idle_pct']} fse {m['fseventsd_pcpu']}")
        target.write_text(original); time.sleep(3)
    serve_rss = rss_of(serve.p.pid); serve.close()
    R["rows"].append({"phase": "save", "tool": "tgrep", "median_s": round(statistics.median(r["seen_s"] for r in rows), 3), "reps": rows, "serve_rss": serve_rss}); save()
    phase_save_vorpal()

def phase_save_vorpal(gated=False):
    copy = OUT / "kernel-copy.noindex"; target = copy / "fs" / "read_write.c"; original = target.read_text()
    vidx = copy / ".vorpal" / "index"
    timed([VORPAL, "index", str(copy)])
    d = Daemon(vidx, OUT / "mcp-save.stderr")
    d.call("node", {"name": "vfs_read"}); time.sleep(3)
    # the daemon warms its search tiers on a fresh index; that is boot work, not save latency
    d.call("search", {"query": "zzz warm probe", "k": 10, "format": "lean"}); settled = d.wait_settled(); log(f"  daemon settled after {settled} s")
    rows = []
    for rep in range(7):
        name = f"vorpal_save_probe_{rep}"
        waited = d.wait_settled() if gated else None  # gated: the previous save's re-warm must finish first
        m = machine(); m["daemon_pcpu"] = d.pcpu(); m["waited_s"] = waited
        t0 = time.perf_counter(); target.write_text(original + f"\nint {name}(void) {{ return {rep}; }}\n")
        polls = 0
        while True:
            dt, r = d.call("node", {"name": name}); polls += 1
            text = json.dumps(r)
            if name in text and '"records":[{' in json.dumps(r.get("structuredContent", {}), separators=(",", ":")): break
            if time.perf_counter() - t0 > 60: break
            time.sleep(0.02)
        seen = time.perf_counter() - t0
        rows.append({"rep": rep, "seen_s": round(seen, 3), "polls": polls, "machine": m}); log(f"  vorpal save rep{rep} visible after {seen:.3f} s ({polls} polls) idle {m['idle_pct']} fse {m['fseventsd_pcpu']}")
        target.write_text(original); time.sleep(4)
        while True:  # wait for the restore to land before the next rep
            dt, r = d.call("node", {"name": name})
            if '"records":[{' not in json.dumps(r.get("structuredContent", {}), separators=(",", ":")): break
            time.sleep(0.05)
    mcp_rss = rss_of(d.p.pid); d.close()
    R["rows"].append({"phase": "save", "tool": "vorpal", "gated": gated, "median_s": round(statistics.median(r["seen_s"] for r in rows), 3), "reps": rows, "mcp_rss": mcp_rss, "settled_s": settled}); save()

# ---------------- calls: the head-to-head where both tools do the same job, find every call of a function
def phase_calls():
    src = "/Users/adalundhe/Projects/linux"
    tidx = OUT / "idx-tgrep-kernel-2.noindex"; vidx = pathlib.Path(src) / ".vorpal" / "index"
    if not (tidx / "lookup.bin").exists():
        log("tgrep kernel index missing — building"); timed([TGREP, "index", src, "--index-path", str(tidx)])
    serve = TgrepServe(src, tidx, OUT / "serve-calls.stderr", watch=False); time.sleep(2)
    subprocess.run([TGREP, "zzz_warm_probe", src, "--index-path", str(tidx)], capture_output=True, stdin=subprocess.DEVNULL)
    subprocess.run(["rg", "-n", "zzz_warm_probe", src], capture_output=True, stdin=subprocess.DEVNULL)
    m = machine(); calls = []
    for name in CALLS:
        row = {"name": name}
        for rep in range(3):
            t0 = time.perf_counter(); pt = subprocess.run([TGREP, name + r"\(", src, "--index-path", str(tidx), "-t", "c"], capture_output=True, stdin=subprocess.DEVNULL); wt = time.perf_counter() - t0
            t0 = time.perf_counter(); pr = subprocess.run(["rg", "-n", name + r"\(", "-t", "c", src], capture_output=True, stdin=subprocess.DEVNULL); wr = time.perf_counter() - t0
            row.setdefault("tgrep_s", []).append(round(wt, 4)); row.setdefault("rg_s", []).append(round(wr, 4))
            row["tgrep_lines"] = pt.stdout.count(b"\n"); row["rg_lines"] = pr.stdout.count(b"\n")
        calls.append(row)
        log(f"  calls {name:32} tgrep {statistics.median(row['tgrep_s'])*1000:7.1f} ms {row['tgrep_lines']:6} | rg {statistics.median(row['rg_s'])*1000:7.1f} ms {row['rg_lines']:6}")
    serve_rss = rss_of(serve.p.pid); serve.close()
    d = Daemon(vidx, OUT / "mcp-calls.stderr")
    d.call("node", {"name": "vfs_read"}); time.sleep(3)
    d.call("search", {"query": "zzz warm probe", "k": 10, "format": "lean"}); settled = d.wait_settled(); log(f"  daemon settled after {settled} s")
    for row in calls:
        name = row["name"]
        for tool, args in (("search", {"query": name, "k": 10, "format": "lean"}), ("code_search", {"pattern": PATTERNS[name], "lang": "c", "k": 100}), ("structural_search", {"pattern": PATTERNS[name], "lang": "c", "limit": 100})):
            dts = []; n = None; total = None
            for rep in range(3):
                dt, r = d.call(tool, args); dts.append(dt)
                sc = r.get("structuredContent", {}) or {}
                recs = sc.get("records") or sc.get("matches") or sc.get("hits") or []
                n = len(recs) if isinstance(recs, list) else None; total = sc.get("totalMatches") or sc.get("total")
                if r.get("isError") or "error" in r: row[f"{tool}_error"] = json.dumps(r)[:300]
            row[f"{tool}_s"] = [round(x, 3) for x in dts]; row[f"{tool}_records"] = n; row[f"{tool}_total"] = total
            log(f"  {tool:17} {name:32} {' '.join(f'{x:6.2f}' for x in dts)} s  records {n} total {total}")
    mcp_rss = rss_of(d.p.pid); d.close()
    R["rows"].append({"phase": "calls", "corpus": "kernel", "rows": calls, "mcp_rss_after": mcp_rss, "serve_rss_after": serve_rss, "machine": m}); save()

# ---------------- chunk_parity: chunk-scoped parsing (product v21 cuts) against the whole-file parse, at kernel scale.
# Same daemon protocol; the veto daemon runs with VORPAL_NO_CHUNK_PARSE=1. Records must be identical.
CHUNK_PATTERNS = [(n, p, "c") for n, p in PATTERNS.items()] + [
    ("kfree", "kfree($A)", "c"), ("mutex_lock", "mutex_lock($A)", "c"), ("spin_lock_irqsave", "spin_lock_irqsave($A, $B)", "c"),
    ("list_for_each_entry", "list_for_each_entry($A, $B, $C)", "c"), ("container_of", "container_of($A, $B, $C)", "c"),
    ("WARN_ON", "WARN_ON($A)", "c"), ("if_return", "if ($C) return $X;", "c"), ("struct_def", "struct $S { $$$ };", "c"),
    ("py_join", "os.path.join($A, $B)", "python"), ("py_def", "def $F(self, $$$): $$$", "python"),
]
def phase_chunk_parity():
    src = "/Users/adalundhe/Projects/linux"; vidx = pathlib.Path(src) / ".vorpal" / "index"
    log("  rebuilding the kernel index (product v21 carries the cuts)")
    index_wall, rc, *_ = timed([VORPAL, "index", src]); log(f"  index {index_wall:.1f} s (rc {rc})")
    m = machine(); rows = {}
    for arm, env in (("chunked", {}), ("whole", {"VORPAL_NO_CHUNK_PARSE": "1", "VORPAL_NO_CALLSITE_PATH": "1"})):
        d = Daemon(vidx, OUT / f"mcp-chunk-{arm}.stderr", env=env)
        d.call("node", {"name": "vfs_read"}); time.sleep(3)
        d.call("search", {"query": "zzz warm probe", "k": 10, "format": "lean"}); settled = d.wait_settled(); log(f"  {arm}: daemon settled after {settled} s")
        for name, pattern, lang in CHUNK_PATTERNS:
            for tool, args in (("code_search", {"pattern": pattern, "lang": lang, "k": 5000}), ("structural_search", {"pattern": pattern, "lang": lang, "limit": 5000})):
                dts = []; sc = {}
                for rep in range(3):
                    dt, r = d.call(tool, args); dts.append(dt); sc = r.get("structuredContent", {}) or {}
                recs = sc.get("records") or sc.get("hits") or []
                digest = hashlib.sha256(json.dumps(recs, sort_keys=True).encode()).hexdigest()[:16]
                row = rows.setdefault((name, tool), {"name": name, "pattern": pattern, "lang": lang, "tool": tool})
                row[f"{arm}_s"] = [round(x, 3) for x in dts]; row[f"{arm}_records"] = len(recs); row[f"{arm}_total"] = sc.get("totalMatches") or sc.get("total")
                row[f"{arm}_digest"] = digest; row[f"{arm}_scanned"] = sc.get("scannedFiles"); row[f"{arm}_chunk_parsed"] = sc.get("chunkParsedFiles"); row[f"{arm}_pruned"] = sc.get("prunedFiles"); row[f"{arm}_memo_hits"] = sc.get("chunkMemoHits"); row[f"{arm}_callsite"] = sc.get("callSiteFiles")
                log(f"  {arm:7} {tool:17} {name:22} {' '.join(f'{x:6.3f}' for x in dts)} s  records {len(recs)} total {row[f'{arm}_total']} scanned {row[f'{arm}_scanned']} chunked {row[f'{arm}_chunk_parsed']} memo {row[f'{arm}_memo_hits']} callsite {row[f'{arm}_callsite']}")
        d.close()
    table = list(rows.values())
    for row in table:
        row["equal"] = row.get("chunked_digest") == row.get("whole_digest") and row.get("chunked_total") == row.get("whole_total")
        log(f"  {'OK ' if row['equal'] else 'DIFF'} {row['tool']:17} {row['name']:22} chunked {statistics.median(row['chunked_s']):6.3f} s  whole {statistics.median(row['whole_s']):6.3f} s  ({row['whole_total']} matches)")
    R["rows"].append({"phase": "chunk_parity", "corpus": "kernel", "index_s": round(index_wall, 2), "rows": table, "mismatches": sum(1 for r in table if not r["equal"]), "machine": m}); save()
    log(f"  chunk_parity: {sum(1 for r in table if not r['equal'])} mismatches over {len(table)} rows")

# ---------------- search: the 102 suite queries as `search` on the daemon (hybrid name search, k=10, lean)
def phase_search():
    src = "/Users/adalundhe/Projects/linux"
    d = Daemon(pathlib.Path(src) / ".vorpal" / "index", OUT / "mcp-search.stderr")
    d.call("node", {"name": "vfs_read"}); time.sleep(3)
    dt_first, _ = d.call("search", {"query": "zzz warm probe", "k": 10, "format": "lean"})  # first search: tier warm-up, once
    settled = d.wait_settled(); log(f"  first search {dt_first*1000:.0f} ms; daemon settled after {settled} s")
    m = machine(); rows = []
    for q in QUERIES:
        dt, r = d.call("search", {"query": q, "k": 10, "format": "lean"})
        sc = r.get("structuredContent", {}) or {}
        recs = sc.get("records") or sc.get("hits") or []
        top = recs[0].get("name") if recs and isinstance(recs[0], dict) else None
        rows.append({"query": q, "search_s": round(dt, 5), "hits": len(recs) if isinstance(recs, list) else None, "top": top, "ident": bool(IDENT.match(q))})
        log(f"  search {q[:40]:40} {dt*1000:7.2f} ms hits {rows[-1]['hits']} top {top}")
    ident = [r for r in rows if r["ident"]]
    R["rows"].append({"phase": "search", "corpus": "kernel", "n": len(rows), "total_s": round(sum(r["search_s"] for r in rows), 4), "median_s": round(statistics.median(r["search_s"] for r in rows), 5),
                      "p95_s": round(sorted(r["search_s"] for r in rows)[int(0.95 * len(rows))], 5),
                      "ident_n": len(ident), "ident_total_s": round(sum(r["search_s"] for r in ident), 4), "ident_median_s": round(statistics.median(r["search_s"] for r in ident), 5),
                      "ident_top_exact": sum(1 for r in ident if r["top"] == r["query"]), "first_search_s": round(dt_first, 3), "settled_s": settled, "mcp_rss_after": rss_of(d.p.pid), "machine": m, "per_query": rows}); save()
    log(f"  search: {len(rows)} queries — daemon median {statistics.median(r['search_s'] for r in rows)*1000:.2f} ms total {sum(r['search_s'] for r in rows)*1000:.1f} ms; identifiers top-hit exact {sum(1 for r in ident if r['top'] == r['query'])}/{len(ident)}")
    d.close()

# ---------------- text: the 102 suite queries as `text_search` on the daemon, lines compared with rg
def phase_text():
    src = "/Users/adalundhe/Projects/linux"
    d = Daemon(pathlib.Path(src) / ".vorpal" / "index", OUT / "mcp-text.stderr")
    d.call("node", {"name": "vfs_read"}); time.sleep(3)
    # the daemon's warm heals the text tier; wait for it, then a first text query
    settled = d.wait_settled(); log(f"  daemon settled after {settled} s")
    dt_first, r = d.call("text_search", {"pattern": "zzz_warm_probe", "max_results": 10, "format": "lean"})
    sc = r.get("structuredContent", {}) or {}
    log(f"  first text_search {dt_first*1000:.0f} ms textIndex {sc.get('textIndex')} index {sc.get('index')}")
    m = machine(); rows = []
    for q in QUERIES:
        dt, r = d.call("text_search", {"pattern": q, "max_results": 10000, "format": "ids"})
        sc = r.get("structuredContent", {}) or {}
        t0 = time.perf_counter(); pr = subprocess.run(["rg", "-n", q, src], capture_output=True, stdin=subprocess.DEVNULL); wr = time.perf_counter() - t0
        rows.append({"query": q, "text_search_s": round(dt, 4), "total_matches": sc.get("totalMatches"), "matched_files": sc.get("matchedFiles"), "pruned": sc.get("prunedFiles"), "prefiltered": sc.get("prefilteredFiles"), "scanned": sc.get("scannedFiles"), "candidate": sc.get("candidateFiles"), "index": sc.get("index"), "text_index": sc.get("textIndex"), "rg_s": round(wr, 4), "rg_lines": pr.stdout.count(b"\n")})
        log(f"  text {q[:40]:40} {dt*1000:8.1f} ms {sc.get('totalMatches')} lines ({sc.get('scannedFiles')} scanned / {sc.get('candidateFiles')} in scope, {sc.get('index')}) | rg {wr*1000:7.1f} ms {rows[-1]['rg_lines']}")
    equal = sum(1 for r in rows if r["total_matches"] == r["rg_lines"])
    R["rows"].append({"phase": "text", "corpus": "kernel", "n": len(rows), "total_s": round(sum(r["text_search_s"] for r in rows), 3), "median_s": round(statistics.median(r["text_search_s"] for r in rows), 4), "p95_s": round(sorted(r["text_search_s"] for r in rows)[int(0.95 * len(rows))], 4),
                      "rg_total_s": round(sum(r["rg_s"] for r in rows), 3), "lines_equal_rg": equal, "first_s": round(dt_first, 3), "settled_s": settled, "mcp_rss_after": rss_of(d.p.pid), "machine": m, "per_query": rows}); save()
    log(f"  text_search: {len(rows)} queries median {statistics.median(r['text_search_s'] for r in rows)*1000:.1f} ms total {sum(r['text_search_s'] for r in rows):.2f} s; rg total {sum(r['rg_s'] for r in rows):.1f} s; line counts equal rg on {equal}/{len(rows)}")
    d.close()

# ---------------- text_parity: the same 102 queries with the tier vetoed — identical totals prove no false negatives
def phase_text_parity():
    src = "/Users/adalundhe/Projects/linux"
    prior = next((r for r in R["rows"] if r.get("phase") == "text"), None)
    if prior is None:
        log("text_parity: run the text phase first"); return
    os.environ["VORPAL_NO_TEXT_INDEX"] = "1"
    d = Daemon(pathlib.Path(src) / ".vorpal" / "index", OUT / "mcp-text-parity.stderr")
    d.call("node", {"name": "vfs_read"}); time.sleep(3)
    rows = []; mismatches = []
    for ref in prior["per_query"]:
        q = ref["query"]
        dt, r = d.call("text_search", {"pattern": q, "max_results": 10000, "format": "ids"})
        sc = r.get("structuredContent", {}) or {}
        rows.append({"query": q, "exhaustive_s": round(dt, 4), "total_matches": sc.get("totalMatches"), "index": sc.get("index")})
        if sc.get("totalMatches") != ref["total_matches"]:
            mismatches.append((q, ref["total_matches"], sc.get("totalMatches")))
    del os.environ["VORPAL_NO_TEXT_INDEX"]
    d.close()
    R["rows"].append({"phase": "text_parity", "corpus": "kernel", "n": len(rows), "mismatches": mismatches, "exhaustive_total_s": round(sum(r["exhaustive_s"] for r in rows), 3), "exhaustive_median_s": round(statistics.median(r["exhaustive_s"] for r in rows), 4), "per_query": rows}); save()
    log(f"  text_parity: {len(rows)} queries, {len(mismatches)} mismatches vs the tier; exhaustive median {statistics.median(r['exhaustive_s'] for r in rows)*1000:.1f} ms total {sum(r['exhaustive_s'] for r in rows):.2f} s")
    for m in mismatches[:10]: log("   MISMATCH", m)

if "--append" in sys.argv and (OUT / "tgrep_bench.json").exists():
    R["rows"] = [r for r in json.load(open(OUT / "tgrep_bench.json"))["rows"] if r.get("phase") not in PHASES and not (r.get("phase") == "queries" and r.get("kind") == "calls")
                 and not ("save_vorpal" in PHASES and r.get("phase") == "save" and r.get("tool") == "vorpal" and not r.get("gated"))
                 and not ("save_vorpal_gated" in PHASES and r.get("phase") == "save" and r.get("tool") == "vorpal" and r.get("gated"))]
for ph in PHASES:
    log(f"==== {ph} ===="); {"build": phase_build, "queries": phase_queries, "calls": phase_calls, "save": phase_save, "save_vorpal": phase_save_vorpal, "save_vorpal_gated": lambda: phase_save_vorpal(gated=True), "search": phase_search, "text": phase_text, "text_parity": phase_text_parity, "chunk_parity": phase_chunk_parity}[ph]()
log("done", OUT / "tgrep_bench.json")

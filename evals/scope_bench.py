#!/usr/bin/env python3
"""Filtered against unfiltered: the same questions to one warm `vorpal mcp` daemon per
corpus, repository-wide and then inside a scope, timed from the client side.

    python3 evals/scope_bench.py <vorpal-binary> <out-dir> [kernel,cpython,vorpal]

Each corpus is indexed in its default layout (`vorpal index <src>`, so the daemon has its
watched tree and scope entries resolve against it), the daemon is left alone until its
tier warm has settled, and every row then waits for the README's quiet gate. Per
(corpus, question, scope): wall time over N round trips (min and median), the rows the
answer holds, the compact bytes of its `structuredContent` (what Claude Code hands the
model), and for graph answers the rows the scope counted as outside. The search rows
also grade completeness: a scoped search asked for k = 8 records its hit count next to the
old `prefix` facet's count on the same directory. `reachable` rows compare the default ring
with the whole closure. VORPAL_SCOPE_REPO points the "vorpal" corpus at a scratch copy of
this repository (its own index is served by other daemons). Recorded runs:
docs/wip/BENCHMARKS.md.
"""
import json, os, pathlib, statistics, subprocess, sys, time

BIN = os.path.abspath(sys.argv[1]); OUT = pathlib.Path(sys.argv[2]); OUT.mkdir(parents=True, exist_ok=True)
ONLY = sys.argv[3].split(",") if len(sys.argv) > 3 else ["kernel", "cpython", "vorpal"]
HOME = os.path.expanduser("~")
CORPORA = {
    "kernel": f"{HOME}/Projects/linux",
    "cpython": f"{HOME}/Projects/cpython",
    "vorpal": os.environ.get("VORPAL_SCOPE_REPO", f"{HOME}/Projects/vorpal"),
}
# (label, scope entries): a subsystem, a leaf directory, and a two-directory scope.
SCOPES = {
    "kernel": [("fs", ["fs"]), ("fs/ext4", ["fs/ext4"]), ("fs+mm", ["fs", "mm"])],
    "cpython": [("Objects", ["Objects"]), ("Lib/asyncio", ["Lib/asyncio"]), ("Python+Objects", ["Python", "Objects"])],
    "vorpal": [("crates/index", ["crates/index"]), ("crates/mcp/src", ["crates/mcp/src"]), ("crates/index+crates/kg", ["crates/index", "crates/kg"])],
}
# The tools an agent reaches for while working in one area: a graph answer with many rows,
# a name search, a descriptive search, a text search, a structural search, and a ring.
QUESTIONS = {
    "kernel": {"graph": ("kmalloc", {"all": True}), "search_name": "vfs_read", "search_desc": "read file into user buffer",
               "text": r"kmalloc\(", "code": ("kmalloc($A, $B)", "c"), "reach": "vfs_read"},
    "cpython": {"graph": ("PyErr_SetString", {}), "search_name": "PyDict_GetItem", "search_desc": "parse argument tuple",
                "text": r"PyErr_SetString\(", "code": ("PyErr_SetString($A, $B)", "c"), "reach": "PyDict_GetItem"},
    "vorpal": {"graph": ("phase_stamp", {}), "search_name": "tool_result", "search_desc": "resolve import path",
               "text": r"Path::new\(", "code": ("Path::new($A)", "rust"), "reach": "tool_result"},
}
REPS = int(os.environ.get("VORPAL_SCOPE_REPS", "20"))
R = {"binary": BIN, "rows": []}

# The README driver's quiet gate, without its phases.
_src = open(str(pathlib.Path(__file__).resolve().with_name("readme_bench.py"))).read().split("# ---------------- index17")[0]
_src = _src.replace('PHASES = sys.argv[sys.argv.index("--phases") + 1].split(",") if "--phases" in sys.argv else ["index17", "edit", "tiers", "giant", "scan"]', 'PHASES = []')
_g = {}; exec(compile(_src, "rb-gate", "exec"), _g)
wait_quiet, log, run = _g["wait_quiet"], _g["log"], _g["run"]


def save():
    (OUT / "scope_bench.json").write_text(json.dumps(R, indent=1))


class Daemon:
    def __init__(self, index, stderr_path):
        self.p = subprocess.Popen([BIN, "mcp", "--index", str(index)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=open(stderr_path, "w"), text=True, bufsize=1)
        self.i = 0
        self.rpc("initialize", {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "scope-bench", "version": "0"}})
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def send(self, o):
        self.p.stdin.write(json.dumps(o) + "\n"); self.p.stdin.flush()

    def rpc(self, method, params):
        self.i += 1
        self.send({"jsonrpc": "2.0", "id": self.i, "method": method, "params": params})
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise SystemExit("daemon closed")
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue
            if msg.get("id") == self.i:
                return msg

    def call(self, tool, args):
        t0 = time.perf_counter()
        msg = self.rpc("tools/call", {"name": tool, "arguments": args})
        ms = (time.perf_counter() - t0) * 1000
        result = msg.get("result", {})
        return ms, result.get("structuredContent", {}) or {}, bool(result.get("isError")) or "error" in msg

    def timed(self, tool, args, reps=REPS):
        walls = []; sc = {}
        for _ in range(reps):
            ms, sc, err = self.call(tool, args)
            if err:
                log("  error from", tool, args, json.dumps(sc)[:300])
                return None, sc
            walls.append(ms)
        return {"min_ms": round(min(walls), 3), "median_ms": round(statistics.median(walls), 3)}, sc

    def pcpu(self):
        out = subprocess.run(["ps", "-o", "pcpu=", "-p", str(self.p.pid)], capture_output=True, text=True, stdin=subprocess.DEVNULL).stdout.strip()
        return float(out) if out else 0.0

    def wait_settled(self, max_s=1800):
        """Block until the daemon is idle: no child process and its own CPU under 20 % on
        two consecutive 1 s samples (the tier warm runs in-process after a generation)."""
        t0 = time.time(); streak = 0
        while time.time() - t0 < max_s:
            kids = subprocess.run(["pgrep", "-P", str(self.p.pid)], capture_output=True, text=True, stdin=subprocess.DEVNULL).stdout.split()
            streak = streak + 1 if (not kids and self.pcpu() < 20.0) else 0
            if streak >= 2:
                return round(time.time() - t0, 1)
            time.sleep(1)
        return None

    def close(self):
        try:
            self.p.stdin.close()
        except Exception:
            pass
        self.p.kill(); self.p.wait()


def records(sc):
    recs = sc.get("records") or sc.get("hits") or []
    return len(recs) if isinstance(recs, list) else None


def bench(corpus):
    src = CORPORA[corpus]
    index = pathlib.Path(src) / ".vorpal" / "index"
    log(f"== {corpus}: index {src}")
    wall, p = run([BIN, "index", src], timeout=3600)
    assert p.returncode == 0, p.stderr[-500:]
    log(f"   index brought current in {wall:.2f} s")
    d = Daemon(index, OUT / f"scope-{corpus}.stderr")
    q = QUESTIONS[corpus]
    d.call("node", {"name": q["reach"]})  # boot revalidation
    settled = d.wait_settled(); log(f"   daemon settled after {settled} s")
    # First touches of every tool used below, then settle again (the text tier heals lazily).
    d.call("search", {"query": q["search_name"], "k": 8})
    d.call("text_search", {"pattern": q["text"], "max_results": 200})
    d.call("code_search", {"pattern": q["code"][0], "lang": q["code"][1], "k": 10})
    d.call("graph", {"relation": "callers", "name": q["graph"][0], "limit": 5, **q["graph"][1]})
    d.call("reachable", {"name": q["reach"], "direction": "out"})
    settled2 = d.wait_settled(); log(f"   settled again after {settled2} s")

    def row(question, scope_label, scope, timing, sc, quiet, extra=None):
        r = {"corpus": corpus, "question": question, "scope": scope_label, "within": scope, "timing": timing,
             "records": records(sc), "total": sc.get("total"), "outside": sc.get("outsideScope"),
             "bytes": len(json.dumps(sc, separators=(",", ":"))), "quiet": quiet}
        if extra:
            r.update(extra)
        R["rows"].append(r); log({k: v for k, v in r.items() if k != "quiet"}); save()

    scopes = [("repository", None)] + SCOPES[corpus]
    for label, scope in scopes:
        within = {} if scope is None else {"within": scope}
        quiet = wait_quiet()
        # graph callers, one page of up to 100 rows (call sites are read for every row kept);
        # `bytes` is the compact structuredContent, what Claude Code hands the model
        t, sc = d.timed("graph", {"relation": "callers", "name": q["graph"][0], "limit": 100, **q["graph"][1], **within})
        row(f"graph callers {q['graph'][0]}", label, scope, t, sc, quiet)
        for key, query in (("name", q["search_name"]), ("descriptive", q["search_desc"])):
            t, sc = d.timed("search", {"query": query, "k": 8, **within})
            extra = {"hits": records(sc)}
            if scope is not None:
                # completeness: the pre-0.10 prefix facet on the same directory (it takes one)
                _, old, err = d.call("search", {"query": query, "k": 8, "prefix": f"{src}/{scope[0]}/"})
                extra["hits_prefix_facet"] = None if err else records(old)
            row(f"search {key} '{query}'", label, scope, t, sc, quiet, extra)
        t, sc = d.timed("text_search", {"pattern": q["text"], "max_results": 200, **within})
        row(f"text_search {q['text']}", label, scope, t, sc, quiet, {"matches": sc.get("totalMatches"), "files": sc.get("matchedFiles")})
        pattern, lang = q["code"]
        t, sc = d.timed("code_search", {"pattern": pattern, "lang": lang, "k": 10, **within}, reps=max(3, REPS // 4))
        row(f"code_search {pattern}", label, scope, t, sc, quiet, {"scanned": sc.get("scannedFiles"), "call_site_files": sc.get("callSiteFiles")})
    # rings: the default ring against the whole closure, repository-wide
    quiet = wait_quiet()
    for label, args in (("ring (default)", {}), ("closure (max_depth 0)", {"max_depth": 0})):
        t, sc = d.timed("reachable", {"name": q["reach"], "direction": "out", **args})
        row(f"reachable {q['reach']} out", label, None, t, sc, quiet, {"frontier": sc.get("frontier"), "max_depth": sc.get("maxDepth")})
    d.close()


log("binary", BIN, subprocess.run([BIN, "--version"], capture_output=True, text=True, stdin=subprocess.DEVNULL).stdout.strip())
for corpus in ONLY:
    bench(corpus)
save(); log("SCOPE-BENCH-DONE")

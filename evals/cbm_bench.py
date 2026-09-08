"""codebase-memory-mcp on the README corpora: cold index (wall, peak RSS over the process tree,
nodes/edges, on-disk), no-change re-index, graded search_graph (BM25 `query` mode) against the
searcheval label files with the same NDCG@10 / MRR / recall@5 math, and one-shot latencies."""
import json, math, os, re, subprocess, sys, threading, time, statistics, pathlib
CBM = os.path.abspath(sys.argv[1]); OUT = pathlib.Path(sys.argv[2]); OUT.mkdir(parents=True, exist_ok=True)
CORPORA = {"kernel": "/Users/adalundhe/Projects/linux", "cpython": "/Users/adalundhe/Projects/cpython", "vorpal": "/Users/adalundhe/Projects/vorpal"}
ONLY = sys.argv[3].split(",") if len(sys.argv) > 3 and not sys.argv[3].startswith("--") else list(CORPORA)
CACHE = OUT / "cbm-cache"; ENV = {**os.environ, "CBM_CACHE_DIR": str(CACHE)}
R = {"rows": []}
def save(): (OUT / ("cbm_bench_search.json" if SEARCH_ONLY else "cbm_bench.json")).write_text(json.dumps(R, indent=1))
def log(*a): print(*a, flush=True)

def tree_rss(root_pid):
    out = subprocess.run(["ps", "-axo", "pid=,ppid=,rss="], capture_output=True, text=True).stdout
    kids = {}; rss = {}
    for line in out.split("\n"):
        p = line.split()
        if len(p) != 3: continue
        pid, ppid, r = int(p[0]), int(p[1]), int(p[2]); kids.setdefault(ppid, []).append(pid); rss[pid] = r
    total, stack = 0, [root_pid]
    while stack:
        pid = stack.pop(); total += rss.get(pid, 0); stack.extend(kids.get(pid, []))
    return total * 1024

def run_cli(tool, args, sample=False):
    cmd = [CBM, "cli", "--json", tool, json.dumps(args)]
    t0 = time.perf_counter(); p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=ENV)
    peak = [0]
    if sample:
        def pump():
            while p.poll() is None:
                peak[0] = max(peak[0], tree_rss(p.pid)); time.sleep(0.05)
        threading.Thread(target=pump, daemon=True).start()
    so, se = p.communicate(); wall = time.perf_counter() - t0
    return wall, p.returncode, so, se, peak[0]

def du(path):
    try: return int(subprocess.run(["du", "-sk", str(path)], capture_output=True, text=True).stdout.split()[0]) * 1024
    except Exception: return None

def parse_counts(text):
    n = re.search(r'"?(?:nodes|node_count|nodes_created|total_nodes)"?\D{0,4}(\d+)', text)
    e = re.search(r'"?(?:edges|edge_count|edges_created|total_edges)"?\D{0,4}(\d+)', text)
    return (int(n.group(1)) if n else None, int(e.group(1)) if e else None)

def gain(g): return (1 << g) - 1
def grade(hits, relevant, root):
    consumed = [False] * len(relevant); matches = []
    for rank, (name, path) in enumerate(hits):
        for i, r in enumerate(relevant):
            if consumed[i] or r["name"] != name: continue
            lp = r.get("path")
            if lp is None: ok = True
            elif lp.startswith("/"): ok = path.startswith(root + "/") and path[len(root) + 1:] == lp[1:]
            else: ok = path.endswith(lp)
            if ok: consumed[i] = True; matches.append((rank, r["grade"])); break
    dcg = sum(gain(g) / math.log2(rank + 2) for rank, g in matches if rank < 10)
    ideal = sorted((r["grade"] for r in relevant), reverse=True)[:10]
    idcg = sum(gain(g) / math.log2(i + 2) for i, g in enumerate(ideal))
    ndcg = dcg / idcg if dcg else 0.0
    mrr = max([1.0 / (rank + 1) for rank, g in matches if g >= 2], default=0.0)
    rel2 = sum(1 for r in relevant if r["grade"] >= 2); hit2 = sum(1 for rank, g in matches if rank < 5 and g >= 2)
    return ndcg, mrr, (hit2 / rel2 if rel2 else 0.0)

def hits_of(response_text):
    """cbm's search_graph answer is a text table inside content[0].text:
    `  <qualified.name> <Label> <path> <lines> <rank>` per result, after a `results:` header.
    The name is the last dotted segment; the path is tree-relative."""
    try:
        doc = json.loads(response_text[response_text.find("{"):])
        text = "".join(c.get("text", "") for c in doc.get("content", []) if isinstance(c, dict))
    except Exception:
        text = response_text
    found = []; started = False
    for line in text.split("\n"):
        if line.startswith("results:"): started = True; continue
        if not started or not line.startswith("  "): continue
        parts = line.split()
        if len(parts) < 3: continue
        found.append((parts[0].rsplit(".", 1)[-1], parts[2]))
    return found

SEARCH_ONLY = "--search-only" in sys.argv
for label in ONLY:
    src = CORPORA[label]
    if not SEARCH_ONLY:
        log(f"== {label}: cold index (full)")
        subprocess.run([CBM, "cli", "--json", "delete_project", json.dumps({"project": os.path.basename(src)})], capture_output=True, text=True, env=ENV)
        wall, rc, so, se, peak = run_cli("index_repository", {"repo_path": src, "mode": "full"}, sample=True)
        nodes, edges = parse_counts(so + se)
        (OUT / f"index-{label}.json").write_text(so[-20000:] + "\n---stderr---\n" + se[-4000:])
        row = {"phase": "index", "corpus": label, "wall_s": round(wall, 2), "rc": rc, "peak_rss": peak, "nodes": nodes, "edges": edges, "disk": du(CACHE)}
        log(f"  index {wall:7.1f} s rc {rc} peak {peak/2**30:.1f} GB nodes {nodes} edges {edges} disk {(row['disk'] or 0)/2**20:.0f} MB")
        wall2, rc2, so2, se2, _ = run_cli("index_repository", {"repo_path": src, "mode": "full"})
        row["reindex_s"] = round(wall2, 2); log(f"  no-change re-index {wall2:.2f} s rc {rc2}")
        R["rows"].append(row); save()
    else:
        log(f"== {label}: search only")
    # graded search
    labels = json.load(open(f"/Users/adalundhe/Projects/vorpal/xtask/labels/{label}.json"))
    project = os.path.basename(src)
    per = []; lat = []
    for q in labels["queries"]:
        w, rc, so, se, _ = run_cli("search_graph", {"query": q["query"], "project": project, "limit": 25})
        if len(per) == 0: (OUT / f"search-sample-{label}.json").write_text(so[:6000])
        hits = hits_of(so)[:25]
        nd, mr, rc5 = grade(hits, q["relevant"], src)
        per.append({"class": q["class"], "query": q["query"], "ndcg": nd, "mrr": mr, "recall": rc5, "hits": len(hits), "wall_s": round(w, 3)}); lat.append(w)
    classes = {}
    for r in per: classes.setdefault(r["class"], []).append(r)
    summary = {c: (len(v), statistics.mean(x["ndcg"] for x in v), statistics.mean(x["mrr"] for x in v), statistics.mean(x["recall"] for x in v)) for c, v in classes.items()}
    summary["all"] = (len(per), statistics.mean(x["ndcg"] for x in per), statistics.mean(x["mrr"] for x in per), statistics.mean(x["recall"] for x in per))
    for c, (n, nd, mr, rc5) in summary.items(): log(f"  search {c:14} n={n:2} ndcg {nd:.3f} mrr {mr:.3f} recall@5 {rc5:.3f}")
    log(f"  search one-shot latency median {statistics.median(lat):.2f} s  (max {max(lat):.2f})")
    R["rows"].append({"phase": "search", "corpus": label, "summary": summary, "median_s": statistics.median(lat), "per_query": per}); save()
    if label == "kernel":
        # one-shot callers of a symbol, 3 reps (vorpal's `graph callers vfs_read` row)
        walls = []
        for rep in range(3):
            w, rc, so, se, _ = run_cli("trace_path", {"function_name": "vfs_read", "project": project, "direction": "inbound", "depth": 1, "limit": 100}); walls.append(w)
            if rep == 0: (OUT / "trace-sample-kernel.json").write_text(so[:4000])
        log(f"  trace_path callers vfs_read one-shot {' '.join(f'{w:.2f}' for w in walls)} s")
        R["rows"].append({"phase": "callers", "corpus": label, "walls": walls}); save()
log("CBM-BENCH DONE")

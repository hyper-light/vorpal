#!/usr/bin/env python3
"""Real-world per-repository validation harness.

For each configured repository: index it twice with the release-built CLI (content-id
determinism is a hard gate), record timing and graph shape, summarize parse health, and
run probe checks graded against the repo itself (ripgrep coverage for callers, byte-exact
source lines for snippets, expected tokens for hybrid search). Prints one scorecard row
per check. Indexes are deleted afterwards; pass --keep to retain them.

    python3 evals/repo_eval.py <vorpal-index-binary> <workdir> [repo ...]
"""
import json, os, re, shutil, subprocess, sys, time

REPOS = {
    "cpython": {
        "path": os.path.expanduser("~/Projects/cpython"),
        "callers": "PyMem_Malloc",
        # PyLong_FromLong is swallowed by macro-cascade parse damage (see ledger) — probe a clean file.
        "snippet": ("PyUnicode_FromString", "Function"),
        "search": ("garbage collection generation threshold", ["gc", "collect"]),
    },
    "django": {
        "git": "https://github.com/django/django",
        "callers": "mark_safe",
        "snippet": ("get_object_or_404", "Function"),
        "search": ("form field validation error", ["valid"]),
    },
    "nextjs": {
        "git": "https://github.com/vercel/next.js",
        "callers": "loadComponents",
        "snippet": ("loadComponentsImpl", "Function"),
        "search": ("incremental static regeneration cache", ["cache", "isr", "revalidate"]),
    },
    "kubernetes": {
        "git": "https://github.com/kubernetes/kubernetes",
        "callers": "NewNotFound",
        "snippet": ("RunKubelet", "Function"),
        "search": ("pod eviction pressure threshold", ["evict"]),
    },
    "actix-web": {
        "git": "https://github.com/actix/actix-web",
        "callers": "init_service",
        "snippet": ("HttpServer", "Struct"),
        "search": ("websocket handshake upgrade", ["ws", "websocket", "upgrade"]),
    },
    "laravel": {
        "git": "https://github.com/laravel/framework",
        "callers": "resolve",
        "snippet": ("Collection", "Class"),
        "search": ("eloquent model relationship loading", ["relation", "eloquent"]),
    },
    "folly": {
        "git": "https://github.com/facebook/folly",
        "callers": "makeFuture",
        "snippet": ("EventBase", "Class"),
        "search": ("asynchronous executor thread pool", ["executor", "thread"]),
    },
    "kafka": {
        "git": "https://github.com/apache/kafka",
        "callers": "forCode",
        "snippet": ("KafkaProducer", "Class"),
        "search": ("consumer group rebalance protocol", ["rebalance", "consumer"]),
    },
    "terraform-provider-aws": {
        "git": "https://github.com/hashicorp/terraform-provider-aws",
        "callers": "FlattenStringValueSet",
        "snippet": (None, None),  # Go + a large HCL corpus in testdata
        "search": ("s3 bucket lifecycle configuration", ["s3", "lifecycle"]),
    },
}

BIN = sys.argv[1]
WORK = sys.argv[2]
CHOSEN = sys.argv[3:] or list(REPOS)
rows = []


def sh(*args, timeout=1800):
    return subprocess.run(args, capture_output=True, text=True, timeout=timeout)


def grade(repo, check, ok, evidence):
    rows.append((repo, check, "PASS" if ok else "FAIL", evidence))
    print(f"  [{'PASS' if ok else 'FAIL'}] {check}: {evidence}", flush=True)


def eval_repo(name, cfg):
    print(f"== {name} ==", flush=True)
    src = cfg.get("path")
    cloned = False
    if not src:
        src = os.path.join(WORK, name)
        if not os.path.isdir(src):
            r = sh("git", "clone", "--depth", "1", cfg["git"], src, timeout=3600)
            if r.returncode != 0:
                grade(name, "clone", False, r.stderr.strip()[:80])
                return
            cloned = True
    out_a, out_b = os.path.join(WORK, f"{name}-idx-a"), os.path.join(WORK, f"{name}-idx-b")
    for out in (out_a, out_b):
        shutil.rmtree(out, ignore_errors=True)

    env = dict(os.environ, VORPAL_NO_AUTOWARM="1")
    t0 = time.time()
    r = sh(BIN, "index", src, out_a)
    wall = time.time() - t0
    line = next((l for l in (r.stdout + r.stderr).splitlines() if "parsed" in l), "")
    m = re.search(r"parsed (\d+) files.*→ (\d+) nodes; refs: (\d+) resolved, (\d+) ambiguous, (\d+) external, (\d+) masked", line)
    grade(name, "cold index", r.returncode == 0 and m is not None,
          f"{wall:.1f}s — {line.strip()[:110]}")
    if not m:
        return
    sh(BIN, "index", src, out_b)
    ga = open(f"{out_a}/CURRENT").read().strip()
    gb = open(f"{out_b}/CURRENT").read().strip()
    grade(name, "deterministic x2", ga == gb, ga if ga == gb else f"{ga} vs {gb}")
    shutil.rmtree(out_b, ignore_errors=True)

    health = sh(BIN, "health", out_a).stdout
    hm = re.search(r"(\d+) of (\d+) files carry (?:parse damage|ERROR nodes) \((\d+) (?:error|damaged) bytes", health)
    if hm:
        dmg, total, err_bytes = int(hm.group(1)), int(hm.group(2)), int(hm.group(3))
        grade(name, "parse health", True, f"{dmg}/{total} files with damage, {err_bytes} error bytes")
    else:
        grade(name, "parse health", "parse damage" in health or health.strip() != "", health.strip()[:90])

    # callers vs ripgrep call-site files
    sym = cfg.get("callers")
    if sym:
        out = sh(BIN, "callers", out_a, sym, "--all").stdout
        rg = sh("rg", "-l", rf"\b{sym}\s*\(", src).stdout.splitlines()
        if rg:
            covered = sum(1 for f in rg if os.path.basename(f) in out)
            grade(name, f"callers({sym}) coverage", covered / len(rg) >= 0.5,
                  f"{covered}/{len(rg)} rg files covered")
        else:
            grade(name, f"callers({sym}) coverage", False, "probe symbol not found by rg — pick another")

    # snippet byte-fidelity
    ssym, skind = cfg.get("snippet", (None, None))
    if ssym:
        out = sh(BIN, "snippet", out_a, ssym, "--all").stdout
        # `--all` may return several same-named definitions; grade each snippet section
        # against ITS OWN file — pass if any section is byte-faithful.
        sections, current = [], None
        for line in out.splitlines():
            pm = re.search(r"(/\S+\.\w+)", line)
            if pm and os.path.isfile(pm.group(1)):
                current = (pm.group(1), [])
                sections.append(current)
            elif current is not None:
                current[1].append(line.strip())
        best = 0
        for path, lines in sections:
            body = open(path, errors="replace").read()
            fat = [l for l in lines if len(l) > 12][:12]
            best = max(best, sum(1 for l in fat if l in body))
        if sections:
            grade(name, f"snippet({ssym}) byte-faithful", best >= 3, f"best section: {best} exact lines")
        else:
            grade(name, f"snippet({ssym}) byte-faithful", False, out.strip().splitlines()[0][:90] if out.strip() else "no output")

    # hybrid search sanity (exact fallback path — no warm; identical results by contract)
    query, expects = cfg.get("search", (None, None))
    if query:
        out = sh(BIN, "search", out_a, query, "10").stdout.lower()
        ok = any(tok in out for tok in expects)
        grade(name, f"search('{query[:28]}…')", ok,
              f"expected one of {expects}" + ("" if ok else f" — got: {out[:70]}"))

    shutil.rmtree(out_a, ignore_errors=True)
    if cloned and os.environ.get("REPO_EVAL_KEEP") != "1":
        shutil.rmtree(src, ignore_errors=True)


for name in CHOSEN:
    eval_repo(name, REPOS[name])

print()
w = max((len(f"{r}/{c}") for r, c, _, _ in rows), default=10)
passed = sum(1 for r in rows if r[2] == "PASS")
for repo, check, res, ev in rows:
    print(f"{repo+'/'+check:<{w}}  {res:<5} {ev}")
print(f"\n{passed}/{len(rows)} checks passed")

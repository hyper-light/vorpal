#!/usr/bin/env python3
"""Daemon: save a file -> answers reflect it. A watched `vorpal mcp` daemon over stdio on a scratch
copy of the kernel tree, quiet-gated, 7 reps: append a function, poll `node` for it every 20 ms,
record the delay, the daemon's `health` record before and after, and a machine sample after.
Usage: daemon_save_latency.py <vorpal-bin> <kernel-copy> <out-dir> [add|body]
  add  (default): append a new function; visibility = `node` returns one record for it
  body: change one statement inside `vfs_read`; visibility = `snippet vfs_read` shows the new text"""
import json, os, subprocess, sys, time, pathlib, statistics
BIN, COPY, OUT = sys.argv[1], pathlib.Path(sys.argv[2]), pathlib.Path(sys.argv[3])
CLASS = sys.argv[4] if len(sys.argv) > 4 else "add"
ANCHOR = "ret = rw_verify_area(READ, file, pos, count);"
# The anchor occurs twice in the file; the body edit must land INSIDE vfs_read (the second
# occurrence, after its signature), or `snippet vfs_read` can never show it.
def edit_inside_vfs_read(text, marker):
    at = text.index("ssize_t vfs_read(struct file *file")
    hit = text.index(ANCHOR, at)
    return text[:hit + len(ANCHOR)] + " " + marker + text[hit + len(ANCHOR):]
src = open(str(pathlib.Path(__file__).resolve().with_name("readme_bench.py"))).read().split("# ---------------- index17")[0]
src = src.replace('BIN = sys.argv[1]; OUT = pathlib.Path(sys.argv[2]); OUT.mkdir(parents=True, exist_ok=True)', 'BIN = sys.argv[1]; OUT = pathlib.Path(sys.argv[3]); OUT.mkdir(parents=True, exist_ok=True)')
src = src.replace('PHASES = sys.argv[sys.argv.index("--phases") + 1].split(",") if "--phases" in sys.argv else ["index17", "edit", "tiers", "giant", "scan"]', 'PHASES = []')
g = {}; exec(compile(src, "rb-gate", "exec"), g); wait_quiet, log, run = g["wait_quiet"], g["log"], g["run"]
target = COPY / "fs" / "read_write.c"; original = target.read_text()
assert CLASS == "add" or ANCHOR in original
run([BIN, "index", str(COPY)])
p = subprocess.Popen([BIN, "mcp", "--index", str(COPY / ".vorpal" / "index")], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=open(OUT / f"save_latency_{CLASS}.stderr", "w"), text=True)
i = 0
def send(o): p.stdin.write(json.dumps(o)+"\n"); p.stdin.flush()
def call(tool, args):
    global i; i += 1; send({"jsonrpc":"2.0","id":i,"method":"tools/call","params":{"name":tool,"arguments":args}}); return json.loads(p.stdout.readline())
send({"jsonrpc":"2.0","id":9000,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"save","version":"0"}}}); p.stdout.readline()
send({"jsonrpc":"2.0","method":"notifications/initialized"})
call("node", {"name": "vfs_read"})  # boot revalidation
time.sleep(3)
def health():
    r = call("health", {}); return r.get("result", {}).get("structuredContent", {})
rows = []
for rep in range(7):
    quiet = wait_quiet()
    h0 = health()
    name = f"vorpal_save_probe_{rep}"
    marker = f"/* body edit {rep} */"
    t0 = time.perf_counter()
    if CLASS == "add":
        target.write_text(original + f"\nint {name}(void) {{ return {rep}; }}\n")
    else:
        target.write_text(edit_inside_vfs_read(original, marker))
    while True:
        if CLASS == "add":
            r = call("node", {"name": name})
            text = r.get("result", {}).get("content", [{}])[0].get("text", "")
            if name in text and "records[1]" in text:
                break
        else:
            r = call("snippet", {"name": "vfs_read"})
            text = json.dumps(r.get("result", {}))
            if marker in text:
                break
        if time.perf_counter() - t0 > 60: break
        time.sleep(0.02)
    seen = time.perf_counter() - t0
    h1 = health(); q1 = {"idle_pct": g["cpu_idle_pct"](), "fseventsd_pcpu": max([float(l.split()[0]) for l in subprocess.run(["ps","-Ao","pcpu,comm"],capture_output=True,text=True,stdin=subprocess.DEVNULL).stdout.splitlines()[1:] if l.strip().endswith("fseventsd")] or [0.0])}
    target.write_text(original)
    t1 = time.perf_counter()
    while True:
        if CLASS == "add":
            r = call("node", {"name": name})
            text = r.get("result", {}).get("content", [{}])[0].get("text", "")
            if "records[0]" in text or name not in text:
                break
        else:
            r = call("snippet", {"name": "vfs_read"})
            if marker not in json.dumps(r.get("result", {})):
                break
        if time.perf_counter() - t1 > 60: break
        time.sleep(0.02)
    gone = time.perf_counter() - t1
    h2 = health()
    row = {"class": CLASS, "rep": rep, "save_to_visible_s": round(seen, 3), "restore_to_gone_s": round(gone, 3), "quiet": quiet, "health_before": h0, "health_after_save": h1, "sample_after_save": q1, "health_after_restore": h2}
    rows.append(row); log(row); json.dump(rows, open(OUT / f"save_latency_{CLASS}.json", "w"), indent=1)
    time.sleep(2)
p.stdin.close(); p.kill()
log("save->visible median", round(statistics.median(r["save_to_visible_s"] for r in rows), 3), "restore median", round(statistics.median(r["restore_to_gone_s"] for r in rows), 3))

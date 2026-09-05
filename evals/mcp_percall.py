#!/usr/bin/env python3
"""Per-call round trips behind the README's "Against an agent's built-in tools" table.

A warm `vorpal mcp` daemon over stdio (VORPAL_NO_AUTOWARM=1; median of five calls after
one first call, whose time is reported separately as the cold open) versus the rg/sed
pipelines Claude Code's Grep and Read tools run, on this repo and the Linux kernel.

    python3 evals/mcp_percall.py <results.json>

Recorded runs: docs/wip/BENCHMARKS.md (2026-09-05, v0.8.2)."""
import json, os, subprocess, sys, time, statistics
VORPAL = os.path.expanduser("~/.local/bin/vorpal")
REPO = "/Users/adalundhe/Projects/vorpal"; KERNEL = "/Users/adalundhe/Projects/linux"
class Daemon:
  def __init__(self, index):
    self.p = subprocess.Popen([VORPAL, "mcp", "--index", index], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, env={**os.environ, "VORPAL_NO_AUTOWARM": "1"})
    self.id = 0
    self.send({"jsonrpc":"2.0","id":self.nid(),"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"bench","version":"0"}}}); self.p.stdout.readline()
    self.send({"jsonrpc":"2.0","method":"notifications/initialized"})
  def nid(self): self.id += 1; return self.id
  def send(self, o): self.p.stdin.write(json.dumps(o)+"\n"); self.p.stdin.flush()
  def call(self, tool, args):
    t0 = time.perf_counter(); self.send({"jsonrpc":"2.0","id":self.nid(),"method":"tools/call","params":{"name":tool,"arguments":args}})
    line = self.p.stdout.readline(); dt = (time.perf_counter()-t0)*1000
    r = json.loads(line)["result"]; return dt, r
  def close(self): self.p.stdin.close(); self.p.kill()
def med_call(d, tool, args, n=5):
  first_dt, r = d.call(tool, args)
  dts = [d.call(tool, args)[0] for _ in range(n)]
  sc = r.get("structuredContent", {}) or {}
  recs = sc.get("records") or sc.get("hits") or []
  return statistics.median(dts), len(json.dumps(sc)), (len(recs) if isinstance(recs, list) else "?"), first_dt
def med_cmd(cmd, cwd, n=5):
  # stdin must be /dev/null: with an inherited pipe and no path argument, rg searches STDIN
  # and waits for EOF forever (the 21-minute hang of 2026-09-05).
  out = subprocess.run(cmd, cwd=cwd, shell=True, capture_output=True, text=True, stdin=subprocess.DEVNULL)
  dts = []
  for _ in range(n):
    t0 = time.perf_counter(); subprocess.run(cmd, cwd=cwd, shell=True, capture_output=True, text=True, stdin=subprocess.DEVNULL); dts.append((time.perf_counter()-t0)*1000)
  return statistics.median(dts), len(out.stdout.encode()), out.stdout.count("\n")
rows = []
for corpus, root in (("repo", REPO), ("kernel", KERNEL)):
  d = Daemon(f"{root}/.vorpal/index")
  cold, _ = d.call("node", {"name": "alpha_nonexistent_probe"})
  if corpus == "repo":
    probes = [("callers tool_result", "graph", {"relation":"callers","name":"tool_result"}, r"rg -n 'tool_result\(' crates"),
              ("callees tool_result", "graph", {"relation":"callees","name":"tool_result"}, None),
              ("reachable run_install out", "reachable", {"name":"run_install","direction":"out","min_grade":"exact"}, r"rg -n -A 75 'fn run_install' crates/cli/src/mcp_install.rs"),
              ("snippet render_toml", "snippet", {"name":"render_toml"}, "rg -n 'fn render_toml' crates && sed -n \"$(rg -n 'fn render_toml' crates | head -1 | cut -d: -f2),+56p\" \"$(rg -l 'fn render_toml' crates | head -1)\""),
              ("search 'stdio pump reader thread'", "search", {"query":"stdio pump reader thread","k":5}, r"rg -n -i 'stdio.*pump|reader thread' crates")]
  else:
    probes = [("callers kmalloc (limit 100)", "graph", {"relation":"callers","name":"kmalloc","limit":100}, r"rg -n 'kmalloc\(' -t c"),
              ("callers vfs_read", "graph", {"relation":"callers","name":"vfs_read"}, r"rg -n 'vfs_read\(' -t c"),
              ("callees vfs_read", "graph", {"relation":"callees","name":"vfs_read"}, "rg -n -A 40 '^ssize_t vfs_read\\(' -t c"),
              ("node schedule_timeout", "node", {"name":"schedule_timeout"}, r"rg -n '^signed long __sched schedule_timeout\(' -t c"),
              ("snippet vfs_read", "snippet", {"name":"vfs_read"}, "rg -n '^ssize_t vfs_read\\(' -t c && sed -n \"$(rg -n '^ssize_t vfs_read\\(' fs/read_write.c | head -1 | cut -d: -f1),+40p\" fs/read_write.c"),
              ("reachable vfs_read out depth 2 exact", "reachable", {"name":"vfs_read","direction":"out","max_depth":2,"min_grade":"exact"}, None)]
  for label, tool, args, cmd in probes:
    dt, nbytes, nrec, first = med_call(d, tool, args)
    if cmd:
      cdt, cbytes, clines = med_cmd(cmd, root)
      rows.append((corpus, label, dt, nbytes, nrec, cdt, cbytes, clines, cmd))
    else:
      rows.append((corpus, label, dt, nbytes, nrec, None, None, None, None))
    print(f"{corpus:6} {label:36} vorpal {dt:7.2f} ms {nbytes:7,} B {nrec} rec | " + (f"cmd {cdt:7.1f} ms {cbytes:9,} B {clines} lines | {cmd}" if cmd else "no grep equivalent"), flush=True)
  print(f"{corpus:6} cold-open first call: {cold:.1f} ms")
  d.close()
json.dump(rows, open(sys.argv[1], "w"), indent=1)

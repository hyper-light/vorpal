#!/usr/bin/env python3
"""Real-world MCP evaluation against the Linux kernel.

Drives the INSTALLED `vorpal mcp` over stdio with kernel-developer questions and grades
every answer against independently derived ground truth (ripgrep over the tree, exact
source lines, or an edit we inject ourselves). Prints a scorecard with latencies.
"""
import json, subprocess, threading, time, re, sys, os

LINUX = "/Users/adalundhe/Projects/linux"
INDEX = f"{LINUX}/.vorpal/index"

p = subprocess.Popen(["vorpal", "mcp", "--index", INDEX],
                     stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                     stderr=subprocess.DEVNULL, text=True, bufsize=1)
rid = [0]

def rpc(method, params=None):
    rid[0] += 1
    msg = {"jsonrpc": "2.0", "id": rid[0], "method": method}
    if params is not None:
        msg["params"] = params
    t0 = time.perf_counter()
    p.stdin.write(json.dumps(msg) + "\n"); p.stdin.flush()
    line = p.stdout.readline()
    dt = (time.perf_counter() - t0) * 1000
    return json.loads(line), dt

# MCP 2026-07-28: stateless — probe once, then every request self-describes in _meta.
META = {"io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": {"name": "mcp_linux_eval", "version": "0"}}

def tool(name, args):
    resp, dt = rpc("tools/call", {"name": name, "arguments": args, "_meta": META})
    try:
        text = resp["result"]["content"][0]["text"]
    except Exception:
        text = json.dumps(resp)[:400]
    return text, dt

def rg(pattern, *extra):
    out = subprocess.run(["rg", "--no-heading", "-n", pattern, *extra, LINUX],
                         capture_output=True, text=True).stdout
    return out.splitlines()

rows = []
def grade(task, ok, latency, evidence):
    rows.append((task, "PASS" if ok else "FAIL", f"{latency:.0f} ms", evidence))

discover, _ = rpc("server/discover", {"_meta": META})
assert "2026-07-28" in discover.get("result", {}).get("supportedVersions", []), discover
tool("health", {})  # first call absorbs boot work

# ---- 1. the no-fake-edges contract: kmalloc_slab is `static inline` in mm/slab.h, so
# cross-file callers are MASKED (counted, never guessed). PASS = the node exists and
# callers is honestly empty — if this ever "improves" without include-graph awareness,
# it means edges are being faked.
text, dt = tool("node", {"name": "kmalloc_slab"})
node_ok = "slab.h" in text and "static inline" in text
text, dt = tool("graph", {"relation": "callers", "name": "kmalloc_slab", "limit": 50})
grade("static-inline masking honest (kmalloc_slab)", node_ok and "no results" in text,
      dt, "node exists, cross-file calls masked" if node_ok else text[:80])

# ---- 2. snippet: verbatim source of vfs_read ----
text, dt = tool("snippet", {"name": "vfs_read", "kind": "Function"})
src = open(f"{LINUX}/fs/read_write.c").read()
body = src[src.index("ssize_t vfs_read"):]
real_lines = [l.strip() for l in body.splitlines()[1:12] if l.strip()][:5]
hits = sum(1 for l in real_lines if l in text)
grade("snippet(vfs_read) is byte-faithful", hits >= 4 and "read_write.c" in text,
      dt, f"{hits}/5 exact source lines present")

# ---- 3. type_users: who defines a struct file_operations? ----
text, dt = tool("graph", {"relation": "type_users", "name": "file_operations", "limit": 200})
ok = text.count("[Variable]") >= 20 and "dir" in text
grade("type_users(file_operations) finds instances", ok,
      dt, f"{text.count(chr(91))} typed instances in first page")

# ---- 4. semantic search: "copy on write page fault" ----
text, dt = tool("search", {"query": "copy on write page fault", "k": 10})
ok = ("do_wp_page" in text) or ("wp_page_copy" in text) or ("mm/memory.c" in text)
grade("search('copy on write page fault') hits mm/memory.c", ok, dt,
      "found COW handler" if ok else text[:80])

# ---- 5. semantic search: "socket buffer allocation" ----
text, dt = tool("search", {"query": "socket buffer allocation", "k": 10})
ok = ("alloc_skb" in text) or ("skbuff.c" in text)
grade("search('socket buffer allocation') hits skbuff", ok, dt,
      "found alloc_skb family" if ok else text[:80])

# ---- 6. callers: schedule_timeout_interruptible (moderate fan-in) ----
text, dt = tool("graph", {"relation": "callers", "name": "schedule_timeout_interruptible", "limit": 200})
gt = set()
for line in rg(r"\bschedule_timeout_interruptible\(", "-t", "c"):
    path, lineno, code = line.split(":", 2)
    if "signed long" in code or "#define" in code:
        continue
    gt.add(os.path.basename(path))
covered = sum(1 for f in gt if f in text)
grade("callers(schedule_timeout_interruptible) coverage", len(gt) > 0 and covered / len(gt) >= 0.6,
      dt, f"{covered}/{len(gt)} rg files covered")

# ---- 7. structural search: kzalloc($A, GFP_KERNEL) ----
text, dt = tool("structural_search", {"pattern": "kzalloc($A, GFP_KERNEL)", "lang": "c", "limit": 5})
m = re.search(r"(\d+)\s+match", text)
vorpal_n = int(m.group(1)) if m else text.count("kzalloc")
rg_n = len(rg(r"kzalloc\([^,)]+,\s*GFP_KERNEL\)", "-t", "c"))
ok = vorpal_n >= int(rg_n * 0.8) or text.count("kzalloc") >= 3
grade("structural_search kzalloc($A, GFP_KERNEL)", ok, dt,
      f"vorpal={vorpal_n if m else '>=%d shown' % text.count('kzalloc')} vs rg(single-line)={rg_n}")

# ---- 8. reachable: what does vfs_write call? ----
text, dt = tool("reachable", {"name": "vfs_write", "direction": "out", "max_depth": 1, "limit": 100})
ok = "rw_verify_area" in text
grade("reachable(vfs_write, out) includes rw_verify_area", ok, dt,
      "direct callee present" if ok else text[:80])

# ---- 9. EDIT FRESHNESS: new symbol visible after a save, gone after revert ----
probe = "\nvoid vorpal_eval_probe(void) { kfree((void *)0); }\n"
with open(f"{LINUX}/mm/slab_common.c", "a") as f:
    f.write(probe)
t0 = time.time(); seen = None
for _ in range(40):
    text, dt = tool("node", {"name": "vorpal_eval_probe"})
    if "slab_common.c" in text:
        seen = time.time() - t0
        break
    time.sleep(0.25)
grade("edit -> new symbol queryable", seen is not None, (seen or 0) * 1000,
      f"visible {seen:.2f}s after save" if seen else "never appeared (10s)")

# ---- 10. impact of the working-tree change (git-diff based) ----
text, dt = tool("impact", {"since": "HEAD", "limit": 30})
m2 = re.search(r"(\d+) seed definitions? .*?(\d+) impacted", text)
ok = m2 is not None and int(m2.group(1)) > 0
grade("impact(since=HEAD) sees the edit", ok, dt,
      f"{m2.group(1)} seeds -> {m2.group(2)} impacted" if m2 else text[:80])

# ---- revert, confirm the symbol is gone ----
subprocess.run(["git", "checkout", "--", "mm/slab_common.c"], cwd=LINUX, capture_output=True)
gone = None
t0 = time.time()
for _ in range(40):
    text, dt = tool("node", {"name": "vorpal_eval_probe"})
    if "no results" in text:
        gone = time.time() - t0
        break
    time.sleep(0.25)
grade("revert -> symbol disappears", gone is not None, (gone or 0) * 1000,
      f"gone {gone:.2f}s after revert" if gone else "still present (10s)")

# ---- 11. why: edge evidence for a call edge, verified against the source line ----
text, dt = tool("node", {"name": "vfs_read", "kind": "Function"})
mid = re.search(r"\bid (\d+)", text) or re.search(r"#(\d+)", text)
if mid:
    caller_id = int(mid.group(1))
    t2, _ = tool("node", {"name": "rw_verify_area", "kind": "Function"})
    to = re.search(r"\bid (\d+)", t2)
    text, dt = tool("why", {"from_id": caller_id, "to_id": int(to.group(1))}) if to else ("", 0)
    ok = "source verified" in text and "read_write.c" in text
    grade("why(vfs_read -> rw_verify_area) evidence", ok, dt, text.splitlines()[0][:60] if text else "no ids")
else:
    grade("why(vfs_read -> rw_verify_area) evidence", False, 0, "could not parse node id")

print()
w = max(len(r[0]) for r in rows)
print(f"{'TASK':<{w}}  {'RESULT':<6} {'LATENCY':>9}  EVIDENCE")
for task, res, lat, ev in rows:
    print(f"{task:<{w}}  {res:<6} {lat:>9}  {ev}")
passed = sum(1 for r in rows if r[1] == "PASS")
print(f"\n{passed}/{len(rows)} tasks passed")
p.stdin.close(); p.terminate()

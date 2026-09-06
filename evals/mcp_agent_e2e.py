#!/usr/bin/env python3
"""End-to-end agent benchmark behind the README's "What that costs end to end" table.

Four questions x three arms (grep/read only; vorpal MCP tools only; vorpal with the shell
allowed) on Claude Code, `--model opus --effort high`. Per cell, RUNS runs back to back
(default 4). Cost on Opus is dominated by the prompt-cache WRITE of the prefix (system
prompt, tool schemas, MCP instructions; 1-hour TTL at twice the input price), and that
prefix is shared by every question of one arm+corpus group, so only the first run of a
group in a fresh hour is cold; every later run is warm. The harness therefore orders runs
by group, marks the first run of each group `cold_intent`, and records every run's usage
classes (cache_creation / cache_read / input / output), billed cost, turns, wall, tool
calls, and stream-json transcript. A run is FULLY cold only when its first turn's cache_read is 0, which needs no Claude Code run on the
machine for the previous hour (static prefix blocks are shared across arms); the README reports
the warm medians per cell and the fully-cold first-ask surcharge per arm. Effort is pinned with the --effort flag: CLAUDE_EFFORT is a variable
Claude Code EXPORTS to hooks and Bash, not one it reads (2026-09-06 finding).

    python3 evals/mcp_agent_e2e.py <out-dir> [grep,mcp,cli] [--runs N] [--not-before <unix epoch>] [--only <corpus>:<question-key>]

Results append to <out-dir>/results.json (one row per run). Needs ~/.local/bin/vorpal and
indexes at <repo>/.vorpal/index and ~/Projects/linux/.vorpal/index.
Recorded runs: docs/wip/BENCHMARKS.md."""
import json, os, statistics, subprocess, sys, time, pathlib
args = sys.argv[1:]
RUNS = int(args[args.index("--runs") + 1]) if "--runs" in args else 4
NOT_BEFORE = float(args[args.index("--not-before") + 1]) if "--not-before" in args else 0.0
ONLY = args[args.index("--only") + 1] if "--only" in args else None  # "<corpus>:<question-key>"
pos = [a for i, a in enumerate(args) if not a.startswith("--") and (i == 0 or args[i - 1] not in ("--runs", "--not-before", "--only"))]
S = pathlib.Path(pos[0]); S.mkdir(parents=True, exist_ok=True)
ARMS = pos[1].split(",") if len(pos) > 1 else ["grep", "mcp", "cli"]
VORPAL = os.path.expanduser("~/.local/bin/vorpal")
REPO = "/Users/adalundhe/Projects/vorpal"; KERNEL = "/Users/adalundhe/Projects/linux"
CORPORA = {"repo": (REPO, f"{REPO}/.vorpal/index"), "kernel": (KERNEL, f"{KERNEL}/.vorpal/index")}
QUESTIONS = [
  ("repo", "callers_tool_result", "Who calls `tool_result`? List every caller with its file and the line of the call."),
  ("repo", "reaches_run_install", "What does `run_install` reach through calls? List the functions it reaches (direct and transitive) with their files."),
  ("kernel", "callers_vfs_read", "Who calls `vfs_read`? List every caller with its file and the line of the call."),
  ("kernel", "callees_vfs_read", "What does `vfs_read` call directly? List the callees with the line of each call."),
]
SUFFIX = {
  "grep": " Use only Grep, Glob, Read (and rg/grep through Bash). Do not use any MCP tool. Reply with the answer only.",
  "mcp":  " Use the vorpal MCP tools. Reply with the answer only.",
  "cli":  " Use vorpal — its MCP tools or the vorpal CLI through the shell, whichever is cheaper. Reply with the answer only.",
}
def mcp_config(index):
  p = S / f"mcp-{pathlib.Path(index).parts[-3]}.json"
  p.write_text(json.dumps({"mcpServers": {"vorpal": {"command": VORPAL, "args": ["mcp", "--index", index]}}}))
  return str(p)
EMPTY = S / "mcp-empty.json"; EMPTY.write_text(json.dumps({"mcpServers": {}}))
RESULTS = S / "results.json"
rows = json.loads(RESULTS.read_text()) if RESULTS.exists() else []

def run(corpus, key, question, arm, run_idx, cold_intent):
  cwd, index = CORPORA[corpus]
  cmd = ["claude", "-p", question + SUFFIX[arm], "--model", "opus", "--effort", "high", "--output-format", "stream-json", "--verbose", "--strict-mcp-config"]
  if arm == "grep":
    cmd += ["--mcp-config", str(EMPTY), "--allowedTools", "Grep,Glob,Read,Bash(rg:*),Bash(grep:*)", "--disallowedTools", "Edit,Write,Agent"]
  elif arm == "mcp":
    cmd += ["--mcp-config", mcp_config(index), "--allowedTools", "mcp__vorpal", "--disallowedTools", "Grep,Glob,Read,Bash,Edit,Write,Agent"]
  else:
    # The fast-path note names the daemon's own executable by absolute path, so the shell
    # allowlist must match that spelling too — `Bash(vorpal:*)` alone denies the command.
    cmd += ["--mcp-config", mcp_config(index), "--allowedTools", f"mcp__vorpal,Bash(vorpal:*),Bash({VORPAL}:*)", "--disallowedTools", "Grep,Glob,Read,Edit,Write,Agent"]
  t0 = time.time()
  proc = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, timeout=600, stdin=subprocess.DEVNULL)
  wall = time.time() - t0
  calls, result, answer, per_turn = [], None, "", []
  for line in proc.stdout.splitlines():
    try: ev = json.loads(line)
    except Exception: continue
    if ev.get("type") == "assistant":
      m = ev.get("message")
      if isinstance(m, dict):
        u = m.get("usage") or {}
        per_turn.append({k: u.get(k) for k in ("input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens", "output_tokens")})
        for block in m.get("content", []) or []:
          if isinstance(block, dict) and block.get("type") == "tool_use":
            name = block.get("name", ""); inp = block.get("input", {})
            brief = name
            if name == "ToolSearch": brief = f"ToolSearch({inp.get('query','')})"
            elif name.startswith("mcp__vorpal__"): brief = name.replace("mcp__vorpal__", "") + (f"({inp.get('relation')})" if inp.get("relation") else "") + (f"[format={inp.get('format')}]" if inp.get("format") else "")
            elif name == "Bash": brief = "Bash(" + str(inp.get("command", ""))[:160] + ")"
            calls.append(brief)
          elif isinstance(block, dict) and block.get("type") == "text":
            answer = block.get("text", "")
    elif ev.get("type") == "result":
      result = ev
  usage = (result or {}).get("usage", {}) or {}
  cc, cr, ci, co = (usage.get(k, 0) or 0 for k in ("cache_creation_input_tokens", "cache_read_input_tokens", "input_tokens", "output_tokens"))
  row = {"corpus": corpus, "question": key, "arm": arm, "run": run_idx, "cold_intent": cold_intent,
         "turns": (result or {}).get("num_turns"), "ts": sum(1 for c in calls if c.startswith("ToolSearch")),
         "tokens": ci + cr + cc, "cache_create": cc, "cache_read": cr, "input": ci, "output": co,
         "cost": (result or {}).get("total_cost_usd"), "wall_s": round(((result or {}).get("duration_ms") or wall * 1000) / 1000, 1),
         "models": sorted(((result or {}).get("modelUsage") or {}).keys()), "calls": calls, "per_turn": per_turn,
         "rc": proc.returncode, "answer": answer[:1500], "error": (result or {}).get("is_error"), "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(t0))}
  (S / f"{corpus}-{key}-{arm}-run{run_idx}.stream.jsonl").write_text(proc.stdout)
  if proc.stderr: (S / f"{corpus}-{key}-{arm}-run{run_idx}.stderr").write_text(proc.stderr)
  return row

if NOT_BEFORE:
  while time.time() < NOT_BEFORE:
    time.sleep(min(30, NOT_BEFORE - time.time()))
for arm in ARMS:
  for corpus in ("repo", "kernel"):
    first_in_group = True
    for c, key, q in QUESTIONS:
      if c != corpus: continue
      if ONLY and f"{c}:{key}" != ONLY: continue
      done = [x["run"] for x in rows if x["corpus"] == c and x["question"] == key and x["arm"] == arm]
      start = (max(done) + 1) if done else 1  # appending to an existing results.json continues the numbering
      for r in range(start, start + RUNS):
        cold_intent = first_in_group and r == 1
        row = run(corpus, key, q, arm, r, cold_intent); rows.append(row)
        RESULTS.write_text(json.dumps(rows, indent=1))
        print(f"{corpus:6} {key:20} {arm:4} run{r} {'COLD' if cold_intent else 'warm'} turns={row['turns']} ts={row['ts']} tokens={row['tokens']:,} create={row['cache_create']:,} read={row['cache_read']:,} cost={row['cost']} wall={row['wall_s']} models={row['models']} calls={' | '.join(row['calls'])}", flush=True)
        first_in_group = False

# summary: cold run cost per group's first cell; warm medians per cell
print("\n== summary (this invocation's arms) ==")
for c, key, q in QUESTIONS:
  for arm in ARMS:
    cell = [r for r in rows if r["corpus"] == c and r["question"] == key and r["arm"] == arm]
    if not cell: continue
    warm = [r for r in cell if not r["cold_intent"]]
    cold = [r for r in cell if r["cold_intent"]]
    med = lambda k, rs: statistics.median(r[k] for r in rs if r.get(k) is not None) if rs else None
    print(f"{c:6} {key:20} {arm:4} cold={cold[0]['cost'] if cold else '-'} (create {cold[0]['cache_create'] if cold else '-'}) warm: n={len(warm)} turns={med('turns', warm)} tokens={med('tokens', warm)} cost={med('cost', warm)} wall={med('wall_s', warm)}")

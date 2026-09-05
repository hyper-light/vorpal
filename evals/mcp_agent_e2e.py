#!/usr/bin/env python3
"""End-to-end agent benchmark behind the README's "What that costs end to end" table.

Four questions x three arms (grep/read only; vorpal MCP tools only; vorpal with the shell
allowed) on Claude Code, `--model opus`, CLAUDE_EFFORT=high, one run each. Writes
results.json plus one stream-json transcript per run into the output directory.
Tokens = input + cache_read + cache_creation from the result event; cost as billed.

    python3 evals/mcp_agent_e2e.py <out-dir> [grep,mcp,cli]

Needs ~/.local/bin/vorpal and indexes at <repo>/.vorpal/index and ~/Projects/linux/.vorpal/index.
Recorded runs: docs/wip/BENCHMARKS.md (2026-09-05, v0.8.2)."""
import json, os, subprocess, sys, time, pathlib
S = pathlib.Path(sys.argv[1]); S.mkdir(parents=True, exist_ok=True)
ARMS = sys.argv[2].split(",") if len(sys.argv) > 2 else ["grep", "mcp", "cli"]
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
def run(corpus, key, question, arm):
  cwd, index = CORPORA[corpus]
  cmd = ["claude", "-p", question + SUFFIX[arm], "--model", "opus", "--output-format", "stream-json", "--verbose", "--strict-mcp-config"]
  if arm == "grep":
    cmd += ["--mcp-config", str(EMPTY), "--allowedTools", "Grep,Glob,Read,Bash(rg:*),Bash(grep:*)", "--disallowedTools", "Edit,Write,Agent"]
  elif arm == "mcp":
    cmd += ["--mcp-config", mcp_config(index), "--allowedTools", "mcp__vorpal", "--disallowedTools", "Grep,Glob,Read,Bash,Edit,Write,Agent"]
  else:
    # The fast-path note names the daemon's own executable by absolute path, so the shell
    # allowlist must match that spelling too — `Bash(vorpal:*)` alone denies the command.
    cmd += ["--mcp-config", mcp_config(index), "--allowedTools", f"mcp__vorpal,Bash(vorpal:*),Bash({VORPAL}:*)", "--disallowedTools", "Grep,Glob,Read,Edit,Write,Agent"]
  t0 = time.time()
  proc = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, timeout=600, stdin=subprocess.DEVNULL, env={**os.environ, 'CLAUDE_EFFORT': 'high'})
  wall = time.time() - t0
  calls, result, answer = [], None, ""
  for line in proc.stdout.splitlines():
    try: ev = json.loads(line)
    except Exception: continue
    if ev.get("type") == "assistant":
      m = ev.get("message")
      if isinstance(m, dict):
        for block in m.get("content", []) or []:
          if isinstance(block, dict) and block.get("type") == "tool_use":
            name = block.get("name", "")
            inp = block.get("input", {})
            brief = name
            if name == "ToolSearch": brief = f"ToolSearch({inp.get('query','')})"
            elif name.startswith("mcp__vorpal__"): brief = name.replace("mcp__vorpal__", "") + (f"({inp.get('relation')})" if inp.get("relation") else "")
            elif name == "Bash": brief = "Bash(" + str(inp.get("command", ""))[:90] + ")"
            calls.append(brief)
          elif isinstance(block, dict) and block.get("type") == "text":
            answer = block.get("text", "")
    elif ev.get("type") == "result":
      result = ev
  usage = (result or {}).get("usage", {}) or {}
  tokens = usage.get("input_tokens", 0) + usage.get("cache_read_input_tokens", 0) + usage.get("cache_creation_input_tokens", 0)
  row = {"corpus": corpus, "question": key, "arm": arm, "turns": (result or {}).get("num_turns"), "ts": sum(1 for c in calls if c.startswith("ToolSearch")),
         "tokens": tokens, "cost": (result or {}).get("total_cost_usd"), "wall_s": round(((result or {}).get("duration_ms") or wall*1000)/1000, 1),
         "calls": calls, "rc": proc.returncode, "answer": answer[:1500], "error": (result or {}).get("is_error")}
  (S / f"{corpus}-{key}-{arm}.stream.jsonl").write_text(proc.stdout)
  if proc.stderr: (S / f"{corpus}-{key}-{arm}.stderr").write_text(proc.stderr)
  return row
rows = []
for corpus, key, q in QUESTIONS:
  for arm in ARMS:
    row = run(corpus, key, q, arm); rows.append(row)
    print(f"{corpus:6} {key:20} {arm:4} turns={row['turns']} ts={row['ts']} tokens={row['tokens']:,} cost={row['cost']} wall={row['wall_s']} calls={' '.join(row['calls'])}", flush=True)
    (S / "results.json").write_text(json.dumps(rows, indent=1))

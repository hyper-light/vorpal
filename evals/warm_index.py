#!/usr/bin/env python3
"""Warm an index's ANN/postings tiers through a daemon with autowarm on; wait for ann.bin.

    python3 evals/warm_index.py <vorpal-binary> <index-dir>
"""
import json, os, subprocess, sys, time, pathlib
binary, index = sys.argv[1], sys.argv[2]
p = subprocess.Popen([binary, "mcp", "--index", index], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
def send(o): p.stdin.write(json.dumps(o)+"\n"); p.stdin.flush()
send({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"warm","version":"0"}}}); p.stdout.readline()
send({"jsonrpc":"2.0","method":"notifications/initialized"})
t0 = time.time(); i = 1
while time.time() - t0 < 600:
    i += 1
    send({"jsonrpc":"2.0","id":i,"method":"tools/call","params":{"name":"node","arguments":{"name":"vfs_read"}}}); p.stdout.readline()
    gen = pathlib.Path(index) / (pathlib.Path(index, "CURRENT").read_text().strip())
    if (gen / "ann.bin").exists() and (gen / "postings.bin").exists() or (gen / "ann.bin").exists() and any(gen.glob("postings*")):
        # give the writer a moment to finish sidecars, then confirm stability
        time.sleep(2)
        print(f"warm artifacts present after {time.time()-t0:.1f}s: {sorted(f.name for f in gen.iterdir() if f.name.startswith(('ann', 'postings')))}")
        break
    time.sleep(1)
else:
    print("warm did not complete in 600 s")
p.stdin.close(); p.kill()

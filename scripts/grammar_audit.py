#!/usr/bin/env python3
"""Grammar supply-chain audit (IMPROVEMENTS #10).

For every entry in grammars/PROVENANCE.json:
  1. the pinned commit must still exist upstream (a vanished commit means history was
     rewritten or the ledger is wrong — exactly what caught tree-sitter-rust's bad pin);
  2. our vendored parser sources (parser.c / scanner.c / grammar.js) must byte-match the
     pinned commit's tree, except where the entry's `patches` field owns the divergence;
  3. newer upstream tags are reported as available updates.

Exit code is nonzero when (1) or (2) fails — CI turns that into a tracking issue. Newer
tags alone are informational. The update procedure itself stays deliberate and reproducible
(docs/UPSTREAM.md): fetch the tag tarball, replace the tree, re-run the provenance
regenerate test, and let the corpus + provenance gates arbitrate the PR.
"""

from __future__ import annotations

import argparse
import io
import json
import os
import subprocess
import sys
import tarfile

PROBES = ("parser.c", "scanner.c", "grammar.js")


def fetch(url: str) -> bytes | None:
    result = subprocess.run(
        ["curl", "-fsSL", "--max-time", "120", url], capture_output=True
    )
    return result.stdout if result.returncode == 0 else None


def newer_tags(org_repo: str, version: str) -> list[str]:
    raw = fetch(f"https://api.github.com/repos/{org_repo}/tags?per_page=40")
    if raw is None:
        return []
    try:
        tags = [t["name"].lstrip("v") for t in json.loads(raw)]
    except (json.JSONDecodeError, KeyError, TypeError):
        return []

    def key(tag: str):
        parts = tag.split(".")
        return tuple(int(p) for p in parts) if all(p.isdigit() for p in parts) else None

    current = key(version)
    if current is None:
        return []
    return [t for t in tags if key(t) is not None and key(t) > current]


def audit(report_path: str) -> int:
    provenance = json.load(open("grammars/PROVENANCE.json"))
    hard_failures: list[str] = []
    updates: list[str] = []
    lines: list[str] = ["# Grammar supply-chain audit", ""]

    for name, entry in sorted(provenance.items()):
        org_repo = entry["repository"].rstrip("/").removeprefix("https://github.com/").removesuffix(".git")
        commit = entry["commit"]
        patched = bool(entry.get("patches"))
        tarball = fetch(f"https://codeload.github.com/{org_repo}/tar.gz/{commit}")
        if tarball is None:
            hard_failures.append(f"{name}: pinned commit {commit} not fetchable upstream")
            continue
        tf = tarfile.open(fileobj=io.BytesIO(tarball), mode="r:gz")
        members = {
            m.name.split("/", 1)[1]: m
            for m in tf.getmembers()
            if m.isfile() and "/" in m.name
        }
        diffs: list[str] = []
        for root, _, files in os.walk(f"grammars/{name}"):
            for fname in files:
                if fname not in PROBES:
                    continue
                local = os.path.join(root, fname)
                rel = os.path.relpath(local, f"grammars/{name}")
                member = members.get(rel)
                if member is None:
                    # Generated files upstream never committed (swift's parser.c) are owned
                    # by the patches field; anything unowned is drift.
                    if not patched:
                        diffs.append(f"{rel}: absent upstream")
                    continue
                if tf.extractfile(member).read() != open(local, "rb").read():
                    diffs.append(f"{rel}: bytes differ")
        if diffs and not patched:
            hard_failures.append(f"{name}: {'; '.join(diffs)}")
        for tag in newer_tags(org_repo, entry["version"]):
            updates.append(f"{name}: {entry['version']} → {tag}")

    if hard_failures:
        lines += ["## Drift (build-blocking)", ""] + [f"- {f}" for f in hard_failures] + [""]
    if updates:
        lines += ["## Newer upstream tags (informational)", ""] + [f"- {u}" for u in updates] + [""]
    if not hard_failures and not updates:
        lines += ["All pinned commits verified; no newer tags.", ""]
    report = "\n".join(lines)
    print(report)
    if report_path:
        open(report_path, "w").write(report)
    return 1 if hard_failures else 0


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--report", default="")
    sys.exit(audit(parser.parse_args().report))

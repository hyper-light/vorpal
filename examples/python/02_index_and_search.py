"""Build a knowledge-graph index and search it — the repo-scale half of vorpal_py.

    pip install vorpal-py
"""
import json
import vorpal_py

SRC = "."                       # any source tree
IDX = ".vorpal/index"           # default CLI-compatible location

# Build (incremental: instant when nothing changed). Returns the one-line report.
print(vorpal_py.index_build(SRC, IDX))

# Structured build report instead, when you want the numbers.
report = vorpal_py.index_build_report(SRC, IDX)
print(f"indexed={report.indexed} replayed={report.skipped} error_files={report.error_files}")

# Hybrid search (rendered text; explain=True appends per-hit provenance).
print(vorpal_py.index_search(IDX, "parse configuration", k=5))

# One search, two orderings (needs an encoder: vorpal enable semantic-f16).
ranked = vorpal_py.index_search_ranked(IDX, "retry backoff", k=5)
print(json.dumps(ranked, indent=2, default=str) if not isinstance(ranked, str) else ranked)

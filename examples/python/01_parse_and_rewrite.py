"""Structural parse, search, and edit with the vorpal_py AST API.

    pip install vorpal-py
"""
from vorpal_py import SgRoot

code = """
def fetch(url):
    print("fetching", url)
    data = requests.get(url)
    print(data.status_code)
    return data
"""

root = SgRoot(code, "python").root()

# Pattern search with metavariable captures.
for call in root.find_all(pattern="print($$$ARGS)"):
    r = call.range()
    print(f"print at line {r.start.line + 1}: {call.text()}")

# Kind-based search.
calls = root.find_all(kind="call")
print(f"{len(calls)} call expressions total")

# Rule-object search: compose pattern + relational constraints.
inside_fetch = root.find_all(
    {"rule": {"pattern": "print($$$A)", "inside": {"kind": "function_definition"}}}
)
print(f"{len(inside_fetch)} prints inside functions")

# Rewrite: build each replacement from the captured args, then commit the
# edits into a new source string (the original is never mutated).
edits = []
for node in root.find_all(pattern="print($$$ARGS)"):
    args = ", ".join(a.text() for a in node.get_multiple_matches("ARGS"))
    edits.append(node.replace(f"logging.info({args})"))
print(root.commit_edits(edits))

"""Walk the knowledge graph: names -> relations -> source, all from Python.

    pip install vorpal-py
"""
import vorpal_py

IDX = ".vorpal/index"
vorpal_py.index_build(".", IDX)

# Relation queries mirror the CLI verbs: callers, refs, importers,
# implementors, typeusers, node, reachable, snippet, ...
print(vorpal_py.index_graph(IDX, "node", "main"))
print(vorpal_py.index_graph(IDX, "callers", "main"))

# Disambiguate same-named symbols with path/kind/id, exactly like the CLI.
print(vorpal_py.index_graph(IDX, "snippet", "main", path=".rs"))

# ids=True appends node ids; feed one back for an exact query.
listing = vorpal_py.index_graph(IDX, "node", "main", ids=True)
print(listing)

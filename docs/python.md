# Python quickstart

```sh
pip install vorpal-py
```

Wheels are published for CPython 3.9+ on macOS, Linux (manylinux + musllinux), and Windows.
The import name is `vorpal_py`.

vorpal-py gives you two things: an **AST pattern engine** (structural search & rewrite on
source strings) and a **repository-intelligence API** (build a knowledge-graph index and query
it — sync and `async`).

## AST pattern matching

Patterns are real code with metavariables (`$X` captures one node, `$$$X` captures a list),
matched on the parsed tree — never regex.

```python
from vorpal_py import SgRoot

# Parse a snippet; the language is a name like "python", "typescript", "rust", "go", "c"…
root = SgRoot("console.log(user.name); console.log(count)", "typescript").root()

node = root.find(pattern="console.log($ARG)")
print(node.kind())                    # "call_expression"
print(node.get_match("ARG").text())   # "user.name"

for m in root.find_all(pattern="console.log($A)"):
    print(m["A"].text())              # dict-style metavar access: user.name, count
```

### Rewrite

Edits are explicit and committed against the source:

```python
r = SgRoot("console.log(user.name); console.log(count)", "typescript").root()
edits = [
    n.replace("logger.info({})".format(n.get_match("A").text()))
    for n in r.find_all(pattern="console.log($A)")
]
print(r.commit_edits(edits))
# => "logger.info(user.name); logger.info(count)"
```

### Rules & navigation

The full YAML rule vocabulary works as keyword arguments (`kind`, `inside`, `has`,
`all`/`any`/`not`, `stopBy`, …):

```python
root = SgRoot("function f() { console.log(1) }", "typescript").root()

# console.log calls, but only inside a function declaration
hits = root.find_all(
    pattern="console.log($A)",
    inside={"kind": "function_declaration", "stopBy": "end"},
)

call = root.find(kind="call_expression")
call.matches(pattern="console.log($A)")     # True — node predicates
call.inside(kind="function_declaration")    # True

fn = root.find(kind="function_declaration")
fn.field("name").text()                     # "f"
fn.children(); fn.parent(); fn.child(0)     # structural moves
[n.kind() for n in call.ancestors()][:3]
```

`register_dynamic_language(...)` loads custom tree-sitter grammars; `Range`, `Pos`, and `Edit`
are plain value classes.

## Repository intelligence

> Requires **vorpal-py 0.1.1+** (the `index_*`/`Index` and `async` functions).

Build a knowledge-graph index of a project, then query it.

```python
import vorpal_py

# Build (writes ./my_project/.vorpal/index), returns a one-line report string.
report = vorpal_py.index_build("./my_project")
print(report)

# One-shot queries against the index directory:
print(vorpal_py.index_search("./my_project/.vorpal/index", "parse config", k=5))
info = vorpal_py.index_node("./my_project/.vorpal/index", 42)   # -> NodeInfo
print(info.name, info.kind, info.path, info.signature)
```

For many queries, open a **pinned session** once (`Index`) — it holds the graph warm and
returns native dicts:

```python
idx = vorpal_py.Index.open("./my_project/.vorpal/index")
print(idx.generation)                                   # content id of the pinned index

for hit in idx.search("http handler", k=10, exported=True):
    print(hit)                                           # {name, kind, path, score, …}

idx.related("callers", "handle_request")                # incoming calls
idx.reachable("handle_request", direction="out",        # transitive traversal
              relations=["calls"], max_depth=3)
idx.why(from_id=10, to_id=42)                            # evidence for an edge
```

`Index` methods: `open`, `generation`, `node`, `nodes`, `related`, `reachable`, `why`, `search`.

## Async

The async functions are **real native coroutines** backed by a Rust-owned worker pool — each
`await` releases the GIL for the whole native call, so concurrent awaits run in genuine parallel
across cores (pool size: `VORPAL_ASYNC_WORKERS`, default 8× CPU cores).

```python
import asyncio, vorpal_py

async def main():
    await vorpal_py.build("./my_project")               # index, off-thread

    # Fan out many searches concurrently — one index open, real parallelism:
    blocks = await vorpal_py.search_many(
        "./my_project/.vorpal/index",
        ["parse config", "http handler", "retry logic"],
        k=5,
    )
    for b in blocks:
        print(b)

asyncio.run(main())
```

Async functions: `build`, `build_report`, `search`, `search_many`, `node`, `graph`.

## Language names

Case-insensitive, with common aliases: `python`/`py`, `javascript`/`js`/`jsx`, `typescript`/`ts`,
`tsx`, `rust`/`rs`, `go`/`golang`, `c`, `cpp`/`c++`, `java`, `ruby`/`rb`, `css`, `html`, `json`,
`yaml`, and more — see the [language matrix](./wip/LANGUAGES.md).

## Async: the whole surface is awaitable

The module-level awaitables (`build`, `build_report`, `search`, `search_many`, `node`,
`graph`, `search_ranked`, `tune`, `install`, `enable`) run GIL-free on a Rust-owned
worker pool and resolve on your running asyncio loop — N concurrent awaits do native
work on N cores. The pinned `Index` class mirrors its queries as `node_async` /
`nodes_async` / `related_async` / `reachable_async` / `why_async` / `search_async`:

```python
import asyncio
import vorpal_py

async def main():
    await vorpal_py.build(".", ".vorpal/index")
    index = vorpal_py.Index.open(".vorpal/index")
    callers, reach = await asyncio.gather(
        index.related_async("callers", "handle_request"),
        index.reachable_async("handle_request", "in", max_depth=3),
    )

asyncio.run(main())
```

Sync forms remain for scripts; on `Index` they answer in well under a millisecond.

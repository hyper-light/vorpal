# Getting started

vorpal is a single-binary code intelligence tool: point it at a repository and it builds a
searchable **knowledge graph** of your code — definitions, calls, imports, implementations,
type uses — plus AST-precise structural search and hybrid semantic search. This guide gets you
from install to your first queries in a few minutes.

## Install the CLI

vorpal ships two binaries: **`vorpal`** and **`vp`** (a short alias for the same tool).

### From source (works today, any platform)

Requires Rust 1.85+ ([rustup.rs](https://rustup.rs)).

```sh
git clone https://github.com/hyper-light/vorpal && cd vorpal
cargo build --release -p vorpal
# binaries are now at ./target/release/vorpal and ./target/release/vp
```

Put them on your `PATH`, e.g.:

```sh
cp target/release/vorpal target/release/vp ~/.local/bin/    # or /usr/local/bin
vorpal --help
```

### Prebuilt binaries (from a tagged release)

Each tagged release attaches a zip per platform to the
[Releases page](https://github.com/hyper-light/vorpal/releases), named `app-<target>.zip`
(each contains both `vorpal` and `vp`):

| Platform | Asset |
|---|---|
| macOS (Apple Silicon) | `app-aarch64-apple-darwin.zip` |
| macOS (Intel) | `app-x86_64-apple-darwin.zip` |
| Linux x86-64 (glibc) | `app-x86_64-unknown-linux-gnu.zip` |
| Linux ARM64 (glibc) | `app-aarch64-unknown-linux-gnu.zip` |
| Linux x86-64 (musl/static) | `app-x86_64-unknown-linux-musl.zip` |
| Linux ARM64 (musl/static) | `app-aarch64-unknown-linux-musl.zip` |
| Windows x64 | `app-x86_64-pc-windows-msvc.zip` |
| Windows ARM64 | `app-aarch64-pc-windows-msvc.zip` |

```sh
# macOS Apple Silicon example
curl -L -o vorpal.zip https://github.com/hyper-light/vorpal/releases/latest/download/app-aarch64-apple-darwin.zip
unzip vorpal.zip && chmod +x vorpal vp && mv vorpal vp ~/.local/bin/
```

> **No release yet for the version you want?** Build from source (above) — that's the supported
> path until the next tag is cut.

### Verify

```sh
vorpal --help
vorpal grammars        # list the 28 languages compiled in
```

## Your first index

vorpal builds an index into a hidden `.vorpal/index` directory. The simplest, foolproof flow is
to run everything from your project root:

```sh
cd my-project
vorpal index .                       # builds ./.vorpal/index
vorpal search "parse http request"   # reads ./.vorpal/index
vorpal graph callers handle_request  # reads ./.vorpal/index
```

That's the whole loop: **index once, then query.** Re-running `vorpal index .` is incremental —
only changed files re-parse, and an unchanged tree finishes in milliseconds.

> **One gotcha worth knowing.** `vorpal index <dir>` writes to `<dir>/.vorpal/index`, but
> `vorpal search`/`graph` read `./.vorpal/index` (relative to where you are). They line up when
> you index `.` from your project root. If you index somewhere else — say `vorpal index src` —
> point queries at it explicitly: `vorpal search "…" --index src/.vorpal/index`.

## The core commands

### `index` — build the graph

```sh
vorpal index .                    # index the current dir → ./.vorpal/index
vorpal index . --out /tmp/idx     # choose the output location
vorpal index . --verify           # content-verify the cache (CI-grade; slower)
```

### `search` — hybrid semantic search

Fuses exact/token name matching, lexical-embedding similarity, and graph popularity:

```sh
vorpal search "retry with backoff"        # top 10 by default
vorpal search "session store" -k 5        # top 5  (note: -k, no --k)
```

### `graph` — navigate relationships

`vorpal graph <verb> <name>`, where `<verb>` is one of:

```sh
vorpal graph callers   parse_config     # who calls it
vorpal graph refs      Config           # who references it
vorpal graph importers logger           # which files import it
vorpal graph implementors Storage       # types implementing/extending it
vorpal graph typeusers  UserId          # defs using it as a type
vorpal graph node      Config           # look up the symbol itself
vorpal graph reachable handle --direction out --relations calls --depth 3
```

Ambiguous names list candidates; refine with `--path <suffix>`, `--kind <function|struct|…>`,
or an exact `--id`. Add `--ids` to print stable node ids you can feed back in.

### `run` — structural search & rewrite (ast-grep style)

Patterns are real code with metavariables (`$X`, `$$$ARGS`), matched on the AST — not regex:

```sh
vorpal run -p 'console.log($ARG)' src/                       # find
vorpal run -p 'fetch($URL)' --rewrite 'await fetch($URL)' src/   # rewrite (-r)
vorpal run -p 'fn $N($$$A) -> Result<$T, $E>' -l rust        # -l sets the language
vorpal -p 'foo($A)' -l rs                                    # `run` is the default command
```

### `scan` — run YAML rules across a project

```sh
vorpal scan                       # runs your project's vorpalconfig.yml rules
vorpal scan -r my-rule.yml src/   # a single rule file  (here -r means --rule)
vorpal scan --format github       # CI-friendly output (also SARIF)
```

> `-r` means `--rewrite` in `run` but `--rule` in `scan` — easy to mix up.

### `outline` — file structure at a glance

```sh
vorpal outline src/                          # symbols, members, imports/exports
vorpal outline src/ --view signatures        # with signatures
```

### Output formats

`run`, `scan`, and `outline` support `--json` (add `=stream` for one object per line, `=compact`
for a single line): `vorpal run -p '…' --json=stream`. The graph/search commands print plain
text.

## Next steps

- **[Use it with Claude (MCP)](./mcp.md)** — wire vorpal's tools into an AI agent.
- **[Python quickstart](./python.md)** — `pip install vorpal-py`.
- **[TypeScript / JavaScript quickstart](./typescript.md)** — the pattern engine for Node & the browser.
- **[Supported languages](./wip/LANGUAGES.md)** — the full matrix of what each grammar extracts.
- **[How it works](./wip/ARCHITECTURE.md)** and **[benchmarks](./wip/BENCHMARKS.md)** — for the curious.

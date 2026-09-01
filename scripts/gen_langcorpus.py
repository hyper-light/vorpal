#!/usr/bin/env python3
"""Build a 49-language sampling corpus from the vendored grammar test corpora.

Each tree-sitter corpus .txt interleaves real code with s-expression
expectations:  =-line header pair around a name (+ optional :attributes),
code, ----line, expectation.  We keep the code sections, skip :skip/:error
tests (deliberately malformed / parser-hostile), route :language(tsx) tests
to Tsx, and write one source file per corpus .txt with the language's real
extension (or routing filename) so the manifest scanner picks it up.
"""
import os
import re
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.environ.get("LANGCORPUS_OUT", "/tmp/vorpal-langcorpus")

# corpus dir (under grammars/) -> language name, mirroring
# crates/language/tests/grammar_corpus.rs corpus_roots().
ROOTS = [
    ("tree-sitter-bash/test/corpus", "Bash"),
    ("tree-sitter-c/test/corpus", "C"),
    ("tree-sitter-c-sharp/test/corpus", "CSharp"),
    ("tree-sitter-cpp/test/corpus", "Cpp"),
    ("tree-sitter-css/test/corpus", "Css"),
    ("tree-sitter-dart/test/corpus", "Dart"),
    ("tree-sitter-elixir/test/corpus", "Elixir"),
    ("tree-sitter-go/test/corpus", "Go"),
    ("tree-sitter-haskell/test/corpus", "Haskell"),
    ("tree-sitter-hcl/test/corpus", "Hcl"),
    ("tree-sitter-html/test/corpus", "Html"),
    ("tree-sitter-java/test/corpus", "Java"),
    ("tree-sitter-javascript/test/corpus", "JavaScript"),
    ("tree-sitter-json/test/corpus", "Json"),
    ("tree-sitter-kotlin-sg/test/corpus", "Kotlin"),
    ("tree-sitter-lua/test/corpus", "Lua"),
    ("tree-sitter-md/tree-sitter-markdown/test/corpus", "Markdown"),
    ("tree-sitter-nix/corpus", "Nix"),
    ("tree-sitter-php/test/corpus", "Php"),
    ("tree-sitter-python/test/corpus", "Python"),
    ("tree-sitter-ruby/test/corpus", "Ruby"),
    ("tree-sitter-rust/test/corpus", "Rust"),
    ("tree-sitter-scala/test/corpus", "Scala"),
    ("tree-sitter-solidity/test/corpus", "Solidity"),
    ("tree-sitter-swift/test/corpus", "Swift"),
    ("tree-sitter-typescript/test/corpus", "TypeScript"),
    ("tree-sitter-cmake/test/corpus", "CMake"),
    ("tree-sitter-dockerfile/test/corpus", "Dockerfile"),
    ("tree-sitter-graphql/test/corpus", "GraphQL"),
    ("tree-sitter-ini/test/corpus", "Ini"),
    ("tree-sitter-make/test/corpus", "Make"),
    ("tree-sitter-proto/test/corpus", "Proto"),
    ("tree-sitter-jsdoc/test/corpus", "JsDoc"),
    ("tree-sitter-svelte-ng/test/corpus", "Svelte"),
    ("tree-sitter-vue/corpus", "Vue"),
    ("tree-sitter-erlang/test/corpus", "Erlang"),
    ("tree-sitter-julia/test/corpus", "Julia"),
    ("tree-sitter-objc/test/corpus", "ObjectiveC"),
    ("tree-sitter-ocaml/test/corpus", "OCaml"),
    ("tree-sitter-perl/test/corpus", "Perl"),
    ("tree-sitter-powershell/test/corpus", "PowerShell"),
    ("tree-sitter-r/test/corpus", "R"),
    ("tree-sitter-sequel/test/corpus", "Sql"),
    ("tree-sitter-toml-ng/test/corpus", "Toml"),
    ("tree-sitter-xml/test/corpus", "Xml"),
    ("tree-sitter-yaml/test/corpus", "Yaml"),
]

# language -> (extension, routing filename or None); primary extensions from
# crates/language/src/lib.rs langs! block.
EXT = {
    "Bash": ("sh", None), "C": ("c", None), "CMake": ("cmake", "CMakeLists.txt"),
    "Erlang": ("erl", None), "Cpp": ("cc", None), "CSharp": ("cs", None),
    "Css": ("css", None), "Dart": ("dart", None),
    "Dockerfile": ("dockerfile", "Dockerfile"), "Elixir": ("ex", None),
    "Go": ("go", None), "GraphQL": ("graphql", None), "Haskell": ("hs", None),
    "Hcl": ("hcl", None), "Html": ("html", None), "Ini": ("ini", None),
    "Java": ("java", None), "JavaScript": ("cjs", None), "Julia": ("jl", None),
    "JsDoc": ("jsdoc", None), "Json": ("json", None), "Kotlin": ("kt", None),
    "Lua": ("lua", None), "Make": ("mk", "Makefile"),
    "Markdown": ("markdown", None), "Astro": ("astro", None),
    "ObjectiveC": ("m", None), "OCaml": ("ml", None), "Nix": ("nix", None),
    "Php": ("php", None), "Proto": ("proto", None), "Perl": ("pl", None),
    "PowerShell": ("ps1", None), "Python": ("py", None), "R": ("r", None),
    "Ruby": ("rb", None), "Rust": ("rs", None), "Scala": ("scala", None),
    "Svelte": ("svelte", None), "Sql": ("sql", None), "Solidity": ("sol", None),
    "Swift": ("swift", None), "Toml": ("toml", None), "Tsx": ("tsx", None),
    "TypeScript": ("ts", None), "Xml": ("xml", None), "Zig": ("zig", None),
    "Vue": ("vue", None), "Yaml": ("yml", None),
}

HEADER = re.compile(r"^=+$")
SEP = re.compile(r"^-+$")


def parse_corpus(text):
    """Yield (attrs, code) per test."""
    lines = text.split("\n")
    i, n = 0, len(lines)
    while i < n:
        if not HEADER.match(lines[i]):
            i += 1
            continue
        i += 1  # past opening =-line
        attrs = []
        while i < n and not HEADER.match(lines[i]):
            if lines[i].startswith(":"):
                attrs.append(lines[i].strip())
            i += 1
        i += 1  # past closing =-line
        code = []
        while i < n and not SEP.match(lines[i]):
            # a new header while scanning code means the test had no
            # expectation section — emit what we have and restart there
            if HEADER.match(lines[i]):
                break
            code.append(lines[i])
            i += 1
        yield attrs, "\n".join(code).strip("\n")
        # skip expectation until next header
        while i < n and not HEADER.match(lines[i]):
            i += 1


def main():
    os.makedirs(OUT, exist_ok=True)
    per_lang_bytes = {}
    per_lang_files = {}
    for rel, lang in ROOTS:
        root = os.path.join(REPO, "grammars", rel)
        if not os.path.isdir(root):
            print(f"MISSING corpus dir: {rel}", file=sys.stderr)
            continue
        for fn in sorted(os.listdir(root)):
            if not fn.endswith(".txt"):
                continue
            text = open(os.path.join(root, fn), encoding="utf-8", errors="replace").read()
            buckets = {}
            for attrs, code in parse_corpus(text):
                if not code or any(a.startswith((":skip", ":error")) for a in attrs):
                    continue
                target = lang
                for a in attrs:
                    if a.startswith(":language("):
                        inner = a[len(":language("):].rstrip(")").strip()
                        target = {
                            "typescript": "TypeScript", "tsx": "Tsx",
                            "javascript": "JavaScript", "php": "Php",
                        }.get(inner, lang)
                buckets.setdefault(target, []).append(code)
            for target, codes in buckets.items():
                ext, fname = EXT[target]
                d = os.path.join(OUT, target)
                os.makedirs(d, exist_ok=True)
                stem = os.path.splitext(fn)[0]
                out_name = fname if fname else f"{stem}.{ext}"
                if fname:  # routing-filename languages: one dir per corpus file
                    d = os.path.join(d, stem)
                    os.makedirs(d, exist_ok=True)
                body = "\n\n".join(codes) + "\n"
                with open(os.path.join(d, out_name), "w", encoding="utf-8") as f:
                    f.write(body)
                per_lang_bytes[target] = per_lang_bytes.get(target, 0) + len(body)
                per_lang_files[target] = per_lang_files.get(target, 0) + 1
    for lang in sorted(per_lang_bytes):
        print(f"{lang}: {per_lang_files[lang]} files, {per_lang_bytes[lang]} bytes")
    missing = sorted(set(EXT) - set(per_lang_bytes))
    print(f"missing (no corpus): {missing}", file=sys.stderr)


if __name__ == "__main__":
    main()

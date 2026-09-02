---
name: vorpal-structural
description: Structural code search and rewrite with vorpal run/scan — match by AST pattern instead of regex, rewrite matches, filter by node kind, scan whole trees with YAML rules, emit JSON/SARIF/GitHub annotations. Use for find/replace that must respect syntax, codemods, and lint-style sweeps.
---

# Structural search & rewrite

## One-shot pattern search: `vorpal run`

```
vorpal run -p 'console.log($ARG)' src/
vorpal run -p 'kfree($PTR);' --lang c ~/linux        # C/C++ calls need statement form
vorpal run -p 'foo($$$ARGS)' -r 'bar($$$ARGS)' -i    # rewrite, interactive review
```

- `$NAME` matches one node and captures it; `$$$NAME` matches zero-or-more (args, body).
- `-r/--rewrite` renders replacements using captured metavars. Nothing is written unless
  you confirm in `-i/--interactive` or pass `-U/--update-all`.
- `--lang LANG` forces the pattern language (required with `--stdin`).
- `--selector KIND` matches a sub-part of a larger pattern; `--strictness
  cst|smart|ast|relaxed|signature|template` tunes how literally trivia/kinds must match.
- `-k/--kind KIND` matches by node kind with ESQuery-style selectors instead of a pattern.
- `--globs`, `--no-ignore`, `--follow`, `-j N` control traversal; `--files-with-matches`
  lists paths only.
- `--json[=pretty|stream|compact]` for machine output; `--debug-query[=ast|cst|sexp|pattern]`
  shows how your pattern parsed (rule debugging gold).

## Tree-wide rule scanning: `vorpal scan`

```
vorpal scan --rule rule.yml src/
vorpal scan --inline-rules "$(cat <<'YML'
id: no-console
language: typescript
severity: warning
message: no console.log in production code
rule: { pattern: console.log($A) }
YML
)"
```

- Runs the FULL YAML rule model (composite/relational rules, constraints, utils,
  transforms, fix) — see the vorpal-rules skill for authoring.
- `--format github` (workflow annotations) or `--format sarif`; `--report-style
  rich|medium|short`; `--json` with `--include-metadata`.
- Rules with `fix:` rewrite under `-i` / `-U`, exactly like `run -r`.

## Choosing the tool

- Know the shape, one-off → `run -p`.
- Reusable/policy/multi-rule/CI → `scan` + rule files (+ `vorpal test` snapshots).
- Want matches ranked by importance in an indexed repo → `vorpal search --code PATTERN`
  (see vorpal-search).

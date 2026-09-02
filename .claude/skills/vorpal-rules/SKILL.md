---
name: vorpal-rules
description: Author, scaffold, and test vorpal YAML rules — atomic patterns, kinds and regex, composite all/any/not, relational inside/has, constraints, utils, transforms and fixes, plus snapshot testing with vorpal test and project scaffolding with vorpal new. Use when building reusable lint/codemod rules.
---

# Rule authoring & testing

## Anatomy of a rule

```yaml
id: unique-rule-id
language: rust          # any of the 49 built-in grammars (vorpal grammars lists them)
severity: warning       # error | warning | info | hint | off
message: what the finding means
note: longer guidance (optional)
rule:                   # the matcher — compose freely:
  all:
    - pattern: $FN($$$ARGS)
    - inside: { kind: function_item }     # relational: inside / has / follows / precedes
    - not: { regex: "^test_" }
constraints:            # per-metavariable refinement
  FN: { regex: "^unwrap|expect$" }
utils:                  # named sub-rules, referenced via { matches: util-name }
fix: $FN($$$ARGS).context("...")?        # optional rewrite template
```

Run one rule without a project: `vorpal scan --rule my-rule.yml paths/`
Run ad-hoc: `vorpal scan --inline-rules '<yaml>'` (multiple docs separated by `---`).

## Project layout

`vorpal new project` scaffolds `vorpalconfig.yml` + rule/test/util directories;
`vorpal new rule|test|util NAME` adds items. The config's `ruleDirs` make plain
`vorpal scan` pick up everything.

## Testing rules

```
vorpal test                      # runs test YAMLs against snapshots
vorpal test -U                   # update changed snapshots after reviewing
vorpal test --skip-snapshot-tests  # validity-only pass
```

Test files pair `valid:` (must not match) and `invalid:` (must match) code cases; the
snapshot dir (default `__snapshots__`) records expected outputs. `-i` reviews
interactively; `-f REGEX` filters which tests run.

## Debugging a rule that won't match

1. `vorpal run -p '<pattern>' --lang L --debug-query=ast` — see what the pattern parsed as.
2. `vorpal run --stdin -p '<pattern>' --lang L <<< 'code'` — minimal reproduction.
3. Loosen `--strictness` (smart → relaxed → signature) to find where matching diverges.
4. Remember: statement-position patterns in C-likes need the trailing `;`.

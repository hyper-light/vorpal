from __future__ import annotations

from typing import List, TypedDict,  Literal, Dict, Union, Mapping, Optional
from .vorpal_py import (
    BuildReport,
    Edit,
    Index,
    NodeInfo,
    Pos,
    Range,
    SgNode,
    SgRoot,
    index_build,
    index_build_report,
    index_graph,
    index_node,
    index_search,
    register_dynamic_language,
    # Awaitable repository API — native coroutines backed by a Rust-owned worker pool
    # (crates/pyo3/src/async_bridge.rs). `await vorpal.search(...)`, `await vorpal.build(...)`.
    # Each releases the GIL for the whole native call and resolves its asyncio.Future from
    # the pool, so concurrent awaits run in genuine parallel across cores. Pool size:
    # VORPAL_ASYNC_WORKERS (default 8× cores); workers spawn lazily under real concurrency.
    build,
    build_report,
    search,
    search_many,
    node,
    graph,
)

Strictness = Union[Literal["cst"], Literal["smart"], Literal["ast"], Literal["relaxed"], Literal["signature"]]

class Pattern(TypedDict):
    selector: Optional[str]
    strictness: Optional[Strictness]
    context: str

class NthChild(TypedDict):
    position: int | str
    ofRule: Rule
    nth: int

class PosRule(TypedDict):
    line: int
    column: int

class RangeRule(TypedDict):
    start: PosRule
    end: PosRule

class RuleWithoutNot(TypedDict, total=False):
    # atomic rule
    pattern: str | Pattern
    kind: str
    regex: str
    nthChild: int | str | NthChild
    range: RangeRule

    # relational rule
    inside: "Relation" # pyright report error if forward reference here?
    has: Relation
    precedes: Relation
    follows: Relation

    # composite rule
    all: List[Rule]
    any: List[Rule]
    # cannot add here due to reserved keyword
    # not: Rule
    matches: str

# workaround
# Python's keyword requires `not` be a special case
class Rule(RuleWithoutNot, TypedDict("Not", {"not": "Rule"}, total=False)):
    pass

# Relational Rule Related
StopBy = Union[Literal["neighbor"], Literal["end"], Rule]

# Relation do NOT inherit from Rule due to pyright bug
# see tests/test_rule.py
class Relation(RuleWithoutNot, TypedDict("Not", {"not": "Rule"}, total=False), total=False):
    stopBy: StopBy
    field: str

class Config(TypedDict, total=False):
    rule: Rule
    constraints: Dict[str, Mapping]
    utils: Dict[str, Rule]
    transform: Dict[str, Mapping]

class CustomLang(TypedDict, total=False):
  library_path: str
  language_symbol: Optional[str]
  meta_var_char: Optional[str]
  expando_char: Optional[str]



__all__ = [
    "Rule",
    "Config",
    "Relation",
    "Pattern",
    "NthChild",
    "SgNode",
    "SgRoot",
    "Pos",
    "Range",
    "Edit",
    "register_dynamic_language",
    "Index",
    "NodeInfo",
    "BuildReport",
    "index_build",
    "index_build_report",
    "index_graph",
    "index_node",
    "index_search",
    # async facade
    "build",
    "build_report",
    "search",
    "search_many",
    "node",
    "graph",
]
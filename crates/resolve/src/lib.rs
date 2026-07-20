//! `vorpal-resolve` — cross-file reference resolution (§3.3), the part sylk left unsolved.
//!
//! Definitions already live in the KG (as `NodeId`s). This crate resolves **references** (call
//! sites, type uses, imports) to those definitions using precise, deterministic scoping:
//! grammar-provided qualifiers (`Kg::load`, `self.helper()`) bind against member owners and
//! module files; intra-file matches win; across files, exported symbols are visible — plus,
//! for Rust, ancestor-module privates (child modules see parent privates). Ambiguity is
//! tolerated only where the syntactic form warrants it ([`RefForm`]): bare names may take a
//! labeled approximate pick; member accesses on untyped values bind only when unique. Every
//! resolution carries a [`Confidence`] and preserves an evidence span — **approximate edges
//! are labeled, never faked** (an unresolvable reference yields no edge, only a count split
//! into *external* and *masked*).
//!
//! Resolution is edge-type-agnostic at its core: one resolver serves `calls` / `references` /
//! `imports` / `of_type`, differing only by [`RefKind`]. Feed a [`SymbolTable`] (built from a
//! [`vorpal_kg::Kg`] via [`SymbolTable::from_kg`]) plus [`Reference`]s to [`resolve_all`].

mod reference;
mod resolver;
mod table;

pub use reference::{RefForm, RefKind, Reference};
pub use resolver::{Confidence, Resolution, ResolveStats, ResolvedEdge, Resolver, resolve_all};
pub use table::{Symbol, SymbolTable};

pub use vorpal_kg::{EdgeType, NodeId, SymbolKind};

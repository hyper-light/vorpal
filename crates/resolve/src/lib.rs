//! `vorpal-resolve` — cross-file reference resolution (§3.3), the part sylk left unsolved.
//!
//! Definitions already live in the KG (as `NodeId`s). This crate resolves **references** (call
//! sites, type uses, imports) to those definitions using precise, deterministic scoping:
//! intra-file matches win; otherwise only **exported** symbols are visible across files;
//! qualified names and ambiguity are handled explicitly. Every resolution carries a
//! [`Confidence`] and preserves an evidence span — **approximate edges are labeled, never faked**
//! (an unresolvable reference yields no edge, only a count).
//!
//! Resolution is edge-type-agnostic at its core: one resolver serves `calls` / `references` /
//! `imports` / `of_type`, differing only by [`RefKind`]. Feed a [`SymbolTable`] (built from a
//! [`vorpal_kg::Kg`] via [`SymbolTable::from_kg`]) plus [`Reference`]s to [`resolve_all`].

mod reference;
mod resolver;
mod table;

pub use reference::{RefKind, Reference};
pub use resolver::{Confidence, Resolution, ResolveStats, ResolvedEdge, Resolver, resolve_all};
pub use table::{Symbol, SymbolTable};

pub use vorpal_kg::{EdgeType, NodeId, SymbolKind};

//! `vorpal-kg` — the L1→L3 bridge: turn extracted structure into a queryable knowledge graph.
//!
//! This crate wires the storage foundations together (§3.1→§3.3, §11):
//! - each entity is interned via [`vorpal_canonical`] → a dense `NodeId` (identity + dedup + skip),
//! - node attributes land in SoA columns sealed into a [`vorpal_segment`] `.vseg` (+ a string heap),
//! - containment relations are emitted as edges into a [`vorpal_graph`] graph.
//!
//! The input is [`vorpal_outline`] extraction (definitions/containment — the deterministic subset
//! available without cross-file resolution, i.e. the containment forest of §11.4). Calls/refs
//! edges arrive later behind the `Language`-trait resolver (§3.3); the assembly API is the same.
//!
//! [`KgWriter`] accumulates and [`KgWriter::seal`]s into a queryable [`Kg`].

mod kg;
mod model;
mod writer;

pub use kg::{Kg, NodeView};
pub use model::SymbolKind;
pub use writer::{KgWriter, NodeDef};

pub use vorpal_graph::EdgeType;
pub use vorpal_segment::NodeId;

//! `vorpal-graph` — the edge graph as one LSM (§9.3) plus the §9.8 locality relabel.
//!
//! The graph has a **write form** and a **read form** that compaction transforms between:
//! - **write** = an append-only [`EdgeLog`] (`edge::EdgeLog`), O(1) per edge, no whole-repo buffer.
//! - **read** = compacted **CSR (out) + CSC (in)** ([`Graph`]) built by GVEL counting-scatter
//!   (§11.2), so `callersOf` (in-edges) and `refsTo` (out-edges) each hit exactly one direction.
//!
//! [`GraphStore`] ties them together: reads merge `compacted ∪ delta`; `flush` compacts.
//!
//! [`relabel`] implements the **compaction-time `NodeId` remap** (§9.8): a deterministic
//! BFS/RCM-style locality order produces a dense [`ForwardingTable`] (`old_id → new_id`) that
//! successive relabels **compose** into a single lookup, so a stale cross-unit reference resolves
//! without a chain. Node ids here are dense `u32` locators (a segment's local id space, §9.2).

pub mod closure;
pub mod edge;
pub mod graph;
pub mod relabel;
pub mod store;

mod csr;

pub use closure::{Direction, Strategy, reachable, reachable_typed};
pub use edge::{EdgeLog, EdgeType};
pub use graph::Graph;
pub use relabel::{ForwardingTable, avg_edge_id_span, bfs_locality_order};
pub use store::GraphStore;

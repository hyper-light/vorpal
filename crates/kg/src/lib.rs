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

pub use kg::{Kg, NodeView, SymbolSelector};
pub use model::SymbolKind;
pub use writer::{KgWriter, NodeDef, layout_entity_paths};

pub use vorpal_graph::{EdgeLog, EdgeType};
pub use vorpal_segment::NodeId;

/// Phase stamp for RSS-timeline profiling, active only under `VORPAL_PHASE_TRACE`.
pub fn phase_stamp(label: &str) {
  if std::env::var_os("VORPAL_PHASE_TRACE").is_some() {
    #[cfg(feature = "alloc-stats")]
    let stats = {
      use tikv_jemalloc_ctl::{epoch, stats};
      epoch::advance().ok();
      format!(
        " [alloc={}MB active={}MB resident={}MB]",
        stats::allocated::read().unwrap_or(0) / 1048576,
        stats::active::read().unwrap_or(0) / 1048576,
        stats::resident::read().unwrap_or(0) / 1048576
      )
    };
    #[cfg(not(feature = "alloc-stats"))]
    let stats = "";
    eprintln!(
      "[phase {:.3}s] {label}{stats}",
      std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
    );
  }
}

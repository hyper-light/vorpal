//! Re-export shim: the runtime language universe moved to `vorpal-lang-registry` (F-M2) so
//! ingest/index/MCP resolve languages exactly the way the CLI scan path does — one universe,
//! one grammar-identity authority. The module stays so `crate::lang::` paths keep working.

pub use vorpal_lang_registry::*;

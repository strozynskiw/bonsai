//! Context-controls submodule: user-driven pin/drop/stub actions
//! (`apply`), the outgoing-message projection that applies them (`outgoing`),
//! the SMOL-persona capping of that projection (`smol`), the control-map
//! bookkeeping (`state`), and the `/ctx` read-model (`report`). Most
//! cross-file helpers stay `pub(super)` (visible within this submodule only);
//! the handful actually called from outside the cluster (`run_loop`,
//! `compaction`, `agent::tests`) are `pub(in crate::agent)`.

use super::*;

mod apply;
mod outgoing;
pub(in crate::agent) mod projection;
mod report;
mod smol;
mod state;

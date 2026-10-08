//! Network namespaces, bridges, TAP devices, and fabric freeze/thaw.

mod error;
mod fabric;
mod netns;
mod plan;
mod tap;

pub use error::Error;
pub use fabric::{LabNetwork, teardown};
pub use netns::DEFAULT_NETNS_DIR;
pub use plan::{Bridge, FabricPlan, IFNAME_MAX_LEN, Tap};

pub mod adapter;
pub mod blackboard;
pub mod config;
pub mod error;
pub mod mcp;
pub mod router;

pub use blackboard::{Blackboard, HandoffNote, PeerHealth, PeerRecord, TaskLogEntry};
pub use config::{MeshConfig, OctoConfig, PeerConfig};
pub use error::{OctoError, Result};
pub use router::{AskRequest, AskResponse, MeshRouter};

//! The different sub agents in Thorpal

pub mod budget;
pub mod naming;
pub mod salvage;
pub mod swarm;
pub mod tool;
pub mod workflow;

pub use workflow::{AgentConfig, Workflow};

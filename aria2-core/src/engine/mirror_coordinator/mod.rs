// Mirror Coordinator - High-level abstraction for multi-mirror download coordination.
//
// This module provides a unified interface for coordinating downloads across
// multiple mirrors, integrating server statistics, URI selection, and segment
// management into a single cohesive API.

pub mod config;
pub mod coordinator;
pub mod helpers;
#[cfg(test)]
mod tests;

// Re-export all public items so they remain accessible at the same paths.
pub use config::MirrorConfig;
pub use coordinator::MirrorCoordinator;
pub use helpers::extract_host;

//! Integrity checking pipeline for verifying downloaded data against known hashes.
//!
//! This module provides the infrastructure for verifying that downloaded file data
//! matches expected hash values. It replaces the C++ class hierarchy:
//!
//! ```text
//! C++ hierarchy                     Rust enum dispatch
//! ──────────────                    ──────────────────
//! IteratableValidator (virtual)  →  ValidatorKind (enum)
//!   └─ IteratableChunkChecksum      └─ ValidatorKind::PieceHash(PieceHashValidator)
//! CheckIntegrityEntry (virtual)  →  CheckIntegrityKind (enum)
//!   └─ PieceHashCheckIntegrityEntry  └─ CheckIntegrityKind::Stream / Bt
//!     └─ StreamCheckIntegrityEntry
//!     └─ BtCheckIntegrityEntry (implied)
//! ```
//!
//! # Design Rationale
//!
//! The C++ version uses deep inheritance + virtual dispatch for both the validator
//! and the entry hierarchy. Rust enum dispatch replaces both:
//!
//! - **No vtable overhead**: enum dispatch is zero-cost at the call site.
//! - **Exhaustive matching**: Adding a new variant requires updating all match arms,
//!   preventing the "forgot to override" bugs common in C++ virtual hierarchies.
//! - **Direct Arc<> references**: Uses `Arc<DownloadContext>` and `Arc<dyn PieceStorage>`
//!   instead of ID-based lookups, matching C++ `shared_ptr<>` semantics.
//!
//! # Interior Mutability Note
//!
//! `PieceStorage` trait methods like `mark_piece_verified` and `mark_piece_failed`
//! require `&mut self`, but the validator holds `Arc<dyn PieceStorage>` (shared
//! reference). To resolve this, `PieceHashValidator::validate_chunk()` collects
//! validation results internally, and the caller applies them via
//! `apply_validation_results()` when a `&mut dyn PieceStorage` is available.
//! This matches the C++ `shared_ptr<PieceStorage>` pattern where mutable access
//! is taken without Rust's borrow-checker enforcement.
//!
//! # C++ Reference
//!
//! - `IteratableValidator.h` — abstract async chunk-by-chunk validator interface
//! - `IteratableChunkChecksumValidator.h/.cc` — piece-hash based validator
//! - `CheckIntegrityEntry.h/.cc` — base entry for integrity checking operations
//! - `PieceHashCheckIntegrityEntry.h/.cc` — piece-hash based integrity checking
//! - `StreamCheckIntegrityEntry.h/.cc` — stream download integrity checking

pub mod bt_check;
pub mod callback;
pub mod kind;
pub mod man;
pub mod stream_check;
pub mod validator;

#[cfg(test)]
mod tests;

// Re-export all public items so that external code using
// `crate::checksum::check_integrity::X` still works.
pub use bt_check::BtCheckIntegrity;
pub use callback::{
    IntegrityFile, IntegrityFinishedAction, IntegrityIncompleteAction,
    IntegrityTrailingGarbageAction,
};
pub use kind::CheckIntegrityKind;
pub use man::{CheckIntegrityEntry, CheckIntegrityMan, CheckIntegrityTask, FileChunkValidator};
pub use stream_check::StreamCheckIntegrity;
pub use validator::{PieceHashValidator, PieceValidationResult, ValidatorKind};

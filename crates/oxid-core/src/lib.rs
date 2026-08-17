//! Oxid core domain.
//!
//! Pure business logic and entities. No I/O, no infrastructure concerns.
//! Follows the hexagonal architecture described in `SPEC.md`: adapters
//! (Docker, `SQLite`, Git, `HTTP`) live in other crates and depend on this one.

pub mod domain;

pub use domain::*;

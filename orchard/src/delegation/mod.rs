//! Delegation ZKP circuit.
//!
//! A single circuit proving all 16 conditions of the delegation ZKP,
//! including 4 per-note slots and gov null pairwise distinctness.
//! The builder layer creates padded notes for unused slots and
//! produces a single proof.

pub mod builder;
pub mod circuit;
pub mod imt;

pub use circuit::{Circuit, Instance};

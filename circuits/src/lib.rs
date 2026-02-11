//! Zally Circuits: Halo2 ZKP circuits and FFI verification layer.
//!
//! This crate provides:
//! - Circuit definitions for the Zally vote chain's three ZKP types
//! - C-compatible FFI functions for proof verification from Go via CGo
//!
//! Currently contains a toy circuit for validating the FFI pipeline.
//! Real circuits (delegation, vote commitment, vote share) will be added later.

pub mod toy;
pub mod ffi;

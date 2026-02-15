//! E2E test support library for the Zally voting API.
//!
//! This crate provides ElGamal on Pallas, HTTP client helpers, and payload
//! builders so that integration tests in `tests/` can run the full voting
//! flow without fixture files.

pub mod api;
pub mod elgamal;
pub mod payloads;
pub mod setup;

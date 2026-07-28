//! Stronghold Gateway library module tree.
//!
//! This module re-exports all submodules for the gateway binary.

// Scaffold-stage allow: many functions and structs are defined but not yet
// called. This allow will be removed in Wave 11 (Integration & E2E) when
// all modules are wired up.
#![allow(dead_code)]

pub mod audit;
pub mod anomaly;
pub mod crypto;
pub mod db;
pub mod images;
pub mod machines;
pub mod push;
pub mod routes;
pub mod sessions;
pub mod tee;
pub mod tenants;

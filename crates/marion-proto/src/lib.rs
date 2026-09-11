//! Transitional re-export shim: the wire vocabulary now lives in [`marion_core::proto`].
//!
//! This crate exists only so the move and the call-site rewrite are two reviewable commits rather
//! than one. It is deleted in the commit that rewrites the imports.

pub use marion_core::proto::*;

#![deny(missing_docs)]
//! # Rocket Multipart Streams
//!
//! Implements support for Multipart streams in Rocket. The core types are
//! `MultipartStream`, which adapts a stream of `MultipartSection`s into a
//! `multipart/mixed` response, and `MultipartReader`, which parses a multipart
//! stream into a sequence of `MultipartReadSection`s.

mod reader;
pub use reader::*;
mod writer;
pub use writer::*;
#[cfg(test)]
mod tests;


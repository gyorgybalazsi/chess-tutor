//! Core logic for chess-tutor.
//!
//! The engine calculates and Claude translates: everything Claude is told
//! comes from Stockfish output plus facts computed here with shakmaty, so
//! explanations never rest on the language model's own move calculation.

pub mod analysis;
pub mod annotate;
pub mod claude;
pub mod engine;
pub mod eval;
pub mod import;
pub mod prompt;
pub mod screenshot;
pub mod secrets;
pub mod tree;

pub use shakmaty;

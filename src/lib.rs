//! swagsh as a library: `main.rs` is a thin binary wrapper around this.
//! Exists so `parser`/`expand` (and anything else worth exercising in
//! isolation) can be linked against directly, without going through a
//! process boundary; a `cargo fuzz` target against `parser::parse` is the
//! motivating case (see `fuzz/`), but the same split is also just the
//! ordinary way to make a Rust binary's internals testable.

pub mod ast;
pub mod builtins;
pub mod cli;
pub mod env;
pub mod errfmt;
pub mod eval;
pub mod expand;
pub mod fd;
pub mod jobs;
pub mod lexer;
pub mod parser;
pub mod prompt;
pub mod repl;
pub mod signal;

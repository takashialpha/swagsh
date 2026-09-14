//! swagsh as a library: `main.rs` is a thin binary wrapper around this.
//! Exists so `parser`/`expand` (and anything else worth exercising in
//! isolation) can be linked against directly, without going through a
//! process boundary; a `cargo fuzz` target against `parser::parse` is the
//! motivating case (see `fuzz/`), but the same split is also just the
//! ordinary way to make a Rust binary's internals testable.
//!
//! Only what those two consumers actually reach is `pub`: `main.rs` needs
//! `cli`/`env`/`errfmt`/`eval`/`parser`/`repl`/`signal`, and the fuzz
//! targets need `parser` and `expand`. Everything else is `pub(crate)`, so
//! the interpreter's internals are not load-bearing API that a change has
//! to stay compatible with.

pub(crate) mod ast;
pub(crate) mod builtins;
pub mod cli;
pub mod env;
pub mod errfmt;
pub mod eval;
pub mod expand;
pub(crate) mod fd;
pub(crate) mod jobs;
pub(crate) mod lexer;
pub mod parser;
pub(crate) mod prompt;
pub mod repl;
pub mod signal;

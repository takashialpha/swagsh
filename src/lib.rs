//! swagsh as a library: `main.rs` is a thin binary wrapper around this.
//! Exists so `parser`/`expand` (and anything else worth exercising in
//! isolation) can be linked against directly, without going through a
//! process boundary; a `cargo fuzz` target against `parser::parse` is the
//! motivating case (see `fuzz/`), but the same split is also just the
//! ordinary way to make a Rust binary's internals testable.
//!
//! Only what those two consumers actually reach is `pub`: `main.rs` needs
//! `cli`/`env`/`errfmt`/`eval`/`parser`/`repl`/`signal`/`stdin_script`, and the fuzz
//! targets need `ast`, `brace`, `expand`, `parser`, `repl` and `unparse`.
//! Everything else is `pub(crate)`, so
//! the interpreter's internals are not load-bearing API that a change has
//! to stay compatible with.

pub mod ast;
pub mod brace;
pub(crate) mod builtins;
pub mod cli;
pub(crate) mod collate;
pub mod env;
pub mod errfmt;
pub mod eval;
pub mod expand;
pub(crate) mod fd;
pub(crate) mod jobs;
pub(crate) mod lexer;
pub mod paint;
pub mod parser;
pub(crate) mod pattern;
pub(crate) mod prompt;
pub mod repl;
pub mod signal;
pub mod stdin_script;
pub(crate) mod sys;
pub mod unparse;

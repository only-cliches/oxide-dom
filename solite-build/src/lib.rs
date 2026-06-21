//! JSX/TS compiler and ahead-of-time bundler for [solite](https://crates.io).
//!
//! This crate is intentionally lightweight (oxc + `relative-path`, no renderer
//! or GPU dependencies) so it can be used from a Cargo build script to transpile
//! a project ahead of time. See [`bundle`] for the build-script entry points.
//!
//! The main `solite` crate re-exports everything here, so application code can
//! keep using `solite::compile_component_source`, `solite::bundle`, etc.

mod compiler;

pub mod bundle;
pub mod workflow;

pub use compiler::{
    CompileError, compile_component_file, compile_component_source, compile_module_source,
    is_compilable_module, map_module_specifiers,
};

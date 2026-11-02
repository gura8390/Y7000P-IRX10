//! Legacy-compatible automation DTOs, matching, graph planning and startup ownership.
//! Execution is injected; importing a document cannot spawn a process or touch hardware.
mod actions;
mod dto;
mod engine;
mod ordinal;
mod triggers;

pub use actions::*;
pub use dto::*;
pub use engine::*;
pub use triggers::*;

/// Pinned .NET ordinal scalar casing; no locale, normalization or extension
/// rewriting. The generated table has an independent C# fixture and generator.
pub fn ordinal_ignore_case(left: &str, right: &str) -> bool {
    ordinal::equal(left, right)
}

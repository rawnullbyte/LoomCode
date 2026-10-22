//! Loon tool system: versioned specs, structural argument validation, bounded outputs.
//!
//! Every tool declares its JSON-Schema (structural subset), read/write effects, and
//! returns bounded output — oversized payloads are returned in full alongside an excerpt
//! so the journal can store them as blobs.

pub mod checks;
pub mod context7;
pub mod exec;
pub mod fetch;
pub mod fs_tools;
pub mod git_tools;
pub mod inspect;
pub mod monitor;
pub mod patch;
pub mod process;
pub mod registry;
pub mod schema;
pub mod search_tools;
pub mod session;
pub mod ssh;
pub mod structured;
pub mod tool;

pub use registry::ToolRegistry;
pub use tool::{validate_args, Effects, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};

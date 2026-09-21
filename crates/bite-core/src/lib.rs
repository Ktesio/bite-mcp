//! bite-core: the single domain layer both the MCP server and the CLI run
//! through, so the two surfaces cannot drift.

pub mod config;
pub mod dates;
pub mod error;
pub mod ops;
pub mod registry;

pub use error::BiteError;
pub use registry::{json_schema, tools, ParamKind, ParamSpec, Tool};

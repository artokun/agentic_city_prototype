//! Shared tool definitions and execution for all LLM providers.
//! Tool catalogs and schema compilation live here; MCP binaries become thin stdio wrappers.

pub mod catalog;
pub mod execute;
pub mod policy;
pub mod schema;

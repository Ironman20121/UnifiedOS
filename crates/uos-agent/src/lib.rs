//! The natural-language agent that operates the system.

mod agent;
mod audit;
mod risk;
mod tools;

pub use agent::{Agent, AgentConfig, AgentError};
pub use audit::{AuditEntry, AuditLog, Decision};
pub use risk::{classify_command, Approver, Policy, Risk, Verdict};
pub use tools::{default_tools, Tool, ToolError};

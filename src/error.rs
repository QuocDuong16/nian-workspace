/// Error type used inside tool implementations.
///
/// Tool-level failures are deliberately plain strings: they are rendered to
/// the MCP client as `CallToolResult::error(...)` text so that an AI client
/// can read and react to them. Rust backtraces never cross this boundary.
#[derive(Debug, Clone)]
pub struct ToolError(pub String);

impl ToolError {
    pub fn msg(message: impl Into<String>) -> Self {
        ToolError(message.into())
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ToolError {}

impl From<std::io::Error> for ToolError {
    fn from(err: std::io::Error) -> Self {
        ToolError(format!("I/O error: {err}"))
    }
}

pub type ToolResult<T> = Result<T, ToolError>;

//! MCP tool implementations. Each public `handle*` function takes the shared
//! [`AppState`](crate::config::AppState) and tool arguments, returning a JSON
//! value or a caller-friendly [`ToolError`](crate::error::ToolError).

pub mod command;
pub mod files;
pub mod git;
pub(crate) mod git_process;
pub mod patch;
pub mod search;
pub mod workspace_info;

use crate::error::ToolError;
use rmcp::model::{CallToolResult, ContentBlock};

/// Version-control metadata directories. These are never searched by the
/// search tool — not even when explicitly requested as a path (policy:
/// exposing `.git` internals adds no value and complicates the model).
///
/// Matching is ASCII case-insensitive: Windows filesystems are commonly
/// case-insensitive, so `.GIT` may be the same directory as `.git`. On
/// case-sensitive Unix filesystems this can only over-reject a directory
/// literally named `.GIT`, which is harmless.
pub(crate) const VCS_METADATA_DIRS: &[&str] = &[".git", ".hg", ".svn"];

/// Ordinary generated/build directories skipped during normal recursive
/// searches. Explicitly requesting such a path IS allowed (`node_modules`,
/// `target`, …) — clients may legitimately need to inspect dependency trees.
pub(crate) const GENERATED_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "__pycache__",
    ".venv",
    "venv",
    "dist",
    ".next",
    ".cache",
    "coverage",
];

pub(crate) fn is_vcs_metadata_dir(name: &str) -> bool {
    VCS_METADATA_DIRS
        .iter()
        .any(|d| d.eq_ignore_ascii_case(name))
}

pub(crate) fn is_generated_dir(name: &str) -> bool {
    GENERATED_DIRS.contains(&name)
}

/// Kept for listing/pruning call sites that treat both kinds alike.
pub(crate) fn is_generated_or_vcs_dir(name: &str) -> bool {
    is_vcs_metadata_dir(name) || is_generated_dir(name)
}

/// Render a successful tool response with both structured content and a
/// pretty-printed JSON text fallback (for clients that do not surface
/// `structuredContent`).
pub(crate) fn result_from_value(
    value: serde_json::Value,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let text = serde_json::to_string_pretty(&value).map_err(|e| {
        rmcp::ErrorData::internal_error(format!("failed to serialize tool output: {e}"), None)
    })?;
    let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
    result.structured_content = Some(value);
    Ok(result)
}

/// Render a tool-level failure as an error result whose message is visible
/// to the AI client (rather than an opaque protocol error).
pub(crate) fn error_result(err: ToolError) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(err.to_string())])
}

pub(crate) struct CappedBytes {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

/// Read an async stream up to `cap` bytes, continuing to drain afterwards so
/// writers never block, reporting truncation.
pub(crate) async fn read_capped<R>(mut reader: R, cap: usize) -> std::io::Result<CappedBytes>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::with_capacity(cap.min(64 * 1024));
    let mut chunk = [0u8; 16 * 1024];
    let mut truncated = false;
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        if bytes.len() < cap {
            let take = n.min(cap - bytes.len());
            bytes.extend_from_slice(&chunk[..take]);
            if take < n {
                truncated = true;
            }
        } else {
            truncated = true;
        }
    }
    Ok(CappedBytes { bytes, truncated })
}

/// Lossily convert bounded bytes to UTF-8 text, reporting replacements.
pub(crate) fn decode_lossy(bytes: &[u8]) -> (String, bool) {
    match String::from_utf8(bytes.to_vec()) {
        Ok(text) => (text, false),
        Err(_) => {
            let text = String::from_utf8_lossy(bytes).into_owned();
            // A replacement char may legitimately exist in the source; treat
            // that as lossy either way — it only affects metadata flags.
            (text, true)
        }
    }
}

/// Clip a string to at most `max_bytes` UTF-8 bytes on a character boundary,
/// appending an ellipsis when clipping occurred.
pub(crate) fn clip_line(line: &str, max_bytes: usize) -> (String, bool) {
    if line.len() <= max_bytes {
        return (line.to_string(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}[…]", &line[..end]), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_line_respects_char_boundaries() {
        let s = "héllo wörld ünïcode".to_string();
        let (clipped, was_clipped) = clip_line(&s, 5);
        assert!(was_clipped);
        assert!(clipped.ends_with("[…]"));
        assert!(clipped.len() <= 5 + "[…]".len());

        let (same, not_clipped) = clip_line(&s, 100);
        assert!(!not_clipped);
        assert_eq!(same, s);
    }

    #[test]
    fn generated_dir_detection() {
        assert!(is_generated_or_vcs_dir(".git"));
        assert!(is_generated_or_vcs_dir("node_modules"));
        assert!(!is_generated_or_vcs_dir("src"));
    }

    #[tokio::test]
    async fn capped_reader_reports_truncation() {
        let data: &[u8] = &[b'x'; 100];
        let capped = read_capped(data, 10).await.unwrap();
        assert_eq!(capped.bytes.len(), 10);
        assert!(capped.truncated);

        let capped = read_capped(b"short".as_slice(), 10).await.unwrap();
        assert_eq!(capped.bytes, b"short");
        assert!(!capped.truncated);
    }
}

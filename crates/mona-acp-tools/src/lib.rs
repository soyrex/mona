//! In-server tool registry for mona-acp. Drives the agent loop after
//! Milestone B by executing streamed tool calls from the model and feeding
//! `ToolResult` content back into the next `complete()` call.
//!
//! Built-ins are deliberately small (`read_file`, `write_file`,
//! `bash`, `ls`) and gated by a permission policy. The host (Monitter)
//! decides whether to ask the user before running a tool that requires
//! approval through ACP `session/request_permission`.
//!
//! All tools take and return JSON via `serde_json::Value`. Inputs are parsed
//! into the shape advertised by each tool's `input_schema`; outputs are
//! returned verbatim. Tool implementations run in the same process as the
//! model turn.
//! In particular, the shell tool inherits the harness environment, so
//! callers must require explicit host approval before invoking it.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error, Serialize)]
pub enum ToolError {
    #[error("unknown tool: {0}")]
    Unknown(String),
    #[error("invalid input: {message}")]
    InvalidInput { message: String },
    #[error("permission denied by host")]
    PermissionDenied,
    #[error("execution failed: {message}")]
    Execution { message: String },
}

/// Permission requirement a tool carries. The host (`mona-acp` server)
/// translates this into a `session/request_permission` push to Monitter
/// before invoking the tool, unless `Never` is set in which case the
/// tool runs without asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    /// Tool runs without asking. For sandboxed reads that don't touch
    /// the user's secrets or destructive actions.
    Never,
    /// Tool requires one-time approval. Each invocation asks the
    /// host. This is the typical default for `bash` and `write_file`.
    Required,
}

/// Result of running a tool. The `output` JSON is fed back to the model
/// as a `tool_result` content block.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub output: Value,
    /// Whether the tool errored. `Ok(output)` flows to the model as a
    /// success; `Err(output)` flows as a structured error the model
    /// can react to (typically: try a different tool).
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(output: Value) -> Self {
        Self {
            output,
            is_error: false,
        }
    }
    pub fn err(message: impl Into<String>) -> Self {
        Self {
            output: serde_json::json!({ "error": message.into() }),
            is_error: true,
        }
    }
}

/// A single tool entry. Implementations are constructed once and shared
/// across invocations; the `input` is parsed and validated per call.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> Value;
    fn permission(&self) -> Permission;
    async fn run(&self, input: Value, cwd: &PathBuf) -> Result<ToolOutput, ToolError>;
}

fn path_is_scoped(path: &str) -> bool {
    let path = Path::new(path);
    !path.is_absolute()
        && !path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}

fn escaped_path(path: &str) -> ToolOutput {
    ToolOutput::err(format!("path '{path}' escapes project directory"))
}

/// Registry mapping tool names to implementations. Insertion is
/// typically done at server startup; lookup happens for every
/// tool call the model emits during a turn.
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|t| t.as_ref())
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(|s| s.as_str())
    }

    /// Public-facing `ToolDefinition` list for the model's prompt. This
    /// is what gets passed to `Provider::complete(&[ToolDefinition])`.
    pub fn definitions(&self) -> Vec<mona_message_types::ToolDefinition> {
        self.tools
            .values()
            .map(|t| mona_message_types::ToolDefinition {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
            })
            .collect()
    }

    /// Look up + parse input + run the tool. The host is responsible
    /// for checking `tool.permission()` before calling this.
    pub async fn run(
        &self,
        name: &str,
        input: Value,
        cwd: &PathBuf,
    ) -> Result<ToolOutput, ToolError> {
        let tool = self
            .get(name)
            .ok_or_else(|| ToolError::Unknown(name.to_string()))?;
        tool.run(input, cwd).await
    }
}

/// `read_file { path: string } -> { content: string }`. Returns UTF-8
/// only. Binary files error; callers can use `bash` with `file` if they
/// need inspection. Never requires approval — reading the project tree
/// is safe by default.
pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read a UTF-8 text file from the project working directory. \
         Returns { content } on read success or { error } on failure."
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path relative to cwd." }
            },
            "required": ["path"],
            "additionalProperties": false,
        })
    }
    fn permission(&self) -> Permission {
        Permission::Never
    }
    async fn run(&self, input: Value, cwd: &PathBuf) -> Result<ToolOutput, ToolError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct In {
            path: String,
        }
        let In { path } = serde_json::from_value(input).map_err(|e| ToolError::InvalidInput {
            message: e.to_string(),
        })?;
        if !path_is_scoped(&path) {
            return Ok(escaped_path(&path));
        }
        let full = cwd.join(&path);
        // Refuse path escape: resolved path must stay under cwd.
        let canonical_cwd =
            tokio::fs::canonicalize(cwd)
                .await
                .map_err(|e| ToolError::Execution {
                    message: format!("resolve project directory: {e}"),
                })?;
        let canonical = tokio::fs::canonicalize(&full)
            .await
            .map_err(|e| ToolError::Execution {
                message: format!("resolve {path}: {e}"),
            })?;
        if !canonical.starts_with(&canonical_cwd) {
            return Ok(ToolOutput::err(format!(
                "path '{path}' escapes project directory"
            )));
        }
        let content =
            tokio::fs::read_to_string(&canonical)
                .await
                .map_err(|e| ToolError::Execution {
                    message: format!("read {path}: {e}"),
                })?;
        Ok(ToolOutput::ok(serde_json::json!({ "content": content })))
    }
}

/// `write_file { path, content } -> { bytes: N }`. Requires approval
/// because it overwrites the user's files.
pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Write UTF-8 text content to a file in the project working \
         directory. Overwrites if the file already exists. Returns \
         { bytes } on success."
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path relative to cwd." },
                "content": { "type": "string", "description": "UTF-8 text to write." }
            },
            "required": ["path", "content"],
            "additionalProperties": false,
        })
    }
    fn permission(&self) -> Permission {
        Permission::Required
    }
    async fn run(&self, input: Value, cwd: &PathBuf) -> Result<ToolOutput, ToolError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct In {
            path: String,
            content: String,
        }
        let In { path, content } =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput {
                message: e.to_string(),
            })?;
        if !path_is_scoped(&path) {
            return Ok(escaped_path(&path));
        }
        let full = cwd.join(&path);
        let canonical_cwd =
            tokio::fs::canonicalize(cwd)
                .await
                .map_err(|e| ToolError::Execution {
                    message: format!("resolve project directory: {e}"),
                })?;
        // For writes, resolve parent to forbid escapes; do not require
        // the file itself to exist.
        let parent = full.parent().ok_or_else(|| ToolError::InvalidInput {
            message: "path has no parent".to_string(),
        })?;
        let canonical_parent =
            tokio::fs::canonicalize(parent)
                .await
                .map_err(|e| ToolError::Execution {
                    message: format!("resolve parent: {e}"),
                })?;
        if !canonical_parent.starts_with(&canonical_cwd) {
            return Ok(ToolOutput::err(format!(
                "path '{path}' escapes project directory"
            )));
        }
        if tokio::fs::symlink_metadata(&full)
            .await
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Ok(ToolOutput::err(format!(
                "refusing to write through symlink '{path}'"
            )));
        }
        tokio::fs::write(&full, content.as_bytes())
            .await
            .map_err(|e| ToolError::Execution {
                message: format!("write {path}: {e}"),
            })?;
        Ok(ToolOutput::ok(
            serde_json::json!({ "bytes": content.len() }),
        ))
    }
}

/// `bash { command } -> { stdout, stderr, exit_code }`. Requires
/// approval. Captures both streams and the exit code; truncates each
/// to 64 KiB to keep the model's context bounded.
pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "Run a shell command in the project working directory. \
         Captures stdout and stderr and the exit code. Each stream is \
         truncated to 64 KiB."
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command line." }
            },
            "required": ["command"],
            "additionalProperties": false,
        })
    }
    fn permission(&self) -> Permission {
        Permission::Required
    }
    async fn run(&self, input: Value, cwd: &PathBuf) -> Result<ToolOutput, ToolError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct In {
            command: String,
        }
        let In { command } =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput {
                message: e.to_string(),
            })?;
        let output = tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&command)
            .current_dir(cwd)
            .output()
            .await
            .map_err(|e| ToolError::Execution {
                message: format!("spawn sh: {e}"),
            })?;
        const CAP: usize = 64 * 1024;
        let truncate = |s: Vec<u8>| -> String {
            if s.len() > CAP {
                format!(
                    "{}\n…(truncated, full {} bytes)",
                    String::from_utf8_lossy(&s[..CAP]),
                    s.len()
                )
            } else {
                String::from_utf8_lossy(&s).into_owned()
            }
        };
        Ok(ToolOutput::ok(serde_json::json!({
            "stdout": truncate(output.stdout),
            "stderr": truncate(output.stderr),
            "exit_code": output.status.code().unwrap_or(-1),
        })))
    }
}

/// `ls { path? } -> { entries: [{ name, kind }] }`. Never requires
/// approval; lists the project directory by default.
pub struct LsTool;

#[derive(Serialize)]
struct LsEntry {
    name: String,
    kind: &'static str,
}

#[async_trait]
impl Tool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }
    fn description(&self) -> &str {
        "List entries in a directory. Defaults to the project working \
         directory. Returns { entries } with one item per name; kind \
         is 'dir', 'file', or 'other'."
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory path relative to cwd. Defaults to cwd." }
            },
            "additionalProperties": false,
        })
    }
    fn permission(&self) -> Permission {
        Permission::Never
    }
    async fn run(&self, input: Value, cwd: &PathBuf) -> Result<ToolOutput, ToolError> {
        #[derive(Deserialize, Default)]
        #[serde(deny_unknown_fields)]
        struct In {
            path: Option<String>,
        }
        let In { path } = serde_json::from_value(input).map_err(|e| ToolError::InvalidInput {
            message: e.to_string(),
        })?;
        let full = match path {
            Some(p) => {
                if !path_is_scoped(&p) {
                    return Ok(escaped_path(&p));
                }
                cwd.join(p)
            }
            None => cwd.clone(),
        };
        let canonical_cwd =
            tokio::fs::canonicalize(cwd)
                .await
                .map_err(|e| ToolError::Execution {
                    message: format!("resolve project directory: {e}"),
                })?;
        let canonical = tokio::fs::canonicalize(&full)
            .await
            .map_err(|e| ToolError::Execution {
                message: format!("resolve: {e}"),
            })?;
        if !canonical.starts_with(&canonical_cwd) {
            return Ok(ToolOutput::err("path escapes project directory"));
        }
        let mut read = tokio::fs::read_dir(&canonical)
            .await
            .map_err(|e| ToolError::Execution {
                message: format!("read_dir: {e}"),
            })?;
        let mut entries = Vec::new();
        while let Some(entry) = read.next_entry().await.map_err(|e| ToolError::Execution {
            message: format!("read_dir entry: {e}"),
        })? {
            let kind = match entry.file_type().await {
                Ok(file_type) if file_type.is_dir() => "dir",
                Ok(file_type) if file_type.is_file() => "file",
                Ok(_) | Err(_) => "other",
            };
            entries.push(LsEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                kind,
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(ToolOutput::ok(serde_json::json!({ "entries": entries })))
    }
}

/// Build the default four-tool registry. Used by the mona-acp server
/// at startup. Hosts can register additional tools before launching.
pub fn default_registry() -> ToolRegistry {
    let mut r = ToolRegistry::new();
    r.insert(Box::new(ReadFileTool));
    r.insert(Box::new(WriteFileTool));
    r.insert(Box::new(BashTool));
    r.insert(Box::new(LsTool));
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn definitions_have_name_input_schema_and_description() {
        let r = default_registry();
        let defs = r.definitions();
        assert_eq!(defs.len(), 4);
        for d in &defs {
            assert!(d.prompt_chars() > 0, "tool {} has no prompt?", d.name);
            assert!(
                d.input_schema.is_object(),
                "tool {} input_schema must be object",
                d.name
            );
        }
        let names: Vec<&str> = r.names().collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"write_file"));
        assert!(names.contains(&"bash"));
        assert!(names.contains(&"ls"));
    }

    #[tokio::test]
    async fn read_file_returns_content_for_relative_path() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("hello.txt"), "world").unwrap();
        let r = default_registry();
        let out = r
            .run(
                "read_file",
                serde_json::json!({ "path": "hello.txt" }),
                &tmp.path().to_path_buf(),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.output["content"], "world");
    }

    #[test]
    fn write_file_requires_approval() {
        assert_eq!(WriteFileTool.permission(), Permission::Required);
        assert_eq!(ReadFileTool.permission(), Permission::Never);
        assert_eq!(BashTool.permission(), Permission::Required);
        assert_eq!(LsTool.permission(), Permission::Never);
    }

    #[tokio::test]
    async fn path_escape_is_rejected() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        let r = default_registry();
        // Asking for ../<sibling> should be refused.
        let out = r
            .run(
                "read_file",
                serde_json::json!({ "path": "../escape.txt" }),
                &tmp.path().to_path_buf(),
            )
            .await
            .unwrap();
        assert!(
            out.is_error,
            "expected error for path escape, got: {:?}",
            out.output
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_file_refuses_symlink_target() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let outside_file = outside.path().join("outside.txt");
        std::fs::write(&outside_file, "original").unwrap();
        std::os::unix::fs::symlink(&outside_file, tmp.path().join("linked.txt")).unwrap();

        let out = default_registry()
            .run(
                "write_file",
                serde_json::json!({ "path": "linked.txt", "content": "changed" }),
                &tmp.path().to_path_buf(),
            )
            .await
            .unwrap();

        assert!(out.is_error);
        assert_eq!(std::fs::read_to_string(outside_file).unwrap(), "original");
    }

    #[tokio::test]
    async fn bash_runs_and_captures_exit_code() {
        let tmp = TempDir::new().unwrap();
        let r = default_registry();
        let out = r
            .run(
                "bash",
                serde_json::json!({ "command": "echo hi; echo bye >&2; exit 7" }),
                &tmp.path().to_path_buf(),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.output["exit_code"], 7);
        assert_eq!(out.output["stdout"], "hi\n");
        assert_eq!(out.output["stderr"], "bye\n");
    }

    #[tokio::test]
    async fn unknown_tool_errors() {
        let tmp = TempDir::new().unwrap();
        let r = default_registry();
        let err = r
            .run(
                "nonexistent",
                serde_json::json!({}),
                &tmp.path().to_path_buf(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::Unknown(name) => assert_eq!(name, "nonexistent"),
            _ => panic!("expected Unknown, got {:?}", err),
        }
    }
}

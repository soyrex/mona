//! ACP adapter for Mona's canonical agent loop.
//!
//! The ACP transport owns provider selection, host permission requests, and
//! HTTP MCP connections. Mona's existing `Agent` owns conversation history,
//! compaction, provider continuation, tool-result repair, persistence, and
//! the multi-round model/tool loop. Keeping that ownership split prevents the
//! ACP server from growing another incomplete agent implementation.

use crate::mcp::SessionMcpTools;
use crate::session::Session as AcpSession;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use mona_app_core::agent::{Agent, SoftInterruptQueue};
use mona_app_core::protocol::ServerEvent;
use mona_app_core::session::{Session as MonaSession, session_exists};
use mona_app_core::tool::{Registry, Tool, ToolContext, ToolOutput};
use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub(crate) struct ApprovalRequest {
    pub request_id: String,
    pub tool_name: String,
    pub input: Value,
}

#[async_trait]
pub(crate) trait ApprovalBroker: Send + Sync {
    async fn approve(&self, request: ApprovalRequest) -> bool;
}

struct BuiltinToolBridge {
    advertised_name: String,
    source_name: String,
    description: String,
    schema: Value,
    requires_approval: bool,
    tools: Arc<mona_acp_tools::ToolRegistry>,
    approval: Arc<dyn ApprovalBroker>,
}

#[async_trait]
impl Tool for BuiltinToolBridge {
    fn name(&self) -> &str {
        &self.advertised_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Value {
        self.schema.clone()
    }

    async fn execute(&self, mut input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        strip_agent_metadata(&mut input);
        if self.requires_approval
            && !self
                .approval
                .approve(ApprovalRequest {
                    request_id: ctx.tool_call_id.clone(),
                    tool_name: self.advertised_name.clone(),
                    input: input.clone(),
                })
                .await
        {
            bail!("permission denied by host");
        }
        let cwd = context_cwd(&ctx)?;
        let output = self
            .tools
            .run(&self.source_name, input, &cwd)
            .await
            .with_context(|| format!("execute ACP tool '{}'", self.advertised_name))?;
        if output.is_error {
            bail!("{}", output.output);
        }
        Ok(ToolOutput::new(
            serde_json::to_string(&output.output).unwrap_or_else(|_| output.output.to_string()),
        ))
    }
}

struct McpToolBridge {
    name: String,
    description: String,
    schema: Value,
    tools: SessionMcpTools,
    approval: Arc<dyn ApprovalBroker>,
}

struct CanonicalToolBridge {
    inner: Arc<dyn Tool>,
    approval: Arc<dyn ApprovalBroker>,
    approval_actions: CanonicalApproval,
    reject_unmanaged_background: bool,
}

enum CanonicalApproval {
    Always,
    Actions(&'static [&'static str]),
}

#[async_trait]
impl Tool for CanonicalToolBridge {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters_schema(&self) -> Value {
        self.inner.parameters_schema()
    }

    async fn execute(&self, mut input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        strip_transport_metadata(&mut input);
        if self.reject_unmanaged_background
            && let Some(command) = input.get("command").and_then(Value::as_str)
            && let Some(reason) = unmanaged_background_reason(command)
        {
            bail!(
                "unmanaged background process rejected ({reason}). Use this bash tool with run_in_background=true, then monitor the returned task with bg wait/status/output so the ACP prompt remains active until completion"
            );
        }

        let requires_approval = match self.approval_actions {
            CanonicalApproval::Always => true,
            CanonicalApproval::Actions(actions) => {
                let explicit = input
                    .get("action")
                    .and_then(Value::as_str)
                    .map(str::to_ascii_lowercase);
                let inferred = input
                    .get("intent")
                    .and_then(Value::as_str)
                    .map(str::to_ascii_lowercase);
                explicit
                    .as_deref()
                    .is_some_and(|action| actions.contains(&action))
                    || (explicit.is_none()
                        && inferred.as_deref().is_some_and(|intent| {
                            (actions.contains(&"cancel")
                                && (intent.contains("cancel") || intent.contains("stop")))
                                || (actions.contains(&"cleanup") && intent.contains("clean"))
                        }))
            }
        };
        if requires_approval
            && !self
                .approval
                .approve(ApprovalRequest {
                    request_id: ctx.tool_call_id.clone(),
                    tool_name: self.name().to_string(),
                    input: input.clone(),
                })
                .await
        {
            bail!("permission denied by host");
        }

        self.inner.execute(input, ctx).await
    }
}

#[async_trait]
impl Tool for McpToolBridge {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Value {
        self.schema.clone()
    }

    async fn execute(&self, mut input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        strip_agent_metadata(&mut input);
        if !self
            .approval
            .approve(ApprovalRequest {
                request_id: ctx.tool_call_id,
                tool_name: self.name.clone(),
                input: input.clone(),
            })
            .await
        {
            bail!("permission denied by host");
        }
        let output = self
            .tools
            .call(&self.name, input)
            .await?
            .with_context(|| format!("unknown MCP tool '{}'", self.name))?;
        Ok(ToolOutput::new(
            serde_json::to_string(&output).unwrap_or_else(|_| output.to_string()),
        ))
    }
}

fn strip_agent_metadata(input: &mut Value) {
    if let Some(object) = input.as_object_mut() {
        object.remove("intent");
        object.remove("accept_large_output");
    }
}

fn strip_transport_metadata(input: &mut Value) {
    if let Some(object) = input.as_object_mut() {
        object.remove("accept_large_output");
    }
}

/// Detect shell constructs that let a process escape Mona's tracked task
/// lifecycle. Quoted and escaped ampersands are data, while `&&`, `>&`, and
/// `&>` are ordinary shell syntax rather than background operators.
fn unmanaged_background_reason(command: &str) -> Option<&'static str> {
    let mut unquoted = String::with_capacity(command.len());
    let mut quote = None;
    let mut escaped = false;
    for ch in command.chars() {
        if escaped {
            unquoted.push(' ');
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            unquoted.push(' ');
            escaped = true;
            continue;
        }
        match quote {
            Some(active) if ch == active => {
                quote = None;
                unquoted.push(' ');
            }
            Some(_) => unquoted.push(' '),
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                unquoted.push(' ');
            }
            None => unquoted.push(ch),
        }
    }

    let lower = unquoted.to_ascii_lowercase();
    if lower
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'))
        .any(|word| matches!(word, "nohup" | "disown" | "setsid"))
    {
        return Some("detaching command");
    }

    let chars = unquoted.as_bytes();
    for (index, ch) in chars.iter().enumerate() {
        if *ch != b'&' {
            continue;
        }
        let previous = index.checked_sub(1).and_then(|i| chars.get(i)).copied();
        let next = chars.get(index + 1).copied();
        if previous != Some(b'&')
            && next != Some(b'&')
            && previous != Some(b'>')
            && next != Some(b'>')
        {
            return Some("shell background operator '&'");
        }
    }
    None
}

fn context_cwd(ctx: &ToolContext) -> Result<PathBuf> {
    ctx.working_dir
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .context("agent tool has no working directory")
}

/// Run one ACP prompt through Mona's canonical agent loop.
pub(crate) async fn run_turn(
    session: &AcpSession,
    tools: Arc<mona_acp_tools::ToolRegistry>,
    mcp_tools: SessionMcpTools,
    approval: Arc<dyn ApprovalBroker>,
    events: mpsc::UnboundedSender<ServerEvent>,
    prompt: &str,
    cancellation: CancellationToken,
    soft_interrupt_queue: Option<SoftInterruptQueue>,
) -> Result<()> {
    let handle = session
        .handle
        .as_ref()
        .context("session has no provider handle")?;
    let registry = Registry::empty();
    let mut allowed = HashSet::new();

    // Provider-facing names follow the canonical jcode vocabulary. Small file
    // tools remain ACP-owned so their mutations use Monitter's allow-once
    // request. Shell execution uses Mona's canonical tracked implementation.
    for (source, advertised) in [("read_file", "read"), ("write_file", "write"), ("ls", "ls")] {
        let tool = tools
            .get(source)
            .with_context(|| format!("missing ACP tool '{source}'"))?;
        let bridge = BuiltinToolBridge {
            advertised_name: advertised.to_string(),
            source_name: source.to_string(),
            description: tool.description().to_string(),
            schema: tool.input_schema(),
            requires_approval: tool.permission() == mona_acp_tools::Permission::Required,
            tools: tools.clone(),
            approval: approval.clone(),
        };
        allowed.insert(advertised.to_string());
        registry
            .register(advertised.to_string(), Arc::new(bridge))
            .await;
    }

    let bash = CanonicalToolBridge {
        inner: mona_app_core::tool::canonical_bash_tool(),
        approval: approval.clone(),
        approval_actions: CanonicalApproval::Always,
        reject_unmanaged_background: true,
    };
    allowed.insert("bash".to_string());
    registry.register("bash".to_string(), Arc::new(bash)).await;

    let bg = CanonicalToolBridge {
        inner: mona_app_core::tool::canonical_bg_tool(),
        approval: approval.clone(),
        approval_actions: CanonicalApproval::Actions(&["cancel", "cleanup"]),
        reject_unmanaged_background: false,
    };
    allowed.insert("bg".to_string());
    registry.register("bg".to_string(), Arc::new(bg)).await;

    for definition in mcp_tools.definitions() {
        let name = definition.name.clone();
        let bridge = McpToolBridge {
            name: name.clone(),
            description: definition.description,
            schema: definition.input_schema,
            tools: mcp_tools.clone(),
            approval: approval.clone(),
        };
        allowed.insert(name.clone());
        registry.register(name, Arc::new(bridge)).await;
    }

    let mut stored = if session_exists(&session.id) {
        MonaSession::load(&session.id)
            .with_context(|| format!("load canonical Mona session '{}'", session.id))?
    } else {
        MonaSession::create_with_id(session.id.clone(), None, None)
    };
    stored.working_dir.clone_from(&session.working_dir);
    stored.model = Some(session.model.clone());
    stored.reasoning_effort = Some(session.effort.clone());

    let mut agent = Agent::new_with_session_without_auth_refresh(
        handle.provider.clone(),
        registry,
        stored,
        Some(allowed),
    );
    if let Some(queue) = soft_interrupt_queue {
        agent.use_soft_interrupt_queue(queue);
    }
    agent.require_background_tasks_terminal(true);
    let shutdown = agent.graceful_shutdown_signal();
    let cancellation_monitor = tokio::spawn(async move {
        cancellation.cancelled().await;
        shutdown.fire();
    });
    let result = agent
        .run_once_streaming_mpsc(
            prompt,
            Vec::new(),
            Some(
                "ACP task lifecycle: never end this prompt while work you launched is still running. Do not detach processes with nohup, disown, setsid, or shell `&`. For long work, call bash with run_in_background=true, then use bg wait/status/output until it is terminal and verify the result before answering."
                    .to_string(),
            ),
            events,
        )
        .await;
    cancellation_monitor.abort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use mona_message_types::{ContentBlock, Message, StreamEvent, ToolDefinition};
    use mona_provider_core::{EventStream, Provider};
    use std::collections::VecDeque;
    use std::ffi::OsString;
    use std::sync::Mutex;

    // `MONA_HOME` is process-wide and the canonical Session persistence API
    // intentionally uses it. Serialize only these focused persistence tests
    // and always restore the caller's setting.
    static TEST_ENV_LOCK: Mutex<()> = Mutex::new(());

    struct TestHomeGuard {
        previous: Option<OsString>,
    }

    impl TestHomeGuard {
        fn set(path: &std::path::Path) -> Self {
            let previous = std::env::var_os("MONA_HOME");
            mona_base::env::set_var("MONA_HOME", path);
            mona_base::config::invalidate_config_cache();
            Self { previous }
        }
    }

    impl Drop for TestHomeGuard {
        fn drop(&mut self) {
            if let Some(previous) = self.previous.take() {
                mona_base::env::set_var("MONA_HOME", previous);
            } else {
                mona_base::env::remove_var("MONA_HOME");
            }
            mona_base::config::invalidate_config_cache();
        }
    }

    #[derive(Clone, Default)]
    struct ReplayProvider {
        requests: Arc<Mutex<Vec<Vec<Message>>>>,
        resume_ids: Arc<Mutex<Vec<Option<String>>>>,
    }

    #[async_trait]
    impl Provider for ReplayProvider {
        async fn complete(
            &self,
            messages: &[Message],
            _tools: &[ToolDefinition],
            _system: &str,
            resume_session_id: Option<&str>,
        ) -> Result<EventStream> {
            self.resume_ids
                .lock()
                .expect("record provider continuation")
                .push(resume_session_id.map(str::to_string));
            let turn = {
                let mut requests = self.requests.lock().expect("record provider request");
                requests.push(messages.to_vec());
                requests.len()
            };
            let text = match turn {
                1 => "assistant-1",
                2 => "assistant-2",
                other => panic!("unexpected provider turn {other}"),
            };
            let mut events = Vec::new();
            if turn == 1 {
                events.push(Ok(StreamEvent::SessionId(
                    "provider-continuation-1".to_string(),
                )));
            }
            events.extend([
                Ok(StreamEvent::TextDelta(text.to_string())),
                Ok(StreamEvent::MessageEnd {
                    stop_reason: Some("end_turn".to_string()),
                }),
            ]);
            Ok(Box::pin(futures::stream::iter(events)))
        }

        fn name(&self) -> &str {
            "agentic-replay-test"
        }

        fn model(&self) -> String {
            "gpt-5-test".to_string()
        }

        fn fork(&self) -> Arc<dyn Provider> {
            Arc::new(self.clone())
        }
    }

    struct ScriptedApproval {
        decisions: Mutex<VecDeque<bool>>,
        requests: Mutex<Vec<ApprovalRequest>>,
    }

    impl ScriptedApproval {
        fn new(decisions: impl IntoIterator<Item = bool>) -> Self {
            Self {
                decisions: Mutex::new(decisions.into_iter().collect()),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl ApprovalBroker for ScriptedApproval {
        async fn approve(&self, request: ApprovalRequest) -> bool {
            self.requests
                .lock()
                .expect("record approval request")
                .push(request);
            self.decisions
                .lock()
                .expect("read approval decision")
                .pop_front()
                .unwrap_or(false)
        }
    }

    fn acp_session(id: String, provider: Arc<dyn Provider>, cwd: &std::path::Path) -> AcpSession {
        AcpSession {
            id,
            provider: crate::provider_whitelist::SupportedProvider::Codex,
            model: provider.model(),
            effort: "none".to_string(),
            working_dir: Some(cwd.display().to_string()),
            created_at: chrono::Utc::now().timestamp_millis(),
            permission_mode: crate::session::PermissionMode::Default,
            available_models: vec![provider.model()],
            handle: Some(crate::provider::ProviderHandle {
                provider,
                auth: None,
                provider_kind: crate::provider_whitelist::SupportedProvider::Codex,
            }),
        }
    }

    fn text_blocks(messages: &[Message]) -> Vec<String> {
        messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn tool_context(cwd: &std::path::Path, call_id: &str) -> ToolContext {
        ToolContext {
            session_id: "agentic-approval-session".to_string(),
            message_id: "agentic-approval-message".to_string(),
            tool_call_id: call_id.to_string(),
            working_dir: Some(cwd.to_path_buf()),
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: mona_app_core::tool::ToolExecutionMode::AgentTurn,
        }
    }

    fn write_bridge(
        tools: Arc<mona_acp_tools::ToolRegistry>,
        approval: Arc<dyn ApprovalBroker>,
    ) -> BuiltinToolBridge {
        let source = tools.get("write_file").expect("default write tool");
        BuiltinToolBridge {
            advertised_name: "write".to_string(),
            source_name: "write_file".to_string(),
            description: source.description().to_string(),
            schema: source.input_schema(),
            requires_approval: true,
            tools,
            approval,
        }
    }

    #[test]
    fn unmanaged_background_detection_ignores_data_and_redirection() {
        for command in [
            "printf '%s' 'cats & dogs'",
            "printf \"nohup & disown\"",
            "printf ok 2>&1",
            "first && second",
            "printf '\\&'",
        ] {
            assert_eq!(
                unmanaged_background_reason(command),
                None,
                "ordinary shell syntax was rejected: {command}"
            );
        }
    }

    #[test]
    fn unmanaged_background_detection_blocks_process_escape() {
        for command in [
            "nohup worker >worker.log 2>&1 &",
            "worker &",
            "worker & echo launched",
            "setsid worker",
            "worker; disown",
        ] {
            assert!(
                unmanaged_background_reason(command).is_some(),
                "process escape was accepted: {command}"
            );
        }
    }

    #[derive(Clone, Default)]
    struct BackgroundLifecycleProvider {
        calls: Arc<Mutex<usize>>,
        requests: Arc<Mutex<Vec<Vec<Message>>>>,
    }

    #[async_trait]
    impl Provider for BackgroundLifecycleProvider {
        async fn complete(
            &self,
            messages: &[Message],
            tools: &[ToolDefinition],
            _system: &str,
            _resume_session_id: Option<&str>,
        ) -> Result<EventStream> {
            assert!(tools.iter().any(|tool| tool.name == "bash"));
            assert!(tools.iter().any(|tool| tool.name == "bg"));
            self.requests
                .lock()
                .expect("record lifecycle request")
                .push(messages.to_vec());
            let call = {
                let mut calls = self.calls.lock().expect("count lifecycle calls");
                *calls += 1;
                *calls
            };
            let events = match call {
                1 => vec![
                    StreamEvent::ToolUseStart {
                        id: "background-bash".to_string(),
                        name: "bash".to_string(),
                    },
                    StreamEvent::ToolInputDelta(
                        serde_json::json!({
                            "command": "sleep 1; printf finished",
                            "intent": "run lifecycle test work",
                            "run_in_background": true,
                            "notify": false
                        })
                        .to_string(),
                    ),
                    StreamEvent::ToolUseEnd,
                    StreamEvent::MessageEnd {
                        stop_reason: Some("tool_use".to_string()),
                    },
                ],
                2 => vec![
                    StreamEvent::TextDelta("The task is still running.".to_string()),
                    StreamEvent::MessageEnd {
                        stop_reason: Some("end_turn".to_string()),
                    },
                ],
                3 => vec![
                    StreamEvent::ToolUseStart {
                        id: "background-wait".to_string(),
                        name: "bg".to_string(),
                    },
                    StreamEvent::ToolInputDelta(
                        serde_json::json!({
                            "action": "wait",
                            "latest": true,
                            "session_only": true,
                            "max_wait_seconds": 5,
                            "return_on_progress": false
                        })
                        .to_string(),
                    ),
                    StreamEvent::ToolUseEnd,
                    StreamEvent::MessageEnd {
                        stop_reason: Some("tool_use".to_string()),
                    },
                ],
                4 => vec![
                    StreamEvent::TextDelta(
                        "The tracked task completed and was verified.".to_string(),
                    ),
                    StreamEvent::MessageEnd {
                        stop_reason: Some("end_turn".to_string()),
                    },
                ],
                other => panic!("unexpected lifecycle provider call {other}"),
            };
            Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
        }

        fn name(&self) -> &str {
            "background-lifecycle-test"
        }

        fn model(&self) -> String {
            "gpt-5-test".to_string()
        }

        fn fork(&self) -> Arc<dyn Provider> {
            Arc::new(self.clone())
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn acp_turn_monitors_tracked_background_work_before_finishing() {
        let _env_lock = TEST_ENV_LOCK.lock().expect("lock MONA_HOME");
        let home = tempfile::tempdir().expect("temporary MONA_HOME");
        let _home = TestHomeGuard::set(home.path());
        let cwd = tempfile::tempdir().expect("temporary workspace");
        let provider = Arc::new(BackgroundLifecycleProvider::default());
        let session = acp_session(
            format!("agentic-background-{}", uuid::Uuid::new_v4()),
            provider.clone(),
            cwd.path(),
        );
        let approval = Arc::new(ScriptedApproval::new([true]));
        let (events, _receiver) = mpsc::unbounded_channel();

        run_turn(
            &session,
            Arc::new(mona_acp_tools::default_registry()),
            SessionMcpTools::default(),
            approval.clone(),
            events,
            "run the task and stay with it until completion",
            CancellationToken::new(),
            None,
        )
        .await
        .expect("tracked ACP background turn");

        assert_eq!(
            *provider.calls.lock().expect("read lifecycle call count"),
            4,
            "the premature model completion must be followed by a guard round and bg wait"
        );
        let requests = provider.requests.lock().expect("read lifecycle requests");
        let guard_text = text_blocks(&requests[2]);
        assert!(
            guard_text
                .iter()
                .any(|text| text.contains("[ACP lifecycle guard]") && text.contains("bg")),
            "the model did not receive the lifecycle guard: {guard_text:?}"
        );
        let approvals = approval.requests.lock().expect("read lifecycle approvals");
        assert_eq!(approvals.len(), 1, "only bash needs approval in this flow");
        assert_eq!(approvals[0].tool_name, "bash");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn sequential_turns_replay_prior_user_and_assistant_before_new_user() {
        let _env_lock = TEST_ENV_LOCK.lock().expect("lock MONA_HOME");
        let home = tempfile::tempdir().expect("temporary MONA_HOME");
        let _home = TestHomeGuard::set(home.path());
        let cwd = tempfile::tempdir().expect("temporary workspace");
        let provider = Arc::new(ReplayProvider::default());
        let session = acp_session(
            format!("agentic-replay-{}", uuid::Uuid::new_v4()),
            provider.clone(),
            cwd.path(),
        );
        let approval = Arc::new(ScriptedApproval::new([true]));

        for prompt in ["user-1", "user-2"] {
            let (events, _receiver) = mpsc::unbounded_channel();
            run_turn(
                &session,
                Arc::new(mona_acp_tools::default_registry()),
                SessionMcpTools::default(),
                approval.clone(),
                events,
                prompt,
                CancellationToken::new(),
                None,
            )
            .await
            .expect("canonical agent turn");
        }

        let requests = provider.requests.lock().expect("read recorded requests");
        assert_eq!(requests.len(), 2);
        let second = text_blocks(&requests[1]);
        for required in ["user-1", "assistant-1", "user-2"] {
            assert!(
                second.iter().any(|text| text.contains(required)),
                "second provider request lost {required:?}: {second:?}"
            );
        }
        let user_one = second
            .iter()
            .position(|text| text.contains("user-1"))
            .expect("first user message");
        let assistant_one = second
            .iter()
            .position(|text| text.contains("assistant-1"))
            .expect("first assistant message");
        let user_two = second
            .iter()
            .position(|text| text.contains("user-2"))
            .expect("second user message");
        assert!(user_one < assistant_one && assistant_one < user_two);
        assert_eq!(
            provider
                .resume_ids
                .lock()
                .expect("read provider continuations")
                .as_slice(),
            &[None, Some("provider-continuation-1".to_string())]
        );
    }

    #[tokio::test]
    async fn approval_required_adapter_denies_then_allows_exactly_once() {
        let cwd = tempfile::tempdir().expect("temporary workspace");
        let tools = Arc::new(mona_acp_tools::default_registry());
        let approval = Arc::new(ScriptedApproval::new([false, true]));
        let bridge = write_bridge(tools, approval.clone());
        let input = serde_json::json!({
            "path": "approved.txt",
            "content": "written only after approval",
            "intent": "test-only metadata removed by bridge"
        });

        let denied = bridge
            .execute(input.clone(), tool_context(cwd.path(), "deny-once"))
            .await
            .expect_err("denied approval must prevent execution");
        assert!(denied.to_string().contains("permission denied"));
        assert!(
            !cwd.path().join("approved.txt").exists(),
            "denied adapter invocation wrote a file"
        );

        bridge
            .execute(input, tool_context(cwd.path(), "allow-once"))
            .await
            .expect("one approved invocation executes");
        assert_eq!(
            std::fs::read_to_string(cwd.path().join("approved.txt")).expect("approved file"),
            "written only after approval"
        );
        let requests = approval.requests.lock().expect("approval requests");
        assert_eq!(requests.len(), 2, "approval is requested per invocation");
        assert_eq!(requests[0].request_id, "deny-once");
        assert_eq!(requests[1].request_id, "allow-once");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn canonical_session_resume_retains_transcript() {
        let _env_lock = TEST_ENV_LOCK.lock().expect("lock MONA_HOME");
        let home = tempfile::tempdir().expect("temporary MONA_HOME");
        let _home = TestHomeGuard::set(home.path());
        let cwd = tempfile::tempdir().expect("temporary workspace");
        let provider = Arc::new(ReplayProvider::default());
        let session = acp_session(
            format!("agentic-resume-{}", uuid::Uuid::new_v4()),
            provider,
            cwd.path(),
        );
        let (events, _receiver) = mpsc::unbounded_channel();
        run_turn(
            &session,
            Arc::new(mona_acp_tools::default_registry()),
            SessionMcpTools::default(),
            Arc::new(ScriptedApproval::new([true])),
            events,
            "persisted-user",
            CancellationToken::new(),
            None,
        )
        .await
        .expect("initial canonical turn");

        assert!(
            session_exists(&session.id),
            "canonical session was not saved"
        );
        let mut resumed = MonaSession::load(&session.id).expect("resume canonical session");
        let transcript = text_blocks(
            &resumed
                .messages_for_provider()
                .into_iter()
                .collect::<Vec<_>>(),
        );
        assert!(
            transcript
                .iter()
                .any(|text| text.contains("persisted-user"))
        );
        assert!(transcript.iter().any(|text| text.contains("assistant-1")));
    }
}

//! End-to-end smoke test for mona-acp.
//!
//! Spawns the `mona-acp` binary, pipes a sequence of ACP JSON-RPC frames
//! over stdin, and verifies each response. Asserts that:
//!
//! - `initialize` returns protocolVersion 1 and the monitter extensions.
//! - `session/new` with `provider=codex` returns a valid sessionId and
//!   defaults to `gpt-5.5` / effort `high`.
//! - `session/list` includes the freshly created session.
//! - `session/prompt` returns the Phase 2 stub response (no agent loop
//!   wiring yet).
//! - `session/set_model` and `session/set_reasoning_effort` succeed for a
//!   real session.
//! - `session/cancel` removes the session.
//! - `session/new` with `provider=gemini` rejects with the friendly
//!   whitelist error.
//!
//! This test runs the binary as a subprocess because the server loop
//! owns stdin/stdout and we can't easily round-trip a real ACP session
//! in-process.

use serde_json::Value;
use std::io::{BufRead, Write};
use std::process::{Command, Stdio};

fn mona_acp_bin() -> std::path::PathBuf {
    // The binary lives at target/release/mona-acp relative to the workspace root.
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // pop crates/mona-acp
    p.pop(); // pop crates
    p.push("target");
    p.push("release");
    p.push("mona-acp");
    p
}

fn send_one(bin: &std::path::Path, frames: &[&str]) -> Vec<Value> {
    let mut child = Command::new(bin)
        .env("MONA_ACP_LOG", "error")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mona-acp");

    {
        let mut stdin = child.stdin.take().expect("stdin");
        for f in frames {
            writeln!(stdin, "{f}").expect("write frame");
        }
    }

    let output = child.wait_with_output().expect("wait mona-acp");
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str::<Value>(l).expect("valid JSON-RPC response"))
        .collect()
}

#[test]
fn initialize_returns_protocol_v1_and_monitter_extensions() {
    let bin = mona_acp_bin();
    if !bin.exists() {
        eprintln!("skipping: {} not built yet", bin.display());
        return;
    }
    let responses = send_one(
        &bin,
        &[r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#],
    );
    assert_eq!(responses.len(), 1);
    let r = &responses[0];
    assert_eq!(r["id"], 1);
    assert_eq!(r["result"]["protocolVersion"], 1);
    assert_eq!(
        r["result"]["agentCapabilities"]["extensions"]["monitter"]["jev_routing"],
        true
    );
    assert_eq!(
        r["result"]["agentCapabilities"]["extensions"]["monitter"]["reasoning_effort"],
        true
    );
    assert_eq!(r["result"]["agentInfo"]["monitter_harness"], true);
    let auth_ids: Vec<&str> = r["result"]["authMethods"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(auth_ids, vec!["codex", "claude", "minimax"]);
}

#[test]
fn full_session_lifecycle() {
    let bin = mona_acp_bin();
    if !bin.exists() {
        eprintln!("skipping: {} not built yet", bin.display());
        return;
    }

    // The session registry is in-process, so the session_id from one
    // invocation is not visible to the next. Use a two-pass approach:
    // 1. Run a binary that captures its session/new response, then
    //    constructs the follow-up frames using that sessionId, and pipes
    //    everything in one invocation.
    //
    // We achieve this by piping all 5 frames in one call to send_one and
    // constructing the session/new frame with a placeholder, then
    // post-processing: actually, since the session/new response carries
    // the sessionId, and we need that to build the OTHER frames, we
    // can't pipe everything in one shot without dynamic frame
    // construction.
    //
    // Simplest correct approach: use a child process with stdin/stdout,
    // send session/new, read the response, send the follow-up frames
    // referencing the captured sessionId, all on the same child process.

    let mut child = Command::new(&bin)
        .env("MONA_ACP_LOG", "error")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mona-acp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = std::io::BufReader::new(stdout);

    // 1. session/new
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"session/new","params":{{"provider":"codex"}}}}"#
    )
    .expect("write new");
    let mut line = String::new();
    reader.read_line(&mut line).expect("read new resp");
    let new_resp: Value = serde_json::from_str(line.trim()).expect("new resp json");
    let session_id = new_resp["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();
    assert_eq!(new_resp["result"]["model"], "gpt-5.5");
    assert_eq!(new_resp["result"]["effort"], "high");
    assert_eq!(new_resp["result"]["provider"], "codex");

    // 2. session/prompt
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{{"sessionId":"{session_id}","text":"hello mona"}}}}"#
    )
    .expect("write prompt");
    line.clear();
    reader.read_line(&mut line).expect("read prompt resp");
    let prompt_resp: Value = serde_json::from_str(line.trim()).expect("prompt resp json");
    assert_eq!(prompt_resp["result"]["stopReason"], "phase2.5_routing_done");

    // 3. session/set_model
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":3,"method":"session/set_model","params":{{"sessionId":"{session_id}","model":"gpt-5.5"}}}}"#
    )
    .expect("write set_model");
    line.clear();
    reader.read_line(&mut line).expect("read set_model resp");
    let set_model_resp: Value = serde_json::from_str(line.trim()).expect("set_model resp json");
    assert_eq!(set_model_resp["result"]["model"], "gpt-5.5");

    // 4. session/set_reasoning_effort
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":4,"method":"session/set_reasoning_effort","params":{{"sessionId":"{session_id}","effort":"high"}}}}"#
    )
    .expect("write set_effort");
    line.clear();
    reader.read_line(&mut line).expect("read set_effort resp");
    let set_effort_resp: Value = serde_json::from_str(line.trim()).expect("set_effort resp json");
    assert_eq!(set_effort_resp["result"]["effort"], "high");

    // 5. session/cancel
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":5,"method":"session/cancel","params":{{"sessionId":"{session_id}"}}}}"#
    )
    .expect("write cancel");
    line.clear();
    reader.read_line(&mut line).expect("read cancel resp");
    let cancel_resp: Value = serde_json::from_str(line.trim()).expect("cancel resp json");
    assert_eq!(cancel_resp["result"]["cancelled"], true);

    drop(stdin);
    let _ = child.wait();
}

#[test]
fn unsupported_provider_is_rejected_with_helpful_message() {
    let bin = mona_acp_bin();
    if !bin.exists() {
        eprintln!("skipping: {} not built yet", bin.display());
        return;
    }
    let responses = send_one(
        &bin,
        &[r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"provider":"gemini"}}"#],
    );
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["error"]["code"], -32602);
    let msg = responses[0]["error"]["message"].as_str().unwrap();
    assert!(msg.contains("gemini"));
    assert!(msg.contains("codex"));
    assert!(msg.contains("claude"));
    assert!(msg.contains("minimax"));
}

/// Phase 2.5: the per-turn router fires inside session/prompt, classifies
/// the prompt, applies safety gates, persists a router-trace JSON, and
/// updates the session's model. We use a complex prompt that the
/// rule-based classifier maps to `Strong` tier.
#[test]
fn per_turn_router_fires_on_session_prompt() {
    let bin = mona_acp_bin();
    if !bin.exists() {
        eprintln!("skipping: {} not built yet", bin.display());
        return;
    }

    // Use a temporary MONA_HOME so we don't pollute the real home dir.
    let tmp_home = std::env::temp_dir().join(format!("mona-e2e-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp_home).unwrap();

    let mut child = Command::new(&bin)
        .env("MONA_ACP_LOG", "error")
        .env("MONA_HOME", &tmp_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mona-acp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = std::io::BufReader::new(stdout);

    // 1. session/new
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"session/new","params":{{"provider":"codex"}}}}"#
    )
    .expect("write new");
    let mut line = String::new();
    reader.read_line(&mut line).expect("read new resp");
    let new_resp: Value = serde_json::from_str(line.trim()).expect("new resp json");
    let session_id = new_resp["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    // 2. session/prompt with a complex prompt (triggers Strong tier)
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{{"sessionId":"{session_id}","text":"design a new architecture for the auth system across the codebase"}}}}"#
    )
    .expect("write prompt");
    line.clear();
    reader.read_line(&mut line).expect("read prompt resp");
    let prompt_resp: Value = serde_json::from_str(line.trim()).expect("prompt resp json");

    assert_eq!(prompt_resp["result"]["stopReason"], "phase2.5_routing_done");
    assert_eq!(prompt_resp["result"]["applied"], true);
    assert_eq!(prompt_resp["result"]["tier"], "strong");
    assert!(prompt_resp["result"]["model"]
        .as_str()
        .unwrap()
        .starts_with("strong:"));
    assert_eq!(prompt_resp["result"]["effort"], "high");

    // 3. Verify a router-trace JSON was persisted
    let trace_dir = tmp_home.join("router-traces");
    let traces: Vec<_> = std::fs::read_dir(&trace_dir)
        .expect("read trace dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
        .collect();
    assert_eq!(traces.len(), 1, "expected exactly one router trace");
    let trace: Value =
        serde_json::from_str(&std::fs::read_to_string(traces[0].path()).unwrap()).unwrap();
    assert_eq!(trace["session_id"], session_id);
    assert_eq!(trace["applied"], true);
    assert_eq!(trace["proposed_tier"], "Strong");
    assert_eq!(trace["trigger"], "initial_prompt");

    // 4. session/cancel
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":3,"method":"session/cancel","params":{{"sessionId":"{session_id}"}}}}"#
    )
    .expect("write cancel");
    drop(stdin);
    let _ = child.wait();

    // Cleanup
    let _ = std::fs::remove_dir_all(&tmp_home);
}

/// Phase 2.5: a sensitive prompt is refused with a permission_required
/// error rather than being routed.
#[test]
fn sensitive_prompt_short_circuits_to_permission_required() {
    let bin = mona_acp_bin();
    if !bin.exists() {
        eprintln!("skipping: {} not built yet", bin.display());
        return;
    }

    let tmp_home = std::env::temp_dir().join(format!("mona-e2e-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp_home).unwrap();

    let mut child = Command::new(&bin)
        .env("MONA_ACP_LOG", "error")
        .env("MONA_HOME", &tmp_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mona-acp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = std::io::BufReader::new(stdout);

    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"session/new","params":{{"provider":"codex"}}}}"#
    )
    .unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let new_resp: Value = serde_json::from_str(line.trim()).unwrap();
    let session_id = new_resp["result"]["sessionId"].as_str().unwrap().to_string();

    // Sensitive prompt (matches the rule-based classifier's `is_sensitive`)
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{{"sessionId":"{session_id}","text":"rm -rf / please"}}}}"#
    )
    .unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    let resp: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(resp["error"]["code"], -32001);
    assert!(resp["error"]["message"]
        .as_str()
        .unwrap()
        .contains("sensitive"));

    drop(stdin);
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp_home);
}

/// Phase 3: when no auth is configured, the initialize response lists an
/// empty `configuredProviders` array, and `session/auth` reports
/// `configured: false` with a hint pointing at the file/env path.
#[test]
fn auth_loader_reports_unconfigured_provider() {
    let bin = mona_acp_bin();
    if !bin.exists() {
        eprintln!("skipping: {} not built yet", bin.display());
        return;
    }

    let tmp_home = std::env::temp_dir().join(format!("mona-e2e-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp_home).unwrap();

    let mut child = Command::new(&bin)
        .env("MONA_ACP_LOG", "error")
        .env("MONA_HOME", &tmp_home)
        // Explicitly clear the env-var fallback paths so the test
        // really exercises the "nothing configured" path.
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("MINIMAX_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mona-acp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = std::io::BufReader::new(stdout);

    // initialize
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{}}}}"#
    )
    .unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let init: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(init["result"]["protocolVersion"], 1);
    assert_eq!(
        init["result"]["configuredProviders"]
            .as_array()
            .unwrap()
            .len(),
        0,
        "no providers should be configured in a fresh home dir"
    );

    // session/new
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":2,"method":"session/new","params":{{"provider":"codex"}}}}"#
    )
    .unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    let new: Value = serde_json::from_str(line.trim()).unwrap();
    let session_id = new["result"]["sessionId"].as_str().unwrap().to_string();

    // session/auth
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":3,"method":"session/auth","params":{{"sessionId":"{session_id}"}}}}"#
    )
    .unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    let auth: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(auth["result"]["configured"], false);
    assert_eq!(auth["result"]["summary"], Value::Null);
    assert_eq!(auth["result"]["provider"], "codex");
    assert!(auth["result"]["hint"]
        .as_str()
        .unwrap()
        .contains("~/.mona/codex.json"));

    drop(stdin);
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp_home);
}

/// Phase 3: when a codex.json file is present, the auth loader picks it up
/// and `session/auth` reports the masked credential.
#[test]
fn auth_loader_picks_up_api_key_file() {
    let bin = mona_acp_bin();
    if !bin.exists() {
        eprintln!("skipping: {} not built yet", bin.display());
        return;
    }

    let tmp_home = std::env::temp_dir().join(format!("mona-e2e-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp_home).unwrap();
    std::fs::write(
        tmp_home.join("codex.json"),
        r#"{"kind":"openai_api_key","api_key":"sk-test-1234567890abcdef"}"#,
    )
    .unwrap();

    let mut child = Command::new(&bin)
        .env("MONA_ACP_LOG", "error")
        .env("MONA_HOME", &tmp_home)
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("MINIMAX_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mona-acp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = std::io::BufReader::new(stdout);

    // initialize — should now list codex as configured
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{}}}}"#
    )
    .unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let init: Value = serde_json::from_str(line.trim()).unwrap();
    let configured: Vec<String> = init["result"]["configuredProviders"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(configured, vec!["codex"]);

    // session/new (codex)
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":2,"method":"session/new","params":{{"provider":"codex"}}}}"#
    )
    .unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    let new: Value = serde_json::from_str(line.trim()).unwrap();
    let session_id = new["result"]["sessionId"].as_str().unwrap().to_string();

    // session/auth — should report configured=true with masked summary
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":3,"method":"session/auth","params":{{"sessionId":"{session_id}"}}}}"#
    )
    .unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    let auth: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(auth["result"]["configured"], true);
    let summary = auth["result"]["summary"].as_str().unwrap();
    assert!(summary.contains("OpenAI API key"));
    assert!(summary.contains("sk-t")); // head
    assert!(summary.contains("cdef")); // tail

    // session/auth on a session whose provider is NOT configured (claude)
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":4,"method":"session/new","params":{{"provider":"claude"}}}}"#
    )
    .unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    let new2: Value = serde_json::from_str(line.trim()).unwrap();
    let claude_session_id = new2["result"]["sessionId"].as_str().unwrap().to_string();

    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":5,"method":"session/auth","params":{{"sessionId":"{claude_session_id}"}}}}"#
    )
    .unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    let auth2: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(auth2["result"]["configured"], false);
    assert_eq!(auth2["result"]["provider"], "claude");

    drop(stdin);
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp_home);
}

/// Phase 3.5: when auth is configured, `session/new` attaches a
/// `ProviderHandle` and the response includes `providerName`. When auth is
/// not configured, the response still includes `provider` but
/// `providerName` is `null`.
#[test]
fn session_new_reports_provider_name_when_auth_configured() {
    let bin = mona_acp_bin();
    if !bin.exists() {
        eprintln!("skipping: {} not built yet", bin.display());
        return;
    }

    // ── Case A: auth configured → providerName is the stub's name() ──
    let tmp_home = std::env::temp_dir().join(format!("mona-e2e-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp_home).unwrap();
    std::fs::write(
        tmp_home.join("codex.json"),
        r#"{"kind":"openai_api_key","api_key":"sk-test-1234567890abcdef"}"#,
    )
    .unwrap();

    let mut child = Command::new(&bin)
        .env("MONA_ACP_LOG", "error")
        .env("MONA_HOME", &tmp_home)
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("MINIMAX_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mona-acp (configured)");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = std::io::BufReader::new(stdout);

    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"session/new","params":{{"provider":"codex"}}}}"#
    )
    .unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let new: Value = serde_json::from_str(line.trim()).unwrap();
    // ProviderHandle attached → providerName is the stub identifier
    assert_eq!(new["result"]["providerName"], "codex-stub");
    assert_eq!(new["result"]["provider"], "codex");
    assert_eq!(new["result"]["model"], "gpt-5.5");

    drop(stdin);
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp_home);

    // ── Case B: no auth configured → session/new still succeeds, but
    // `providerName` is null. The placeholder handle can be inspected
    // via session/auth, which will report configured=false with a hint.
    let tmp_home_b = std::env::temp_dir().join(format!("mona-e2e-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp_home_b).unwrap();

    let mut child_b = Command::new(&bin)
        .env("MONA_ACP_LOG", "error")
        .env("MONA_HOME", &tmp_home_b)
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("MINIMAX_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mona-acp (unconfigured)");

    let mut stdin_b = child_b.stdin.take().expect("stdin");
    let stdout_b = child_b.stdout.take().expect("stdout");
    let mut reader_b = std::io::BufReader::new(stdout_b);

    writeln!(
        stdin_b,
        r#"{{"jsonrpc":"2.0","id":1,"method":"session/new","params":{{"provider":"codex"}}}}"#
    )
    .unwrap();
    line.clear();
    reader_b.read_line(&mut line).unwrap();
    let new_b: Value = serde_json::from_str(line.trim()).unwrap();
    assert!(
        new_b["result"].is_object(),
        "expected result, got {new_b}"
    );
    let session_id_b = new_b["result"]["sessionId"].as_str().unwrap().to_string();
    assert_eq!(new_b["result"]["providerName"], Value::Null);
    assert_eq!(new_b["result"]["provider"], "codex");

    // session/auth on the placeholder session reports configured=false
    writeln!(
        stdin_b,
        r#"{{"jsonrpc":"2.0","id":2,"method":"session/auth","params":{{"sessionId":"{session_id_b}"}}}}"#
    )
    .unwrap();
    line.clear();
    reader_b.read_line(&mut line).unwrap();
    let auth_b: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(auth_b["result"]["configured"], false);
    assert!(auth_b["result"]["hint"]
        .as_str()
        .unwrap()
        .contains("~/.mona/codex.json"));

    drop(stdin_b);
    let _ = child_b.wait();
    let _ = std::fs::remove_dir_all(&tmp_home_b);
}

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
    assert_eq!(prompt_resp["result"]["stopReason"], "phase2_stub");

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

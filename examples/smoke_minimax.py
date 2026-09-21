#!/usr/bin/env python3
"""
End-to-end smoke test: drive a real session/prompt through mona-acp
against api.minimax.io using the API key in ~/.mona/env.

The MINIMAX_API_KEY env var is set from ~/.mona/env in the parent
shell; this script never reads the file. It only consumes the env
var passed in.

Steps:
  1. initialize
  2. session/new (provider: minimax)
  3. session/prompt with a fixed, short prompt

Output: prints the final assistant text (truncated to 400 chars) plus
the wire-level event log. The API key never appears in this output.
"""
import json
import os
import subprocess
import sys
import threading


def send(stdin, frame_id, method, params):
    frame = {"jsonrpc": "2.0", "id": frame_id, "method": method, "params": params}
    stdin.write(json.dumps(frame) + "\n")
    stdin.flush()


def reader_thread(stdout, events, done, stderr_buf):
    """Drain mona-acp stdout line-by-line, classify each frame.
    Stderr lines are appended to stderr_buf (separately) for printing
    at the end."""
    for raw in stdout:
        line = raw.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            events.append(("PARSE_ERROR", line))
            continue
        if msg.get("method") == "session/update":
            upd = msg.get("params", {}).get("update", {})
            events.append((upd.get("sessionUpdate", "?"), upd))
        else:
            events.append(("RESPONSE", msg))
    done.set()


def stderr_thread(stderr, buf):
    """Drain stderr into buf; do not classify."""
    for raw in stderr:
        buf.append(raw)


def main():
    bin_path = os.environ.get(
        "MONA_ACP_BIN",
        "/Users/alex/code/mona-acp-milestone-a/target/debug/mona-acp",
    )
    if not os.path.exists(bin_path):
        print(f"binary not found: {bin_path}", file=sys.stderr)
        sys.exit(2)

    prompt_text = (
        "Reply with exactly one short sentence naming a single "
        "vegetable. No other text."
    )

    # Allow override of model via env var so we can retry with M2.7 etc.
    model_override = os.environ.get("MINIMAX_SMOKE_MODEL")

    child = subprocess.Popen(
        [bin_path],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env={**os.environ},  # inherit MINIMAX_API_KEY from parent
    )

    events = []
    stderr_buf = []
    done = threading.Event()
    t_out = threading.Thread(
        target=reader_thread,
        args=(child.stdout, events, done, stderr_buf),
        daemon=True,
    )
    t_err = threading.Thread(
        target=stderr_thread,
        args=(child.stderr, stderr_buf),
        daemon=True,
    )
    t_out.start()
    t_err.start()

    # 1. initialize
    send(
        child.stdin,
        1,
        "initialize",
        {
            "protocolVersion": 1,
            "clientInfo": {"name": "mavis-smoke", "version": "0.1"},
            "capabilities": {},
        },
    )
    # Read the initialize response before sending session/new.
    import time
    time.sleep(0.2)

    # 2. session/new (minimax)
    new_params = {"provider": "minimax"}
    if model_override:
        new_params["model"] = model_override
    send(child.stdin, 2, "session/new", new_params)
    # Wait for the session/new response so we can capture the real id.
    deadline = time.time() + 5
    session_id = None
    while time.time() < deadline:
        for kind, payload in events:
            if kind == "RESPONSE" and payload.get("id") == 2:
                session_id = (
                    payload.get("result", {}).get("sessionId") if "result" in payload else None
                )
                break
        if session_id:
            break
        time.sleep(0.05)
    if not session_id:
        print("session/new did not return a sessionId; aborting", file=sys.stderr)
        sys.exit(2)
    print(f"# session_id: {session_id}")

    # 3. session/prompt with the real sessionId
    send(child.stdin, 3, "session/prompt", {"sessionId": session_id, "text": prompt_text})

    # Wait briefly for responses.
    import time
    deadline = time.time() + 60
    while time.time() < deadline and not done.is_set():
        # We expect at least 3 RESPONSE events; let the stream drain a
        # little past the final prompt response.
        resp_count = sum(1 for k, _ in events if k == "RESPONSE")
        if resp_count >= 3:
            # Give the model a moment to finish streaming.
            time.sleep(2)
            break
        time.sleep(0.1)

    try:
        child.stdin.close()
    except Exception:
        pass

    try:
        child.wait(timeout=10)
    except subprocess.TimeoutExpired:
        child.terminate()

    stderr_out = child.stderr.read() if child.stderr else ""

    # Print events for the transcript.
    print("=== session events ===")
    for kind, payload in events:
        # For text-delta events, only print the kind + summary, not the
        # full text (still helpful to verify streaming worked).
        if kind == "agent_message_chunk":
            text = payload.get("content", {}).get("text", "")
            print(f"  agent_message_chunk: {text!r}")
        elif kind == "agent_thought_chunk":
            text = payload.get("content", {}).get("text", "")
            if text:
                print(f"  agent_thought_chunk: {text!r}")
            else:
                print(f"  agent_thought_chunk: {payload}")
        elif kind in ("tool_call", "tool_call_update"):
            print(f"  {kind}: {payload}")
        elif kind == "usage_update":
            print(f"  usage_update: input={payload.get('inputTokens')} output={payload.get('outputTokens')}")
        elif kind == "error":
            print(f"  ERROR: {payload}")
        elif kind == "RESPONSE":
            print(f"  RESPONSE: {json.dumps(payload)[:300]}")
        else:
            print(f"  {kind}: {json.dumps(payload)[:200]}")

    # Extract the final prompt response.
    prompt_resp = None
    for kind, payload in events:
        if kind == "RESPONSE" and payload.get("id") == 3:
            prompt_resp = payload
            break

    print()
    print("=== final session/prompt response ===")
    print(json.dumps(prompt_resp, indent=2))

    print()
    print("=== stderr (truncated) ===")
    print(stderr_out[:1500])

    # Build a short summary.
    if prompt_resp and prompt_resp.get("result"):
        result = prompt_resp["result"]
        text = result.get("output", "")
        print()
        print("=== summary ===")
        print(f"  model:    {result.get('model')}")
        print(f"  stopReason: {result.get('stopReason')}")
        print(f"  usage:    {result.get('usage')}")
        print(f"  output:   {text[:400]!r}{'...' if len(text) > 400 else ''}")
    elif prompt_resp and prompt_resp.get("error"):
        err = prompt_resp["error"]
        print()
        print("=== summary ===")
        print(f"  FAILED: code={err.get('code')} message={err.get('message')}")
        sys.exit(1)


if __name__ == "__main__":
    main()
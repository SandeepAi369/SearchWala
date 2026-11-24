use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

fn searchwala_bin() -> &'static str {
    env!("CARGO_BIN_EXE_searchwala")
}

#[test]
fn cli_version_help_and_invalid_subcommand() {
    let ver = Command::new(searchwala_bin())
        .env_clear()
        .arg("--version")
        .output()
        .expect("run --version");
    assert!(ver.status.success());
    let stdout = String::from_utf8(ver.stdout).unwrap();
    assert_eq!(
        stdout.trim(),
        format!("searchwala {}", env!("CARGO_PKG_VERSION"))
    );
    assert!(ver.stderr.is_empty());

    let help = Command::new(searchwala_bin())
        .env_clear()
        .arg("--help")
        .output()
        .expect("run --help");
    assert!(help.status.success());
    let help_out = String::from_utf8(help.stdout).unwrap();
    assert!(help_out.contains("searchwala mcp"));
    assert!(help_out.contains("--log-level"));

    let bad = Command::new(searchwala_bin())
        .env_clear()
        .arg("not-a-subcommand")
        .output()
        .expect("run invalid subcommand");
    assert_eq!(bad.status.code(), Some(2));
    let bad_err = String::from_utf8(bad.stderr).unwrap();
    assert!(bad_err.contains("unknown subcommand"));
}

#[test]
fn mcp_stdio_zero_env_zero_noise_session() {
    // Launch with completely cleared environment: zero LLM API keys, no HOME,
    // no RUST_LOG, no .env dependency.
    let mut child = Command::new(searchwala_bin())
        .env_clear()
        .current_dir(std::env::temp_dir())
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn searchwala mcp");

    let frames = [
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "binary-integration-test", "version": "1.0" }
            }
        }),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        json!({ "jsonrpc": "2.0", "id": "ping-1", "method": "ping" }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "search_status" }
        }),
        json!({ "jsonrpc": "2.0", "id": 4, "method": "resources/list" }),
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "resources/read",
            "params": { "uri": "searchwala://config/modes" }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "resources/read",
            "params": { "uri": "searchwala://health/engines" }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "resources/read",
            "params": { "uri": "searchwala://health/cache" }
        }),
    ];

    {
        let mut stdin = child.stdin.take().expect("stdin piped");
        for f in &frames {
            writeln!(stdin, "{f}").expect("write jsonrpc frame");
        }
    } // EOF on stdin

    let output = child.wait_with_output().expect("wait for searchwala mcp");
    let stderr = String::from_utf8(output.stderr).unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(
        output.status.success(),
        "expected exit 0, got {:?}; stderr={stderr:?}",
        output.status.code()
    );
    assert!(
        stderr.is_empty(),
        "expected zero startup noise / zero API-key warnings on stderr, got: {stderr:?}"
    );

    let responses: Vec<Value> = stdout
        .lines()
        .map(|l| {
            serde_json::from_str(l).unwrap_or_else(|e| panic!("bad stdout JSON line {l:?}: {e}"))
        })
        .collect();
    assert_eq!(responses.len(), 8, "expected 8 responses: {responses:?}");

    let by_id = |id: Value| -> &Value {
        responses
            .iter()
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("missing id={id} in {responses:?}"))
    };

    assert_eq!(by_id(json!(1))["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(
        by_id(json!(1))["result"]["serverInfo"]["name"],
        "searchwala"
    );
    assert_eq!(by_id(json!("ping-1"))["result"], json!({}));
    assert_eq!(
        by_id(json!(2))["result"]["tools"].as_array().unwrap().len(),
        4
    );
    assert_eq!(
        by_id(json!(3))["result"]["structuredContent"]["version"],
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(
        by_id(json!(4))["result"]["resources"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    for id in [5, 6, 7] {
        let text = by_id(json!(id))["result"]["contents"][0]["text"]
            .as_str()
            .unwrap();
        let parsed: Value = serde_json::from_str(text).expect("valid resource JSON");
        assert!(parsed.is_object());
    }
}

#[test]
fn mcp_stdio_error_handling_and_ssrf_stdout_purity() {
    let mut child = Command::new(searchwala_bin())
        .env_clear()
        .current_dir(std::env::temp_dir())
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn searchwala mcp");

    let raw_lines = [
        "{broken json".to_string(),
        json!({ "jsonrpc": "2.0", "id": 10, "method": "unknown/method" }).to_string(),
        json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/call",
            "params": { "name": "search_web", "arguments": { "query": "   " } }
        })
        .to_string(),
        json!({
            "jsonrpc": "2.0",
            "id": 12,
            "method": "tools/call",
            "params": { "name": "search_passages", "arguments": { "query": "rust", "mode": "invalid_mode" } }
        })
        .to_string(),
        json!({
            "jsonrpc": "2.0",
            "id": 13,
            "method": "tools/call",
            "params": { "name": "extract_url", "arguments": { "url": "http://169.254.169.254/latest/meta-data/" } }
        })
        .to_string(),
    ];

    {
        let mut stdin = child.stdin.take().expect("stdin piped");
        for l in &raw_lines {
            writeln!(stdin, "{l}").unwrap();
        }
    }

    let output = child.wait_with_output().expect("wait for searchwala mcp");
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let responses: Vec<Value> = stdout
        .lines()
        .map(|l| {
            serde_json::from_str(l)
                .unwrap_or_else(|e| panic!("stdout corrupted by non-JSON line {l:?}: {e}"))
        })
        .collect();
    assert_eq!(
        responses.len(),
        5,
        "expected 5 JSON-RPC replies: {responses:?}"
    );

    let by_id = |id: Value| -> &Value {
        responses
            .iter()
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("missing id={id} in {responses:?}"))
    };

    assert_eq!(by_id(Value::Null)["error"]["code"], -32700);
    assert_eq!(by_id(json!(10))["error"]["code"], -32601);
    assert_eq!(by_id(json!(11))["result"]["isError"], true);
    assert_eq!(by_id(json!(12))["result"]["isError"], true);
    assert_eq!(by_id(json!(13))["result"]["isError"], true);
    assert!(by_id(json!(13))["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("SSRF"));
}

#[tokio::test]
async fn mcp_http_binary_headless_serves_health_and_mcp_zero_env() {
    // Pick an available ephemeral loopback port.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe port");
    let port = probe.local_addr().unwrap().port();
    drop(probe);

    let child = Command::new(searchwala_bin())
        .env_clear()
        .current_dir(std::env::temp_dir())
        .args([
            "mcp",
            "--http",
            "--bind",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "--log-level",
            "warn",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn searchwala mcp --http");

    struct KillOnDrop(std::process::Child);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _guard = KillOnDrop(child);

    let client = reqwest::Client::new();
    let health_url = format!("http://127.0.0.1:{port}/health");
    let mcp_url = format!("http://127.0.0.1:{port}/mcp");

    // Wait briefly for listener startup.
    let mut ready = false;
    for _ in 0..40 {
        if let Ok(resp) = client.get(&health_url).send().await {
            if resp.status().is_success() {
                let body: Value = resp.json().await.unwrap();
                assert_eq!(body["transport"], "mcp-streamable-http");
                ready = true;
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        ready,
        "headless searchwala mcp --http failed to start on port {port}"
    );

    // Cross-origin browser request must be blocked with 403.
    let evil = client
        .post(&mcp_url)
        .header("Origin", "https://evil.example")
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }))
        .send()
        .await
        .unwrap();
    assert_eq!(evil.status().as_u16(), 403);

    // Initialize + tools/list + search_status over real HTTP.
    let init = client
        .post(&mcp_url)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "bin-http-test", "version": "1.0" } }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(init.status().as_u16(), 200);
    let sid = init
        .headers()
        .get("mcp-session-id")
        .expect("session header")
        .to_str()
        .unwrap()
        .to_string();
    let init_json: Value = init.json().await.unwrap();
    assert_eq!(init_json["result"]["serverInfo"]["name"], "searchwala");

    let tools: Value = client
        .post(&mcp_url)
        .header("mcp-session-id", &sid)
        .json(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 4);
    assert!(tools["result"]["tools"][0]["inputSchema"]["properties"]
        .get("strict_domains")
        .is_some());
}

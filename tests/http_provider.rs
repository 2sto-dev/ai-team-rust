mod common;

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use ai_team::{config::ProviderKind, llm::provider_for};
use common::fast_model;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

enum Reply {
    Status(u16, String),
    Hang,
}

fn ok_body(content: &str) -> String {
    serde_json::json!({"choices": [{"message": {"content": content}}]}).to_string()
}

/// OpenAI-style server: base URL ends in `/v1`.
async fn serve(replies: Vec<Reply>) -> (String, Arc<AtomicUsize>) {
    let (origin, hits, _requests) = serve_capturing(replies).await;
    (format!("{origin}/v1"), hits)
}

/// Minimal HTTP/1.1 server answering one connection per scripted reply. Returns the origin
/// (`http://127.0.0.1:port`), the hit counter and every raw request received.
async fn serve_capturing(
    replies: Vec<Reply>,
) -> (String, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();

    tokio::spawn(async move {
        for reply in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            counter.fetch_add(1, Ordering::SeqCst);
            let request = read_request(&mut socket).await;
            captured.lock().unwrap().push(request);
            match reply {
                Reply::Status(status, body) => {
                    let response = format!(
                        "HTTP/1.1 {status} Scripted\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                    let _ = socket.shutdown().await;
                }
                Reply::Hang => {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            }
        }
    });

    (format!("http://{addr}"), hits, requests)
}

async fn read_request(socket: &mut TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = socket.read(&mut chunk).await.unwrap();
        if read == 0 {
            return String::from_utf8_lossy(&buffer).into_owned();
        }
        buffer.extend_from_slice(&chunk[..read]);
        let text = String::from_utf8_lossy(&buffer);
        if let Some(header_end) = text.find("\r\n\r\n") {
            let content_length = text[..header_end]
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if buffer.len() >= header_end + 4 + content_length {
                return text.into_owned();
            }
        }
    }
}

fn http_config(
    base_url: String,
    max_retries: u32,
    timeout_secs: u64,
) -> ai_team::config::ModelConfig {
    let mut config = fast_model("model-builder");
    config.provider = ProviderKind::Openai;
    config.base_url = Some(base_url);
    config.max_retries = max_retries;
    config.timeout_secs = timeout_secs;
    config
}

#[tokio::test]
async fn retries_server_errors_then_succeeds() {
    let (url, hits) = serve(vec![
        Reply::Status(503, "busy".to_string()),
        Reply::Status(429, "slow down".to_string()),
        Reply::Status(200, ok_body("hello")),
    ])
    .await;
    let provider = provider_for(&http_config(url, 2, 5)).unwrap();

    let answer = provider.complete("sys", "user").await.unwrap();

    assert_eq!(answer, "hello");
    assert_eq!(hits.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn gives_up_after_max_retries() {
    let (url, hits) = serve(vec![
        Reply::Status(500, "boom".to_string()),
        Reply::Status(500, "boom".to_string()),
    ])
    .await;
    let provider = provider_for(&http_config(url, 1, 5)).unwrap();

    let err = provider.complete("sys", "user").await.unwrap_err();

    assert!(format!("{err:#}").contains("after 2 attempt(s)"));
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn does_not_retry_client_errors() {
    let (url, hits) = serve(vec![
        Reply::Status(401, "bad key".to_string()),
        Reply::Status(200, ok_body("unreachable")),
    ])
    .await;
    let provider = provider_for(&http_config(url, 3, 5)).unwrap();

    let err = provider.complete("sys", "user").await.unwrap_err();

    assert!(format!("{err:#}").contains("401"));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn hung_endpoint_times_out() {
    let (url, _hits) = serve(vec![Reply::Hang]).await;
    let provider = provider_for(&http_config(url, 0, 1)).unwrap();

    let started = std::time::Instant::now();
    let err = provider.complete("sys", "user").await.unwrap_err();

    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(format!("{err:#}").contains("LLM HTTP request failed"));
}

fn request_json(raw: &str) -> serde_json::Value {
    let body = raw
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or("");
    serde_json::from_str(body).unwrap()
}

#[tokio::test]
async fn ollama_dialect_sends_native_options() {
    let reply = serde_json::json!({
        "message": {"role": "assistant", "content": " ok "},
        "done": true,
        "prompt_eval_count": 42,
        "eval_count": 7
    });
    let (origin, _hits, requests) =
        serve_capturing(vec![Reply::Status(200, reply.to_string())]).await;

    let mut config = fast_model("qwen3:4b-instruct");
    config.provider = ProviderKind::Ollama;
    config.base_url = Some(origin);
    config.num_ctx = Some(65536);
    config.num_predict = Some(1200);
    let provider = provider_for(&config).unwrap();

    let completion = provider.generate("sys", "user").await.unwrap();
    assert_eq!(completion.text, "ok");
    let usage = completion.usage.expect("Ollama reports token counts");
    assert_eq!((usage.input_tokens, usage.output_tokens), (42, 7));

    let raw = requests.lock().unwrap()[0].clone();
    assert!(raw.starts_with("POST /api/chat "), "{raw}");
    let body = request_json(&raw);
    assert_eq!(body["stream"], false);
    assert_eq!(body["options"]["num_ctx"], 65536);
    assert_eq!(body["options"]["num_predict"], 1200);
    assert_eq!(body["messages"][0]["role"], "system");
}

#[tokio::test]
async fn claude_dialect_uses_messages_api() {
    let reply = serde_json::json!({
        "stop_reason": "end_turn",
        "content": [{"type": "text", "text": "{\"decision\":\"APPROVED\",\"feedback\":\"ok\"}"}]
    });
    let (origin, _hits, requests) =
        serve_capturing(vec![Reply::Status(200, reply.to_string())]).await;

    // SAFETY: variable name unique to this test; no other test reads it.
    unsafe { std::env::set_var("AI_TEAM_TEST_CLAUDE_KEY", "test-key") };
    let mut config = fast_model("claude-opus-5-5");
    config.provider = ProviderKind::Claude;
    config.base_url = Some(origin);
    config.api_key_env = Some("AI_TEAM_TEST_CLAUDE_KEY".to_string());
    config.effort = Some("high".to_string());
    let provider = provider_for(&config).unwrap();

    let answer = provider.complete("be strict", "review this").await.unwrap();
    assert!(answer.contains("APPROVED"));

    let raw = requests.lock().unwrap()[0].clone();
    let lower = raw.to_lowercase();
    assert!(raw.starts_with("POST /v1/messages "), "{raw}");
    assert!(lower.contains("x-api-key: test-key"));
    assert!(lower.contains("anthropic-version: 2023-06-01"));
    assert!(
        !lower.contains("authorization:"),
        "claude must not use bearer auth"
    );

    let body = request_json(&raw);
    assert_eq!(body["system"], "be strict");
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["max_tokens"], 16000);
    assert_eq!(body["output_config"]["effort"], "high");
    assert!(
        body.get("temperature").is_none(),
        "claude rejects sampling parameters"
    );
}

#[tokio::test]
async fn truncated_answers_are_errors_in_every_dialect() {
    let ollama_reply = serde_json::json!({
        "message": {"role": "assistant", "content": "## Spec
    The module does not depend on other"},
        "done": true,
        "done_reason": "length"
    });
    let (origin, _hits, _requests) =
        serve_capturing(vec![Reply::Status(200, ollama_reply.to_string())]).await;
    let mut ollama = fast_model("qwen3:4b-instruct");
    ollama.provider = ProviderKind::Ollama;
    ollama.base_url = Some(origin);
    ollama.num_predict = Some(1200);
    let err = provider_for(&ollama)
        .unwrap()
        .complete("sys", "user")
        .await
        .unwrap_err();
    let message = format!("{err:#}");
    assert!(
        message.contains("cut off at num_predict (1200 tokens)"),
        "{message}"
    );

    let openai_reply = serde_json::json!({
        "choices": [{"message": {"content": "partial"}, "finish_reason": "length"}]
    });
    let (url, hits) = serve(vec![Reply::Status(200, openai_reply.to_string())]).await;
    let err = provider_for(&http_config(url, 2, 5))
        .unwrap()
        .complete("sys", "user")
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("cut off at max_tokens"));
    assert_eq!(hits.load(Ordering::SeqCst), 1, "truncation is not retried");
}

#[tokio::test]
async fn prompts_that_overflow_the_context_are_errors() {
    // Too long to fit at all: refused before anything is sent.
    let (origin, hits, _requests) = serve_capturing(vec![]).await;
    let mut config = fast_model("qwen3-coder:30b");
    config.provider = ProviderKind::Ollama;
    config.base_url = Some(origin);
    config.num_ctx = Some(2048);
    config.num_predict = Some(1024);
    let provider = provider_for(&config).unwrap();
    let error = provider
        .generate("sys", &"x".repeat(20_000))
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("prompt too large"),
        "{error:#}"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0);

    // Ollama reports a prompt that filled the context: it was truncated.
    let reply = serde_json::json!({
        "message": {"role": "assistant", "content": "ok"},
        "done": true, "done_reason": "stop",
        "prompt_eval_count": 1024, "eval_count": 3
    });
    let (origin, _hits, _requests) =
        serve_capturing(vec![Reply::Status(200, reply.to_string())]).await;
    config.base_url = Some(origin);
    let provider = provider_for(&config).unwrap();
    let error = provider.generate("sys", "short").await.unwrap_err();
    assert!(
        format!("{error:#}").contains("prompt filled the context"),
        "{error:#}"
    );
}

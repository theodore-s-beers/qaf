use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{future::Future, time::Duration};

// The only abstraction: tests can supply scripted replies to the real agent loop.
pub trait Model: Sync {
    fn respond(
        &self,
        history: &[Value],
        tools: &[Value],
    ) -> impl Future<Output = Result<Reply>> + Send;
}

pub struct Client {
    http: reqwest::Client,
    store: crate::auth::Store,
    account: String,
    model: String,
}

pub fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(180))
        .build()?)
}

impl Client {
    pub fn new(store: crate::auth::Store, account: String, model: String) -> Result<Self> {
        ensure!(!model.trim().is_empty(), "model must not be empty");
        Ok(Self {
            http: http_client()?,
            store,
            account,
            model,
        })
    }

    pub async fn models(
        store: &crate::auth::Store,
        account: &str,
    ) -> Result<Vec<(String, String)>> {
        let http = http_client()?;
        let token = store.access_token(&http, account).await?;
        let response = http
            .get("https://api.openai.com/v1/models")
            .bearer_auth(token)
            .send()
            .await?;
        let status = response.status();
        let body: Value = response.json().await.context("decode model catalog")?;
        ensure!(
            status.is_success(),
            "model catalog returned {status}: {}",
            api_error(&body)
        );
        let models = body["models"]
            .as_array()
            .context("model catalog omitted models")?;
        models
            .iter()
            .filter(|m| m["visibility"] == "list")
            .map(|m| {
                Ok((
                    m["slug"].as_str().context("model omitted slug")?.to_owned(),
                    m["display_name"].as_str().unwrap_or("").to_owned(),
                ))
            })
            .collect()
    }
}

fn request_body(model: &str, history: &[Value], tools: &[Value]) -> Value {
    json!({
        "model": model, "input": history,
        "tools": [{"type": "namespace", "name": "qaf", "description": "Local coding tools", "tools": tools}],
        "store": false, "stream": true, "include": ["reasoning.encrypted_content"],
        "parallel_tool_calls": false,
    })
}

impl Model for Client {
    async fn respond(&self, history: &[Value], tools: &[Value]) -> Result<Reply> {
        let token = self.store.access_token(&self.http, &self.account).await?;
        let response = self
            .http
            .post("https://api.openai.com/v1/responses")
            .bearer_auth(token)
            .json(&request_body(&self.model, history, tools))
            .send()
            .await
            .context("request model response")?;
        read_reply(response).await
    }
}

async fn read_reply(mut response: reqwest::Response) -> Result<Reply> {
    let status = response.status();
    if !status.is_success() {
        let body: Value = response.json().await.unwrap_or(Value::Null);
        bail!("model API returned {status}: {}", api_error(&body));
    }
    let mut stream = Events::default();
    while let Some(chunk) = response.chunk().await.context("read model stream")? {
        for event in stream.push(&chunk)? {
            if let Some(reply) = completion(&event)? {
                return Ok(reply);
            }
        }
    }
    bail!("model stream ended before response.completed; no tools were executed for this turn")
}

fn api_error(body: &Value) -> String {
    let error = body.get("error").unwrap_or(body);
    let code = error["code"].as_str().unwrap_or("unknown_error");
    match code {
        "subscription_sharing_usage_limit_exceeded" | "subscription_sharing_usage_unavailable" => {
            format!(
                "{code}: ChatGPT plan usage is unavailable or exhausted; check ChatGPT Settings > Usage"
            )
        }
        "subscription_sharing_user_not_eligible" => {
            format!("{code}: this account or workspace cannot use its ChatGPT plan here")
        }
        _ => format!(
            "{code}: {}",
            error["message"].as_str().unwrap_or("request failed")
        ),
    }
}

fn completion(event: &Value) -> Result<Option<Reply>> {
    match event["type"].as_str() {
        Some("response.completed") => {
            let body = &event["response"];
            ensure!(
                body["status"] == "completed",
                "model response did not complete"
            );
            Ok(Some(
                serde_json::from_value(body.clone()).context("parse model output")?,
            ))
        }
        Some("response.failed" | "response.incomplete") => {
            bail!("model response failed: {}", api_error(&event["response"]))
        }
        Some("error") => bail!("model stream error: {}", api_error(event)),
        _ => Ok(None),
    }
}

// SSE is byte-framed: HTTP chunks may split UTF-8 characters, lines, or events.
#[derive(Default)]
struct Events {
    line: Vec<u8>,
    data: Vec<u8>,
}

impl Events {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<Value>> {
        let mut events = Vec::new();
        for &byte in bytes {
            if byte == b'\n' {
                if self.line.last() == Some(&b'\r') {
                    self.line.pop();
                }
                if self.line.is_empty() {
                    if !self.data.is_empty() {
                        self.data.pop(); // trailing data-line newline
                        if self.data != b"[DONE]" {
                            events.push(
                                serde_json::from_slice(&self.data)
                                    .context("invalid model stream event")?,
                            );
                        }
                        self.data.clear();
                    }
                } else if let Some(data) = self.line.strip_prefix(b"data:") {
                    let data = data.strip_prefix(b" ").unwrap_or(data);
                    self.data.extend_from_slice(data);
                    self.data.push(b'\n');
                }
                self.line.clear();
            } else {
                self.line.push(byte);
            }
            ensure!(
                self.line.len() + self.data.len() <= 16 * 1024 * 1024,
                "model stream event exceeds 16 MiB"
            );
        }
        Ok(events)
    }
}

#[derive(Debug, Deserialize)]
pub struct Reply {
    // Preserve every item, including reasoning and provider metadata.
    pub output: Vec<Value>,
}

#[derive(Debug, Deserialize)]
pub struct ToolCall {
    #[serde(default)]
    pub namespace: Option<String>,
    pub call_id: String,
    pub name: String,
    pub arguments: String,
}

impl Reply {
    pub fn calls(&self) -> Result<Vec<ToolCall>> {
        self.output
            .iter()
            .filter(|item| item["type"] == "function_call")
            .map(|item| {
                let call: ToolCall =
                    serde_json::from_value(item.clone()).context("parse tool call")?;
                ensure!(
                    call.namespace.as_deref().is_none_or(|ns| ns == "qaf"),
                    "unknown tool namespace"
                );
                Ok(call)
            })
            .collect()
    }

    pub fn text(&self) -> String {
        self.output
            .iter()
            .filter(|item| item["type"] == "message")
            .filter_map(|item| item["content"].as_array())
            .flatten()
            .filter_map(|part| match part["type"].as_str() {
                Some("output_text") => part["text"].as_str(),
                Some("refusal") => part["refusal"].as_str(),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_request_has_required_flags_and_namespaced_tools() {
        let body = request_body(
            "test-model",
            &[json!({"role":"user","content":"hello"})],
            &[json!({"type":"function","name":"read_file"})],
        );
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["tools"][0]["type"], "namespace");
        assert_eq!(body["tools"][0]["name"], "qaf");
        assert_eq!(body["tools"][0]["tools"][0]["name"], "read_file");
        assert!(body.get("previous_response_id").is_none());
    }

    #[test]
    fn parses_arbitrary_chunk_boundaries_and_preserves_completed_output() {
        let reasoning = json!({"type":"reasoning","encrypted_content":"opaque","summary":[]});
        let output = vec![
            reasoning.clone(),
            json!({"type":"function_call","namespace":"qaf","name":"read_file","arguments":"{\"path\":\"café\"}","call_id":"c1"}),
        ];
        let terminal =
            json!({"type":"response.completed","response":{"status":"completed","output":output}});
        let wire = format!(
            ": keepalive\r\nevent: response.completed\r\ndata: {terminal}\r\n\r\ndata: [DONE]\r\n\r\n"
        );
        for chunk_size in [1, 2, 7, wire.len()] {
            let mut events = Events::default();
            let mut replies = Vec::new();
            for chunk in wire.as_bytes().chunks(chunk_size) {
                for event in events.push(chunk).unwrap() {
                    if let Some(reply) = completion(&event).unwrap() {
                        replies.push(reply);
                    }
                }
            }
            assert_eq!(replies.len(), 1);
            assert_eq!(replies[0].output[0], reasoning);
            assert_eq!(replies[0].calls().unwrap()[0].name, "read_file");
        }
    }

    #[test]
    fn failed_and_incomplete_responses_are_errors() {
        for event in [
            json!({"type":"response.failed","response":{"error":{"code":"subscription_sharing_usage_limit_exceeded"}}}),
            json!({"type":"response.incomplete","response":{}}),
            json!({"type":"error","message":"failed"}),
        ] {
            assert!(completion(&event).is_err());
        }
    }

    #[test]
    fn parser_rejects_malformed_json_and_buffers_partial_events() {
        let mut events = Events::default();
        assert!(events.push(b"data: invalid\n\n").is_err());
        let mut events = Events::default();
        assert_eq!(
            events
                .push(b"data: {\"type\":\"response.completed\"")
                .unwrap(),
            [] as [serde_json::Value; 0]
        );
    }

    #[test]
    fn tool_calls_reject_foreign_namespaces() {
        let reply = Reply {
            output: vec![
                json!({"type":"function_call","namespace":"foreign","name":"shell","arguments":"{}","call_id":"x"}),
            ],
        };
        assert!(reply.calls().is_err());
    }

    #[tokio::test]
    async fn eof_before_completion_fails_agent_without_executing_tools() {
        use crate::{agent::Agent, tools::Tools, transcript::Transcript};
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };

        struct StreamModel {
            http: reqwest::Client,
            endpoint: String,
        }
        impl Model for StreamModel {
            async fn respond(&self, _: &[Value], _: &[Value]) -> Result<Reply> {
                read_reply(self.http.get(&self.endpoint).send().await?).await
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::for_test(dir.path(), Duration::from_secs(1)).unwrap();
        let mut transcript = Transcript::create(&dir.path().join("log")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let model = StreamModel {
            http: http_client().unwrap(),
            endpoint: format!("http://{}", listener.local_addr().unwrap()),
        };
        let server = async {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let count = socket.read(&mut buffer).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&buffer[..count]);
            }
            let event = json!({"type": "response.output_item.done", "item": {
                "type":"function_call", "namespace":"qaf", "name":"write_file", "call_id":"c1",
                "arguments": r#"{"path":"must-not-exist","content":"unexpected"}"#
            }});
            // Deliver a full tool call, then EOF during the terminal event.
            let body = format!("data: {event}\n\ndata: {{\"type\":\"response.completed\"");
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        };
        let mut agent = Agent::new("test incomplete response", "test-model");
        let mut tool_events = 0;
        let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                agent.run(
                    &model,
                    &tools,
                    &mut transcript,
                    1,
                    &crate::permissions::Deny,
                    |event| {
                        if matches!(event, crate::agent::Event::Tool(_)) {
                            tool_events += 1;
                        }
                    }
                ),
                server
            )
        })
        .await
        .expect("stream test timed out");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("before response.completed")
        );
        assert_eq!(tool_events, 0);
        assert!(!dir.path().join("must-not-exist").exists());
    }
}

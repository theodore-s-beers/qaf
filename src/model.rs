use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    future::Future,
    sync::{Arc, RwLock},
    time::Duration,
};

pub enum Progress<'a> {
    Text(&'a str),
    Search,
}

// The only abstraction: tests can supply scripted replies to the real agent loop.
pub trait Model: Sync {
    fn select_model(&self, _name: &str) -> impl Future<Output = Result<()>> + Send {
        std::future::ready(Err(anyhow::anyhow!("model switching is unavailable")))
    }

    async fn respond_stream(
        &self,
        history: &[Value],
        tools: &[Value],
        _emit: &mut (impl FnMut(Progress<'_>) -> Result<()> + Send),
    ) -> Result<Reply> {
        self.respond(history, tools).await
    }

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
    model: Arc<RwLock<String>>,
    settings: crate::preferences::ModelSettings,
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
            model: Arc::new(RwLock::new(model)),
            settings: crate::preferences::ModelSettings::default(),
        })
    }

    pub fn with_settings(mut self, settings: crate::preferences::ModelSettings) -> Self {
        self.settings = settings;
        self
    }

    pub fn reviewer(&self, settings: crate::preferences::ModelSettings) -> Self {
        Self {
            http: self.http.clone(),
            store: self.store.clone(),
            account: self.account.clone(),
            model: settings.name.as_ref().map_or_else(
                || Arc::clone(&self.model),
                |name| Arc::new(RwLock::new(name.clone())),
            ),
            settings,
        }
    }

    fn body(&self, model: &str, history: &[Value], tools: &[Value], date: &str) -> Value {
        let mut body = request_body(model, history, tools, date);
        if let Some(effort) = &self.settings.reasoning_effort {
            body["reasoning"] = json!({"effort": effort});
        }
        body["service_tier"] = json!(if self.settings.fast {
            "priority"
        } else {
            "default"
        });
        body
    }

    fn remember_selection(&self, name: &str) -> Result<()> {
        let mut selected = self
            .model
            .write()
            .map_err(|_| anyhow::anyhow!("model selection lock poisoned"))?;
        crate::preferences::remember_model(&self.store.directory().join("preferences.json"), name)?;
        name.clone_into(&mut selected);
        drop(selected);
        Ok(())
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

async fn local_date() -> Result<String> {
    let output = tokio::process::Command::new("/bin/date")
        .arg("+%Y-%m-%d")
        .env("LC_ALL", "C")
        .kill_on_drop(true)
        .output()
        .await
        .context("read local calendar date")?;
    ensure!(
        output.status.success(),
        "could not read local calendar date"
    );
    let date = String::from_utf8(output.stdout).context("decode local date")?;
    Ok(date.trim().to_owned())
}

fn request_body(model: &str, history: &[Value], tools: &[Value], date: &str) -> Value {
    let mut input = vec![
        json!({"role":"developer", "content":format!("Current local date: {date} (in the user's system timezone). Use this date when interpreting today; it takes precedence over a fallback current date.")}),
    ];
    input.extend_from_slice(history);
    json!({
        "model": model, "input": input,
        "tools": if tools.is_empty() { vec![] } else { vec![json!({"type": "namespace", "name": "qaf", "description": "Local coding tools", "tools": tools}), json!({"type":"web_search"})] },
        "store": false, "stream": true, "include": ["reasoning.encrypted_content"],
        "parallel_tool_calls": false,
        "context_management": [{"type":"compaction", "compact_threshold":crate::context::COMPACT_AT}],
    })
}

impl Model for Client {
    async fn select_model(&self, name: &str) -> Result<()> {
        let available = Self::models(&self.store, &self.account).await?;
        ensure!(
            available.iter().any(|(slug, _)| slug == name),
            "model {name:?} is not in your account's catalog; run qaf models for choices"
        );
        self.remember_selection(name)
    }

    async fn respond(&self, history: &[Value], tools: &[Value]) -> Result<Reply> {
        self.respond_stream(history, tools, &mut |_| Ok(())).await
    }

    async fn respond_stream(
        &self,
        history: &[Value],
        tools: &[Value],
        emit: &mut (impl FnMut(Progress<'_>) -> Result<()> + Send),
    ) -> Result<Reply> {
        let token = self.store.access_token(&self.http, &self.account).await?;
        let selected = self
            .model
            .read()
            .map_err(|_| anyhow::anyhow!("model selection lock poisoned"))?
            .clone();
        let date = local_date().await?;
        let body = self.body(&selected, history, tools, &date);
        let response = self
            .http
            .post("https://api.openai.com/v1/responses")
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .context("request model response")?;
        read_reply_stream(response, emit).await
    }
}

#[cfg(test)]
async fn read_reply(response: reqwest::Response) -> Result<Reply> {
    read_reply_stream(response, &mut |_| Ok(())).await
}

async fn read_reply_stream(
    mut response: reqwest::Response,
    emit: &mut (impl FnMut(Progress<'_>) -> Result<()> + Send),
) -> Result<Reply> {
    let status = response.status();
    if !status.is_success() {
        let body: Value = response.json().await.unwrap_or(Value::Null);
        bail!("model API returned {status}: {}", api_error(&body));
    }
    let mut stream = Events::default();
    let mut output = StreamOutput::default();
    while let Some(chunk) = response.chunk().await.context("read model stream")? {
        for event in stream.push(&chunk)? {
            if let Some(reply) = output.accept(&event, emit)? {
                return Ok(reply);
            }
        }
    }
    bail!(
        "model stream ended before response.completed; no local tools were executed for this turn"
    )
}

#[derive(Default)]
struct StreamOutput {
    items: BTreeMap<u64, Value>,
    last_text: Option<(u64, u64)>,
    bytes: usize,
}

impl StreamOutput {
    fn accept(
        &mut self,
        event: &Value,
        emit: &mut impl FnMut(Progress<'_>) -> Result<()>,
    ) -> Result<Option<Reply>> {
        match event["type"].as_str() {
            Some("response.output_text.delta" | "response.refusal.delta") => {
                let delta = event["delta"].as_str().context("text delta omitted text")?;
                let key = (
                    event["output_index"]
                        .as_u64()
                        .context("text delta omitted output index")?,
                    event["content_index"]
                        .as_u64()
                        .context("text delta omitted content index")?,
                );
                if self.last_text.is_some_and(|last| last != key) {
                    emit(Progress::Text("\n"))?;
                }
                self.last_text = Some(key);
                emit(Progress::Text(delta))?;
            }
            Some("response.web_search_call.searching") => emit(Progress::Search)?,
            Some("response.output_item.done") => {
                let index = event["output_index"]
                    .as_u64()
                    .context("completed item omitted output index")?;
                let item = &event["item"];
                ensure!(item.is_object(), "completed item omitted payload");
                self.bytes += item.to_string().len();
                ensure!(
                    self.bytes <= 32 * 1024 * 1024,
                    "model output exceeds 32 MiB"
                );
                ensure!(
                    !self.items.contains_key(&index),
                    "duplicate completed output item"
                );
                self.items.insert(index, item.clone());
            }
            _ => {}
        }
        if let Some(mut reply) = completion(event)? {
            // Some subscription responses carry items only in output_item.done.
            // When populated, the terminal output remains authoritative.
            if reply.output.is_empty() {
                for (expected, index) in self.items.keys().enumerate() {
                    ensure!(
                        expected as u64 == *index,
                        "model stream omitted an output item"
                    );
                }
                reply.output = std::mem::take(&mut self.items).into_values().collect();
            }
            return Ok(Some(reply));
        }
        Ok(None)
    }
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
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Deserialize, serde::Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
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

    pub fn citations(&self) -> String {
        let text = self.text();
        let mut urls = Vec::new();
        for part in self
            .output
            .iter()
            .filter_map(|item| item["content"].as_array())
            .flatten()
        {
            for annotation in part["annotations"].as_array().into_iter().flatten() {
                if annotation["type"] == "url_citation"
                    && let Some(url) = annotation["url"]
                        .as_str()
                        .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
                    && !text.contains(url)
                    && !urls.contains(&url)
                {
                    urls.push(url);
                }
            }
        }
        if urls.is_empty() {
            String::new()
        } else {
            format!(
                "\n\nSources:\n{}",
                urls.iter()
                    .map(|url| format!("- <{url}>"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        }
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
    fn switching_remembers_selection_and_preserves_it_when_save_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(
            &path,
            r#"{"model":"first","approvals_reviewer":"auto_review"}"#,
        )
        .unwrap();
        let client = Client::new(
            crate::auth::Store::new(Some(dir.path().to_owned())).unwrap(),
            "default".into(),
            "first".into(),
        )
        .unwrap();
        let pinned = Client::new(
            crate::auth::Store::new(Some(dir.path().to_owned())).unwrap(),
            "default".into(),
            "review-model".into(),
        )
        .unwrap();
        let reviewer_client = client.reviewer(crate::preferences::ModelSettings {
            fast: true,
            ..Default::default()
        });
        let primary_body = client.body("first", &[], &[], "2026-11-09");
        let reviewer_body = reviewer_client.body("first", &[], &[], "2026-11-09");
        assert_eq!(primary_body["service_tier"], "default");
        assert_eq!(reviewer_body["service_tier"], "priority");
        assert!(reviewer_body.get("reasoning").is_none());
        let configured = client.reviewer(crate::preferences::ModelSettings {
            name: Some("fixed".into()),
            reasoning_effort: Some("medium".into()),
            fast: false,
        });
        assert_eq!(
            configured.body("fixed", &[], &[], "2026-11-09")["reasoning"]["effort"],
            "medium"
        );
        client.remember_selection("second").unwrap();
        assert_eq!(*reviewer_client.model.read().unwrap(), "second");
        assert_eq!(*pinned.model.read().unwrap(), "review-model");
        assert_eq!(*configured.model.read().unwrap(), "fixed");
        assert_eq!(
            crate::preferences::select_model(&path, None).unwrap(),
            "second"
        );
        assert_eq!(
            crate::preferences::reviewer(&path).unwrap(),
            crate::preferences::Reviewer::AutoReview
        );
        std::fs::write(&path, "broken").unwrap();
        assert!(client.remember_selection("third").is_err());
        assert_eq!(*client.model.read().unwrap(), "second");
        assert_eq!(std::fs::read_to_string(path).unwrap(), "broken");
    }

    #[test]
    fn subscription_request_has_required_flags_and_namespaced_tools() {
        let body = request_body(
            "test-model",
            &[json!({"role":"user","content":"hello"})],
            &[json!({"type":"function","name":"read_file"})],
            "2026-11-08",
        );
        assert_eq!(body["input"][0]["role"], "developer");
        assert!(
            body["input"][0]["content"]
                .as_str()
                .unwrap()
                .contains("Current local date: 2026-11-08")
        );
        assert_eq!(body["input"][1], json!({"role":"user","content":"hello"}));
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert_eq!(
            body["context_management"][0]["compact_threshold"],
            crate::context::COMPACT_AT
        );
        assert_eq!(body["tools"][0]["type"], "namespace");
        assert_eq!(body["tools"][0]["name"], "qaf");
        assert_eq!(body["tools"][0]["tools"][0]["name"], "read_file");
        assert_eq!(body["tools"][1]["type"], "web_search");
        assert_eq!(
            request_body("reviewer", &[], &[], "2026-11-08")["tools"],
            json!([])
        );
        assert!(body.get("previous_response_id").is_none());
    }

    #[test]
    fn parses_arbitrary_chunk_boundaries_and_preserves_completed_output() {
        let reasoning = json!({"type":"reasoning","encrypted_content":"opaque","summary":[]});
        let output = vec![
            reasoning.clone(),
            json!({"type":"function_call","namespace":"qaf","name":"read_file","arguments":"{\"path\":\"café\"}","call_id":"c1"}),
        ];
        let terminal = json!({"type":"response.completed","response":{"status":"completed","output":output,"usage":{"input_tokens":123,"output_tokens":45}}});
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
            assert_eq!(replies[0].usage.as_ref().unwrap().input_tokens, 123);
            assert_eq!(replies[0].usage.as_ref().unwrap().output_tokens, 45);
            assert_eq!(replies[0].output[0], reasoning);
            assert_eq!(replies[0].calls().unwrap()[0].name, "read_file");
        }
    }

    #[test]
    fn streaming_search_preserves_items_citations_and_does_not_duplicate_output() {
        let search = json!({"type":"web_search_call", "status":"completed", "action":{"type":"search","query":"Rust"}});
        let message = json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"A café.","annotations":[{"type":"url_citation","url":"https://example.com/source","title":"Source"}]}]});
        for terminal_output in [vec![], vec![search.clone(), message.clone()]] {
            let events = [
                json!({"type":"response.web_search_call.searching"}),
                json!({"type":"response.output_item.done","output_index":0,"item":search}),
                json!({"type":"response.output_text.delta","output_index":1,"content_index":0,"delta":"A café."}),
                json!({"type":"response.output_item.done","output_index":1,"item":message}),
                json!({"type":"response.completed","response":{"status":"completed","output":terminal_output}}),
            ];
            let mut output = StreamOutput::default();
            let mut text = String::new();
            let mut searches = 0;
            for event in &events[..4] {
                assert!(
                    output
                        .accept(event, &mut |progress| {
                            match progress {
                                Progress::Text(delta) => text.push_str(delta),
                                Progress::Search => searches += 1,
                            }
                            Ok(())
                        })
                        .unwrap()
                        .is_none()
                );
            }
            assert_eq!(text, "A café.");
            assert_eq!(searches, 1);
            let reply = output
                .accept(&events[4], &mut |_| panic!("duplicate progress"))
                .unwrap()
                .unwrap();
            assert_eq!(reply.output, vec![search.clone(), message.clone()]);
            assert_eq!(reply.text(), text);
            assert!(reply.citations().contains("https://example.com/source"));
        }
        let mut output = StreamOutput::default();
        output
            .accept(
                &json!({"type":"response.output_item.done","output_index":1,"item":message}),
                &mut |_| Ok(()),
            )
            .unwrap();
        assert!(output.accept(&json!({"type":"response.completed","response":{"status":"completed","output":[]}}), &mut |_| Ok(())).is_err());
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
            usage: None,
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
            let event = json!({"type": "response.output_item.done", "output_index":0, "item": {
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

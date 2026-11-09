use crate::{
    model::Model,
    permissions::{Access, Approver, Decision, Request},
    tools::Tools,
    transcript::Transcript,
};
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};

pub enum Event<'a> {
    Text(&'a str),
    Approval,
    Tool(&'a crate::tools::Tool),
}

pub struct Agent {
    history: Vec<Value>,
}

impl Agent {
    pub fn new(prompt: &str, requested_model: &str) -> Self {
        let instructions =
            include_str!("../SYSTEM_PROMPT.md").replace("{{REQUESTED_MODEL}}", requested_model);
        Self {
            history: vec![
                json!({"role": "developer", "content": instructions}),
                json!({"role": "user", "content": prompt}),
            ],
        }
    }

    pub fn add_prompt(&mut self, prompt: &str) {
        self.history
            .push(json!({"role": "user", "content": prompt}));
    }

    /// Close unanswered calls before another user turn, without replaying tools.
    pub fn stop(&mut self, transcript: &mut Transcript) -> Result<()> {
        let mut pending = Vec::new();
        for item in &self.history {
            match item["type"].as_str() {
                Some("function_call") => pending.push(item["call_id"].clone()),
                Some("function_call_output") => pending.retain(|id| *id != item["call_id"]),
                _ => {}
            }
        }
        for call_id in pending {
            let item = json!({"type": "function_call_output", "call_id": call_id,
                "output": json!({"error": "Task stopped before a result was received. This tool may not have run, or may have partially executed. Check state before retrying."}).to_string()});
            self.history.push(item.clone());
            transcript.record("tool_result", &item)?;
        }
        let note = json!({"role": "developer", "content": "The previous task stopped before completion. No further tools from that task will run. Changes already made were not rolled back. Wait for the user's next instruction."});
        self.history.push(note.clone());
        transcript.record("task_stopped", &note)
    }

    pub async fn run(
        &mut self,
        model: &impl Model,
        tools: &Tools,
        transcript: &mut Transcript,
        max_turns: u32,
        approver: &impl Approver,
        mut emit: impl FnMut(Event<'_>) + Send,
    ) -> Result<()> {
        transcript.record("input", &json!(self.history))?;
        let definitions = Tools::definitions();
        for _ in 0..max_turns {
            let reply = model.respond(&self.history, &definitions).await?;
            transcript.record("model", &json!(reply.output))?;
            let calls = reply.calls()?;
            let text = reply.text();
            self.history.extend(reply.output);
            if !text.is_empty() {
                emit(Event::Text(&text));
            }
            if calls.is_empty() {
                ensure!(
                    !text.is_empty(),
                    "model returned neither text nor tool calls"
                );
                return Ok(());
            }
            for call in calls {
                transcript.record("tool_start", &json!({"call_id": call.call_id, "name": call.name, "arguments": call.arguments}))?;
                let output = match Request::parse(&call.name, &call.arguments) {
                    Ok(request) => {
                        let review = if request.access == Access::Unrestricted {
                            let mut description = request.description(tools.directory());
                            description["call_id"] = json!(call.call_id);
                            transcript.record("approval_request", &description)?;
                            emit(Event::Approval);
                            let review = approver
                                .review(&description, &self.history, transcript)
                                .await?;
                            transcript.record(
                                "approval_decision",
                                &json!({"call_id": call.call_id, "decision": review.decision, "rationale": review.rationale}),
                            )?;
                            review
                        } else {
                            crate::permissions::Review {
                                decision: Decision::AllowOnce,
                                rationale: String::new(),
                            }
                        };
                        match review.decision {
                            Decision::AllowOnce => {
                                emit(Event::Tool(&request.tool));
                                tools.run(&request).await
                            }
                            Decision::Deny => {
                                json!({"error": "Broader access denied. Do not work around this denial. Choose a materially safer alternative or ask the user.", "reason": review.rationale})
                            }
                            Decision::Cancel => bail!("run cancelled during approval"),
                        }
                    }
                    Err(error) => json!({"error": format!("{error:#}")}),
                };
                let item = json!({"type": "function_call_output", "call_id": call.call_id, "output": output.to_string()});
                transcript.record("tool_result", &item)?;
                self.history.push(item);
            }
        }
        bail!("reached the limit of {max_turns} model turns")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Reply;
    use std::{collections::VecDeque, sync::Mutex, time::Duration};

    struct Scripted {
        replies: Mutex<VecDeque<Reply>>,
        requests: Mutex<Vec<Vec<Value>>>,
    }

    impl Scripted {
        fn new(outputs: Vec<Vec<Value>>) -> Self {
            Self {
                replies: Mutex::new(outputs.into_iter().map(|output| Reply { output }).collect()),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl Model for Scripted {
        fn respond(
            &self,
            history: &[Value],
            _: &[Value],
        ) -> impl std::future::Future<Output = Result<Reply>> + Send {
            self.requests.lock().unwrap().push(history.to_vec());
            std::future::ready(
                self.replies
                    .lock()
                    .unwrap()
                    .pop_front()
                    .ok_or_else(|| anyhow::anyhow!("script exhausted")),
            )
        }
    }

    fn call(id: &str, name: &str, arguments: &Value) -> Value {
        json!({"type": "function_call", "call_id": id, "name": name, "arguments": arguments.to_string()})
    }

    fn answer() -> Value {
        json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Fixed and verified."}]})
    }

    #[tokio::test]
    async fn auto_review_gates_execution_and_reports_denials() {
        for allowed in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let tools = Tools::for_test(dir.path(), Duration::from_secs(5)).unwrap();
            let path = dir.path().join("transcript.jsonl");
            let mut transcript = Transcript::create(&path).unwrap();
            let verdict = if allowed { "allow" } else { "deny" };
            let model = Scripted::new(vec![
                vec![call(
                    "c1",
                    "shell",
                    &json!({"command":"printf reviewed > result.txt", "access":"unrestricted", "justification":"Requested file"}),
                )],
                vec![
                    json!({"type":"message", "content":[{"type":"output_text", "text":json!({"decision":verdict, "rationale":"Review explanation"}).to_string()}]}),
                ],
                vec![answer()],
            ]);
            let approver = crate::review::Approval {
                mode: crate::preferences::Reviewer::AutoReview,
                model: &model,
                human: &crate::permissions::Deny,
                config_directory: dir.path(),
            };
            Agent::new("Create result.txt", "test")
                .run(&model, &tools, &mut transcript, 2, &approver, |_| {})
                .await
                .unwrap();
            assert_eq!(dir.path().join("result.txt").exists(), allowed);
            let requests = model.requests.lock().unwrap();
            let tool_result = requests[2]
                .iter()
                .find(|item| item["type"] == "function_call_output")
                .unwrap();
            if !allowed {
                assert!(
                    tool_result["output"]
                        .as_str()
                        .unwrap()
                        .contains("Review explanation")
                );
            }
            drop(requests);
            let log = std::fs::read_to_string(path).unwrap();
            assert!(log.contains("auto_review_request"));
            assert!(log.contains("auto_review_result"));
            assert!(log.contains("approval_decision"));
        }
    }

    #[tokio::test]
    async fn repairs_fixture_recovers_from_error_and_preserves_history() {
        let dir = tempfile::tempdir().unwrap();
        let source = "fn add(a: i32, b: i32) -> i32 { a - b }\n#[test] fn adds() { assert_eq!(add(2, 3), 5); }\n";
        std::fs::write(dir.path().join("fixture.rs"), source).unwrap();
        let reasoning =
            json!({"type": "reasoning", "id": "r1", "summary": [], "encrypted_content": "opaque"});
        let model = Scripted::new(vec![
            vec![
                reasoning.clone(),
                call("missing", "read_file", &json!({"path": "missing.rs"})),
            ],
            vec![
                call("read", "read_file", &json!({"path": "fixture.rs"})),
                call(
                    "before",
                    "shell",
                    &json!({"command": "rustc --test fixture.rs -o fixture-test && ./fixture-test"}),
                ),
            ],
            vec![call(
                "edit",
                "edit_file",
                &json!({"path": "fixture.rs", "old": "a - b", "new": "a + b"}),
            )],
            vec![call(
                "after",
                "shell",
                &json!({"command": "rustc --test fixture.rs -o fixture-test && ./fixture-test"}),
            )],
            vec![answer()],
        ]);
        let tools = Tools::for_test(dir.path(), Duration::from_secs(30)).unwrap();
        let path = dir.path().join("transcript.jsonl");
        let mut transcript = Transcript::create(&path).unwrap();
        let mut agent = Agent::new("Fix the test", "test-model");
        agent
            .run(
                &model,
                &tools,
                &mut transcript,
                5,
                &crate::permissions::Deny,
                |_| {},
            )
            .await
            .unwrap();
        assert!(
            std::fs::read_to_string(dir.path().join("fixture.rs"))
                .unwrap()
                .contains("a + b")
        );
        let results: Vec<_> = agent
            .history
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .map(|item| {
                (
                    item["call_id"].as_str().unwrap(),
                    serde_json::from_str::<Value>(item["output"].as_str().unwrap()).unwrap(),
                )
            })
            .collect();
        assert!(
            results
                .iter()
                .find(|(id, _)| *id == "missing")
                .unwrap()
                .1
                .get("error")
                .is_some()
        );
        assert_ne!(
            results.iter().find(|(id, _)| *id == "before").unwrap().1["exit_code"],
            0
        );
        assert_eq!(
            results.iter().find(|(id, _)| *id == "after").unwrap().1["exit_code"],
            0
        );
        let requests = model.requests.lock().unwrap();
        assert!(requests[1].contains(&reasoning));
        assert_eq!(requests[1].last().unwrap()["call_id"], "missing");
        let records: Vec<Value> = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            records
                .iter()
                .filter(|r| r["kind"] == "tool_result")
                .count(),
            5
        );
    }

    #[tokio::test]
    async fn stops_at_turn_limit() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::for_test(dir.path(), Duration::from_secs(1)).unwrap();
        let mut transcript = Transcript::create(&dir.path().join("log")).unwrap();
        let model = Scripted::new(vec![vec![call(
            "1",
            "read_file",
            &json!({"path": "missing"}),
        )]]);
        let error = Agent::new("loop", "test-model")
            .run(
                &model,
                &tools,
                &mut transcript,
                1,
                &crate::permissions::Deny,
                |_| {},
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("limit of 1"));
        assert_eq!(model.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn propagates_model_failure_and_rejects_empty_completion() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::for_test(dir.path(), Duration::from_secs(1)).unwrap();
        let mut transcript = Transcript::create(&dir.path().join("log")).unwrap();
        let error = Agent::new("task", "test-model")
            .run(
                &Scripted::new(vec![]),
                &tools,
                &mut transcript,
                1,
                &crate::permissions::Deny,
                |_| {},
            )
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "script exhausted");
        let error = Agent::new("task", "test-model")
            .run(
                &Scripted::new(vec![vec![]]),
                &tools,
                &mut transcript,
                1,
                &crate::permissions::Deny,
                |_| {},
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("neither text nor tool calls"));
    }

    #[tokio::test]
    async fn broader_access_requires_a_fresh_decision_and_records_denial() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Human {
            calls: AtomicUsize,
        }
        impl Approver for Human {
            fn approve(
                &self,
                request: &Value,
            ) -> impl std::future::Future<Output = Result<Decision>> + Send {
                assert_eq!(request["access"], "unrestricted");
                assert_eq!(request["operation"]["name"], "write_file");
                let decision = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Decision::AllowOnce
                } else {
                    Decision::Deny
                };
                std::future::ready(Ok(decision))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::for_test(dir.path(), Duration::from_secs(1)).unwrap();
        let path = dir.path().join("log");
        let mut transcript = Transcript::create(&path).unwrap();
        let model = Scripted::new(vec![
            vec![call(
                "first",
                "write_file",
                &json!({"path":"first","content":"allowed",
                "access":"unrestricted","justification":"test"}),
            )],
            vec![call(
                "second",
                "write_file",
                &json!({"path":"second","content":"denied",
                "access":"unrestricted","justification":"test"}),
            )],
            vec![answer()],
        ]);
        let human = Human {
            calls: AtomicUsize::new(0),
        };
        let mut agent = Agent::new("test approvals", "test-model");
        agent
            .run(&model, &tools, &mut transcript, 3, &human, |_| {})
            .await
            .unwrap();
        assert_eq!(human.calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("first")).unwrap(),
            "allowed"
        );
        assert!(!dir.path().join("second").exists());
        let history = model.requests.lock().unwrap();
        assert!(
            history[2].last().unwrap()["output"]
                .as_str()
                .unwrap()
                .contains("denied")
        );
        drop(history);
        let records: Vec<Value> = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let decisions: Vec<_> = records
            .iter()
            .filter(|r| r["kind"] == "approval_decision")
            .map(|r| {
                (
                    r["data"]["call_id"].as_str().unwrap(),
                    r["data"]["decision"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(decisions, [("first", "allow_once"), ("second", "deny")]);
    }

    #[tokio::test]
    async fn cancelling_approval_stops_without_running_the_tool() {
        struct Cancel;
        impl Approver for Cancel {
            fn approve(
                &self,
                _: &Value,
            ) -> impl std::future::Future<Output = Result<Decision>> + Send {
                std::future::ready(Ok(Decision::Cancel))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::for_test(dir.path(), Duration::from_secs(1)).unwrap();
        let mut transcript = Transcript::create(&dir.path().join("log")).unwrap();
        let model = Scripted::new(vec![vec![call(
            "1",
            "write_file",
            &json!({
                "path":"never","content":"bad","access":"unrestricted","justification":"test"
            }),
        )]]);
        let error = Agent::new("test", "test-model")
            .run(&model, &tools, &mut transcript, 2, &Cancel, |_| {})
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert!(!dir.path().join("never").exists());
        assert_eq!(model.requests.lock().unwrap().len(), 1);
    }
    #[tokio::test]
    async fn followups_preserve_context_and_get_a_fresh_turn_budget() {
        let root = tempfile::tempdir().unwrap();
        let tools = Tools::for_test(root.path(), Duration::from_secs(1)).unwrap();
        let mut transcript = Transcript::create(&root.path().join("log")).unwrap();
        let model = Scripted::new(vec![vec![answer()], vec![answer()]]);
        let mut agent = Agent::new("First task", "test-model");
        agent
            .run(
                &model,
                &tools,
                &mut transcript,
                1,
                &crate::permissions::Deny,
                |_| {},
            )
            .await
            .unwrap();
        agent.add_prompt("Follow up");
        agent
            .run(
                &model,
                &tools,
                &mut transcript,
                1,
                &crate::permissions::Deny,
                |_| {},
            )
            .await
            .unwrap();
        let inputs = model.requests.lock().unwrap().clone();
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[1][1]["content"], "First task");
        assert_eq!(inputs[1][2], answer());
        assert_eq!(inputs[1][3]["content"], "Follow up");
    }

    #[tokio::test]
    async fn dropped_task_closes_pending_calls_without_replaying_completed_tools() {
        struct Waiting(tokio::sync::Notify);
        impl Approver for Waiting {
            async fn approve(&self, _: &Value) -> Result<Decision> {
                self.0.notify_one();
                std::future::pending().await
            }
        }
        let root = tempfile::tempdir().unwrap();
        let tools = Tools::for_test(root.path(), Duration::from_secs(1)).unwrap();
        let mut transcript = Transcript::create(&root.path().join("log")).unwrap();
        let calls = ["completed", "waiting", "unstarted"].map(|id| {
            json!({
                "type":"function_call", "name":"write_file", "call_id":id,
                "arguments": json!({"path":id,"content":"original",
                    "access": if id == "waiting" { "unrestricted" } else { "sandbox" },
                    "justification":"test"}).to_string()
            })
        });
        let model = Scripted::new(vec![calls.to_vec(), vec![answer()]]);
        let mut agent = Agent::new("First task", "test-model");
        let waiting = Waiting(tokio::sync::Notify::new());
        tokio::select! {
            result = agent.run(&model, &tools, &mut transcript, 2, &waiting, |_| {}) => panic!("unexpected completion: {result:?}"),
            () = waiting.0.notified() => {}
        }
        agent.stop(&mut transcript).unwrap();
        assert!(root.path().join("completed").exists());
        assert!(!root.path().join("waiting").exists());
        assert!(!root.path().join("unstarted").exists());
        std::fs::write(root.path().join("completed"), "changed by user").unwrap();
        agent.add_prompt("Continue with a different task");
        agent
            .run(
                &model,
                &tools,
                &mut transcript,
                1,
                &crate::permissions::Deny,
                |_| {},
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("completed")).unwrap(),
            "changed by user"
        );
        let inputs = model.requests.lock().unwrap().clone();
        let outputs: Vec<_> = inputs[1]
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .collect();
        assert_eq!(outputs.len(), 3);
        assert_eq!(outputs[0]["call_id"], "completed");
        assert!(outputs[0]["output"].as_str().unwrap().contains("true"));
        for (output, id) in outputs[1..].iter().zip(["waiting", "unstarted"]) {
            assert_eq!(output["call_id"], id);
            assert!(output["output"].as_str().unwrap().contains("Task stopped"));
        }
    }
}

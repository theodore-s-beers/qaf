use crate::{
    permissions::{Access, Request},
    sandbox::Sandbox,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

const OUTPUT_LIMIT: usize = 32 * 1024;
const FILE_LIMIT: u64 = 1024 * 1024;

pub struct Tools {
    directory: PathBuf,
    timeout: Duration,
    sandbox: Option<Sandbox>,
}

#[derive(Deserialize, Serialize)]
#[serde(
    tag = "name",
    content = "arguments",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Tool {
    ReadFile {
        path: String,
    },
    WriteFile {
        path: String,
        content: String,
    },
    EditFile {
        path: String,
        old: String,
        new: String,
    },
    Shell {
        command: String,
    },
}

impl Tools {
    pub async fn new(directory: &Path, timeout: Duration, credentials: &Path) -> Result<Self> {
        let sandbox = Sandbox::new(directory, credentials, &std::env::current_exe()?)?;
        sandbox.check().await?;
        Ok(Self {
            directory: sandbox.project().to_owned(),
            timeout,
            sandbox: Some(sandbox),
        })
    }

    #[cfg(test)]
    pub fn for_test(directory: &Path, timeout: Duration) -> Result<Self> {
        Ok(Self {
            directory: directory.canonicalize()?,
            timeout,
            sandbox: None,
        })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn definitions() -> Vec<Value> {
        [
            ("read_file", "Read a UTF-8 file, up to 1 MiB; output is capped at 32 KiB. Use shell for selected lines of larger files.", vec!["path"]),
            ("write_file", "Create or overwrite a UTF-8 file. Parent directory must exist.", vec!["path", "content"]),
            ("edit_file", "Replace exactly one occurrence of old with new. Fails for missing or ambiguous matches.", vec!["path", "old", "new"]),
            ("shell", "Run a command with /bin/sh in the working directory. Returns exit code, stdout, stderr, and timeout status. Output is capped.", vec!["command"]),
        ].into_iter().map(|(name, description, fields)| {
            let mut properties: serde_json::Map<String, Value> = fields.iter().map(|field| (field.to_string(), json!({"type": "string"}))).collect();
            properties.insert("access".into(), json!({"type": "string", "enum": ["sandbox", "unrestricted"],
                "description": "Use sandbox normally. Unrestricted requests require human approval before execution; never retry automatically."}));
            properties.insert("justification".into(), json!({"type": ["string", "null"],
                "description": "Explain why broader access is needed for an unrestricted request; otherwise null."}));
            let required: Vec<_> = fields.into_iter().chain(["access", "justification"]).collect();
            json!({"type": "function", "name": name, "description": description, "strict": true,
                "parameters": {"type": "object", "properties": properties, "required": required, "additionalProperties": false}})
        }).collect()
    }

    #[cfg(test)]
    pub async fn execute(&self, name: &str, arguments: &str) -> Value {
        match Request::parse(name, arguments) {
            Ok(request) => self.run(&request).await,
            Err(error) => json!({"error": format!("{error:#}")}),
        }
    }

    // The caller must obtain human approval before passing Unrestricted.
    pub async fn run(&self, request: &Request) -> Value {
        let mut output = match self.execute_inner(request).await {
            Ok(result) => result,
            Err(error) => json!({"error": format!("{error:#}")}),
        };
        annotate_failure(
            &mut output,
            self.sandbox.is_some() && request.access == Access::Sandbox,
            self.timeout,
        );
        output
    }

    async fn execute_inner(&self, request: &Request) -> Result<Value> {
        let unrestricted = request.access == Access::Unrestricted;
        if let Some(sandbox) = &self.sandbox {
            match &request.tool {
                Tool::Shell { command } => {
                    run_command(
                        sandbox.shell_command(command, unrestricted)?,
                        self.timeout,
                        OUTPUT_LIMIT,
                    )
                    .await
                }
                tool => {
                    let payload = serde_json::to_vec(tool)?;
                    let (command, _request_file) = sandbox.file_command(&payload, unrestricted)?;
                    let result = run_command(command, self.timeout, 256 * 1024).await?;
                    ensure!(result["timed_out"] == false, "file tool timed out");
                    ensure!(
                        result["exit_code"] == 0,
                        "file worker failed: {}",
                        result["stderr"]
                    );
                    ensure!(
                        result["stdout_truncated"] == false,
                        "file worker response exceeded output limit"
                    );
                    serde_json::from_str(
                        result["stdout"].as_str().context("missing worker output")?,
                    )
                    .context("invalid file worker response")
                }
            }
        } else {
            // Only unit tests construct Tools without a sandbox.
            #[cfg(test)]
            {
                match &request.tool {
                    Tool::Shell { command } => self.shell(command).await,
                    tool => execute_file(tool, &self.directory),
                }
            }
            #[cfg(not(test))]
            anyhow::bail!("sandbox is not configured")
        }
    }

    #[cfg(test)]
    async fn shell(&self, script: &str) -> Result<Value> {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(script)
            .current_dir(&self.directory)
            .stdin(Stdio::null());
        run_command(command, self.timeout, OUTPUT_LIMIT).await
    }
}

fn annotate_failure(output: &mut Value, sandboxed: bool, timeout: Duration) {
    let failed = output.get("error").is_some()
        || output["timed_out"] == true
        || output.get("exit_code").is_some_and(|code| code != 0);
    if !failed {
        return;
    }
    output["execution_context"] = json!({
        "access": if sandboxed { "sandbox" } else { "unrestricted" },
        "timeout_seconds": timeout.as_secs_f64(),
    });
    if sandboxed {
        output["diagnostic_hint"] = json!(
            "This operation ran sandboxed: no network access, writes limited to the project and private scratch, qaf configuration hidden, repository metadata read-only. These restrictions may explain the failure or timeout, but this is not a confirmed sandbox denial. Inspect stdout/stderr and the operation before requesting unrestricted access with a justification. Do not repeat a potentially partially completed operation without checking its effects."
        );
    }
}

async fn run_command(mut command: Command, timeout: Duration, limit: usize) -> Result<Value> {
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().context("start shell")?;
    #[cfg(unix)]
    let group = ProcessGroup(
        rustix::process::Pid::from_raw(i32::try_from(child.id().context("missing child PID")?)?)
            .context("invalid child PID")?,
    );
    let mut stdout = child.stdout.take().context("missing stdout")?;
    let mut stderr = child.stderr.take().context("missing stderr")?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let execution = async {
        let (status, out_truncated, err_truncated) = tokio::try_join!(
            child.wait(),
            drain(&mut stdout, &mut out, limit),
            drain(&mut stderr, &mut err, limit)
        )?;
        Ok::<_, std::io::Error>((status, out_truncated, err_truncated))
    };
    let (code, timed_out, out_truncated, err_truncated) =
        if let Ok(result) = tokio::time::timeout(timeout, execution).await {
            let (status, out_truncated, err_truncated) = result.context("run shell")?;
            (status.code(), false, out_truncated, err_truncated)
        } else {
            #[cfg(unix)]
            group.kill();
            let _ = child.kill().await;
            let _ = child.wait().await;
            (None, true, out.len() >= limit, err.len() >= limit)
        };
    Ok(json!({
        "exit_code": code, "timed_out": timed_out,
        "stdout": String::from_utf8_lossy(&out), "stderr": String::from_utf8_lossy(&err),
        "stdout_truncated": out_truncated, "stderr_truncated": err_truncated,
    }))
}

fn execute_file(tool: &Tool, directory: &Path) -> Result<Value> {
    match tool {
        Tool::ReadFile { path } => {
            let content = read_text(&directory.join(path))?;
            Ok(json!({"content": clip(&content), "truncated": content.len() > OUTPUT_LIMIT}))
        }
        Tool::WriteFile { path, content } => {
            std::fs::write(directory.join(path), content).context("write file")?;
            Ok(json!({"ok": true}))
        }
        Tool::EditFile { path, old, new } => {
            ensure!(!old.is_empty(), "old must not be empty");
            let path = directory.join(path);
            let content = read_text(&path)?;
            ensure!(
                content.matches(old).count() == 1,
                "old must match exactly once; file unchanged"
            );
            std::fs::write(path, content.replacen(old, new, 1)).context("edit file")?;
            Ok(json!({"ok": true}))
        }
        Tool::Shell { .. } => anyhow::bail!("file worker cannot execute shell requests"),
    }
}

/// Internal worker entrypoint. It inherits OS restrictions from its launcher.
pub fn worker(path: &Path) -> Result<()> {
    let payload = read_text(path)?;
    let tool: Tool = serde_json::from_str(&payload).context("invalid worker request")?;
    let result = execute_file(&tool, &std::env::current_dir()?);
    let value = match result {
        Ok(value) => value,
        Err(error) => json!({"error": format!("{error:#}")}),
    };
    println!("{value}");
    Ok(())
}

fn read_text(path: &Path) -> Result<String> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    ensure!(file.metadata()?.is_file(), "path must be a regular file");
    let mut bytes = Vec::new();
    file.take(FILE_LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > FILE_LIMIT {
        bail!("file exceeds 1 MiB; use shell to read selected lines");
    }
    String::from_utf8(bytes).context("file is not UTF-8")
}

fn clip(text: &str) -> &str {
    let mut end = text.len().min(OUTPUT_LIMIT);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

// Continue draining after the cap, so a verbose child cannot deadlock on a full pipe.
async fn drain(
    reader: &mut (impl AsyncRead + Unpin),
    output: &mut Vec<u8>,
    limit: usize,
) -> std::io::Result<bool> {
    let mut buffer = Box::new([0u8; 8192]);
    let mut truncated = false;
    loop {
        let count = reader.read(&mut *buffer).await?;
        if count == 0 {
            return Ok(truncated);
        }
        let keep = count.min(limit.saturating_sub(output.len()));
        output.extend_from_slice(&buffer[..keep]);
        truncated |= keep < count;
    }
}

#[cfg(unix)]
struct ProcessGroup(rustix::process::Pid);

#[cfg(unix)]
impl ProcessGroup {
    fn kill(&self) {
        let _ = rustix::process::kill_process_group(self.0, rustix::process::Signal::KILL);
    }
}

#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_context_distinguishes_restrictions_from_command_errors() {
        for failure in [
            json!({"exit_code": 7, "stderr": "problem"}),
            json!({"exit_code": null, "timed_out": true, "stdout": "partial"}),
            json!({"error": "permission denied"}),
        ] {
            for sandboxed in [false, true] {
                let mut output = failure.clone();
                annotate_failure(&mut output, sandboxed, Duration::from_secs(120));
                for (key, value) in failure.as_object().unwrap() {
                    assert_eq!(&output[key], value);
                }
                assert_eq!(output["execution_context"]["timeout_seconds"], 120.0);
                assert_eq!(
                    output["execution_context"]["access"],
                    if sandboxed { "sandbox" } else { "unrestricted" }
                );
                assert_eq!(output.get("diagnostic_hint").is_some(), sandboxed);
            }
        }
        for mut success in [json!({"exit_code": 0}), json!({"content": "file"})] {
            let original = success.clone();
            annotate_failure(&mut success, true, Duration::from_secs(120));
            assert_eq!(success, original);
        }
    }

    #[tokio::test]
    async fn edit_rejects_ambiguous_matches_and_bad_arguments() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file"), "same same").unwrap();
        let tools = Tools::for_test(dir.path(), Duration::from_secs(1)).unwrap();
        let result = tools
            .execute(
                "edit_file",
                r#"{"path":"file","old":"same","new":"changed"}"#,
            )
            .await;
        assert!(result["error"].as_str().unwrap().contains("exactly once"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("file")).unwrap(),
            "same same"
        );
        assert!(
            tools
                .execute("shell", r#"{"command":"true","extra":1}"#)
                .await
                .get("error")
                .is_some()
        );
        assert!(tools.execute("shell", "{").await.get("error").is_some());
    }

    #[tokio::test]
    async fn shell_bounds_output_and_reports_failure() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::for_test(dir.path(), Duration::from_secs(5)).unwrap();
        let result = tools
            .execute(
                "shell",
                r#"{"command":"yes x | head -c 100000; echo problem >&2; exit 7"}"#,
            )
            .await;
        assert_eq!(result["exit_code"], 7);
        assert_eq!(result["stdout"].as_str().unwrap().len(), OUTPUT_LIMIT);
        assert_eq!(result["stdout_truncated"], true);
        assert_eq!(result["stderr"], "problem\n");
    }

    // The child announces its own PID before blocking. Tests observe readiness
    // before triggering cleanup, rather than assuming the shell started in 50 ms.
    const WAITING_CHILD: &str = r#"{"command":"sh -c 'echo $$ > ready; exec sleep 30' & wait"}"#;

    async fn ready_child(directory: &Path) -> rustix::process::Pid {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(directory.join("ready"))
                    && let Ok(raw) = text.trim().parse()
                    && let Some(pid) = rustix::process::Pid::from_raw(raw)
                {
                    rustix::process::test_kill_process(pid).expect("announced child must be alive");
                    return pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("descendant did not announce readiness")
    }

    async fn assert_child_stopped(pid: rustix::process::Pid) {
        let stopped = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let status = Command::new("ps")
                    .args(["-o", "stat=", "-p", &pid.as_raw_nonzero().to_string()])
                    .output()
                    .await
                    .unwrap();
                let state = String::from_utf8(status.stdout).unwrap();
                // A killed orphan can remain a zombie until the system reaps it.
                if (status.status.code() == Some(1) && state.trim().is_empty())
                    || (status.status.success() && state.trim().starts_with('Z'))
                {
                    return;
                }
                assert!(status.status.success(), "could not inspect descendant");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        if stopped.is_err() {
            // Clean up even if process-group handling regresses.
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            panic!("descendant survived cleanup");
        }
    }

    #[tokio::test]
    async fn timeout_kills_a_running_descendant() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::for_test(dir.path(), Duration::from_secs(60)).unwrap();
        let task = tokio::spawn(async move { tools.execute("shell", WAITING_CHILD).await });
        let pid = ready_child(dir.path()).await;
        // Advance only after OS-level readiness; process startup uses real time.
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(61)).await;
        tokio::time::resume();
        // Reaping is an OS operation and must finish under a real-time deadline.
        let result = tokio::time::timeout(Duration::from_secs(5), task).await;
        assert_eq!(
            result.expect("tool did not time out").unwrap()["timed_out"],
            true
        );
        assert_child_stopped(pid).await;
    }

    #[tokio::test]
    async fn cancellation_kills_a_running_descendant() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::for_test(dir.path(), Duration::from_secs(60)).unwrap();
        let task = tokio::spawn(async move { tools.execute("shell", WAITING_CHILD).await });
        let pid = ready_child(dir.path()).await;
        task.abort();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .expect("task did not cancel")
                .unwrap_err()
                .is_cancelled()
        );
        assert_child_stopped(pid).await;
    }
}

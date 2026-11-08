use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
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
}

#[derive(Deserialize)]
#[serde(
    tag = "name",
    content = "arguments",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum Tool {
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
    pub fn new(directory: &Path, timeout: Duration) -> Result<Self> {
        let directory = directory
            .canonicalize()
            .context("resolve working directory")?;
        ensure!(directory.is_dir(), "working directory is not a directory");
        Ok(Self { directory, timeout })
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
            let properties: serde_json::Map<String, Value> = fields.iter().map(|field| (field.to_string(), json!({"type": "string"}))).collect();
            json!({"type": "function", "name": name, "description": description, "strict": true,
                "parameters": {"type": "object", "properties": properties, "required": fields, "additionalProperties": false}})
        }).collect()
    }

    pub async fn execute(&self, name: &str, arguments: &str) -> Value {
        match self.execute_inner(name, arguments).await {
            Ok(result) => result,
            Err(error) => json!({"error": format!("{error:#}")}),
        }
    }

    async fn execute_inner(&self, name: &str, arguments: &str) -> Result<Value> {
        let arguments: Value = serde_json::from_str(arguments).context("invalid tool arguments")?;
        let tool: Tool = serde_json::from_value(json!({"name": name, "arguments": arguments}))
            .context("unknown tool or invalid arguments")?;
        match tool {
            Tool::ReadFile { path } => {
                let content = read_text(&self.directory.join(path))?;
                Ok(json!({"content": clip(&content), "truncated": content.len() > OUTPUT_LIMIT}))
            }
            Tool::WriteFile { path, content } => {
                std::fs::write(self.directory.join(path), content).context("write file")?;
                Ok(json!({"ok": true}))
            }
            Tool::EditFile { path, old, new } => {
                ensure!(!old.is_empty(), "old must not be empty");
                let path = self.directory.join(path);
                let content = read_text(&path)?;
                ensure!(
                    content.matches(&old).count() == 1,
                    "old must match exactly once; file unchanged"
                );
                std::fs::write(path, content.replacen(&old, &new, 1)).context("edit file")?;
                Ok(json!({"ok": true}))
            }
            Tool::Shell { command } => self.shell(&command).await,
        }
    }

    async fn shell(&self, script: &str) -> Result<Value> {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(script)
            .current_dir(&self.directory)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // A separate process group lets cancellation kill ordinary descendants as well.
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().context("start shell")?;
        #[cfg(unix)]
        let group = ProcessGroup(
            rustix::process::Pid::from_raw(i32::try_from(
                child.id().context("missing child PID")?,
            )?)
            .context("invalid child PID")?,
        );
        let mut stdout = child.stdout.take().context("missing stdout")?;
        let mut stderr = child.stderr.take().context("missing stderr")?;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let execution = async {
            let (status, out_truncated, err_truncated) = tokio::try_join!(
                child.wait(),
                drain(&mut stdout, &mut out),
                drain(&mut stderr, &mut err)
            )?;
            Ok::<_, std::io::Error>((status, out_truncated, err_truncated))
        };
        let (code, timed_out, out_truncated, err_truncated) =
            if let Ok(result) = tokio::time::timeout(self.timeout, execution).await {
                let (status, out_truncated, err_truncated) = result.context("run shell")?;
                (status.code(), false, out_truncated, err_truncated)
            } else {
                #[cfg(unix)]
                group.kill();
                let _ = child.kill().await;
                let _ = child.wait().await;
                (
                    None,
                    true,
                    out.len() >= OUTPUT_LIMIT,
                    err.len() >= OUTPUT_LIMIT,
                )
            };
        Ok(json!({
            "exit_code": code, "timed_out": timed_out,
            "stdout": String::from_utf8_lossy(&out), "stderr": String::from_utf8_lossy(&err),
            "stdout_truncated": out_truncated, "stderr_truncated": err_truncated,
        }))
    }
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
) -> std::io::Result<bool> {
    let mut buffer = Box::new([0u8; 8192]);
    let mut truncated = false;
    loop {
        let count = reader.read(&mut *buffer).await?;
        if count == 0 {
            return Ok(truncated);
        }
        let keep = count.min(OUTPUT_LIMIT.saturating_sub(output.len()));
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

    #[tokio::test]
    async fn edit_rejects_ambiguous_matches_and_bad_arguments() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file"), "same same").unwrap();
        let tools = Tools::new(dir.path(), Duration::from_secs(1)).unwrap();
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
        let tools = Tools::new(dir.path(), Duration::from_secs(5)).unwrap();
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
        let tools = Tools::new(dir.path(), Duration::from_secs(60)).unwrap();
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
        let tools = Tools::new(dir.path(), Duration::from_secs(60)).unwrap();
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

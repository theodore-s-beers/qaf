use anyhow::{Context, Result, ensure};
use rustyline::{Behavior, Config, Editor, error::ReadlineError, history::DefaultHistory};
use std::{
    borrow::Cow,
    fs::{File, OpenOptions},
    io::{IsTerminal, Read, Write},
    os::unix::fs::OpenOptionsExt,
    time::Duration,
};

/// Print secondary information using the terminal's dim style when available.
pub fn status(message: std::fmt::Arguments<'_>) {
    let dim = std::io::stderr().is_terminal()
        && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
        && std::env::var_os("TERM").is_none_or(|value| value != "dumb");
    if dim {
        eprintln!("\x1b[2m{message}\x1b[22m");
    } else {
        eprintln!("{message}");
    }
}

/// A bounded display preview; execution and approval retain the full arguments.
pub fn tool_preview(tool: &crate::tools::Tool) -> String {
    use crate::tools::Tool;
    let (verb, detail) = match tool {
        Tool::ReadFile { path } => ("Reading", path),
        Tool::WriteFile { path, .. } => ("Writing", path),
        Tool::EditFile { path, .. } => ("Editing", path),
        Tool::Shell { command } => ("Running:", command),
    };
    let single_line = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    // Escape controls before printing model-provided paths or commands.
    let safe: String = single_line
        .chars()
        .map(|character| {
            if character.is_control() {
                character.escape_debug().to_string()
            } else {
                character.to_string()
            }
        })
        .collect();
    let mut characters = safe.chars();
    let preview: String = characters.by_ref().take(100).collect();
    let suffix = if characters.next().is_some() {
        "…"
    } else {
        ""
    };
    format!("{verb} {preview}{suffix}")
}

pub enum Input {
    Line(String),
    Interrupted,
    Eof,
}

pub trait PromptReader {
    fn read_prompt(&mut self) -> impl std::future::Future<Output = Result<Input>> + Send;
}

pub struct LineEditor {
    editor: Option<Editor<PromptStyle, DefaultHistory>>,
}

struct PromptStyle;

impl rustyline::Helper for PromptStyle {}
impl rustyline::completion::Completer for PromptStyle {
    type Candidate = String;
}
impl rustyline::hint::Hinter for PromptStyle {
    type Hint = String;
}
impl rustyline::validate::Validator for PromptStyle {}
impl rustyline::highlight::Highlighter for PromptStyle {
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        default: bool,
    ) -> Cow<'b, str> {
        if default && prompt == "qaf> " {
            Cow::Borrowed("\x1b[36mqaf>\x1b[39m ")
        } else {
            Cow::Borrowed(prompt)
        }
    }
}

impl LineEditor {
    pub fn new() -> Result<Self> {
        let config = Config::builder()
            .behavior(Behavior::PreferTerm)
            .auto_add_history(true)
            .build();
        // Rustyline handles color capability and NO_COLOR, and measures the
        // unstyled prompt so highlighting doesn't affect cursor positioning.
        let mut editor = Editor::with_config(config)?;
        editor.set_helper(Some(PromptStyle));
        Ok(Self {
            editor: Some(editor),
        })
    }
}

impl PromptReader for LineEditor {
    async fn read_prompt(&mut self) -> Result<Input> {
        let mut editor = self
            .editor
            .take()
            .context("line editor is already reading")?;
        // Await this worker to completion: dropping a blocking readline would leave
        // a competing terminal reader. Rustyline handles Ctrl-C while editing and
        // restores terminal modes and signal handlers before tools or approvals run.
        let (editor, result) = tokio::task::spawn_blocking(move || {
            // Rustyline 19 polls its signal pipe when an external printer exists.
            // Keep one alive so signals delivered to another runtime thread still
            // wake readline; we don't use it to print task or approval output.
            let result = match editor.create_external_printer() {
                Ok(_signals) => editor.readline("qaf> "),
                Err(error) => Err(error),
            };
            (editor, result)
        })
        .await
        .context("line editor failed")?;
        self.editor = Some(editor);
        match result {
            Ok(line) => Ok(Input::Line(line)),
            Err(
                ReadlineError::Interrupted
                | ReadlineError::Signal(rustyline::error::Signal::Interrupt),
            ) => Ok(Input::Interrupted),
            Err(ReadlineError::Eof) => Ok(Input::Eof),
            Err(error) => Err(error.into()),
        }
    }
}

/// Nonblocking terminal I/O keeps approval prompts cancellable.
pub struct Terminal {
    file: File,
}

impl Terminal {
    pub fn open() -> std::io::Result<Self> {
        Ok(Self {
            file: OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
                .open("/dev/tty")?,
        })
    }

    pub async fn write(&self, text: &str) -> std::io::Result<()> {
        let mut remaining = text.as_bytes();
        let mut file = &self.file;
        while !remaining.is_empty() {
            match file.write(remaining) {
                Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                Ok(count) => remaining = &remaining[count..],
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub async fn read_line(&self, limit: usize) -> Result<Option<String>> {
        let mut bytes = Vec::new();
        let mut file = &self.file;
        loop {
            let mut byte = [0];
            match file.read(&mut byte) {
                Ok(0) if bytes.is_empty() => return Ok(None),
                Ok(0) => break,
                Ok(_) if byte[0] == b'\n' => break,
                Ok(_) => {
                    // Drain the entire line so leftovers cannot answer a later prompt.
                    if bytes.len() <= limit {
                        bytes.push(byte[0]);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
        ensure!(bytes.len() <= limit, "input exceeds {limit} bytes");
        Ok(Some(String::from_utf8(bytes)?))
    }
}

fn approval_prompt(request: &serde_json::Value) -> Result<String> {
    use crate::tools::Tool;
    use std::fmt::Write as _;

    let tool: Tool = serde_json::from_value(request["operation"].clone())?;
    let directory = request["directory"]
        .as_str()
        .context("missing approval directory")?;
    let reason = request["justification"]
        .as_str()
        .context("missing approval reason")?;
    let mut prompt = String::from("\nApproval required · unrestricted access\n\n");
    // JSON quoting keeps paths on one line and makes unusual filenames explicit.
    match tool {
        Tool::ReadFile { path } => writeln!(prompt, "Read {}", serde_json::to_string(&path)?)?,
        Tool::WriteFile { path, content } => {
            writeln!(
                prompt,
                "Write {} (create or overwrite)",
                serde_json::to_string(&path)?
            )?;
            approval_block(&mut prompt, "Full contents", &content);
        }
        Tool::EditFile { path, old, new } => {
            writeln!(prompt, "Edit {}", serde_json::to_string(&path)?)?;
            approval_block(&mut prompt, "Replace exactly one occurrence", &old);
            approval_block(&mut prompt, "With", &new);
        }
        Tool::Shell { command } => approval_block(&mut prompt, "Run shell command", &command),
    }
    writeln!(
        prompt,
        "\nWorking directory: {}",
        serde_json::to_string(directory)?
    )?;
    approval_block(&mut prompt, "Reason", reason);
    prompt.push_str("\nThis operation will run WITHOUT the sandbox, with your user permissions.\n\nType allow (once), deny [default], or cancel: ");
    Ok(prompt)
}

fn approval_block(output: &mut String, label: &str, text: &str) {
    if !output.ends_with("\n\n") {
        output.push('\n');
    }
    output.push_str(label);
    output.push_str(":\n");
    if text.is_empty() {
        output.push_str("    (empty)\n");
        return;
    }
    // Keep every line, indent untrusted text, and escape terminal controls.
    for line in text.split('\n') {
        output.push_str("    ");
        for character in line.chars() {
            if character.is_control() {
                output.push_str(&character.escape_debug().to_string());
            } else {
                output.push(character);
            }
        }
        output.push('\n');
    }
}

pub struct Human;

impl crate::permissions::Approver for Human {
    async fn approve(&self, request: &serde_json::Value) -> Result<crate::permissions::Decision> {
        use crate::permissions::Decision;
        let Ok(terminal) = Terminal::open() else {
            eprintln!("Broader access denied: no interactive terminal is available.");
            return Ok(Decision::Deny);
        };
        terminal.write(&approval_prompt(request)?).await?;
        let answer = terminal.read_line(32).await?.unwrap_or_default();
        Ok(match answer.trim() {
            "allow" => Decision::AllowOnce,
            "cancel" => Decision::Cancel,
            _ => Decision::Deny,
        })
    }
}

#[cfg(test)]
impl Terminal {
    pub fn pair() -> (Self, std::os::unix::net::UnixStream) {
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();
        let file = File::from(std::os::fd::OwnedFd::from(server));
        (Self { file }, client)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approvals_show_full_operations_and_escape_terminal_controls() {
        use serde_json::json;
        let long_command = format!("echo '{}'\nprintf done", "x".repeat(200));
        for (operation, expected) in [
            (
                json!({"name": "read_file", "arguments": {"path": "prefs.json"}}),
                vec!["Read \"prefs.json\"".to_owned()],
            ),
            (
                json!({"name": "shell", "arguments": {"command": long_command}}),
                vec![format!("    echo '{}'\n    printf done", "x".repeat(200))],
            ),
            (
                json!({"name": "write_file", "arguments": {"path": "file", "content": "first\n\nlast\n"}}),
                vec![
                    "(create or overwrite)".into(),
                    "    first\n    \n    last\n    \n".into(),
                ],
            ),
            (
                json!({"name": "edit_file", "arguments": {"path": "file", "old": "before", "new": "after\x1b[31m"}}),
                vec![
                    "Replace exactly one occurrence:\n    before".into(),
                    "With:\n    after\\u{1b}[31m".into(),
                ],
            ),
        ] {
            let prompt = approval_prompt(&json!({"operation": operation, "directory": "/project", "justification": "Requested\rby user"})).unwrap();
            for text in expected {
                assert!(prompt.contains(&text), "missing {text:?} in {prompt:?}");
            }
            assert!(prompt.contains("Working directory: \"/project\""));
            assert!(prompt.contains("Requested\\rby user"));
            assert!(prompt.contains("WITHOUT the sandbox"));
            assert!(prompt.ends_with("Type allow (once), deny [default], or cancel: "));
            assert!(!prompt.contains('\x1b'));
        }
    }

    #[test]
    fn tool_previews_are_bounded_single_line_and_escape_controls() {
        use crate::tools::Tool;
        assert_eq!(
            tool_preview(&Tool::ReadFile {
                path: "src/main.rs".into()
            }),
            "Reading src/main.rs"
        );
        assert_eq!(
            tool_preview(&Tool::Shell {
                command: "cargo test\n\t&& echo \x1b[31m".into()
            }),
            "Running: cargo test && echo \\u{1b}[31m"
        );
        let command = "界".repeat(100);
        assert_eq!(
            tool_preview(&Tool::Shell {
                command: command.clone()
            }),
            format!("Running: {command}")
        );
        let command = "界".repeat(150);
        assert_eq!(
            tool_preview(&Tool::Shell { command }),
            format!("Running: {}…", "界".repeat(100))
        );
    }

    #[tokio::test]
    async fn cancelled_reads_do_not_steal_later_input_and_long_lines_are_drained() {
        let (terminal, mut client) = Terminal::pair();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), terminal.read_line(32))
                .await
                .is_err()
        );
        client
            .write_all("héllo\nthis line is too long\nallow\n".as_bytes())
            .unwrap();
        assert_eq!(
            terminal.read_line(32).await.unwrap().as_deref(),
            Some("héllo")
        );
        assert!(terminal.read_line(4).await.is_err());
        assert_eq!(
            terminal.read_line(32).await.unwrap().as_deref(),
            Some("allow")
        );
        drop(client);
        assert!(terminal.read_line(32).await.unwrap().is_none());
    }
}

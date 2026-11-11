use anyhow::{Context, Result, ensure};
use rustyline::{Behavior, Config, Editor, error::ReadlineError, history::DefaultHistory};
use std::{
    borrow::Cow,
    fs::{File, OpenOptions},
    io::{IsTerminal, Read, Write},
    os::unix::fs::OpenOptionsExt,
    time::Duration,
};

/// Format a count with comma separators for terminal output.
pub fn number(value: u64) -> String {
    let digits = value.to_string();
    let mut formatted = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(digit);
    }
    formatted
}

/// Print secondary information using the terminal's dim style when available.
pub fn status(message: std::fmt::Arguments<'_>) {
    let dim = std::io::stderr().is_terminal()
        && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
        && std::env::var_os("TERM").is_none_or(|value| value != "dumb");
    let text = if dim {
        format!("\x1b[2m{message}\x1b[22m\n")
    } else {
        format!("{message}\n")
    };
    if !crate::live::status(&text).unwrap_or(false) {
        eprint!("{text}");
    }
}

pub fn review_status(label: &str, reason: &str) {
    let width = terminal_size::terminal_size_of(std::io::stderr())
        .map_or(80, |(width, _)| usize::from(width.0));
    status(format_args!("{}", review_message(label, reason, width)));
}

fn review_message(label: &str, reason: &str, width: usize) -> String {
    let safe: String = reason
        .chars()
        .map(|character| {
            if character.is_control() {
                character.escape_debug().to_string()
            } else {
                character.to_string()
            }
        })
        .collect();
    textwrap::fill(
        &format!("{label}: {safe}"),
        textwrap::Options::new(width.max(5))
            .subsequent_indent("    ")
            .break_words(false),
    )
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
        if crate::live::active() {
            return crate::live::read(false).await;
        }
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

fn approval_prompt(request: &serde_json::Value, width: usize) -> Result<String> {
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
            approval_block(&mut prompt, "Full contents", &content, None);
        }
        Tool::EditFile { path, old, new } => {
            writeln!(prompt, "Edit {}", serde_json::to_string(&path)?)?;
            approval_block(&mut prompt, "Replace exactly one occurrence", &old, None);
            approval_block(&mut prompt, "With", &new, None);
        }
        Tool::Shell { command } => approval_block(&mut prompt, "Run shell command", &command, None),
    }
    writeln!(
        prompt,
        "\nWorking directory: {}",
        serde_json::to_string(directory)?
    )?;
    approval_block(&mut prompt, "Reason", reason, Some(width));
    prompt.push_str("\nThis operation will run WITHOUT the sandbox, with your user permissions.\n\nType allow (once), deny [default], or cancel: ");
    Ok(prompt)
}

fn approval_block(output: &mut String, label: &str, text: &str, width: Option<usize>) {
    if !output.ends_with("\n\n") {
        output.push('\n');
    }
    output.push_str(label);
    output.push_str(":\n");
    if text.is_empty() {
        output.push_str("    (empty)\n");
        return;
    }
    // Escape terminal controls before wrapping prose; code stays verbatim.
    for line in text.split('\n') {
        let safe: String = line
            .chars()
            .map(|character| {
                if character.is_control() {
                    character.escape_debug().to_string()
                } else {
                    character.to_string()
                }
            })
            .collect();
        if let Some(width) = width {
            let options = textwrap::Options::new(width.max(5))
                .initial_indent("    ")
                .subsequent_indent("    ")
                .break_words(false);
            output.push_str(&textwrap::fill(&safe, options));
        } else {
            output.push_str("    ");
            output.push_str(&safe);
        }
        output.push('\n');
    }
}

pub struct Human;

impl crate::permissions::Approver for Human {
    async fn approve(&self, request: &serde_json::Value) -> Result<crate::permissions::Decision> {
        use crate::permissions::Decision;
        if crate::live::active() {
            let _approval = crate::live::Approval::start()?;
            let width = terminal_size::terminal_size().map_or(80, |(w, _)| usize::from(w.0));
            output(&approval_prompt(request, width)?)?;
            return Ok(match crate::live::read(true).await? {
                Input::Line(line) if line.trim() == "allow" => Decision::AllowOnce,
                Input::Line(line) if line.trim() == "cancel" => Decision::Cancel,
                Input::Interrupted | Input::Eof => Decision::Cancel,
                Input::Line(_) => Decision::Deny,
            });
        }
        let Ok(terminal) = Terminal::open() else {
            eprintln!("Broader access denied: no interactive terminal is available.");
            return Ok(Decision::Deny);
        };
        let width = terminal_size::terminal_size_of(&terminal.file)
            .map_or(80, |(width, _)| usize::from(width.0));
        terminal.write(&approval_prompt(request, width)?).await?;
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

/// Shared word-aware wrapping for editable and submitted prompts.
pub fn wrap_prompt(
    text: &[char],
    cursor_index: usize,
    width: usize,
) -> (Vec<String>, (usize, usize)) {
    use unicode_width::UnicodeWidthChar;
    let width = width.max(1);
    let mut lines = vec![String::new()];
    let mut column = 0;
    let mut cursor = (0, 0);
    let mut has_word = false;
    for (index, &c) in text.iter().enumerate() {
        let visible = if c == '\t' { ' ' } else { c };
        let size = visible.width().unwrap_or(0);
        let word_start = !c.is_whitespace() && (index == 0 || text[index - 1].is_whitespace());
        let word_width = if word_start {
            text[index..]
                .iter()
                .take_while(|c| !c.is_whitespace())
                .map(|c| c.width().unwrap_or(0))
                .sum::<usize>()
        } else {
            0
        };
        // Keep words that fit on a fresh line together; split longer tokens.
        let move_word = word_start && word_width <= width && column + word_width > width;
        // Spaces at a soft boundary remain in the actual prompt, but must not
        // create a whitespace-only display row between two words.
        let hidden_separator = c != '\n' && c.is_whitespace() && has_word && column + size > width;
        if move_word || (c != '\n' && column + size > width && !hidden_separator) {
            lines.push(String::new());
            column = 0;
            has_word = false;
        }
        if index == cursor_index {
            cursor = (lines.len() - 1, column);
        }
        if c == '\n' {
            lines.push(String::new());
            column = 0;
            has_word = false;
        } else if !hidden_separator {
            lines.last_mut().unwrap().push(visible);
            column += size;
            has_word |= !c.is_whitespace();
        }
    }
    if cursor_index == text.len() {
        cursor = (lines.len() - 1, column);
    }
    (lines, cursor)
}

/// Keep user text at normal brightness and color only its prefix.
pub fn user_message(prefix: &str, message: &str) -> String {
    let colored = std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
        && std::env::var_os("TERM").is_none_or(|value| value != "dumb");
    let width = std::io::stdout()
        .is_terminal()
        .then(|| terminal_size::terminal_size().map_or(80, |(width, _)| usize::from(width.0)));
    format_user_message(prefix, message, width, colored)
}

fn format_user_message(prefix: &str, message: &str, width: Option<usize>, colored: bool) -> String {
    let label = if colored {
        format!("\x1b[36m{prefix}\x1b[39m ")
    } else {
        format!("{prefix} ")
    };
    let Some(width) = width else {
        return format!("{label}{message}\n");
    };
    let indent = " ".repeat(textwrap::core::display_width(prefix) + 1);
    let text: Vec<char> = message.chars().collect();
    let (lines, _) = wrap_prompt(&text, text.len(), width.saturating_sub(indent.len() + 1));
    format!("{label}{}\n", lines.join(&format!("\n{indent}")))
}

/// Write model output above the live input area when present.
pub fn output(text: &str) -> std::io::Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    if !crate::live::output(text)? {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(text.as_bytes())?;
        stdout.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_wraps_words_and_maps_cursor_at_soft_boundaries() {
        let text: Vec<char> = "hello world".chars().collect();
        assert_eq!(
            wrap_prompt(&text, 6, 8),
            (vec!["hello ".into(), "world".into()], (1, 0))
        );
        assert_eq!(wrap_prompt(&text, 8, 8).1, (1, 2));
        assert_eq!(
            wrap_prompt(&text, 5, 5),
            (vec!["hello".into(), "world".into()], (0, 5))
        );
        assert_eq!(wrap_prompt(&text, 6, 5).1, (1, 0));
        let text: Vec<char> = "  one  two\n\n界界 end\nabcdefghijk".chars().collect();
        assert_eq!(
            wrap_prompt(&text, text.len(), 6).0,
            ["  one ", "two", "", "界界 ", "end", "abcdef", "ghijk"]
        );
    }

    #[test]
    fn submitted_prompts_indent_wrapped_lines_and_preserve_paragraphs() {
        let message = "ab界cdef\n\nnext";
        assert_eq!(
            format_user_message("qaf>", message, Some(12), false),
            "qaf> ab界cd\n     ef\n     \n     next\n"
        );
        assert_eq!(
            format_user_message("Queued:", message, Some(15), true),
            "\x1b[36mQueued:\x1b[39m ab界cd\n        ef\n        \n        next\n"
        );
        assert_eq!(
            format_user_message("qaf>", message, None, false),
            format!("qaf> {message}\n")
        );
    }

    #[test]
    fn review_messages_wrap_and_escape_controls_without_quoting() {
        assert_eq!(
            review_message(
                "Auto-review allowed",
                "Read the requested preferences file.",
                40
            ),
            "Auto-review allowed: Read the requested\n    preferences file."
        );
        assert_eq!(
            review_message("Auto-review denied", "don't run \x1b[31m", 80),
            "Auto-review denied: don't run \\u{1b}[31m"
        );
    }

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
            let prompt = approval_prompt(&json!({"operation": operation, "directory": "/project", "justification": "Requested\rby user"}), 80).unwrap();
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
    fn approval_reason_wraps_at_words_with_continuation_indent() {
        let request = serde_json::json!({
            "operation": {"name":"read_file", "arguments":{"path":"preferences.json"}},
            "directory":"/project", "justification":"Read the requested preferences file."
        });
        let prompt = approval_prompt(&request, 24).unwrap();
        assert!(prompt.contains("\nReason:\n    Read the requested\n    preferences file.\n"));
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

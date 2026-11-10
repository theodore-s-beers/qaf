//! A small scrollback-preserving terminal interface with one input owner.
use crate::{agent::Followups, terminal::Input};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use std::{
    io::{self, IsTerminal, Write},
    sync::Mutex,
    time::Duration,
};
use unicode_width::UnicodeWidthChar;

static DISPLAY: Mutex<Option<Display>> = Mutex::new(None);

#[derive(Default)]
struct Editor {
    text: Vec<char>,
    cursor: usize,
    history: Vec<String>,
    recalled: Option<usize>,
    draft: String,
}

impl Editor {
    fn replace(&mut self, text: &str) {
        self.text = text.chars().collect();
        self.cursor = self.text.len();
    }

    fn key(&mut self, code: KeyCode, modifiers: KeyModifiers, history: bool) -> Option<Input> {
        if modifiers.contains(KeyModifiers::CONTROL) {
            match code {
                KeyCode::Char('c') => {
                    self.replace("");
                    self.recalled = None;
                    self.draft.clear();
                    return Some(Input::Interrupted);
                }
                KeyCode::Char('d') if self.text.is_empty() => return Some(Input::Eof),
                KeyCode::Char('a') => self.cursor = 0,
                KeyCode::Char('e') => self.cursor = self.text.len(),
                KeyCode::Char('u') => {
                    self.text.drain(..self.cursor);
                    self.cursor = 0;
                }
                KeyCode::Char('w') => {
                    let end = self.cursor;
                    while self.cursor > 0 && self.text[self.cursor - 1].is_whitespace() {
                        self.cursor -= 1;
                    }
                    while self.cursor > 0 && !self.text[self.cursor - 1].is_whitespace() {
                        self.cursor -= 1;
                    }
                    self.text.drain(self.cursor..end);
                }
                _ => {}
            }
            return None;
        }
        match code {
            KeyCode::Enter => {
                let line: String = self.text.drain(..).collect();
                if history && !line.trim().is_empty() {
                    self.history.push(line.clone());
                }
                self.cursor = 0;
                self.recalled = None;
                self.draft.clear();
                return Some(Input::Line(line));
            }
            KeyCode::Char(c) if !c.is_control() && !modifiers.contains(KeyModifiers::ALT) => {
                self.text.insert(self.cursor, c);
                self.cursor += 1;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.text.remove(self.cursor);
            }
            KeyCode::Delete if self.cursor < self.text.len() => {
                self.text.remove(self.cursor);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.text.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.text.len(),
            KeyCode::Up if history && !self.history.is_empty() => {
                if self.recalled.is_none() {
                    self.draft = self.text.iter().collect();
                }
                let index = self
                    .recalled
                    .map_or(self.history.len() - 1, |i| i.saturating_sub(1));
                self.recalled = Some(index);
                self.replace(&self.history[index].clone());
            }
            KeyCode::Down if history => {
                if let Some(index) = self.recalled {
                    if index + 1 < self.history.len() {
                        self.recalled = Some(index + 1);
                        self.replace(&self.history[index + 1].clone());
                    } else {
                        self.recalled = None;
                        self.replace(&self.draft.clone());
                    }
                }
            }
            _ => {}
        }
        None
    }

    fn paste(&mut self, text: &str) {
        for c in text.replace("\r\n", "\n").replace('\r', "\n").chars() {
            if !c.is_control() || matches!(c, '\n' | '\t') {
                self.text.insert(self.cursor, c);
                self.cursor += 1;
            }
        }
    }

    const fn visible(c: char) -> char {
        match c {
            '\n' => '↵',
            '\t' => ' ',
            _ => c,
        }
    }

    fn viewport(&self, width: usize) -> (String, usize) {
        let width = width.max(1);
        let mut start = self.cursor;
        let mut column = 0;
        while start > 0 {
            let size = Self::visible(self.text[start - 1]).width().unwrap_or(0);
            if column + size >= width {
                break;
            }
            column += size;
            start -= 1;
        }
        let mut used = 0;
        let visible = self.text[start..]
            .iter()
            .copied()
            .map(Self::visible)
            .take_while(|c| {
                used += c.width().unwrap_or(0);
                used <= width
            })
            .collect();
        (visible, column)
    }
}

/// Status separators share the output stream, including ANSI-styled messages.
#[derive(Default)]
struct Spacing {
    newlines: usize,
    escape: u8,
}

impl Spacing {
    fn prepare(&mut self, text: &str, separate: bool) -> String {
        let mut output = String::with_capacity(text.len());
        let mut leading = separate;
        for c in text.chars() {
            match self.escape {
                1 => self.escape = if c == '[' { 2 } else { 0 },
                2 => {
                    if ('@'..='~').contains(&c) {
                        self.escape = 0;
                    }
                }
                _ if c == '\x1b' => self.escape = 1,
                _ if c == '\n' => {
                    if leading && self.newlines >= 2 {
                        continue;
                    }
                    self.newlines = (self.newlines + 1).min(2);
                }
                _ if c == '\r' => {}
                _ => {
                    self.newlines = 0;
                    leading = false;
                }
            }
            output.push(c);
        }
        output
    }
}

struct Display {
    editor: Editor,
    spacing: Spacing,
    width: u16,
    height: u16,
    queue: Option<Followups>,
    approval: bool,
}

impl Display {
    fn layout(&mut self, width: u16, height: u16, starting: bool) -> io::Result<()> {
        self.width = width.max(1);
        self.height = height.max(3);
        // Changing the scroll region moves the cursor. At startup, restore the
        // output position saved below the existing text instead of jumping down.
        write!(io::stdout(), "\x1b[r\x1b[1;{}r", self.height - 2)?;
        if starting {
            write!(io::stdout(), "\x1b8")?;
        } else {
            write!(io::stdout(), "\x1b[{};1H", self.height - 2)?;
        }
        write!(io::stdout(), "\x1b7")?;
        self.draw()
    }

    fn draw(&self) -> io::Result<()> {
        let label = if self.approval { "approval> " } else { "qaf> " };
        let hint = if self.approval {
            "allow / deny / cancel (draft saved)"
        } else {
            ""
        };
        let width = usize::from(self.width);
        let label: String = label.chars().take(width.saturating_sub(1)).collect();
        let (text, cursor) = self.editor.viewport(width.saturating_sub(label.len() + 1));
        let hint: String = hint.chars().take(width.saturating_sub(1)).collect();
        let styled = std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
            && std::env::var_os("TERM").is_none_or(|value| value != "dumb");
        let reset = if styled { "\x1b[0m" } else { "" };
        let color = if styled && !self.approval {
            "\x1b[36m"
        } else {
            ""
        };
        let mut out = io::stdout().lock();
        write!(
            out,
            "{reset}\x1b[{};1H\x1b[2K{hint}\x1b[{};1H\x1b[2K{color}{label}{reset}{text}\x1b[{};{}H",
            self.height - 1,
            self.height,
            self.height,
            (label.len() + cursor + 1).min(width)
        )?;
        out.flush()
    }

    fn output(&mut self, text: &str) -> io::Result<()> {
        self.write(text, false)
    }

    fn write(&mut self, text: &str, separate: bool) -> io::Result<()> {
        let text = self.spacing.prepare(text, separate);
        let mut out = io::stdout().lock();
        write!(out, "\x1b8{}\x1b7", text.replace('\n', "\r\n"))?;
        drop(out);
        self.draw()
    }

    fn poll(&mut self) -> Result<Option<Input>> {
        if !event::poll(Duration::ZERO)? {
            return Ok(None);
        }
        let input = match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                self.editor.key(key.code, key.modifiers, !self.approval)
            }
            Event::Paste(text) => {
                // Pasted newlines never submit a message or an approval.
                self.editor.paste(&text);
                None
            }
            Event::Resize(width, height) => {
                self.layout(width, height, false)?;
                None
            }
            _ => None,
        };
        self.draw()?;
        Ok(input)
    }

    fn queue_input(&mut self, input: Input) -> Result<()> {
        match input {
            Input::Line(line) if !line.trim().is_empty() => {
                let prompt = line.trim();
                if matches!(
                    prompt.split_whitespace().next(),
                    Some("/model" | "/clear" | "/context" | "/exit")
                ) || (prompt.starts_with('/') && !prompt.contains(char::is_whitespace))
                {
                    self.output(
                        "\nSession commands are available after the task stops. Ctrl-C stops it.\n",
                    )?;
                } else if let Some(queue) = &self.queue {
                    queue.submit(line.clone());
                    self.write(
                        &format!("\n{}\n", crate::terminal::user_message("Queued:", &line)),
                        true,
                    )?;
                }
            }
            Input::Interrupted | Input::Eof => anyhow::bail!("interrupted"),
            Input::Line(_) => {}
        }
        Ok(())
    }
}

pub struct Session;
impl Session {
    pub fn start() -> Result<Option<Self>> {
        if !io::stdout().is_terminal()
            || !io::stderr().is_terminal()
            || std::env::var_os("TERM").is_some_and(|term| term == "dumb")
        {
            return Ok(None);
        }
        let (width, height) = crossterm::terminal::size()?;
        if width < 12 || height < 4 {
            return Ok(None);
        }
        crossterm::terminal::enable_raw_mode()?;
        let session = Self;
        crossterm::execute!(io::stdout(), event::EnableBracketedPaste)?;
        let mut display = Display {
            editor: Editor::default(),
            spacing: Spacing::default(),
            width,
            height,
            queue: None,
            approval: false,
        };
        // Reserve the input rows without overwriting the latest shell/startup output.
        write!(io::stdout(), "\r\n\r\n\x1b[2A\x1b7")?;
        display.layout(width, height, true)?;
        *DISPLAY.lock().expect("display poisoned") = Some(display);
        Ok(Some(session))
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let display = DISPLAY.lock().ok().and_then(|mut display| display.take());
        if let Some(display) = display {
            let _ = write!(io::stdout(), "\x1b[r\x1b[{};1H\x1b[J", display.height - 1);
        }
        let _ = crossterm::execute!(io::stdout(), event::DisableBracketedPaste);
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

pub fn active() -> bool {
    DISPLAY.lock().expect("display poisoned").is_some()
}

/// Returns false when ordinary stdout/stderr should be used instead.
pub fn output(text: &str) -> io::Result<bool> {
    write(text, false)
}

/// Avoid stacking leading status separators on an existing blank line.
pub fn status(text: &str) -> io::Result<bool> {
    write(text, true)
}

fn write(text: &str, separate: bool) -> io::Result<bool> {
    let mut display = DISPLAY.lock().expect("display poisoned");
    if let Some(display) = display.as_mut() {
        display.write(text, separate)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

pub async fn read(approval: bool) -> Result<Input> {
    loop {
        {
            let mut display = DISPLAY.lock().expect("display poisoned");
            if let Some(display) = display.as_mut()
                && display.approval == approval
            {
                while event::poll(Duration::ZERO)? {
                    if let Some(input) = display.poll()? {
                        return Ok(input);
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(16)).await;
    }
}

pub struct Task;
impl Task {
    pub fn start(queue: Followups) -> Result<Self> {
        if let Some(display) = DISPLAY.lock().expect("display poisoned").as_mut() {
            display.queue = Some(queue);
            display.draw()?;
        }
        Ok(Self)
    }
}
impl Drop for Task {
    fn drop(&mut self) {
        if let Some(display) = DISPLAY.lock().expect("display poisoned").as_mut() {
            display.queue = None;
            let _ = display.draw();
        }
    }
}

pub async fn collect() -> Result<()> {
    if !active() {
        return std::future::pending().await;
    }
    loop {
        let input = read(false).await?;
        if let Some(display) = DISPLAY.lock().expect("display poisoned").as_mut() {
            display.queue_input(input)?;
        }
    }
}

pub struct Approval {
    saved: Editor,
}
impl Approval {
    pub fn start() -> Result<Self> {
        let mut lock = DISPLAY.lock().expect("display poisoned");
        let display = lock.as_mut().expect("live approval requires display");
        // Finish processing keys already typed as task input before changing modes.
        while event::poll(Duration::ZERO)? {
            if let Some(input) = display.poll()? {
                display.queue_input(input)?;
            }
        }
        let saved = std::mem::take(&mut display.editor);
        display.approval = true;
        let guard = Self { saved };
        drop(lock);
        Ok(guard)
    }
}
impl Drop for Approval {
    fn drop(&mut self) {
        if let Some(display) = DISPLAY.lock().expect("display poisoned").as_mut() {
            display.editor = std::mem::take(&mut self.saved);
            display.approval = false;
            let _ = display.draw();
        }
    }
}

/// Keep editing responsive during model selection without submitting a task.
pub async fn cancel() -> Result<()> {
    if !active() {
        return std::future::pending().await;
    }
    loop {
        match read(false).await? {
            Input::Interrupted | Input::Eof => anyhow::bail!("interrupted"),
            Input::Line(line) => {
                if let Some(display) = DISPLAY.lock().expect("display poisoned").as_mut() {
                    display.editor.replace(&line);
                    display.draw()?;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_separators_do_not_stack_after_queued_messages() {
        for next in [
            "Sending queued follow-up to model…",
            "Reviewing requested access…",
            "Running: gh issue list",
        ] {
            let mut spacing = Spacing::default();
            let queued = spacing.prepare("\x1b[36mQueued:\x1b[39m follow up\n\n", true);
            let notice = spacing.prepare(&format!("\x1b[2m\n{next}\x1b[22m\n"), true);
            assert_eq!(
                format!("{queued}{notice}"),
                format!("\x1b[36mQueued:\x1b[39m follow up\n\n\x1b[2m{next}\x1b[22m\n")
            );
        }
        let mut spacing = Spacing::default();
        spacing.prepare("Running: command\n", true);
        assert_eq!(
            spacing.prepare("\nQueued: one\n\n", true),
            "\nQueued: one\n\n"
        );
        assert_eq!(
            spacing.prepare("\nQueued: two\n\n", true),
            "Queued: two\n\n"
        );
    }

    #[test]
    fn spacing_preserves_message_content_and_streamed_code_blank_lines() {
        let mut spacing = Spacing::default();
        let code = "code\n\n\nmore code\n\n";
        assert_eq!(spacing.prepare(code, false), code);
        assert_eq!(
            spacing.prepare("\ncontinued code\n", false),
            "\ncontinued code\n"
        );
        assert_eq!(
            spacing.prepare("\nQueued: first paragraph\n\n\nsecond paragraph\n\n", true),
            "\nQueued: first paragraph\n\n\nsecond paragraph\n\n"
        );
    }

    #[test]
    fn editor_preserves_paste_and_history_draft_and_clears_interrupts() {
        let mut editor = Editor::default();
        editor.paste("one\r\ntwo\t界\x1b");
        assert_eq!(editor.text.iter().collect::<String>(), "one\ntwo\t界");
        assert!(
            matches!(editor.key(KeyCode::Enter, KeyModifiers::NONE, true), Some(Input::Line(line)) if line == "one\ntwo\t界")
        );
        editor.paste("draft");
        editor.key(KeyCode::Up, KeyModifiers::NONE, true);
        assert_eq!(editor.text.iter().collect::<String>(), "one\ntwo\t界");
        editor.key(KeyCode::Down, KeyModifiers::NONE, true);
        assert_eq!(editor.text.iter().collect::<String>(), "draft");
        editor.key(KeyCode::Char('c'), KeyModifiers::CONTROL, true);
        assert_eq!(editor.text, [] as [char; 0]);
        editor.key(KeyCode::Up, KeyModifiers::NONE, false);
        assert!(
            editor.text.is_empty(),
            "approval input cannot recall task history"
        );
    }

    #[test]
    fn editor_keeps_unicode_cursor_visible_in_narrow_viewports() {
        let mut editor = Editor::default();
        editor.paste("a界bc");
        assert_eq!(editor.viewport(4), ("bc".into(), 2));
        editor.key(KeyCode::Left, KeyModifiers::NONE, true);
        editor.key(KeyCode::Backspace, KeyModifiers::NONE, true);
        assert_eq!(editor.text.iter().collect::<String>(), "a界c");
        assert_eq!(editor.viewport(4), ("a界c".into(), 3));
        editor.key(KeyCode::Home, KeyModifiers::NONE, true);
        assert_eq!(editor.viewport(1), ("a".into(), 0));
    }

    // Driven by tests/fixtures/live-terminal.py in a real pseudo-terminal.
    #[tokio::test]
    #[ignore = "requires an interactive pseudo-terminal; use tests/fixtures/live-terminal.py"]
    async fn terminal_probe() {
        let session = Session::start().unwrap().expect("terminal required");
        crate::terminal::status(format_args!("STATUS STYLE PROBE"));
        output("PROBE READY\n").unwrap();
        let Input::Line(line) = read(false).await.unwrap() else {
            panic!("expected input");
        };
        assert_eq!(line, "first");
        let queue = Followups::default();
        let task = Task::start(queue.clone()).unwrap();
        output("WORKING\n").unwrap();
        tokio::select! {
            result = collect() => panic!("unexpected input result {result:?}"),
            () = async {
                while queue.is_empty() { tokio::time::sleep(Duration::from_millis(10)).await; }
                // Let the driver start a draft before asking for approval.
                tokio::time::sleep(Duration::from_millis(100)).await;
            } => {}
        }
        {
            let _approval = Approval::start().unwrap();
            output("APPROVAL READY\n").unwrap();
            assert!(matches!(read(true).await.unwrap(), Input::Line(line) if line == "deny"));
        }
        output("DRAFT RESTORED\n").unwrap();
        assert!(matches!(read(false).await.unwrap(), Input::Line(line) if line == "saved draft"));
        output("CANCEL READY\n").unwrap();
        assert!(
            collect()
                .await
                .unwrap_err()
                .to_string()
                .contains("interrupted")
        );
        drop(task);
        output("PROBE DONE\n").unwrap();
        drop(session);
        println!("TERMINAL RESTORED");
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "finish");
    }
}

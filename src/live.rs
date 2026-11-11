//! A small scrollback-preserving terminal interface with one input owner.
use crate::{agent::Followups, terminal::Input};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use std::{
    io::{self, IsTerminal, Write},
    sync::Mutex,
    time::Duration,
};

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
                KeyCode::Char('j') if history => self.paste("\n"),
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
            KeyCode::Enter if modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) => {
                if history {
                    self.paste("\n");
                }
            }
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
            KeyCode::Home => self.cursor = self.line_start(),
            KeyCode::End => self.cursor = self.line_end(),
            KeyCode::Up if self.move_line(false) => {}
            KeyCode::Down if self.move_line(true) => {}
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

    fn line_start(&self) -> usize {
        self.text[..self.cursor]
            .iter()
            .rposition(|c| *c == '\n')
            .map_or(0, |i| i + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .iter()
            .position(|c| *c == '\n')
            .map_or(self.text.len(), |i| self.cursor + i)
    }

    fn move_line(&mut self, down: bool) -> bool {
        let start = self.line_start();
        let column = self.cursor - start;
        if down {
            let end = self.line_end();
            if end == self.text.len() {
                return false;
            }
            let next = end + 1;
            let length = self.text[next..]
                .iter()
                .position(|c| *c == '\n')
                .unwrap_or(self.text.len() - next);
            self.cursor = next + column.min(length);
        } else {
            if start == 0 {
                return false;
            }
            let previous = self.text[..start - 1]
                .iter()
                .rposition(|c| *c == '\n')
                .map_or(0, |i| i + 1);
            self.cursor = previous + column.min(start - 1 - previous);
        }
        true
    }

    fn viewport(&self, width: usize, rows: usize) -> Viewport {
        let (lines, cursor) = crate::terminal::wrap_prompt(&self.text, self.cursor, width);
        let count = rows.max(1);
        let start = lines.len().saturating_sub(count).min(cursor.0);
        let end = (start + count).min(lines.len());
        Viewport {
            lines: lines[start..end].to_vec(),
            row: cursor.0 - start,
            column: cursor.1,
            clipped: start > 0,
        }
    }
}

struct Viewport {
    lines: Vec<String>,
    row: usize,
    column: usize,
    clipped: bool,
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
    fn footer_rows(&self) -> u16 {
        self.height.saturating_sub(2).clamp(2, 7)
    }

    fn layout(&mut self, width: u16, height: u16, starting: bool) -> io::Result<()> {
        self.width = width.max(1);
        self.height = height.max(3);
        // Changing the scroll region moves the cursor. At startup, restore the
        // output position saved below the existing text instead of jumping down.
        write!(
            io::stdout(),
            "\x1b[r\x1b[1;{}r",
            self.height - self.footer_rows()
        )?;
        if starting {
            write!(io::stdout(), "\x1b8")?;
        } else {
            write!(io::stdout(), "\x1b[{};1H", self.height - self.footer_rows())?;
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
        let view = self.editor.viewport(
            width.saturating_sub(label.len() + 1),
            usize::from(self.footer_rows() - 1),
        );
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
        let top = self.height - self.footer_rows() + 1;
        write!(out, "{reset}\x1b[{top};1H\x1b[J{hint}")?;
        let first = self.height - u16::try_from(view.lines.len()).unwrap() + 1;
        for (index, text) in view.lines.iter().enumerate() {
            let row = first + u16::try_from(index).unwrap();
            let prefix = if index == 0 {
                label.clone()
            } else {
                " ".repeat(label.len())
            };
            // An ellipsis marks earlier lines hidden by the bounded viewport.
            let prefix = if index == 0 && view.clipped {
                format!("…{}", " ".repeat(label.len().saturating_sub(1)))
            } else {
                prefix
            };
            write!(out, "\x1b[{row};1H{color}{prefix}{reset}{text}")?;
        }
        write!(
            out,
            "\x1b[{};{}H",
            first + u16::try_from(view.row).unwrap(),
            (label.len() + view.column + 1).min(width)
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
        crossterm::execute!(
            io::stdout(),
            event::EnableBracketedPaste,
            event::PushKeyboardEnhancementFlags(
                event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
        )?;
        let mut display = Display {
            editor: Editor::default(),
            spacing: Spacing::default(),
            width,
            height,
            queue: None,
            approval: false,
        };
        // Reserve the input rows without overwriting the latest shell/startup output.
        let reserved = display.footer_rows();
        write!(
            io::stdout(),
            "{}\x1b[{reserved}A\x1b7",
            "\r\n".repeat(usize::from(reserved))
        )?;
        display.layout(width, height, true)?;
        *DISPLAY.lock().expect("display poisoned") = Some(display);
        Ok(Some(session))
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let display = DISPLAY.lock().ok().and_then(|mut display| display.take());
        if let Some(display) = display {
            let _ = write!(
                io::stdout(),
                "\x1b[r\x1b[{};1H\x1b[J",
                display.height - display.footer_rows() + 1
            );
        }
        let _ = crossterm::execute!(
            io::stdout(),
            event::PopKeyboardEnhancementFlags,
            event::DisableBracketedPaste
        );
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
    fn multiline_editing_wraps_and_keeps_full_prompt_when_clipped() {
        let mut editor = Editor::default();
        editor.paste("first");
        assert!(
            editor
                .key(KeyCode::Enter, KeyModifiers::SHIFT, true)
                .is_none()
        );
        editor.paste("second");
        let view = editor.viewport(4, 2);
        assert_eq!(view.lines, ["seco", "nd"]);
        assert!(view.clipped);
        assert_eq!((view.row, view.column), (1, 2));
        editor.key(KeyCode::Up, KeyModifiers::NONE, true);
        assert_eq!(editor.cursor, 5);
        editor.key(KeyCode::Down, KeyModifiers::NONE, true);
        editor.key(KeyCode::End, KeyModifiers::NONE, true);
        assert!(
            matches!(editor.key(KeyCode::Enter, KeyModifiers::NONE, true), Some(Input::Line(text)) if text == "first\nsecond")
        );
        editor.paste("a界bc\nlast\n");
        let view = editor.viewport(4, 6);
        assert_eq!(view.lines, ["a界b", "c", "last", ""]);
        assert_eq!((view.row, view.column), (3, 0));
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
        assert_eq!(line, "first\nsecond");
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
        assert!(matches!(read(false).await.unwrap(), Input::Line(line) if line == "saved\ndraft"));
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

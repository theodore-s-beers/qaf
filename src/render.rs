//! Presentation only: conversation history and transcripts retain the original Markdown.
use std::{borrow::Cow, io::IsTerminal};

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

fn styled() -> bool {
    !(!std::io::stdout().is_terminal()
        || std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty())
        || std::env::var_os("TERM").is_some_and(|value| value == "dumb"))
}

fn terminal_width() -> usize {
    terminal_size::terminal_size().map_or(80, |(width, _)| usize::from(width.0))
}

// Render only stable top-level Markdown blocks. The unfinished final block stays
// buffered so a fence, list, or emphasis span is never interpreted halfway through.
pub struct Stream {
    source: String,
    emitted: usize,
    started: bool,
    styled: bool,
}

impl Stream {
    pub fn new() -> Self {
        Self {
            source: String::new(),
            emitted: 0,
            started: false,
            styled: styled(),
        }
    }

    pub fn push(&mut self, delta: &str) -> String {
        let boundary_changed = self.source.ends_with('\n') || delta.contains('\n');
        self.source.push_str(delta);
        let end = if !self.styled {
            self.source.len()
        } else if boundary_changed {
            stable_end(&self.source)
        } else {
            self.emitted
        };
        self.take(end)
    }

    pub fn finish(&mut self, final_text: &str) -> anyhow::Result<String> {
        anyhow::ensure!(
            final_text.starts_with(&self.source[..self.emitted]),
            "completed text differs from streamed text"
        );
        final_text.clone_into(&mut self.source);
        Ok(self.finish_partial())
    }

    pub fn finish_partial(&mut self) -> String {
        let mut output = self.take(self.source.len());
        if self.started {
            output.push('\n');
        }
        self.source.clear();
        self.emitted = 0;
        self.started = false;
        output
    }

    fn take(&mut self, end: usize) -> String {
        if end <= self.emitted {
            return String::new();
        }
        let chunk = &self.source[self.emitted..end];
        let text = if self.styled {
            markdown(chunk, terminal_width()).trim_end().to_owned()
        } else {
            chunk.to_owned()
        };
        self.emitted = end;
        if text.is_empty() {
            return text;
        }
        let prefix = if !self.started {
            "\n"
        } else if self.styled {
            "\n\n"
        } else {
            ""
        };
        self.started = true;
        format!("{prefix}{text}")
    }
}

fn stable_end(source: &str) -> usize {
    // A partial line (especially a lone table-row pipe) can look like a new
    // block and then merge back into the preceding block as more text arrives.
    let Some(end) = source.rfind('\n') else {
        return 0;
    };
    let source = &source[..=end];
    let mut depth = 0;
    let mut last_start = 0;
    for (event, range) in Parser::new_ext(source, Options::ENABLE_TABLES).into_offset_iter() {
        match event {
            Event::Start(_) => {
                if depth == 0 {
                    last_start = range.start;
                }
                depth += 1;
            }
            Event::End(_) => depth -= 1,
            Event::Rule if depth == 0 => last_start = range.start,
            _ => {}
        }
    }
    last_start
}

#[derive(Default)]
struct Renderer {
    width: usize,
    lines: Vec<String>,
    text: String,
    // Each container has a first-line prefix and a continuation prefix.
    prefixes: Vec<(String, String)>,
    lists: Vec<Option<u64>>,
    styles: Vec<&'static str>,
    links: Vec<String>,
}

impl Renderer {
    fn flush(&mut self) {
        if self.text.is_empty() {
            return;
        }
        let first: String = self
            .prefixes
            .iter()
            .map(|(first, _)| first.as_str())
            .collect();
        let rest: String = self
            .prefixes
            .iter()
            .map(|(_, rest)| rest.as_str())
            .collect();
        let options = textwrap::Options::new(self.width.max(1))
            .initial_indent(&first)
            .subsequent_indent(&rest)
            .break_words(false);
        self.lines.extend(
            textwrap::wrap(&self.text, options)
                .into_iter()
                .map(Cow::into_owned),
        );
        self.text.clear();
        for (first, rest) in &mut self.prefixes {
            first.clone_from(rest);
        }
    }

    fn gap(&mut self) {
        self.flush();
        if self.lines.last().is_some_and(|line| !line.is_empty()) {
            self.lines.push(String::new());
        }
    }

    fn style(&mut self, style: &'static str) {
        self.styles.push(style);
        self.text.push_str(style);
    }

    fn end_style(&mut self) {
        self.styles.pop();
        self.text.push_str("\x1b[0m");
        for style in &self.styles {
            self.text.push_str(style);
        }
    }

    fn verbatim(&mut self, source: &str, indent: &str) {
        self.flush();
        for line in source.split_terminator('\n') {
            let prefix: String = self
                .prefixes
                .iter()
                .map(|(first, _)| first.as_str())
                .collect();
            self.lines.push(format!("{prefix}{indent}{line}"));
            for (first, rest) in &mut self.prefixes {
                first.clone_from(rest);
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.flush(),
            Tag::Heading { .. } => {
                self.gap();
                self.style("\x1b[1m");
            }
            Tag::BlockQuote(_) => {
                self.gap();
                self.prefixes.push(("> ".into(), "> ".into()));
            }
            Tag::List(start) => {
                if self.lists.is_empty() {
                    self.gap();
                } else {
                    self.flush();
                }
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush();
                let marker = match self.lists.last_mut() {
                    Some(Some(number)) => {
                        let marker = format!("{number}. ");
                        *number = number.saturating_add(1);
                        marker
                    }
                    _ => "- ".into(),
                };
                let indent = " ".repeat(marker.len());
                self.prefixes.push((marker, indent));
            }
            Tag::Strong => self.style("\x1b[1m"),
            Tag::Emphasis => self.style("\x1b[3m"),
            Tag::Strikethrough => self.style("\x1b[9m"),
            Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                self.links.push(dest_url.into_string());
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough => self.end_style(),
            TagEnd::Heading(_) => {
                self.end_style();
                self.gap();
            }
            TagEnd::Paragraph => self.gap(),
            TagEnd::Item => {
                self.flush();
                self.prefixes.pop();
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.gap();
                }
            }
            TagEnd::BlockQuote(_) => {
                self.gap();
                self.prefixes.pop();
            }
            TagEnd::Link | TagEnd::Image => {
                if let Some(destination) = self.links.pop() {
                    // Autolinks already display their destination.
                    if !self.text.ends_with(&destination) {
                        self.text.push_str(" (");
                        self.text.push_str(&destination);
                        self.text.push(')');
                    }
                }
            }
            _ => {}
        }
    }
}

fn table<'a>(
    events: &mut impl Iterator<Item = (Event<'a>, std::ops::Range<usize>)>,
    width: usize,
) -> Option<String> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut cell = Renderer::default();
    let mut header = false;
    for (event, _) in events.by_ref() {
        match event {
            Event::Start(Tag::TableHead) => {
                header = true;
                rows.push(Vec::new());
            }
            Event::End(TagEnd::TableHead) => header = false,
            Event::Start(Tag::TableRow) => rows.push(Vec::new()),
            Event::Start(Tag::TableCell) if header => cell.style("\x1b[1m"),
            Event::End(TagEnd::TableCell) => {
                if header {
                    cell.end_style();
                }
                rows.last_mut()?.push(std::mem::take(&mut cell.text));
            }
            Event::End(TagEnd::Table) => break,
            Event::Start(tag) => cell.start(tag),
            Event::End(tag) => cell.end(tag),
            Event::Text(text) | Event::InlineHtml(text) => cell.text.push_str(&text),
            Event::Code(text) => {
                cell.style("\x1b[36m");
                cell.text.push_str(&text);
                cell.end_style();
            }
            Event::SoftBreak | Event::HardBreak => cell.text.push(' '),
            _ => {}
        }
    }
    let columns = rows.first()?.len();
    if columns == 0 {
        return None;
    }
    let mut widths = vec![1; columns];
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(textwrap::core::display_width(cell));
        }
    }
    let minimum: Vec<_> = widths.iter().map(|width| (*width).min(8)).collect();
    let budget = width.checked_sub(2 * (columns - 1))?;
    if minimum.iter().sum::<usize>() > budget {
        return None;
    }
    let mut total = widths.iter().sum::<usize>();
    while total > budget {
        let index = (0..columns)
            .filter(|&i| widths[i] > minimum[i])
            .max_by_key(|&i| widths[i])?;
        widths[index] -= 1;
        total -= 1;
    }
    let mut lines = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let wrapped: Vec<_> = row
            .iter()
            .zip(&widths)
            .map(|(cell, &width)| wrap_cell(cell, width))
            .collect();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        for line in 0..height {
            let mut cells = Vec::new();
            for (column, &width) in widths.iter().enumerate() {
                let text = wrapped
                    .get(column)
                    .and_then(|cell| cell.get(line))
                    .map_or("", String::as_str);
                let padding = width.saturating_sub(textwrap::core::display_width(text));
                cells.push(format!("{text}{}", " ".repeat(padding)));
            }
            lines.push(cells.join("  ").trim_end().to_owned());
        }
        if index == 0 {
            lines.push(
                widths
                    .iter()
                    .map(|&width| "─".repeat(width))
                    .collect::<Vec<_>>()
                    .join("  "),
            );
        }
    }
    Some(lines.join("\n"))
}

fn wrap_cell(source: &str, width: usize) -> Vec<String> {
    let mut active = String::new();
    textwrap::wrap(source, width)
        .iter()
        .map(|line| {
            let prefix = active.clone();
            // Only SGR styles are generated by the inline renderer. Restore them after
            // wrapping, and reset before padding so styles never leak into other cells.
            for sequence in line.split("\x1b[").skip(1) {
                if let Some((code, _)) = sequence.split_once('m') {
                    if code == "0" {
                        active.clear();
                    } else {
                        active.push_str("\x1b[");
                        active.push_str(code);
                        active.push('m');
                    }
                }
            }
            format!("{prefix}{line}\x1b[0m")
        })
        .collect()
}

fn markdown(source: &str, width: usize) -> String {
    let mut renderer = Renderer {
        width,
        ..Renderer::default()
    };
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut events = Parser::new_ext(source, options).into_offset_iter();
    while let Some((event, range)) = events.next() {
        match event {
            Event::Start(Tag::Table(_)) => {
                renderer.gap();
                let prefix: String = renderer
                    .prefixes
                    .iter()
                    .map(|(first, _)| first.as_str())
                    .collect();
                let available = width.saturating_sub(textwrap::core::display_width(&prefix));
                let table = table(&mut events, available);
                renderer.verbatim(table.as_deref().unwrap_or_else(|| &source[range]), "");
                renderer.gap();
            }
            Event::Start(Tag::CodeBlock(_)) => {
                renderer.gap();
                let mut code = String::new();
                for (event, _) in events.by_ref() {
                    match event {
                        Event::Text(text) => code.push_str(&text),
                        Event::End(TagEnd::CodeBlock) => break,
                        _ => {}
                    }
                }
                renderer.verbatim(&code, "    ");
                renderer.gap();
            }
            Event::Start(tag) => renderer.start(tag),
            Event::End(tag) => renderer.end(tag),
            Event::Text(text) | Event::InlineHtml(text) => renderer.text.push_str(&text),
            Event::Html(text) => renderer.verbatim(&text, ""),
            Event::Code(text) => {
                renderer.style("\x1b[36m");
                renderer.text.push_str(&text);
                renderer.end_style();
            }
            Event::SoftBreak => renderer.text.push(' '),
            Event::HardBreak => renderer.flush(),
            Event::Rule => {
                renderer.gap();
                renderer.verbatim(&"─".repeat(width.clamp(1, 40)), "");
                renderer.gap();
            }
            Event::TaskListMarker(checked) => {
                renderer
                    .text
                    .push_str(if checked { "[x] " } else { "[ ] " });
            }
            _ => {}
        }
    }
    renderer.flush();
    while renderer.lines.last().is_some_and(String::is_empty) {
        renderer.lines.pop();
    }
    renderer.lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::{Stream, markdown};

    #[test]
    fn streaming_buffers_unfinished_blocks_and_flushes_partial_text() {
        let mut stream = Stream::new();
        stream.styled = true;
        assert_eq!(stream.push("First **bold"), "");
        assert_eq!(stream.push("** paragraph.\n\n"), "");
        assert_ne!(stream.push("```rust\n"), "");
        let emitted = stream.emitted;
        assert_eq!(stream.push("let x = 1;\n\n"), "");
        assert_eq!(stream.emitted, emitted);
        assert_eq!(stream.push("```\n\n"), "");
        assert_ne!(stream.push("Next paragraph\n"), "");
        let remaining = stream.finish_partial();
        assert!(remaining.contains("Next paragraph"));
        assert_eq!(stream.finish_partial(), "");

        stream.styled = false;
        assert_eq!(stream.push("A café"), "\nA café");
        assert_eq!(stream.push("."), ".");
        assert_eq!(
            stream
                .finish("A café.\n\nSources:\n- <https://example.com>")
                .unwrap(),
            "\n\nSources:\n- <https://example.com>\n"
        );
        assert_eq!(
            stream.finish("No delta fallback").unwrap(),
            "\nNo delta fallback\n"
        );
    }

    #[test]
    fn nested_styles_restore_outer_style() {
        assert_eq!(
            markdown("**bold *italic* bold** and `code`", 80),
            "\x1b[1mbold \x1b[3mitalic\x1b[0m\x1b[1m bold\x1b[0m and \x1b[36mcode\x1b[0m"
        );
    }

    #[test]
    fn lists_wrap_with_hanging_indent_and_preserve_nesting() {
        assert_eq!(
            markdown("3. alpha beta gamma\n   - inner item\n4. last", 16),
            "3. alpha beta\n   gamma\n   - inner item\n4. last"
        );
    }

    #[test]
    fn code_is_not_reflowed_or_parsed_even_with_unclosed_fence() {
        assert_eq!(
            markdown("```rust\nlet long_name = \"**literal**\";\n\n  tail  ", 8),
            "    let long_name = \"**literal**\";\n    \n      tail  "
        );
    }

    fn plain(source: &str) -> String {
        let mut pieces = source.split("\x1b[");
        let mut text = pieces.next().unwrap_or("").to_owned();
        for piece in pieces {
            text.push_str(piece.split_once('m').unwrap().1);
        }
        text
    }

    #[test]
    fn tables_align_compact_rows_and_preserve_inline_styles() {
        let output = markdown(
            "| Issue | Title |\n|---|---|\n| #12 | `alpha` |\n| #345 | **beta** |",
            80,
        );
        assert_eq!(
            plain(&output),
            "Issue  Title\n─────  ─────\n#12    alpha\n#345   beta"
        );
        assert!(output.contains("\x1b[1mIssue"));
        assert!(output.contains("\x1b[36malpha"));
        assert!(output.contains("\x1b[1mbeta"));
    }

    #[test]
    fn tables_wrap_long_cells_without_losing_unicode_or_links() {
        let output = markdown(
            "| ID | Description |\n|---|---|\n| 1 | **你好 世界 alpha beta gamma delta** |\n| 2 | [docs](https://example.com/long/path) |",
            24,
        );
        let text = plain(&output);
        assert!(
            text.lines()
                .all(|line| textwrap::core::display_width(line) <= 24)
        );
        let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(compact.contains("你好世界alphabetagammadelta"));
        assert!(compact.contains("docs(https://example.com/long/path)"));
        assert_eq!(
            super::wrap_cell("\x1b[36malpha beta\x1b[0m", 5),
            vec!["\x1b[36malpha\x1b[0m", "\x1b[36mbeta\x1b[0m\x1b[0m"]
        );
    }

    #[test]
    fn streaming_waits_for_the_whole_table() {
        let mut stream = Stream::new();
        stream.styled = true;
        for chunk in [
            "| Issue | Title |\n",
            "|---|---|\n",
            "| 1 | short |\n",
            "| 2 | much longer title |\n\n",
        ] {
            assert_eq!(stream.push(chunk), "");
        }
        assert_ne!(stream.push("After the table\n"), "");
        assert!(stream.source[..stream.emitted].contains("much longer title"));
        assert_eq!(stream.finish_partial().trim(), "After the table");
    }

    #[test]
    fn streaming_partial_table_rows_match_complete_rendering() {
        let source = "Intro.\n\n| Issue | Title |\n|---|---|\n| #12 | `alpha` |\n| #345 | **longer title** |\n\nAfter.\n";
        for chunk_size in [1, 2, 3, 7, source.len()] {
            let mut stream = Stream::new();
            stream.styled = true;
            let mut output = String::new();
            for chunk in source.as_bytes().chunks(chunk_size) {
                output.push_str(&stream.push(std::str::from_utf8(chunk).unwrap()));
                let table_start = source.find("| Issue").unwrap();
                let table_end = source.find("After.").unwrap();
                assert!(
                    stream.emitted <= table_start || stream.emitted >= table_end,
                    "emitted part of the table at byte {} with chunk size {chunk_size}",
                    stream.emitted
                );
            }
            output.push_str(&stream.finish(source).unwrap());
            assert_eq!(output.trim(), markdown(source, super::terminal_width()));
        }
    }

    #[test]
    fn tables_and_link_destinations_are_retained() {
        let table = "| A | B |\n|---|---|\n| x | y |";
        assert_eq!(markdown(table, 2), table);
        assert_eq!(
            markdown("[docs](https://example.com)", 80),
            "docs (https://example.com)"
        );
    }

    #[test]
    fn unicode_and_narrow_widths_preserve_words() {
        assert_eq!(markdown("你好 世界 hello", 9), "你好 世界\nhello");
        assert_eq!(markdown("- hello world", 1), "- hello\n  world");
    }
}

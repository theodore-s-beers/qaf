//! Presentation only: conversation history and transcripts retain the original Markdown.
use std::{borrow::Cow, io::IsTerminal};

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

pub fn response(source: &str) -> Cow<'_, str> {
    if !std::io::stdout().is_terminal()
        || std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty())
        || std::env::var_os("TERM").is_some_and(|value| value == "dumb")
    {
        return Cow::Borrowed(source);
    }
    let width = terminal_size::terminal_size().map_or(80, |(width, _)| usize::from(width.0));
    Cow::Owned(markdown(source, width))
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
                renderer.verbatim(&source[range], "");
                for (event, _) in events.by_ref() {
                    if event == Event::End(TagEnd::Table) {
                        break;
                    }
                }
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
    use super::markdown;

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

    #[test]
    fn tables_and_link_destinations_are_retained() {
        let table = "| A | B |\n|---|---|\n| x | y |";
        assert_eq!(markdown(table, 8), table);
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

//! Markdown and width-aware transcript rendering.

use crate::components::text_layout::trim_before_ellipsis;
use crate::theme;
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::layout::Rect;
#[cfg(test)]
use ratatui::style::Color;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::ops::ControlFlow;
use unicode_segmentation::UnicodeSegmentation;

/// The microphone glyph in the symbol set in force, so a console without UTF-8
/// gets the plain button rather than mojibake in the composer's border.
pub(super) fn voice_button_glyph() -> &'static str {
    theme::glyphs().microphone
}

/// The microphone button is a chip in the upper-left of the prompt's top
/// border. Keep its geometry derived from the same Unicode width ratatui uses
/// to lay out `Line`s.
pub(super) fn voice_button_area(prompt_area: Rect) -> Option<Rect> {
    if prompt_area.width == 0 || prompt_area.height == 0 {
        return None;
    }
    let button_width = display_width(&format!(" {} ", voice_button_glyph()));
    let button_width = u16::try_from(button_width).ok()?;
    let x = prompt_area.x.saturating_add(1);
    (x.saturating_add(button_width) < prompt_area.right())
        .then(|| Rect::new(x, prompt_area.y, button_width, 1))
}

/// Renders the microphone chip that belongs to the prompt's top border.
pub(super) fn voice_button_line(voice_available: bool, voice_active: bool) -> Line<'static> {
    let style = if voice_active {
        theme::filled(theme::palette().error).add_modifier(Modifier::BOLD)
    } else if voice_available {
        theme::filled(theme::palette().accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(theme::palette().muted)
            .patch(theme::raised())
    };
    Line::from(Span::styled(format!(" {} ", voice_button_glyph()), style)).left_aligned()
}

pub(super) use mj_client::transcript::TranscriptRenderMode;

#[derive(Debug, Clone)]
pub(super) struct LogicalLine {
    pub(super) line: Line<'static>,
    pub(super) continuation_indent: usize,
    /// The spans of `line` that render a Markdown link, and where each points.
    pub(super) links: Vec<LinkSpan>,
}

/// One span of a rendered line that belongs to a Markdown link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LinkSpan {
    /// Index into the line's spans.
    pub(super) span: usize,
    pub(super) url: String,
}

#[derive(Debug, Clone)]
struct ListState {
    next: Option<u64>,
}

#[derive(Debug, Default)]
struct TableState {
    alignments: Vec<Alignment>,
    rows: Vec<Vec<TableCell>>,
    row: Vec<TableCell>,
    cell: TableCell,
}

/// One table cell's styled text, laid out once the whole table is known.
#[derive(Debug, Clone, Default)]
struct TableCell {
    spans: Vec<Span<'static>>,
    /// Link spans, indexed into `spans`.
    links: Vec<LinkSpan>,
}

impl TableCell {
    fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    fn text(&self) -> String {
        self.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }
}

struct MarkdownWriter {
    lines: Vec<LogicalLine>,
    spans: Vec<Span<'static>>,
    style: Style,
    quote_depth: usize,
    lists: Vec<ListState>,
    item_prefix: Option<String>,
    table: Option<TableState>,
    width: usize,
    /// Destination of the link whose text is being written, if any.
    link: Option<String>,
    /// Link spans of the line in progress, indexed into `spans`.
    line_links: Vec<LinkSpan>,
    /// Consecutive text events not yet written. The parser can split one
    /// run of text into several events, so URLs are found in the whole run.
    pending_text: String,
}

impl MarkdownWriter {
    fn new(width: usize, style: Style) -> Self {
        Self {
            lines: Vec::new(),
            spans: Vec::new(),
            style,
            quote_depth: 0,
            lists: Vec::new(),
            item_prefix: None,
            table: None,
            width: width.max(1),
            link: None,
            line_links: Vec::new(),
            pending_text: String::new(),
        }
    }

    /// Adds an inline span to the table cell or line in progress, recording
    /// it as link text inside a link.
    fn push_span(&mut self, span: Span<'static>) {
        let (spans, links) = match &mut self.table {
            Some(table) => (&mut table.cell.spans, &mut table.cell.links),
            None => (&mut self.spans, &mut self.line_links),
        };
        if let Some(url) = &self.link {
            links.push(LinkSpan {
                span: spans.len(),
                url: url.clone(),
            });
        }
        spans.push(span);
    }

    fn push_text(&mut self, text: &str, style: Style) {
        if self.table.is_some() {
            // A table cell is one row of source text.
            self.push_span(Span::styled(text.replace('\n', " "), style));
            return;
        }
        let mut parts = text.split('\n').peekable();
        while let Some(part) = parts.next() {
            if !part.is_empty() {
                self.push_span(Span::styled(part.to_owned(), style));
            }
            if parts.peek().is_some() {
                self.finish_line();
            }
        }
    }

    /// Writes text, turning each URL written out in it into a link to itself.
    /// Text that is already a link's label stays one link.
    fn push_linkified(&mut self, text: &str, style: Style) {
        if self.link.is_some() {
            self.push_text(text, style);
            return;
        }
        let mut finder = linkify::LinkFinder::new();
        finder.kinds(&[linkify::LinkKind::Url]);
        for part in finder.spans(text) {
            if part.kind().is_some() {
                self.link = Some(part.as_str().to_owned());
                self.push_text(part.as_str(), style.add_modifier(Modifier::UNDERLINED));
                self.link = None;
            } else {
                self.push_text(part.as_str(), style);
            }
        }
    }

    fn flush_text(&mut self) {
        if self.pending_text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.pending_text);
        self.push_linkified(&text, self.style);
    }

    fn finish_line(&mut self) {
        let quote = "> ".repeat(self.quote_depth);
        let item = self.item_prefix.take().unwrap_or_default();
        let continuation_indent = display_width(&quote) + display_width(&item);
        let mut spans = Vec::with_capacity(self.spans.len() + 2);
        if !quote.is_empty() {
            spans.push(Span::styled(
                quote,
                Style::default().fg(theme::palette().muted),
            ));
        }
        if !item.is_empty() {
            spans.push(Span::styled(
                item,
                Style::default().fg(theme::palette().muted),
            ));
        }
        let prefix_spans = spans.len();
        spans.append(&mut self.spans);
        let links = self
            .line_links
            .drain(..)
            .map(|link| LinkSpan {
                span: link.span + prefix_spans,
                url: link.url,
            })
            .collect();
        self.lines.push(LogicalLine {
            line: Line::from(spans),
            continuation_indent,
            links,
        });
    }

    fn finish_block(&mut self) {
        if !self.spans.is_empty() || self.item_prefix.is_some() {
            self.finish_line();
        }
        if self
            .lines
            .last()
            .is_some_and(|line| !line.line.spans.is_empty())
        {
            self.lines.push(LogicalLine {
                line: Line::default(),
                continuation_indent: 0,
                links: Vec::new(),
            });
        }
    }

    fn finish(mut self) -> Vec<LogicalLine> {
        if !self.spans.is_empty() || self.item_prefix.is_some() {
            self.finish_line();
        }
        while self
            .lines
            .last()
            .is_some_and(|line| line.line.spans.is_empty())
        {
            self.lines.pop();
        }
        self.lines
    }

    fn render_table(&mut self, mut table: TableState) {
        if !table.cell.is_empty() || !table.row.is_empty() {
            table.row.push(std::mem::take(&mut table.cell));
        }
        if !table.row.is_empty() {
            table.rows.push(std::mem::take(&mut table.row));
        }
        let Some(header) = table
            .rows
            .first()
            .map(|row| row.iter().map(TableCell::text).collect::<Vec<_>>())
        else {
            return;
        };
        let columns = table.alignments.len().max(header.len()).max(1);
        for row in &mut table.rows {
            row.truncate(columns);
            row.resize(columns, TableCell::default());
        }
        table.alignments.resize(columns, Alignment::None);
        let texts = table
            .rows
            .iter()
            .map(|row| row.iter().map(TableCell::text).collect::<Vec<_>>())
            .collect::<Vec<_>>();

        const CELL_PADDING: usize = 1;
        const COLUMN_GAP: usize = 2;
        const MIN_COLUMN_WIDTH: usize = 3;
        let reserved = columns * CELL_PADDING * 2 + columns.saturating_sub(1) * COLUMN_GAP;
        let available = self.width.saturating_sub(reserved);
        let mut column_widths = (0..columns)
            .map(|column| {
                texts
                    .iter()
                    .map(|row| display_width(&row[column]))
                    .max()
                    .unwrap_or(0)
                    .max(MIN_COLUMN_WIDTH)
            })
            .collect::<Vec<_>>();

        while column_widths.iter().sum::<usize>() > available {
            let Some((column, _)) = column_widths
                .iter()
                .enumerate()
                .filter(|(_, width)| **width > MIN_COLUMN_WIDTH)
                .max_by_key(|(_, width)| **width)
            else {
                break;
            };
            column_widths[column] -= 1;
        }
        let grid_fits = column_widths.iter().sum::<usize>() <= available;
        let fragments_tokens = texts.iter().skip(1).any(|row| {
            row.iter().zip(&column_widths).any(|(cell, width)| {
                *width < 12
                    && cell
                        .split_whitespace()
                        .any(|token| display_width(token) > *width)
            })
        });
        if grid_fits && !fragments_tokens {
            let rows = std::mem::take(&mut table.rows);
            self.render_table_row(
                &rows[0],
                &column_widths,
                &table.alignments,
                Style::default().add_modifier(Modifier::BOLD),
            );
            let separator = column_widths
                .iter()
                .map(|width| crate::theme::glyphs().rule.repeat(width + CELL_PADDING * 2))
                .collect::<Vec<_>>()
                .join(&" ".repeat(COLUMN_GAP));
            self.lines.push(LogicalLine {
                line: Line::from(Span::styled(
                    separator,
                    Style::default().fg(theme::palette().muted),
                )),
                continuation_indent: 0,
                links: Vec::new(),
            });
            for row in rows.iter().skip(1) {
                self.render_table_row(row, &column_widths, &table.alignments, Style::default());
            }
        } else {
            for (row_index, row) in table.rows.into_iter().skip(1).enumerate() {
                if row_index > 0 {
                    self.lines.push(LogicalLine {
                        line: Line::from(Span::styled(
                            "────────────────────",
                            Style::default().fg(theme::palette().muted),
                        )),
                        continuation_indent: 0,
                        links: Vec::new(),
                    });
                }
                for (column, value) in row.into_iter().enumerate() {
                    let label = header
                        .get(column)
                        .filter(|label| !label.is_empty())
                        .cloned()
                        .unwrap_or_else(|| format!("Column {}", column + 1));
                    let continuation_indent = display_width(&label) + 2;
                    let mut spans = vec![Span::styled(
                        format!("{label}: "),
                        Style::default().add_modifier(Modifier::BOLD),
                    )];
                    spans.extend(value.spans);
                    self.lines.push(LogicalLine {
                        line: Line::from(spans),
                        continuation_indent,
                        links: value
                            .links
                            .into_iter()
                            .map(|link| LinkSpan {
                                span: link.span + 1,
                                url: link.url,
                            })
                            .collect(),
                    });
                }
            }
        }
    }

    fn render_table_row(
        &mut self,
        row: &[TableCell],
        column_widths: &[usize],
        alignments: &[Alignment],
        emphasis: Style,
    ) {
        const CELL_PADDING: usize = 1;
        const COLUMN_GAP: usize = 2;
        let cells = row
            .iter()
            .zip(column_widths)
            .map(|(cell, width)| {
                let spans = cell
                    .spans
                    .iter()
                    .map(|span| Span::styled(span.content.clone(), span.style.patch(emphasis)))
                    .collect::<Vec<_>>();
                wrap_styled_line_with_sources(Line::from(spans), *width, 0)
            })
            .collect::<Vec<_>>();
        let height = cells.iter().map(|(rows, _)| rows.len()).max().unwrap_or(1);
        for line_index in 0..height {
            let mut spans = Vec::new();
            let mut links = Vec::new();
            for (column, width) in column_widths.iter().copied().enumerate() {
                if column > 0 {
                    spans.push(Span::raw(" ".repeat(COLUMN_GAP)));
                }
                let (rows, sources) = &cells[column];
                let value = rows.get(line_index).cloned().unwrap_or_default();
                let remaining = width.saturating_sub(value.width());
                let (left, right) = match alignments[column] {
                    Alignment::Left | Alignment::None => (0, remaining),
                    Alignment::Center => (remaining / 2, remaining - remaining / 2),
                    Alignment::Right => (remaining, 0),
                };
                spans.push(Span::raw(" ".repeat(CELL_PADDING + left)));
                let cell_sources = sources.get(line_index).map_or(&[][..], Vec::as_slice);
                let mut offset = 0;
                for span in value.spans {
                    // Each wrapped span comes from one source span of the cell.
                    let source = cell_sources.get(offset).copied().flatten();
                    if let Some(link) = row[column]
                        .links
                        .iter()
                        .find(|link| Some(link.span) == source)
                    {
                        links.push(LinkSpan {
                            span: spans.len(),
                            url: link.url.clone(),
                        });
                    }
                    offset += display_width(&span.content);
                    spans.push(span);
                }
                spans.push(Span::raw(" ".repeat(right + CELL_PADDING)));
            }
            self.lines.push(LogicalLine {
                line: Line::from(spans),
                continuation_indent: 0,
                links,
            });
        }
    }
}

pub(super) use mj_core::transcript::sanitize_terminal_text;

pub(super) fn markdown_lines(
    source: &str,
    body_style: Style,
    accent_style: Style,
    width: usize,
) -> Vec<LogicalLine> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    let parser = Parser::new_ext(source, options);
    let mut writer = MarkdownWriter::new(width, body_style);
    let mut style_stack = Vec::new();

    for event in parser {
        if let Event::Text(text) = &event {
            writer.pending_text.push_str(text);
            continue;
        }
        writer.flush_text();
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {}
                Tag::Heading { level, .. } => {
                    let count = match level {
                        HeadingLevel::H1 => 1,
                        HeadingLevel::H2 => 2,
                        HeadingLevel::H3 => 3,
                        HeadingLevel::H4 => 4,
                        HeadingLevel::H5 => 5,
                        HeadingLevel::H6 => 6,
                    };
                    writer.spans.push(Span::styled(
                        format!("{} ", "#".repeat(count)),
                        Style::default().fg(theme::palette().muted),
                    ));
                    style_stack.push(writer.style);
                    writer.style = accent_style.add_modifier(Modifier::BOLD);
                }
                Tag::BlockQuote => writer.quote_depth += 1,
                Tag::CodeBlock(kind) => {
                    writer.finish_block();
                    let language = match kind {
                        CodeBlockKind::Fenced(language) if !language.is_empty() => {
                            format!("code · {language}")
                        }
                        _ => "code".to_owned(),
                    };
                    writer.lines.push(LogicalLine {
                        line: Line::from(Span::styled(
                            language,
                            Style::default()
                                .fg(theme::palette().muted)
                                .add_modifier(Modifier::BOLD),
                        )),
                        continuation_indent: 0,
                        links: Vec::new(),
                    });
                    style_stack.push(writer.style);
                    writer.style = Style::default()
                        .fg(theme::palette().text)
                        .patch(theme::raised());
                }
                Tag::List(start) => writer.lists.push(ListState { next: start }),
                Tag::Item => {
                    let depth = writer.lists.len().saturating_sub(1);
                    let marker = writer
                        .lists
                        .last_mut()
                        .and_then(|list| list.next.as_mut())
                        .map_or_else(
                            || "• ".to_owned(),
                            |next| {
                                let marker = format!("{next}. ");
                                *next += 1;
                                marker
                            },
                        );
                    writer.item_prefix = Some(format!("{}{marker}", "  ".repeat(depth)));
                }
                Tag::Emphasis => {
                    style_stack.push(writer.style);
                    writer.style = writer.style.add_modifier(Modifier::ITALIC);
                }
                Tag::Strong => {
                    style_stack.push(writer.style);
                    writer.style = writer.style.add_modifier(Modifier::BOLD);
                }
                Tag::Strikethrough => {
                    style_stack.push(writer.style);
                    writer.style = writer.style.add_modifier(Modifier::CROSSED_OUT);
                }
                Tag::Link { dest_url, .. } => {
                    writer.link = Some(dest_url.into_string());
                    style_stack.push(writer.style);
                    writer.style = writer
                        .style
                        .fg(theme::palette().text)
                        .add_modifier(Modifier::UNDERLINED);
                }
                Tag::Table(alignments) => {
                    writer.table = Some(TableState {
                        alignments,
                        ..TableState::default()
                    });
                }
                Tag::TableHead | Tag::TableRow | Tag::TableCell | Tag::Image { .. } => {}
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => writer.finish_block(),
                TagEnd::Heading(_) => {
                    writer.style = style_stack.pop().unwrap_or(body_style);
                    writer.finish_block();
                }
                TagEnd::BlockQuote => {
                    writer.finish_block();
                    writer.quote_depth = writer.quote_depth.saturating_sub(1);
                }
                TagEnd::CodeBlock => {
                    if !writer.spans.is_empty() {
                        writer.finish_line();
                    }
                    writer.style = style_stack.pop().unwrap_or(body_style);
                    writer.finish_block();
                }
                TagEnd::List(_) => {
                    writer.finish_block();
                    writer.lists.pop();
                }
                TagEnd::Item => writer.finish_block(),
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    writer.style = style_stack.pop().unwrap_or(body_style);
                }
                TagEnd::Link => {
                    writer.link = None;
                    writer.style = style_stack.pop().unwrap_or(body_style);
                }
                TagEnd::TableCell => {
                    if let Some(table) = &mut writer.table {
                        table.row.push(std::mem::take(&mut table.cell));
                    }
                }
                TagEnd::TableHead | TagEnd::TableRow => {
                    if let Some(table) = &mut writer.table {
                        table.rows.push(std::mem::take(&mut table.row));
                    }
                }
                TagEnd::Table => {
                    if let Some(table) = writer.table.take() {
                        writer.render_table(table);
                    }
                    writer.finish_block();
                }
                _ => {}
            },
            // Collected at the top of the loop.
            Event::Text(_) => {}
            Event::Code(code) => {
                let style = writer
                    .style
                    .fg(theme::palette().secondary)
                    .patch(theme::raised());
                writer.push_linkified(&code, style);
            }
            Event::SoftBreak | Event::HardBreak => writer.finish_line(),
            Event::Rule => {
                writer.finish_block();
                writer.lines.push(LogicalLine {
                    line: Line::from(Span::styled(
                        "────────────────────",
                        Style::default().fg(theme::palette().muted),
                    )),
                    continuation_indent: 0,
                    links: Vec::new(),
                });
            }
            // Rich mode intentionally does not interpret or display raw HTML.
            // The raw transcript mode still exposes it as sanitized source
            // when needed.
            Event::Html(_) | Event::InlineHtml(_) => {}
            Event::FootnoteReference(reference) => {
                writer.push_text(&format!("[{reference}]"), writer.style);
            }
            Event::TaskListMarker(checked) => {
                writer.push_text(if checked { "[x] " } else { "[ ] " }, writer.style);
            }
        }
    }
    writer.flush_text();
    writer.finish()
}

pub(super) fn raw_lines(source: &str, style: Style) -> Vec<LogicalLine> {
    source
        .split('\n')
        .map(|line| LogicalLine {
            line: Line::from(Span::styled(line.to_owned(), style)),
            continuation_indent: display_width(
                &line[..line.len().saturating_sub(line.trim_start().len())],
            ),
            links: Vec::new(),
        })
        .collect()
}

/// Wrap a styled line to `width` terminal cells, keeping each span's style.
///
/// Wrapping is grapheme-aware and breaks on word boundaries, cutting a word
/// only when it cannot fit a row on its own. Continuation rows are indented by
/// `continuation_indent` cells; pass 0 for no indent. The returned rows are
/// exactly the rows a renderer draws, so a caller can count them for a scroll
/// clamp or a scrollbar.
pub fn wrap_styled_line(
    line: Line<'static>,
    width: usize,
    continuation_indent: usize,
) -> Vec<Line<'static>> {
    wrap_line(line, width, continuation_indent, None)
}

/// For each wrapped row, the index of the source span drawn in each cell, or
/// `None` for a continuation indent.
type CellSources = Vec<Vec<Option<usize>>>;

/// Wrap `line` exactly as [`wrap_styled_line`] does, and also report which of
/// its spans each cell of each row came from.
pub(super) fn wrap_styled_line_with_sources(
    line: Line<'static>,
    width: usize,
    continuation_indent: usize,
) -> (Vec<Line<'static>>, CellSources) {
    let mut sources = Vec::new();
    let rows = wrap_line(line, width, continuation_indent, Some(&mut sources));
    (rows, sources)
}

fn wrap_line(
    line: Line<'static>,
    width: usize,
    continuation_indent: usize,
    mut sources: Option<&mut CellSources>,
) -> Vec<Line<'static>> {
    let span_count = line.spans.len();
    let mut rows = Vec::new();
    wrap_graphemes(&line, width, continuation_indent, |buffer, row| {
        if let Some(sources) = sources.as_mut() {
            // Style indexes past the line's spans belong to the indent.
            sources.push(
                row.iter()
                    .flat_map(|grapheme| {
                        let source = (grapheme.style < span_count).then_some(grapheme.style);
                        std::iter::repeat_n(source, usize::from(grapheme.width))
                    })
                    .collect(),
            );
        }
        rows.push(buffer.line(row));
        ControlFlow::Continue(())
    });
    rows
}

/// Wrap `line` exactly as [`wrap_styled_line`] does, handing each row to
/// `emit` as soon as it is complete. Wrapping stops when `emit` breaks, so a
/// caller that needs only the first rows of a long line never segments the
/// rest of it.
pub(crate) fn wrap_styled_line_until(
    line: Line<'static>,
    width: usize,
    continuation_indent: usize,
    mut emit: impl FnMut(Line<'static>) -> ControlFlow<()>,
) {
    wrap_graphemes(&line, width, continuation_indent, |buffer, row| {
        emit(buffer.line(row))
    });
}

/// One grapheme of a line being wrapped, kept as a byte range into a shared
/// buffer so wrapping a long transcript does not allocate per grapheme.
#[derive(Debug, Clone, Copy)]
struct Grapheme {
    start: usize,
    end: usize,
    /// Index into the wrapper's style table.
    style: usize,
    width: u8,
    whitespace: bool,
}

/// The text and styles of a logical line, flattened for wrapping.
struct StyledBuffer {
    text: String,
    styles: Vec<Style>,
    /// Each span's byte range in `text`. A span's index is its style's.
    spans: Vec<std::ops::Range<usize>>,
    /// The single trailing space that continuation indents point at.
    space: usize,
}

impl StyledBuffer {
    fn new(line: &Line<'static>) -> Self {
        let capacity: usize = line.spans.iter().map(|span| span.content.len()).sum();
        let mut text = String::with_capacity(capacity + 1);
        let mut styles = Vec::with_capacity(line.spans.len() + 1);
        let mut spans = Vec::with_capacity(line.spans.len());
        for span in &line.spans {
            styles.push(line.style.patch(span.style));
            let base = text.len();
            text.push_str(span.content.as_ref());
            spans.push(base..text.len());
        }
        // Continuation indents reuse one space rather than allocating their own.
        let space = text.len();
        text.push(' ');
        styles.push(
            line.spans
                .first()
                .map(|span| span.style)
                .unwrap_or_default(),
        );
        Self {
            text,
            styles,
            spans,
            space,
        }
    }

    /// The line's graphemes in order. Segmentation is the costly part of
    /// wrapping, so it goes only as far as the caller reads.
    fn graphemes(&self) -> impl Iterator<Item = Grapheme> + '_ {
        self.spans
            .iter()
            .enumerate()
            .flat_map(move |(style, range)| {
                self.text[range.clone()]
                    .grapheme_indices(true)
                    .map(move |(offset, grapheme)| {
                        let start = range.start + offset;
                        Grapheme {
                            start,
                            end: start + grapheme.len(),
                            style,
                            // Graphemes render as at most two columns.
                            width: display_width(grapheme).min(u8::MAX as usize) as u8,
                            whitespace: grapheme.chars().all(char::is_whitespace),
                        }
                    })
            })
    }

    fn indent(&self) -> Grapheme {
        Grapheme {
            start: self.space,
            end: self.space + 1,
            style: self.styles.len() - 1,
            width: 1,
            whitespace: true,
        }
    }

    /// Join `row` into spans, merging runs that share a style.
    fn line(&self, row: &[Grapheme]) -> Line<'static> {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut style: Option<usize> = None;
        let mut text = String::new();
        for grapheme in row {
            if style != Some(grapheme.style) {
                if let Some(style) = style {
                    spans.push(Span::styled(std::mem::take(&mut text), self.styles[style]));
                }
                style = Some(grapheme.style);
            }
            text.push_str(&self.text[grapheme.start..grapheme.end]);
        }
        if let Some(style) = style {
            spans.push(Span::styled(text, self.styles[style]));
        }
        Line::from(spans)
    }
}

/// The row being filled while a line wraps, and how many rows it has handed
/// on so far.
struct RowFill {
    width: usize,
    continuation_indent: usize,
    indent: Grapheme,
    current: Vec<Grapheme>,
    current_width: usize,
    emitted: usize,
}

impl RowFill {
    /// Hands the current row on and leaves it empty.
    fn finish_row(
        &mut self,
        buffer: &StyledBuffer,
        emit: &mut impl FnMut(&StyledBuffer, &[Grapheme]) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        self.emitted += 1;
        let flow = emit(buffer, &self.current);
        self.current.clear();
        flow
    }

    fn start_continuation(&mut self) {
        self.current.clear();
        self.current.resize(self.continuation_indent, self.indent);
        self.current_width = self.continuation_indent;
    }

    /// Places one run of whitespace or non-whitespace graphemes, breaking on
    /// word boundaries and cutting a word only when it cannot fit a row on
    /// its own.
    fn place(
        &mut self,
        token: &[Grapheme],
        buffer: &StyledBuffer,
        emit: &mut impl FnMut(&StyledBuffer, &[Grapheme]) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        let token_width: usize = token
            .iter()
            .map(|grapheme| usize::from(grapheme.width))
            .sum();
        let is_whitespace = token.first().is_some_and(|grapheme| grapheme.whitespace);
        if self.current_width + token_width <= self.width {
            self.current.extend_from_slice(token);
            self.current_width += token_width;
        } else if is_whitespace {
            trim_trailing_whitespace(&mut self.current);
            if !self.current.is_empty() {
                self.finish_row(buffer, emit)?;
            }
            self.start_continuation();
        } else if token_width + self.continuation_indent <= self.width {
            if self.current.len() > self.continuation_indent {
                trim_trailing_whitespace(&mut self.current);
                self.finish_row(buffer, emit)?;
            }
            self.start_continuation();
            self.current.extend_from_slice(token);
            self.current_width += token_width;
        } else {
            for grapheme in token {
                let grapheme_width = usize::from(grapheme.width);
                if self.current_width + grapheme_width > self.width && !self.current.is_empty() {
                    trim_trailing_whitespace(&mut self.current);
                    self.finish_row(buffer, emit)?;
                    self.start_continuation();
                }
                self.current.push(*grapheme);
                self.current_width += grapheme_width;
            }
        }
        ControlFlow::Continue(())
    }
}

/// Wraps `line`, handing each finished row to `emit` until it breaks.
fn wrap_graphemes(
    line: &Line<'static>,
    width: usize,
    continuation_indent: usize,
    mut emit: impl FnMut(&StyledBuffer, &[Grapheme]) -> ControlFlow<()>,
) {
    let width = width.max(1);
    let continuation_indent = continuation_indent.min(width.saturating_sub(1));
    let buffer = StyledBuffer::new(line);
    let mut fill = RowFill {
        width,
        continuation_indent,
        indent: buffer.indent(),
        current: Vec::new(),
        current_width: 0,
        emitted: 0,
    };
    // Rows are filled a run of whitespace or non-whitespace graphemes at a
    // time, each run as soon as the next one begins.
    let mut token: Vec<Grapheme> = Vec::new();
    for grapheme in buffer.graphemes() {
        if token
            .first()
            .is_some_and(|first| first.whitespace != grapheme.whitespace)
        {
            if fill.place(&token, &buffer, &mut emit).is_break() {
                return;
            }
            token.clear();
        }
        token.push(grapheme);
    }
    if !token.is_empty() && fill.place(&token, &buffer, &mut emit).is_break() {
        return;
    }
    trim_trailing_whitespace(&mut fill.current);
    if !fill.current.is_empty() || fill.emitted == 0 {
        // The last row; there is nothing left for a break to stop.
        let _ = fill.finish_row(&buffer, &mut emit);
    }
}

fn trim_trailing_whitespace(graphemes: &mut Vec<Grapheme>) {
    while graphemes.last().is_some_and(|grapheme| grapheme.whitespace) {
        graphemes.pop();
    }
}

pub(super) fn display_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

fn trim_spans_before_ellipsis(spans: &mut Vec<Span<'static>>, preserved_spans: usize) {
    while spans.len() > preserved_spans {
        let last = spans.last_mut().expect("nonempty ellipsis span tail");
        let trimmed = last.content.trim_end_matches(trim_before_ellipsis);
        if trimmed.is_empty() {
            spans.pop();
        } else {
            last.content = trimmed.to_owned().into();
            break;
        }
    }
}

pub(super) fn append_trimmed_ellipsis(line: &mut Line<'static>, preserved_spans: usize) {
    let style = line
        .spans
        .last()
        .map_or(Style::default(), |span| span.style);
    trim_spans_before_ellipsis(&mut line.spans, preserved_spans);
    line.spans
        .push(Span::styled(crate::theme::glyphs().ellipsis, style));
}

/// Truncate a styled line to `width` terminal cells, keeping each span's style and
/// marking the cut with `…` in the style of the span it landed in.
pub fn truncate_line_to_width(line: Line<'static>, width: usize) -> Line<'static> {
    let total = line
        .spans
        .iter()
        .map(|span| display_width(&span.content))
        .sum::<usize>();
    if total <= width {
        return line;
    }
    let budget = width.saturating_sub(1);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for span in line.spans {
        if used >= budget {
            if width > 0 {
                trim_spans_before_ellipsis(&mut spans, 0);
                spans.push(Span::styled(crate::theme::glyphs().ellipsis, span.style));
            }
            return Line::from(spans);
        }
        let count = display_width(&span.content);
        if used + count <= budget {
            used += count;
            spans.push(span);
        } else {
            let kept = span
                .content
                .graphemes(true)
                .take_while(|grapheme| {
                    let next = used + display_width(grapheme);
                    if next > budget {
                        return false;
                    }
                    used = next;
                    true
                })
                .collect::<String>();
            let style = span.style;
            if !kept.is_empty() {
                spans.push(Span::styled(kept, style));
            }
            if width > 0 {
                trim_spans_before_ellipsis(&mut spans, 0);
                spans.push(Span::styled(crate::theme::glyphs().ellipsis, style));
            }
            return Line::from(spans);
        }
    }
    Line::from(spans)
}

pub(super) fn truncate_to_width(text: &str, width: usize) -> String {
    truncate_line_to_width(Line::raw(text.to_owned()), width)
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn sanitizer_removes_terminal_controls_and_normalizes_carriage_returns() {
        assert_eq!(
            sanitize_terminal_text("safe\x1b[31mred\x1b[0m\rnext\u{7}"),
            "safered\nnext"
        );
    }

    // Hard-won: 6c06003: OSC window-title payloads leaked into rendered transcripts
    #[test]
    fn sanitizer_consumes_osc_payloads_and_two_byte_escapes() {
        // A build tool setting the window title, terminated by BEL and by ST.
        assert_eq!(sanitize_terminal_text("\x1b]0;make: build\x07done"), "done");
        assert_eq!(
            sanitize_terminal_text("\x1b]2;cargo test\x1b\\done"),
            "done"
        );
        // Charset selection and cursor save/restore.
        assert_eq!(
            sanitize_terminal_text("\x1b(B\x1b)0plain\x1b7saved\x1b8"),
            "plainsaved"
        );
        // An OSC that is never terminated stops at the line break instead of
        // eating the rest of the transcript.
        assert_eq!(
            sanitize_terminal_text("\x1b]0;title\nnext line"),
            "\nnext line"
        );
        // An OSC ended by another escape does not swallow that escape's own
        // sequence, and nesting cannot recurse without bound.
        assert_eq!(sanitize_terminal_text("\x1b]0;title\x1b[31mred"), "red");
        assert_eq!(sanitize_terminal_text(&"\x1b]".repeat(50_000)), "");
    }

    #[test]
    fn grapheme_wrapper_never_splits_joined_or_combining_characters() {
        let wrapped = wrap_styled_line(Line::from("a 👩‍💻 e\u{301} ｶﾞ z"), 4, 0);
        let rendered = text(&wrapped);
        assert!(rendered.iter().any(|line| line.contains("👩‍💻")));
        assert!(rendered.iter().any(|line| line.contains("e\u{301}")));
        assert!(rendered.iter().any(|line| line.contains("ｶﾞ")));
    }
    fn append_rendered_markdown(
        output: &mut String,
        label: &str,
        buffer: &ratatui::buffer::Buffer,
        details: &[String],
    ) {
        use std::fmt::Write as _;

        if !output.is_empty() {
            output.push('\n');
        }
        writeln!(
            output,
            "=== {label} ({}x{}) ===",
            buffer.area.width, buffer.area.height
        )
        .expect("write state label");
        output.push_str(&crate::golden::buffer_lines(buffer).join("\n"));
        output.push('\n');
        for detail in details {
            writeln!(output, "{detail}").expect("write state detail");
        }
    }

    fn draw_markdown_lines(lines: Vec<Line<'static>>, width: u16) -> ratatui::buffer::Buffer {
        let height = u16::try_from(lines.len().max(1)).expect("bounded markdown rows");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                .expect("markdown terminal");
        terminal
            .draw(|frame| {
                frame.render_widget(
                    ratatui::widgets::Paragraph::new(ratatui::text::Text::from(lines)),
                    frame.area(),
                );
            })
            .expect("draw markdown lines");
        terminal.backend().buffer().clone()
    }

    fn markdown_style_details(lines: &[LogicalLine]) -> Vec<String> {
        lines
            .iter()
            .enumerate()
            .map(|(row, logical)| {
                let spans = logical
                    .line
                    .spans
                    .iter()
                    .map(|span| {
                        format!(
                            "{:?}:fg={:?}:mod={:?}",
                            span.content, span.style.fg, span.style.add_modifier
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" | ");
                if spans.is_empty() {
                    format!("row {row} styles: <empty>")
                } else {
                    format!("row {row} styles: {spans}")
                }
            })
            .collect()
    }

    fn rendered_span_styles(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.style.fg))
            .fold(Vec::new(), |mut colors, color| {
                if colors.last() != Some(&color) {
                    colors.push(color);
                }
                colors
            })
            .into_iter()
            .map(|color| format!("{color:?}"))
            .collect::<Vec<_>>()
            .join(" -> ")
    }

    #[test]
    fn golden_transcript_markdown_render() {
        let mut output = String::new();

        let parsed = markdown_lines(
            "# Heading\n\n- **bold** and `code`\n\n```rust\nfn main() {}",
            Style::default(),
            Style::default().fg(theme::palette().success),
            40,
        );
        let details = markdown_style_details(&parsed);
        let lines = parsed.into_iter().map(|line| line.line).collect();
        let buffer = draw_markdown_lines(lines, 40);
        append_rendered_markdown(
            &mut output,
            "heading list and incomplete fenced code",
            &buffer,
            &details,
        );

        let parsed = markdown_lines(
            "| Name | Description |\n| --- | --- |\n| alpha | a long explanation |",
            Style::default(),
            Style::default(),
            18,
        );
        let details = markdown_style_details(&parsed);
        let lines = parsed.into_iter().map(|line| line.line).collect();
        let buffer = draw_markdown_lines(lines, 32);
        append_rendered_markdown(
            &mut output,
            "narrow-width table fallback records",
            &buffer,
            &details,
        );

        let parsed = markdown_lines(
            "| Name | Score |\n| :--- | ---: |\n| alpha | 7 |",
            Style::default(),
            Style::default(),
            40,
        );
        let details = markdown_style_details(&parsed);
        let lines = parsed.into_iter().map(|line| line.line).collect();
        let buffer = draw_markdown_lines(lines, 40);
        append_rendered_markdown(
            &mut output,
            "aligned Markdown table and header rule",
            &buffer,
            &details,
        );

        let prefix = "bifrost2 · 2 repositories ";
        let path = "/workspace/project/run-1000/session-9275cf63/scratchpad/replay-with-a-long-stable-name";
        let plain = Line::raw(format!("{prefix}{path}"));
        let styled = Line::from(vec![
            Span::styled(prefix, Style::default().fg(Color::Yellow)),
            Span::styled(path, Style::default().fg(Color::Blue)),
        ]);
        for width in [1, 12, 40, 80] {
            let plain_rows = wrap_styled_line(plain.clone(), width, 0);
            let plain_buffer = draw_markdown_lines(plain_rows.clone(), width as u16);
            append_rendered_markdown(
                &mut output,
                &format!("plain long path wrap at width {width}"),
                &plain_buffer,
                &[format!("rows={}", plain_rows.len())],
            );

            let (styled_rows, sources) = wrap_styled_line_with_sources(styled.clone(), width, 0);
            let source_alignment = styled_rows
                .iter()
                .zip(&sources)
                .all(|(row, source)| row.width() == source.len());
            let details = vec![
                format!(
                    "rows={} source rows={} widths aligned={source_alignment}",
                    styled_rows.len(),
                    sources.len()
                ),
                format!("style runs={}", rendered_span_styles(&styled_rows)),
            ];
            let styled_buffer = draw_markdown_lines(styled_rows, width as u16);
            append_rendered_markdown(
                &mut output,
                &format!("styled long path wrap at width {width}"),
                &styled_buffer,
                &details,
            );
        }

        let plain = truncate_to_width("alpha, beta", 7);
        let styled = Line::from(vec![
            Span::styled("alpha,", Style::default().fg(theme::palette().error)),
            Span::styled(" beta", Style::default().fg(Color::Blue)),
        ]);
        let truncated = truncate_line_to_width(styled, 7);
        let plain_buffer = draw_markdown_lines(vec![Line::raw(plain)], 7);
        append_rendered_markdown(
            &mut output,
            "ellipsis trims cutoff punctuation and whitespace",
            &plain_buffer,
            &[],
        );
        let style = truncated.spans.last().and_then(|span| span.style.fg);
        let truncated_buffer = draw_markdown_lines(vec![truncated], 7);
        append_rendered_markdown(
            &mut output,
            "styled ellipsis preserves its trailing span style",
            &truncated_buffer,
            &[format!("ellipsis fg={style:?}")],
        );

        mj_core::golden::assert_golden(
            env!("CARGO_MANIFEST_DIR"),
            "transcript-markdown-render",
            &output,
        );
    }

    #[test]
    fn truncation_respects_wide_glyphs_and_keeps_combining_sequences_intact() {
        assert_eq!(truncate_to_width("界e\u{301}abc", 4), "界e\u{301}…");
        assert_eq!(truncate_to_width("👩‍💻 abc", 3), "👩‍💻…");
        for width in 0..8 {
            let text = truncate_to_width("界e\u{301}👩‍💻 abc", width);
            assert!(
                display_width(&text) <= width,
                "{text:?} exceeds {width} cells"
            );
        }
    }

    #[test]
    fn microphone_button_uses_text_presentation_and_matches_line_width() {
        let line = voice_button_line(true, false);
        assert_eq!(
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            format!(" {} ", voice_button_glyph())
        );
        assert_eq!(line.width(), 3);
        assert_eq!(display_width(voice_button_glyph()), display_width("🎙︎"));
    }

    /// The ASCII set reaches the composer's border, where the microphone was
    /// the last glyph a console without UTF-8 could not draw.
    // Hard-won: 57f76ac: ASCII mode still rendered the microphone button as Unicode
    #[test]
    fn the_ascii_microphone_button_is_plain_text() {
        theme::with_symbols(mj_core::config::SymbolSet::Ascii, || {
            let line = voice_button_line(true, false);
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            assert!(text.is_ascii(), "{text:?}");
            assert_eq!(
                u16::try_from(display_width(&text)).unwrap(),
                voice_button_area(Rect::new(0, 0, 40, 3))
                    .expect("a chip fits")
                    .width,
                "the chip's width follows the glyph"
            );
        });
    }
}

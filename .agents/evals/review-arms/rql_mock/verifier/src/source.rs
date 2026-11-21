use proc_macro2::Span;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct Site {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Bytes {
    pub start: usize,
    pub end: usize,
}

impl Bytes {
    pub fn contains(self, other: Self) -> bool {
        self.start <= other.start && other.end <= self.end
    }

    pub fn overlaps(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }
}

pub struct Source {
    pub path: String,
    pub text: String,
    lines: Vec<usize>,
    // syn::parse_file strips a BOM and an optional Unix shebang before lexing.
    prefix: usize,
}

#[derive(Clone, Copy)]
pub struct Anchor {
    pub range: Bytes,
    pub exact: bool,
}

impl Source {
    pub fn new(path: String, text: String) -> Self {
        let lines = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(i, _)| i + 1))
            .collect();
        Self {
            path,
            text,
            lines,
            prefix: 0,
        }
    }

    pub fn set_lexical_prefix(&mut self, file: &syn::File) {
        self.prefix = usize::from(self.text.starts_with('\u{feff}')) * '\u{feff}'.len_utf8()
            + file.shebang.as_ref().map_or(0, String::len);
    }

    pub fn bytes(&self, span: Span) -> Bytes {
        let range = span.byte_range();
        Bytes {
            start: self.prefix + range.start,
            end: self.prefix + range.end,
        }
    }

    pub fn slice(&self, span: Bytes) -> &str {
        &self.text[span.start..span.end]
    }

    fn line(&self, byte: usize) -> usize {
        self.lines.partition_point(|&offset| offset <= byte)
    }

    pub fn site(&self, span: Bytes) -> Site {
        Site {
            path: self.path.clone(),
            start_line: self.line(span.start),
            end_line: self.line(span.end.saturating_sub(1).max(span.start)),
        }
    }

    pub fn line_bytes(&self, site: &Site) -> Option<Bytes> {
        if site.path != self.path || site.start_line > site.end_line {
            return None;
        }
        self.lines.get(site.end_line.checked_sub(1)?)?;
        Some(Bytes {
            start: *self.lines.get(site.start_line.checked_sub(1)?)?,
            end: self
                .lines
                .get(site.end_line)
                .copied()
                .unwrap_or(self.text.len()),
        })
    }

    pub fn by_line(&self, span: Bytes) -> Bytes {
        self.line_bytes(&self.site(span))
            .expect("valid source span")
    }

    pub fn anchor(&self, site: &Site, evidence: &Value) -> Option<Anchor> {
        let lines = self.line_bytes(site)?;
        let mut ranges = Vec::new();
        let site_json = serde_json::to_value(site).expect("serialize site");
        self.evidence_ranges(site, &site_json, evidence, &mut ranges);
        ranges.sort_unstable();
        ranges.dedup();
        // Several written snippets on the same line do not uniquely identify
        // the primary anchor.
        if let [range] = ranges.as_slice() {
            return Some(Anchor {
                range: *range,
                exact: true,
            });
        }
        Some(Anchor {
            range: lines,
            exact: false,
        })
    }

    fn evidence_ranges(
        &self,
        site: &Site,
        site_json: &Value,
        evidence: &Value,
        ranges: &mut Vec<Bytes>,
    ) {
        if let Some(parts) = evidence["verbatim"].as_array()
            && evidence["site"] == *site_json
            && let Some(range) = self.written_range(site, parts)
        {
            ranges.push(range);
            return;
        }
        match evidence {
            Value::Object(fields) => {
                for value in fields.values() {
                    self.evidence_ranges(site, site_json, value, ranges);
                }
            }
            Value::Array(parts) => {
                for part in parts {
                    self.evidence_ranges(site, site_json, part, ranges);
                }
            }
            _ => {}
        }
    }

    fn written_range(&self, site: &Site, parts: &[Value]) -> Option<Bytes> {
        if parts.len() != site.end_line - site.start_line + 1 {
            return None;
        }
        let mut whole = None;
        let mut previous_end = None;
        for (index, part) in parts.iter().enumerate() {
            let line = site.start_line - 1 + index;
            let offset = self.lines[line];
            let end_of_line = self.lines.get(line + 1).map_or(self.text.len(), |n| n - 1);
            let start = offset.checked_add(part["start_byte"].as_u64()?.try_into().ok()?)?;
            let end = offset.checked_add(part["end_byte"].as_u64()?.try_into().ok()?)?;
            if end > end_of_line || self.text.get(start..end)? != part["text"].as_str()? {
                return None;
            }
            if let Some(previous_end) = previous_end
                && !self
                    .text
                    .get(previous_end..start)?
                    .chars()
                    .all(char::is_whitespace)
            {
                return None;
            }
            let bytes = whole.get_or_insert(Bytes { start, end });
            bytes.end = end;
            previous_end = Some(end);
        }
        let range = whole?;
        let written = self.slice(range);
        let start = range.start + written.len() - written.trim_start().len();
        let end = range.start + written.trim_end().len();
        (start < end).then_some(Bytes { start, end })
    }
}

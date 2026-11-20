//! Oracle module `pulldown_cmark`. Requests are objects with an `"op"` member; see the
//! Go tests that use this module (`go/crates/pulldowncmark/oracle_test.go`) for the
//! operations it serves.
//!
//! `events` parses `"text"` with the option bits in `"options"` and replies
//! with everything a caller can observe: every event of the offset iterator
//! with its source range, the reference definitions, and whether the plain
//! iterator yields the same events. With `"callback": true` the parser is
//! given a broken link callback, and the reply also lists the calls it got.
//!
//! `digest` parses `"text"` once for each option set in `"options"` and
//! replies with a 32-bit FNV-1a hash of the JSON text `events` would reply
//! with, so that a large generated corpus does not make the recorded file
//! large. A mismatch is investigated by sending the same text to `events`.
//!
//! An event is an array: start, end, the event's name, then its fields. Tags
//! are flattened into the array the same way, and no objects are used, so
//! the text is the same whatever order a JSON library keeps keys in.

use std::ops::Range;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Once;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use pulldown_cmark::{
    Alignment, BrokenLink, CodeBlockKind, CowStr, Event, HeadingLevel, LinkType,
    MetadataBlockKind, Options, Parser, Tag, TagEnd,
};
use serde_json::{Value, json};

pub fn handle(request: &Value) -> Value {
    let _busy = Busy::start();
    match crate::text(request, "op") {
        "events" => events(request),
        "digest" => digest(request),
        _ => crate::unknown(request),
    }
}

/// pulldown-cmark 0.10.3 does not return for some inputs when metadata
/// blocks are enabled (an indented `---` line that would open one makes the
/// first pass start over from offset 0). A request that runs for this long is
/// one of those: the process stops instead of spinning, and the Go test that
/// sent the request fails with the request named.
const REQUEST_LIMIT: Duration = Duration::from_secs(20);

/// Milliseconds since `EPOCH` at which the request in progress started, plus
/// one; zero when no request is in progress.
static STARTED: AtomicU64 = AtomicU64::new(0);
static WATCHDOG: Once = Once::new();

struct Busy;

impl Busy {
    fn start() -> Busy {
        static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        let epoch = *EPOCH.get_or_init(Instant::now);
        WATCHDOG.call_once(|| {
            std::thread::spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_millis(100));
                    let started = STARTED.load(Ordering::SeqCst);
                    if started == 0 {
                        continue;
                    }
                    let running = epoch.elapsed().saturating_sub(Duration::from_millis(started - 1));
                    if running > REQUEST_LIMIT {
                        eprintln!(
                            "oracle pulldown_cmark: a request ran for more than {REQUEST_LIMIT:?}; \
                             pulldown-cmark does not return for it"
                        );
                        std::process::exit(3);
                    }
                }
            });
        });
        STARTED.store(epoch.elapsed().as_millis() as u64 + 1, Ordering::SeqCst);
        Busy
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        STARTED.store(0, Ordering::SeqCst);
    }
}

fn options(bits: &Value) -> Options {
    Options::from_bits_truncate(bits.as_u64().unwrap_or(0) as u32)
}

fn events(request: &Value) -> Value {
    let text = crate::text(request, "text");
    let options = options(&request["options"]);
    if request["callback"].as_bool().unwrap_or(false) {
        parse_with_callback(text, options)
    } else {
        parse(text, options)
    }
}

fn digest(request: &Value) -> Value {
    let text = crate::text(request, "text");
    let empty = Vec::new();
    let sets = request["options"].as_array().unwrap_or(&empty);
    let mut digests = Vec::with_capacity(sets.len());
    for bits in sets {
        let options = options(bits);
        // One option set panicking must not hide what the others do.
        let digest = match catch_unwind(AssertUnwindSafe(|| parse(text, options))) {
            Ok(reply) => format!("{:08x}", fnv1a_32(reply.to_string().as_bytes())),
            Err(payload) => format!("panic: {}", panic_message(payload)),
        };
        digests.push(digest);
    }
    json!({ "d": digests.join(" ") })
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|text| (*text).to_owned()))
        .unwrap_or_else(|| "non-string panic payload".to_owned())
}

/// FNV-1a over 64 bits, folded to 32.
fn fnv1a_32(bytes: &[u8]) -> u32 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    ((hash >> 32) ^ (hash & 0xffff_ffff)) as u32
}

fn parse(text: &str, options: Options) -> Value {
    let mut iter = Parser::new_ext(text, options).into_offset_iter();
    let mut with_offsets = Vec::new();
    let mut offset_events = Vec::new();
    for (event, range) in iter.by_ref() {
        with_offsets.push(event_json(&event, Some(&range)));
        offset_events.push(event);
    }
    let refdefs = refdefs_json(iter.reference_definitions());
    let plain_events: Vec<Event> = Parser::new_ext(text, options).collect();
    json!({
        "events": with_offsets,
        "refdefs": refdefs,
        "same_plain": plain_events == offset_events,
    })
}

fn parse_with_callback(text: &str, options: Options) -> Value {
    let mut calls = Vec::new();
    let mut callback = |link: BrokenLink<'_>| -> Option<(CowStr<'_>, CowStr<'_>)> {
        calls.push(json!([
            link.span.start,
            link.span.end,
            link_type_name(link.link_type),
            link.reference.as_ref(),
        ]));
        // References that start with `x` stay broken, so that both answers
        // of a callback are exercised.
        if link.reference.starts_with('x') || link.reference.starts_with('X') {
            None
        } else {
            let url = format!("/broken/{}", link.reference);
            let title = link_type_name(link.link_type).to_owned();
            Some((url.into(), title.into()))
        }
    };
    let mut iter =
        Parser::new_with_broken_link_callback(text, options, Some(&mut callback)).into_offset_iter();
    let mut with_offsets = Vec::new();
    for (event, range) in iter.by_ref() {
        with_offsets.push(event_json(&event, Some(&range)));
    }
    let refdefs = refdefs_json(iter.reference_definitions());
    drop(iter);
    json!({
        "events": with_offsets,
        "refdefs": refdefs,
        "broken": calls,
    })
}

fn refdefs_json(refdefs: &pulldown_cmark::RefDefs<'_>) -> Value {
    // The map iterates in an order that changes from run to run.
    let mut entries: Vec<_> = refdefs.iter().collect();
    entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    Value::Array(
        entries
            .into_iter()
            .map(|(label, def)| {
                json!([
                    label,
                    def.dest.as_ref(),
                    def.title.as_ref().map(|title| title.as_ref()),
                    def.span.start,
                    def.span.end,
                ])
            })
            .collect(),
    )
}

fn event_json(event: &Event<'_>, range: Option<&Range<usize>>) -> Value {
    let mut out = Vec::new();
    if let Some(range) = range {
        out.push(json!(range.start));
        out.push(json!(range.end));
    }
    match event {
        Event::Start(tag) => {
            out.push(json!("Start"));
            tag_json(tag, &mut out);
        }
        Event::End(tag) => {
            out.push(json!("End"));
            tag_end_json(tag, &mut out);
        }
        Event::Text(text) => {
            out.push(json!("Text"));
            out.push(json!(text.as_ref()));
        }
        Event::Code(text) => {
            out.push(json!("Code"));
            out.push(json!(text.as_ref()));
        }
        Event::Html(text) => {
            out.push(json!("Html"));
            out.push(json!(text.as_ref()));
        }
        Event::InlineHtml(text) => {
            out.push(json!("InlineHtml"));
            out.push(json!(text.as_ref()));
        }
        Event::FootnoteReference(text) => {
            out.push(json!("FootnoteReference"));
            out.push(json!(text.as_ref()));
        }
        Event::SoftBreak => out.push(json!("SoftBreak")),
        Event::HardBreak => out.push(json!("HardBreak")),
        Event::Rule => out.push(json!("Rule")),
        Event::TaskListMarker(checked) => {
            out.push(json!("TaskListMarker"));
            out.push(json!(checked));
        }
    }
    Value::Array(out)
}

fn tag_json(tag: &Tag<'_>, out: &mut Vec<Value>) {
    match tag {
        Tag::Paragraph => out.push(json!("Paragraph")),
        Tag::Heading { level, id, classes, attrs } => {
            out.push(json!("Heading"));
            out.push(json!(heading_level_name(*level)));
            out.push(json!(id.as_ref().map(|id| id.as_ref())));
            out.push(json!(classes.iter().map(|class| class.as_ref()).collect::<Vec<_>>()));
            out.push(Value::Array(
                attrs
                    .iter()
                    .map(|(name, value)| json!([name.as_ref(), value.as_ref().map(|value| value.as_ref())]))
                    .collect(),
            ));
        }
        Tag::BlockQuote => out.push(json!("BlockQuote")),
        Tag::CodeBlock(CodeBlockKind::Indented) => {
            out.push(json!("CodeBlock"));
            out.push(json!("Indented"));
        }
        Tag::CodeBlock(CodeBlockKind::Fenced(info)) => {
            out.push(json!("CodeBlock"));
            out.push(json!("Fenced"));
            out.push(json!(info.as_ref()));
        }
        Tag::HtmlBlock => out.push(json!("HtmlBlock")),
        Tag::List(start) => {
            out.push(json!("List"));
            out.push(json!(start));
        }
        Tag::Item => out.push(json!("Item")),
        Tag::FootnoteDefinition(label) => {
            out.push(json!("FootnoteDefinition"));
            out.push(json!(label.as_ref()));
        }
        Tag::Table(alignments) => {
            out.push(json!("Table"));
            out.push(json!(alignments.iter().map(|alignment| alignment_name(*alignment)).collect::<Vec<_>>()));
        }
        Tag::TableHead => out.push(json!("TableHead")),
        Tag::TableRow => out.push(json!("TableRow")),
        Tag::TableCell => out.push(json!("TableCell")),
        Tag::Emphasis => out.push(json!("Emphasis")),
        Tag::Strong => out.push(json!("Strong")),
        Tag::Strikethrough => out.push(json!("Strikethrough")),
        Tag::Link { link_type, dest_url, title, id } => {
            out.push(json!("Link"));
            out.push(json!(link_type_name(*link_type)));
            out.push(json!(dest_url.as_ref()));
            out.push(json!(title.as_ref()));
            out.push(json!(id.as_ref()));
        }
        Tag::Image { link_type, dest_url, title, id } => {
            out.push(json!("Image"));
            out.push(json!(link_type_name(*link_type)));
            out.push(json!(dest_url.as_ref()));
            out.push(json!(title.as_ref()));
            out.push(json!(id.as_ref()));
        }
        Tag::MetadataBlock(kind) => {
            out.push(json!("MetadataBlock"));
            out.push(json!(metadata_block_kind_name(*kind)));
        }
    }
}

fn tag_end_json(tag: &TagEnd, out: &mut Vec<Value>) {
    match tag {
        TagEnd::Paragraph => out.push(json!("Paragraph")),
        TagEnd::Heading(level) => {
            out.push(json!("Heading"));
            out.push(json!(heading_level_name(*level)));
        }
        TagEnd::BlockQuote => out.push(json!("BlockQuote")),
        TagEnd::CodeBlock => out.push(json!("CodeBlock")),
        TagEnd::HtmlBlock => out.push(json!("HtmlBlock")),
        TagEnd::List(ordered) => {
            out.push(json!("List"));
            out.push(json!(ordered));
        }
        TagEnd::Item => out.push(json!("Item")),
        TagEnd::FootnoteDefinition => out.push(json!("FootnoteDefinition")),
        TagEnd::Table => out.push(json!("Table")),
        TagEnd::TableHead => out.push(json!("TableHead")),
        TagEnd::TableRow => out.push(json!("TableRow")),
        TagEnd::TableCell => out.push(json!("TableCell")),
        TagEnd::Emphasis => out.push(json!("Emphasis")),
        TagEnd::Strong => out.push(json!("Strong")),
        TagEnd::Strikethrough => out.push(json!("Strikethrough")),
        TagEnd::Link => out.push(json!("Link")),
        TagEnd::Image => out.push(json!("Image")),
        TagEnd::MetadataBlock(kind) => {
            out.push(json!("MetadataBlock"));
            out.push(json!(metadata_block_kind_name(*kind)));
        }
    }
}

/// The level as its `Display` impl writes it.
fn heading_level_name(level: HeadingLevel) -> String {
    level.to_string()
}

fn link_type_name(link_type: LinkType) -> &'static str {
    match link_type {
        LinkType::Inline => "Inline",
        LinkType::Reference => "Reference",
        LinkType::ReferenceUnknown => "ReferenceUnknown",
        LinkType::Collapsed => "Collapsed",
        LinkType::CollapsedUnknown => "CollapsedUnknown",
        LinkType::Shortcut => "Shortcut",
        LinkType::ShortcutUnknown => "ShortcutUnknown",
        LinkType::Autolink => "Autolink",
        LinkType::Email => "Email",
    }
}

fn alignment_name(alignment: Alignment) -> &'static str {
    match alignment {
        Alignment::None => "None",
        Alignment::Left => "Left",
        Alignment::Center => "Center",
        Alignment::Right => "Right",
    }
}

fn metadata_block_kind_name(kind: MetadataBlockKind) -> &'static str {
    match kind {
        MetadataBlockKind::YamlStyle => "YamlStyle",
        MetadataBlockKind::PlusesStyle => "PlusesStyle",
    }
}

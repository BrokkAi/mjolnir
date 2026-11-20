//! Oracle module `linkify`. Requests are objects with an `"op"` member; see the
//! Go tests that use this module (`go/crates/linkify/oracle_test.go`) for the
//! operations it serves.
//!
//! `find` configures a `LinkFinder` as the request says and replies with the
//! links and the spans it finds in `text`:
//!
//! ```text
//! {"op": "find", "text": "..", "kinds": ["url", "email"],
//!  "url_must_have_scheme": true, "url_can_be_iri": true,
//!  "email_domain_must_have_dot": true}
//! ```
//!
//! Every member but `text` is optional; a member left out leaves the finder's
//! default in place.

use linkify::{LinkFinder, LinkKind};
use serde_json::{Value, json};

pub fn handle(request: &Value) -> Value {
    match crate::text(request, "op") {
        "find" => find(request),
        _ => crate::unknown(request),
    }
}

fn kind_name(kind: &LinkKind) -> &'static str {
    match kind {
        LinkKind::Url => "url",
        LinkKind::Email => "email",
        _ => "other",
    }
}

fn find(request: &Value) -> Value {
    let text = crate::text(request, "text");
    let mut finder = LinkFinder::new();
    if let Some(kinds) = request.get("kinds").and_then(Value::as_array) {
        let kinds: Vec<LinkKind> = kinds
            .iter()
            .map(|kind| match kind.as_str() {
                Some("email") => LinkKind::Email,
                _ => LinkKind::Url,
            })
            .collect();
        finder.kinds(&kinds);
    }
    if let Some(value) = request.get("url_must_have_scheme").and_then(Value::as_bool) {
        finder.url_must_have_scheme(value);
    }
    if let Some(value) = request.get("url_can_be_iri").and_then(Value::as_bool) {
        finder.url_can_be_iri(value);
    }
    if let Some(value) = request.get("email_domain_must_have_dot").and_then(Value::as_bool) {
        finder.email_domain_must_have_dot(value);
    }

    let links: Vec<Value> = finder
        .links(text)
        .map(|link| json!([link.start(), link.end(), kind_name(link.kind()), link.as_str()]))
        .collect();
    let spans: Vec<Value> = finder
        .spans(text)
        .map(|span| json!([span.start(), span.end(), span.kind().map(kind_name), span.as_str().len()]))
        .collect();
    // What tests/common/mod.rs of the crate prints: links between bars.
    let mut shown = String::new();
    for span in finder.spans(text) {
        if span.kind().is_some() {
            shown.push('|');
            shown.push_str(span.as_str());
            shown.push('|');
        } else {
            shown.push_str(span.as_str());
        }
    }
    json!({"links": links, "spans": spans, "shown": shown})
}

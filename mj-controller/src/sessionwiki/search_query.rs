//! Query syntax shared by live transcript search and its previews.

/// Match SessionWiki's query syntax: whitespace separates required terms,
/// double quotes group a phrase, and doubled quotes escape a literal quote.
/// An unfinished phrase consumes the remainder, so search works while typing.
/// SessionWiki's parser is private; index integration tests protect agreement.
pub(super) fn terms(query: &str) -> Vec<String> {
    let normalized = sessionwiki::util::nfc(query);
    let mut chars = normalized.chars().peekable();
    let mut terms = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    while let Some(character) = chars.next() {
        if character == '"' {
            if quoted && chars.peek() == Some(&'"') {
                chars.next();
                current.push('"');
            } else {
                push_term(&mut terms, &mut current);
                quoted = !quoted;
            }
        } else if !quoted && character.is_whitespace() {
            push_term(&mut terms, &mut current);
        } else {
            current.push(character);
        }
    }
    push_term(&mut terms, &mut current);
    terms
}

fn push_term(terms: &mut Vec<String>, current: &mut String) {
    if !current.trim().is_empty() {
        terms.push(std::mem::take(current));
    } else {
        current.clear();
    }
}

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

pub(super) struct ParsedQuery {
    pub groups: Vec<Vec<QueryAtom>>,
    pub ignored_terms: Vec<String>,
}

pub(super) enum QueryAtom {
    Phrase(String),
    Term { raw: String, prefix: bool },
}

pub(super) fn parse_query(raw: &str) -> ParsedQuery {
    let normalized = sessionwiki::util::nfc(raw);
    let mut chars = normalized.chars().peekable();
    let mut tokens = Vec::new();
    while let Some(c) = chars.next() {
        if c.is_whitespace() {
            continue;
        }
        if c == '|' {
            tokens.push(("|".to_owned(), false));
            continue;
        }
        let quoted = c == '"';
        let mut text = String::new();
        if quoted {
            while let Some(c) = chars.next() {
                if c == '"' {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        text.push('"');
                    } else {
                        break;
                    }
                } else {
                    text.push(c);
                }
            }
        } else {
            text.push(c);
            while chars
                .peek()
                .is_some_and(|c| !c.is_whitespace() && *c != '|' && *c != '"')
            {
                text.push(chars.next().unwrap());
            }
        }
        tokens.push((text, quoted));
    }
    let mut parsed = ParsedQuery {
        groups: Vec::new(),
        ignored_terms: Vec::new(),
    };
    let mut alternative = false;
    for (i, (raw, quoted)) in tokens.iter().enumerate() {
        if !quoted
            && (raw == "OR" || raw == "|")
            && !parsed.groups.is_empty()
            && tokens.get(i + 1).is_some_and(|(s, q)| {
                s.chars().any(char::is_alphanumeric) && (*q || (s != "OR" && s != "|"))
            })
        {
            alternative = true;
            continue;
        }
        if !quoted
            && raw == "OR"
            && !parsed.groups.is_empty()
            && tokens
                .get(i + 1)
                .is_some_and(|(s, _)| !s.chars().any(char::is_alphanumeric))
        {
            continue;
        }
        if !raw.chars().any(char::is_alphanumeric) {
            parsed.ignored_terms.push(raw.clone());
            continue;
        }
        let atom = if *quoted {
            QueryAtom::Phrase(raw.clone())
        } else {
            QueryAtom::Term {
                raw: raw.trim_end_matches('*').to_owned(),
                prefix: raw.ends_with('*'),
            }
        };
        if alternative {
            parsed.groups.last_mut().unwrap().push(atom);
        } else {
            parsed.groups.push(vec![atom]);
        }
        alternative = false;
    }
    parsed
}

pub(super) fn compile_group_fts_match(group: &[QueryAtom]) -> Option<String> {
    let parts: Vec<_> = group
        .iter()
        .map(|atom| {
            let (raw, prefix) = match atom {
                QueryAtom::Phrase(raw) => (raw, false),
                QueryAtom::Term { raw, prefix } => (raw, *prefix),
            };
            format!(
                "\"{}\"{}",
                raw.replace('"', "\"\""),
                if prefix { "*" } else { "" }
            )
        })
        .collect();
    (!parts.is_empty()).then(|| format!("({})", parts.join(" OR ")))
}

pub(super) fn compile_fts_match(groups: &[Vec<QueryAtom>]) -> Option<String> {
    let parts: Vec<_> = groups
        .iter()
        .filter_map(|g| compile_group_fts_match(g))
        .collect();
    (!parts.is_empty()).then(|| parts.join(" AND "))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn porter_precision_and_query_syntax() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE VIRTUAL TABLE msgs USING fts5(text, tokenize='porter unicode61');",
        )
        .unwrap();
        for text in [
            "#1031 P2 diff_scoped inventory retry park quoted phrase 1591 mbx foo baz index.rs 6%/h a25b06ab123 59921234",
            "#10311 Ready bar baz",
            "1591 alone",
            "1589 mbx",
            "quoted intervening phrase",
        ] {
            db.execute("INSERT INTO msgs(text) VALUES (?1)", [text])
                .unwrap();
        }
        for (query, expected) in [
            ("#1031", 1),
            ("P2", 1),
            ("diff-scoped", 1),
            ("inventories", 1),
            ("retries", 1),
            ("parked", 1),
            ("read", 0),
            ("a25b06ab*", 1),
            ("a25b06ab", 0),
            ("5992*", 1),
            ("\"quoted phrase\"", 1),
            ("1591 OR 1589 mbx", 2),
            ("foo|bar baz", 2),
            ("index.rs", 1),
            ("6%/h", 1),
        ] {
            let compiled = compile_fts_match(&parse_query(query).groups).unwrap();
            let n: i64 = db
                .query_row(
                    "SELECT count(*) FROM msgs WHERE msgs MATCH ?1",
                    [&compiled],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, expected, "{query}: {compiled}");
        }
        assert_eq!(parse_query("---").ignored_terms, ["---"]);
        let punctuation = parse_query(r#"foo OR "---" bar"#);
        assert_eq!(punctuation.groups.len(), 2);
        assert_eq!(punctuation.ignored_terms, ["---"]);
        db.execute("DELETE FROM msgs", []).unwrap();
        for (left, right) in [
            ("inventories", "inventory"),
            ("retries", "retry"),
            ("parked", "park"),
        ] {
            for (query, text) in [(left, right), (right, left)] {
                db.execute("DELETE FROM msgs", []).unwrap();
                db.execute("INSERT INTO msgs(text) VALUES (?1)", [text])
                    .unwrap();
                let fts = compile_fts_match(&parse_query(query).groups).unwrap();
                assert_eq!(
                    db.query_row(
                        "SELECT count(*) FROM msgs WHERE msgs MATCH ?1",
                        [fts],
                        |r| r.get::<_, i64>(0)
                    )
                    .unwrap(),
                    1
                );
            }
        }
    }
}

#[cfg(test)]
mod migration_tests {
    use super::super::*;
    #[tokio::test]
    #[allow(
        clippy::await_holding_lock,
        reason = "the test owns SESSIONWIKI_DATA for its whole duration"
    )]
    async fn migration_preserves_rows_and_readers_wait_without_a_transaction() {
        let _held = tags::testing::lock();
        let (_dir, connection) = tags::testing::isolated_index();
        tags::testing::index_row(&connection, "migration", TOOL);
        connection.execute("INSERT INTO messages(session_id, role, text) VALUES ('migration', 'user', 'inventory P2')", []).unwrap();
        connection.execute("INSERT INTO archive(session_id,path,mtime,size,tool,transcript,touched,archived_at) VALUES ('saved','saved',0,0,'mjolnir','[]','[]','2026-11-24')", []).unwrap();
        let mid: i64 = connection
            .query_row("SELECT id FROM messages", [], |r| r.get(0))
            .unwrap();
        connection
            .execute(
                "INSERT INTO msgs(rowid,text) VALUES (?1,'inventory P2')",
                [mid],
            )
            .unwrap();
        sessionwiki::index::set_tokenizer_spec(&connection, "trigram").unwrap();
        assert!(
            history::wait_for_tokenizer_migration(std::time::Duration::ZERO)
                .await
                .unwrap_err()
                .to_string()
                .contains("retry shortly")
        );
        let path = connection.path().unwrap().to_owned();
        let writer = std::thread::spawn(move || {
            let db = rusqlite::Connection::open(path).unwrap();
            migrate_tokenizer(&db).unwrap();
        });
        history::wait_for_tokenizer_migration(std::time::Duration::from_secs(5))
            .await
            .unwrap();
        writer.join().unwrap();
        migrate_tokenizer(&connection).unwrap();
        assert_eq!(
            sessionwiki::index::tokenizer_spec(&connection).unwrap(),
            TARGET_TOKENIZER
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM messages", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM files", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        let preview =
            fts_hit_transcript(&connection, "migration", "inventories P2", 0, 100).unwrap();
        assert_eq!(preview.blocks[0].hits, [(0, 9), (10, 12)]);
        sessionwiki::index::set_tokenizer_spec(&connection, "unicode61").unwrap();
        migrate_tokenizer(&connection).unwrap();
        assert_eq!(
            sessionwiki::index::tokenizer_spec(&connection).unwrap(),
            "unicode61"
        );
    }
}

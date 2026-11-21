use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::*;

fn form(step: &str, options: &[&str], seed: Value) -> Value {
    let mut parts = vec![json!(step)];
    parts.extend(options.iter().map(|option| json!(option)));
    parts.push(seed);
    json!(parts)
}

fn procedure() -> Value {
    json!(["procedure-of", ["callable", ":name", "\"seed\""]])
}

fn query(step: &str, options: &[&str], seed: Value) -> Query {
    Query::parse(Step::parse(step).unwrap(), &form(step, options, seed)).unwrap()
}

fn file(query: &Query, text: &str) -> Parsed {
    Parsed::new(query, Source::new("fixture.rs".into(), text.into())).unwrap()
}

fn fixture_seed_names(file: &Parsed) -> BTreeSet<&str> {
    file.seeds
        .iter()
        .map(|&id| file.syntax.procedures[id].name.as_str())
        .collect()
}

fn by_kind(file: &Parsed, query: &Query) -> BTreeMap<&'static str, Vec<String>> {
    let mut kinds = BTreeMap::<_, Vec<_>>::new();
    for Selected {
        construct: candidate,
        ..
    } in file.candidates(query, &fixture_seed_names(file))
    {
        kinds
            .entry(candidate.kind.name())
            .or_default()
            .push(file.source.slice(candidate.span).to_owned());
    }
    kinds
}

fn value_site(start: usize, end: usize) -> Value {
    json!({"path": "fixture.rs", "start_line": start, "end_line": end})
}

fn written(line: usize, byte: usize, text: &str) -> Value {
    json!({"site": value_site(line, line),
           "verbatim": [{"start_byte": byte, "end_byte": byte + text.len(), "text": text}]})
}

fn guarded(event: Value, guards: Value, calls: Value, pivot: Value) -> Value {
    json!({"kind": "guarded_path", "sites": [event["site"]], "fields":
        {"event": event, "guards": guards, "calls": calls, "pivot": pivot}})
}

fn row_checks(query: &Query, text: &str, row: Value) -> Output {
    let file = file(query, text);
    let answer = json!({"rows": [row]});
    let rows = rows::answer_rows(&answer, query.step);
    let row = &rows[0];
    let mut out = Output::new();
    rows::check(
        query,
        row,
        row.site.as_ref().unwrap(),
        Some(&file),
        !file.seeds.is_empty(),
        &mut out,
    );
    out
}

fn accounting(query: &Query, text: &str, answer: Value) -> Output {
    let file = file(query, text);
    let rows = rows::answer_rows(&answer, query.step);
    let mut out = Output::new();
    let selected = file.candidates(query, &fixture_seed_names(&file));
    rows::account(
        query,
        &rows,
        &answer,
        selected.into_iter().map(|construct| (&file, construct)),
        &mut out,
    );
    out
}

fn codes(out: &Output) -> Vec<&str> {
    out.errors.iter().map(|error| error.code).collect()
}

#[test]
fn resolves_free_inherent_trait_and_default_bodies() {
    let query = query("contract-cases", &[], procedure());
    let source = file(
        &query,
        r#"
        fn seed() -> i32 { 1 }
        impl Thing { fn seed(&self) -> i32 { 2 } }
        trait Trait { fn seed(&self) -> i32 { 3 } fn declaration(); }
        impl Trait for Thing { fn seed(&self) -> i32 { 4 } }
        fn other() -> i32 { 5 }
    "#,
    );
    assert_eq!(by_kind(&source, &query)["return"], ["1", "2", "3", "4"]);
}

#[test]
fn contract_cases_keep_errors_exits_panics_and_parameter_calls_distinct() {
    let query = query("contract-cases", &[], procedure());
    let source = file(
        &query,
        r#"
        fn seed(f: fn(), hooks: Hooks) -> Result<()> {
            run()?;
            if first { return Err("bad"); }
            f(); (hooks.callback)();
            panic!("panic"); assert_eq!(a, b);
            value.unwrap(); result.expect("failure");
            let nested = || { panic!("deferred"); };
            fn nested_item() { panic!("nested fn"); }
            Err("tail")
        }
    "#,
    );
    let found = by_kind(&source, &query);
    assert_eq!(found["error"], ["run()?", "Err(\"bad\")", "Err(\"tail\")"]);
    assert_eq!(found["return"], ["return Err(\"bad\")", "Err(\"tail\")"]);
    assert_eq!(found["callback"], ["f()", "(hooks.callback)()"]);
    assert_eq!(
        found["panic"],
        [
            "panic!(\"panic\")",
            "assert_eq!(a, b)",
            "value.unwrap()",
            "result.expect(\"failure\")"
        ]
    );
}

#[test]
fn shadowed_formal_does_not_produce_a_callback_candidate() {
    let query = query("contract-cases", &[":kind", "\"callback\""], procedure());
    let source = file(&query, "fn seed(f: fn()) { let f = arbitrary(); f(); }");
    assert!(by_kind(&source, &query).is_empty());
    assert_eq!(source.syntax.uncertain_callbacks.len(), 1);
}

#[test]
fn value_ports_include_receivers_and_distinguish_places_from_rhs() {
    let text = r#"
        impl Thing {
            fn seed(&mut self, index: usize, pair: Pair) -> usize {
                self.data[index] += pair.x;
                self.data[0] = call(index);
                self.apply(index, pair);
                return index;
            }
        }
    "#;
    for (port, expected) in [
        ("return", vec!["index"]),
        ("input", vec!["&mut self", "index: usize", "pair: Pair"]),
        ("write", vec!["self.data[index]", "self.data[0]"]),
        ("argument", vec!["index", "self", "index", "pair"]),
    ] {
        let selector = format!("\"{port}\"");
        let query = query("value-transfers", &[":port", &selector], procedure());
        let file = file(&query, text);
        assert_eq!(by_kind(&file, &query)[port], expected);
    }
}

#[test]
fn guarded_events_include_propagation_and_exclude_non_raise_macros() {
    let query = query("guarded-paths", &[], procedure());
    let source = file(
        &query,
        r#"
        fn seed() -> Result<()> {
            state = call()?;
            ordinary_macro!();
            if fail { unreachable!(); }
            Ok(())
        }
    "#,
    );
    let found = by_kind(&source, &query);
    assert_eq!(found["call"], ["call()"]);
    assert_eq!(found["write"], ["state"]);
    assert_eq!(found["raise"], ["unreachable!()"]);
    assert_eq!(found["return"], ["call()?", "Ok(())"]);
}

#[test]
fn incoming_and_call_seeds_use_the_written_invocation() {
    let text = r#"
        fn seed() {}
        fn caller() { module::seed(); other.method(); value.seed(); }
        fn another() { let f = || seed(); }
    "#;
    for query in [
        query(
            "guarded-paths",
            &[":direction", "\"incoming\""],
            procedure(),
        ),
        query("guarded-paths", &[], json!(["call", ":name", "\"seed\""])),
    ] {
        let source = file(&query, text);
        assert_eq!(
            by_kind(&source, &query)["call"],
            ["module::seed()", "value.seed()", "seed()"]
        );
    }
}

#[test]
fn procedure_of_call_selects_the_callers_body_only() {
    let query = query(
        "value-transfers",
        &[":port", "\"write\""],
        json!(["procedure-of", ["call", ":name", "\"seed\""]]),
    );
    let text = "fn seed() { x = 0; } fn caller() { seed(); y = 1; } fn other() { caller(); }";
    let source = file(&query, text);
    assert_eq!(by_kind(&source, &query)["write"], ["y"]);
    let incoming = self::query(
        "guarded-paths",
        &[":direction", "\"incoming\""],
        json!(["procedure-of", ["call", ":name", "\"seed\""]]),
    );
    assert_eq!(
        by_kind(&file(&incoming, text), &incoming)["call"],
        ["caller()"]
    );
}

#[test]
fn a_whole_body_anchor_cannot_account_for_a_nested_candidate() {
    let query = query("guarded-paths", &[":event", "\"call\""], procedure());
    let text = "fn seed() {\n    action();\n}\n";
    let event = json!({"kind": "call", "site": value_site(1, 3)});
    let row = guarded(event, json!([]), json!([]), Value::Null);
    let out = accounting(&query, text, json!({"rows": [row]}));
    assert_eq!(codes(&out), ["unaccounted-candidate"]);
    let event = json!({"kind": "call", "site": value_site(1, 1)});
    let row = guarded(event, json!([]), json!([]), Value::Null);
    let one_line = accounting(&query, "fn seed() { action(); }", json!({"rows": [row]}));
    assert!(one_line.errors.is_empty());
    assert_eq!(one_line.unchecked.len(), 1);
}

#[test]
fn role_byte_bounds_use_the_argument_anchor_and_distinguish_same_line_operands() {
    let query = query("value-transfers", &[":port", "\"argument\""], procedure());
    let text = "fn seed() {\n    first(blå + 1); second(other);\n}\n";
    let row = json!({"kind": "value_transfer", "fields": {
        "selected_port": {"operand": written(2, 10, "blå")},
        "sink": {"port": "argument", "site": value_site(2, 2)}
    }});
    let out = accounting(&query, text, json!({"rows": [row]}));
    assert_eq!(codes(&out), ["unaccounted-candidate"]);
    assert_eq!(out.errors[0].candidate.as_deref(), Some("c2"));
}

#[test]
fn narrow_and_wide_try_sites_cover_the_propagation_token() {
    let query = query(
        "contract-cases",
        &[":depth", "0", ":kind", "\"error\""],
        procedure(),
    );
    let text = "fn seed() -> Result<(), Error> {\n    first()\n        .transpose()?;\n    second()?;\n    third().await?;\n    Ok(())\n}\n";
    let first_operand = json!({"site": value_site(2, 3), "verbatim": [
        {"start_byte": 4, "end_byte": 11, "text": "first()"},
        {"start_byte": 8, "end_byte": 20, "text": ".transpose()"}
    ]});
    for first_site in [value_site(3, 3), value_site(2, 3)] {
        let rows = [
            (first_site, first_operand.clone(), written(3, 20, "?")),
            (
                value_site(4, 4),
                written(4, 4, "second()"),
                written(4, 12, "?"),
            ),
            (
                value_site(5, 5),
                written(5, 4, "third().await"),
                written(5, 17, "?"),
            ),
        ]
        .into_iter()
        .map(|(site, operand, token)| {
            json!({"kind": "contract_case", "fields": {
                "kind": "error", "site": site,
                "outcome": {"operand": operand, "propagation_site": token}
            }})
        })
        .collect::<Vec<_>>();
        let out = accounting(&query, text, json!({"rows": rows}));
        assert_eq!(out.candidates.len(), 3);
        assert!(out.errors.is_empty() && out.unchecked.is_empty());
    }
}

#[test]
fn call_site_on_its_last_path_segment_covers_the_call() {
    let query = query("guarded-paths", &[":event", "\"call\""], procedure());
    let text = "fn seed() {\n    scope::\n        action(42);\n}\n";
    let row = guarded(
        json!({"kind": "call", "site": value_site(3, 3)}),
        json!([]),
        json!([]),
        Value::Null,
    );
    assert!(
        accounting(&query, text, json!({"rows": [row]}))
            .errors
            .is_empty()
    );
}

#[test]
fn selected_port_and_pivot_account_even_when_primary_is_elsewhere() {
    let text = "fn seed() -> u8 {\n    7\n}\nfn caller() {\n    seed();\n    done();\n}\n";
    let value = query(
        "value-transfers",
        &[":direction", "\"downstream\""],
        procedure(),
    );
    let row = json!({"kind": "value_transfer", "fields": {
        "source": {"port": "return", "site": value_site(2, 2)},
        "sink": {"port": "argument", "site": value_site(6, 6)}
    }});
    assert!(
        accounting(&value, text, json!({"rows": [row]}))
            .errors
            .is_empty()
    );
    let incoming = query(
        "guarded-paths",
        &[":direction", "\"incoming\""],
        procedure(),
    );
    let event = json!({"kind": "call", "site": value_site(6, 6)});
    let row = guarded(event, json!([]), json!([]), value_site(5, 5));
    assert!(
        accounting(&incoming, text, json!({"rows": [row.clone()]}))
            .errors
            .is_empty()
    );
    let mut missing = row;
    missing["fields"]["pivot"] = Value::Null;
    assert_eq!(
        codes(&accounting(&incoming, text, json!({"rows": [missing]}))),
        ["unaccounted-candidate"]
    );
}

// Hard-won: 12abda8: seed-body ? exits excluded as other
#[test]
fn contract_error_propagation_cannot_be_excluded_as_outside_depth_zero() {
    let query = query(
        "contract-cases",
        &[":depth", "0", ":kind", "\"error\""],
        procedure(),
    );
    let text = "fn seed() -> Result<(), Error> {\n    first()\n        .transpose()?;\n    second()?;\n    third().await?;\n    Err(failure)\n}\n";
    let row = json!({"kind": "contract_case", "fields": {
        "kind": "error", "site": value_site(6, 6)
    }});
    let excluded: Vec<_> = [(2, 3), (4, 4), (5, 5)]
        .into_iter()
        .map(|(start, end)| {
            json!({"candidate_site": value_site(start, end), "reason": "other",
               "detail": "callees outside depth 0"})
        })
        .collect();
    let out = accounting(&query, text, json!({"rows": [row], "excluded": excluded}));
    assert_eq!(codes(&out), ["unaccounted-candidate"; 3]);
    assert_eq!(
        out.errors
            .iter()
            .map(|issue| issue.site.start_line)
            .collect::<Vec<_>>(),
        [2, 4, 5]
    );
}

#[test]
fn unreachable_needs_a_condition_enclosing_the_candidate_in_a_branch() {
    let query = query("guarded-paths", &[":event", "\"call\""], procedure());
    let text = "fn seed() {\n    if flag {\n        action();\n    }\n    if unrelated {}\n}\n";
    for (guard, passes) in [
        (None, false),
        (Some(value_site(5, 5)), false),
        (Some(value_site(3, 3)), false),
        (Some(value_site(2, 2)), true),
    ] {
        let mut exclusion =
            json!({"candidate_site": value_site(3, 3), "reason": "unreachable", "detail": ""});
        if let Some(site) = guard {
            exclusion["guard_site"] = site;
        }
        let out = accounting(&query, text, json!({"rows": [], "excluded": [exclusion]}));
        if passes {
            assert!(out.errors.is_empty());
            assert_eq!(out.unchecked.len(), 1);
            assert_eq!(out.unchecked[0].what, "c1");
        } else {
            assert_eq!(codes(&out), ["false-exclusion"]);
        }
    }
}

#[test]
fn unreachable_guard_can_cite_match_arm_while_or_let_else() {
    let query = query("guarded-paths", &[":event", "\"call\""], procedure());
    for text in [
        "fn seed() {\n    match mode {\n        A => {\n            action();\n        }\n    }\n}\n",
        "fn seed() {\n    {\n        while flag {\n            action();\n        }\n    }\n}\n",
        "fn seed() {\n    {\n        let Some(value) = optional else {\n            action();\n            return;\n        };\n    }\n}\n",
    ] {
        let exclusion = json!({"candidate_site": value_site(4, 4), "reason": "unreachable",
                               "detail": "", "guard_site": value_site(3, 3)});
        let out = accounting(&query, text, json!({"rows": [], "excluded": [exclusion]}));
        assert!(out.errors.is_empty(), "{text}");
        assert_eq!(out.unchecked.len(), 1);
    }
}

#[test]
fn macro_not_call_requires_a_macro_invocation() {
    let call = query("guarded-paths", &[":event", "\"call\""], procedure());
    let raise = query("guarded-paths", &[":event", "\"raise\""], procedure());
    let answer = json!({"rows": [], "excluded": [
        {"candidate_site": value_site(2, 2), "reason": "macro-not-call", "detail": ""}
    ]});
    let out = accounting(&call, "fn seed() {\n    invoke();\n}\n", answer.clone());
    assert_eq!(codes(&out), ["false-exclusion"]);
    let out = accounting(&raise, "fn seed() {\n    panic!();\n}\n", answer);
    assert!(out.errors.is_empty());
    assert_eq!(out.candidates.len(), 1);
}

#[test]
fn argument_and_call_candidates_skip_constructors_but_keep_nested_calls() {
    let text = "fn seed() {\n    Ok(text);\n    Some(x);\n    Box::new(x);\n    Foo::Bar(fetch(y));\n    Widget(w);\n    return Err(e);\n}\n";
    let arguments = query("value-transfers", &[":port", "\"argument\""], procedure());
    assert_eq!(
        by_kind(&file(&arguments, text), &arguments)["argument"],
        ["x", "y"]
    );
    let calls = query("guarded-paths", &[":event", "\"call\""], procedure());
    assert_eq!(
        by_kind(&file(&calls, text), &calls)["call"],
        ["Box::new(x)", "fetch(y)"]
    );
    let errors = query("contract-cases", &[":kind", "\"error\""], procedure());
    assert_eq!(by_kind(&file(&errors, text), &errors)["error"], ["Err(e)"]);
}

#[test]
fn explicit_aliases_and_names_that_cross_constructor_conventions_stay_unchecked() {
    let query = query("guarded-paths", &[":event", "\"call\""], procedure());
    let text = "use Option::Some as wrap;\nfn Upper(x: u8) {}\nstruct lower(u8);\nfn seed() {\n    wrap(1);\n    Upper(2);\n    lower(3);\n}\n";
    let source = file(&query, text);
    assert!(by_kind(&source, &query).is_empty());
    assert_eq!(source.syntax.uncertain_constructors.len(), 3);
}

// Hard-won: 73f3134: exclusions passed with reasons false by construction
#[test]
fn removed_selection_reasons_cannot_exclude_every_candidate() {
    let text = "fn seed() {\n    first();\n    second();\n}\n";
    for (query, reason) in [
        (
            query("guarded-paths", &[":event", "\"call\""], procedure()),
            "kind-not-selected",
        ),
        (
            query("value-transfers", &[":port", "\"argument\""], procedure()),
            "port-not-selected",
        ),
    ] {
        let text = if reason == "port-not-selected" {
            "fn seed() {\n    first(1);\n    second(2);\n}\n"
        } else {
            text
        };
        let answer = json!({"rows": [], "excluded": [
            {"candidate_site": value_site(2, 2), "reason": reason, "detail": ""},
            {"candidate_site": value_site(3, 3), "reason": reason, "detail": ""}
        ]});
        let out = accounting(&query, text, answer);
        assert_eq!(
            codes(&out),
            ["unaccounted-candidate", "unaccounted-candidate"]
        );
    }
}

#[test]
fn incoming_at_depth_zero_has_no_candidates() {
    let incoming = query(
        "guarded-paths",
        &[":direction", "\"incoming\"", ":depth", "0"],
        procedure(),
    );
    let text = "fn seed() {}\nfn caller() {\n    seed();\n}\n";
    let out = accounting(&incoming, text, json!({"rows": [], "excluded": []}));
    assert!(out.candidates.is_empty());
    assert!(out.errors.is_empty());
    assert!(out.unchecked.is_empty());
}

#[test]
fn incoming_name_match_can_exclude_an_unrelated_callee() {
    let incoming = query(
        "guarded-paths",
        &[":direction", "\"incoming\""],
        procedure(),
    );
    let text = "fn seed() {}\nfn caller() {\n    seed();\n    external::seed();\n}\n";
    let event = json!({"kind": "call", "site": value_site(3, 3)});
    let row = guarded(event, json!([]), json!([]), value_site(3, 3));
    let answer = json!({"rows": [row], "excluded": [
        {"candidate_site": value_site(4, 4), "reason": "not-the-seed", "detail": "external::seed"}
    ]});
    let out = accounting(&incoming, text, answer);
    assert_eq!(out.candidates.len(), 2);
    assert!(out.errors.is_empty());
    assert!(out.unchecked.is_empty());
}

#[test]
fn exact_seed_candidates_cannot_be_excluded_as_not_the_seed() {
    let query = query("guarded-paths", &[":event", "\"call\""], procedure());
    let event = json!({"kind": "call", "site": value_site(2, 2)});
    let row = guarded(event, json!([]), json!([]), Value::Null);
    for rows in [json!([]), json!([row])] {
        let answer = json!({"rows": rows, "excluded": [
            {"candidate_site": value_site(2, 2), "reason": "not-the-seed", "detail": ""}
        ]});
        let out = accounting(&query, "fn seed() {\n    action();\n}\n", answer);
        assert_eq!(codes(&out), ["false-exclusion"]);
    }
}

#[test]
fn not_the_seed_exclusion_is_unchecked_for_unmodeled_filters() {
    let input = Input {
        root: "nonexistent".into(),
        step: "guarded-paths".into(),
        query: form(
            "guarded-paths",
            &[],
            json!([
                "procedure-of",
                ["where", "\"src/*.rs\"", ["callable", ":name", "\"seed\""]]
            ]),
        ),
        answer: json!({"rows": [], "excluded": [
            {"candidate_site": value_site(2, 2), "reason": "not-the-seed", "detail": ""}
        ]}),
    };
    let out = verify(input).unwrap();
    assert!(out.errors.is_empty());
    assert!(out.unchecked.iter().any(|item| item.what == "excluded[0]"));
}

#[test]
fn duplicate_exclusions_require_a_row_covered_twin_regardless_of_order() {
    let query = query("guarded-paths", &[":event", "\"call\""], procedure());
    let event = json!({"kind": "call", "site": value_site(3, 3)});
    let row = guarded(event, json!([]), json!([]), Value::Null);
    let text = "fn seed() {\n    action();\n    action();\n    action();\n}\n";
    for (twin, expected) in [
        ("c2", vec![]),
        ("c3", vec!["false-exclusion"]),
        ("c4", vec!["false-exclusion"]),
        ("c1", vec!["false-exclusion"]),
        ("", vec!["false-exclusion"]),
    ] {
        let answer = json!({"rows": [row], "excluded": [
            {"candidate_site": value_site(2, 2), "reason": "duplicate", "detail": twin},
            {"candidate_site": value_site(4, 4), "reason": "duplicate", "detail": "c2"}
        ]});
        let out = accounting(&query, text, answer);
        assert_eq!(codes(&out), expected, "claimed twin {twin}");
    }
}

#[test]
fn anchor_checks_calls_and_written_places_and_defers_macro_expansion() {
    let query = query("guarded-paths", &[":depth", "0"], procedure());
    let text = "fn seed() {\n    action();\n    data.x = 0;\n    make_call!();\n}\n";
    for (event, expected) in [
        (
            json!({"kind":"call", "site":value_site(2,2), "resolved_target":"module::action"}),
            vec![],
        ),
        (
            json!({"kind":"call", "site":value_site(2,2), "resolved_target":"other"}),
            vec!["anchor-kind"],
        ),
        (
            json!({"kind":"write", "site":value_site(3,3), "place":"data.x"}),
            vec![],
        ),
        (
            json!({"kind":"write", "site":value_site(3,3), "place":"data.y"}),
            vec!["anchor-kind"],
        ),
        (json!({"kind":"call", "site":value_site(4,4)}), vec![]),
        (
            json!({"kind":"return", "site":value_site(2,2)}),
            vec!["anchor-kind"],
        ),
    ] {
        let line = event["site"]["start_line"].as_u64().unwrap();
        let out = row_checks(
            &query,
            text,
            guarded(event, json!([]), json!([]), Value::Null),
        );
        assert_eq!(codes(&out), expected, "line {line}");
        if line == 4 {
            assert!(
                out.unchecked
                    .iter()
                    .any(|item| item.what.starts_with("anchor-kind"))
            );
        }
    }
}

#[test]
fn outside_seed_requires_a_depth_zero_local_anchor_and_resolved_seed() {
    let text = "fn seed() {}\nfn other() {\n    action();\n}";
    let event = json!({"kind": "call", "site": value_site(3,3)});
    let row = guarded(event, json!([]), json!([]), Value::Null);
    for (options, calls, expected) in [
        (&[":depth", "0"][..], json!([]), vec!["outside-seed"]),
        (&[][..], json!([]), vec!["outside-seed"]),
        (&[][..], json!([{"entered": true}]), vec![]),
    ] {
        let query = query("guarded-paths", options, procedure());
        let mut row = row.clone();
        row["fields"]["calls"] = calls;
        assert_eq!(codes(&row_checks(&query, text, row)), expected);
    }
    let query = query("guarded-paths", &[":depth", "0"], procedure());
    let missing = row_checks(
        &query,
        "fn unknown() {}\nfn other() {\n    action();\n}",
        row,
    );
    assert!(missing.errors.is_empty());
    assert!(
        missing
            .unchecked
            .iter()
            .any(|item| item.what.starts_with("outside-seed"))
    );
}

#[test]
fn guard_checks_if_branch_but_leaves_preceding_guards_unchecked() {
    let query = query("guarded-paths", &[":depth", "0"], procedure());
    let text = "fn seed() {\n    if flag {\n        left();\n    } else {\n        right();\n    }\n    after();\n}\n";
    let predicate = json!({"kind":"if", "condition": written(2, 7, "flag"), "branch":"then"});
    for (line, expected) in [(3, vec![]), (5, vec!["guard-containment"]), (7, vec![])] {
        let event = json!({"kind":"call", "site":value_site(line,line)});
        let out = row_checks(
            &query,
            text,
            guarded(event, json!([predicate]), json!([]), Value::Null),
        );
        assert_eq!(codes(&out), expected);
        if line == 7 {
            assert!(
                out.unchecked
                    .iter()
                    .any(|item| item.what.starts_with("guard-containment"))
            );
        }
    }
}

#[test]
fn guard_checks_match_while_and_let_else_conditions() {
    let query = query("guarded-paths", &[":depth", "0"], procedure());
    let text = "fn seed() {\n    match mode {\n        A => first(),\n        B => second(),\n    }\n    while again {\n        repeat();\n    }\n    let Some(x) = object else {\n        abort();\n    };\n}\n";
    for (event_line, predicate, expected) in [
        (
            3,
            json!({"kind":"match", "condition":written(3, 8, "A"), "taken":true}),
            vec![],
        ),
        (
            4,
            json!({"kind":"match", "condition":written(3, 8, "A"), "taken":true}),
            vec!["guard-containment"],
        ),
        (
            7,
            json!({"kind":"while", "condition":written(6, 10, "again"), "taken":true}),
            vec![],
        ),
        (
            10,
            json!({"kind":"let-else", "condition":written(9, 18, "object"), "taken":false}),
            vec![],
        ),
    ] {
        let event = json!({"kind":"call", "site":value_site(event_line,event_line)});
        let mut event = event;
        if event_line == 3 {
            event["written"] = written(3, 13, "first()");
        }
        if event_line == 4 {
            event["written"] = written(4, 13, "second()");
        }
        let out = row_checks(
            &query,
            text,
            guarded(event, json!([predicate]), json!([]), Value::Null),
        );
        assert_eq!(codes(&out), expected, "event on {event_line}");
    }
}

#[test]
fn depth_counts_entered_hops_and_ignores_external_evidence() {
    let query = query("guarded-paths", &[":depth", "0"], procedure());
    let text = "fn seed() {\n    action();\n}\n";
    let event = json!({"kind":"call", "site":value_site(2,2)});
    let calls = json!([{"entered": false, "body": "external"}, {"body_traversed": false}]);
    let row = guarded(event.clone(), json!([]), calls, Value::Null);
    assert!(row_checks(&query, text, row).errors.is_empty());
    let row = guarded(event, json!([]), json!([{"entered": true}]), Value::Null);
    assert_eq!(codes(&row_checks(&query, text, row)), ["depth-exceeded"]);
}

#[test]
fn unsupported_seed_and_documented_step_do_not_create_findings() {
    for (step, expected) in [("contract-cases", 1), ("call-sites-from", 0)] {
        let input = Input {
            root: "nonexistent".into(),
            step: step.into(),
            query: json!([step, ["unsupported", ":name", "\"seed\""]]),
            answer: json!({"rows": []}),
        };
        let out = verify(input).unwrap();
        assert!(out.candidates.is_empty() && out.errors.is_empty());
        assert_eq!(out.unchecked.len(), expected);
    }
}

#[test]
fn source_spans_preserve_unicode_offsets_after_bom_and_shebang() {
    let query = query(
        "guarded-paths",
        &[":event", "\"call\""],
        json!(["call", ":name", "\"type\""]),
    );
    let text = "\u{feff}#!/usr/bin/env rust-script\nfn seed() {\n    let blå = r#type();\n}\n";
    let source = file(&query, text);
    assert_eq!(by_kind(&source, &query)["call"], ["r#type()"]);
    let event =
        json!({"kind": "call", "site": value_site(3, 3), "written": written(3, 15, "r#type()")});
    let row = guarded(event, json!([]), json!([]), Value::Null);
    let out = accounting(&query, text, json!({"rows": [row]}));
    assert!(out.errors.is_empty() && out.unchecked.is_empty());
}

#[test]
fn tail_branches_supply_error_candidates_and_partial_return_evidence() {
    let text = "fn seed() -> Result<u8, u8> {\n    if flag {\n        Err(1)\n    } else {\n        Ok(2)\n    }\n}\n";
    let errors = query("contract-cases", &[":kind", "\"error\""], procedure());
    assert_eq!(by_kind(&file(&errors, text), &errors)["error"], ["Err(1)"]);

    let returns = query("contract-cases", &[":kind", "\"return\""], procedure());
    let row = json!({"kind": "contract_case", "fields": {
        "kind": "return", "site": value_site(5, 5), "outcome": {"written": written(5, 8, "Ok(2)")}
    }});
    let out = accounting(&returns, text, json!({"rows": [row]}));
    assert!(out.errors.is_empty());
    assert_eq!(out.unchecked.len(), 1);
}

#[test]
fn exact_seed_paths_and_parse_failures_are_reported_per_file() {
    let scratch = std::env::temp_dir().join(format!("rql-verifier-test-{}", std::process::id()));
    fs::create_dir(&scratch).unwrap();
    fs::write(scratch.join("fixture.rs"), "fn seed() -> u8 {\n    1\n}\n").unwrap();
    fs::write(scratch.join("other.rs"), "fn seed() -> u8 { 2 }\n").unwrap();
    fs::write(scratch.join("invalid.rs"), "fn invalid\n").unwrap();
    let input = Input {
        root: scratch.clone(),
        step: "contract-cases".into(),
        query: form(
            "contract-cases",
            &[":kind", "\"return\""],
            json!([
                "procedure-of",
                ["where", "\"fixture.rs\"", ["callable", ":name", "\"seed\""]]
            ]),
        ),
        answer: json!({"rows": [{"kind": "contract_case", "fields": {
            "kind": "return", "site": value_site(2, 2), "calls": []
        }}]}),
    };
    let out = verify(input);
    fs::remove_dir_all(scratch).unwrap();
    let out = out.unwrap();
    assert!(out.errors.is_empty());
    assert_eq!(out.candidates.len(), 1);
    assert_eq!(out.candidates[0].site.path, "fixture.rs");
    assert_eq!(out.unchecked.len(), 1);
    assert_eq!(out.unchecked[0].what, "invalid.rs");
}

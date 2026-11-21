use serde_json::Value;
use syn::ext::IdentExt;
use syn::parse::Parser;

use crate::query::{Query, Step};
use crate::source::{Anchor, Bytes, Site, Source};
use crate::syntax::{Construct, Kind};
use crate::{Candidate, Finding, Output, Parsed};

pub struct Row<'a> {
    pub index: usize,
    pub site: Option<Site>,
    fields: &'a Value,
}

fn site(value: &Value) -> Option<Site> {
    serde_json::from_value(value.clone()).ok()
}

pub fn answer_rows(answer: &Value, step: Step) -> Vec<Row<'_>> {
    answer["rows"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, row)| {
            if row["kind"] != step.row() {
                return None;
            }
            let fields = &row["fields"];
            let primary = match step {
                Step::Contract => &fields["site"],
                Step::Value => &fields["sink"]["site"],
                Step::Guarded => &fields["event"]["site"],
            };
            Some(Row {
                index,
                site: site(primary),
                fields,
            })
        })
        .collect()
}

fn evidence(value: &Value) -> &Value {
    for key in ["written", "operand"] {
        if value[key]["verbatim"].is_array() {
            return &value[key];
        }
    }
    value
}

fn role_site(value: &Value) -> Option<(Site, &Value)> {
    if let Some(site) = site(value).or_else(|| site(&value["site"])) {
        return Some((site, evidence(value)));
    }
    for key in ["written", "operand"] {
        if let Some(site) = site(&value[key]["site"]) {
            return Some((site, evidence(&value[key])));
        }
    }
    None
}

impl Row<'_> {
    fn primary_evidence(&self, step: Step) -> &Value {
        match step {
            Step::Contract => evidence(&self.fields["outcome"]),
            Step::Value => evidence(&self.fields["sink"]),
            Step::Guarded => evidence(&self.fields["event"]),
        }
    }

    fn can_cite_subport(&self, query: &Query) -> bool {
        match query.step {
            Step::Value => true,
            Step::Contract => self.fields["kind"] == "return",
            Step::Guarded => !query.incoming && self.fields["event"]["kind"] == "return",
        }
    }

    fn candidate_sites(&self, query: &Query) -> Vec<(Site, &Value)> {
        match query.step {
            Step::Guarded if query.incoming => {
                role_site(&self.fields["pivot"]).into_iter().collect()
            }
            Step::Value => {
                if let Some(selected) = role_site(&self.fields["selected_port"]) {
                    return vec![selected];
                }
                let endpoints: &[_] = if query.downstream {
                    &["source"]
                } else {
                    &["sink", "source"]
                };
                endpoints
                    .iter()
                    .filter_map(|endpoint| {
                        let value = &self.fields[*endpoint];
                        (value["port"] == query.selector)
                            .then(|| role_site(value))
                            .flatten()
                    })
                    .collect()
            }
            _ => self
                .site
                .iter()
                .map(|site| (site.clone(), self.primary_evidence(query.step)))
                .collect(),
        }
    }
}

fn project(source: &Source, range: Bytes, anchor: Anchor) -> Bytes {
    if anchor.exact {
        range
    } else {
        source.by_line(range)
    }
}

pub fn account(
    query: &Query,
    rows: &[Row<'_>],
    answer: &Value,
    file: &Parsed,
    construct: &Construct,
    candidate: &Candidate,
    out: &mut Output,
) {
    let source = &file.source;
    let owner = &file.syntax.procedures[construct.owner.expect("selected procedure")];
    let mut ambiguity = None;
    for row in rows {
        for (site, evidence) in row.candidate_sites(query) {
            let Some(anchor) = source.anchor(&site, evidence) else {
                continue;
            };
            let candidate_range = project(source, construct.span, anchor);
            let statement = project(source, construct.statement, anchor);
            let body = project(source, owner.body, anchor);
            let declaration = project(source, owner.span, anchor);
            if anchor.range.contains(candidate_range) && statement.contains(anchor.range) {
                if anchor.range.contains(body) || anchor.range.contains(declaration) {
                    // Line-only evidence can be indistinguishable from a whole-body
                    // citation in a one-line function.
                    if !anchor.exact {
                        ambiguity = Some(
                            "line-only role site does not distinguish the statement from the body",
                        );
                    }
                } else {
                    return;
                }
            } else if candidate_range.contains(anchor.range)
                && row.can_cite_subport(query)
                && (query.step == Step::Value || construct.kind == Kind::Return)
            {
                ambiguity =
                    Some("role site may cite an alternative exit or part of the selected port");
            }
        }
    }

    for exclusion in answer["excluded"].as_array().into_iter().flatten() {
        let Some((site, evidence)) = role_site(&exclusion["candidate_site"]) else {
            continue;
        };
        if site != candidate.site {
            continue;
        }
        let exact_site = source
            .anchor(&site, evidence)
            .is_some_and(|anchor| !anchor.exact || anchor.range == construct.span);
        let valid_reason = match exclusion["reason"].as_str() {
            Some(
                "kind-not-selected" | "port-not-selected" | "unreachable" | "not-the-seed"
                | "macro-not-call" | "duplicate",
            ) => true,
            Some("other") => exclusion["detail"]
                .as_str()
                .is_some_and(|detail| !detail.trim().is_empty()),
            _ => false,
        };
        if exact_site && valid_reason {
            return;
        }
    }
    if let Some(why) = ambiguity {
        out.unchecked(&candidate.id, why);
        return;
    }
    out.errors.push(Finding {
        code: "unaccounted-candidate",
        message: format!(
            "{} candidate has no covering row or valid exclusion",
            candidate.kind
        ),
        row: None,
        candidate: Some(candidate.id.clone()),
        site: candidate.site.clone(),
    });
}

fn error(
    out: &mut Output,
    code: &'static str,
    message: impl Into<String>,
    row: &Row<'_>,
    site: &Site,
) {
    out.errors.push(Finding {
        code,
        message: message.into(),
        row: Some(row.index),
        candidate: None,
        site: site.clone(),
    });
}

fn unchecked(out: &mut Output, check: &str, row: &Row<'_>, why: &str) {
    out.unchecked(format!("{check} row {}", row.index), why);
}

fn unentered(call: &Value) -> bool {
    ["body_traversed", "entered", "crossed"]
        .iter()
        .any(|key| call[key] == false)
        || call["depth_charged"].as_u64() == Some(0)
        || ["external", "opaque", "unresolved"].iter().any(|status| {
            ["body", "resolution", "target_kind"].iter().any(|key| {
                call[key].as_str().is_some_and(|value| {
                    value == *status
                        || value
                            .strip_prefix(status)
                            .is_some_and(|suffix| suffix.starts_with('_'))
                })
            })
        })
}

fn depth(query: &Query, row: &Row<'_>, site: &Site, out: &mut Output) -> Option<usize> {
    if let Some(calls) = row.fields["calls"].as_array() {
        if calls.iter().any(|call| !call.is_object()) {
            unchecked(
                out,
                "depth-exceeded",
                row,
                "calls is not a list of recorded invocation objects",
            );
            return None;
        }
        let crossed = calls.iter().filter(|call| !unentered(call)).count();
        if crossed > query.depth {
            error(
                out,
                "depth-exceeded",
                format!(
                    "recorded calls chain has {crossed} hops, depth is {}",
                    query.depth
                ),
                row,
                site,
            );
        }
        Some(crossed)
    } else {
        unchecked(out, "depth-exceeded", row, "no recorded calls chain");
        None
    }
}

fn claim(step: Step, fields: &Value) -> Option<(Vec<Kind>, Option<&str>)> {
    let (value, kind) = match step {
        Step::Contract => (&fields["outcome"], fields["kind"].as_str()?),
        Step::Guarded => (&fields["event"], fields["event"]["kind"].as_str()?),
        Step::Value => (&fields["sink"], fields["sink"]["port"].as_str()?),
    };
    let kinds = match (step, kind) {
        (_, "return") => vec![Kind::Return, Kind::Operand, Kind::Try],
        (Step::Contract, "error") => vec![Kind::Error],
        (Step::Contract, "callback") => vec![Kind::Callback],
        (Step::Contract, "panic") => vec![Kind::Panic],
        (Step::Guarded, "raise") => vec![Kind::Raise],
        (Step::Guarded, "call" | "method_call") => vec![Kind::Call],
        (Step::Guarded | Step::Value, "write") => vec![Kind::Write],
        _ => return None,
    };
    let name = match kind {
        "call" | "method_call" => {
            let target = [
                value["resolved_target"].as_str(),
                value["resolved_target"]["name"].as_str(),
                value["resolved_identity"].as_str(),
            ]
            .into_iter()
            .flatten()
            .next();
            target.and_then(|target| {
                let name = target.rsplit("::").next()?;
                syn::Ident::parse_any.parse_str(name).ok()?;
                Some(name.strip_prefix("r#").unwrap_or(name))
            })
        }
        "write" => value["place"].as_str().or(value["place"]["text"].as_str()),
        _ => None,
    };
    Some((kinds, name))
}

fn anchor_kind(
    query: &Query,
    row: &Row<'_>,
    site: &Site,
    file: &Parsed,
    anchor: Anchor,
    out: &mut Output,
) {
    let Some((kinds, expected)) = claim(query.step, row.fields) else {
        unchecked(
            out,
            "anchor-kind",
            row,
            "no supported syntactic claim at the primary anchor",
        );
        return;
    };
    let source = &file.source;
    let mut uncertain = false;
    for construct in &file.syntax.constructs {
        if !kinds.contains(&construct.kind) {
            continue;
        }
        let span = project(source, construct.span, anchor);
        if anchor.range.contains(span) {
            match (construct.kind, expected) {
                (Kind::Call, Some(name)) if construct.name.as_deref() != Some(name) => {
                    uncertain |= construct.name.is_none() || construct.indirect;
                }
                (Kind::Write, Some(place)) => {
                    let written = source.slice(construct.span);
                    if written == place {
                        return;
                    }
                    let written_tokens = written.parse::<proc_macro2::TokenStream>();
                    let claimed_tokens = place.parse::<proc_macro2::TokenStream>();
                    if let (Ok(written_tokens), Ok(claimed_tokens)) =
                        (written_tokens, claimed_tokens)
                        && written_tokens.to_string() == claimed_tokens.to_string()
                    {
                        return;
                    }
                    // A resolved place can use an actual/formal binding or
                    // branch-specific index. Conclude on simple local places.
                    uncertain |= !simple_place(written) || !simple_place(place) || query.depth > 0;
                }
                _ => return,
            }
        } else if anchor.range.overlaps(span) {
            uncertain = true;
        }
    }
    // An error can reach a syntactic return through a local binding or
    // call. The absence of a written Err constructor is inconclusive there.
    uncertain |= kinds == [Kind::Error]
        && file.syntax.constructs.iter().any(|construct| {
            construct.kind == Kind::Return
                && project(source, construct.span, anchor).overlaps(anchor.range)
        });
    uncertain |= kinds.contains(&Kind::Return)
        && file.syntax.procedures.iter().any(|procedure| {
            let closing_brace = Bytes {
                start: procedure.body.end.saturating_sub(1),
                end: procedure.body.end,
            };
            project(source, closing_brace, anchor).overlaps(anchor.range)
        });
    if file
        .syntax
        .opaque
        .iter()
        .any(|&span| project(source, span, anchor).overlaps(anchor.range))
        || uncertain
        || kinds == [Kind::Callback]
        || kinds == [Kind::Error] && query.depth > 0
    {
        unchecked(
            out,
            "anchor-kind",
            row,
            "anchor intersects opaque, partial or semantically resolved evidence",
        );
    } else {
        let expected = expected.map_or_else(String::new, |name| format!(" ({name})"));
        error(
            out,
            "anchor-kind",
            format!("anchor contains no {} construct{expected}", kinds[0].name()),
            row,
            site,
        );
    }
}

fn simple_place(text: &str) -> bool {
    fn simple(expr: &syn::Expr) -> bool {
        match expr {
            syn::Expr::Path(_) => true,
            syn::Expr::Field(field) => simple(&field.base),
            syn::Expr::Index(index) => {
                simple(&index.expr) && matches!(&*index.index, syn::Expr::Lit(_))
            }
            _ => false,
        }
    }
    syn::parse_str::<syn::Expr>(text).is_ok_and(|expr| simple(&expr))
}

fn guard_site(guard: &Value) -> Option<(Site, &Value)> {
    for key in ["test", "condition", "predicate"] {
        if let Some(site) = role_site(&guard[key]) {
            return Some(site);
        }
    }
    role_site(guard)
}

fn guard_checks(row: &Row<'_>, file: &Parsed, event: Anchor, out: &mut Output) {
    let source = &file.source;
    for guard in row.fields["guards"].as_array().into_iter().flatten() {
        let Some((site, evidence)) = guard_site(guard) else {
            unchecked(
                out,
                "guard-containment",
                row,
                "guard has no written condition site",
            );
            continue;
        };
        let Some(condition) = source.anchor(&site, evidence) else {
            unchecked(
                out,
                "guard-containment",
                row,
                "guard and event may be in different procedures",
            );
            continue;
        };
        let branch = ["branch", "taken", "polarity", "outcome"]
            .iter()
            .find_map(|key| {
                guard[key].as_bool().or(match guard[key].as_str() {
                    Some("then" | "true") => Some(true),
                    Some("else" | "false") => Some(false),
                    _ => None,
                })
            });
        let mut exact_conflicts = 0;
        let mut matched = 0;
        let mut holds = false;
        for control in &file.syntax.guards {
            if !control
                .conditions
                .iter()
                .any(|&span| condition.range.contains(project(source, span, condition)))
            {
                continue;
            }
            matched += 1;
            for &(polarity, span) in &control.branches {
                let branch = branch
                    .zip(polarity)
                    .map(|(claimed, actual)| claimed == actual);
                if branch != Some(false) && project(source, span, event).contains(event.range) {
                    holds = true;
                }
            }
            if condition.exact
                && branch.is_some()
                && control.scope.contains(event.range)
                && control.branches.iter().all(|&(polarity, span)| {
                    polarity != branch && polarity.is_some()
                        || !project(source, span, event).overlaps(event.range)
                })
                && (control
                    .branches
                    .iter()
                    .any(|&(_, span)| span.contains(event.range))
                    || branch == Some(true)
                        && control
                            .match_arms
                            .iter()
                            .any(|&span| span.contains(event.range)))
            {
                exact_conflicts += 1;
            }
        }
        if holds {
            continue;
        }
        let claims_condition = matches!(
            guard["kind"].as_str(),
            Some("if" | "while" | "match" | "let-else" | "let_else")
        );
        let known_wrong_condition = matched == 0
            && claims_condition
            && condition.exact
            && !file
                .syntax
                .guards
                .iter()
                .flat_map(|control| &control.conditions)
                .any(|&span| span.overlaps(condition.range))
            && !file
                .syntax
                .opaque
                .iter()
                .any(|&span| span.overlaps(condition.range));
        if known_wrong_condition {
            error(
                out,
                "guard-containment",
                "cited guard is not a written branch or loop condition",
                row,
                &site,
            );
        } else if matched > 0 && exact_conflicts == matched {
            error(
                out,
                "guard-containment",
                "event lies in a different branch from the claimed guard outcome",
                row,
                &site,
            );
        } else {
            unchecked(
                out,
                "guard-containment",
                row,
                "branch, evaluation order or relation to a preceding guard is ambiguous",
            );
        }
    }
}

pub fn check(
    query: &Query,
    row: &Row<'_>,
    site: &Site,
    file: Option<&Parsed>,
    seed_found: bool,
    out: &mut Output,
) {
    let recorded_depth = depth(query, row, site, out);
    let Some(file) = file else {
        unchecked(
            out,
            "anchor-kind/outside-seed/guard-containment",
            row,
            "primary source is unavailable or unparsed",
        );
        return;
    };
    let Some(anchor) = file.source.anchor(site, row.primary_evidence(query.step)) else {
        unchecked(
            out,
            "anchor-kind/outside-seed/guard-containment",
            row,
            "primary source span is unavailable",
        );
        return;
    };
    anchor_kind(query, row, site, file, anchor, out);

    let local_anchor = !query.incoming && !(query.step == Step::Value && query.downstream);
    if local_anchor
        && (query.depth == 0
            || recorded_depth == Some(0)
            || row.fields["depth"].as_u64() == Some(0))
    {
        if !seed_found {
            unchecked(out, "outside-seed", row, "no supported authored seed");
        } else if file.seeds.iter().all(|&id| {
            !project(&file.source, file.syntax.procedures[id].span, anchor).overlaps(anchor.range)
        }) {
            error(
                out,
                "outside-seed",
                "depth-zero primary anchor lies outside every seed procedure",
                row,
                site,
            );
        }
    } else if local_anchor && recorded_depth.is_none() {
        unchecked(out, "outside-seed", row, "row depth cannot be determined");
    }
    if query.step == Step::Guarded {
        guard_checks(row, file, anchor, out);
    }
}

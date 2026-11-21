use std::collections::BTreeSet;

use serde_json::Value;
use syn::ext::IdentExt;
use syn::parse::Parser;

use crate::syntax::Kind;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Step {
    Contract,
    Value,
    Guarded,
}

impl Step {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "contract-cases" | "contract_cases" => Some(Self::Contract),
            "value-transfers" | "value_transfers" => Some(Self::Value),
            "guarded-paths" | "guarded_paths" => Some(Self::Guarded),
            _ => None,
        }
    }

    pub fn row(self) -> &'static str {
        match self {
            Self::Contract => "contract_case",
            Self::Value => "value_transfer",
            Self::Guarded => "guarded_path",
        }
    }
}

pub struct Query {
    pub step: Step,
    pub name: String,
    pub call_seed: bool,
    pub procedure_of_call: bool,
    pub incoming: bool,
    pub downstream: bool,
    pub depth: usize,
    pub selector: String,
    paths: Option<BTreeSet<String>>,
}

fn quoted(value: &Value) -> Option<String> {
    serde_json::from_str(value.as_str()?).ok()
}

fn unwrap<'a>(value: &'a Value, paths: &mut Option<BTreeSet<String>>) -> Option<&'a [Value]> {
    let form = value.as_array()?;
    match form.first()?.as_str()? {
        "profile" | "result-detail" | "result_detail" => unwrap(form.last()?, paths),
        "where" if form.len() >= 3 => {
            let exact_paths = form[1..form.len() - 1]
                .iter()
                .map(|value| {
                    let path = quoted(value)?;
                    (!path.contains(['*', '?', '[', ']', '{', '}', '\\'])).then_some(path)
                })
                .collect::<Option<BTreeSet<_>>>()?;
            *paths = Some(match paths.take() {
                None => exact_paths,
                Some(previous) => previous.intersection(&exact_paths).cloned().collect(),
            });
            unwrap(form.last()?, paths)
        }
        _ => Some(form),
    }
}

impl Query {
    pub fn parse(step: Step, value: &Value) -> Option<Self> {
        let mut paths = None;
        let form = unwrap(value, &mut paths)?;
        // An outer where filters the menu output, not the seed lookup.
        if paths.is_some() {
            return None;
        }
        if Step::parse(form.first()?.as_str()?)? != step || form.len() < 2 {
            return None;
        }
        let mut depth = if step == Step::Value { 2 } else { 1 };
        let mut selector = if step == Step::Value { "return" } else { "all" }.to_owned();
        let mut incoming = false;
        let mut downstream = false;
        let (options, remainder) = form[1..form.len() - 1].as_chunks::<2>();
        for option in options {
            match (step, option[0].as_str()?) {
                (_, ":depth") => depth = option[1].as_str()?.parse().ok()?,
                (Step::Guarded, ":direction") => incoming = quoted(&option[1])? == "incoming",
                (Step::Value, ":direction") => downstream = quoted(&option[1])? == "downstream",
                (Step::Contract, ":kind") | (Step::Guarded, ":event") | (Step::Value, ":port") => {
                    selector = quoted(&option[1])?;
                }
                _ => return None,
            }
        }
        if !remainder.is_empty() {
            return None;
        }

        let mut seed = unwrap(form.last()?, &mut paths)?;
        let is_procedure = matches!(
            seed.first()?.as_str()?,
            "procedure-of" | "procedure_of" | "enclosing-decl" | "enclosing_decl"
        );
        if is_procedure && seed.len() == 2 {
            seed = unwrap(&seed[1], &mut paths)?;
        }
        let call_seed = match seed {
            [head, key, _] if key == ":name" => match head.as_str()? {
                "call" => true,
                "function" | "callable" => false,
                _ => return None,
            },
            _ => return None,
        };
        let name = quoted(&seed[2])?;
        // Qualified/pattern names require symbol or pattern resolution.
        let name = syn::Ident::parse_any
            .parse_str(&name)
            .ok()?
            .unraw()
            .to_string();
        let procedure_of_call = call_seed && is_procedure;
        if incoming && call_seed && !is_procedure {
            return None;
        }
        Some(Self {
            step,
            name,
            call_seed: call_seed && !is_procedure,
            procedure_of_call,
            incoming,
            downstream,
            depth,
            selector,
            paths,
        })
    }

    pub fn selects_path(&self, path: &str) -> bool {
        self.paths.as_ref().is_none_or(|paths| paths.contains(path))
    }

    pub fn selects_kind(&self, kind: Kind) -> bool {
        let eligible = match self.step {
            Step::Contract => matches!(
                kind,
                Kind::Return | Kind::Error | Kind::Panic | Kind::Callback
            ),
            Step::Value => matches!(
                kind,
                Kind::Operand | Kind::Argument | Kind::Input | Kind::Write
            ),
            Step::Guarded if self.incoming || self.call_seed => kind == Kind::Call,
            Step::Guarded => matches!(
                kind,
                Kind::Call | Kind::Write | Kind::Return | Kind::Try | Kind::Raise
            ),
        };
        eligible
            && (self.incoming
                || self.call_seed
                || self.selector == "all"
                || self.selector == kind.name())
    }
}

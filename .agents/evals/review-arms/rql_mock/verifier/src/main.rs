mod query;
mod rows;
mod source;
mod syntax;

use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use query::{Query, Step};
use source::{Site, Source};
use syntax::{Construct, Kind, Syntax};

#[derive(Deserialize)]
struct Input {
    root: PathBuf,
    step: String,
    query: Value,
    answer: Value,
}

#[derive(Clone, Copy)]
enum CandidateOrigin {
    SeedBody,
    SeedCallSite,
    IncomingCallSite,
}

struct Selected<'a> {
    construct: &'a Construct,
    origin: CandidateOrigin,
}

#[derive(Serialize)]
struct Candidate {
    #[serde(skip)]
    origin: CandidateOrigin,
    id: String,
    kind: &'static str,
    site: Site,
    procedure: String,
}

#[derive(Serialize)]
struct Finding {
    code: &'static str,
    message: String,
    row: Option<usize>,
    candidate: Option<String>,
    site: Site,
}

#[derive(Serialize)]
struct Unchecked {
    what: String,
    why: String,
}

#[derive(Serialize)]
struct Output {
    version: u8,
    candidates: Vec<Candidate>,
    errors: Vec<Finding>,
    unchecked: Vec<Unchecked>,
}

impl Output {
    fn new() -> Self {
        Self {
            version: 1,
            candidates: Vec::new(),
            errors: Vec::new(),
            unchecked: Vec::new(),
        }
    }

    fn unchecked(&mut self, what: impl Into<String>, why: impl Into<String>) {
        self.unchecked.push(Unchecked {
            what: what.into(),
            why: why.into(),
        });
    }
}

struct Parsed {
    source: Source,
    syntax: Syntax,
    seeds: BTreeSet<usize>,
}

impl Parsed {
    fn new(query: &Query, mut source: Source) -> syn::Result<Self> {
        let syntax = syntax::scan(&mut source)?;
        let mut seeds = BTreeSet::new();
        if query.selects_path(&source.path) {
            if query.procedure_of_call || query.call_seed {
                seeds.extend(syntax.constructs.iter().filter_map(|construct| {
                    (construct.kind == Kind::Call && construct.name.as_deref() == Some(&query.name))
                        .then_some(construct.owner)
                        .flatten()
                }));
            } else {
                seeds.extend(
                    syntax
                        .procedures
                        .iter()
                        .enumerate()
                        .filter_map(|(id, procedure)| (procedure.name == query.name).then_some(id)),
                );
            }
        }
        Ok(Self {
            source,
            syntax,
            seeds,
        })
    }

    fn candidates(&self, query: &Query, seed_names: &BTreeSet<&str>) -> Vec<Selected<'_>> {
        // An incoming route must cross at least the pivot invocation.
        if query.incoming && query.depth == 0 {
            return Vec::new();
        }
        let mut selected: Vec<_> = self
            .syntax
            .constructs
            .iter()
            .filter_map(|construct| {
                let owner = construct.owner?;
                if !query.selects_kind(construct.kind) {
                    return None;
                }
                let origin = if query.incoming {
                    construct
                        .name
                        .as_deref()
                        .is_some_and(|name| seed_names.contains(name))
                        .then_some(CandidateOrigin::IncomingCallSite)
                } else if query.call_seed {
                    (self.seeds.contains(&owner) && construct.name.as_deref() == Some(&query.name))
                        .then_some(CandidateOrigin::SeedCallSite)
                } else {
                    (!construct.deferred && self.seeds.contains(&owner))
                        .then_some(CandidateOrigin::SeedBody)
                }?;
                Some(Selected { construct, origin })
            })
            .collect();
        selected.sort_unstable_by_key(|item| (item.construct.span, item.construct.kind));
        selected.dedup_by_key(|item| (item.construct.span, item.construct.kind));
        selected
    }
}

fn scan_directory(
    root: &Path,
    dir: &Path,
    query: &Query,
    files: &mut Vec<Parsed>,
    out: &mut Output,
) -> io::Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_unstable_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let relative = path.strip_prefix(root).expect("entry under snapshot root");
        let display = relative.display().to_string();
        let ty = match entry.file_type() {
            Ok(ty) => ty,
            Err(error) => {
                out.unchecked(display, format!("cannot inspect snapshot entry: {error}"));
                continue;
            }
        };
        if ty.is_dir() && entry.file_name() != ".git" {
            if let Err(error) = scan_directory(root, &path, query, files, out) {
                out.unchecked(display, format!("cannot read directory: {error}"));
            }
        } else if ty.is_file() && path.extension().is_some_and(|extension| extension == "rs") {
            let Some(components) = relative
                .iter()
                .map(|part| part.to_str())
                .collect::<Option<Vec<_>>>()
            else {
                out.unchecked(display, "non-UTF-8 path cannot be cited in the answer");
                continue;
            };
            let repo_path = components.join("/");
            match fs::read_to_string(&path) {
                Ok(text) => match Parsed::new(query, Source::new(repo_path.clone(), text)) {
                    Ok(file) => files.push(file),
                    Err(error) => out.unchecked(repo_path, format!("Rust parse failed: {error}")),
                },
                Err(error) => out.unchecked(repo_path, format!("cannot read source: {error}")),
            }
        }
        // A snapshot's symbolic links are not regular source blobs.
    }
    Ok(())
}

fn verify(input: Input) -> io::Result<Output> {
    let mut out = Output::new();
    let Some(step) = Step::parse(&input.step) else {
        return Ok(out);
    };
    let Some(query) = Query::parse(step, &input.query) else {
        out.unchecked(
            "query",
            "seed, selector or enclosing RQL form needs semantic resolution",
        );
        rows::unchecked_seed_exclusions(&input.answer, &mut out);
        return Ok(out);
    };
    let mut files = Vec::new();
    scan_directory(&input.root, &input.root, &query, &mut files, &mut out)?;
    let rows = rows::answer_rows(&input.answer, query.step);
    let seed_names: BTreeSet<_> = files
        .iter()
        .flat_map(|file| {
            file.seeds
                .iter()
                .map(|&id| file.syntax.procedures[id].name.as_str())
        })
        .collect();
    let seed_found = !seed_names.is_empty();
    if !seed_found {
        out.unchecked(
            "seed",
            format!("no supported authored seed named {:?}", query.name),
        );
    }

    for file in &files {
        let source = &file.source;
        if !(query.incoming && query.depth == 0)
            && (query.selects_kind(Kind::Argument) || query.selects_kind(Kind::Call))
        {
            for &span in &file.syntax.uncertain_constructors {
                if query.incoming
                    || file
                        .seeds
                        .iter()
                        .any(|&id| file.syntax.procedures[id].body.contains(span))
                {
                    let site = source.site(span);
                    out.unchecked(
                        format!("{}:{}", site.path, site.start_line),
                        "naming exception or alias obscures whether this is a call or constructor",
                    );
                }
            }
        }
        for span in &file.syntax.uncertain_callbacks {
            if query.step == Step::Contract
                && matches!(query.selector.as_str(), "all" | "callback")
                && file
                    .seeds
                    .iter()
                    .any(|&id| file.syntax.procedures[id].body.contains(*span))
            {
                let site = source.site(*span);
                out.unchecked(
                    format!("{}:{}", site.path, site.start_line),
                    "a local may shadow the callback formal",
                );
            }
        }
    }

    let selected = files.iter().flat_map(|file| {
        file.candidates(&query, &seed_names)
            .into_iter()
            .map(move |item| (file, item))
    });
    rows::account(&query, &rows, &input.answer, selected, &mut out);

    for row in &rows {
        let Some(site) = &row.site else {
            out.unchecked(format!("row {}", row.index), "primary site is unavailable");
            continue;
        };
        let file = files.iter().find(|file| file.source.path == site.path);
        rows::check(&query, row, site, file, seed_found, &mut out);
    }
    Ok(out)
}

fn main() -> Result<(), Box<dyn Error>> {
    let input = serde_json::from_reader(io::stdin().lock())?;
    serde_json::to_writer(io::stdout().lock(), &verify(input)?)?;
    Ok(())
}

#[cfg(test)]
mod tests;

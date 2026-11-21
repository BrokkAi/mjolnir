use std::collections::{BTreeMap, BTreeSet};

use syn::ext::IdentExt;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};

use crate::source::{Bytes, Source};

// The tuple variants supplied by Result and Option in the standard prelude.
const PRELUDE_CONSTRUCTORS: &[(&str, &str, &str)] = &[
    ("result", "Result", "Ok"),
    ("result", "Result", "Err"),
    ("option", "Option", "Some"),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Declaration {
    Function,
    Constructor,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Module {
    crate_dir: String,
    path: Vec<String>,
}

impl Module {
    fn for_file(path: &str) -> Self {
        let pieces: Vec<_> = path.split('/').collect();
        let (root, module) = if let Some(src) = pieces.iter().rposition(|&part| part == "src") {
            (&pieces[..src], &pieces[src + 1..])
        } else {
            (&pieces[..pieces.len() - 1], &pieces[pieces.len() - 1..])
        };
        let mut module = module.to_vec();
        if let Some(file) = module.pop() {
            let stem = file.strip_suffix(".rs").unwrap_or(file);
            if stem != "mod" && !(module.is_empty() && matches!(stem, "lib" | "main")) {
                module.push(stem);
            }
        }
        Self {
            crate_dir: root.join("/"),
            path: module.into_iter().map(str::to_owned).collect(),
        }
    }
}

#[derive(Clone)]
enum Binding {
    Declared(Declaration),
    Imported(Vec<String>),
}

type LookupKey = (String, Vec<String>);

#[derive(Default)]
pub struct Declarations {
    modules: BTreeMap<LookupKey, Vec<Binding>>,
    locals: BTreeMap<LookupKey, Vec<(Bytes, Binding)>>,
}

impl Declarations {
    pub fn add_file(&mut self, source: &Source, file: &syn::File) {
        Indexer {
            declarations: self,
            source,
            module: Module::for_file(&source.path),
            block: None,
        }
        .visit_file(file);
    }

    pub fn resolve(
        &self,
        source: &Source,
        inline_modules: &[String],
        at: usize,
        path: &syn::Path,
        self_type: Option<&syn::Path>,
    ) -> Option<Declaration> {
        let mut module = Module::for_file(&source.path);
        module.path.extend_from_slice(inline_modules);
        let mut names = segments(path);
        if names.first().is_some_and(|name| name == "Self") {
            names.splice(..1, segments(self_type?));
        }
        if path.leading_colon.is_some() {
            return prelude_constructor(&names).then_some(Declaration::Constructor);
        }
        let local_key = (source.path.clone(), names.clone());
        if let Some(bindings) = self.locals.get(&local_key) {
            let visible = || {
                bindings
                    .iter()
                    .filter(|(scope, _)| (scope.start..scope.end).contains(&at))
            };
            if let Some(width) = visible().map(|(scope, _)| scope.end - scope.start).min() {
                return consensus(
                    visible()
                        .filter(|(scope, _)| scope.end - scope.start == width)
                        .map(|(_, binding)| {
                            self.binding(&module, &local_key, binding, &[], &mut BTreeSet::new())
                        }),
                );
            }
        }
        self.resolve_names(&module, &names, &mut BTreeSet::new())
    }

    fn resolve_names(
        &self,
        module: &Module,
        names: &[String],
        visiting: &mut BTreeSet<LookupKey>,
    ) -> Option<Declaration> {
        let has_prefix = names
            .first()
            .is_some_and(|first| matches!(first.as_str(), "crate" | "self" | "super"));
        let (head, names) = if has_prefix {
            self.path_prefix(module, names)?
        } else {
            (module.clone(), names)
        };
        let mut absolute = head.path.clone();
        absolute.extend_from_slice(names);
        let key = (head.crate_dir.clone(), absolute);
        if let Some(bindings) = self.modules.get(&key) {
            let scope = Module {
                crate_dir: head.crate_dir.clone(),
                path: key.1[..key.1.len() - 1].to_vec(),
            };
            return consensus(
                bindings
                    .iter()
                    .map(|binding| self.binding(&scope, &key, binding, &[], visiting)),
            );
        }
        if let Some((first, suffix)) = names.split_first()
            && !suffix.is_empty()
        {
            let mut prefix = head.path.clone();
            prefix.push(first.clone());
            let alias = (head.crate_dir.clone(), prefix);
            if let Some(bindings) = self.modules.get(&alias) {
                return consensus(
                    bindings
                        .iter()
                        .map(|binding| self.binding(&head, &alias, binding, suffix, visiting)),
                );
            }
        }
        (!has_prefix && prelude_constructor(names)).then_some(Declaration::Constructor)
    }

    fn path_prefix<'a>(
        &self,
        module: &Module,
        names: &'a [String],
    ) -> Option<(Module, &'a [String])> {
        let mut head = module.clone();
        let rest = match names.first()?.as_str() {
            "crate" => {
                head.path.clear();
                &names[1..]
            }
            "self" => &names[1..],
            "super" => {
                let count = names.iter().take_while(|name| *name == "super").count();
                head.path.truncate(head.path.len().checked_sub(count)?);
                &names[count..]
            }
            _ => return None,
        };
        (!rest.is_empty()).then_some((head, rest))
    }

    fn binding(
        &self,
        scope: &Module,
        key: &LookupKey,
        binding: &Binding,
        suffix: &[String],
        visiting: &mut BTreeSet<LookupKey>,
    ) -> Option<Declaration> {
        match binding {
            Binding::Declared(kind) => suffix.is_empty().then_some(*kind),
            Binding::Imported(target) => {
                // Track the binding, since substituting it can grow the
                // expanded path without ever repeating a lookup key.
                if !visiting.insert(key.clone()) {
                    return None;
                }
                let mut expanded = target.clone();
                expanded.extend_from_slice(suffix);
                let result = self.resolve_names(scope, &expanded, visiting).or_else(|| {
                    // An unqualified use path may start at the crate root.
                    if scope.path.is_empty()
                        || target
                            .first()
                            .is_some_and(|head| matches!(head.as_str(), "self" | "super" | "crate"))
                    {
                        return None;
                    }
                    let root = Module {
                        crate_dir: scope.crate_dir.clone(),
                        path: Vec::new(),
                    };
                    self.resolve_names(&root, &expanded, visiting)
                });
                visiting.remove(key);
                result
            }
        }
    }
}

fn prelude_constructor(names: &[String]) -> bool {
    PRELUDE_CONSTRUCTORS
        .iter()
        .any(|&(module, owner, variant)| match names {
            [name] => name == variant,
            [ty, name] => ty == owner && name == variant,
            [library, namespace, ty, name] => {
                matches!(library.as_str(), "std" | "core")
                    && namespace == module
                    && ty == owner
                    && name == variant
            }
            _ => false,
        })
}

fn consensus(mut kinds: impl Iterator<Item = Option<Declaration>>) -> Option<Declaration> {
    let first = kinds.next()??;
    kinds.all(|kind| kind == Some(first)).then_some(first)
}

fn segments(path: &syn::Path) -> Vec<String> {
    path.segments
        .iter()
        .map(|segment| segment.ident.unraw().to_string())
        .collect()
}

struct Indexer<'a> {
    declarations: &'a mut Declarations,
    source: &'a Source,
    module: Module,
    block: Option<Bytes>,
}

impl Indexer<'_> {
    fn insert(&mut self, prefix: &[String], name: String, binding: Binding) {
        let mut path = prefix.to_vec();
        path.push(name);
        if let Some(block) = self.block {
            self.declarations
                .locals
                .entry((self.source.path.clone(), path))
                .or_default()
                .push((block, binding));
        } else {
            let mut qualified = self.module.path.clone();
            qualified.extend(path);
            self.declarations
                .modules
                .entry((self.module.crate_dir.clone(), qualified))
                .or_default()
                .push(binding);
        }
    }

    fn use_tree(&mut self, prefix: &mut Vec<String>, tree: &syn::UseTree) {
        match tree {
            syn::UseTree::Path(path) => {
                prefix.push(path.ident.unraw().to_string());
                self.use_tree(prefix, &path.tree);
                prefix.pop();
            }
            syn::UseTree::Group(group) => {
                for tree in &group.items {
                    self.use_tree(prefix, tree);
                }
            }
            syn::UseTree::Name(name) => {
                let name = name.ident.unraw().to_string();
                let mut target = prefix.clone();
                let alias = if name == "self" {
                    let Some(last) = target.last() else { return };
                    last.clone()
                } else {
                    target.push(name.clone());
                    name
                };
                self.insert(&[], alias, Binding::Imported(target));
            }
            syn::UseTree::Rename(rename) => {
                let mut target = prefix.clone();
                let name = rename.ident.unraw().to_string();
                if name != "self" {
                    target.push(name);
                }
                self.insert(
                    &[],
                    rename.rename.unraw().to_string(),
                    Binding::Imported(target),
                );
            }
            syn::UseTree::Glob(_) => {}
        }
    }
}

impl<'ast> Visit<'ast> for Indexer<'_> {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        self.module.path.push(item.ident.unraw().to_string());
        let block = self.block.take();
        visit::visit_item_mod(self, item);
        self.block = block;
        self.module.path.pop();
    }

    fn visit_block(&mut self, block: &'ast syn::Block) {
        let previous = self.block.replace(self.source.bytes(block.span()));
        visit::visit_block(self, block);
        self.block = previous;
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        self.insert(
            &[],
            item.sig.ident.unraw().to_string(),
            Binding::Declared(Declaration::Function),
        );
        visit::visit_item_fn(self, item);
    }

    fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
        if matches!(item.fields, syn::Fields::Unnamed(_)) {
            self.insert(
                &[],
                item.ident.unraw().to_string(),
                Binding::Declared(Declaration::Constructor),
            );
        }
    }

    fn visit_item_enum(&mut self, item: &'ast syn::ItemEnum) {
        let owner = [item.ident.unraw().to_string()];
        for variant in &item.variants {
            if matches!(variant.fields, syn::Fields::Unnamed(_)) {
                self.insert(
                    &owner,
                    variant.ident.unraw().to_string(),
                    Binding::Declared(Declaration::Constructor),
                );
            }
        }
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        if let syn::Type::Path(ty) = &*item.self_ty
            && ty.qself.is_none()
            && ty.path.segments.len() == 1
        {
            let owner = segments(&ty.path);
            for member in &item.items {
                if let syn::ImplItem::Fn(method) = member {
                    self.insert(
                        &owner,
                        method.sig.ident.unraw().to_string(),
                        Binding::Declared(Declaration::Function),
                    );
                }
            }
        }
        visit::visit_item_impl(self, item);
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        self.use_tree(&mut Vec::new(), &item.tree);
    }
}

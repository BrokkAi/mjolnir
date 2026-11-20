//! Prints a JSON index of a Rust source tree: every module file, the items it
//! defines with their line spans, its `use` imports, the paths it mentions,
//! and its tests. `go/port/portmap.py` turns the index into the Go port's
//! module graph, naming map and schedule.
//!
//! Usage: rsindex <crate-dir>...   (each directory holds a Cargo.toml)

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use proc_macro2::{TokenStream, TokenTree};
use serde::Serialize;
use syn::spanned::Spanned;
use syn::visit::Visit;

#[derive(Serialize, Default)]
struct FileIndex {
    /// Path relative to the working directory.
    path: String,
    /// Module path inside the target, `""` for the root.
    module: String,
    /// `lib`, `bin`, `test` (an integration test under `tests/`), `build`, `example`.
    target: String,
    /// `cfg` predicates on the `mod` declarations leading here.
    cfg: Vec<String>,
    /// Reached only through a `#[cfg(test)]` module declaration.
    test_only: bool,
    lines: usize,
    items: Vec<ItemIndex>,
    uses: Vec<UseIndex>,
    /// Distinct paths of two or more segments in production code.
    paths: Vec<String>,
    /// The same for code compiled only for tests.
    test_paths: Vec<String>,
    macros: Vec<String>,
    test_macros: Vec<String>,
    parse_error: Option<String>,
}

#[derive(Serialize, Default, Clone)]
struct ItemIndex {
    kind: String,
    name: String,
    /// Module path of the inline module that holds the item, relative to the file's module.
    inline_module: String,
    vis: String,
    start: usize,
    end: usize,
    cfg: Vec<String>,
    /// Inside a `#[cfg(test)]` inline module or itself `#[cfg(test)]`.
    test_only: bool,
    is_async: bool,
    is_unsafe: bool,
    /// `#[test]`, `#[tokio::test(...)]` and friends: the attribute text.
    test_attr: Option<String>,
    ignore: bool,
    should_panic: bool,
    derives: Vec<String>,
    serde: Vec<String>,
    /// For `impl` blocks: the implemented trait, if any.
    trait_: Option<String>,
    /// For methods and associated items: the `impl` or `trait` they belong to.
    owner: Option<String>,
    has_self: bool,
    generics: usize,
    /// For functions and methods: the signature as written.
    signature: Option<String>,
}

#[derive(Serialize, Clone)]
struct UseIndex {
    path: String,
    alias: Option<String>,
    glob: bool,
    vis: String,
    line: usize,
    test_only: bool,
}

fn tokens_text(tokens: &TokenStream) -> String {
    tokens.to_string().split_whitespace().collect::<Vec<_>>().join(" ")
}

fn path_text(path: &syn::Path) -> String {
    let mut out = String::new();
    if path.leading_colon.is_some() {
        out.push_str("::");
    }
    for (index, segment) in path.segments.iter().enumerate() {
        if index > 0 {
            out.push_str("::");
        }
        out.push_str(&segment.ident.to_string());
    }
    out
}

fn vis_text(vis: &syn::Visibility) -> String {
    match vis {
        syn::Visibility::Public(_) => "pub".to_owned(),
        syn::Visibility::Restricted(restricted) => {
            format!("pub({})", path_text(&restricted.path))
        }
        syn::Visibility::Inherited => String::new(),
    }
}

#[derive(Default, Clone)]
struct Attrs {
    cfg: Vec<String>,
    cfg_test: bool,
    test_attr: Option<String>,
    ignore: bool,
    should_panic: bool,
    derives: Vec<String>,
    serde: Vec<String>,
    path: Option<String>,
}

fn read_attrs(attrs: &[syn::Attribute]) -> Attrs {
    let mut out = Attrs::default();
    for attr in attrs {
        let name = path_text(attr.path());
        match name.as_str() {
            "cfg" => {
                if let syn::Meta::List(list) = &attr.meta {
                    let text = tokens_text(&list.tokens);
                    if text == "test" || text.starts_with("test ,") || text.contains("( test") || text.contains(", test") || text.contains("all (test") || text == "any (test , feature = \"test-support\")" {
                        // `not(test)` is production code.
                        if !text.contains("not (test") && !text.contains("not(test") {
                            out.cfg_test = true;
                        }
                    }
                    out.cfg.push(text);
                }
            }
            "test" => out.test_attr = Some("test".to_owned()),
            "ignore" => out.ignore = true,
            "should_panic" => out.should_panic = true,
            "path" => {
                if let syn::Meta::NameValue(pair) = &attr.meta {
                    if let syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(text), .. }) = &pair.value {
                        out.path = Some(text.value());
                    }
                }
            }
            "derive" => {
                if let syn::Meta::List(list) = &attr.meta {
                    for part in tokens_text(&list.tokens).split(',') {
                        let part = part.trim().replace(' ', "");
                        if !part.is_empty() {
                            out.derives.push(part);
                        }
                    }
                }
            }
            "serde" => {
                if let syn::Meta::List(list) = &attr.meta {
                    out.serde.push(tokens_text(&list.tokens));
                }
            }
            other => {
                if other.ends_with("::test") || other == "rstest" || other == "test_case" {
                    out.test_attr = Some(match &attr.meta {
                        syn::Meta::List(list) => format!("{other}({})", tokens_text(&list.tokens)),
                        _ => other.to_owned(),
                    });
                }
            }
        }
    }
    out
}

/// Collects paths from a macro's token stream, which syn leaves unparsed.
fn scan_tokens(tokens: TokenStream, paths: &mut BTreeSet<String>, macros: &mut BTreeSet<String>) {
    let trees: Vec<TokenTree> = tokens.into_iter().collect();
    let mut index = 0;
    while index < trees.len() {
        match &trees[index] {
            TokenTree::Group(group) => {
                scan_tokens(group.stream(), paths, macros);
                index += 1;
            }
            TokenTree::Ident(first) => {
                let mut text = first.to_string();
                let mut segments = 1;
                let mut next = index + 1;
                loop {
                    let colons = matches!(
                        (trees.get(next), trees.get(next + 1)),
                        (Some(TokenTree::Punct(a)), Some(TokenTree::Punct(b)))
                            if a.as_char() == ':' && b.as_char() == ':'
                    );
                    if !colons {
                        break;
                    }
                    let Some(TokenTree::Ident(ident)) = trees.get(next + 2) else {
                        break;
                    };
                    text.push_str("::");
                    text.push_str(&ident.to_string());
                    segments += 1;
                    next += 3;
                }
                if matches!(trees.get(next), Some(TokenTree::Punct(bang)) if bang.as_char() == '!')
                    && matches!(trees.get(next + 1), Some(TokenTree::Group(_)))
                {
                    macros.insert(text.clone());
                }
                if segments > 1 {
                    paths.insert(text);
                }
                index = next;
            }
            _ => index += 1,
        }
    }
}

struct Collector {
    items: Vec<ItemIndex>,
    uses: Vec<UseIndex>,
    paths: BTreeSet<String>,
    macros: BTreeSet<String>,
    test_paths: BTreeSet<String>,
    test_macros: BTreeSet<String>,
    /// `mod name;` declarations: (inline module path, name, attrs).
    child_modules: Vec<(String, String, Attrs)>,
    inline: Vec<String>,
    test_depth: usize,
    owner: Vec<String>,
}

impl Collector {
    fn inline_module(&self) -> String {
        self.inline.join("::")
    }

    fn push(&mut self, kind: &str, name: String, vis: String, span: proc_macro2::Span, attrs: &Attrs) -> usize {
        self.items.push(ItemIndex {
            kind: kind.to_owned(),
            name,
            inline_module: self.inline_module(),
            vis,
            start: span.start().line,
            end: span.end().line,
            cfg: attrs.cfg.clone(),
            test_only: self.test_depth > 0 || attrs.cfg_test,
            test_attr: attrs.test_attr.clone(),
            ignore: attrs.ignore,
            should_panic: attrs.should_panic,
            derives: attrs.derives.clone(),
            serde: attrs.serde.clone(),
            owner: self.owner.last().cloned(),
            ..ItemIndex::default()
        });
        self.items.len() - 1
    }

    fn use_tree(&mut self, prefix: &str, tree: &syn::UseTree, vis: &str, line: usize, test_only: bool) {
        let join = |name: &str| {
            if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}::{name}")
            }
        };
        match tree {
            syn::UseTree::Path(path) => {
                let next = join(&path.ident.to_string());
                self.use_tree(&next, &path.tree, vis, line, test_only);
            }
            syn::UseTree::Name(name) => {
                let ident = name.ident.to_string();
                let path = if ident == "self" { prefix.to_owned() } else { join(&ident) };
                self.uses.push(UseIndex { path, alias: None, glob: false, vis: vis.to_owned(), line, test_only });
            }
            syn::UseTree::Rename(rename) => {
                let ident = rename.ident.to_string();
                let path = if ident == "self" { prefix.to_owned() } else { join(&ident) };
                self.uses.push(UseIndex {
                    path,
                    alias: Some(rename.rename.to_string()),
                    glob: false,
                    vis: vis.to_owned(),
                    line,
                    test_only,
                });
            }
            syn::UseTree::Glob(_) => {
                self.uses.push(UseIndex { path: prefix.to_owned(), alias: None, glob: true, vis: vis.to_owned(), line, test_only });
            }
            syn::UseTree::Group(group) => {
                for item in &group.items {
                    self.use_tree(prefix, item, vis, line, test_only);
                }
            }
        }
    }
}

fn signature_text(signature: &syn::Signature) -> String {
    use syn::__private::ToTokens;
    tokens_text(&signature.to_token_stream())
}

fn type_name(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(path) => path_text(&path.path),
        syn::Type::Reference(reference) => type_name(&reference.elem),
        other => tokens_text(&quote_tokens(other)),
    }
}

fn quote_tokens<T: Spanned + quote_free::ToTokensLite>(value: &T) -> TokenStream {
    value.tokens()
}

/// `quote` is not a dependency; syn's own `ToTokens` comes through `syn::__private`.
mod quote_free {
    use proc_macro2::TokenStream;
    pub trait ToTokensLite {
        fn tokens(&self) -> TokenStream;
    }
    impl ToTokensLite for syn::Type {
        fn tokens(&self) -> TokenStream {
            use syn::__private::ToTokens;
            self.to_token_stream()
        }
    }
}

impl<'ast> Visit<'ast> for Collector {
    fn visit_path(&mut self, path: &'ast syn::Path) {
        if path.segments.len() > 1 {
            let text = path_text(path);
            if self.test_depth > 0 {
                self.test_paths.insert(text);
            } else {
                self.paths.insert(text);
            }
        }
        syn::visit::visit_path(self, path);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if self.test_depth > 0 {
            self.test_macros.insert(path_text(&mac.path));
            scan_tokens(mac.tokens.clone(), &mut self.test_paths, &mut self.test_macros);
        } else {
            self.macros.insert(path_text(&mac.path));
            scan_tokens(mac.tokens.clone(), &mut self.paths, &mut self.macros);
        }
        syn::visit::visit_macro(self, mac);
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        let attrs = read_attrs(&item.attrs);
        let test_only = self.test_depth > 0 || attrs.cfg_test;
        let prefix = if item.leading_colon.is_some() { "::" } else { "" };
        let mut collected = Vec::new();
        std::mem::swap(&mut collected, &mut self.uses);
        self.use_tree("", &item.tree, &vis_text(&item.vis), item.span().start().line, test_only);
        for entry in &mut self.uses {
            if !prefix.is_empty() {
                entry.path = format!("{prefix}{}", entry.path);
            }
        }
        collected.append(&mut self.uses);
        self.uses = collected;
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        let attrs = read_attrs(&item.attrs);
        let name = item.ident.to_string();
        if item.content.is_none() {
            self.child_modules.push((self.inline_module(), name, attrs));
            return;
        }
        self.push("mod", name.clone(), vis_text(&item.vis), item.span(), &attrs);
        self.inline.push(name);
        if attrs.cfg_test {
            self.test_depth += 1;
        }
        syn::visit::visit_item_mod(self, item);
        if attrs.cfg_test {
            self.test_depth -= 1;
        }
        self.inline.pop();
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        let attrs = read_attrs(&item.attrs);
        let index = self.push("fn", item.sig.ident.to_string(), vis_text(&item.vis), item.span(), &attrs);
        self.items[index].is_async = item.sig.asyncness.is_some();
        self.items[index].is_unsafe = item.sig.unsafety.is_some();
        self.items[index].generics = item.sig.generics.params.len();
        self.items[index].signature = Some(signature_text(&item.sig));
        let test_item = attrs.cfg_test || attrs.test_attr.is_some();
        if test_item {
            self.test_depth += 1;
        }
        syn::visit::visit_item_fn(self, item);
        if test_item {
            self.test_depth -= 1;
        }
    }

    fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
        let attrs = read_attrs(&item.attrs);
        let index = self.push("struct", item.ident.to_string(), vis_text(&item.vis), item.span(), &attrs);
        self.items[index].generics = item.generics.params.len();
        for field in &item.fields {
            let field_attrs = read_attrs(&field.attrs);
            self.items[index].serde.extend(field_attrs.serde.into_iter().map(|text| {
                format!("{}: {text}", field.ident.as_ref().map(ToString::to_string).unwrap_or_default())
            }));
        }
        let test_item = attrs.cfg_test || attrs.test_attr.is_some();
        if test_item {
            self.test_depth += 1;
        }
        syn::visit::visit_item_struct(self, item);
        if test_item {
            self.test_depth -= 1;
        }
    }

    fn visit_item_enum(&mut self, item: &'ast syn::ItemEnum) {
        let attrs = read_attrs(&item.attrs);
        let index = self.push("enum", item.ident.to_string(), vis_text(&item.vis), item.span(), &attrs);
        self.items[index].generics = item.generics.params.len();
        for variant in &item.variants {
            let variant_attrs = read_attrs(&variant.attrs);
            self.items[index]
                .serde
                .extend(variant_attrs.serde.into_iter().map(|text| format!("{}: {text}", variant.ident)));
        }
        let test_item = attrs.cfg_test || attrs.test_attr.is_some();
        if test_item {
            self.test_depth += 1;
        }
        syn::visit::visit_item_enum(self, item);
        if test_item {
            self.test_depth -= 1;
        }
    }

    fn visit_item_union(&mut self, item: &'ast syn::ItemUnion) {
        let attrs = read_attrs(&item.attrs);
        self.push("union", item.ident.to_string(), vis_text(&item.vis), item.span(), &attrs);
        syn::visit::visit_item_union(self, item);
    }

    fn visit_item_trait(&mut self, item: &'ast syn::ItemTrait) {
        let attrs = read_attrs(&item.attrs);
        let name = item.ident.to_string();
        let index = self.push("trait", name.clone(), vis_text(&item.vis), item.span(), &attrs);
        self.items[index].generics = item.generics.params.len();
        self.owner.push(name);
        syn::visit::visit_item_trait(self, item);
        self.owner.pop();
    }

    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        let attrs = read_attrs(&item.attrs);
        let index = self.push("trait_fn", item.sig.ident.to_string(), String::new(), item.span(), &attrs);
        self.items[index].is_async = item.sig.asyncness.is_some();
        self.items[index].has_self = item.sig.receiver().is_some();
        syn::visit::visit_trait_item_fn(self, item);
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        let attrs = read_attrs(&item.attrs);
        let name = type_name(&item.self_ty);
        let index = self.push("impl", name.clone(), String::new(), item.span(), &attrs);
        self.items[index].trait_ = item.trait_.as_ref().map(|(_, path, _)| path_text(path));
        self.items[index].generics = item.generics.params.len();
        self.owner.push(name);
        let cfg_test = attrs.cfg_test;
        if cfg_test {
            self.test_depth += 1;
        }
        syn::visit::visit_item_impl(self, item);
        if cfg_test {
            self.test_depth -= 1;
        }
        self.owner.pop();
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        let attrs = read_attrs(&item.attrs);
        let index = self.push("method", item.sig.ident.to_string(), vis_text(&item.vis), item.span(), &attrs);
        self.items[index].is_async = item.sig.asyncness.is_some();
        self.items[index].is_unsafe = item.sig.unsafety.is_some();
        self.items[index].has_self = item.sig.receiver().is_some();
        self.items[index].generics = item.sig.generics.params.len();
        self.items[index].signature = Some(signature_text(&item.sig));
        let test_item = attrs.cfg_test || attrs.test_attr.is_some();
        if test_item {
            self.test_depth += 1;
        }
        syn::visit::visit_impl_item_fn(self, item);
        if test_item {
            self.test_depth -= 1;
        }
    }

    fn visit_impl_item_const(&mut self, item: &'ast syn::ImplItemConst) {
        let attrs = read_attrs(&item.attrs);
        self.push("assoc_const", item.ident.to_string(), vis_text(&item.vis), item.span(), &attrs);
        syn::visit::visit_impl_item_const(self, item);
    }

    fn visit_item_const(&mut self, item: &'ast syn::ItemConst) {
        let attrs = read_attrs(&item.attrs);
        self.push("const", item.ident.to_string(), vis_text(&item.vis), item.span(), &attrs);
        let test_item = attrs.cfg_test || attrs.test_attr.is_some();
        if test_item {
            self.test_depth += 1;
        }
        syn::visit::visit_item_const(self, item);
        if test_item {
            self.test_depth -= 1;
        }
    }

    fn visit_item_static(&mut self, item: &'ast syn::ItemStatic) {
        let attrs = read_attrs(&item.attrs);
        self.push("static", item.ident.to_string(), vis_text(&item.vis), item.span(), &attrs);
        let test_item = attrs.cfg_test || attrs.test_attr.is_some();
        if test_item {
            self.test_depth += 1;
        }
        syn::visit::visit_item_static(self, item);
        if test_item {
            self.test_depth -= 1;
        }
    }

    fn visit_item_type(&mut self, item: &'ast syn::ItemType) {
        let attrs = read_attrs(&item.attrs);
        self.push("type", item.ident.to_string(), vis_text(&item.vis), item.span(), &attrs);
        let test_item = attrs.cfg_test || attrs.test_attr.is_some();
        if test_item {
            self.test_depth += 1;
        }
        syn::visit::visit_item_type(self, item);
        if test_item {
            self.test_depth -= 1;
        }
    }

    fn visit_item_macro(&mut self, item: &'ast syn::ItemMacro) {
        let attrs = read_attrs(&item.attrs);
        if let Some(ident) = &item.ident {
            self.push("macro_rules", ident.to_string(), String::new(), item.span(), &attrs);
        }
        syn::visit::visit_item_macro(self, item);
    }

    fn visit_expr_unsafe(&mut self, expr: &'ast syn::ExprUnsafe) {
        let attrs = Attrs::default();
        self.push("unsafe_block", String::new(), String::new(), expr.span(), &attrs);
        syn::visit::visit_expr_unsafe(self, expr);
    }
}

struct Walker {
    files: Vec<FileIndex>,
    seen: BTreeSet<PathBuf>,
}

impl Walker {
    fn file(&mut self, path: &Path, module: &str, target: &str, cfg: &[String], test_only: bool) {
        let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if !self.seen.insert(canonical) {
            return;
        }
        let mut index = FileIndex {
            path: path.to_string_lossy().replace('\\', "/"),
            module: module.to_owned(),
            target: target.to_owned(),
            cfg: cfg.to_vec(),
            test_only,
            ..FileIndex::default()
        };
        let source = match fs::read_to_string(path) {
            Ok(source) => source,
            Err(error) => {
                index.parse_error = Some(format!("read: {error}"));
                self.files.push(index);
                return;
            }
        };
        index.lines = source.lines().count();
        let parsed = match syn::parse_file(&source) {
            Ok(parsed) => parsed,
            Err(error) => {
                index.parse_error = Some(format!("parse: {error}"));
                self.files.push(index);
                return;
            }
        };
        let mut collector = Collector {
            items: Vec::new(),
            uses: Vec::new(),
            paths: BTreeSet::new(),
            macros: BTreeSet::new(),
            test_paths: BTreeSet::new(),
            test_macros: BTreeSet::new(),
            child_modules: Vec::new(),
            inline: Vec::new(),
            test_depth: usize::from(test_only),
            owner: Vec::new(),
        };
        collector.visit_file(&parsed);
        index.items = collector.items;
        index.uses = collector.uses;
        index.paths = collector.paths.into_iter().collect();
        index.macros = collector.macros.into_iter().collect();
        index.test_paths = collector.test_paths.into_iter().collect();
        index.test_macros = collector.test_macros.into_iter().collect();
        let children = collector.child_modules;
        self.files.push(index);

        // `foo.rs` keeps its children in `foo/`; `mod.rs`, `lib.rs` and `main.rs`
        // keep them beside themselves. Inline modules add a directory each.
        let stem = path.file_stem().and_then(|stem| stem.to_str()).unwrap_or_default();
        let parent = path.parent().unwrap_or(Path::new("."));
        let is_root_like = matches!(stem, "mod" | "lib" | "main") || module.is_empty();
        let base = if is_root_like { parent.to_path_buf() } else { parent.join(stem) };
        for (inline, name, attrs) in children {
            let mut directory = base.clone();
            for part in inline.split("::").filter(|part| !part.is_empty()) {
                directory = directory.join(part);
            }
            let candidates = match &attrs.path {
                Some(explicit) => {
                    let anchor = if inline.is_empty() { parent.to_path_buf() } else { directory.clone() };
                    vec![anchor.join(explicit)]
                }
                None => vec![directory.join(format!("{name}.rs")), directory.join(&name).join("mod.rs")],
            };
            let child_module = [module, inline.as_str(), name.as_str()]
                .iter()
                .filter(|part| !part.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join("::");
            let mut child_cfg = cfg.to_vec();
            child_cfg.extend(attrs.cfg.iter().cloned());
            match candidates.iter().find(|candidate| candidate.is_file()) {
                Some(found) => self.file(found, &child_module, target, &child_cfg, test_only || attrs.cfg_test),
                None => self.files.push(FileIndex {
                    path: candidates[0].to_string_lossy().replace('\\', "/"),
                    module: child_module,
                    target: target.to_owned(),
                    parse_error: Some("module file not found".to_owned()),
                    ..FileIndex::default()
                }),
            }
        }
    }
}

#[derive(Serialize)]
struct CrateIndex {
    dir: String,
    files: Vec<FileIndex>,
    /// `.rs` files under the crate that no module declaration reaches.
    unreached: Vec<String>,
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if name != "target" && !name.starts_with('.') {
                rust_files(&path, out);
            }
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
}

fn main() {
    let mut crates = Vec::new();
    for dir in std::env::args().skip(1) {
        let root = PathBuf::from(&dir);
        let mut walker = Walker { files: Vec::new(), seen: BTreeSet::new() };
        for (file, target) in [("src/lib.rs", "lib"), ("src/main.rs", "bin"), ("build.rs", "build")] {
            let path = root.join(file);
            if path.is_file() {
                walker.file(&path, "", target, &[], false);
            }
        }
        for (sub, target) in [("tests", "test"), ("examples", "example"), ("src/bin", "bin")] {
            let Ok(entries) = fs::read_dir(root.join(sub)) else { continue };
            let mut roots: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "rs"))
                .collect();
            roots.sort();
            for path in roots {
                let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
                // The module path names the target so two test binaries stay apart.
                walker.file(&path, "", &format!("{target}:{stem}"), &[], target == "test");
            }
        }
        let mut all = Vec::new();
        rust_files(&root, &mut all);
        all.sort();
        let unreached = all
            .into_iter()
            .filter(|path| !walker.seen.contains(&fs::canonicalize(path).unwrap_or_else(|_| path.clone())))
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .collect();
        crates.push(CrateIndex { dir, files: walker.files, unreached });
    }
    println!("{}", serde_json::to_string(&crates).expect("serialize index"));
}

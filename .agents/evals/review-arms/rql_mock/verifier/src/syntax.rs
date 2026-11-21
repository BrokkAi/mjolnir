use std::collections::BTreeSet;
use std::rc::Rc;

use proc_macro2::Span;
use syn::ext::IdentExt;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{BinOp, Block, Expr, FnArg, Pat, Signature, Stmt};

use crate::declarations::{Declaration, Declarations};
use crate::source::{Bytes, Source};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Kind {
    Return,
    Operand,
    Error,
    Panic,
    Raise,
    Callback,
    Argument,
    Input,
    Write,
    Call,
    Try,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Return | Self::Operand | Self::Try => "return",
            Self::Error => "error",
            Self::Panic => "panic",
            Self::Raise => "raise",
            Self::Callback => "callback",
            Self::Argument => "argument",
            Self::Input => "input",
            Self::Write => "write",
            Self::Call => "call",
        }
    }
}

pub struct Procedure {
    pub name: String,
    pub span: Bytes,
    pub body: Bytes,
}

pub struct Construct {
    pub kind: Kind,
    pub span: Bytes,
    pub anchor: usize,
    pub statement: Bytes,
    pub owner: Option<usize>,
    pub deferred: bool,
    pub name: Option<String>,
    pub indirect: bool,
}

pub struct Guard {
    pub conditions: Vec<Bytes>,
    pub scope: Bytes,
    pub branches: Vec<(Option<bool>, Bytes)>,
    pub match_arms: Rc<[Bytes]>,
    pub match_scrutinee: bool,
}

#[derive(Default)]
pub struct Syntax {
    pub procedures: Vec<Procedure>,
    pub constructs: Vec<Construct>,
    pub guards: Vec<Guard>,
    pub macro_invocations: BTreeSet<Bytes>,
    pub opaque: Vec<Bytes>,
    pub uncertain_callbacks: Vec<Bytes>,
    pub uncertain_constructors: Vec<Bytes>,
}

struct Scanner<'a> {
    source: &'a Source,
    syntax: Syntax,
    owner: Option<usize>,
    deferred: bool,
    statement: Option<Bytes>,
    formals: BTreeSet<String>,
    shadowed: BTreeSet<String>,
    indirect_names: BTreeSet<String>,
    wildcard_import: bool,
    declarations: &'a Declarations,
    inline_modules: Vec<String>,
    self_type: Option<syn::Path>,
}

pub fn scan(source: &Source, file: &syn::File, declarations: &Declarations) -> Syntax {
    let mut imports = Imports::default();
    imports.visit_file(file);
    let mut scanner = Scanner {
        source,
        syntax: Syntax::default(),
        owner: None,
        deferred: false,
        statement: None,
        formals: BTreeSet::new(),
        shadowed: BTreeSet::new(),
        indirect_names: imports.aliases,
        wildcard_import: imports.wildcard,
        declarations,
        inline_modules: Vec::new(),
        self_type: None,
    };
    scanner.visit_file(file);
    scanner.syntax
}

#[derive(Default)]
struct Bindings(BTreeSet<String>);

impl<'ast> Visit<'ast> for Bindings {
    fn visit_pat_ident(&mut self, pattern: &'ast syn::PatIdent) {
        self.0.insert(pattern.ident.unraw().to_string());
        visit::visit_pat_ident(self, pattern);
    }

    fn visit_item(&mut self, _: &'ast syn::Item) {}
}

#[derive(Default)]
struct Imports {
    aliases: BTreeSet<String>,
    wildcard: bool,
}

impl<'ast> Visit<'ast> for Imports {
    fn visit_use_rename(&mut self, rename: &'ast syn::UseRename) {
        self.aliases.insert(rename.rename.unraw().to_string());
    }

    fn visit_use_glob(&mut self, _: &'ast syn::UseGlob) {
        self.wildcard = true;
    }
}

fn upper_camel_case(name: &str) -> bool {
    name.starts_with(char::is_uppercase) && !name.contains('_')
}

fn call_path(expr: &Expr) -> Option<&syn::ExprPath> {
    match expr {
        Expr::Path(path) => Some(path),
        Expr::Paren(paren) => call_path(&paren.expr),
        Expr::Group(group) => call_path(&group.expr),
        _ => None,
    }
}

fn call_ident(expr: &Expr) -> Option<&syn::Ident> {
    Some(&call_path(expr)?.path.segments.last()?.ident)
}

fn formal_base(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Path(path) if path.qself.is_none() && path.path.leading_colon.is_none() => {
            Some(path.path.get_ident()?.unraw().to_string())
        }
        Expr::Field(field) => formal_base(&field.base),
        Expr::Paren(paren) => formal_base(&paren.expr),
        Expr::Group(group) => formal_base(&group.expr),
        _ => None,
    }
}

fn tail(block: &Block) -> Option<&Expr> {
    match block.stmts.last()? {
        Stmt::Expr(expr, None) => Some(expr),
        _ => None,
    }
}

fn tail_alternatives(expr: &Expr) -> Vec<&Expr> {
    match expr {
        Expr::Block(block) => tail(&block.block).into_iter().collect(),
        Expr::Unsafe(block) => tail(&block.block).into_iter().collect(),
        Expr::If(branch) => tail(&branch.then_branch)
            .into_iter()
            .chain(branch.else_branch.iter().map(|(_, expr)| &**expr))
            .collect(),
        Expr::Match(branch) => branch.arms.iter().map(|arm| &*arm.body).collect(),
        Expr::Paren(paren) => vec![&paren.expr],
        Expr::Group(group) => vec![&group.expr],
        _ => Vec::new(),
    }
}

fn compound_assignment(op: &BinOp) -> bool {
    matches!(
        op,
        BinOp::AddAssign(_)
            | BinOp::SubAssign(_)
            | BinOp::MulAssign(_)
            | BinOp::DivAssign(_)
            | BinOp::RemAssign(_)
            | BinOp::BitXorAssign(_)
            | BinOp::BitAndAssign(_)
            | BinOp::BitOrAssign(_)
            | BinOp::ShlAssign(_)
            | BinOp::ShrAssign(_)
    )
}

impl Scanner<'_> {
    fn add(&mut self, kind: Kind, node: &impl Spanned, name: Option<String>) {
        self.add_at(kind, node, node.span(), name);
    }

    fn add_at(&mut self, kind: Kind, node: &impl Spanned, anchor: Span, name: Option<String>) {
        let span = self.source.bytes(node.span());
        self.syntax.constructs.push(Construct {
            kind,
            span,
            anchor: self.source.bytes(anchor).start,
            statement: self.statement.unwrap_or(span),
            owner: self.owner,
            deferred: self.deferred,
            name,
            indirect: false,
        });
    }

    fn returned(&mut self, exit: &Expr, operand: Option<&Expr>) {
        let anchor = match exit {
            Expr::Return(ret) => ret.return_token.span,
            _ => exit.span(),
        };
        self.add_at(Kind::Return, exit, anchor, None);
        if let Some(operand) = operand {
            self.add(Kind::Operand, operand, None);
            self.returned_errors(operand);
        }
    }

    fn returned_errors(&mut self, operand: &Expr) {
        if let Expr::Call(call) = operand
            && let Some(callee) = call_ident(&call.func)
            && callee.unraw() == "Err"
        {
            self.add_at(Kind::Error, operand, callee.span(), None);
        }
        for alternative in tail_alternatives(operand) {
            self.returned_errors(alternative);
        }
    }

    fn block_tail(&mut self, block: &Block) {
        if let Some(Stmt::Expr(expr, None)) = block.stmts.last()
            && !matches!(expr, Expr::Return(_))
        {
            let previous = self.statement.replace(self.source.bytes(expr.span()));
            self.returned(expr, Some(expr));
            self.statement = previous;
        }
        // A trailing macro with braces is represented as Stmt::Macro. It can
        // expand to a statement or an expression, so its tail role is unknown.
    }

    fn procedure(&mut self, sig: &Signature, block: &Block, node: &impl Spanned) {
        let owner = self.owner.replace(self.syntax.procedures.len());
        let deferred = std::mem::replace(&mut self.deferred, false);
        let statement = self.statement.take();
        self.syntax.procedures.push(Procedure {
            name: sig.ident.unraw().to_string(),
            span: self.source.bytes(node.span()),
            body: self.source.bytes(block.span()),
        });

        let mut formals = Bindings::default();
        for arg in &sig.inputs {
            let anchor = match arg {
                FnArg::Receiver(receiver) => {
                    formals.0.insert("self".into());
                    receiver.self_token.span
                }
                FnArg::Typed(typed) => {
                    formals.visit_pat(&typed.pat);
                    typed.pat.span()
                }
            };
            self.add_at(Kind::Input, arg, anchor, None);
        }
        let mut locals = Bindings::default();
        locals.visit_block(block);
        let shadows = formals.0.intersection(&locals.0).cloned().collect();
        let old_formals = std::mem::replace(&mut self.formals, formals.0);
        let old_shadows = std::mem::replace(&mut self.shadowed, shadows);
        let mut names = self.indirect_names.clone();
        names.extend(locals.0);
        names.extend(self.formals.iter().cloned());
        let old_names = std::mem::replace(&mut self.indirect_names, names);
        self.block_tail(block);
        self.visit_block(block);
        self.indirect_names = old_names;
        self.formals = old_formals;
        self.shadowed = old_shadows;
        self.statement = statement;
        self.deferred = deferred;
        self.owner = owner;
    }

    fn condition(
        &mut self,
        condition: &impl Spanned,
        scope: &impl Spanned,
        branches: Vec<(Option<bool>, Bytes)>,
    ) {
        self.syntax.guards.push(Guard {
            conditions: vec![self.source.bytes(condition.span())],
            scope: self.source.bytes(scope.span()),
            branches,
            match_arms: Rc::default(),
            match_scrutinee: false,
        });
    }
}

impl<'ast> Visit<'ast> for Scanner<'_> {
    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        let self_type = match &*item.self_ty {
            syn::Type::Path(ty) if ty.qself.is_none() => Some(ty.path.clone()),
            _ => None,
        };
        let previous = std::mem::replace(&mut self.self_type, self_type);
        visit::visit_item_impl(self, item);
        self.self_type = previous;
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        self.inline_modules.push(item.ident.unraw().to_string());
        visit::visit_item_mod(self, item);
        self.inline_modules.pop();
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        self.procedure(&node.sig, &node.block, node);
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        self.procedure(&node.sig, &node.block, node);
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        if let Some(block) = &node.default {
            self.procedure(&node.sig, block, node);
        }
    }

    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        let previous = self.statement.replace(self.source.bytes(stmt.span()));
        visit::visit_stmt(self, stmt);
        self.statement = previous;
    }

    fn visit_expr_closure(&mut self, expr: &'ast syn::ExprClosure) {
        let deferred = std::mem::replace(&mut self.deferred, true);
        if let Expr::Block(block) = &*expr.body {
            self.block_tail(&block.block);
        } else if !matches!(&*expr.body, Expr::Return(_)) {
            self.returned(&expr.body, Some(&expr.body));
        }
        visit::visit_expr_closure(self, expr);
        self.deferred = deferred;
    }

    fn visit_expr_async(&mut self, expr: &'ast syn::ExprAsync) {
        let deferred = std::mem::replace(&mut self.deferred, true);
        self.block_tail(&expr.block);
        visit::visit_expr_async(self, expr);
        self.deferred = deferred;
    }

    fn visit_expr_const(&mut self, expr: &'ast syn::ExprConst) {
        let deferred = std::mem::replace(&mut self.deferred, true);
        self.block_tail(&expr.block);
        visit::visit_expr_const(self, expr);
        self.deferred = deferred;
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            Expr::Return(ret) => self.returned(expr, ret.expr.as_deref()),
            Expr::Try(propagation) => {
                let anchor = propagation.question_token.span;
                self.add_at(Kind::Try, expr, anchor, None);
                self.add_at(Kind::Error, expr, anchor, None);
            }
            Expr::Call(call) => {
                let callee = call_ident(&call.func);
                let name = callee.map(|ident| ident.unraw().to_string());
                let anchor = callee.map_or_else(|| call.func.span(), syn::Ident::span);
                let indirect = self.wildcard_import
                    || name
                        .as_ref()
                        .is_none_or(|name| self.indirect_names.contains(name))
                    || !matches!(&*call.func, Expr::Path(_));
                let declaration = call_path(&call.func).and_then(|callee| {
                    if callee.qself.is_some() {
                        return None;
                    }
                    if callee
                        .path
                        .get_ident()
                        .is_some_and(|name| self.formals.contains(&name.unraw().to_string()))
                    {
                        return Some(Declaration::Function);
                    }
                    self.declarations.resolve(
                        self.source,
                        &self.inline_modules,
                        self.source.bytes(anchor).start,
                        &callee.path,
                        self.self_type.as_ref(),
                    )
                });
                if declaration.is_none() && name.as_deref().is_some_and(upper_camel_case) {
                    let span = self.source.bytes(expr.span());
                    self.syntax.uncertain_constructors.push(span);
                    self.syntax.opaque.push(span);
                // The non_camel_case_types lint is the basis for the pilot's
                // override: unresolved lowercase callees remain required calls.
                } else if declaration != Some(Declaration::Constructor) {
                    self.add_at(Kind::Call, expr, anchor, name);
                    self.syntax
                        .constructs
                        .last_mut()
                        .expect("added call")
                        .indirect = indirect;
                    for argument in &call.args {
                        self.add(Kind::Argument, argument, None);
                    }
                }
                if let Some(base) = formal_base(&call.func)
                    && self.formals.contains(&base)
                {
                    if self.shadowed.contains(&base) {
                        self.syntax
                            .uncertain_callbacks
                            .push(self.source.bytes(expr.span()));
                    } else {
                        self.add_at(Kind::Callback, expr, anchor, None);
                    }
                }
            }
            Expr::MethodCall(call) => {
                let name = call.method.unraw().to_string();
                if matches!(name.as_str(), "unwrap" | "expect") {
                    self.add_at(Kind::Panic, expr, call.method.span(), None);
                }
                self.add_at(Kind::Call, expr, call.method.span(), Some(name));
                self.add(Kind::Argument, &call.receiver, None);
                for argument in &call.args {
                    self.add(Kind::Argument, argument, None);
                }
            }
            Expr::Assign(assign) => self.add(Kind::Write, &assign.left, None),
            Expr::Binary(binary) if compound_assignment(&binary.op) => {
                self.add(Kind::Write, &binary.left, None);
            }
            Expr::Verbatim(_) => self.syntax.opaque.push(self.source.bytes(expr.span())),
            _ => {}
        }
        visit::visit_expr(self, expr);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        let ident = &mac.path.segments.last().expect("macro name").ident;
        let name = ident.unraw().to_string();
        if matches!(
            name.as_str(),
            "panic" | "unreachable" | "todo" | "unimplemented"
        ) || name.starts_with("assert")
            || name.starts_with("debug_assert")
        {
            self.add_at(Kind::Panic, mac, ident.span(), None);
            self.add_at(Kind::Raise, mac, ident.span(), None);
        }
        let span = self.source.bytes(mac.span());
        self.syntax.macro_invocations.insert(span);
        self.syntax.opaque.push(span);
    }

    fn visit_expr_if(&mut self, expr: &'ast syn::ExprIf) {
        let mut branches = vec![(Some(true), self.source.bytes(expr.then_branch.span()))];
        if let Some((_, branch)) = &expr.else_branch {
            branches.push((Some(false), self.source.bytes(branch.span())));
        }
        self.condition(&expr.cond, expr, branches);
        visit::visit_expr_if(self, expr);
    }

    fn visit_expr_while(&mut self, expr: &'ast syn::ExprWhile) {
        let body = self.source.bytes(expr.body.span());
        self.condition(&expr.cond, expr, vec![(Some(true), body)]);
        visit::visit_expr_while(self, expr);
    }

    fn visit_expr_match(&mut self, expr: &'ast syn::ExprMatch) {
        let arms: Rc<[Bytes]> = expr
            .arms
            .iter()
            .map(|arm| self.source.bytes(arm.body.span()))
            .collect();
        self.syntax.guards.push(Guard {
            conditions: vec![self.source.bytes(expr.expr.span())],
            scope: self.source.bytes(expr.span()),
            branches: arms.iter().map(|&arm| (None, arm)).collect(),
            match_arms: Rc::default(),
            match_scrutinee: true,
        });
        for (index, arm) in expr.arms.iter().enumerate() {
            let body = arms[index];
            let pattern = self.source.bytes(arm.pat.span());
            let scope = self.source.bytes(expr.span());
            let mut guard = Guard {
                conditions: vec![pattern],
                scope,
                branches: vec![(Some(true), body)],
                match_arms: Rc::clone(&arms),
                match_scrutinee: false,
            };
            if let Some((_, condition)) = &arm.guard {
                // A pattern can match even when its if-guard rejects the arm.
                self.syntax.guards.push(Guard {
                    conditions: vec![pattern],
                    scope,
                    branches: vec![(None, body)],
                    match_arms: Rc::default(),
                    match_scrutinee: false,
                });
                guard.conditions = vec![self.source.bytes(condition.span())];
            }
            self.syntax.guards.push(guard);
        }
        visit::visit_expr_match(self, expr);
    }

    fn visit_local(&mut self, node: &'ast syn::Local) {
        if let Some(init) = &node.init
            && let Some((_, branch)) = &init.diverge
        {
            let branch = self.source.bytes(branch.span());
            let scope = self.source.bytes(node.span());
            let conditions = vec![
                self.source.bytes(init.expr.span()),
                self.source.bytes(node.pat.span()),
                Bytes {
                    start: self.source.bytes(node.pat.span()).start,
                    end: self.source.bytes(init.expr.span()).end,
                },
            ];
            self.syntax.guards.push(Guard {
                conditions,
                scope,
                branches: vec![(Some(false), branch)],
                match_arms: Rc::default(),
                match_scrutinee: false,
            });
        }
        visit::visit_local(self, node);
    }

    fn visit_pat(&mut self, node: &'ast Pat) {
        if let Pat::Verbatim(_) = node {
            self.syntax.opaque.push(self.source.bytes(node.span()));
        }
        visit::visit_pat(self, node);
    }
}

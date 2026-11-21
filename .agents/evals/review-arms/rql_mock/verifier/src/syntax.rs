use std::collections::BTreeSet;
use std::rc::Rc;

use syn::ext::IdentExt;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{BinOp, Block, Expr, FnArg, Pat, Signature, Stmt};

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
}

#[derive(Default)]
pub struct Syntax {
    pub procedures: Vec<Procedure>,
    pub constructs: Vec<Construct>,
    pub guards: Vec<Guard>,
    pub opaque: Vec<Bytes>,
    pub uncertain_callbacks: Vec<Bytes>,
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
}

pub fn scan(source: &mut Source) -> syn::Result<Syntax> {
    let file = syn::parse_file(&source.text)?;
    source.set_lexical_prefix(&file);
    let mut imports = Imports::default();
    imports.visit_file(&file);
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
    };
    scanner.visit_file(&file);
    Ok(scanner.syntax)
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

fn call_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Path(path) => Some(path.path.segments.last()?.ident.unraw().to_string()),
        Expr::Paren(paren) => call_name(&paren.expr),
        Expr::Group(group) => call_name(&group.expr),
        _ => None,
    }
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
        let span = self.source.bytes(node.span());
        self.syntax.constructs.push(Construct {
            kind,
            span,
            statement: self.statement.unwrap_or(span),
            owner: self.owner,
            deferred: self.deferred,
            name,
            indirect: false,
        });
    }

    fn returned(&mut self, exit: &Expr, operand: Option<&Expr>) {
        self.add(Kind::Return, exit, None);
        if let Some(operand) = operand {
            self.add(Kind::Operand, operand, None);
            self.returned_errors(operand);
        }
    }

    fn returned_errors(&mut self, operand: &Expr) {
        if let Expr::Call(call) = operand
            && call_name(&call.func).as_deref() == Some("Err")
        {
            self.add(Kind::Error, operand, None);
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
            self.add(Kind::Input, arg, None);
            match arg {
                FnArg::Receiver(_) => {
                    formals.0.insert("self".into());
                }
                FnArg::Typed(typed) => formals.visit_pat(&typed.pat),
            }
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
        });
    }
}

impl<'ast> Visit<'ast> for Scanner<'_> {
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
            Expr::Try(_) => {
                self.add(Kind::Try, expr, None);
                self.add(Kind::Error, expr, None);
            }
            Expr::Call(call) => {
                let name = call_name(&call.func);
                let indirect = self.wildcard_import
                    || name
                        .as_ref()
                        .is_none_or(|name| self.indirect_names.contains(name))
                    || !matches!(&*call.func, Expr::Path(_));
                self.add(Kind::Call, expr, name);
                self.syntax
                    .constructs
                    .last_mut()
                    .expect("added call")
                    .indirect = indirect;
                if let Some(base) = formal_base(&call.func)
                    && self.formals.contains(&base)
                {
                    if self.shadowed.contains(&base) {
                        self.syntax
                            .uncertain_callbacks
                            .push(self.source.bytes(expr.span()));
                    } else {
                        self.add(Kind::Callback, expr, None);
                    }
                }
                for argument in &call.args {
                    self.add(Kind::Argument, argument, None);
                }
            }
            Expr::MethodCall(call) => {
                let name = call.method.unraw().to_string();
                if matches!(name.as_str(), "unwrap" | "expect") {
                    self.add(Kind::Panic, expr, None);
                }
                self.add(Kind::Call, expr, Some(name));
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
        let name = mac
            .path
            .segments
            .last()
            .expect("macro name")
            .ident
            .to_string();
        if matches!(
            name.as_str(),
            "panic" | "unreachable" | "todo" | "unimplemented"
        ) || name.starts_with("assert")
            || name.starts_with("debug_assert")
        {
            self.add(Kind::Panic, mac, None);
            self.add(Kind::Raise, mac, None);
        }
        self.syntax.opaque.push(self.source.bytes(mac.span()));
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
        self.condition(
            &expr.expr,
            expr,
            arms.iter().map(|&arm| (None, arm)).collect(),
        );
        for (index, arm) in expr.arms.iter().enumerate() {
            let body = arms[index];
            let pattern = self.source.bytes(arm.pat.span());
            let scope = self.source.bytes(expr.span());
            let mut guard = Guard {
                conditions: vec![pattern],
                scope,
                branches: vec![(Some(true), body)],
                match_arms: Rc::clone(&arms),
            };
            if let Some((_, condition)) = &arm.guard {
                // A pattern can match even when its if-guard rejects the arm.
                self.syntax.guards.push(Guard {
                    conditions: vec![pattern],
                    scope,
                    branches: vec![(None, body)],
                    match_arms: Rc::default(),
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

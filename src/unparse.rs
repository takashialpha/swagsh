//! Turning commands back into source text, for `declare -f`.
//!
//! The output parses back to the same commands, though not always to the
//! same text: quoting is re-chosen (single quotes wherever nothing inside
//! expands), and layout follows one fixed style.

use std::fmt::Write as _;

use crate::ast::{
    AndOrList, AndOrOp, AssignValue, CaseClause, CaseEnd, Command, CondExpr, IfClause, Pipeline,
    Redirect, RedirectKind, SimpleCmd, TimeFormat, Word,
};
use crate::expand::shell_quote_always;

/// The definition of function `name`, laid out as bash prints it.
#[must_use]
pub fn function(name: &str, body: &Command) -> String {
    let mut p = Printer::default();
    p.out.push_str(name);
    p.out.push_str(" () ");
    p.newline();
    p.command(body);
    p.finish()
}

#[derive(Default)]
struct Printer {
    out: String,
    indent: usize,
    /// Here-document bodies to write once the current line ends.
    pending: Vec<(String, String)>,
}

impl Printer {
    fn finish(mut self) -> String {
        self.flush_heredocs();
        while self.out.ends_with([' ', '\n']) {
            self.out.pop();
        }
        self.out
    }

    fn newline(&mut self) {
        self.out.push('\n');
        self.flush_heredocs();
        for _ in 0..self.indent {
            self.out.push_str("    ");
        }
    }

    fn flush_heredocs(&mut self) {
        for (body, delim) in std::mem::take(&mut self.pending) {
            self.out.push_str(&body);
            if !body.is_empty() && !body.ends_with('\n') {
                self.out.push('\n');
            }
            self.out.push_str(&delim);
            self.out.push('\n');
        }
    }

    /// A list of commands, one per line, at one deeper indent.
    fn block(&mut self, list: &[AndOrList]) {
        self.indent += 1;
        for aol in list {
            self.newline();
            self.and_or(aol);
        }
        self.indent -= 1;
        self.newline();
    }

    /// A list on one line, as in a condition: `a; b`.
    fn inline(&mut self, list: &[AndOrList]) {
        for (i, aol) in list.iter().enumerate() {
            if i > 0 {
                self.out.push_str("; ");
            }
            self.and_or(aol);
        }
    }

    fn and_or(&mut self, aol: &AndOrList) {
        for item in &aol.items {
            self.pipeline(&item.command);
            match item.op {
                Some(AndOrOp::And) => self.out.push_str(" && "),
                Some(AndOrOp::Or) => self.out.push_str(" || "),
                None => {}
            }
        }
        if aol.is_async {
            self.out.push_str(" &");
        }
    }

    fn pipeline(&mut self, pipeline: &Pipeline) {
        match pipeline.timed {
            Some(TimeFormat::Default) => self.out.push_str("time "),
            Some(TimeFormat::Posix) => self.out.push_str("time -p "),
            None => {}
        }
        if pipeline.negated {
            self.out.push_str("! ");
        }
        for (i, command) in pipeline.commands.iter().enumerate() {
            if i > 0 {
                self.out.push_str(" | ");
            }
            self.command(command);
        }
    }

    fn command(&mut self, command: &Command) {
        match command {
            Command::Simple(sc) => self.simple(sc),
            Command::Pipeline(p) => self.pipeline(p),
            Command::If(ic) => self.if_clause(ic),
            Command::For(fc) | Command::Select(fc) => {
                self.out.push_str(if matches!(command, Command::For(_)) {
                    "for "
                } else {
                    "select "
                });
                self.out.push_str(&fc.var);
                self.out.push_str(" in");
                for item in &fc.items {
                    self.out.push(' ');
                    let text = word(item);
                    self.out.push_str(&text);
                }
                self.out.push_str("; do");
                self.block(&fc.body);
                self.out.push_str("done");
            }
            Command::ArithFor(fc) => {
                let _ = write!(
                    self.out,
                    "for (({}; {}; {})); do",
                    fc.init.trim(),
                    fc.condition.trim(),
                    fc.step.trim()
                );
                self.block(&fc.body);
                self.out.push_str("done");
            }
            Command::While(wc) => {
                self.out
                    .push_str(if wc.until { "until " } else { "while " });
                self.inline(&wc.condition);
                self.out.push_str("; do");
                self.block(&wc.body);
                self.out.push_str("done");
            }
            Command::Case(cc) => self.case_clause(cc),
            Command::FunctionDef(fd) => {
                self.out.push_str(&fd.name);
                self.out.push_str(" () ");
                self.newline();
                self.command(&fd.body);
            }
            Command::Group(gc) => {
                self.out.push_str(if gc.subshell { "(" } else { "{ " });
                self.block(&gc.body);
                self.out.push_str(if gc.subshell { ")" } else { "}" });
            }
            Command::Redirected(inner, redirects) => {
                self.command(inner);
                self.redirects(redirects);
            }
            Command::Arith(expr) => {
                self.out.push_str("((");
                self.out.push_str(expr);
                self.out.push_str("))");
            }
            Command::Coproc(name, body) => {
                let _ = write!(self.out, "coproc {name} ");
                self.command(body);
            }
            Command::Cond(expr) => {
                self.out.push_str("[[ ");
                self.out.push_str(&cond(expr));
                self.out.push_str(" ]]");
            }
        }
    }

    fn if_clause(&mut self, ic: &IfClause) {
        self.out.push_str("if ");
        self.inline(&ic.condition);
        self.out.push_str("; then");
        self.block(&ic.then_body);
        for (condition, body) in &ic.elif_clauses {
            self.out.push_str("elif ");
            self.inline(condition);
            self.out.push_str("; then");
            self.block(body);
        }
        if let Some(body) = &ic.else_body {
            self.out.push_str("else");
            self.block(body);
        }
        self.out.push_str("fi");
    }

    fn case_clause(&mut self, cc: &CaseClause) {
        self.out.push_str("case ");
        self.out.push_str(&word(&cc.word));
        self.out.push_str(" in");
        self.indent += 1;
        for arm in &cc.arms {
            self.newline();
            let patterns: Vec<String> = arm.patterns.iter().map(word).collect();
            self.out.push_str(&patterns.join(" | "));
            self.out.push(')');
            self.block(&arm.body);
            self.out.push_str(match arm.end {
                CaseEnd::Break => ";;",
                CaseEnd::FallThrough => ";&",
                CaseEnd::Continue => ";;&",
            });
        }
        self.indent -= 1;
        self.newline();
        self.out.push_str("esac");
    }

    fn simple(&mut self, sc: &SimpleCmd) {
        let words: Vec<String> = sc.words.iter().map(word).collect();
        self.out.push_str(&words.join(" "));
        self.redirects(&sc.redirects);
    }

    fn redirects(&mut self, redirects: &[Redirect]) {
        for r in redirects {
            let target = word(&r.target);
            let op = match &r.kind {
                RedirectKind::Out => format!("{}>", fd_prefix(r.fd, 1)),
                RedirectKind::Clobber => format!("{}>|", fd_prefix(r.fd, 1)),
                RedirectKind::Append => format!("{}>>", fd_prefix(r.fd, 1)),
                RedirectKind::In => format!("{}<", fd_prefix(r.fd, 0)),
                RedirectKind::ReadWrite => format!("{}<>", fd_prefix(r.fd, 0)),
                RedirectKind::Dup if r.fd == 0 => "<&".to_owned(),
                RedirectKind::Dup => format!("{}>&", fd_prefix(r.fd, 1)),
                RedirectKind::Both => "&>".to_owned(),
                RedirectKind::HereString => format!("{}<<<", fd_prefix(r.fd, 0)),
                RedirectKind::HereDoc { raw_body, quoted } => {
                    let delim = heredoc_delimiter(raw_body);
                    let shown = if *quoted {
                        format!("'{delim}'")
                    } else {
                        delim.clone()
                    };
                    let _ = write!(self.out, " {}<<{shown}", fd_prefix(r.fd, 0));
                    self.pending.push((raw_body.clone(), delim));
                    continue;
                }
            };
            self.out.push(' ');
            self.out.push_str(&op);
            self.out.push_str(&target);
        }
    }
}

/// `fd` before a redirection operator, left out when it is the default.
fn fd_prefix(fd: i32, default: i32) -> String {
    if fd == default {
        String::new()
    } else {
        fd.to_string()
    }
}

/// A here-document delimiter that no line of `body` equals.
fn heredoc_delimiter(body: &str) -> String {
    let mut delim = "EOF".to_owned();
    while body.lines().any(|l| l == delim) {
        delim.push('_');
    }
    delim
}

/// A word as source text.
fn word(w: &Word) -> String {
    match w {
        Word::Literal(s) => {
            if s.is_empty() {
                "''".to_owned()
            } else if s.contains(|c: char| " \t\n;&|<>()'\"\\$`".contains(c)) {
                shell_quote_always(s)
            } else {
                s.clone()
            }
        }
        Word::Quoted(inner) if !expands(inner) => shell_quote_always(&literal_text(inner)),
        Word::Quoted(inner) => format!("\"{}\"", in_double_quotes(inner)),
        Word::Compound(parts) => parts.iter().map(word).collect(),
        other => expansion(other),
    }
}

/// `$name`, `$(cmd)` or `$((expr))` as source text.
fn expansion(w: &Word) -> String {
    match w {
        Word::Var(name) => format!("${{{name}}}"),
        Word::Arith(expr) => format!("$(({expr}))"),
        Word::CmdSub(cmd) => {
            let mut p = Printer::default();
            p.command(cmd);
            format!("$({})", p.finish())
        }
        Word::ProcSub { cmd, output } => {
            let mut p = Printer::default();
            p.command(cmd);
            format!("{}({})", if *output { '>' } else { '<' }, p.finish())
        }
        Word::Assign(assign) => {
            let mut out = assign.name.clone();
            if let Some(index) = &assign.index {
                out.push('[');
                out.push_str(&word(index));
                out.push(']');
            }
            out.push_str(if assign.append { "+=" } else { "=" });
            match &assign.value {
                AssignValue::Scalar(value) => out.push_str(&word(value)),
                AssignValue::Array(items) => {
                    let items: Vec<String> = items
                        .iter()
                        .map(|item| {
                            item.index.as_ref().map_or_else(
                                || word(&item.value),
                                |index| {
                                    format!(
                                        "[{}]{}{}",
                                        word(index),
                                        if item.append { "+=" } else { "=" },
                                        word(&item.value)
                                    )
                                },
                            )
                        })
                        .collect();
                    out.push('(');
                    out.push_str(&items.join(" "));
                    out.push(')');
                }
            }
            out
        }
        other => word(other),
    }
}

fn expands(w: &Word) -> bool {
    match w {
        Word::Literal(_) => false,
        Word::Compound(parts) => parts.iter().any(expands),
        Word::Quoted(inner) => expands(inner),
        _ => true,
    }
}

fn literal_text(w: &Word) -> String {
    match w {
        Word::Literal(s) => s.clone(),
        Word::Compound(parts) => parts.iter().map(literal_text).collect(),
        Word::Quoted(inner) => literal_text(inner),
        _ => String::new(),
    }
}

/// The inside of a double-quoted word.
fn in_double_quotes(w: &Word) -> String {
    match w {
        Word::Literal(s) => {
            let mut out = String::with_capacity(s.len());
            for c in s.chars() {
                if matches!(c, '$' | '`' | '"' | '\\') {
                    out.push('\\');
                }
                out.push(c);
            }
            out
        }
        Word::Compound(parts) => parts.iter().map(in_double_quotes).collect(),
        Word::Quoted(inner) => in_double_quotes(inner),
        other => expansion(other),
    }
}

/// A `[[ ... ]]` expression as source text.
fn cond(expr: &CondExpr) -> String {
    match expr {
        CondExpr::Word(w) => word(w),
        CondExpr::Unary(op, w) => format!("{op} {}", word(w)),
        CondExpr::Binary(a, op, b) => format!("{} {op} {}", word(a), word(b)),
        CondExpr::Not(inner) => format!("! {}", cond(inner)),
        CondExpr::And(a, b) => format!("{} && {}", cond_operand(a), cond_operand(b)),
        CondExpr::Or(a, b) => format!("{} || {}", cond_operand(a), cond_operand(b)),
    }
}

/// An operand of `&&` or `||`, parenthesized when it is itself one.
fn cond_operand(expr: &CondExpr) -> String {
    match expr {
        CondExpr::And(..) | CondExpr::Or(..) => format!("( {} )", cond(expr)),
        _ => cond(expr),
    }
}

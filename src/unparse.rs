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
use crate::expand::{is_name, shell_quote_always};

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
    fn finish(self) -> String {
        self.finish_with_heredoc_end().0
    }

    /// The text, and whether it ends with a here-document's delimiter line,
    /// after which a newline must come.
    fn finish_with_heredoc_end(mut self) -> (String, bool) {
        let heredoc_end = !self.pending.is_empty();
        if heredoc_end {
            self.out.push('\n');
            self.flush_heredocs();
        }
        while self.out.ends_with([' ', '\n']) {
            self.out.pop();
        }
        (self.out, heredoc_end)
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
            // Only a compound command can follow a name: before a simple
            // one, the name would be read as the command.
            Command::Coproc(_, body) if matches!(**body, Command::Simple(_)) => {
                self.out.push_str("coproc ");
                self.command(body);
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
        let mut words: Vec<String> = sc.words.iter().map(word).collect();
        // A command named like a reserved word was quoted to be one.
        if let Some(first) = words.first_mut()
            && RESERVED.contains(&first.as_str())
        {
            *first = shell_quote_always(first);
        }
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

/// The words that are reserved at the start of a command.
const RESERVED: [&str; 22] = [
    "!", "[[", "]]", "{", "}", "case", "coproc", "do", "done", "elif", "else", "esac", "fi", "for",
    "function", "if", "in", "select", "then", "time", "until", "while",
];

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
    let text = WordPrinter::print(w, Extra::Nothing);
    // `name[` with no `]` to close it reads on past the end of the word,
    // as the subscript of an assignment if a `]=` comes later on the line.
    // Quoting the name stops that, and changes nothing else.
    let left_open = text
        .split_once('[')
        .is_some_and(|(name, rest)| is_name(name) && subscript_end(rest).is_none());
    if left_open {
        WordPrinter::print(w, Extra::Name)
    } else {
        text
    }
}

/// An assignment's subscript as source text, `[index]`.
fn subscript(index: &Word) -> String {
    let text = word(index);
    // The lexer counts only the brackets outside quotes to find where the
    // subscript ends, and reads the word as no assignment at all if it ends
    // anywhere else. Quoting every bracket of the index leaves none to count.
    if subscript_end(&format!("{text}]")) == Some(text.len()) {
        format!("[{text}]")
    } else {
        format!("[{}]", WordPrinter::print(index, Extra::Brackets))
    }
}

/// Where the `]` closing a subscript is in `rest`, the text after its `[`,
/// by the same count the lexer makes when it looks for a subscript
/// assignment.
fn subscript_end(rest: &str) -> Option<usize> {
    let bytes = rest.as_bytes();
    let mut depth = 1usize;
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        match b {
            b'\\' => i += 1,
            b'\'' | b'"' => {
                i += 1;
                while let Some(&c) = bytes.get(i) {
                    if c == b {
                        break;
                    }
                    if c == b'\\' && b == b'"' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// What a word is printed with quoted beyond what its own text needs, so
/// that the lexer reads it back as the same word.
#[derive(Default, Clone, Copy)]
enum Extra {
    #[default]
    Nothing,
    /// The literal text before the word's first `[` (see [`word`]).
    Name,
    /// Every literal `[` and `]` (see [`subscript`]).
    Brackets,
}

/// Prints a word, putting its neighbouring single-quoted parts in one pair
/// of quotes.
#[derive(Default)]
struct WordPrinter {
    out: String,
    /// Text waiting to be written in single quotes.
    quoted: Option<String>,
    /// What is still to be quoted beyond what the text needs.
    extra: Extra,
}

impl WordPrinter {
    fn print(w: &Word, extra: Extra) -> String {
        let mut out = Self {
            extra,
            ..Self::default()
        };
        out.word(w);
        out.flush();
        out.out
    }

    fn word(&mut self, w: &Word) {
        if let Some(text) = single_quoted(w, self.at_start()) {
            self.quoted.get_or_insert_default().push_str(&text);
            return;
        }
        match w {
            // A `{` is quoted so that the text around it cannot make a
            // brace expansion that the word did not have.
            Word::Literal(s) => {
                for (i, piece) in s.split('{').enumerate() {
                    if i > 0 {
                        self.quoted.get_or_insert_default().push('{');
                    }
                    if !piece.is_empty() {
                        self.literal(piece);
                    }
                }
            }
            Word::Quoted(inner) => self.bare(&format!("\"{}\"", in_double_quotes(inner))),
            Word::Compound(parts) => {
                for part in parts {
                    self.word(part);
                }
            }
            other => self.bare(&expansion(other)),
        }
    }

    /// Whether nothing of the word is printed yet.
    const fn at_start(&self) -> bool {
        self.out.is_empty() && self.quoted.is_none()
    }

    /// Prints literal text bare, apart from what [`Extra`] quotes of it.
    fn literal(&mut self, piece: &str) {
        match self.extra {
            Extra::Nothing => self.bare(piece),
            Extra::Name => {
                let Some((name, rest)) = piece.split_once('[') else {
                    return self.quoted.get_or_insert_default().push_str(piece);
                };
                self.extra = Extra::Nothing;
                self.quoted.get_or_insert_default().push_str(name);
                self.bare(&format!("[{rest}"));
            }
            Extra::Brackets => {
                let mut last = 0;
                for (i, bracket) in piece.match_indices(['[', ']']) {
                    if i > last {
                        self.bare(&piece[last..i]);
                    }
                    self.quoted.get_or_insert_default().push_str(bracket);
                    last = i + bracket.len();
                }
                if last < piece.len() {
                    self.bare(&piece[last..]);
                }
            }
        }
    }

    fn bare(&mut self, text: &str) {
        self.flush();
        self.out.push_str(text);
    }

    fn flush(&mut self) {
        if let Some(text) = self.quoted.take() {
            self.out.push_str(&shell_quote_always(&text));
        }
    }
}

/// The text of `w`, when it is printed in single quotes; `at_start` says
/// whether `w` begins the word.
fn single_quoted(w: &Word, at_start: bool) -> Option<String> {
    match w {
        // A `#` beginning the word is quoted so that it cannot start a
        // comment; anywhere else it is plain text.
        Word::Literal(s)
            if s.is_empty()
                || (at_start && s.starts_with('#'))
                || s.contains(|c: char| " \t\r\n;&|<>()'\"\\$`\x01\x02\x03".contains(c)) =>
        {
            Some(s.clone())
        }
        Word::Quoted(inner) if !expands(inner) => Some(literal_text(inner)),
        _ => None,
    }
}

/// `$name`, `$(cmd)` or `$((expr))` as source text.
fn expansion(w: &Word) -> String {
    match w {
        Word::Var(name) => format!("${{{name}}}"),
        Word::Arith(expr) => format!("$(({expr}))"),
        Word::CmdSub(cmd) => format!("$({})", substitution(cmd)),
        Word::ProcSub { cmd, output } => {
            format!("{}({})", if *output { '>' } else { '<' }, substitution(cmd))
        }
        Word::Assign(assign) => {
            let mut out = assign.name.clone();
            if let Some(index) = &assign.index {
                out.push_str(&subscript(index));
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

/// The inside of a `$(...)`, `<(...)` or `>(...)`: a subshell is set off
/// by a space, so that `$( (...))` does not read as arithmetic.
fn substitution(cmd: &Command) -> String {
    let mut p = Printer::default();
    p.command(cmd);
    let (mut body, heredoc_end) = p.finish_with_heredoc_end();
    if heredoc_end {
        body.push('\n');
    }
    if body.starts_with('(') {
        body.insert(0, ' ');
    }
    body
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

#[cfg(test)]
mod tests {
    use crate::ast::{Command, Program};

    /// The definition of `f` printed back, if `src` parses to it.
    fn unparse(src: &str) -> Option<String> {
        let Program { body } = crate::parser::parse(src).ok()?;
        let [list] = body.as_slice() else { return None };
        let [item] = list.items.as_slice() else {
            return None;
        };
        let [Command::FunctionDef(def)] = item.command.commands.as_slice() else {
            return None;
        };
        Some(super::function(&def.name, &def.body))
    }

    /// Prints `f` with `body`, and checks the printed text prints the same
    /// once parsed again.
    fn round_trip(body: &str) -> Option<String> {
        let first = unparse(&format!("f() {{\n{body}\n}}\n"))?;
        assert_eq!(unparse(&first).as_deref(), Some(first.as_str()));
        Some(first)
    }

    // Found by the `unparse` fuzz target: brace expansion left a word
    // starting with `#`, which printed bare read back as a comment.
    #[test]
    fn leading_hash_is_quoted() {
        assert_eq!(
            round_trip("echo {,#}x").as_deref(),
            Some("f () \n{ \n    echo x '#x'\n}")
        );
    }

    // Found by the `unparse` fuzz target once the case above was fixed:
    // `{#` printed as `'{'#`, which read back as two parts printed `'{#'`.
    #[test]
    fn hash_after_brace_stays_bare() {
        assert_eq!(
            round_trip("echo {#").as_deref(),
            Some("f () \n{ \n    echo '{'#\n}")
        );
    }

    // Found by the `unparse` fuzz target: brace expansion left a word `h[`,
    // which printed bare took the next word into an assignment subscript.
    #[test]
    fn open_subscript_name_is_quoted() {
        assert_eq!(
            round_trip("echo {h[,x]=1}").as_deref(),
            Some("f () \n{ \n    echo 'h'[ x]=1\n}")
        );
    }

    // Found by the `unparse` fuzz target: the subscript `[\x01$]` printed
    // its `]` in quotes, so the word read back was no assignment.
    #[test]
    fn subscript_brackets_are_quoted_when_unbalanced() {
        assert_eq!(
            round_trip("P[[\x01$]]=").as_deref(),
            Some("f () \n{ \n    P['[\x01$]']=''\n}")
        );
    }

    #[test]
    fn closed_subscript_stays_bare() {
        assert_eq!(
            round_trip("echo a[b] h[x]=1").as_deref(),
            Some("f () \n{ \n    echo a[b] h[x]=1\n}")
        );
    }

    #[test]
    fn inner_hash_stays_bare() {
        assert_eq!(
            round_trip("echo a#b").as_deref(),
            Some("f () \n{ \n    echo a#b\n}")
        );
    }
}

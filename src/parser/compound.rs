use crate::ast::{
    ArithForClause, CaseArm, CaseClause, CaseEnd, Command, CondExpr, ForClause, FunctionDef,
    GroupCmd, IfClause, WhileClause, Word,
};
use crate::expand::find_unnested;
use crate::lexer::{CondToken, Token};

use super::{ParseResult, Parser, Spanned, keyword_text};

impl Parser<'_> {
    pub(super) fn parse_if(&mut self) -> ParseResult<Command> {
        self.expect(&Token::If)?;
        let condition = self.parse_list()?;
        self.expect(&Token::Then)?;
        let then_body = self.parse_list()?;

        let mut elif_clauses = Vec::new();
        let mut else_body = None;

        loop {
            match self.peek() {
                Token::Elif => {
                    self.advance();
                    let elif_cond = self.parse_list()?;
                    self.expect(&Token::Then)?;
                    let elif_body = self.parse_list()?;
                    elif_clauses.push((elif_cond, elif_body));
                }
                Token::Else => {
                    self.advance();
                    else_body = Some(self.parse_list()?);
                    break;
                }
                _ => break,
            }
        }

        self.expect(&Token::Fi)?;

        Ok(Command::If(IfClause {
            condition,
            then_body,
            elif_clauses,
            else_body,
        }))
    }

    pub(super) fn parse_for(&mut self) -> ParseResult<Command> {
        self.expect(&Token::For)?;

        if let Token::ArithCmd(text) = self.peek().clone() {
            self.advance();
            return self.parse_arith_for(&text);
        }

        let var = match self.advance().clone() {
            Token::Word(name) => name,
            ref other if let Some(text) = keyword_text(other) => text.to_owned(),
            other => {
                return Err(self.err(format!("expected variable name after `for`, got `{other}`")));
            }
        };

        let (items, body) = self.parse_for_rest()?;
        Ok(Command::For(ForClause { var, items, body }))
    }

    /// What follows a `for` or `select` loop's name: `in words` if there,
    /// then the `do ... done` body.
    fn parse_for_rest(&mut self) -> ParseResult<(Vec<Word>, Vec<crate::ast::AndOrList>)> {
        self.skip_newlines();

        let items = if self.eat(&Token::In) {
            let mut words = Vec::new();
            while let Token::Word(w) = self.peek().clone() {
                self.push_words(w, &mut words)?;
                self.advance();
            }
            words
        } else {
            vec![Word::Var("@".to_owned())]
        };

        self.skip_newlines();
        self.eat(&Token::Semi);
        self.skip_newlines();
        self.expect(&Token::Do)?;
        let body = self.parse_list()?;
        self.expect(&Token::Done)?;
        Ok((items, body))
    }

    /// The rest of `for (( init; condition; step ))`, from its body.
    // Kept out of line: rare, and inlined it bloats the hot parser
    // frames that call it.
    #[inline(never)]
    fn parse_arith_for(&mut self, text: &str) -> ParseResult<Command> {
        let mut parts = Vec::with_capacity(3);
        let mut rest = text;
        while let Some(i) = find_unnested(rest, |c| c == ';') {
            parts.push(rest[..i].to_owned());
            rest = &rest[i + 1..];
        }
        parts.push(rest.to_owned());
        let [init, condition, step] = <[String; 3]>::try_from(parts)
            .map_err(|_| self.err("expected `for (( init; condition; step ))`"))?;
        self.skip_newlines();
        self.eat(&Token::Semi);
        self.skip_newlines();
        self.expect(&Token::Do)?;
        let body = self.parse_list()?;
        self.expect(&Token::Done)?;
        Ok(Command::ArithFor(Box::new(ArithForClause {
            init,
            condition,
            step,
            body,
        })))
    }

    /// `[[ expression ]]`, from its `[[`.
    ///
    /// The expression has a grammar of its own, in which `(`, `<` and the
    /// like are not the shell's operators, so whatever was already lexed
    /// past the `[[` is dropped and read again in that grammar.
    // Kept out of line: rare, and inlined it bloats the hot parser
    // frames that call it.
    #[inline(never)]
    pub(super) fn parse_cond(&mut self) -> ParseResult<Command> {
        let Spanned { line, col, end, .. } = self.tokens[self.pos];
        self.tokens.truncate(self.pos + 1);
        self.lexed_eof = false;
        if self.lex_error.as_ref().is_some_and(|(i, _)| *i > self.pos) {
            self.lex_error = None;
        }
        let pos = self.pos;
        self.unfilled_heredocs.retain(|&i| i <= pos);
        self.lexer.rewind(end, line, col + 2);
        // `pos` stays on the `[[` until the expression is read, so that an
        // error in it has a token to point at.
        let mut cond = CondParser {
            parser: self,
            peeked: Vec::new(),
        };
        let expr = cond.or()?;
        match cond.next()? {
            CondToken::End => {}
            other => {
                return Err(cond
                    .parser
                    .err(format!("unexpected `{}` in `[[`", cond_text(&other))));
            }
        }
        self.pos += 1;
        self.fill();
        Ok(Command::Cond(Box::new(expr)))
    }

    /// `coproc [name] command`. A name is only taken when a compound
    /// command follows it; otherwise the word is the command's own name
    /// and the coprocess is `COPROC`.
    // Kept out of line: rare, and inlined it bloats the hot parser
    // frames that call it.
    #[inline(never)]
    pub(super) fn parse_coproc(&mut self) -> ParseResult<Command> {
        self.advance();
        let named = matches!(self.peek(), Token::Word(w) if crate::expand::is_name(w))
            && matches!(
                self.tokens.get(self.pos + 1).map(|s| &s.token),
                Some(
                    Token::LBrace
                        | Token::LParen
                        | Token::If
                        | Token::For
                        | Token::While
                        | Token::Until
                        | Token::Case
                        | Token::ArithCmd(_)
                )
            );
        let name = if named {
            let Token::Word(name) = self.advance().clone() else {
                return Err(self.err("expected a name after `coproc`"));
            };
            name
        } else {
            "COPROC".to_owned()
        };
        let body = self.parse_command()?;
        Ok(Command::Coproc(name, Box::new(body)))
    }

    /// Whether the `select` here starts a `select` loop: a name follows it.
    pub(super) fn is_select_ahead(&self) -> bool {
        matches!(self.tokens.get(self.pos + 1).map(|s| &s.token), Some(Token::Word(w)) if crate::expand::is_name(w))
    }

    /// `select name [in words]; do body; done`, which has the shape of a
    /// `for` loop.
    // Kept out of line: rare, and inlined it bloats the hot parser
    // frames that call it.
    #[inline(never)]
    pub(super) fn parse_select(&mut self) -> ParseResult<Command> {
        self.advance();
        let Token::Word(var) = self.advance().clone() else {
            return Err(self.err("expected a name after `select`"));
        };
        let (items, body) = self.parse_for_rest()?;
        Ok(Command::Select(ForClause { var, items, body }))
    }

    pub(super) fn parse_while(&mut self, until: bool) -> ParseResult<Command> {
        self.advance();
        let condition = self.parse_list()?;
        self.expect(&Token::Do)?;
        let body = self.parse_list()?;
        self.expect(&Token::Done)?;

        Ok(Command::While(WhileClause {
            condition,
            body,
            until,
        }))
    }

    pub(super) fn parse_case(&mut self) -> ParseResult<Command> {
        self.expect(&Token::Case)?;

        let word_raw = match self.advance().clone() {
            Token::Word(w) => w,
            ref other if let Some(text) = keyword_text(other) => text.to_owned(),
            other => return Err(self.err(format!("expected word after `case`, got `{other}`"))),
        };
        let word = self.parse_word_str(word_raw)?;

        self.skip_newlines();
        self.expect(&Token::In)?;
        self.skip_newlines();

        let mut arms = Vec::new();

        loop {
            if self.eat(&Token::Esac) {
                break;
            }

            self.eat(&Token::LParen);

            let mut patterns = Vec::new();
            loop {
                match self.advance().clone() {
                    Token::Word(p) => patterns.push(self.parse_word_str(p)?),
                    // A case pattern is always plain text, never a
                    // reserved word, regardless of what precedes it
                    // (`in`, `;;`, `|` all precede a pattern here):
                    // `case $x in done) ...` is completely standard.
                    ref other if let Some(text) = keyword_text(other) => {
                        patterns.push(self.parse_word_str(text.to_owned())?);
                    }
                    other => {
                        return Err(
                            self.err(format!("expected pattern in case arm, got `{other}`"))
                        );
                    }
                }
                if !self.eat(&Token::Pipe) {
                    break;
                }
            }

            self.expect(&Token::RParen)?;
            self.skip_newlines();

            let body = self.parse_list()?;

            let end = if self.eat(&Token::SemiAmp) {
                CaseEnd::FallThrough
            } else if self.eat(&Token::SemiSemiAmp) {
                CaseEnd::Continue
            } else {
                self.eat(&Token::SemiSemi);
                CaseEnd::Break
            };
            self.skip_newlines();

            arms.push(CaseArm {
                patterns,
                body,
                end,
            });
        }

        Ok(Command::Case(CaseClause { word, arms }))
    }

    pub(super) fn parse_brace_group(&mut self) -> ParseResult<Command> {
        self.expect(&Token::LBrace)?;
        self.skip_newlines();
        let body = self.parse_list()?;
        self.expect(&Token::RBrace)?;
        Ok(Command::Group(GroupCmd {
            body,
            subshell: false,
        }))
    }

    pub(super) fn parse_subshell(&mut self) -> ParseResult<Command> {
        self.expect(&Token::LParen)?;
        self.skip_newlines();
        let body = self.parse_list()?;
        self.expect(&Token::RParen)?;
        Ok(Command::Group(GroupCmd {
            body,
            subshell: true,
        }))
    }

    pub(super) fn parse_function_def(&mut self) -> ParseResult<Command> {
        let has_keyword = self.eat(&Token::Function);

        let name = match self.advance().clone() {
            Token::Word(n) => n,
            other => return Err(self.err(format!("expected function name, got `{other}`"))),
        };

        if !has_keyword {
            self.expect(&Token::LParen)?;
            self.expect(&Token::RParen)?;
        } else if self.eat(&Token::LParen) {
            self.expect(&Token::RParen)?;
        }

        self.skip_newlines();

        let body = self.parse_command()?;

        Ok(Command::FunctionDef(FunctionDef {
            name,
            body: Box::new(body),
        }))
    }
}

/// The unary operators of `[[ ... ]]`.
const COND_UNARY: &[&str] = &[
    "-a", "-b", "-c", "-d", "-e", "-f", "-g", "-h", "-k", "-n", "-o", "-p", "-r", "-s", "-t", "-u",
    "-v", "-w", "-x", "-z", "-G", "-L", "-N", "-O", "-S",
];

/// The binary operators of `[[ ... ]]` written as words.
const COND_BINARY: &[&str] = &[
    "=", "==", "!=", "=~", "-eq", "-ne", "-lt", "-le", "-gt", "-ge", "-nt", "-ot", "-ef",
];

fn cond_text(token: &CondToken) -> String {
    match token {
        CondToken::Word(w) => w.clone(),
        CondToken::And => "&&".to_owned(),
        CondToken::Or => "||".to_owned(),
        CondToken::Not => "!".to_owned(),
        CondToken::LParen => "(".to_owned(),
        CondToken::RParen => ")".to_owned(),
        CondToken::Less => "<".to_owned(),
        CondToken::Greater => ">".to_owned(),
        CondToken::End => "]]".to_owned(),
    }
}

/// Parses the expression of `[[ ... ]]`, reading tokens straight from the
/// lexer.
struct CondParser<'p, 'src> {
    parser: &'p mut Parser<'src>,
    /// Tokens read ahead, next first.
    peeked: Vec<CondToken>,
}

impl CondParser<'_, '_> {
    fn next(&mut self) -> ParseResult<CondToken> {
        if self.peeked.is_empty() {
            return Ok(self.parser.lexer.cond_token()?);
        }
        Ok(self.peeked.remove(0))
    }

    /// The token `n` places ahead, without taking it.
    fn peek(&mut self, n: usize) -> ParseResult<&CondToken> {
        while self.peeked.len() <= n {
            let token = self.parser.lexer.cond_token()?;
            self.peeked.push(token);
        }
        Ok(&self.peeked[n])
    }

    fn word(&self, raw: String) -> ParseResult<Word> {
        self.parser.parse_word_str(raw)
    }

    /// `or := and ('||' and)*`
    fn or(&mut self) -> ParseResult<CondExpr> {
        let mut lhs = self.and()?;
        while *self.peek(0)? == CondToken::Or {
            self.next()?;
            let rhs = self.and()?;
            lhs = CondExpr::Or(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    /// `and := not ('&&' not)*`
    fn and(&mut self) -> ParseResult<CondExpr> {
        let mut lhs = self.not()?;
        while *self.peek(0)? == CondToken::And {
            self.next()?;
            let rhs = self.not()?;
            lhs = CondExpr::And(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    /// `not := '!' not | primary`
    fn not(&mut self) -> ParseResult<CondExpr> {
        if *self.peek(0)? == CondToken::Not {
            self.next()?;
            return Ok(CondExpr::Not(Box::new(self.not()?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> ParseResult<CondExpr> {
        match self.next()? {
            CondToken::LParen => {
                let inner = self.or()?;
                match self.next()? {
                    CondToken::RParen => Ok(inner),
                    other => Err(self
                        .parser
                        .err(format!("expected `)` in `[[`, got `{}`", cond_text(&other)))),
                }
            }
            // `-f file`, unless the word after the operator is itself a
            // binary operator, as in `[[ -a = -a ]]`.
            CondToken::Word(op)
                if COND_UNARY.contains(&op.as_str())
                    && matches!(self.peek(0)?, CondToken::Word(w) if !COND_BINARY.contains(&w.as_str())) =>
            {
                let CondToken::Word(operand) = self.next()? else {
                    return Err(self.parser.err("expected an operand in `[[`"));
                };
                Ok(CondExpr::Unary(op, self.word(operand)?))
            }
            CondToken::Word(lhs) => {
                let op = match self.peek(0)? {
                    CondToken::Less => "<".to_owned(),
                    CondToken::Greater => ">".to_owned(),
                    CondToken::Word(w) if COND_BINARY.contains(&w.as_str()) => w.clone(),
                    _ => return Ok(CondExpr::Word(self.word(lhs)?)),
                };
                self.next()?;
                let rhs = if op == "=~" {
                    if !self.peeked.is_empty() {
                        return Err(self.parser.err("unexpected regular expression in `[[`"));
                    }
                    self.parser.lexer.cond_regex()?
                } else {
                    match self.next()? {
                        CondToken::Word(rhs) => rhs,
                        other => {
                            return Err(self.parser.err(format!(
                                "expected an operand after `{op}` in `[[`, got `{}`",
                                cond_text(&other)
                            )));
                        }
                    }
                };
                Ok(CondExpr::Binary(self.word(lhs)?, op, self.word(rhs)?))
            }
            other => Err(self
                .parser
                .err(format!("unexpected `{}` in `[[`", cond_text(&other)))),
        }
    }
}

use crate::ast::{
    CaseArm, CaseClause, Command, ForClause, FunctionDef, GroupCmd, IfClause, WhileClause, Word,
};
use crate::lexer::Token;

use super::{ParseResult, Parser, keyword_text};

impl Parser {
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

        let var = match self.advance().clone() {
            Token::Word(name) => name,
            other if keyword_text(&other).is_some() => keyword_text(&other).unwrap().to_owned(),
            other => {
                return Err(self.err(format!("expected variable name after `for`, got `{other}`")));
            }
        };

        self.skip_newlines();

        let items = if self.eat(&Token::In) {
            let mut words = Vec::new();
            while let Token::Word(w) = self.peek().clone() {
                let word = self.parse_word_str(&w)?;
                self.advance();
                words.push(word);
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

        Ok(Command::For(ForClause { var, items, body }))
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
            other if keyword_text(&other).is_some() => keyword_text(&other).unwrap().to_owned(),
            other => return Err(self.err(format!("expected word after `case`, got `{other}`"))),
        };
        let word = self.parse_word_str(&word_raw)?;

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
                    Token::Word(p) => patterns.push(self.parse_word_str(&p)?),
                    // A case pattern is always plain text, never a
                    // reserved word, regardless of what precedes it
                    // (`in`, `;;`, `|` all precede a pattern here):
                    // `case $x in done) ...` is completely standard.
                    other if keyword_text(&other).is_some() => {
                        let text = keyword_text(&other).unwrap().to_owned();
                        patterns.push(self.parse_word_str(&text)?);
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

            self.eat(&Token::SemiSemi);
            self.skip_newlines();

            arms.push(CaseArm { patterns, body });
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

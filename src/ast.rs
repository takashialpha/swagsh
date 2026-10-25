/// A single word after expansion: may be a literal, a variable reference,
/// a glob pattern, a command substitution, etc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Word {
    /// Bare literal text, no expansion needed.
    Literal(String),
    /// `$VAR` or `${...}`, holding the text between the braces (`VAR:-word`,
    /// `#VAR`, ...) for `parse_param_op` to interpret at expansion time.
    Var(String),
    /// `$(( expr ))` arithmetic expansion.
    Arith(String),
    /// `$(cmd)` or backtick form.
    CmdSub(Box<Command>),
    /// `<(cmd)`, or with `output`, `>(cmd)`: the name of a pipe from or to
    /// `cmd`.
    ProcSub { cmd: Box<Command>, output: bool },
    /// A sequence of word-parts concatenated at expansion time.
    Compound(Vec<Self>),
    /// Content from a single- or double-quoted region.
    /// Suppresses IFS word-splitting and glob expansion on the result.
    Quoted(Box<Self>),
    /// An assignment, before a command or as an argument of `declare` and
    /// the other declaration commands.
    Assign(Box<Assign>),
}

/// `name=value`, `name+=value`, `name[index]=value` or `name=(items)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assign {
    pub name: String,
    /// `name[index]=`: the subscript, still to be expanded.
    pub index: Option<Word>,
    /// `+=`: append to the current value instead of replacing it.
    pub append: bool,
    pub value: AssignValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssignValue {
    Scalar(Word),
    /// `name=(items)`: a whole array.
    Array(Vec<ArrayItem>),
}

/// One item of `name=(items)`: a value, or `[index]=value`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrayItem {
    pub index: Option<Word>,
    pub append: bool,
    pub value: Word,
}

// ---------------------------------------------------------------------------
// Redirections
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectKind {
    /// `>file`
    Out,
    /// `>|file`: like `Out`, but ignores `set -C`.
    Clobber,
    /// `>>file`
    Append,
    /// `<file`
    In,
    /// `<>file`: open for reading and writing, creating it if needed.
    ReadWrite,
    /// `>&N` or `<&N`: duplicate descriptor `N` onto [`Redirect::fd`]
    /// (`2>&1`), or close it when `N` is `-`.
    Dup,
    /// `&>file`: stdout + stderr
    Both,
    /// `<<DELIM` here-document body. `quoted` is true when any part of the
    /// delimiter was quoted, which turns off expansion of the body.
    HereDoc { raw_body: String, quoted: bool },
    /// `<<<word` (herestring)
    HereString,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    pub kind: RedirectKind,
    /// Source file descriptor (default 1 for out, 0 for in).
    pub fd: i32,
    pub target: Word,
    /// `{name}>file`: the descriptor is a new one, which stays open after
    /// the command, and `name` gets its number. Boxed, as rare.
    pub fd_var: Option<Box<str>>,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// A simple command: `prog arg1 arg2 >out`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimpleCmd {
    pub words: Vec<Word>,
    pub redirects: Vec<Redirect>,
    /// The line it starts on, for `$LINENO`.
    pub line: usize,
}

/// A pipeline: `cmd1 | cmd2 | cmd3`.
/// `negated` covers the `!` prefix (POSIX §2.9.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pipeline {
    pub commands: Vec<Command>,
    pub negated: bool,
    /// `time` (or `time -p`, for POSIX's output format) before it.
    pub timed: Option<TimeFormat>,
}

/// How `time` reports a pipeline's times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeFormat {
    /// `real\t0m0.010s`, as bash and ksh print it.
    Default,
    /// `time -p`: `real 0.01`.
    Posix,
}

/// The body of an `if` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IfClause {
    pub condition: Vec<AndOrList>,
    pub then_body: Vec<AndOrList>,
    pub elif_clauses: Vec<(Vec<AndOrList>, Vec<AndOrList>)>,
    pub else_body: Option<Vec<AndOrList>>,
}

/// `for name in words; do body; done`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForClause {
    pub var: String,
    pub items: Vec<Word>,
    pub body: Vec<AndOrList>,
}

/// `for (( init; condition; step )); do body; done`, each part an
/// arithmetic expression, any of them empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArithForClause {
    pub init: String,
    pub condition: String,
    pub step: String,
    pub body: Vec<AndOrList>,
}

/// The expression of `[[ ... ]]`. Its words are expanded without field
/// splitting or pathname expansion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CondExpr {
    /// A word alone: true when it is not empty.
    Word(Word),
    /// `-f path` and the other unary tests.
    Unary(String, Word),
    /// `a == pattern`, `a =~ regex`, `a -lt b` and the other binary tests.
    Binary(Word, String, Word),
    Not(Box<Self>),
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
}

/// `while condition; do body; done`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhileClause {
    pub condition: Vec<AndOrList>,
    pub body: Vec<AndOrList>,
    /// `true` for `until` loops.
    pub until: bool,
}

/// `case word in pattern) body ;; ... esac`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseClause {
    pub word: Word,
    pub arms: Vec<CaseArm>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseArm {
    pub patterns: Vec<Word>,
    pub body: Vec<AndOrList>,
    pub end: CaseEnd,
}

/// How a `case` arm ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseEnd {
    /// `;;`: the `case` is done.
    Break,
    /// `;&`: the next arm's body runs too, without its patterns tested.
    FallThrough,
    /// `;;&`: the next arms' patterns are tested too.
    Continue,
}

/// `name() { body; }`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionDef {
    pub name: String,
    pub body: Box<Command>,
}

/// `{ list; }` or `( list )`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupCmd {
    pub body: Vec<AndOrList>,
    /// `true`: subshell `( )`; `false`: brace group `{ }`.
    pub subshell: bool,
}

/// The top-level command node: every syntactic construct reduces to this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Simple(SimpleCmd),
    Pipeline(Pipeline),
    If(IfClause),
    For(ForClause),
    /// Boxed: as large as the other clauses, it would make every command
    /// larger by its tag.
    ArithFor(Box<ArithForClause>),
    While(WhileClause),
    Case(CaseClause),
    FunctionDef(FunctionDef),
    Group(GroupCmd),
    /// `coproc [name] command`: `command` runs in the background with pipes
    /// to and from the shell, whose descriptors the array `name` holds.
    Coproc(String, Box<Self>),
    /// `select name in words; do body; done`: a menu.
    Select(ForClause),
    /// `(( expression ))`: succeeds when the expression is not zero.
    Arith(String),
    /// `[[ expression ]]`.
    Cond(Box<CondExpr>),
    /// A compound command followed by redirections: `while ...; done <file`.
    Redirected(Box<Self>, Vec<Redirect>),
}

// ---------------------------------------------------------------------------
// And-or lists  (`&&` / `||`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AndOrOp {
    And, // &&
    Or,  // ||
}

/// One element in an and-or chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndOrItem {
    pub command: Pipeline,
    /// The operator that *follows* this item (none for the last one).
    pub op: Option<AndOrOp>,
}

/// A sequence of pipelines joined by `&&` / `||`.
/// `is_async` marks a trailing `&` (background execution).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndOrList {
    pub items: Vec<AndOrItem>,
    pub is_async: bool,
}

// ---------------------------------------------------------------------------
// Top-level program
// ---------------------------------------------------------------------------

/// A complete parsed script or interactive input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub body: Vec<AndOrList>,
}

impl Program {
    /// Collapse a parsed program into a single `Command` for use in `$()` and
    /// backtick substitutions. A simple single-statement body is unwrapped to
    /// the inner `Command` directly; anything else becomes a subshell group.
    #[must_use]
    pub fn into_command(mut self) -> Command {
        if self.body.len() == 1 && self.body[0].items.len() == 1 && !self.body[0].is_async {
            let mut pl = self.body.swap_remove(0).items.swap_remove(0).command;
            if pl.commands.len() == 1
                && let Some(cmd) = pl.commands.pop()
            {
                return cmd;
            }
            Command::Pipeline(pl)
        } else {
            Command::Group(GroupCmd {
                body: self.body,
                subshell: true,
            })
        }
    }
}

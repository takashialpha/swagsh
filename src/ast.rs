/// A single word after expansion — may be a literal, a variable reference,
/// a glob pattern, a command substitution, etc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Word {
    /// Bare literal text, no expansion needed.
    Literal(String),
    /// `$VAR` or `${VAR}`.
    Var(String),
    /// `$(cmd)` or backtick form.
    CmdSub(Box<Command>),
    /// A sequence of word-parts concatenated at expansion time.
    Compound(Vec<Word>),
}

// ---------------------------------------------------------------------------
// Redirections
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectKind {
    /// `>file`
    Out,
    /// `>>file`
    Append,
    /// `<file`
    In,
    /// `2>file`  — fd is stored in `RedirectOp.fd`
    FdOut,
    /// `&>file` — stdout + stderr
    Both,
    /// `<<<word` (herestring)
    HereString,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    pub kind: RedirectKind,
    /// Source file descriptor (default 1 for out, 0 for in).
    pub fd: i32,
    pub target: Word,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// A simple command: `prog arg1 arg2 >out`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimpleCmd {
    pub words: Vec<Word>,
    pub redirects: Vec<Redirect>,
}

/// A pipeline: `cmd1 | cmd2 | cmd3`.
/// `negated` covers the `!` prefix (POSIX §2.9.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pipeline {
    pub commands: Vec<Command>,
    pub negated: bool,
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
    /// `true` → subshell `( )`, `false` → brace group `{ }`.
    pub subshell: bool,
}

/// The top-level command node — every syntactic construct reduces to this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Simple(SimpleCmd),
    Pipeline(Pipeline),
    If(IfClause),
    For(ForClause),
    While(WhileClause),
    Case(CaseClause),
    FunctionDef(FunctionDef),
    Group(GroupCmd),
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
/// `async` marks a trailing `&` (background execution).
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

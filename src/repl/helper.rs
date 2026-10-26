//! What the line editor asks of the shell while a line is typed: Tab
//! completion, syntax highlighting, and suggestions as it is typed.
//!
//! The editor holds this helper while the shell waits for input, so the
//! helper keeps its own copy of what it needs from the shell (functions,
//! aliases, variables), taken by [`ShellHelper::refresh`] before each
//! prompt.
//!
//! What the editor cannot do itself, a key asks for with a [`Request`]: the
//! key ends the line, and the shell carries the request out before reading
//! the line again.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use rustyline::completion::{Completer, Pair, longest_common_prefix};
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::Hinter;
use rustyline::history::SearchDirection;
use rustyline::validate::Validator;
use rustyline::{
    Cmd, ConditionalEventHandler, Context, EditMode, Event, EventContext, Helper, InputMode,
    RepeatCount,
};

use super::config::{Case, Config, Suggestions, ansi};
use super::highlight::{self, KEYWORDS, Kind, Lookup, Styles};
use super::manpage::{self, Page};
use crate::eval::Shell;

/// The names the shell knows, as of the last prompt.
#[derive(Default)]
struct Names {
    functions: Vec<String>,
    aliases: Vec<String>,
    /// Every variable and its value, sorted by name.
    variables: Vec<(String, String)>,
    home: Option<String>,
    autocd: bool,
    /// The programs on `$PATH`, sorted.
    programs: Vec<String>,
    /// The `$PATH` they were found on, and when each of its directories was
    /// last changed, to know when to look again.
    programs_from: (String, Vec<Option<SystemTime>>),
    /// At a continuation prompt, the lines already read of the command.
    context: String,
    case: Case,
    /// Whether to read other programs' options from their man pages.
    man: bool,
    manpath: Option<String>,
    /// The man pages read so far, by name; `None` for one there is not.
    man_pages: RefCell<HashMap<String, Option<Arc<Page>>>>,
}

impl Names {
    fn variable(&self, name: &str) -> Option<&str> {
        self.variables
            .binary_search_by(|(n, _)| n.as_str().cmp(name))
            .ok()
            .map(|i| self.variables[i].1.as_str())
    }

    /// Looks for the programs on `path` again, if it or a directory on it
    /// changed.
    fn refresh_programs(&mut self, path: &str) {
        let dirs: Vec<&str> = path.split(':').filter(|d| !d.is_empty()).collect();
        let stamps: Vec<Option<SystemTime>> = dirs
            .iter()
            .map(|d| std::fs::metadata(d).and_then(|m| m.modified()).ok())
            .collect();
        if self.programs_from.0 == path && self.programs_from.1 == stamps {
            return;
        }
        let mut programs = Vec::new();
        for dir in &dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                if is_executable(&entry.path()) {
                    programs.push(entry.file_name().to_string_lossy().into_owned());
                }
            }
        }
        programs.sort_unstable();
        programs.dedup();
        self.programs = programs;
        self.programs_from = (path.to_owned(), stamps);
    }
}

impl Lookup for Names {
    fn command_exists(&self, name: &str) -> bool {
        if name.contains('/') {
            let path = Path::new(name);
            return is_executable(path) || (self.autocd && path.is_dir());
        }
        crate::builtins::lookup_builtin(name).is_some()
            || KEYWORDS.contains(&name)
            || self
                .functions
                .binary_search_by(|f| f.as_str().cmp(name))
                .is_ok()
            || self
                .aliases
                .binary_search_by(|a| a.as_str().cmp(name))
                .is_ok()
            || self
                .programs
                .binary_search_by(|p| p.as_str().cmp(name))
                .is_ok()
            || (self.autocd && Path::new(name).is_dir())
    }

    fn home(&self) -> Option<&str> {
        self.home.as_deref()
    }
}

/// Whether `path` is a regular file someone may execute.
fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// What a key asked the shell to do, once the editor has returned the line.
pub enum Request {
    /// List `candidates`, the completions of `line[start..pos]`: more than
    /// the editor can list well, so a page at a time.
    List {
        line: String,
        pos: usize,
        start: usize,
        candidates: Vec<Pair>,
    },
    /// Open `line` in an editor.
    Edit { line: String },
}

impl Request {
    /// The line to read again once the candidates have been listed, and
    /// where its cursor goes: with the part they all start with filled in.
    #[must_use]
    pub fn completed(line: &str, pos: usize, start: usize, candidates: &[Pair]) -> (String, usize) {
        match longest_common_prefix(candidates) {
            Some(common) if common.len() > pos - start => (
                format!("{}{common}{}", &line[..start], &line[pos..]),
                start + common.len(),
            ),
            _ => (line.to_owned(), pos),
        }
    }
}

/// The shared slots of the helper and its keys.
type Shared<T> = Arc<Mutex<T>>;

fn lock<T>(shared: &Shared<T>) -> MutexGuard<'_, T> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct ShellHelper {
    names: Shared<Names>,
    request: Shared<Option<Request>>,
    /// `None` with highlighting off.
    styles: Option<Styles>,
    suggestions: Suggestions,
}

impl ShellHelper {
    #[must_use]
    pub fn new(config: &Config) -> Self {
        let h = &config.highlight;
        let style = |spec: &str| ansi(spec).unwrap_or_default();
        let styles = h.enabled.then(|| Styles {
            styles: [
                style(&h.command),
                style(&h.unknown),
                style(&h.keyword),
                style(&h.string),
                style(&h.variable),
                style(&h.operator),
                style(&h.option),
                style(&h.comment),
                style(&h.path),
                style(&h.error),
                style(&h.bracket),
            ],
            suggestion: style(&h.suggestion),
        });
        Self {
            names: Arc::new(Mutex::new(Names {
                case: config.completion.case,
                man: config.completion.man,
                ..Names::default()
            })),
            request: Shared::default(),
            styles,
            suggestions: Suggestions {
                enabled: config.suggestions.enabled,
                history: config.suggestions.history,
                completions: config.suggestions.completions,
            },
        }
    }

    /// Takes what completion and highlighting need from `shell`.
    pub fn refresh(&self, shell: &Shell) {
        let env = &shell.env;
        let names = &mut *lock(&self.names);
        names.functions = env
            .function_names()
            .into_iter()
            .map(str::to_owned)
            .collect();
        names.functions.sort_unstable();
        names.aliases = env.all_aliases().map(|(n, _)| n.to_owned()).collect();
        names.aliases.sort_unstable();
        names.variables = env
            .all_vars()
            .map(|(n, v)| (n.to_owned(), v.to_owned()))
            .chain(
                env.all_arrays()
                    .into_iter()
                    .map(|(n, _)| (n.to_owned(), String::new())),
            )
            .collect();
        names.variables.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        names.variables.dedup_by(|a, b| a.0 == b.0);
        names.home = env.get("HOME").filter(|h| !h.is_empty());
        names.autocd = shell.shopt.autocd;
        names.manpath = env.get("MANPATH");
        names.refresh_programs(&env.get_or_empty("PATH"));
    }

    /// Sets the lines already read of the command, for a continuation
    /// prompt; empty at the first prompt.
    pub fn set_context(&self, text: &str) {
        text.clone_into(&mut lock(&self.names).context);
    }

    /// What a key asked for while the line was read, if anything.
    #[must_use]
    pub fn take_request(&self) -> Option<Request> {
        lock(&self.request).take()
    }

    /// The handler for Tab, which leaves a long list of completions to the
    /// shell.
    #[must_use]
    pub fn tab_key(&self) -> TabKey {
        TabKey {
            names: Arc::clone(&self.names),
            request: Arc::clone(&self.request),
        }
    }

    /// The handler for a key that opens the line in an editor; with
    /// `vi_command`, only in vi's command mode.
    #[must_use]
    pub fn edit_key(&self, vi_command: bool) -> EditKey {
        EditKey {
            request: Arc::clone(&self.request),
            vi_command,
        }
    }
}

pub struct TabKey {
    names: Shared<Names>,
    request: Shared<Option<Request>>,
}

impl ConditionalEventHandler for TabKey {
    fn handle(&self, _: &Event, _: RepeatCount, _: bool, ctx: &EventContext<'_>) -> Option<Cmd> {
        if ctx.mode() == EditMode::Vi && ctx.input_mode() == InputMode::Command {
            return None;
        }
        let (line, pos) = (ctx.line(), ctx.pos());
        let (start, candidates) = lock(&self.names).complete(line, pos);
        if candidates.len() < 2 || super::pager::fits(&candidates) {
            return None;
        }
        *lock(&self.request) = Some(Request::List {
            line: line.to_owned(),
            pos,
            start,
            candidates,
        });
        Some(Cmd::AcceptLine)
    }
}

pub struct EditKey {
    request: Shared<Option<Request>>,
    vi_command: bool,
}

impl ConditionalEventHandler for EditKey {
    fn handle(&self, _: &Event, _: RepeatCount, _: bool, ctx: &EventContext<'_>) -> Option<Cmd> {
        if self.vi_command
            && !(ctx.mode() == EditMode::Vi && ctx.input_mode() == InputMode::Command)
        {
            return None;
        }
        *lock(&self.request) = Some(Request::Edit {
            line: ctx.line().to_owned(),
        });
        Some(Cmd::AcceptLine)
    }
}

/// The items whose names `word` completes: those starting with it, or
/// failing that, those containing it. `case` says whether a capital matches
/// a small letter; a word with a capital matching nothing also tries
/// ignoring case under [`Case::Smart`].
fn matching<T>(mut items: Vec<T>, word: &str, case: Case, name: impl Fn(&T) -> &str) -> Vec<T> {
    let capital = word.chars().any(char::is_uppercase);
    // Each way of matching, tried in turn: (ignoring case, anywhere in the name).
    let tiers: &[(bool, bool)] = match case {
        Case::Sensitive => &[(false, false), (false, true)],
        Case::Smart if capital => &[(false, false), (true, false), (true, true)],
        Case::Smart | Case::Insensitive => &[(true, false), (true, true)],
    };
    let lower = word.to_lowercase();
    for &(fold, anywhere) in tiers {
        let matches = |t: &T| {
            let name = name(t);
            match (fold, anywhere) {
                (false, false) => name.starts_with(word),
                (false, true) => name.contains(word),
                (true, false) => name.to_lowercase().starts_with(&lower),
                (true, true) => name.to_lowercase().contains(&lower),
            }
        };
        if items.iter().any(matches) {
            items.retain(matches);
            return items;
        }
    }
    Vec::new()
}

/// The characters a completed word escapes with a backslash.
const SPECIAL: &str = " \t\\'\"$`&;|<>()*?[]!#{}~";

fn escape(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if SPECIAL.contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Where the word ending at `pos` starts.
fn word_start(line: &str, pos: usize) -> usize {
    let bytes = line.as_bytes();
    let mut start = pos;
    while start > 0 {
        let c = bytes[start - 1];
        let escaped = start >= 2 && bytes[start - 2] == b'\\';
        if b" \t|&;<>()".contains(&c) && !escaped {
            break;
        }
        start -= 1;
    }
    start
}

impl Names {
    fn commands(&self, prefix: &str) -> Vec<Pair> {
        let mut found: Vec<&str> = crate::builtins::BUILTINS
            .iter()
            .map(|(name, ..)| *name)
            .chain(KEYWORDS.iter().copied())
            .chain(self.functions.iter().map(String::as_str))
            .chain(self.aliases.iter().map(String::as_str))
            .chain(self.programs.iter().map(String::as_str))
            .collect();
        found.sort_unstable();
        found.dedup();
        matching(found, prefix, self.case, |name| name)
            .into_iter()
            .map(|name| Pair {
                display: name.to_owned(),
                replacement: format!("{} ", escape(name)),
            })
            .collect()
    }

    fn variables(&self, prefix: &str, sigil: &str, close: &str) -> Vec<Pair> {
        let names: Vec<&str> = self
            .variables
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        matching(names, prefix, self.case, |name| name)
            .into_iter()
            .map(|name| Pair {
                display: name.to_owned(),
                replacement: format!("{sigil}{name}{close}"),
            })
            .collect()
    }

    /// The options of `command` starting with `prefix`: a builtin's from
    /// its help page, another program's from its man page. For a command
    /// with subcommands, such as `git commit`, `sub` is the subcommand,
    /// whose page is `git-commit`.
    fn options(&self, command: &str, sub: Option<&str>, prefix: &str) -> Vec<Pair> {
        let entries: Vec<(String, String, String)> = if let Some(&(_, _, page)) =
            crate::builtins::BUILTINS
                .iter()
                .find(|(name, ..)| *name == command)
        {
            page()
                .options
                .into_iter()
                .filter_map(|(flag, what)| {
                    Some((flag.split_whitespace().next()?.to_owned(), flag, what))
                })
                .collect()
        } else if let Some(page) = self.man_page_for(command, sub) {
            page.options
                .iter()
                .flat_map(|o| {
                    o.words
                        .iter()
                        .map(|w| (w.clone(), o.tag.clone(), o.what.clone()))
                })
                .collect()
        } else {
            return Vec::new();
        };
        let mut found = matching(entries, prefix, self.case, |(word, ..)| word);
        // An option spelled two ways is listed once.
        let mut seen = std::collections::HashSet::new();
        found.retain(|(_, tag, _)| seen.insert(tag.clone()));
        let width = found
            .iter()
            .map(|(_, tag, _)| tag.chars().count())
            .max()
            .unwrap_or(0)
            .min(30);
        found
            .into_iter()
            .map(|(word, tag, what)| Pair {
                display: format!("{tag:<width$}  {what}"),
                replacement: format!("{word} "),
            })
            .collect()
    }

    /// The man page `name`, read once.
    fn man_page(&self, name: &str) -> Option<Arc<Page>> {
        if let Some(page) = self.man_pages.borrow().get(name) {
            return page.clone();
        }
        let page =
            manpage::page(name, &self.programs_from.0, self.manpath.as_deref()).map(Arc::new);
        self.man_pages
            .borrow_mut()
            .insert(name.to_owned(), page.clone());
        page
    }

    /// The man page of `command`, or of its subcommand `sub` when that has
    /// one (`git-commit`); `None` with man pages turned off.
    fn man_page_for(&self, command: &str, sub: Option<&str>) -> Option<Arc<Page>> {
        if !self.man {
            return None;
        }
        let command = command.rsplit('/').next().unwrap_or(command);
        sub.and_then(|sub| self.man_page(&format!("{command}-{sub}")))
            .or_else(|| self.man_page(command))
    }

    /// Whether `command` (with subcommand `sub`) takes operands besides
    /// its options, as its usage or man page says; `None` when neither
    /// says.
    fn takes_operands(&self, command: &str, sub: Option<&str>) -> Option<bool> {
        if let Some(&(_, _, page)) = crate::builtins::BUILTINS
            .iter()
            .find(|(name, ..)| *name == command)
        {
            return Some(manpage::takes_operands(&page().usage, command));
        }
        self.man_page_for(command, sub).map(|page| page.operands)
    }

    /// Whether history entry `entry` could still run as it did: every
    /// command it names exists, and so does every path it names.
    fn still_valid(&self, entry: &str) -> bool {
        let scan = highlight::scan(entry, self);
        if scan.spans.iter().any(|span| span.kind == Kind::Unknown) {
            return false;
        }
        let mut after_cd = false;
        for &(start, end, command) in &scan.words {
            let word = unescape(&entry[start..end]);
            if command {
                after_cd = matches!(word.as_str(), "cd" | "pushd");
                continue;
            }
            let expands = word.contains(['$', '`', '*', '?', '[', '{', '\'', '"']);
            if expands || word.starts_with('-') || word.contains("://") {
                after_cd = false;
                continue;
            }
            let path = match (word.strip_prefix("~/"), &self.home) {
                (Some(rest), Some(home)) => Path::new(home).join(rest),
                _ => PathBuf::from(&word),
            };
            if after_cd && !path.is_dir() || word.contains('/') && !path.exists() {
                return false;
            }
            after_cd = false;
        }
        true
    }

    /// The rest of a command from history that starts with `line`, the
    /// newest one that is still valid.
    fn history_hint(&self, line: &str, ctx: &Context<'_>) -> Option<String> {
        let history = ctx.history();
        let mut start = if ctx.history_index() == history.len() {
            history.len().checked_sub(1)?
        } else {
            ctx.history_index()
        };
        // Enough to look past a few stale entries, without slowing typing.
        for _ in 0..200 {
            let found = history
                .starts_with(line, start, SearchDirection::Reverse)
                .ok()??;
            if found.entry.len() > line.len() && self.still_valid(&found.entry) {
                return Some(found.entry[line.len()..].to_owned());
            }
            start = found.idx.checked_sub(1)?;
        }
        None
    }

    /// The rest of the word before `pos`, when it has one completion. Not
    /// for a word not yet started, or a command's name, which are left to
    /// history.
    fn completion_hint(&self, line: &str, pos: usize) -> Option<String> {
        let start = word_start(line, pos);
        if start == pos || self.is_command_word(line, start) {
            return None;
        }
        let (start, candidates) = self.complete(line, pos);
        let [only] = candidates.as_slice() else {
            return None;
        };
        let typed = &line[start..pos];
        let rest = only.replacement.trim_end_matches(' ').strip_prefix(typed)?;
        (!rest.is_empty()).then(|| rest.to_owned())
    }

    /// The scan of the command up to the word starting at `start` of
    /// `line`, with a word standing in for that one, and where that word is
    /// in it.
    fn probe(&self, line: &str, start: usize) -> (String, usize, highlight::Scan) {
        let probe = format!("{}{}x", self.context, &line[..start]);
        let at = self.context.len() + start;
        let scan = highlight::scan(&probe, self);
        (probe, at, scan)
    }

    /// Whether the word starting at `start` of `line` is a command's name.
    fn is_command_word(&self, line: &str, start: usize) -> bool {
        let (_, at, scan) = self.probe(line, start);
        scan.words
            .last()
            .is_some_and(|&(s, _, command)| s == at && command)
    }

    /// The files whose names complete `word`, a path as typed; only
    /// directories with `dirs_only`.
    fn files(&self, word: &str, dirs_only: bool) -> Vec<Pair> {
        let (dir_part, prefix) = word
            .rfind('/')
            .map_or(("", word), |i| (&word[..=i], &word[i + 1..]));
        let dir = self.expand_dir(dir_part);
        let prefix = unescape(prefix);
        let Ok(entries) = std::fs::read_dir(if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            &dir
        }) else {
            return Vec::new();
        };
        let mut found: Vec<(String, bool)> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') && !prefix.starts_with('.') {
                    return None;
                }
                let is_dir = entry.path().is_dir();
                (is_dir || !dirs_only).then_some((name, is_dir))
            })
            .collect();
        found.sort_unstable();
        matching(found, &prefix, self.case, |(name, _)| name)
            .into_iter()
            .map(|(name, is_dir)| Pair {
                display: if is_dir {
                    format!("{name}/")
                } else {
                    name.clone()
                },
                replacement: format!(
                    "{dir_part}{}{}",
                    escape(&name),
                    if is_dir { "/" } else { " " }
                ),
            })
            .collect()
    }

    /// The directory a typed path's directory part names: with a leading `~`
    /// and `$NAME` expanded.
    fn expand_dir(&self, dir_part: &str) -> PathBuf {
        let mut text = unescape(dir_part);
        if let Some(rest) = text.strip_prefix('~')
            && (rest.is_empty() || rest.starts_with('/'))
            && let Some(home) = &self.home
        {
            text = format!("{home}{rest}");
        }
        if let Some(rest) = text.strip_prefix('$') {
            let (name, after) = rest.strip_prefix('{').map_or_else(
                || {
                    let len = rest.find('/').unwrap_or(rest.len());
                    (&rest[..len], &rest[len..])
                },
                |braced| {
                    let len = braced.find('}').unwrap_or(braced.len());
                    (&braced[..len], braced.get(len + 1..).unwrap_or(""))
                },
            );
            if let Some(value) = self.variable(name) {
                text = format!("{value}{after}");
            }
        }
        PathBuf::from(text)
    }
}

fn unescape(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    let mut chars = word.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.extend(chars.next());
        } else {
            out.push(c);
        }
    }
    out
}

/// The commands whose arguments are variable names.
const TAKES_VARIABLES: &[&str] = &["declare", "export", "local", "readonly", "typeset", "unset"];

impl Names {
    /// Where the word before `pos` starts, and its completions.
    fn complete(&self, line: &str, pos: usize) -> (usize, Vec<Pair>) {
        let start = word_start(line, pos);
        let word = &line[start..pos];
        if let Some(rest) = word.strip_prefix("${") {
            return (start, self.variables(rest, "${", "}"));
        }
        if let Some(i) = word.rfind('$')
            && word[i + 1..]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            let pairs = self.variables(&word[i + 1..], "$", "");
            return (start + i, pairs);
        }
        // The scan of what comes before, with a word standing in for this
        // one, tells whether it is a command and which command it is for.
        let (probe, at, scan) = self.probe(line, start);
        let is_command = scan
            .words
            .last()
            .is_some_and(|&(s, _, command)| s == at && command);
        if is_command && !word.contains('/') {
            return (start, self.commands(word));
        }
        let command_at = scan
            .words
            .iter()
            .rposition(|&(s, _, command)| command && s < at);
        let command = command_at.map_or("", |i| &probe[scan.words[i].0..scan.words[i].1]);
        // The first word after the command that is not an option.
        let sub = command_at.and_then(|i| {
            scan.words[i + 1..]
                .iter()
                .filter(|&&(s, ..)| s < at)
                .map(|&(s, e, _)| &probe[s..e])
                .find(|w| !w.starts_with('-'))
        });
        if word.starts_with('-') {
            let options = self.options(command, sub, word);
            if !options.is_empty() {
                return (start, options);
            }
        }
        if !word.contains('/') {
            if matches!(command, "help" | "builtin" | "enable") {
                let mut pairs = self.commands(word);
                pairs.retain(|p| crate::builtins::lookup_builtin(&p.display).is_some());
                return (start, pairs);
            }
            if matches!(command, "type" | "which" | "man" | "hash") {
                return (start, self.commands(word));
            }
        }
        // A command that takes nothing but options offers them at once.
        if word.is_empty()
            && !command.is_empty()
            && self.takes_operands(command, sub) == Some(false)
        {
            let options = self.options(command, sub, "");
            if !options.is_empty() {
                return (start, options);
            }
        }
        if TAKES_VARIABLES.contains(&command) && !word.contains('/') {
            return (start, self.variables(word, "", ""));
        }
        (start, self.files(word, matches!(command, "cd" | "pushd")))
    }
}

impl Completer for ShellHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        Ok(lock(&self.names).complete(line, pos))
    }
}

impl Hinter for ShellHelper {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, ctx: &Context<'_>) -> Option<String> {
        let s = &self.suggestions;
        if !s.enabled || line.is_empty() || pos < line.len() {
            return None;
        }
        let names = lock(&self.names);
        if !names.context.is_empty() {
            return None;
        }
        let hint = s
            .history
            .then(|| names.history_hint(line, ctx))
            .flatten()
            .or_else(|| {
                s.completions
                    .then(|| names.completion_hint(line, pos))
                    .flatten()
            });
        drop(names);
        hint
    }
}

impl Highlighter for ShellHelper {
    fn highlight<'l>(&self, line: &'l str, pos: usize) -> Cow<'l, str> {
        let Some(styles) = &self.styles else {
            return Cow::Borrowed(line);
        };
        let names = lock(&self.names);
        let from = names.context.len();
        let whole = if from == 0 {
            Cow::Borrowed(line)
        } else {
            Cow::Owned(format!("{}{line}", names.context))
        };
        let scan = highlight::scan(&whole, &*names);
        drop(names);
        Cow::Owned(highlight::render(&whole, from, from + pos, &scan, styles))
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        match &self.styles {
            Some(styles) if !styles.suggestion.is_empty() => {
                Cow::Owned(format!("{}{hint}\x1b[0m", styles.suggestion))
            }
            _ => Cow::Borrowed(hint),
        }
    }

    fn highlight_char(&self, _line: &str, _pos: usize, _kind: CmdKind) -> bool {
        self.styles.is_some()
    }
}

impl Validator for ShellHelper {}

impl Helper for ShellHelper {}

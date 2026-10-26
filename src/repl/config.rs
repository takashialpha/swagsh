//! The interactive configuration file, `~/.config/swagsh/config.toml`
//! (`$XDG_CONFIG_HOME/swagsh/config.toml` when that is set).
//!
//! It is read only by an interactive shell, and every setting in it has a
//! default, so the file and each of its sections can be left out.

use serde::Deserialize;

use crate::env::Env;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub banner: Banner,
    pub editor: Editor,
    pub highlight: Highlight,
    pub history: History,
}

/// The greeting shown when an interactive shell starts.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Banner {
    pub enabled: bool,
    /// A tip on using swagsh under the logo.
    pub tips: bool,
}

impl Default for Banner {
    fn default() -> Self {
        Self {
            enabled: true,
            tips: true,
        }
    }
}

/// The key bindings of the prompt.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Keys {
    #[default]
    Emacs,
    Vi,
}

/// Editing the line being typed.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Editor {
    /// `set -o vi` and `set -o emacs` change it from the shell.
    pub keys: Keys,
    /// The editor program Ctrl-X Ctrl-E (`v` with vi keys) and `fc` open,
    /// with arguments if it needs them; empty for `$VISUAL`, then
    /// `$EDITOR`, then `vi`.
    pub command: String,
    /// Suggest the rest of a line from history, in grey, as it is typed.
    pub suggestions: bool,
}

impl Default for Editor {
    fn default() -> Self {
        Self {
            keys: Keys::Emacs,
            command: String::new(),
            suggestions: true,
        }
    }
}

/// Syntax highlighting of the line being typed, and its colors.
///
/// Each color is a style as `ls`-like tools take them: names (`red`,
/// `bright-blue`), attributes (`bold`, `dim`, `italic`, `underline`), a
/// 256-color number, or `#rrggbb`, separated by spaces (`"bold #ff8800"`).
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Highlight {
    pub enabled: bool,
    /// A command that exists: a builtin, function, alias or program.
    pub command: String,
    /// A command that does not exist.
    pub unknown: String,
    /// A reserved word: `if`, `for`, `done`, ...
    pub keyword: String,
    pub string: String,
    pub variable: String,
    /// `|`, `&&`, `;`, redirections.
    pub operator: String,
    /// An argument starting with `-`.
    pub option: String,
    pub comment: String,
    /// An argument naming a file or directory that exists.
    pub path: String,
    /// A quote that is never closed, and the like.
    pub error: String,
    /// The bracket matching the one at the cursor.
    pub bracket: String,
    /// A suggestion from history.
    pub suggestion: String,
}

impl Default for Highlight {
    fn default() -> Self {
        let style = |s: &str| s.to_owned();
        Self {
            enabled: true,
            command: style("green"),
            unknown: style("red"),
            keyword: style("magenta"),
            string: style("yellow"),
            variable: style("cyan"),
            operator: style("bold"),
            option: style("blue"),
            comment: style("dim"),
            path: style("underline"),
            error: style("bold red"),
            bracket: style("bold reverse"),
            suggestion: style("dim"),
        }
    }
}

/// The history of commands typed.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct History {
    /// How many commands to keep; `$HISTSIZE` overrides it.
    pub size: usize,
    /// Don't record a command that repeats the one before.
    pub ignore_dups: bool,
    /// Don't record a command that starts with a space.
    pub ignore_space: bool,
}

impl Default for History {
    fn default() -> Self {
        Self {
            size: 1000,
            ignore_dups: true,
            ignore_space: true,
        }
    }
}

/// The file written where there is none: every setting at its default,
/// with what it does.
pub const TEMPLATE: &str = r#"# swagsh's settings for interactive use. Scripts never read this file.
# Every value here is the default: change one, or delete it to go back.
#
# A color is any mix of names (red, bright-blue), attributes (bold, dim,
# italic, underline, reverse), a 256-color number and #rrggbb, separated by
# spaces: "bold #ff8800".

[banner]
enabled = true       # the logo and version at startup
tips = true          # and a tip next to them

[editor]
keys = "emacs"       # or "vi"; `set -o vi` and `set -o emacs` switch too
command = ""         # the editor for Ctrl-X Ctrl-E (`v` with vi keys) and fc,
                     # such as "nvim" or "code --wait"; empty for $VISUAL,
                     # then $EDITOR, then vi
suggestions = true   # the rest of a line from history, in grey; Right takes it

[highlight]
enabled = true
command = "green"    # a command that exists
unknown = "red"      # one that does not
keyword = "magenta"  # if, for, done, ...
string = "yellow"
variable = "cyan"
operator = "bold"    # |, &&, ;, redirections
option = "blue"      # -x, --flag
comment = "dim"
path = "underline"   # an argument naming a file that exists
error = "bold red"   # an unclosed quote, and the like
bracket = "bold reverse"  # the bracket matching the one at the cursor
suggestion = "dim"

[history]
size = 1000          # commands kept; $HISTSIZE overrides it
ignore_dups = true   # don't keep a command repeating the one before
ignore_space = true  # don't keep a command starting with a space
"#;

/// Where the configuration file is, if there is a home to put it in.
#[must_use]
pub fn path(env: &Env) -> Option<std::path::PathBuf> {
    let base = env
        .get("XDG_CONFIG_HOME")
        .filter(|d| !d.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            env.get("HOME")
                .filter(|h| !h.is_empty())
                .map(|h| std::path::Path::new(&h).join(".config"))
        })?;
    Some(base.join("swagsh").join("config.toml"))
}

/// The configuration, and a note on the file if there is one to make.
///
/// The note says what was wrong with the file, or that it was just
/// written. A file that cannot be used leaves every setting at its
/// default; a missing one is written from [`TEMPLATE`].
#[must_use]
pub fn load(env: &Env) -> (Config, Option<String>) {
    let Some(path) = path(env) else {
        return (Config::default(), None);
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let written = path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(&path, TEMPLATE));
            let note = written.map_or_else(
                |e| format!("{}: could not write it: {e}", path.display()),
                |()| format!("wrote the default settings to {}", path.display()),
            );
            return (Config::default(), Some(note));
        }
        Err(e) => return (Config::default(), Some(format!("{}: {e}", path.display()))),
    };
    let config: Config = match toml::from_str(&text) {
        Ok(config) => config,
        Err(e) => {
            let message = e.message().to_owned();
            let at = e.span().map(|span| {
                let line = text[..span.start].matches('\n').count() + 1;
                format!(" (line {line})")
            });
            return (
                Config::default(),
                Some(format!(
                    "{}: {message}{}",
                    path.display(),
                    at.unwrap_or_default()
                )),
            );
        }
    };
    match config.highlight.check() {
        Ok(()) => (config, None),
        Err(e) => (Config::default(), Some(format!("{}: {e}", path.display()))),
    }
}

impl Highlight {
    /// Every color, by its key.
    fn colors(&self) -> [(&'static str, &str); 12] {
        [
            ("command", &self.command),
            ("unknown", &self.unknown),
            ("keyword", &self.keyword),
            ("string", &self.string),
            ("variable", &self.variable),
            ("operator", &self.operator),
            ("option", &self.option),
            ("comment", &self.comment),
            ("path", &self.path),
            ("error", &self.error),
            ("bracket", &self.bracket),
            ("suggestion", &self.suggestion),
        ]
    }

    /// An error naming the first color that is not a style.
    fn check(&self) -> Result<(), String> {
        for (key, value) in self.colors() {
            ansi(value).map_err(|e| format!("highlight.{key}: {e}"))?;
        }
        Ok(())
    }
}

/// The ANSI escape sequence that turns style `spec` on (`"bold red"`),
/// empty for an empty spec.
///
/// # Errors
///
/// Returns a message naming a word that is not a color or attribute.
pub fn ansi(spec: &str) -> Result<String, String> {
    let mut codes = Vec::new();
    for word in spec.split_whitespace() {
        codes.push(sgr(word).ok_or_else(|| format!("`{word}` is not a color or attribute"))?);
    }
    Ok(if codes.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", codes.join(";"))
    })
}

/// The SGR parameters for one word of a style.
fn sgr(word: &str) -> Option<String> {
    const COLORS: [&str; 8] = [
        "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
    ];
    let attribute = match word {
        "bold" => Some(1),
        "dim" => Some(2),
        "italic" => Some(3),
        "underline" => Some(4),
        "reverse" => Some(7),
        _ => None,
    };
    if let Some(code) = attribute {
        return Some(code.to_string());
    }
    if let Some(i) = COLORS.iter().position(|&c| c == word) {
        return Some((30 + i).to_string());
    }
    if let Some(i) = word
        .strip_prefix("bright-")
        .and_then(|c| COLORS.iter().position(|&n| n == c))
    {
        return Some((90 + i).to_string());
    }
    if let Ok(n) = word.parse::<u8>() {
        return Some(format!("38;5;{n}"));
    }
    let hex = word.strip_prefix('#').filter(|h| h.len() == 6)?;
    let channel = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
    Some(format!(
        "38;2;{};{};{}",
        channel(0)?,
        channel(2)?,
        channel(4)?
    ))
}

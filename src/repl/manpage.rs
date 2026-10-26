//! The options of a program, read from its man page, for completing them,
//! and whether it takes operands besides them.
//!
//! Man pages come in a few dialects of roff. An option is introduced the
//! same way in most of them: a tag line naming it (`-l, --long`) after
//! `.TP` or `.PP`, an `.IP "tag"`, or mdoc's `.It Fl l`, then the text
//! saying what it does.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// An option of a program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManOption {
    /// Each way of spelling it: `-l` and `--long`.
    pub words: Vec<String>,
    /// How the man page writes it: `-T, --tabsize COLS`.
    pub tag: String,
    /// The start of what it does.
    pub what: String,
}

/// What completion takes from a man page.
#[derive(Debug, Default)]
pub struct Page {
    pub options: Vec<ManOption>,
    /// Whether the synopsis shows operands besides the options (`[FILE]...`).
    pub operands: bool,
}

/// The man page of program `name`, looked for in the man directories that
/// go with `path` (`$PATH`), or `manpath` (`$MANPATH`) when that is set.
#[must_use]
pub fn page(name: &str, path: &str, manpath: Option<&str>) -> Option<Page> {
    if name.is_empty() || name.contains('/') {
        return None;
    }
    let dirs = man_dirs(path, manpath);
    let text = read_page(&find_page(&dirs, name)?, path)?;
    Some(Page {
        options: parse(&text),
        operands: takes_operands(&synopsis(&text), name),
    })
}

/// The text of the SYNOPSIS section, with mdoc's optional parts in
/// brackets as man pages write them.
fn synopsis(text: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in text.lines() {
        let (name, args) = split_macro(line);
        match name {
            Some("SH" | "Sh") => {
                inside = args.trim_matches('"').eq_ignore_ascii_case("synopsis");
            }
            _ if !inside => {}
            Some("Op") => {
                out.push_str(" [");
                out.push_str(&text_of(&format!(".No {args}")));
                out.push(']');
            }
            Some("Fl") => {
                out.push_str(" [");
                out.push_str(&text_of(line));
                out.push(']');
            }
            _ => {
                out.push(' ');
                out.push_str(&text_of(line));
            }
        }
    }
    out
}

/// Whether `synopsis`, the usage of command `name` (a page name such as
/// `git-commit` counts as `git commit`), shows operands besides options:
/// anything left once the optional parts starting with `-` and the
/// command's own name are taken out.
#[must_use]
pub fn takes_operands(synopsis: &str, name: &str) -> bool {
    let mut text = synopsis.to_owned();
    // Innermost bracket first: drop an option, keep what else it holds.
    while let Some(close) = text.find(']') {
        let open = text[..close].rfind('[').unwrap_or(close);
        let inside = text.get(open + 1..close).unwrap_or("").trim().to_owned();
        let keep = if inside.starts_with('-')
            || matches!(
                inside.trim_end_matches('.'),
                "OPTION" | "OPTIONS" | "option" | "options"
            ) {
            String::new()
        } else {
            inside
        };
        text.replace_range(open..=close, &format!(" {keep} "));
    }
    let own: Vec<&str> = name.split('-').collect();
    text.split_whitespace()
        .map(|w| w.trim_matches(|c| c == '.' || c == ','))
        .any(|w| {
            !w.is_empty()
                && !w.starts_with('-')
                && !own.contains(&w)
                && !matches!(w, "|" | "OPTION" | "OPTIONS" | "options")
        })
}

/// The directories man pages are in: `$MANPATH`'s, where an empty entry
/// stands for the usual ones, or the usual ones: `share/man` beside each
/// `bin` directory on `$PATH`.
fn man_dirs(path: &str, manpath: Option<&str>) -> Vec<PathBuf> {
    let usual = || {
        let mut dirs: Vec<PathBuf> = path
            .split(':')
            .filter_map(|dir| Path::new(dir).parent())
            .flat_map(|parent| [parent.join("share/man"), parent.join("man")])
            .collect();
        dirs.push(PathBuf::from("/usr/share/man"));
        dirs
    };
    let mut dirs = manpath
        .filter(|m| !m.is_empty())
        .map_or_else(usual, |manpath| {
            manpath
                .split(':')
                .flat_map(|dir| {
                    if dir.is_empty() {
                        usual()
                    } else {
                        vec![PathBuf::from(dir)]
                    }
                })
                .collect()
        });
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| seen.insert(d.clone()) && d.is_dir());
    dirs
}

const EXTENSIONS: [&str; 5] = ["", ".gz", ".zst", ".xz", ".bz2"];

fn find_page(dirs: &[PathBuf], name: &str) -> Option<PathBuf> {
    for dir in dirs {
        for section in ["1", "8", "6"] {
            for ext in EXTENSIONS {
                let page = dir.join(format!("man{section}/{name}.{section}{ext}"));
                if page.is_file() {
                    return Some(page);
                }
            }
        }
    }
    None
}

/// The text of `page`, uncompressed by the program for its kind, found on
/// `path`. A page that only points at another (`.so man1/other.1`) is
/// followed once.
fn read_page(page: &Path, path: &str) -> Option<String> {
    let text = uncompress(page, path)?;
    let Some(target) = text.trim().strip_prefix(".so ") else {
        return Some(text);
    };
    let root = page.parent()?.parent()?;
    let target = root.join(target.trim());
    EXTENSIONS.iter().find_map(|ext| {
        let file = PathBuf::from(format!("{}{ext}", target.display()));
        file.is_file().then(|| uncompress(&file, path)).flatten()
    })
}

fn uncompress(page: &Path, path: &str) -> Option<String> {
    let program = match page.extension().and_then(|e| e.to_str()) {
        Some("gz") => "gzip",
        Some("zst") => "zstd",
        Some("xz") => "xz",
        Some("bz2") => "bzip2",
        _ => return std::fs::read_to_string(page).ok(),
    };
    let program = path
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(program))
        .find(|p| p.is_file())?;
    let output = Command::new(program)
        .arg("-dc")
        .arg(page)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The macros that start a new paragraph or item, ending the text of the
/// one before.
const BREAKS: &[&str] = &[
    "TP", "IP", "PP", "LP", "P", "HP", "SH", "SS", "It", "Sh", "Ss", "El", "Bl", "sp", "Pp",
];

/// The man macros whose arguments are text of the line.
const TEXT: &[&str] = &["B", "I", "BR", "RB", "IR", "RI", "BI", "IB", "SM"];

/// The same in mdoc, whose macros can also follow each other on one line.
const MDOC_TEXT: &[&str] = &[
    "Nm", "Ar", "Fl", "Cm", "Pa", "Em", "Sy", "Va", "Dq", "Ql", "Xr", "Ev", "Li", "No", "Op",
];

/// The options a man page's roff `text` describes.
#[must_use]
pub fn parse(text: &str) -> Vec<ManOption> {
    let lines: Vec<&str> = text.lines().collect();
    let mut found: Vec<ManOption> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        let (name, args) = split_macro(line);
        let tag = match name {
            Some("TP" | "PP" | "LP" | "P") => {
                let Some(next) = lines.get(i) else { break };
                let tag = text_of(next);
                if !tag.starts_with('-') {
                    continue;
                }
                i += 1;
                tag
            }
            Some("IP") => text_of(&format!(".B {args}")),
            Some("It") => text_of(&format!(".No {args}")),
            // A tag on a line of its own, its text indented under it.
            None if lines
                .get(i)
                .is_some_and(|next| split_macro(next).0 == Some("RS")) =>
            {
                text_of(line)
            }
            _ => continue,
        };
        let tag = tag
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .replace(" ,", ",");
        let words = option_words(&tag);
        if words.is_empty() {
            continue;
        }
        let mut what = String::new();
        while let Some(line) = lines.get(i) {
            let (name, _) = split_macro(line);
            if name.is_some_and(|n| BREAKS.contains(&n)) || what.len() > 80 {
                break;
            }
            i += 1;
            let text = text_of(line);
            if !text.is_empty() {
                if !what.is_empty() {
                    what.push(' ');
                }
                what.push_str(&text);
            }
        }
        if found.iter().any(|o| o.words == words) {
            continue;
        }
        found.push(ManOption {
            words,
            tag,
            what: first_sentence(&what),
        });
    }
    found
}

/// The macro a line calls, if it is a request line, and the rest of it.
fn split_macro(line: &str) -> (Option<&str>, &str) {
    let Some(rest) = line.strip_prefix('.').or_else(|| line.strip_prefix('\'')) else {
        return (None, line);
    };
    let rest = rest.trim_start();
    let end = rest.find([' ', '\t']).unwrap_or(rest.len());
    (Some(&rest[..end]), rest[end..].trim_start())
}

/// The text a line shows: a text macro's arguments, without quotes and
/// font changes; nothing for other requests.
fn text_of(line: &str) -> String {
    let (name, args) = split_macro(line);
    let raw = match name {
        None => line.to_owned(),
        Some(name) if TEXT.contains(&name) => quoted_words(args).join(" "),
        Some(name) if MDOC_TEXT.contains(&name) => mdoc_words(name, args),
        Some(_) => return String::new(),
    };
    clean(&raw).trim().to_owned()
}

/// A text macro's arguments as the words they show. In mdoc, `Fl` puts a
/// `-` before the word after it, and further macro names can follow on the
/// same line (`Fl B Ar bind_interface`).
fn mdoc_words(name: &str, args: &str) -> String {
    let mut out = String::new();
    let mut dash = name == "Fl";
    let mut glue = false;
    for word in quoted_words(args) {
        if MDOC_TEXT.contains(&word.as_str()) {
            dash = word == "Fl";
            continue;
        }
        if word == "Ns" {
            glue = true;
            continue;
        }
        if !out.is_empty() && !glue && !matches!(word.as_str(), "," | "." | ")" | "]") {
            out.push(' ');
        }
        glue = false;
        if dash {
            out.push('-');
        }
        out.push_str(&word);
    }
    out
}

/// `args` split into words, a `"..."` counting as one.
fn quoted_words(args: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut chars = args.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '"' {
            chars.next();
            words.push(chars.by_ref().take_while(|&c| c != '"').collect());
        } else {
            let mut word = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                word.push(c);
                chars.next();
            }
            words.push(word);
        }
    }
    words
}

/// `text` without roff's escapes: font changes go, and the special
/// characters become what they show.
fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('f') => match chars.next() {
                Some('(') => {
                    chars.next();
                    chars.next();
                }
                Some('[') => {
                    chars.by_ref().find(|&c| c == ']');
                }
                _ => {}
            },
            Some('(') => {
                let name: String = chars.by_ref().take(2).collect();
                out.push_str(special(&name));
            }
            Some('[') => {
                let name: String = chars.by_ref().take_while(|&c| c != ']').collect();
                out.push_str(special(&name));
            }
            Some('*') => {
                if chars.next() == Some('(') {
                    chars.next();
                    chars.next();
                }
            }
            Some('-') => out.push('-'),
            Some('e' | '\\') => out.push('\\'),
            Some(' ' | '~') => out.push(' '),
            Some('&' | '%' | 'c' | '|' | '^' | ')' | '/' | ',') | None => {}
            Some(other) => out.push(other),
        }
    }
    out
}

fn special(name: &str) -> &'static str {
    match name {
        "em" | "en" | "mi" | "hy" => "-",
        "aq" | "cq" | "oq" => "'",
        "lq" | "rq" | "dq" => "\"",
        "rs" => "\\",
        "ga" => "`",
        "ti" => "~",
        _ => "",
    }
}

/// The options a tag names: `-l, --long` gives `-l` and `--long`, and
/// `--color[=WHEN]` gives `--color`.
fn option_words(tag: &str) -> Vec<String> {
    let mut words = Vec::new();
    for part in tag.split([',', '|']) {
        let Some(token) = part.split_whitespace().next() else {
            continue;
        };
        let word = token.split(['=', '[', '<']).next().unwrap_or(token);
        let name = word.trim_start_matches('-');
        let dashes = word.len() - name.len();
        let valid = (1..=2).contains(&dashes)
            && name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '?' || c == '#')
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_?#.".contains(c));
        if valid && !words.iter().any(|w| w == word) {
            words.push(word.to_owned());
        }
    }
    words
}

/// The first sentence of `text`, cut short when long.
fn first_sentence(text: &str) -> String {
    let end = text
        .match_indices(". ")
        .next()
        .map_or(text.len(), |(i, _)| i);
    let sentence = text[..end].trim_end_matches('.');
    if sentence.chars().count() <= 60 {
        return sentence.to_owned();
    }
    let cut: String = sentence.chars().take(59).collect();
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<(Vec<String>, String)> {
        parse(text).into_iter().map(|o| (o.words, o.what)).collect()
    }

    fn w(list: &[&str], what: &str) -> (Vec<String>, String) {
        (
            list.iter().map(|&s| s.to_owned()).collect(),
            what.to_owned(),
        )
    }

    #[test]
    fn tp_tags() {
        let page = ".TP\n\\fB\\-l\\fR, \\fB\\-\\-long\\fR\nDisplay detailed information.\n\
                    .TP\n\\fB\\-T\\fR, \\fB\\-\\-tabsize\\fR \\fI<COLS>\\fR\nAssume tab stops.\n";
        assert_eq!(
            words(page),
            [
                w(&["-l", "--long"], "Display detailed information"),
                w(&["-T", "--tabsize"], "Assume tab stops")
            ]
        );
    }

    #[test]
    fn pp_tags_from_asciidoc() {
        let page = ".PP\n\\fB\\-a\\fR, \\fB\\-\\-all\\fR\n.RS 4\nStage files. More text.\n.RE\n\
                    .PP\nSome prose\n";
        assert_eq!(words(page), [w(&["-a", "--all"], "Stage files")]);
    }

    #[test]
    fn mdoc_items() {
        let page = ".It Fl B Ar bind_interface\nBind to the address of\n.Ar bind_interface\n\
                    on the host.\n.It Fl 4\nForces IPv4.\n";
        assert_eq!(
            words(page),
            [
                w(&["-B"], "Bind to the address of bind_interface on the host"),
                w(&["-4"], "Forces IPv4")
            ]
        );
    }

    #[test]
    fn tags_before_an_indent() {
        let page = ".SS OPTIONS\n\\fB\\-s\\fP, \\fB\\-\\-case\\-sensitive\\fP\n.RS 4\n\
                    Search case sensitively.\n.RE\n.TP\n.BR \\-R \", \" \\-\\^\\-deref\nFollow links.\n";
        assert_eq!(
            parse(page)
                .into_iter()
                .map(|o| (o.tag, o.what))
                .collect::<Vec<_>>(),
            [
                (
                    "-s, --case-sensitive".to_owned(),
                    "Search case sensitively".to_owned()
                ),
                ("-R, --deref".to_owned(), "Follow links".to_owned())
            ]
        );
    }

    #[test]
    fn ip_tags() {
        let page = ".IP \"\\-a, \\-\\-append\"\nAppend to the file.\n.IP DICT\nDictionaries.\n";
        assert_eq!(words(page), [w(&["-a", "--append"], "Append to the file")]);
    }

    #[test]
    fn operands() {
        assert!(!takes_operands(
            "clear [ -x ] [ -T terminal-type ] clear -V",
            "clear"
        ));
        assert!(takes_operands("ls [OPTION]... [FILE]...", "ls"));
        assert!(!takes_operands("pwd [OPTIONS]", "pwd"));
        assert!(takes_operands("cd [OPTIONS] [DIR]", "cd"));
        assert!(takes_operands(
            "git commit [-a | --interactive] [-m <msg>] [--] [<pathspec>...]",
            "git-commit"
        ));
        let mdoc = ".Sh SYNOPSIS\n.Nm ssh\n.Op Fl 46A\n.Op Fl B Ar bind_interface\n.Ar destination\n.Sh DESCRIPTION\nx\n";
        assert!(takes_operands(&synopsis(mdoc), "ssh"));
        let none = ".Sh SYNOPSIS\n.Nm true\n.Op Fl v\n.Sh DESCRIPTION\n";
        assert!(!takes_operands(&synopsis(none), "true"));
    }

    #[test]
    fn values_are_not_part_of_the_word() {
        assert_eq!(option_words("--color[=WHEN]"), ["--color"]);
        assert_eq!(option_words("--format=FORMAT, -f"), ["--format", "-f"]);
        assert_eq!(option_words("- (a dash)"), Vec::<String>::new());
    }
}

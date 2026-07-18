//! `getopts optstring name [arg ...]`: parses positional-parameter-shaped
//! options one flag per call, POSIX-style.
//!
//! `optstring`/`name`/`arg...` are `getopts`'s own argument *shape* (three
//! plain positionals, the last variadic), so that part goes through clap
//! like every other builtin's argument list. What clap can't do anything
//! about is what `getopts` actually computes once those are in hand: which
//! option character `optstring` recognizes, whether it takes an argument,
//! and where `$OPTIND` resumes next time: that's `getopts`'s whole reason
//! to exist, not argument parsing, so it stays hand-written below like
//! any other option-parsing implementation would.

use anyhow::Result;
use clap::Parser;

use crate::errfmt::emit;
use crate::eval::Shell;
use crate::jobs::ExitStatus;

use super::Builtin;

#[derive(Parser)]
#[command(
    name = "getopts",
    about = "Parse option arguments",
    trailing_var_arg = true
)]
pub struct GetoptsBuiltin {
    #[arg(allow_hyphen_values = true)]
    optstring: String,
    #[arg(allow_hyphen_values = true)]
    name: String,
    /// Parse these instead of the positional parameters, if given
    #[arg(allow_hyphen_values = true)]
    arg: Vec<String>,
}

/// Ends option processing for this call: advances `$OPTIND` to
/// `next_optind`, resets the intra-word offset, sets `name` to `"?"`, and
/// unsets `OPTARG`: the state left behind once `getopts` runs out of
/// options to report (a real end, `--`, or a bounds/state inconsistency
/// that shouldn't be able to happen but shouldn't panic if it somehow does).
fn end_of_options(shell: &mut Shell, name: &str, next_optind: i64) -> ExitStatus {
    shell.env.set("OPTIND", next_optind.to_string());
    shell.getopts_state.optind = next_optind;
    shell.getopts_state.offset = 0;
    shell.env.set(name, "?");
    shell.env.unset_var("OPTARG");
    ExitStatus::FAILURE
}

impl Builtin for GetoptsBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        let name = self.name.as_str();
        let silent = self.optstring.starts_with(':');
        let spec = self.optstring.trim_start_matches(':');
        let report_errors = shell.env.get("OPTERR").as_deref() != Some("0");

        let arglist: Vec<String> = if self.arg.is_empty() {
            shell.env.positional_args().to_vec()
        } else {
            self.arg
        };

        let optind = shell
            .env
            .get("OPTIND")
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(1)
            .max(1);
        if shell.getopts_state.optind != optind {
            shell.getopts_state.offset = 0;
        }
        let idx = usize::try_from(optind - 1).unwrap_or(0);
        let pos = shell.getopts_state.offset;

        let Some(word) = arglist.get(idx) else {
            return Ok(end_of_options(shell, name, optind));
        };

        if pos == 0 {
            if word == "--" {
                return Ok(end_of_options(shell, name, optind + 1));
            }
            if !word.starts_with('-') || word == "-" {
                return Ok(end_of_options(shell, name, optind));
            }
        }

        let chars: Vec<char> = word.chars().skip(1).collect();
        let Some(&opt) = chars.get(pos) else {
            return Ok(end_of_options(shell, name, optind));
        };
        let is_last_char = pos + 1 >= chars.len();
        let next_optind_same_word = if is_last_char { optind + 1 } else { optind };

        let Some(spec_idx) = spec.find(opt) else {
            shell.env.set("OPTIND", next_optind_same_word.to_string());
            shell.getopts_state.optind = next_optind_same_word;
            shell.getopts_state.offset = if is_last_char { 0 } else { pos + 1 };
            if silent {
                shell.env.set(name, "?");
                shell.env.set("OPTARG", opt.to_string());
            } else {
                if report_errors {
                    emit(format!("illegal option -- {opt}"));
                }
                shell.env.set(name, "?");
                shell.env.unset_var("OPTARG");
            }
            return Ok(ExitStatus::SUCCESS);
        };

        let takes_arg = spec.as_bytes().get(spec_idx + 1) == Some(&b':');
        if !takes_arg {
            shell.env.set("OPTIND", next_optind_same_word.to_string());
            shell.getopts_state.optind = next_optind_same_word;
            shell.getopts_state.offset = if is_last_char { 0 } else { pos + 1 };
            shell.env.set(name, opt.to_string());
            shell.env.unset_var("OPTARG");
            return Ok(ExitStatus::SUCCESS);
        }

        if !is_last_char {
            // `-oARG`: the rest of this word is the argument.
            let optarg: String = chars[pos + 1..].iter().collect();
            shell.env.set("OPTARG", optarg);
            shell.env.set("OPTIND", (optind + 1).to_string());
            shell.getopts_state.optind = optind + 1;
            shell.getopts_state.offset = 0;
            shell.env.set(name, opt.to_string());
            return Ok(ExitStatus::SUCCESS);
        }

        if let Some(argval) = arglist.get(idx + 1) {
            // `-o ARG`: the next word is the argument.
            shell.env.set("OPTARG", argval.clone());
            shell.env.set("OPTIND", (optind + 2).to_string());
            shell.getopts_state.optind = optind + 2;
            shell.getopts_state.offset = 0;
            shell.env.set(name, opt.to_string());
            return Ok(ExitStatus::SUCCESS);
        }

        // Required argument missing.
        shell.env.set("OPTIND", (optind + 1).to_string());
        shell.getopts_state.optind = optind + 1;
        shell.getopts_state.offset = 0;
        if silent {
            shell.env.set(name, ":");
            shell.env.set("OPTARG", opt.to_string());
        } else {
            if report_errors {
                emit(format!("option requires an argument -- {opt}"));
            }
            shell.env.set(name, "?");
            shell.env.unset_var("OPTARG");
        }
        Ok(ExitStatus::SUCCESS)
    }
}

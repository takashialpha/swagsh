use anyhow::{Result, bail};

use crate::eval::Shell;
use crate::jobs::ExitStatus;

pub fn builtin_read(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) => return Ok(ExitStatus::FAILURE),
        Ok(_) => {}
        Err(e) => bail!("read: {e}"),
    }
    let line = line.trim_end_matches('\n').trim_end_matches('\r');
    let vars: &[&str] = if args.is_empty() { &["REPLY"] } else { args };
    let ifs = shell.env.get("IFS").unwrap_or_else(|| " \t\n".to_owned());
    let is_ifs = |c: char| ifs.contains(c);
    let mut rest = line.trim_start_matches(is_ifs);
    for (i, &var) in vars.iter().enumerate() {
        let is_last = i + 1 == vars.len();
        if is_last {
            shell.env.set(var, rest.trim_end_matches(is_ifs));
        } else {
            let field_end = rest.find(is_ifs).unwrap_or(rest.len());
            shell.env.set(var, &rest[..field_end]);
            rest = rest[field_end..].trim_start_matches(is_ifs);
        }
    }
    Ok(ExitStatus::SUCCESS)
}

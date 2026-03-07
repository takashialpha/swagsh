use app_base::run;
use clap::Parser;
use swagsh::{app, cli::Cli};

fn main() {
    let cli = Cli::parse();

    if let Err(e) = run(app::SwagSH, None, cli) {
        eprintln!("{}", e);
        std::process::exit(1);
    }
}

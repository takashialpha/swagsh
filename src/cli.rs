use crate::APP_NAME;
use app_base::app::ConfigPath;
use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Parser, Clone)]
#[command(
    name = APP_NAME,
    version,
    about = "A sleek, high-performance POSIX-compatible shell built in Rust for speed and reliability.
    Name inspired by 'swag' slang for stylish flair."
)]
pub struct Cli {}

impl ConfigPath for Cli {
    fn config_path(&self) -> Option<PathBuf> {
        None // here should be real config path
    }
}

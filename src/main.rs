mod app;
mod cli;
mod core;
mod fs_source;
mod index;
mod service;
mod tui;

use anyhow::Result;

fn main() -> Result<()> {
    let cli = cli::Cli::parse_args();
    app::run(cli)
}

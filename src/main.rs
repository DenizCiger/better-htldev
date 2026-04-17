mod app;
mod cli;
mod core;
mod credentials;
mod fs_source;
mod index;
mod scraper;
mod service;
mod tui;

use anyhow::Result;

fn main() -> Result<()> {
    let cli = cli::Cli::parse_args();
    app::run(cli)
}

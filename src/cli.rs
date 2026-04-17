use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "htldev",
    version,
    about = "Offline-first search CLI for the local htl.dev markdown mirror"
)]
pub struct Cli {
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        help = "Path to the markdown mirror root"
    )]
    pub source: Option<PathBuf>,
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        help = "Path to the SQLite index database"
    )]
    pub index_db: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Index(IndexArgs),
    Search(SearchArgs),
    Show(TargetArgs),
    Open(TargetArgs),
    Doctor,
    Tui,
    Scrape(ScrapeArgs),
}

#[derive(Debug, Args)]
pub struct IndexArgs {
    #[arg(long, value_name = "PATH", help = "Mirror path to index")]
    pub source: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct SearchArgs {
    #[arg(value_name = "QUERY")]
    pub query: String,
    #[arg(long, help = "Use regex file scanning instead of FTS")]
    pub regex: bool,
    #[arg(long, value_name = "GLOB", help = "Restrict matches to matching paths")]
    pub path: Option<String>,
    #[arg(long, default_value_t = 20, help = "Maximum number of results")]
    pub limit: usize,
}

#[derive(Debug, Args)]
pub struct TargetArgs {
    #[arg(value_name = "DOC")]
    pub target: String,
}

#[derive(Debug, Args)]
pub struct ScrapeArgs {
    #[arg(long, help = "Re-check all files for updates")]
    pub sync: bool,
    #[arg(long, help = "Delete state and re-download everything")]
    pub fresh: bool,
    #[arg(long, value_name = "USER", help = "HTL username (or set HTL_USERNAME env var)")]
    pub username: Option<String>,
    #[arg(long, value_name = "PASS", help = "HTL password (or set HTL_PASSWORD env var)")]
    pub password: Option<String>,
}

impl Cli {
    pub fn parse_args() -> Self {
        Self::parse()
    }
}

use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::cli::{Cli, Command as CliCommand};
use crate::core::{SearchHit, SearchMode, SearchQuery, default_source_path};
use crate::fs_source::FilesystemSource;
use crate::scraper::HtlScraper;
use crate::service::SearchService;
use crate::tui;

pub fn run(cli: Cli) -> Result<()> {
    match cli.command {
        CliCommand::Index(args) => {
            let source = args
                .source
                .or(cli.source)
                .unwrap_or_else(default_source_path);
            run_index(source, cli.index_db)
        }
        CliCommand::Search(args) => {
            let source = cli.source.unwrap_or_else(default_source_path);
            let query = SearchQuery {
                text: args.query,
                mode: if args.regex {
                    SearchMode::Regex
                } else {
                    SearchMode::FullText
                },
                path_filter: args.path,
                limit: args.limit,
            };
            run_search(source, cli.index_db, query)
        }
        CliCommand::Show(args) => {
            let source = cli.source.unwrap_or_else(default_source_path);
            run_show(source, cli.index_db, &args.target)
        }
        CliCommand::Open(args) => {
            let source = cli.source.unwrap_or_else(default_source_path);
            run_open(source, cli.index_db, &args.target)
        }
        CliCommand::Doctor => {
            let source = cli.source.unwrap_or_else(default_source_path);
            run_doctor(source, cli.index_db)
        }
        CliCommand::Tui => {
            let source = cli.source.unwrap_or_else(default_source_path);
            let service = SearchService::new(source, cli.index_db);
            tui::run(service)
        }
        CliCommand::Scrape(args) => {
            let username = args
                .username
                .or_else(|| std::env::var("HTL_USERNAME").ok())
                .unwrap_or_else(|| {
                    eprint!("HTL username: ");
                    let mut s = String::new();
                    std::io::stdin().read_line(&mut s).ok();
                    s.trim().to_string()
                });

            let password = args
                .password
                .or_else(|| std::env::var("HTL_PASSWORD").ok())
                .unwrap_or_else(|| {
                    rpassword::prompt_password("HTL password: ").unwrap_or_default()
                });

            let scraper = HtlScraper::new(args.sync)?;
            scraper.run(&username, &password, !args.fresh)
        }
    }
}

fn run_index(source: PathBuf, index_db: Option<PathBuf>) -> Result<()> {
    let service = SearchService::new(source, index_db);
    let stats = service.index_documents()?;
    println!("Indexed source: {}", service.source().display());
    println!("Database: {}", service.index_path().display());
    println!("documents: {}", stats.indexed);
    println!("updated: {}", stats.updated);
    println!("unchanged: {}", stats.unchanged);
    println!("removed: {}", stats.removed);
    Ok(())
}

fn run_search(source: PathBuf, index_db: Option<PathBuf>, query: SearchQuery) -> Result<()> {
    let service = SearchService::new(source.clone(), index_db);
    service.ensure_source_exists()?;

    let hits = if query.mode == SearchMode::Regex {
        FilesystemSource::new(source).regex_search(&query)?
    } else {
        service.search(&query.text, query.limit)?
    };

    if hits.is_empty() {
        println!("No matches found.");
        return Ok(());
    }

    for (index, hit) in hits.iter().enumerate() {
        print_hit(index + 1, hit);
    }

    Ok(())
}

fn run_show(source: PathBuf, index_db: Option<PathBuf>, target: &str) -> Result<()> {
    let service = SearchService::new(source, index_db);
    service.ensure_source_exists()?;

    if let Some(preview) = service.preview_for_target(target)? {
        println!("{}", preview.title);
        println!("{}", preview.path);
        println!("status: {}", preview.status.as_str());
        if let Some(modified) = &preview.modified_at {
            println!("modified_at: {}", modified);
        }
        if !preview.headings.is_empty() {
            println!("headings: {}", preview.headings.join(" | "));
        }
        if !preview.links.is_empty() {
            println!("links: {}", preview.links.len());
        }
        println!();

        for line in preview.body_lines {
            println!("{line}");
        }
        return Ok(());
    }

    if let Some(content) = service.read_direct_file(target)? {
        println!("{content}");
        return Ok(());
    }

    bail!("document not found: {target}");
}

fn run_open(source: PathBuf, index_db: Option<PathBuf>, target: &str) -> Result<()> {
    let service = SearchService::new(source, index_db);
    service.ensure_source_exists()?;
    service.open_target(target)?;
    println!("Opened {target}");
    Ok(())
}

fn run_doctor(source: PathBuf, index_db: Option<PathBuf>) -> Result<()> {
    let service = SearchService::new(source.clone(), index_db);
    println!("source: {}", source.display());
    println!("source_exists: {}", source.exists());
    println!("index_db: {}", service.index_path().display());
    println!("index_exists: {}", service.index_exists());
    println!("indexed_documents: {}", service.document_count()?);

    if source.exists() {
        let count = FilesystemSource::new(source).scan_documents()?.len();
        println!("source_markdown_files: {count}");
    }

    Ok(())
}

fn print_hit(position: usize, hit: &SearchHit) {
    println!("{}. {} [{}]", position, hit.title, hit.doc_id);
    println!("   {}", hit.path);
    if let Some(score) = hit.score {
        println!("   score: {:.3}", score);
    }
    if !hit.snippet.trim().is_empty() {
        println!("   {}", hit.snippet.trim());
    }
    for line in &hit.line_matches {
        println!("   {}: {}", line.line_number, line.line.trim());
    }
    println!();
}

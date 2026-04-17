use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;
use walkdir::WalkDir;

use crate::core::{
    DocumentRecord, DocumentStatus, LineMatch, SearchHit, SearchMode, SearchQuery, content_hash,
    detect_status, display_path_from_rel, extract_headings, extract_links, extract_title,
    modified_at_string,
};

pub struct FilesystemSource {
    root: PathBuf,
}

impl FilesystemSource {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn scan_documents(&self) -> Result<Vec<DocumentRecord>> {
        let mut docs = Vec::new();

        for entry in WalkDir::new(&self.root).into_iter().filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }

            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }

            docs.push(self.read_document(path)?);
        }

        docs.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));
        Ok(docs)
    }

    fn read_document(&self, path: &Path) -> Result<DocumentRecord> {
        let rel_path = path
            .strip_prefix(&self.root)
            .with_context(|| format!("failed to strip source root from {}", path.display()))?;
        let display_path = display_path_from_rel(rel_path);

        let raw_bytes =
            fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
        let content = String::from_utf8_lossy(&raw_bytes).into_owned();
        let status = detect_status(&content);
        let metadata = fs::metadata(path).ok();

        let normalized_body = match status {
            DocumentStatus::PermissionDenied => String::new(),
            _ => content.clone(),
        };

        Ok(DocumentRecord {
            rel_path: rel_path.to_string_lossy().to_string(),
            display_path,
            title: extract_title(&content, rel_path),
            body: normalized_body,
            headings: extract_headings(&content),
            links: extract_links(&content),
            status,
            modified_at: metadata.as_ref().and_then(modified_at_string),
            hash: content_hash(&content),
        })
    }

    pub fn regex_search(&self, query: &SearchQuery) -> Result<Vec<SearchHit>> {
        let path_matcher = compile_glob_filter(query.path_filter.as_deref())?;
        let regex = Regex::new(&query.text)
            .with_context(|| format!("invalid regex query: {}", query.text))?;
        let mut hits = Vec::new();

        for document in self.scan_documents()? {
            if document.status != DocumentStatus::Ok {
                continue;
            }
            if !path_matches(path_matcher.as_ref(), &document.display_path) {
                continue;
            }

            let line_matches: Vec<LineMatch> = document
                .body
                .lines()
                .enumerate()
                .filter_map(|(index, line)| {
                    regex.is_match(line).then(|| LineMatch {
                        line_number: index + 1,
                        line: line.to_string(),
                    })
                })
                .take(5)
                .collect();

            if line_matches.is_empty() {
                continue;
            }

            let snippet = line_matches
                .first()
                .map(|line| line.line.clone())
                .unwrap_or_default();

            hits.push(SearchHit {
                doc_id: 0,
                path: document.display_path,
                title: document.title,
                score: None,
                snippet,
                line_matches,
            });

            if hits.len() >= query.limit {
                break;
            }
        }

        Ok(hits)
    }
}

pub fn compile_glob_filter(pattern: Option<&str>) -> Result<Option<GlobSet>> {
    let Some(pattern) = pattern else {
        return Ok(None);
    };

    let mut builder = GlobSetBuilder::new();
    builder.add(Glob::new(pattern).with_context(|| format!("invalid glob: {pattern}"))?);
    Ok(Some(builder.build()?))
}

pub fn path_matches(matcher: Option<&GlobSet>, path: &str) -> bool {
    matcher.is_none_or(|globset| globset.is_match(path))
}

#[allow(dead_code)]
pub fn search_mode_name(mode: SearchMode) -> &'static str {
    match mode {
        SearchMode::FullText => "full-text",
        SearchMode::Regex => "regex",
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use crate::core::{DocumentStatus, SearchMode, SearchQuery};

    use super::FilesystemSource;

    #[test]
    fn regex_search_respects_status_and_path_filter() {
        let dir = tempdir().expect("temp dir");
        fs::create_dir_all(dir.path().join("nested")).expect("mkdir");
        fs::write(
            dir.path().join("nested").join("docker.md"),
            "# Docker\ncompose up\n",
        )
        .expect("write");
        fs::write(
            dir.path().join("gated.md"),
            "Error parsing markdown: Error: You do not have the required permissions to view this content.",
        )
        .expect("write");

        let source = FilesystemSource::new(dir.path().to_path_buf());
        let docs = source.scan_documents().expect("scan");
        assert_eq!(docs.len(), 2);
        assert!(
            docs.iter()
                .any(|doc| doc.status == DocumentStatus::PermissionDenied)
        );

        let hits = source
            .regex_search(&SearchQuery {
                text: "compose".to_string(),
                mode: SearchMode::Regex,
                path_filter: Some("nested/*.md".to_string()),
                limit: 10,
            })
            .expect("regex search");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Docker");
    }
}

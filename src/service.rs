use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::core::{DocumentStatus, SearchHit, SearchMode, SearchQuery, default_index_path};
use crate::fs_source::FilesystemSource;
use crate::index::{IndexStats, IndexStore, StoredDocument};

#[derive(Debug, Clone)]
pub struct PreviewDocument {
    pub title: String,
    pub path: String,
    pub status: DocumentStatus,
    pub modified_at: Option<String>,
    pub headings: Vec<String>,
    pub links: Vec<String>,
    pub body_lines: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SearchService {
    source: PathBuf,
    index_path: PathBuf,
}

impl SearchService {
    pub fn new(source: PathBuf, index_db: Option<PathBuf>) -> Self {
        Self {
            source,
            index_path: index_db.unwrap_or_else(default_index_path),
        }
    }

    pub fn source(&self) -> &Path {
        &self.source
    }

    pub fn index_path(&self) -> &Path {
        &self.index_path
    }

    pub fn ensure_source_exists(&self) -> Result<()> {
        if self.source.exists() {
            Ok(())
        } else {
            bail!("source directory does not exist: {}", self.source.display())
        }
    }

    pub fn index_exists(&self) -> bool {
        self.index_path.exists()
    }

    pub fn document_count(&self) -> Result<i64> {
        let store = self.open_store()?;
        store.document_count()
    }

    pub fn index_documents(&self) -> Result<IndexStats> {
        self.ensure_source_exists()?;
        let fs_source = FilesystemSource::new(self.source.clone());
        let docs = fs_source.scan_documents()?;
        let mut store = self.open_store()?;
        store.index_documents(&docs)
    }

    pub fn search(&self, query_text: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let store = self.open_store()?;
        if query_text.trim().is_empty() {
            store.list_documents(limit)
        } else {
            store.search(&SearchQuery {
                text: query_text.to_string(),
                mode: SearchMode::FullText,
                path_filter: None,
                limit,
            })
        }
    }

    pub fn preview_for_hit(&self, hit: &SearchHit) -> Result<Option<PreviewDocument>> {
        self.preview_by_id(hit.doc_id)
    }

    pub fn preview_by_id(&self, doc_id: i64) -> Result<Option<PreviewDocument>> {
        let store = self.open_store()?;
        Ok(store.get_document(doc_id)?.map(map_preview))
    }

    pub fn preview_for_target(&self, target: &str) -> Result<Option<PreviewDocument>> {
        let store = self.open_store()?;
        Ok(store.resolve_document(target)?.map(map_preview))
    }

    pub fn open_hit(&self, hit: &SearchHit) -> Result<()> {
        self.open_target(&hit.doc_id.to_string())
    }

    pub fn open_target(&self, target: &str) -> Result<()> {
        let store = self.open_store()?;
        let path = if let Some(doc) = store.resolve_document(target)? {
            self.source.join(doc.rel_path)
        } else {
            let candidate = PathBuf::from(target);
            if candidate.is_absolute() {
                candidate
            } else {
                self.source.join(candidate)
            }
        };

        if !path.exists() {
            bail!("file does not exist: {}", path.display());
        }

        open_with_default_app(&path)?;
        Ok(())
    }

    pub fn read_direct_file(&self, target: &str) -> Result<Option<String>> {
        let path = self.source.join(target);
        if !path.exists() {
            return Ok(None);
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        Ok(Some(content))
    }

    fn open_store(&self) -> Result<IndexStore> {
        IndexStore::open(Some(self.index_path.clone()))
    }
}

fn map_preview(doc: StoredDocument) -> PreviewDocument {
    let body_lines = match doc.status {
        DocumentStatus::Ok => doc.body.lines().map(ToString::to_string).collect(),
        DocumentStatus::PermissionDenied => vec![
            "This document is indexed as permission-gated and does not store searchable body text."
                .to_string(),
        ],
        DocumentStatus::ParseError => {
            vec!["This document was indexed with a parse error.".to_string()]
        }
    };

    PreviewDocument {
        title: doc.title,
        path: doc.display_path,
        status: doc.status,
        modified_at: doc.modified_at,
        headings: doc.headings,
        links: doc.links,
        body_lines,
    }
}

pub fn open_in_browser(display_path: &str) -> Result<()> {
    let encoded: String = display_path
        .chars()
        .flat_map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '~') {
                vec![c]
            } else {
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                s.bytes().flat_map(|b| {
                    format!("%{:02X}", b).chars().collect::<Vec<_>>()
                }).collect()
            }
        })
        .collect();
    let url = format!("https://htl.dev/md/{}", encoded);
    open_with_default_app(Path::new(&url))
}

pub fn open_with_default_app(path: &Path) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        let path_string = path.to_string_lossy().into_owned();
        Command::new("cmd")
            .args(["/C", "start", "", &path_string])
            .spawn()
            .with_context(|| format!("failed to open {}", path.display()))?;
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(path)
            .spawn()
            .with_context(|| format!("failed to open {}", path.display()))?;
        return Ok(());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open")
            .arg(path)
            .spawn()
            .with_context(|| format!("failed to open {}", path.display()))?;
        return Ok(());
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use crate::core::DocumentStatus;

    use super::SearchService;

    #[test]
    fn search_and_preview_use_same_document() {
        let dir = tempdir().expect("temp dir");
        fs::write(
            dir.path().join("docker.md"),
            "# Docker Compose\n\nUse docker compose for local dev.\n",
        )
        .expect("write doc");

        let service = SearchService::new(
            dir.path().to_path_buf(),
            Some(dir.path().join("index.sqlite3")),
        );
        service.index_documents().expect("index docs");

        let hits = service.search("docker", 10).expect("search");
        assert_eq!(hits.len(), 1);
        let preview = service
            .preview_for_hit(&hits[0])
            .expect("preview")
            .expect("preview exists");
        assert_eq!(preview.title, hits[0].title);
        assert!(
            preview
                .body_lines
                .iter()
                .any(|line| line.contains("docker compose"))
        );
    }

    #[test]
    fn permission_denied_preview_shows_message() {
        let dir = tempdir().expect("temp dir");
        fs::write(
            dir.path().join("gated.md"),
            "Error parsing markdown: Error: You do not have the required permissions to view this content.",
        )
        .expect("write doc");

        let service = SearchService::new(
            dir.path().to_path_buf(),
            Some(dir.path().join("index.sqlite3")),
        );
        service.index_documents().expect("index docs");
        let preview = service
            .preview_for_target("gated.md")
            .expect("preview")
            .expect("preview exists");
        assert_eq!(preview.status, DocumentStatus::PermissionDenied);
        assert!(preview.body_lines[0].contains("permission-gated"));
    }
}

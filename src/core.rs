use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub const DEFAULT_SOURCE_STR: &str =
    r"C:\Users\Deniz\Documents\School\WMC\htl-dev-scraper\htl_dev_backup\md";
pub const DEFAULT_INDEX_DIR: &str = ".htldev";
pub const DEFAULT_INDEX_FILE: &str = "index.sqlite3";
const PERMISSION_DENIED_PREFIX: &str =
    "Error parsing markdown: Error: You do not have the required permissions to view this content.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentStatus {
    Ok,
    PermissionDenied,
    ParseError,
}

impl DocumentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "Ok",
            Self::PermissionDenied => "PermissionDenied",
            Self::ParseError => "ParseError",
        }
    }

    pub fn from_db(value: &str) -> Self {
        match value {
            "PermissionDenied" => Self::PermissionDenied,
            "ParseError" => Self::ParseError,
            _ => Self::Ok,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DocumentRecord {
    pub rel_path: String,
    pub display_path: String,
    pub title: String,
    pub body: String,
    pub headings: Vec<String>,
    pub links: Vec<String>,
    pub status: DocumentStatus,
    pub modified_at: Option<String>,
    pub hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    FullText,
    Regex,
}

#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub text: String,
    pub mode: SearchMode,
    pub path_filter: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub doc_id: i64,
    pub path: String,
    pub title: String,
    pub score: Option<f64>,
    pub snippet: String,
    pub line_matches: Vec<LineMatch>,
}

#[derive(Debug, Clone)]
pub struct LineMatch {
    pub line_number: usize,
    pub line: String,
}

pub fn default_source_path() -> PathBuf {
    PathBuf::from(DEFAULT_SOURCE_STR)
}

pub fn default_index_path() -> PathBuf {
    PathBuf::from(DEFAULT_INDEX_DIR).join(DEFAULT_INDEX_FILE)
}

pub fn display_path_from_rel(rel_path: &Path) -> String {
    rel_path.to_string_lossy().replace('\\', "/")
}

pub fn modified_at_string(metadata: &std::fs::Metadata) -> Option<String> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs().to_string())
}

pub fn content_hash(content: &str) -> String {
    blake3::hash(content.as_bytes()).to_hex().to_string()
}

pub fn detect_status(content: &str) -> DocumentStatus {
    let trimmed = content.trim();
    if trimmed.starts_with(PERMISSION_DENIED_PREFIX) {
        DocumentStatus::PermissionDenied
    } else {
        DocumentStatus::Ok
    }
}

pub fn extract_title(content: &str, fallback_path: &Path) -> String {
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("# ") {
            let title = rest.trim();
            if !title.is_empty() {
                return title.to_string();
            }
        }
    }

    fallback_path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("Untitled")
        .to_string()
}

pub fn extract_headings(content: &str) -> Vec<String> {
    content
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                let heading = trimmed.trim_start_matches('#').trim();
                (!heading.is_empty()).then(|| heading.to_string())
            } else {
                None
            }
        })
        .collect()
}

pub fn extract_links(content: &str) -> Vec<String> {
    let mut links = Vec::new();
    let bytes = content.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] == b'[' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b']' {
                j += 1;
            }

            if j + 1 < bytes.len() && bytes[j] == b']' && bytes[j + 1] == b'(' {
                let mut k = j + 2;
                while k < bytes.len() && bytes[k] != b')' {
                    k += 1;
                }

                if k < bytes.len() && bytes[k] == b')' {
                    let link = &content[j + 2..k];
                    if !link.trim().is_empty() {
                        links.push(link.trim().to_string());
                    }
                    i = k + 1;
                    continue;
                }
            }
        }
        i += 1;
    }

    links
}

pub fn join_lines(lines: &[String]) -> String {
    lines.join("\n")
}

pub fn split_lines(value: &str) -> Vec<String> {
    value
        .split('\n')
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{DocumentStatus, detect_status, extract_headings, extract_links, extract_title};

    #[test]
    fn extracts_title_headings_and_links() {
        let markdown =
            "# Docker\n\n## Compose\nSee [Guide](docs/guide.md) and [Site](https://htl.dev).";
        assert_eq!(extract_title(markdown, Path::new("fallback.md")), "Docker");
        assert_eq!(extract_headings(markdown), vec!["Docker", "Compose"]);
        assert_eq!(
            extract_links(markdown),
            vec!["docs/guide.md", "https://htl.dev"]
        );
    }

    #[test]
    fn falls_back_to_filename_and_detects_permission_stub() {
        let content = "Error parsing markdown: Error: You do not have the required permissions to view this content.";
        assert_eq!(extract_title("", Path::new("Tools.md")), "Tools");
        assert_eq!(detect_status(content), DocumentStatus::PermissionDenied);
    }
}

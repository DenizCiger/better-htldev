use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use crate::core::{
    DocumentRecord, DocumentStatus, SearchHit, SearchMode, SearchQuery, default_index_path,
    join_lines, split_lines,
};
use crate::fs_source::{compile_glob_filter, path_matches};

pub struct IndexStore {
    conn: Connection,
}

#[derive(Debug)]
pub struct IndexStats {
    pub indexed: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub removed: usize,
}

#[derive(Debug, Clone)]
pub struct StoredDocument {
    pub rel_path: String,
    pub display_path: String,
    pub title: String,
    pub body: String,
    pub headings: Vec<String>,
    pub links: Vec<String>,
    pub status: DocumentStatus,
    pub modified_at: Option<String>,
}

impl IndexStore {
    pub fn open(path: Option<PathBuf>) -> Result<Self> {
        let path = path.unwrap_or_else(default_index_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let conn = Connection::open(&path)
            .with_context(|| format!("failed to open SQLite database {}", path.display()))?;
        let store = Self { conn };
        store.initialize()?;
        Ok(store)
    }

    fn initialize(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS documents (
                id INTEGER PRIMARY KEY,
                rel_path TEXT NOT NULL UNIQUE,
                display_path TEXT NOT NULL,
                title TEXT NOT NULL,
                body TEXT NOT NULL,
                headings TEXT NOT NULL,
                links TEXT NOT NULL,
                status TEXT NOT NULL,
                modified_at TEXT,
                hash TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );

            CREATE VIRTUAL TABLE IF NOT EXISTS document_fts USING fts5(
                title,
                body,
                rel_path UNINDEXED
            );
            "#,
        )?;
        self.migrate_contentless_fts()?;
        Ok(())
    }

    /// If the on-disk FTS table was created with `content=''` (contentless),
    /// it cannot be DELETEd from. Drop it, recreate as a regular FTS5 table,
    /// and repopulate from the documents table so search still works.
    fn migrate_contentless_fts(&self) -> Result<()> {
        let sql: Option<String> = self
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'document_fts'",
                [],
                |row| row.get(0),
            )
            .optional()?;

        let is_contentless = sql
            .as_deref()
            .map(|s| s.contains("content=''") || s.contains("content = ''"))
            .unwrap_or(false);

        if !is_contentless {
            return Ok(());
        }

        self.conn.execute_batch(
            r#"
            DROP TABLE document_fts;
            CREATE VIRTUAL TABLE document_fts USING fts5(
                title,
                body,
                rel_path UNINDEXED
            );
            INSERT INTO document_fts(rowid, title, body, rel_path)
                SELECT id, title, body, display_path FROM documents;
            "#,
        )?;

        Ok(())
    }

    pub fn index_documents(&mut self, docs: &[DocumentRecord]) -> Result<IndexStats> {
        let existing = self.load_existing_hashes()?;
        let existing_paths: HashSet<String> = existing.keys().cloned().collect();
        let current_paths: HashSet<String> = docs.iter().map(|doc| doc.rel_path.clone()).collect();
        let removed_paths: Vec<String> =
            existing_paths.difference(&current_paths).cloned().collect();

        let tx = self.conn.transaction()?;
        let mut updated = 0usize;
        let mut unchanged = 0usize;

        for doc in docs {
            let is_changed = existing
                .get(&doc.rel_path)
                .is_none_or(|hash| hash != &doc.hash);
            if is_changed {
                upsert_document(&tx, doc)?;
                updated += 1;
            } else {
                unchanged += 1;
            }
        }

        for rel_path in &removed_paths {
            delete_document(&tx, rel_path)?;
        }

        tx.commit()?;

        Ok(IndexStats {
            indexed: docs.len(),
            updated,
            unchanged,
            removed: removed_paths.len(),
        })
    }

    fn load_existing_hashes(&self) -> Result<HashMap<String, String>> {
        let mut stmt = self.conn.prepare("SELECT rel_path, hash FROM documents")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut hashes = HashMap::new();
        for row in rows {
            let (path, hash) = row?;
            hashes.insert(path, hash);
        }
        Ok(hashes)
    }

    pub fn search(&self, query: &SearchQuery) -> Result<Vec<SearchHit>> {
        match query.mode {
            SearchMode::FullText => self.search_full_text(query),
            SearchMode::Regex => unreachable!("regex search is handled by filesystem scanning"),
        }
    }

    fn search_full_text(&self, query: &SearchQuery) -> Result<Vec<SearchHit>> {
        let matcher = compile_glob_filter(query.path_filter.as_deref())?;
        let fts_query = build_fts_query(&query.text);

        let mut stmt = self.conn.prepare(
            r#"
            SELECT
                d.id,
                d.display_path,
                d.title,
                CASE
                    WHEN length(trim(snippet(document_fts, 1, '[', ']', ' ... ', 16))) > 0
                        THEN snippet(document_fts, 1, '[', ']', ' ... ', 16)
                    ELSE substr(replace(replace(d.body, char(10), ' '), char(13), ' '), 1, 160)
                END AS snippet,
                bm25(document_fts, 8.0, 1.0) - CASE
                    WHEN d.title LIKE '%' || ?3 || '%' THEN 10.0
                    WHEN d.body  LIKE '%' || ?3 || '%' THEN 8.0
                    ELSE 0.0
                END AS rank
            FROM document_fts
            JOIN documents d ON d.id = document_fts.rowid
            WHERE document_fts MATCH ?1
              AND d.status = 'Ok'
            ORDER BY rank
            LIMIT ?2
            "#,
        )?;

        let rows = stmt.query_map(params![fts_query, (query.limit * 10) as i64, query.text.as_str()], |row| {
            Ok(SearchHit {
                doc_id: row.get(0)?,
                path: row.get(1)?,
                title: row.get(2)?,
                snippet: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                score: row.get::<_, Option<f64>>(4)?,
                line_matches: Vec::new(),
            })
        })?;

        let mut hits = Vec::new();
        for row in rows {
            let hit = row?;
            if !path_matches(matcher.as_ref(), &hit.path) {
                continue;
            }
            hits.push(hit);
            if hits.len() >= query.limit {
                break;
            }
        }

        if hits.is_empty() {
            return self.search_like_fallback(query);
        }

        Ok(hits)
    }

    fn search_like_fallback(&self, query: &SearchQuery) -> Result<Vec<SearchHit>> {
        let matcher = compile_glob_filter(query.path_filter.as_deref())?;
        let like = format!("%{}%", query.text);
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, display_path, title, body
            FROM documents
            WHERE status = 'Ok'
              AND (title LIKE ?1 OR body LIKE ?1)
            ORDER BY title
            LIMIT ?2
            "#,
        )?;

        let rows = stmt.query_map(params![like, query.limit as i64], |row| {
            let body: String = row.get(3)?;
            Ok(SearchHit {
                doc_id: row.get(0)?,
                path: row.get(1)?,
                title: row.get(2)?,
                snippet: body
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or("")
                    .to_string(),
                score: None,
                line_matches: Vec::new(),
            })
        })?;

        let mut hits = Vec::new();
        for row in rows {
            let hit = row?;
            if path_matches(matcher.as_ref(), &hit.path) {
                hits.push(hit);
            }
        }

        Ok(hits)
    }

    pub fn resolve_document(&self, target: &str) -> Result<Option<StoredDocument>> {
        if let Ok(doc_id) = target.parse::<i64>() {
            if let Some(doc) = self.find_by_id(doc_id)? {
                return Ok(Some(doc));
            }
        }

        if let Some(doc) = self.find_by_path(target)? {
            return Ok(Some(doc));
        }

        self.find_by_title(target)
    }

    fn find_by_id(&self, doc_id: i64) -> Result<Option<StoredDocument>> {
        self.conn
            .query_row(SELECT_DOCUMENT_SQL, params![doc_id], row_to_document)
            .optional()
            .map_err(Into::into)
    }

    fn find_by_path(&self, target: &str) -> Result<Option<StoredDocument>> {
        let normalized = target.replace('/', "\\");
        self.conn
            .query_row(
                "SELECT id, rel_path, display_path, title, body, headings, links, status, modified_at, hash
                 FROM documents
                 WHERE rel_path = ?1 OR display_path = ?2
                 LIMIT 1",
                params![normalized, target],
                row_to_document,
            )
            .optional()
            .map_err(Into::into)
    }

    fn find_by_title(&self, target: &str) -> Result<Option<StoredDocument>> {
        let like = format!("%{}%", target);
        self.conn
            .query_row(
                "SELECT id, rel_path, display_path, title, body, headings, links, status, modified_at, hash
                 FROM documents
                 WHERE title LIKE ?1
                 ORDER BY title
                 LIMIT 1",
                params![like],
                row_to_document,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn document_count(&self) -> Result<i64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))
            .map_err(Into::into)
    }

    pub fn get_document(&self, doc_id: i64) -> Result<Option<StoredDocument>> {
        self.find_by_id(doc_id)
    }

    pub fn list_documents(&self, limit: usize) -> Result<Vec<SearchHit>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, display_path, title, body
            FROM documents
            ORDER BY title
            LIMIT ?1
            "#,
        )?;

        let rows = stmt.query_map(params![limit as i64], |row| {
            let body: String = row.get(3)?;
            Ok(SearchHit {
                doc_id: row.get(0)?,
                path: row.get(1)?,
                title: row.get(2)?,
                snippet: body
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or("")
                    .to_string(),
                score: None,
                line_matches: Vec::new(),
            })
        })?;

        let mut hits = Vec::new();
        for row in rows {
            hits.push(row?);
        }

        Ok(hits)
    }
}

const SELECT_DOCUMENT_SQL: &str =
    "SELECT id, rel_path, display_path, title, body, headings, links, status, modified_at, hash
     FROM documents
     WHERE id = ?1
     LIMIT 1";

fn row_to_document(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredDocument> {
    let status: String = row.get(7)?;
    let _: i64 = row.get(0)?;
    Ok(StoredDocument {
        rel_path: row.get(1)?,
        display_path: row.get(2)?,
        title: row.get(3)?,
        body: row.get(4)?,
        headings: split_lines(&row.get::<_, String>(5)?),
        links: split_lines(&row.get::<_, String>(6)?),
        status: DocumentStatus::from_db(&status),
        modified_at: row.get(8)?,
    })
}

fn upsert_document(tx: &rusqlite::Transaction<'_>, doc: &DocumentRecord) -> Result<()> {
    tx.execute(
        r#"
        INSERT INTO documents (
            rel_path, display_path, title, body, headings, links, status, modified_at, hash
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
        ON CONFLICT(rel_path) DO UPDATE SET
            display_path = excluded.display_path,
            title = excluded.title,
            body = excluded.body,
            headings = excluded.headings,
            links = excluded.links,
            status = excluded.status,
            modified_at = excluded.modified_at,
            hash = excluded.hash,
            updated_at = CURRENT_TIMESTAMP
        "#,
        params![
            doc.rel_path,
            doc.display_path,
            doc.title,
            doc.body,
            join_lines(&doc.headings),
            join_lines(&doc.links),
            doc.status.as_str(),
            doc.modified_at,
            doc.hash
        ],
    )?;

    let doc_id: i64 = tx.query_row(
        "SELECT id FROM documents WHERE rel_path = ?1",
        params![doc.rel_path],
        |row| row.get(0),
    )?;
    tx.execute("DELETE FROM document_fts WHERE rowid = ?1", params![doc_id])?;
    tx.execute(
        "INSERT INTO document_fts(rowid, title, body, rel_path) VALUES (?1, ?2, ?3, ?4)",
        params![doc_id, doc.title, doc.body, doc.display_path],
    )?;
    Ok(())
}

fn delete_document(tx: &rusqlite::Transaction<'_>, rel_path: &str) -> Result<()> {
    if let Some(doc_id) = tx
        .query_row(
            "SELECT id FROM documents WHERE rel_path = ?1",
            params![rel_path],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
    {
        tx.execute("DELETE FROM document_fts WHERE rowid = ?1", params![doc_id])?;
        tx.execute("DELETE FROM documents WHERE id = ?1", params![doc_id])?;
    }
    Ok(())
}

fn build_fts_query(text: &str) -> String {
    let tokens: Vec<String> = text
        .split_whitespace()
        .map(|token| token.trim_matches(|c: char| c.is_ascii_punctuation()))
        .filter(|token| !token.is_empty())
        .map(|token| format!("{token}*"))
        .collect();

    if tokens.is_empty() {
        text.to_string()
    } else {
        tokens.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use crate::core::{SearchMode, SearchQuery};
    use crate::fs_source::FilesystemSource;

    use super::IndexStore;

    #[test]
    fn indexes_and_searches_documents() {
        let dir = tempdir().expect("temp dir");
        fs::write(
            dir.path().join("docker.md"),
            "# Docker Compose\n\nUse docker compose for local dev.\n",
        )
        .expect("write doc");
        fs::write(
            dir.path().join("quarkus.md"),
            "# Quarkus REST\n\nPanache is used here.\n",
        )
        .expect("write doc");

        let source = FilesystemSource::new(dir.path().to_path_buf());
        let docs = source.scan_documents().expect("scan docs");

        let db_path = dir.path().join("index.sqlite3");
        let mut store = IndexStore::open(Some(db_path)).expect("open index");
        let stats = store.index_documents(&docs).expect("index docs");
        assert_eq!(stats.indexed, 2);

        let hits = store
            .search(&SearchQuery {
                text: "docker".to_string(),
                mode: SearchMode::FullText,
                path_filter: None,
                limit: 10,
            })
            .expect("search");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Docker Compose");

        let resolved = store.resolve_document("docker.md").expect("resolve");
        assert!(resolved.is_some());
    }
}

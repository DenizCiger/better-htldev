use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};

use crate::core::app_data_dir;

const BASE_URL: &str = "https://htl.dev";
const RATE_LIMIT_MS: u64 = 300;

// ---------------------------------------------------------------------------
// Public event type
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ScraperEvent {
    Log(String),
    /// current / total files in the active download phase
    Progress { current: usize, total: usize },
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Default)]
struct ScraperState {
    discovered_files: HashSet<String>,
    downloaded_files: HashSet<String>,
    failed_files: HashSet<String>,
    discovered_assets: HashSet<String>,
    downloaded_assets: HashSet<String>,
    file_hashes: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Scraper
// ---------------------------------------------------------------------------

pub struct HtlScraper {
    output_dir: PathBuf,
    md_dir: PathBuf,
    assets_dir: PathBuf,
    state_file: PathBuf,
    sync_mode: bool,
    client: reqwest::blocking::Client,
    state: ScraperState,
    last_request: Instant,
    progress_tx: Option<std::sync::mpsc::Sender<ScraperEvent>>,
}

impl HtlScraper {
    pub fn new(sync_mode: bool) -> Result<Self> {
        let output_dir = app_data_dir();
        let jar = std::sync::Arc::new(reqwest::cookie::Jar::default());
        let client = reqwest::blocking::Client::builder()
            .cookie_provider(jar)
            .timeout(Duration::from_secs(30))
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .context("build HTTP client")?;

        Ok(Self {
            md_dir: output_dir.join("md"),
            assets_dir: output_dir.join("assets"),
            state_file: output_dir.join("scraper_state.json"),
            output_dir,
            sync_mode,
            client,
            state: ScraperState::default(),
            last_request: Instant::now() - Duration::from_millis(RATE_LIMIT_MS + 1),
            progress_tx: None,
        })
    }

    pub fn set_progress_tx(&mut self, tx: std::sync::mpsc::Sender<ScraperEvent>) {
        self.progress_tx = Some(tx);
    }

    fn log(&self, msg: impl Into<String>) {
        let s = msg.into();
        if let Some(tx) = &self.progress_tx {
            let _ = tx.send(ScraperEvent::Log(s));
        } else {
            println!("{s}");
        }
    }

    fn send_progress(&self, current: usize, total: usize) {
        if let Some(tx) = &self.progress_tx {
            let _ = tx.send(ScraperEvent::Progress { current, total });
        }
    }

    fn rate_limit(&mut self) {
        let elapsed = self.last_request.elapsed();
        let limit = Duration::from_millis(RATE_LIMIT_MS);
        if elapsed < limit {
            std::thread::sleep(limit - elapsed);
        }
        self.last_request = Instant::now();
    }

    fn get(&mut self, url: &str) -> Result<reqwest::blocking::Response> {
        self.rate_limit();
        Ok(self.client.get(url).send()?)
    }

    // -----------------------------------------------------------------------
    // Auth
    // -----------------------------------------------------------------------

    pub fn authenticate(&mut self, username: &str, password: &str) -> Result<bool> {
        self.log(format!("Authenticating as: {username}"));

        self.client.get(BASE_URL).send()?; // seed cookies
        self.rate_limit();

        let login_page = self.client.get(BASE_URL).send()?;
        let body = login_page.text()?;

        if !body.contains("Sign in to your account") {
            self.log("[OK] Already authenticated");
            return Ok(true);
        }

        let doc = Html::parse_document(&body);
        let form_sel = Selector::parse("form#kc-form-login").unwrap();
        let form = doc.select(&form_sel).next().context("login form not found")?;

        let action = form
            .value()
            .attr("action")
            .context("form has no action")?
            .replace("&amp;", "&");

        // Collect all form fields
        let input_sel = Selector::parse("input").unwrap();
        let mut form_data: Vec<(String, String)> = Vec::new();
        for input in form.select(&input_sel) {
            let name = match input.value().attr("name") {
                Some(n) => n.to_string(),
                None => continue,
            };
            let input_type = input.value().attr("type").unwrap_or("text");
            let value = input.value().attr("value").unwrap_or("").to_string();

            let filled = match (input_type, name.as_str()) {
                ("text", "username") => username.to_string(),
                ("password", "password") => password.to_string(),
                _ => value,
            };
            form_data.push((name, filled));
        }

        self.rate_limit();
        self.client
            .post(&action)
            .form(&form_data)
            .header("Origin", "https://auth.htl-leonding.ac.at")
            .send()?;

        self.rate_limit();
        let check = self.client.get(BASE_URL).send()?.text()?;
        if check.contains("Sign in to your account") {
            self.log("[ERROR] Authentication failed — check credentials");
            Ok(false)
        } else {
            self.log("[OK] Authentication successful");
            Ok(true)
        }
    }

    // -----------------------------------------------------------------------
    // State persistence
    // -----------------------------------------------------------------------

    fn save_state(&self) -> Result<()> {
        std::fs::create_dir_all(&self.output_dir)?;
        let json = serde_json::to_string_pretty(&self.state)?;
        std::fs::write(&self.state_file, json)?;
        Ok(())
    }

    fn load_state(&mut self) -> bool {
        let Ok(data) = std::fs::read_to_string(&self.state_file) else {
            return false;
        };
        match serde_json::from_str(&data) {
            Ok(state) => {
                self.state = state;
                self.log(format!(
                    "[OK] Loaded previous state ({} files downloaded)",
                    self.state.downloaded_files.len()
                ));
                true
            }
            Err(e) => {
                self.log(format!("[WARN] Could not load state: {e}"));
                false
            }
        }
    }

    // -----------------------------------------------------------------------
    // Discovery
    // -----------------------------------------------------------------------

    pub fn discover_all_files(&mut self) -> Result<()> {
        self.log("[1/4] Discovering markdown files...");

        self.discover_from_html_page("/")?;

        let known = vec![
            "/md/index.md",
            "/md/contacts/",
            "/md/exercises/",
            "/md/presentations/",
            "/md/technologies/",
            "/md/diploma-examination/",
            "/md/assets/",
        ];
        for path in known {
            if path.ends_with('/') {
                let _ = self.discover_from_html_page(path);
            } else {
                self.state.discovered_files.insert(path.to_string());
            }
        }

        // Follow links iteratively
        for iteration in 0..10 {
            let initial = self.state.discovered_files.len();
            let to_check: Vec<String> = self
                .state
                .discovered_files
                .difference(&self.state.downloaded_files)
                .take(50)
                .cloned()
                .collect();

            for md_path in to_check {
                let _ = self.discover_from_markdown(&md_path);
            }

            let new_count = self.state.discovered_files.len();
            if new_count == initial {
                break;
            }
            self.log(format!(
                "  Iteration {}: +{} files (total: {})",
                iteration + 1,
                new_count - initial,
                new_count
            ));
        }

        self.log(format!("[OK] Discovered {} files", self.state.discovered_files.len()));
        self.save_state()?;
        Ok(())
    }

    fn discover_from_html_page(&mut self, path: &str) -> Result<()> {
        let url = format!("{BASE_URL}{path}");
        let resp = self.get(&url)?;
        if !resp.status().is_success() {
            return Ok(());
        }
        let body = resp.text()?;
        let doc = Html::parse_document(&body);
        let a_sel = Selector::parse("a[href]").unwrap();
        for a in doc.select(&a_sel) {
            if let Some(href) = a.value().attr("href") {
                self.collect_md_href(href);
            }
        }
        Ok(())
    }

    fn discover_from_markdown(&mut self, md_path: &str) -> Result<()> {
        let url = format!("{BASE_URL}{md_path}?document=true");
        let resp = self.get(&url)?;
        if !resp.status().is_success() {
            return Ok(());
        }
        let body = resp.text()?;
        let doc = Html::parse_document(&body);
        let div_sel = Selector::parse("div#markdown-content").unwrap();
        let Some(content_div) = doc.select(&div_sel).next() else {
            return Ok(());
        };

        let a_sel = Selector::parse("a[href]").unwrap();
        for a in content_div.select(&a_sel) {
            if let Some(href) = a.value().attr("href") {
                self.collect_md_href(href);
            }
        }

        let img_sel = Selector::parse("img[src]").unwrap();
        for img in content_div.select(&img_sel) {
            if let Some(src) = img.value().attr("src") {
                let full = if src.starts_with("https://htl.dev") {
                    src.to_string()
                } else if src.starts_with("/md/assets/") {
                    format!("{BASE_URL}{src}")
                } else if src.contains("assets/") {
                    src.to_string()
                } else {
                    continue;
                };
                self.state.discovered_assets.insert(full);
            }
        }
        Ok(())
    }

    fn collect_md_href(&mut self, href: &str) {
        let path = if href.starts_with("https://htl.dev/md/") && href.ends_with(".md") {
            href.trim_start_matches("https://htl.dev").to_string()
        } else if href.starts_with("/md/") && href.ends_with(".md") {
            href.to_string()
        } else {
            return;
        };
        self.state.discovered_files.insert(path);
    }

    // -----------------------------------------------------------------------
    // Download markdown
    // -----------------------------------------------------------------------

    pub fn download_all_files(&mut self) -> Result<()> {
        self.log("[2/4] Downloading markdown files...");
        std::fs::create_dir_all(&self.md_dir)?;

        let to_process: Vec<String> = if self.sync_mode {
            let all: Vec<_> = self.state.discovered_files.iter().cloned().collect();
            self.log(format!("[SYNC] Checking {} files for updates...", all.len()));
            all
        } else {
            let new: Vec<_> = self
                .state
                .discovered_files
                .difference(&self.state.downloaded_files)
                .cloned()
                .collect();
            if !self.state.downloaded_files.is_empty() {
                self.log(format!(
                    "  {} already downloaded, {} new",
                    self.state.downloaded_files.len(),
                    new.len()
                ));
            }
            new
        };

        let total = to_process.len();
        if total == 0 {
            self.log("[OK] All files already downloaded");
            return Ok(());
        }

        let mut new_count = 0usize;
        let mut updated_count = 0usize;
        let mut unchanged_count = 0usize;
        let mut failed_count = 0usize;

        let mut sorted = to_process;
        sorted.sort();

        for (i, md_path) in sorted.iter().enumerate() {
            let old_hash = self.state.file_hashes.get(md_path).cloned();
            let is_new = !self.state.downloaded_files.contains(md_path);

            match self.download_markdown(md_path) {
                Ok(new_hash) => {
                    self.state.downloaded_files.insert(md_path.clone());
                    self.state.file_hashes.insert(md_path.clone(), new_hash.clone());
                    if is_new {
                        new_count += 1;
                    } else if old_hash.as_deref() != Some(&new_hash) {
                        updated_count += 1;
                        self.log(format!("  [UPDATED] {}", url_decode(md_path)));
                    } else {
                        unchanged_count += 1;
                    }
                }
                Err(_) => {
                    self.state.failed_files.insert(md_path.clone());
                    failed_count += 1;
                }
            }

            self.send_progress(i + 1, total);
            if (i + 1) % 10 == 0 || i + 1 == total {
                self.log(format!("  Progress: {}/{} ({}%)", i + 1, total, (i + 1) * 100 / total));
                let _ = self.save_state();
            }
        }

        self.log(format!("[OK] Processed {total} files:"));
        if new_count > 0 { self.log(format!("  - {new_count} new")); }
        if updated_count > 0 { self.log(format!("  - {updated_count} updated")); }
        if unchanged_count > 0 { self.log(format!("  - {unchanged_count} unchanged")); }
        if failed_count > 0 { self.log(format!("  - {failed_count} failed")); }

        Ok(())
    }

    fn download_markdown(&mut self, md_path: &str) -> Result<String> {
        let url = format!("{BASE_URL}{md_path}?document=true");
        let resp = self.get(&url)?;
        if !resp.status().is_success() {
            bail!("HTTP {}", resp.status());
        }

        let body = resp.text()?;
        let doc = Html::parse_document(&body);
        let div_sel = Selector::parse("div#markdown-content").unwrap();
        let content_div = doc.select(&div_sel).next().context("no #markdown-content")?;

        let preprocessed = preprocess_html(content_div.html());
        let current_dir = md_path.rsplitn(2, '/').last().unwrap_or("");
        let markdown = html_to_markdown(&preprocessed, md_path, current_dir);
        let markdown = postprocess_markdown(&markdown);

        let hash = format!("{:x}", md5_digest(markdown.as_bytes()));

        // Write to disk
        let rel = url_decode(md_path.trim_start_matches('/'));
        let out_path = self.output_dir.join(&rel);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&out_path, &markdown)?;

        Ok(hash)
    }

    // -----------------------------------------------------------------------
    // Download assets
    // -----------------------------------------------------------------------

    pub fn download_all_assets(&mut self) -> Result<()> {
        let to_download: Vec<String> = self
            .state
            .discovered_assets
            .difference(&self.state.downloaded_assets)
            .cloned()
            .collect();

        self.log(format!("[3/4] Downloading {} assets...", to_download.len()));
        if to_download.is_empty() {
            self.log("[OK] All assets already downloaded");
            return Ok(());
        }

        std::fs::create_dir_all(&self.assets_dir)?;
        let total = to_download.len();

        for (i, asset_url) in to_download.iter().enumerate() {
            if self.download_asset(asset_url).is_ok() {
                self.state.downloaded_assets.insert(asset_url.clone());
            }
            if (i + 1) % 20 == 0 || i + 1 == total {
                self.log(format!("  Assets: {}/{} ({}%)", i + 1, total, (i + 1) * 100 / total));
                let _ = self.save_state();
            }
        }

        self.log(format!("[OK] Downloaded {} assets", self.state.downloaded_assets.len()));
        Ok(())
    }

    fn download_asset(&mut self, url: &str) -> Result<()> {
        let resp = self.get(url)?;
        if !resp.status().is_success() {
            bail!("HTTP {}", resp.status());
        }
        let filename = url.rsplit('/').next().map(url_decode).unwrap_or_else(|| "asset".into());
        let out = self.assets_dir.join(filename);
        std::fs::write(out, resp.bytes()?)?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Run
    // -----------------------------------------------------------------------

    pub fn run(mut self, username: &str, password: &str, resume: bool) -> Result<()> {
        self.log("HTL.dev Scraper");
        self.log(format!("Output: {}", self.output_dir.display()));

        if resume {
            self.load_state();
        }

        if !self.authenticate(username, password)? {
            bail!("Authentication failed");
        }

        self.discover_all_files()?;
        self.download_all_files()?;
        self.download_all_assets()?;
        self.save_state()?;

        self.log("DONE");
        self.log(format!(
            "Total: {} markdown files, {} assets",
            self.state.downloaded_files.len(),
            self.state.downloaded_assets.len()
        ));
        if !self.state.failed_files.is_empty() {
            self.log(format!("Failed: {} (run again to retry)", self.state.failed_files.len()));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// HTML preprocessing — port of Python _preprocess_html
// ---------------------------------------------------------------------------

fn preprocess_html(html: String) -> String {
    // We work at the string level for simplicity, then re-parse for htmd.
    // Real DOM manipulation would require a mutable scraper tree which is complex;
    // instead we do targeted regex-style replacements on the raw HTML string.

    // Rebuild HTML with transformations applied
    // Strategy: walk the original HTML and apply known transforms via string ops.
    // This mirrors the Python approach closely enough for the htl.dev content.

    let mut result = html;

    // 1. Remove <span class="docanchor">...</span>
    result = remove_tags_with_class(&result, "span", "docanchor");

    // 2. Transform Shiki code blocks: <pre class="shiki ..."> → plain <pre><code>text</code></pre>
    result = transform_shiki_blocks(&result);

    // 3. Remove fragment-related attributes (cosmetic, safe to skip for markdown)
    result = result.replace(" data-fragment-index=\"", " data-rm=\"");

    result
}

fn remove_tags_with_class(html: &str, _tag: &str, class: &str) -> String {
    // Simple approach: parse and rebuild without matching elements
    let doc = Html::parse_fragment(html);
    let sel = Selector::parse(&format!(".{class}")).unwrap();
    let mut out = html.to_string();
    for el in doc.select(&sel) {
        let outer = el.html();
        out = out.replace(&outer, "");
    }
    out
}

fn transform_shiki_blocks(html: &str) -> String {
    let doc = Html::parse_fragment(html);
    let pre_sel = Selector::parse("pre.shiki").unwrap();
    let line_sel = Selector::parse("span.line").unwrap();

    let mut result = html.to_string();

    for pre in doc.select(&pre_sel) {
        // Determine language from class list
        let classes = pre.value().attr("class").unwrap_or("");
        let lang = classes
            .split_whitespace()
            .find(|c| !matches!(*c, "shiki" | "one-dark-pro" | "fragment"))
            .unwrap_or("")
            .trim_start_matches("language-")
            .to_string();

        // Extract text line by line
        let mut lines: Vec<String> = Vec::new();
        for line_span in pre.select(&line_sel) {
            lines.push(line_span.text().collect::<String>());
        }
        let code_text = lines.join("\n");

        // Escape HTML entities for plain text inside <code>
        let escaped = code_text
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");

        let replacement = if lang.is_empty() {
            format!("<pre><code>{escaped}</code></pre>")
        } else {
            format!("<pre><code class=\"language-{lang}\">{escaped}</code></pre>")
        };

        let original = pre.html();
        result = result.replacen(&original, &replacement, 1);
    }

    result
}

// ---------------------------------------------------------------------------
// HTML → Markdown
// ---------------------------------------------------------------------------

fn html_to_markdown(html: &str, md_path: &str, current_dir: &str) -> String {
    // Normalize links before converting
    let html = normalize_links_in_html(html, md_path, current_dir);
    htmd::convert(&html).unwrap_or_default()
}

fn normalize_links_in_html(html: &str, _md_path: &str, current_dir: &str) -> String {
    let doc = Html::parse_fragment(html);
    let mut result = html.to_string();

    let a_sel = Selector::parse("a[href]").unwrap();
    for a in doc.select(&a_sel) {
        if let Some(href) = a.value().attr("href") {
            let normalized = normalize_url(href, current_dir, false);
            if normalized != href {
                // Replace just the href attribute value
                let original = format!("href=\"{href}\"");
                let replacement = format!("href=\"{normalized}\"");
                result = result.replacen(&original, &replacement, 1);
            }
        }
    }

    let img_sel = Selector::parse("img[src]").unwrap();
    for img in doc.select(&img_sel) {
        if let Some(src) = img.value().attr("src") {
            let normalized = normalize_url(src, current_dir, true);
            if normalized != src {
                let original = format!("src=\"{src}\"");
                let replacement = format!("src=\"{normalized}\"");
                result = result.replacen(&original, &replacement, 1);
            }
        }
    }

    result
}

fn normalize_url(url: &str, current_dir: &str, is_asset: bool) -> String {
    if url.is_empty() {
        return url.to_string();
    }

    let is_htl = url.starts_with("https://htl.dev") || url.starts_with("https://www.htl.dev");
    let is_root_relative = url.starts_with('/');

    // External non-htl link
    if url.starts_with("http") && !is_htl {
        return url.to_string();
    }

    let path = if is_htl {
        url.trim_start_matches("https://www.htl.dev")
            .trim_start_matches("https://htl.dev")
            .split('?')
            .next()
            .unwrap_or("")
            .to_string()
    } else if is_root_relative {
        url.split('?').next().unwrap_or("").to_string()
    } else {
        url.to_string()
    };

    let path = url_decode(&path);

    // Asset normalization
    if is_asset && path.contains("/md/assets/") {
        let filename = path.rsplit('/').next().unwrap_or("");
        let depth = current_dir
            .split('/')
            .filter(|p| !p.is_empty() && *p != "md")
            .count();
        let prefix = "../".repeat(depth);
        return format!("{prefix}assets/{filename}");
    }

    // Markdown link normalization
    if path.ends_with(".md") && path.starts_with("/md/") {
        let target_parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        let current_parts: Vec<&str> = current_dir.split('/').filter(|p| !p.is_empty()).collect();

        let mut common = 0;
        for i in 0..target_parts.len().min(current_parts.len()) {
            if target_parts[i] == current_parts[i] {
                common = i + 1;
            } else {
                break;
            }
        }

        let ups = current_parts.len() - common;
        let downs = &target_parts[common..];

        return if ups == 0 {
            downs.join("/")
        } else {
            format!("{}{}", "../".repeat(ups), downs.join("/"))
        };
    }

    url.to_string()
}

// ---------------------------------------------------------------------------
// Markdown post-processing
// ---------------------------------------------------------------------------

fn postprocess_markdown(md: &str) -> String {
    // Collapse 3+ blank lines to 2
    let mut result = String::with_capacity(md.len());
    let mut blank_count = 0usize;
    for line in md.lines() {
        if line.trim().is_empty() {
            blank_count += 1;
            if blank_count <= 2 {
                result.push('\n');
            }
        } else {
            blank_count = 0;
            result.push_str(line);
            result.push('\n');
        }
    }
    result.trim().to_string()
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

fn url_decode(s: &str) -> String {
    // Simple percent-decoding for common cases
    let mut result = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    result.push(byte as char);
                    i += 3;
                    continue;
                }
            }
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

fn md5_digest(data: &[u8]) -> u128 {
    // Simple FNV-like hash — good enough for change detection (not security)
    // Using blake3 from our existing dep is cleaner:
    let hash = blake3::hash(data);
    let bytes = hash.as_bytes();
    u128::from_le_bytes(bytes[..16].try_into().unwrap())
}

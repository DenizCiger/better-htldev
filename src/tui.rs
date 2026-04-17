use std::io;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, List, ListItem, Paragraph, Wrap},
};

use crate::core::SearchHit;
use crate::scraper::{HtlScraper, ScraperEvent};
use crate::service::{PreviewDocument, SearchService};

const SEARCH_LIMIT: usize = 50;
const DEBOUNCE_MS: u64 = 150;

// ---------------------------------------------------------------------------
// Screen state
// ---------------------------------------------------------------------------

enum Screen {
    Search,
    Setup(SetupState),
    Scraping(ScrapingState),
}

struct SetupState {
    username: String,
    password: String,
    active_field: SetupField,
    sync_mode: bool,
    error: Option<String>,
}

#[derive(PartialEq)]
enum SetupField {
    Username,
    Password,
}

struct ScrapingState {
    lines: Vec<String>,
    rx: mpsc::Receiver<ScraperEvent>,
    done_rx: mpsc::Receiver<Result<(), String>>,
    done: bool,
    result: Option<Result<(), String>>,
    progress_current: usize,
    progress_total: usize,
}

impl SetupState {
    fn new() -> Self {
        Self {
            username: std::env::var("HTL_USERNAME").unwrap_or_default(),
            password: std::env::var("HTL_PASSWORD").unwrap_or_default(),
            active_field: SetupField::Username,
            sync_mode: false,
            error: None,
        }
    }

    fn active_field_mut(&mut self) -> &mut String {
        match self.active_field {
            SetupField::Username => &mut self.username,
            SetupField::Password => &mut self.password,
        }
    }
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

struct App {
    query: String,
    results: Vec<SearchHit>,
    selected: usize,
    preview: Option<PreviewDocument>,
    show_preview: bool,
    preview_scroll: u16,
    status: String,
    dirty: bool,
    last_edit: Instant,
    screen: Screen,
}

impl App {
    fn new(has_source: bool) -> Self {
        let screen = if has_source {
            Screen::Search
        } else {
            Screen::Setup(SetupState::new())
        };
        Self {
            query: String::new(),
            results: Vec::new(),
            selected: 0,
            preview: None,
            show_preview: true,
            preview_scroll: 0,
            status: if has_source {
                String::new()
            } else {
                "No data found. Set up credentials to scrape htl.dev.".to_string()
            },
            dirty: true,
            last_edit: Instant::now(),
            screen,
        }
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
        self.last_edit = Instant::now();
    }

    fn should_search(&self) -> bool {
        matches!(self.screen, Screen::Search)
            && self.dirty
            && self.last_edit.elapsed() >= Duration::from_millis(DEBOUNCE_MS)
    }

    fn set_results(&mut self, results: Vec<SearchHit>) {
        self.results = results;
        self.selected = 0;
    }

    fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn move_down(&mut self) {
        if !self.results.is_empty() {
            self.selected = (self.selected + 1).min(self.results.len() - 1);
        }
    }

    fn selected_hit(&self) -> Option<&SearchHit> {
        self.results.get(self.selected)
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run(service: SearchService) -> Result<()> {
    let has_source = service.source().exists();

    let mut terminal = setup_terminal()?;
    let _guard = TerminalGuard;
    let mut app = App::new(has_source);

    if has_source {
        do_search(&service, &mut app)?;
    }

    loop {
        terminal.draw(|frame| render(frame, &app))?;

        // Poll scraping channel
        if let Screen::Scraping(ref mut state) = app.screen {
            while let Ok(event) = state.rx.try_recv() {
                match event {
                    ScraperEvent::Log(line) => state.lines.push(line),
                    ScraperEvent::Progress { current, total } => {
                        state.progress_current = current;
                        state.progress_total = total;
                    }
                }
            }
            if !state.done {
                if let Ok(result) = state.done_rx.try_recv() {
                    state.done = true;
                    state.result = Some(result);
                }
            }
        }

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if handle_key(key, &service, &mut app)? {
                    break;
                }
            }
        }

        if app.should_search() {
            do_search(&service, &mut app)?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Key handling
// ---------------------------------------------------------------------------

fn handle_key(
    key: crossterm::event::KeyEvent,
    service: &SearchService,
    app: &mut App,
) -> Result<bool> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Ok(true);
    }

    match &app.screen {
        Screen::Search => handle_search_key(key, service, app),
        Screen::Setup(_) => handle_setup_key(key, app),
        Screen::Scraping(_) => handle_scraping_key(key, service, app),
    }
}

fn handle_search_key(
    key: crossterm::event::KeyEvent,
    service: &SearchService,
    app: &mut App,
) -> Result<bool> {
    match key.code {
        KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::ALT) => return Ok(true),
        KeyCode::Tab => app.show_preview = !app.show_preview,
        KeyCode::Up => {
            app.move_up();
            refresh_preview(service, app)?;
        }
        KeyCode::Down => {
            app.move_down();
            refresh_preview(service, app)?;
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => open_in_browser(app)?,
        KeyCode::Enter => open_selected(service, app)?,
        KeyCode::PageDown => app.preview_scroll = app.preview_scroll.saturating_add(10),
        KeyCode::PageUp => app.preview_scroll = app.preview_scroll.saturating_sub(10),
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::ALT) => {
            rebuild_index(service, app)?
        }
        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::ALT) => {
            app.screen = Screen::Setup(SetupState::new());
        }
        KeyCode::Esc => {
            app.query.clear();
            app.mark_dirty();
        }
        KeyCode::Backspace => {
            app.query.pop();
            app.mark_dirty();
        }
        KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.query.push(ch);
            app.mark_dirty();
        }
        _ => {}
    }
    Ok(false)
}

fn handle_setup_key(key: crossterm::event::KeyEvent, app: &mut App) -> Result<bool> {
    let Screen::Setup(ref mut state) = app.screen else {
        return Ok(false);
    };

    match key.code {
        KeyCode::Esc => {
            app.screen = Screen::Search;
        }
        KeyCode::Tab | KeyCode::Down => {
            state.active_field = match state.active_field {
                SetupField::Username => SetupField::Password,
                SetupField::Password => SetupField::Username,
            };
        }
        KeyCode::Up => {
            state.active_field = match state.active_field {
                SetupField::Username => SetupField::Password,
                SetupField::Password => SetupField::Username,
            };
        }
        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::ALT) => {
            if let Screen::Setup(ref mut s) = app.screen {
                s.sync_mode = !s.sync_mode;
            }
        }
        KeyCode::Enter => {
            let Screen::Setup(ref s) = app.screen else {
                return Ok(false);
            };
            if s.username.is_empty() || s.password.is_empty() {
                if let Screen::Setup(ref mut s) = app.screen {
                    s.error = Some("Username and password required.".to_string());
                }
                return Ok(false);
            }
            start_scraping(app);
        }
        KeyCode::Backspace => {
            state.active_field_mut().pop();
        }
        KeyCode::Char(ch) => {
            state.active_field_mut().push(ch);
        }
        _ => {}
    }
    Ok(false)
}

fn handle_scraping_key(
    key: crossterm::event::KeyEvent,
    service: &SearchService,
    app: &mut App,
) -> Result<bool> {
    let Screen::Scraping(ref state) = app.screen else {
        return Ok(false);
    };

    if !state.done {
        return Ok(false); // ignore keys while running
    }

    // Scraping finished — any key reindexes and returns to search
    match key.code {
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char(_) => {
            let success = state.result.as_ref().map_or(false, |r| r.is_ok());
            app.screen = Screen::Search;
            if success {
                rebuild_index(service, app)?;
                app.status = "Scrape complete. Index rebuilt.".to_string();
            } else {
                app.status = "Scrape failed. Check credentials.".to_string();
            }
            app.mark_dirty();
        }
        _ => {}
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Scraper thread launch
// ---------------------------------------------------------------------------

fn start_scraping(app: &mut App) {
    let Screen::Setup(ref state) = app.screen else {
        return;
    };

    let username = state.username.clone();
    let password = state.password.clone();
    let sync_mode = state.sync_mode;

    let (progress_tx, progress_rx) = mpsc::channel::<ScraperEvent>();
    let (done_tx, done_rx) = mpsc::channel::<Result<(), String>>();

    std::thread::spawn(move || {
        let result = (|| -> Result<(), String> {
            let mut scraper = HtlScraper::new(sync_mode).map_err(|e| e.to_string())?;
            scraper.set_progress_tx(progress_tx);
            scraper
                .run(&username, &password, true)
                .map_err(|e| e.to_string())
        })();
        let _ = done_tx.send(result);
    });

    app.screen = Screen::Scraping(ScrapingState {
        lines: vec!["Starting scraper...".to_string()],
        rx: progress_rx,
        done_rx,
        done: false,
        result: None,
        progress_current: 0,
        progress_total: 0,
    });
}

// ---------------------------------------------------------------------------
// Search helpers
// ---------------------------------------------------------------------------

fn do_search(service: &SearchService, app: &mut App) -> Result<()> {
    let results = match service.search(&app.query, SEARCH_LIMIT) {
        Ok(r) => r,
        Err(e) => {
            app.status = format!("Index error: {}. Press Alt+R to build.", e);
            app.dirty = false;
            return Ok(());
        }
    };

    app.set_results(results);
    app.dirty = false;

    if app.results.is_empty() && !app.query.trim().is_empty() {
        app.status = format!("No results for '{}'.", app.query);
    } else {
        app.status.clear();
    }

    refresh_preview(service, app)
}

fn refresh_preview(service: &SearchService, app: &mut App) -> Result<()> {
    app.preview = match app.selected_hit() {
        Some(hit) => service.preview_for_hit(hit)?,
        None => None,
    };
    app.preview_scroll = first_match_line(&app.preview, &app.query);
    Ok(())
}

fn first_match_line(preview: &Option<PreviewDocument>, query: &str) -> u16 {
    let Some(preview) = preview else { return 0 };
    let tokens: Vec<String> = query
        .split_whitespace()
        .map(|t| t.to_lowercase())
        .collect();
    if tokens.is_empty() {
        return 0;
    }
    for (i, line) in preview.body_lines.iter().enumerate() {
        let lower = line.to_lowercase();
        if tokens.iter().any(|t| lower.contains(t.as_str())) {
            return (i as u16).saturating_sub(2);
        }
    }
    0
}

fn rebuild_index(service: &SearchService, app: &mut App) -> Result<()> {
    let stats = service.index_documents()?;
    app.status = format!(
        "Indexed {} docs, updated {}, unchanged {}, removed {}.",
        stats.indexed, stats.updated, stats.unchanged, stats.removed
    );
    app.mark_dirty();
    do_search(service, app)
}

fn open_selected(service: &SearchService, app: &mut App) -> Result<()> {
    if let Some(hit) = app.selected_hit() {
        service.open_hit(hit)?;
        app.status = format!("Opened {}", hit.path);
    } else {
        app.status = "No document selected.".to_string();
    }
    Ok(())
}

fn open_in_browser(app: &mut App) -> Result<()> {
    if let Some(hit) = app.selected_hit() {
        crate::service::open_in_browser(&hit.path)?;
        app.status = format!("Opened in browser: htl.dev/md/{}", hit.path);
    } else {
        app.status = "No document selected.".to_string();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render(frame: &mut ratatui::Frame<'_>, app: &App) {
    match &app.screen {
        Screen::Search => render_search(frame, app),
        Screen::Setup(state) => render_setup(frame, state),
        Screen::Scraping(state) => render_scraping(frame, state),
    }
}

fn render_search(frame: &mut ratatui::Frame<'_>, app: &App) {
    let layout = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(5),
        Constraint::Length(1),
    ])
    .split(frame.area());

    render_query(frame, layout[0], app);

    if app.show_preview {
        let cols = Layout::horizontal([
            Constraint::Percentage(40),
            Constraint::Percentage(60),
        ])
        .split(layout[1]);
        render_results(frame, cols[0], app);
        render_preview(frame, cols[1], app);
    } else {
        render_results(frame, layout[1], app);
    }

    render_status(frame, layout[2], app);
}

fn render_query(frame: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, app: &App) {
    let paragraph = Paragraph::new(app.query.as_str()).block(
        Block::default()
            .title(" Search ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(paragraph, area);

    let cursor_x = area.x + 1 + app.query.len() as u16;
    let cursor_y = area.y + 1;
    if cursor_x < area.x + area.width - 1 {
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

fn render_results(frame: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, app: &App) {
    let items: Vec<ListItem> = if app.results.is_empty() {
        vec![ListItem::new(Line::raw("No results"))]
    } else {
        app.results
            .iter()
            .map(|hit| ListItem::new(Line::from(path_spans(&hit.path))))
            .collect()
    };

    let results_title = if app.results.is_empty() {
        " Results ".to_string()
    } else {
        format!(" Results ({}) ", app.results.len())
    };
    let list = List::new(items)
        .block(Block::default().title(results_title).borders(Borders::ALL))
        .highlight_style(Style::default().bg(Color::Rgb(30, 30, 60)))
        .highlight_symbol(">> ");

    let mut list_state = ratatui::widgets::ListState::default();
    if !app.results.is_empty() {
        list_state.select(Some(app.selected));
    }
    frame.render_stateful_widget(list, area, &mut list_state);
}

fn render_preview(frame: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, app: &App) {
    let lines: Vec<Line> = match &app.preview {
        None => vec![Line::raw("No document selected.")],
        Some(preview) => {
            let mut v = vec![
                Line::from(Span::styled(
                    preview.path.as_str(),
                    Style::default().fg(Color::DarkGray),
                )),
                Line::raw(""),
            ];
            for line in &preview.body_lines {
                v.push(highlight_line(line, &app.query));
            }
            v
        }
    };

    let widget = Paragraph::new(lines)
        .block(Block::default().title(" Preview ").borders(Borders::ALL))
        .scroll((app.preview_scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
}

fn render_status(frame: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, app: &App) {
    let line = if app.status.is_empty() {
        let help = "[↑↓] select  [Enter] open  [Shift+Enter] browser  [Tab] preview  [PgUp/Dn] scroll  [Alt+R] reindex  [Alt+S] scrape  [Esc] clear  [Ctrl+C] quit";
        Line::from(Span::styled(help, Style::default().fg(Color::DarkGray)))
    } else {
        Line::from(Span::styled(app.status.as_str(), Style::default().fg(Color::Yellow)))
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn render_setup(frame: &mut ratatui::Frame<'_>, state: &SetupState) {
    let area = frame.area();

    // Center dialog box (60 wide, 12 tall)
    let dialog_w = 62u16.min(area.width);
    let dialog_h = 12u16.min(area.height);
    let dialog = ratatui::layout::Rect {
        x: area.x + (area.width.saturating_sub(dialog_w)) / 2,
        y: area.y + (area.height.saturating_sub(dialog_h)) / 2,
        width: dialog_w,
        height: dialog_h,
    };

    // Outer block — render first, then use its inner area for content
    let block = Block::default()
        .title(" HTL.dev Setup ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner_area = block.inner(dialog);
    frame.render_widget(block, dialog);

    // Split the *inner* area (no border overlap)
    let rows = Layout::vertical([
        Constraint::Length(3), // username field
        Constraint::Length(3), // password field
        Constraint::Length(1), // sync toggle
        Constraint::Length(1), // error
        Constraint::Length(1), // help
    ])
    .split(inner_area);

    // Username field
    let user_style = if state.active_field == SetupField::Username {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::Gray)
    };
    frame.render_widget(
        Paragraph::new(state.username.as_str()).block(
            Block::default()
                .title(" Username ")
                .borders(Borders::ALL)
                .border_style(user_style),
        ),
        rows[0],
    );

    // Password field (masked)
    let pass_style = if state.active_field == SetupField::Password {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::Gray)
    };
    let masked: String = "*".repeat(state.password.len());
    frame.render_widget(
        Paragraph::new(masked.as_str()).block(
            Block::default()
                .title(" Password ")
                .borders(Borders::ALL)
                .border_style(pass_style),
        ),
        rows[1],
    );

    // Sync toggle
    let sync_label = if state.sync_mode {
        Span::styled("[x] Sync mode", Style::default().fg(Color::Green))
    } else {
        Span::styled("[ ] Sync mode", Style::default().fg(Color::DarkGray))
    };
    frame.render_widget(Paragraph::new(Line::from(sync_label)), rows[2]);

    // Error
    if let Some(ref err) = state.error {
        frame.render_widget(
            Paragraph::new(Span::styled(err.as_str(), Style::default().fg(Color::Red))),
            rows[3],
        );
    }

    // Help line
    frame.render_widget(
        Paragraph::new(Span::styled(
            "[Tab] switch  [Alt+S] sync  [Enter] start  [Esc] cancel",
            Style::default().fg(Color::DarkGray),
        )),
        rows[4],
    );

    // Cursor inside active field (field content starts at x+1, y+1 due to border)
    let (cx, cy) = match state.active_field {
        SetupField::Username => (rows[0].x + 1 + state.username.len() as u16, rows[0].y + 1),
        SetupField::Password => (rows[1].x + 1 + state.password.len() as u16, rows[1].y + 1),
    };
    if cx < inner_area.x + inner_area.width {
        frame.set_cursor_position((cx, cy));
    }
}

fn render_scraping(frame: &mut ratatui::Frame<'_>, state: &ScrapingState) {
    let layout = Layout::vertical([
        Constraint::Min(5),   // log area
        Constraint::Length(3), // progress bar
        Constraint::Length(1), // status line
    ])
    .split(frame.area());

    let border_color = if state.done { Color::Green } else { Color::Yellow };
    let title = if state.done {
        " Scraping — Done! Press any key to continue "
    } else {
        " Scraping in progress… "
    };

    // Log lines — show the most recent lines that fit
    let log_height = layout[0].height.saturating_sub(2) as usize;
    let lines: Vec<Line> = state
        .lines
        .iter()
        .rev()
        .take(log_height)
        .rev()
        .map(|l| {
            let style = if l.starts_with("[OK]") || l.starts_with("DONE") {
                Style::default().fg(Color::Green)
            } else if l.starts_with("[ERROR]") || l.starts_with("[WARN]") {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::White)
            };
            Line::from(Span::styled(l.as_str(), style))
        })
        .collect();

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color)),
        ),
        layout[0],
    );

    // Progress bar
    let (ratio, label) = if state.done {
        (1.0f64, "Complete".to_string())
    } else if state.progress_total > 0 {
        let pct = state.progress_current as f64 / state.progress_total as f64;
        (
            pct,
            format!("{}/{} files", state.progress_current, state.progress_total),
        )
    } else {
        (0.0, "Waiting…".to_string())
    };

    let gauge_color = if state.done { Color::Green } else { Color::Cyan };
    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title(" Progress "))
        .gauge_style(Style::default().fg(gauge_color))
        .ratio(ratio.clamp(0.0, 1.0))
        .label(label);
    frame.render_widget(gauge, layout[1]);

    // Status line
    let status = if state.done {
        match &state.result {
            Some(Ok(())) => Line::from(Span::styled(
                "Scrape successful. Press any key to reindex and return.",
                Style::default().fg(Color::Green),
            )),
            Some(Err(e)) => Line::from(Span::styled(
                format!("Scrape failed: {e}"),
                Style::default().fg(Color::Red),
            )),
            None => Line::raw(""),
        }
    } else {
        Line::from(Span::styled(
            "Scraping…  [Ctrl+C] force quit",
            Style::default().fg(Color::DarkGray),
        ))
    };
    frame.render_widget(Paragraph::new(status), layout[2]);
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

fn path_spans(path: &str) -> Vec<Span<'_>> {
    let sep = if path.contains('/') { '/' } else { '\\' };
    match path.rfind(sep) {
        Some(i) => vec![
            Span::styled(&path[..=i], Style::default().fg(Color::DarkGray)),
            Span::raw(&path[i + 1..]),
        ],
        None => vec![Span::raw(path)],
    }
}

fn highlight_line<'a>(text: &'a str, query: &str) -> Line<'a> {
    let tokens: Vec<String> = query
        .split_whitespace()
        .map(|t| t.to_lowercase())
        .filter(|t| !t.is_empty())
        .collect();

    if tokens.is_empty() {
        return Line::from(Span::raw(text));
    }

    let lower = text.to_lowercase();
    let mut matches: Vec<(usize, usize)> = Vec::new();
    for token in &tokens {
        let mut pos = 0;
        while let Some(offset) = lower[pos..].find(token.as_str()) {
            let start = pos + offset;
            let end = start + token.len();
            matches.push((start, end));
            pos = end;
        }
    }

    if matches.is_empty() {
        return Line::from(Span::raw(text));
    }

    matches.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e) in matches {
        if let Some(last) = merged.last_mut() {
            if s <= last.1 {
                last.1 = last.1.max(e);
                continue;
            }
        }
        merged.push((s, e));
    }

    let highlight = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);

    let mut spans = Vec::new();
    let mut cursor = 0usize;
    for (s, e) in merged {
        if cursor < s {
            spans.push(Span::raw(&text[cursor..s]));
        }
        spans.push(Span::styled(&text[s..e], highlight));
        cursor = e;
    }
    if cursor < text.len() {
        spans.push(Span::raw(&text[cursor..]));
    }

    Line::from(spans)
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{App, Screen};

    fn search_app() -> App {
        let mut app = App::new(true);
        app.screen = Screen::Search;
        app
    }

    #[test]
    fn debounce_requires_elapsed_time() {
        let mut app = search_app();
        app.dirty = true;
        app.last_edit = Instant::now();
        assert!(!app.should_search());

        app.last_edit = Instant::now() - Duration::from_millis(200);
        assert!(app.should_search());
    }

    #[test]
    fn set_results_clamps_selection() {
        let mut app = search_app();
        app.selected = 5;
        app.set_results(vec![]);
        assert_eq!(app.selected, 0);
    }
}

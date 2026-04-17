use std::io;
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
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};

use crate::core::SearchHit;
use crate::service::{PreviewDocument, SearchService};

const SEARCH_LIMIT: usize = 50;
const DEBOUNCE_MS: u64 = 150;

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
}

impl App {
    fn new() -> Self {
        Self {
            query: String::new(),
            results: Vec::new(),
            selected: 0,
            preview: None,
            show_preview: true,
            preview_scroll: 0,
            status: String::new(),
            dirty: true,
            last_edit: Instant::now(),
        }
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
        self.last_edit = Instant::now();
    }

    fn should_search(&self) -> bool {
        self.dirty && self.last_edit.elapsed() >= Duration::from_millis(DEBOUNCE_MS)
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

pub fn run(service: SearchService) -> Result<()> {
    service.ensure_source_exists()?;

    let mut terminal = setup_terminal()?;
    let _guard = TerminalGuard;
    let mut app = App::new();

    // Initial search
    do_search(&service, &mut app)?;

    loop {
        terminal.draw(|frame| render(frame, &app))?;

        if event::poll(Duration::from_millis(16))? {
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

fn handle_key(
    key: crossterm::event::KeyEvent,
    service: &SearchService,
    app: &mut App,
) -> Result<bool> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Ok(true);
    }

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
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            open_in_browser(app)?
        }
        KeyCode::Enter => open_selected(service, app)?,
        KeyCode::PageDown => app.preview_scroll = app.preview_scroll.saturating_add(10),
        KeyCode::PageUp => app.preview_scroll = app.preview_scroll.saturating_sub(10),
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::ALT) => {
            rebuild_index(service, app)?
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

fn do_search(service: &SearchService, app: &mut App) -> Result<()> {
    let results = match service.search(&app.query, SEARCH_LIMIT) {
        Ok(r) => r,
        Err(e) => {
            app.status = format!("Index error: {}. Press r to build.", e);
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

/// Returns the line index of the first query token match in the preview body,
/// so the scroll offset jumps straight to it.
fn first_match_line(preview: &Option<PreviewDocument>, query: &str) -> u16 {
    let Some(preview) = preview else { return 0 };
    let tokens: Vec<String> = query
        .split_whitespace()
        .map(|t| t.to_lowercase())
        .collect();
    if tokens.is_empty() {
        return 0;
    }
    // +3 accounts for title / path / headings lines before body
    for (i, line) in preview.body_lines.iter().enumerate() {
        let lower = line.to_lowercase();
        if tokens.iter().any(|t| lower.contains(t.as_str())) {
            return (i as u16).saturating_sub(2); // show a couple lines of context above
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

fn render(frame: &mut ratatui::Frame<'_>, app: &App) {
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

    // Real terminal cursor inside the box
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
            .map(|hit| {
                ListItem::new(Line::from(path_spans(&hit.path)))
            })
            .collect()
    };

    let results_title = if app.results.is_empty() {
        " Results ".to_string()
    } else {
        format!(" Results ({}) ", app.results.len())
    };
    let list = List::new(items)
        .block(
            Block::default()
                .title(results_title)
                .borders(Borders::ALL),
        )
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
        .block(
            Block::default()
                .title(" Preview ")
                .borders(Borders::ALL),
        )
        .scroll((app.preview_scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
}

fn render_status(frame: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, app: &App) {
    let line = if app.status.is_empty() {
        let help = "[↑↓] select  [Enter] open  [Shift+Enter] browser  [Tab] preview  [PgUp/Dn] scroll  [Alt+R] reindex  [Esc] clear  [Ctrl+C] quit";
        Line::from(Span::styled(help, Style::default().fg(Color::DarkGray)))
    } else {
        Line::from(Span::styled(app.status.as_str(), Style::default().fg(Color::Yellow)))
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// Gray parent path + white filename on one line, e.g. `presentations/` `Access.md`
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

/// Split `text` into spans, highlighting every occurrence of each query token
/// (case-insensitive) with a yellow bold style.
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
    // Collect [start, end) byte ranges of all matches
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

    // Sort and merge overlapping ranges
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

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::App;

    #[test]
    fn debounce_requires_elapsed_time() {
        let mut app = App::new();
        app.dirty = true;
        // Simulate edit happening just now
        app.last_edit = Instant::now();
        assert!(!app.should_search());

        // Simulate edit happening 200ms ago
        app.last_edit = Instant::now() - Duration::from_millis(200);
        assert!(app.should_search());
    }

    #[test]
    fn set_results_clamps_selection() {
        let mut app = App::new();
        app.selected = 5;
        app.set_results(vec![]);
        assert_eq!(app.selected, 0);
    }
}

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
    widgets::{Block, BorderType, Borders, Clear, Gauge, List, ListItem, Paragraph, Wrap},
};

use crate::core::SearchHit;
use crate::credentials;
use crate::scraper::{HtlScraper, ScraperEvent};
use crate::service::{PreviewDocument, SearchService};

const SEARCH_LIMIT: usize = 50;
const DEBOUNCE_MS: u64 = 150;

// TokyoNight Storm palette
const BG: Color = Color::Rgb(31, 35, 53);
const SURFACE: Color = Color::Rgb(36, 40, 59);
const BORDER: Color = Color::Rgb(59, 66, 97);
const MUTED: Color = Color::Rgb(86, 95, 137);
const FG: Color = Color::Rgb(192, 202, 245);
const BLUE: Color = Color::Rgb(122, 162, 247);
const CYAN: Color = Color::Rgb(125, 207, 255);
const GREEN: Color = Color::Rgb(158, 206, 106);
const YELLOW: Color = Color::Rgb(224, 175, 104);
const RED: Color = Color::Rgb(247, 118, 142);
const PURPLE: Color = Color::Rgb(187, 154, 247);

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
    saved_accounts: Vec<String>,
    account_cursor: usize,
}

#[derive(PartialEq)]
enum SetupField {
    AccountList,
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
    username: String,
    password: String,
}

impl SetupState {
    fn new() -> Self {
        let saved_accounts = credentials::list().unwrap_or_default();
        let has_saved = !saved_accounts.is_empty();
        let (active_field, username, password) = if has_saved {
            (SetupField::AccountList, String::new(), String::new())
        } else {
            (
                SetupField::Username,
                std::env::var("HTL_USERNAME").unwrap_or_default(),
                std::env::var("HTL_PASSWORD").unwrap_or_default(),
            )
        };
        Self {
            username,
            password,
            active_field,
            sync_mode: false,
            error: None,
            saved_accounts,
            account_cursor: 0,
        }
    }

    fn active_field_mut(&mut self) -> &mut String {
        match self.active_field {
            SetupField::Username => &mut self.username,
            SetupField::Password => &mut self.password,
            SetupField::AccountList => &mut self.username,
        }
    }

    fn select_saved_account(&mut self) {
        if let Some(user) = self.saved_accounts.get(self.account_cursor) {
            self.username = user.clone();
            self.password = credentials::load_password(user).unwrap_or_default();
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
    let (active_field, has_saved, n_saved, _cursor, user_empty, pass_empty) = {
        let Screen::Setup(ref s) = app.screen else {
            return Ok(false);
        };
        let af = match s.active_field {
            SetupField::AccountList => 0u8,
            SetupField::Username => 1,
            SetupField::Password => 2,
        };
        (af, !s.saved_accounts.is_empty(), s.saved_accounts.len(), s.account_cursor, s.username.is_empty(), s.password.is_empty())
    };

    match key.code {
        KeyCode::Esc => {
            app.screen = Screen::Search;
        }

        KeyCode::Tab | KeyCode::Down => {
            let Screen::Setup(ref mut s) = app.screen else { return Ok(false); };
            s.active_field = match s.active_field {
                SetupField::AccountList => SetupField::Username,
                SetupField::Username => SetupField::Password,
                SetupField::Password => if has_saved { SetupField::AccountList } else { SetupField::Username },
            };
        }

        KeyCode::Up => {
            let Screen::Setup(ref mut s) = app.screen else { return Ok(false); };
            s.active_field = match s.active_field {
                SetupField::AccountList => SetupField::Password,
                SetupField::Username => if has_saved { SetupField::AccountList } else { SetupField::Password },
                SetupField::Password => SetupField::Username,
            };
        }

        KeyCode::Left if active_field == 0 => {
            let Screen::Setup(ref mut s) = app.screen else { return Ok(false); };
            s.account_cursor = s.account_cursor.saturating_sub(1);
        }
        KeyCode::Right if active_field == 0 => {
            let Screen::Setup(ref mut s) = app.screen else { return Ok(false); };
            if s.account_cursor + 1 < n_saved { s.account_cursor += 1; }
        }

        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::ALT) => {
            let Screen::Setup(ref mut s) = app.screen else { return Ok(false); };
            s.sync_mode = !s.sync_mode;
        }

        KeyCode::Enter if active_field == 0 => {
            let Screen::Setup(ref mut s) = app.screen else { return Ok(false); };
            s.select_saved_account();
            s.active_field = SetupField::Username;
        }

        KeyCode::Enter => {
            if user_empty || pass_empty {
                let Screen::Setup(ref mut s) = app.screen else { return Ok(false); };
                s.error = Some("Username and password required.".to_string());
                return Ok(false);
            }
            start_scraping(app);
        }

        KeyCode::Delete if active_field == 0 => {
            let Screen::Setup(ref mut s) = app.screen else { return Ok(false); };
            if let Some(user) = s.saved_accounts.get(s.account_cursor).cloned() {
                let _ = credentials::delete(&user);
                s.saved_accounts.retain(|a| *a != user);
                s.account_cursor = s.account_cursor.min(s.saved_accounts.len().saturating_sub(1));
                if s.saved_accounts.is_empty() {
                    s.active_field = SetupField::Username;
                }
            }
        }

        KeyCode::Backspace if active_field > 0 => {
            let Screen::Setup(ref mut s) = app.screen else { return Ok(false); };
            s.active_field_mut().pop();
        }

        KeyCode::Char(ch) if active_field > 0 && !key.modifiers.contains(KeyModifiers::ALT) && !key.modifiers.contains(KeyModifiers::CONTROL) => {
            let Screen::Setup(ref mut s) = app.screen else { return Ok(false); };
            s.active_field_mut().push(ch);
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
        return Ok(false);
    }

    match key.code {
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char(_) => {
            let (success, username, password) = {
                let Screen::Scraping(ref s) = app.screen else { return Ok(false); };
                (
                    s.result.as_ref().map_or(false, |r| r.is_ok()),
                    s.username.clone(),
                    s.password.clone(),
                )
            };

            app.screen = Screen::Search;
            if success {
                let _ = credentials::save(&username, &password);
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

    let thread_username = username.clone();
    let thread_password = password.clone();
    std::thread::spawn(move || {
        let result = (|| -> Result<(), String> {
            let mut scraper = HtlScraper::new(sync_mode).map_err(|e| e.to_string())?;
            scraper.set_progress_tx(progress_tx);
            scraper
                .run(&thread_username, &thread_password, true)
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
        username,
        password,
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
    // Fill background
    frame.render_widget(
        Block::default().style(Style::default().bg(BG)),
        frame.area(),
    );
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
    let paragraph = Paragraph::new(Line::from(vec![
        Span::styled("> ", Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
        Span::styled(app.query.as_str(), Style::default().fg(FG)),
    ]))
    .block(
        Block::default()
            .title(Span::styled(" Search ", Style::default().fg(MUTED)))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(BLUE))
            .style(Style::default().bg(SURFACE)),
    );
    frame.render_widget(paragraph, area);

    // cursor after "> " prefix (2 chars) + query length
    let cursor_x = area.x + 1 + 2 + app.query.len() as u16;
    let cursor_y = area.y + 1;
    if cursor_x < area.x + area.width.saturating_sub(1) {
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

fn render_results(frame: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, app: &App) {
    let items: Vec<ListItem> = if app.results.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "  no results",
            Style::default().fg(MUTED),
        )))]
    } else {
        app.results
            .iter()
            .map(|hit| ListItem::new(Line::from(path_spans(&hit.path))))
            .collect()
    };

    let title = if app.results.is_empty() {
        " Results ".to_string()
    } else {
        format!(" Results ({}) ", app.results.len())
    };

    let list = List::new(items)
        .block(
            Block::default()
                .title(Span::styled(title, Style::default().fg(MUTED)))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(BORDER))
                .style(Style::default().bg(BG)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(35, 56, 92))
                .fg(FG)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    let mut list_state = ratatui::widgets::ListState::default();
    if !app.results.is_empty() {
        list_state.select(Some(app.selected));
    }
    frame.render_stateful_widget(list, area, &mut list_state);
}

fn render_preview(frame: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, app: &App) {
    let lines: Vec<Line> = match &app.preview {
        None => vec![Line::from(Span::styled(
            "  no document selected",
            Style::default().fg(MUTED),
        ))],
        Some(preview) => {
            let total = preview.body_lines.len();
            let gutter_w = total.to_string().len().max(3);

            let mut v: Vec<Line> = Vec::new();

            // File path header
            let gutter_pad = " ".repeat(gutter_w + 1); // gutter + trailing space
            v.push(Line::from(vec![
                Span::styled(gutter_pad, Style::default().fg(MUTED)),
                Span::styled(
                    preview.path.as_str(),
                    Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
                ),
            ]));
            v.push(Line::raw(""));

            for (i, line_text) in preview.body_lines.iter().enumerate() {
                let num_str = format!("{:>width$} ", i + 1, width = gutter_w);
                let content = highlight_line(line_text, &app.query);
                let mut spans = vec![
                    Span::styled(num_str, Style::default().fg(MUTED)),
                ];
                spans.extend(content.spans);
                v.push(Line::from(spans));
            }
            v
        }
    };

    let widget = Paragraph::new(lines)
        .block(
            Block::default()
                .title(Span::styled(" Preview ", Style::default().fg(MUTED)))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(BORDER))
                .style(Style::default().bg(BG)),
        )
        .scroll((app.preview_scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
}

fn render_status(frame: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, app: &App) {
    let text = if !app.status.is_empty() {
        app.status.clone()
    } else if !app.query.is_empty() {
        format!("/{}", app.query)
    } else {
        "[↑↓] navigate  [Enter] open  [Shift+Enter] browser  [Tab] preview  [PgUp/Dn] scroll  [Alt+R] reindex  [Alt+S] scrape  [Esc] clear".to_string()
    };

    let style = if !app.status.is_empty() {
        Style::default().fg(YELLOW).bg(BG)
    } else {
        Style::default().fg(MUTED).bg(BG)
    };

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(text, style))),
        area,
    );
}

fn render_setup(frame: &mut ratatui::Frame<'_>, state: &SetupState) {
    // Clear background for floating modal effect
    frame.render_widget(Clear, frame.area());
    frame.render_widget(
        Block::default().style(Style::default().bg(BG)),
        frame.area(),
    );

    let area = frame.area();
    let has_accounts = !state.saved_accounts.is_empty();

    let accounts_block_h = if has_accounts {
        (state.saved_accounts.len().min(3) as u16) + 2
    } else {
        0
    };
    let dialog_h = (12 + accounts_block_h).min(area.height);
    let dialog_w = 62u16.min(area.width);
    let dialog = ratatui::layout::Rect {
        x: area.x + (area.width.saturating_sub(dialog_w)) / 2,
        y: area.y + (area.height.saturating_sub(dialog_h)) / 2,
        width: dialog_w,
        height: dialog_h,
    };

    let block = Block::default()
        .title(Span::styled(
            " HTL.dev Setup ",
            Style::default().fg(BLUE).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(PURPLE))
        .style(Style::default().bg(SURFACE));
    let inner_area = block.inner(dialog);
    frame.render_widget(block, dialog);

    let mut constraints = Vec::new();
    if has_accounts {
        constraints.push(Constraint::Length(accounts_block_h));
    }
    constraints.push(Constraint::Length(3));
    constraints.push(Constraint::Length(3));
    constraints.push(Constraint::Length(1));
    constraints.push(Constraint::Length(1));
    constraints.push(Constraint::Length(1));
    let rows = Layout::vertical(constraints).split(inner_area);

    let mut row = 0usize;

    if has_accounts {
        let acct_border = if state.active_field == SetupField::AccountList {
            BLUE
        } else {
            BORDER
        };
        let items: Vec<ListItem> = state
            .saved_accounts
            .iter()
            .enumerate()
            .map(|(i, name)| {
                if i == state.account_cursor && state.active_field == SetupField::AccountList {
                    ListItem::new(Line::from(vec![
                        Span::styled("▶ ", Style::default().fg(BLUE)),
                        Span::styled(
                            name.as_str(),
                            Style::default().fg(FG).add_modifier(Modifier::BOLD),
                        ),
                    ]))
                } else {
                    ListItem::new(Span::styled(
                        format!("  {name}"),
                        Style::default().fg(MUTED),
                    ))
                }
            })
            .collect();
        let accounts_list = List::new(items).block(
            Block::default()
                .title(Span::styled(
                    " Saved accounts  [←→] select  [Enter] use  [Del] remove ",
                    Style::default().fg(MUTED),
                ))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(acct_border)),
        );
        frame.render_widget(accounts_list, rows[row]);
        row += 1;
    }

    // Username
    let user_border = if state.active_field == SetupField::Username { BLUE } else { BORDER };
    frame.render_widget(
        Paragraph::new(state.username.as_str()).block(
            Block::default()
                .title(Span::styled(" Username ", Style::default().fg(MUTED)))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(user_border)),
        ),
        rows[row],
    );
    let user_row = rows[row];
    row += 1;

    // Password
    let pass_border = if state.active_field == SetupField::Password { BLUE } else { BORDER };
    let masked: String = "*".repeat(state.password.len());
    frame.render_widget(
        Paragraph::new(masked.as_str()).block(
            Block::default()
                .title(Span::styled(" Password ", Style::default().fg(MUTED)))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(pass_border)),
        ),
        rows[row],
    );
    let pass_row = rows[row];
    row += 1;

    // Sync toggle
    let sync_label = if state.sync_mode {
        Span::styled(
            "[x] Sync mode (incremental)",
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled("[ ] Sync mode (incremental)", Style::default().fg(MUTED))
    };
    frame.render_widget(Paragraph::new(Line::from(sync_label)), rows[row]);
    row += 1;

    // Error
    if let Some(ref err) = state.error {
        frame.render_widget(
            Paragraph::new(Span::styled(err.as_str(), Style::default().fg(RED))),
            rows[row],
        );
    }
    row += 1;

    // Help
    let help = if has_accounts {
        "[Tab/↑↓] switch  [Alt+S] sync  [Enter] start  [Esc] cancel"
    } else {
        "[Tab] switch  [Alt+S] sync  [Enter] start  [Esc] cancel"
    };
    frame.render_widget(
        Paragraph::new(Span::styled(help, Style::default().fg(MUTED))),
        rows[row],
    );

    // Cursor
    let (cx, cy) = match state.active_field {
        SetupField::AccountList => return,
        SetupField::Username => (user_row.x + 1 + state.username.len() as u16, user_row.y + 1),
        SetupField::Password => (pass_row.x + 1 + state.password.len() as u16, pass_row.y + 1),
    };
    if cx < inner_area.x + inner_area.width {
        frame.set_cursor_position((cx, cy));
    }
}

fn render_scraping(frame: &mut ratatui::Frame<'_>, state: &ScrapingState) {
    let layout = Layout::vertical([
        Constraint::Min(5),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(frame.area());

    let border_color = if state.done { GREEN } else { YELLOW };
    let title = if state.done {
        " Scraping — Done! Press any key to continue "
    } else {
        " Scraping in progress… "
    };

    let log_height = layout[0].height.saturating_sub(2) as usize;
    let lines: Vec<Line> = state
        .lines
        .iter()
        .rev()
        .take(log_height)
        .rev()
        .map(|l| {
            let style = if l.starts_with("[OK]") || l.starts_with("DONE") {
                Style::default().fg(GREEN)
            } else if l.starts_with("[ERROR]") || l.starts_with("[WARN]") {
                Style::default().fg(RED)
            } else {
                Style::default().fg(FG)
            };
            Line::from(Span::styled(l.as_str(), style))
        })
        .collect();

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(Span::styled(title, Style::default().fg(border_color)))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(border_color))
                .style(Style::default().bg(BG)),
        ),
        layout[0],
    );

    let (ratio, label) = if state.done {
        (1.0f64, "Complete".to_string())
    } else if state.progress_total > 0 {
        let pct = state.progress_current as f64 / state.progress_total as f64;
        (pct, format!("{}/{} files", state.progress_current, state.progress_total))
    } else {
        (0.0, "Waiting…".to_string())
    };

    let gauge_color = if state.done { GREEN } else { BLUE };
    let gauge = Gauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(BORDER))
                .title(Span::styled(" Progress ", Style::default().fg(MUTED))),
        )
        .gauge_style(Style::default().fg(gauge_color).bg(SURFACE))
        .ratio(ratio.clamp(0.0, 1.0))
        .label(Span::styled(
            label,
            Style::default().fg(FG).add_modifier(Modifier::BOLD),
        ));
    frame.render_widget(gauge, layout[1]);

    let status = if state.done {
        match &state.result {
            Some(Ok(())) => Line::from(Span::styled(
                "Scrape successful. Press any key to reindex and return.",
                Style::default().fg(GREEN),
            )),
            Some(Err(e)) => Line::from(Span::styled(
                format!("Scrape failed: {e}"),
                Style::default().fg(RED),
            )),
            None => Line::raw(""),
        }
    } else {
        Line::from(Span::styled(
            "Scraping…  [Ctrl+C] force quit",
            Style::default().fg(MUTED),
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
            Span::styled(&path[..=i], Style::default().fg(MUTED)),
            Span::styled(&path[i + 1..], Style::default().fg(FG)),
        ],
        None => vec![Span::styled(path, Style::default().fg(FG))],
    }
}

fn highlight_line<'a>(text: &'a str, query: &str) -> Line<'a> {
    let tokens: Vec<String> = query
        .split_whitespace()
        .map(|t| t.to_lowercase())
        .filter(|t| !t.is_empty())
        .collect();

    if tokens.is_empty() {
        return Line::from(Span::styled(text, Style::default().fg(FG)));
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
        return Line::from(Span::styled(text, Style::default().fg(FG)));
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
        .fg(YELLOW)
        .bg(Color::Rgb(58, 45, 20))
        .add_modifier(Modifier::BOLD);

    let mut spans = Vec::new();
    let mut cursor = 0usize;
    for (s, e) in merged {
        if cursor < s {
            spans.push(Span::styled(&text[cursor..s], Style::default().fg(FG)));
        }
        spans.push(Span::styled(&text[s..e], highlight));
        cursor = e;
    }
    if cursor < text.len() {
        spans.push(Span::styled(&text[cursor..], Style::default().fg(FG)));
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

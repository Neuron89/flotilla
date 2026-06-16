//! flotilla — terminal control plane for the agent fleet.
//!
//! Lists every session on every host on the tailnet, shows live status, and
//! lets you attach (raw, tmux-style), spawn, and kill from one keyboard-driven
//! screen. Detach with Ctrl-] and the session keeps running on its host.

mod client;

use anyhow::Result;
use client::{Client, Host, HostsFile, ProfileInfo, SessionInfo};
use clap::Parser;
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::{SinkExt, StreamExt};
use ratatui::{prelude::*, widgets::*};
use std::io::{stdout, Write};
use std::path::PathBuf;
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[derive(Parser, Debug)]
#[command(name = "flotilla", version, about = "Terminal control plane for the Flotilla agent fleet")]
struct Args {
    /// Path to a hosts TOML file (a list of [[hosts]] with label/url/token).
    #[arg(short, long, env = "FLOTILLA_HOSTS")]
    config: Option<PathBuf>,
}

struct Row {
    host_idx: usize,
    info: SessionInfo,
}

struct HostStatus {
    reachable: bool,
    meta: String,
}

enum Mode {
    Normal,
    NewSession { host_idx: usize, profiles: Vec<ProfileInfo>, sel: usize },
}

struct App {
    hosts: Vec<Host>,
    client: Client,
    rows: Vec<Row>,
    host_status: Vec<HostStatus>,
    list_state: ListState,
    status_msg: String,
    mode: Mode,
}

impl App {
    fn selected_row(&self) -> Option<&Row> {
        self.list_state.selected().and_then(|i| self.rows.get(i))
    }

    async fn refresh(&mut self) {
        let mut rows = Vec::new();
        let mut statuses = Vec::new();
        for (idx, host) in self.hosts.iter().enumerate() {
            match self.client.health(host).await {
                Ok(h) => {
                    let sessions = self.client.sessions(host).await.unwrap_or_default();
                    statuses.push(HostStatus {
                        reachable: true,
                        meta: format!("{}/{} · {} sess", h.os, h.arch, sessions.len()),
                    });
                    for info in sessions {
                        rows.push(Row { host_idx: idx, info });
                    }
                }
                Err(_) => statuses.push(HostStatus { reachable: false, meta: "unreachable".into() }),
            }
        }
        self.rows = rows;
        self.host_status = statuses;
        // Keep selection in range.
        let len = self.rows.len();
        if len == 0 {
            self.list_state.select(None);
        } else {
            let sel = self.list_state.selected().unwrap_or(0).min(len - 1);
            self.list_state.select(Some(sel));
        }
    }

    fn move_sel(&mut self, delta: i32) {
        let len = self.rows.len();
        if len == 0 {
            return;
        }
        let cur = self.list_state.selected().unwrap_or(0) as i32;
        let next = (cur + delta).rem_euclid(len as i32) as usize;
        self.list_state.select(Some(next));
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let hosts = HostsFile::load(args.config.as_deref())?;

    let mut app = App {
        hosts,
        client: Client::new(),
        rows: Vec::new(),
        host_status: Vec::new(),
        list_state: ListState::default(),
        status_msg: "loading…".into(),
        mode: Mode::Normal,
    };

    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;

    let res = run(&mut terminal, &mut app).await;

    disable_raw_mode()?;
    execute!(stdout(), LeaveAlternateScreen)?;
    res
}

async fn run<B: Backend + Write>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()> {
    app.refresh().await;
    app.status_msg = "ready".into();

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_secs(3));

    loop {
        terminal.draw(|f| ui(f, app))?;

        tokio::select! {
            _ = tick.tick() => {
                if matches!(app.mode, Mode::Normal) {
                    app.refresh().await;
                }
            }
            maybe_ev = events.next() => {
                let Some(Ok(ev)) = maybe_ev else { continue };
                if let Event::Key(k) = ev {
                    if k.kind != KeyEventKind::Press { continue; }
                    match &app.mode {
                        Mode::Normal => {
                            match k.code {
                                KeyCode::Char('q') | KeyCode::Esc => break,
                                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => break,
                                KeyCode::Up | KeyCode::Char('k') => app.move_sel(-1),
                                KeyCode::Down | KeyCode::Char('j') => app.move_sel(1),
                                KeyCode::Char('r') => { app.status_msg = "refreshing…".into(); app.refresh().await; app.status_msg = "ready".into(); }
                                KeyCode::Char('n') => open_new_session(app).await,
                                KeyCode::Char('x') | KeyCode::Char('K') => kill_selected(app).await,
                                KeyCode::Enter => {
                                    if let Some(row) = app.selected_row() {
                                        let host = app.hosts[row.host_idx].clone();
                                        let id = row.info.id.clone();
                                        let name = row.info.name.clone();
                                        // Suspend the UI for a raw interactive attach.
                                        let r = attach(&host, &id, &name, &mut events).await;
                                        terminal.clear()?;
                                        match r {
                                            Ok(_) => app.status_msg = format!("detached from {name}"),
                                            Err(e) => app.status_msg = format!("attach error: {e}"),
                                        }
                                        app.refresh().await;
                                    }
                                }
                                _ => {}
                            }
                        }
                        Mode::NewSession { .. } => handle_new_session_key(app, k.code).await,
                    }
                }
            }
        }
    }
    Ok(())
}

async fn open_new_session(app: &mut App) {
    let host_idx = app.selected_row().map(|r| r.host_idx).unwrap_or(0);
    if app.hosts.is_empty() {
        return;
    }
    let host = app.hosts[host_idx].clone();
    match app.client.profiles(&host).await {
        Ok(profiles) => app.mode = Mode::NewSession { host_idx, profiles, sel: 0 },
        Err(e) => app.status_msg = format!("can't load profiles: {e}"),
    }
}

async fn handle_new_session_key(app: &mut App, code: KeyCode) {
    let (host_idx, sel, len, chosen) = match &app.mode {
        Mode::NewSession { host_idx, profiles, sel } => {
            (*host_idx, *sel, profiles.len(), profiles.get(*sel).map(|p| p.name.clone()))
        }
        _ => return,
    };
    match code {
        KeyCode::Esc => app.mode = Mode::Normal,
        KeyCode::Up | KeyCode::Char('k') => {
            if let Mode::NewSession { sel, .. } = &mut app.mode {
                *sel = if sel.checked_sub(1).is_some() && len > 0 { (*sel + len - 1) % len } else { 0 };
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Mode::NewSession { sel, .. } = &mut app.mode {
                if len > 0 { *sel = (*sel + 1) % len; }
            }
        }
        KeyCode::Enter => {
            let _ = sel;
            if let Some(profile) = chosen {
                let host = app.hosts[host_idx].clone();
                match app.client.create(&host, &profile).await {
                    Ok(s) => app.status_msg = format!("created {}", s.name),
                    Err(e) => app.status_msg = format!("create failed: {e}"),
                }
            }
            app.mode = Mode::Normal;
            app.refresh().await;
        }
        _ => {}
    }
}

async fn kill_selected(app: &mut App) {
    let Some(row) = app.selected_row() else { return };
    let host = app.hosts[row.host_idx].clone();
    let id = row.info.id.clone();
    let name = row.info.name.clone();
    match app.client.kill(&host, &id).await {
        Ok(_) => app.status_msg = format!("killed {name}"),
        Err(e) => app.status_msg = format!("kill failed: {e}"),
    }
    app.refresh().await;
}

fn state_color(state: &str) -> Color {
    match state {
        "working" => Color::Rgb(255, 180, 84),
        "waiting" => Color::Rgb(255, 93, 108),
        "idle" => Color::Rgb(107, 122, 146),
        "exited" => Color::Rgb(69, 80, 103),
        _ => Color::Gray,
    }
}

fn ui(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1)])
        .split(f.area());

    // Title bar with per-host reachability chips.
    let mut title_spans = vec![
        Span::styled(" ⚓ FLOTILLA ", Style::default().fg(Color::Black).bg(Color::Rgb(79, 140, 255)).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
    ];
    for (i, h) in app.hosts.iter().enumerate() {
        match app.host_status.get(i) {
            Some(s) if s.reachable => {
                title_spans.push(Span::styled(format!("● {} ", h.label), Style::default().fg(Color::Rgb(74, 217, 145))));
                title_spans.push(Span::styled(format!("{}  ", s.meta), Style::default().fg(Color::DarkGray)));
            }
            _ => title_spans.push(Span::styled(format!("● {} (down)  ", h.label), Style::default().fg(Color::Rgb(255, 93, 108)))),
        }
    }
    f.render_widget(Paragraph::new(Line::from(title_spans)), chunks[0]);

    // Session list
    let mut items: Vec<ListItem> = Vec::new();
    if app.rows.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "  no sessions — press 'n' to spawn one",
            Style::default().fg(Color::DarkGray),
        ))));
    }
    for row in &app.rows {
        let host = &app.hosts[row.host_idx];
        let color = state_color(&row.info.state);
        let mut spans = vec![
            Span::styled(format!(" {:<12}", trunc(&host.label, 12)), Style::default().fg(Color::Rgb(139, 155, 180))),
            Span::styled(format!(" {:<8} ", row.info.state.to_uppercase()), Style::default().fg(Color::Black).bg(color).add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            Span::styled(row.info.name.clone(), Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(format!("  ({})", row.info.profile), Style::default().fg(Color::DarkGray)),
        ];
        if !row.info.tags.is_empty() {
            spans.push(Span::styled(format!("  [{}]", row.info.tags.join(", ")), Style::default().fg(Color::Rgb(79, 140, 255))));
        }
        if let Some(code) = row.info.exit_code {
            spans.push(Span::styled(format!("  exit={code}"), Style::default().fg(Color::DarkGray)));
        }
        items.push(ListItem::new(Line::from(spans)));
    }
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" sessions ").border_style(Style::default().fg(Color::Rgb(36, 48, 73))))
        .highlight_style(Style::default().bg(Color::Rgb(26, 34, 51)))
        .highlight_symbol("▌");
    let mut state = app.list_state.clone();
    f.render_stateful_widget(list, chunks[1], &mut state);

    // Footer
    let footer = Line::from(vec![
        Span::styled(" ↑↓/jk ", Style::default().fg(Color::Rgb(79, 140, 255))), Span::raw("move  "),
        Span::styled("⏎ ", Style::default().fg(Color::Rgb(79, 140, 255))), Span::raw("attach  "),
        Span::styled("n ", Style::default().fg(Color::Rgb(79, 140, 255))), Span::raw("new  "),
        Span::styled("x ", Style::default().fg(Color::Rgb(79, 140, 255))), Span::raw("kill  "),
        Span::styled("r ", Style::default().fg(Color::Rgb(79, 140, 255))), Span::raw("refresh  "),
        Span::styled("q ", Style::default().fg(Color::Rgb(79, 140, 255))), Span::raw("quit   "),
        Span::styled(app.status_msg.clone(), Style::default().fg(Color::Rgb(74, 217, 145))),
    ]);
    f.render_widget(Paragraph::new(footer), chunks[2]);

    // New-session popup
    if let Mode::NewSession { host_idx, profiles, sel } = &app.mode {
        let area = centered_rect(50, 60, f.area());
        f.render_widget(Clear, area);
        let host_label = app.hosts.get(*host_idx).map(|h| h.label.as_str()).unwrap_or("?");
        let items: Vec<ListItem> = profiles
            .iter()
            .map(|p| {
                let desc = p.description.clone().unwrap_or_default();
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:<10}", p.name), Style::default().add_modifier(Modifier::BOLD)),
                    Span::styled(desc, Style::default().fg(Color::DarkGray)),
                ]))
            })
            .collect();
        let mut pstate = ListState::default();
        pstate.select(Some(*sel));
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(format!(" new session on {host_label}  (⏎ create, esc cancel) ")).border_style(Style::default().fg(Color::Rgb(79, 140, 255))))
            .highlight_style(Style::default().bg(Color::Rgb(79, 140, 255)).fg(Color::Black))
            .highlight_symbol("▌");
        f.render_stateful_widget(list, area, &mut pstate);
    }
}

fn trunc(s: &str, n: usize) -> String {
    if s.len() <= n { s.to_string() } else { format!("{}…", &s[..n.saturating_sub(1)]) }
}

fn centered_rect(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage((100 - pct_y) / 2), Constraint::Percentage(pct_y), Constraint::Percentage((100 - pct_y) / 2)])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage((100 - pct_x) / 2), Constraint::Percentage(pct_x), Constraint::Percentage((100 - pct_x) / 2)])
        .split(v[1])[1]
}

/// Raw interactive attach. Writes PTY output straight to the terminal and
/// forwards keystrokes (translated to bytes) until Ctrl-] is pressed.
async fn attach(host: &Host, id: &str, name: &str, events: &mut EventStream) -> Result<()> {
    let scheme_url = host.url.replacen("http", "ws", 1);
    let mut url = format!("{scheme_url}/api/sessions/{id}/attach");
    if !host.token.is_empty() {
        url.push_str(&format!("?token={}", host.token));
    }

    let (stream, _) = connect_async(url).await?;
    let (mut tx, mut rx) = stream.split();

    let mut out = stdout();
    execute!(out, crossterm::terminal::Clear(crossterm::terminal::ClearType::All), crossterm::cursor::MoveTo(0, 0))?;
    write!(out, "\x1b[90m[flotilla] attached to {name} — Ctrl-] to detach\x1b[0m\r\n")?;
    out.flush()?;

    // Tell the agent our size.
    let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
    let _ = tx.send(Message::Text(serde_json::json!({ "type": "resize", "cols": cols, "rows": rows }).to_string())).await;

    loop {
        tokio::select! {
            msg = rx.next() => {
                match msg {
                    Some(Ok(Message::Binary(b))) => { out.write_all(&b)?; out.flush()?; }
                    Some(Ok(Message::Text(t))) => { out.write_all(t.as_bytes())?; out.flush()?; }
                    Some(Ok(Message::Close(_))) | None => {
                        write!(out, "\r\n\x1b[90m[flotilla] session closed\x1b[0m\r\n")?;
                        out.flush()?;
                        break;
                    }
                    _ => {}
                }
            }
            ev = events.next() => {
                match ev {
                    Some(Ok(Event::Key(k))) if k.kind == KeyEventKind::Press => {
                        if k.code == KeyCode::Char(']') && k.modifiers.contains(KeyModifiers::CONTROL) {
                            break;
                        }
                        let bytes = client::key_to_bytes(&k);
                        if !bytes.is_empty() {
                            tx.send(Message::Binary(bytes)).await?;
                        }
                    }
                    Some(Ok(Event::Resize(c, r))) => {
                        let _ = tx.send(Message::Text(serde_json::json!({ "type": "resize", "cols": c, "rows": r }).to_string())).await;
                    }
                    None => break,
                    _ => {}
                }
            }
        }
    }
    let _ = tx.send(Message::Close(None)).await;
    Ok(())
}

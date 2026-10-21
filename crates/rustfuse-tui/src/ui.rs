//! Drawing: a sidebar of volumes and connections, the selected one's page,
//! a footer with the keys that work there, and any form or dialog on top.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Gauge, List, ListItem, ListState, Paragraph, Row, Table,
    TableState, Tabs, Wrap,
};
use rustfuse_sync::provider::Kind;

use crate::app::{App, Entry, Focus, Loaded, Modal, State, Tab, format_size};
use crate::form::{FieldKind, Form};
use crate::library::{self, Mode};

const ACCENT: Color = Color::Cyan;
const DIM: Style = Style::new().fg(Color::DarkGray);

pub fn draw(f: &mut Frame, app: &App) {
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(5), Constraint::Length(2)]).areas(f.area());
    draw_header(f, app, header);
    let [side, pane] = Layout::horizontal([Constraint::Length(30), Constraint::Min(30)]).areas(body);
    draw_sidebar(f, app, side);
    match app.entry() {
        Some(Entry::Volume(_)) => draw_volume(f, app, pane),
        Some(Entry::Connection(_)) => draw_connection(f, app, pane),
        None => draw_welcome(f, pane),
    }
    draw_footer(f, app, footer);
    match &app.modal {
        Some(Modal::Form(_, form)) => draw_form(f, form),
        Some(Modal::Confirm { text, .. }) => draw_confirm(f, text),
        Some(Modal::Log { key, lines, scroll }) => draw_log(f, key, lines, *scroll),
        Some(Modal::Help) => draw_help(f),
        None => {}
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let line = Line::from(vec![
        Span::styled(" rustFUSE ", Style::new().fg(Color::Black).bg(ACCENT).bold()),
        Span::raw("  "),
        Span::styled(app.places.library.display().to_string(), DIM),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn pane_block(title: &str, focused: bool) -> Block<'static> {
    Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(if focused { Style::new().fg(ACCENT) } else { DIM })
        .title(Span::styled(format!(" {title} "), Style::new().bold()))
}

fn state_span(app: &App, key: &str, mode: Mode) -> Span<'static> {
    match app.state(key) {
        State::Syncing => Span::styled("● syncing", Style::new().fg(Color::Green)),
        State::Starting => Span::styled("◌ starting", Style::new().fg(Color::Yellow)),
        State::Elsewhere => Span::styled("● Mac app", Style::new().fg(Color::Green)),
        State::Stopped if mode == Mode::Finder => Span::styled("○ Mac app", DIM),
        State::Stopped => Span::styled("○ stopped", DIM),
    }
}

fn draw_sidebar(f: &mut Frame, app: &App, area: Rect) {
    let mut items = Vec::new();
    let mut selected = None;
    let push_header = |items: &mut Vec<ListItem>, text: &str| {
        items.push(ListItem::new(Line::from(Span::styled(
            text.to_string(),
            DIM.add_modifier(Modifier::BOLD),
        ))));
    };
    push_header(&mut items, "VOLUMES");
    if app.lib.volumes.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled("  none yet", DIM))));
    }
    for (i, entry) in app.entries.iter().enumerate() {
        if let Entry::Connection(_) = entry
            && (i == 0 || matches!(app.entries[i - 1], Entry::Volume(_)))
        {
            items.push(ListItem::new(""));
            push_header(&mut items, "CONNECTIONS");
        }
        if i == app.selected {
            selected = Some(items.len());
        }
        let line = match entry {
            Entry::Volume(key) => {
                let mode = app.lib.volume(key).map_or(Mode::Folder, |v| v.mode);
                let state = state_span(app, key, mode);
                let width = (area.width as usize).saturating_sub(4 + state.width());
                Line::from(vec![Span::raw(format!(" {key:<width$}")), state])
            }
            Entry::Connection(id) => {
                let name = app.lib.connection(id).map_or("?", |c| c.name.as_str());
                Line::from(format!(" {name}"))
            }
        };
        items.push(ListItem::new(line));
    }
    if app.lib.connections.is_empty() {
        items.push(ListItem::new(""));
        push_header(&mut items, "CONNECTIONS");
        items.push(ListItem::new(Line::from(Span::styled(
            "  press a to add one",
            DIM,
        ))));
    }
    let highlight = if app.focus == Focus::Sidebar {
        Style::new().bg(ACCENT).fg(Color::Black)
    } else {
        Style::new().add_modifier(Modifier::REVERSED)
    };
    let list = List::new(items)
        .block(pane_block("rustFUSE", app.focus == Focus::Sidebar))
        .highlight_style(highlight);
    let mut state = ListState::default().with_selected(selected);
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_welcome(f: &mut Frame, area: Rect) {
    let text = Text::from(vec![
        Line::from(""),
        Line::from("Nothing here yet.".bold()),
        Line::from(""),
        Line::from("Press a to add a connection to a hub (with its token from"),
        Line::from("`rustfuse key create`), then pick one of its volumes to add."),
    ]);
    f.render_widget(
        Paragraph::new(text)
            .alignment(Alignment::Center)
            .block(pane_block("Welcome", false)),
        area,
    );
}

fn draw_volume(f: &mut Frame, app: &App, area: Rect) {
    let Some(v) = app.selected_volume() else { return };
    let block = pane_block(&v.key, app.focus == Focus::Pane);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [tabs, body] = Layout::vertical([Constraint::Length(2), Constraint::Min(3)]).areas(inner);
    let titles = ["1 Overview", "2 Files", "3 In the bucket"];
    let index = match app.tab {
        Tab::Overview => 0,
        Tab::Files => 1,
        Tab::Bucket => 2,
    };
    f.render_widget(
        Tabs::new(titles)
            .select(index)
            .highlight_style(Style::new().fg(ACCENT).bold())
            .divider("│"),
        tabs,
    );
    match app.tab {
        Tab::Overview => draw_overview(f, app, body),
        Tab::Files => draw_files(f, app, body),
        Tab::Bucket => draw_bucket(f, app, body),
    }
}

fn label(text: &str) -> Span<'static> {
    Span::styled(format!("{text:<14}"), DIM)
}

fn draw_overview(f: &mut Frame, app: &App, area: Rect) {
    let Some(v) = app.selected_volume() else { return };
    let conn = app.lib.connection(&v.connection);
    let dir = library::synced_dir(v, &app.places.data);
    let mut lines = vec![
        Line::from(vec![
            label("State"),
            state_span(app, &v.key, v.mode),
            Span::styled(state_note(app, &v.key, v.mode), DIM),
        ]),
        Line::from(vec![label("Volume"), Span::raw(v.name.clone())]),
        Line::from(vec![
            label("Connection"),
            Span::raw(conn.map_or("(missing)".to_string(), |c| format!("{}  ", c.name))),
            Span::styled(conn.map_or(String::new(), |c| c.hub.clone()), DIM),
        ]),
    ];
    let shown = match v.mode {
        Mode::Finder => "in Finder (Mac app)".to_string(),
        Mode::Folder => format!("folder {}", v.path.as_deref().unwrap_or("")),
        Mode::Mount => format!("mounted at {}", v.path.as_deref().unwrap_or("")),
    };
    lines.push(Line::from(vec![label("Shown as"), Span::raw(shown)]));
    if v.mode != Mode::Folder {
        lines.push(Line::from(vec![
            label("Synced copy"),
            Span::styled(dir.display().to_string(), DIM),
        ]));
    }
    lines.push(Line::from(""));
    let usage = app.usage.get(&v.key);
    let [top, gauge_area, rest] = Layout::vertical([
        Constraint::Length(lines.len() as u16),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);
    f.render_widget(Paragraph::new(lines), top);

    let mut more = Vec::new();
    match usage {
        Some(u) => {
            let limit = u.limit.or(v.cache_size).filter(|_| u.lazy);
            let (ratio, text) = match limit {
                Some(l) if l > 0 => (
                    (u.usage.cached as f64 / l as f64).min(1.0),
                    format!("{} of {} cache used", format_size(u.usage.cached), format_size(l)),
                ),
                _ => (
                    if u.usage.total > 0 {
                        (u.usage.local as f64 / u.usage.total as f64).min(1.0)
                    } else {
                        0.0
                    },
                    format!("{} on this machine", format_size(u.usage.local)),
                ),
            };
            let gauge = Gauge::default().ratio(ratio).label(text).gauge_style(
                Style::new()
                    .fg(if limit.is_some() && ratio > 0.9 {
                        Color::Yellow
                    } else {
                        ACCENT
                    })
                    .bg(Color::Black),
            );
            let [l, g] = Layout::horizontal([Constraint::Length(14), Constraint::Max(60)]).areas(gauge_area);
            f.render_widget(Paragraph::new(label("Disk")), l);
            f.render_widget(gauge, g);
            more.push(Line::from(vec![
                label(""),
                Span::styled(
                    format!(
                        "{} files, {} in the volume",
                        thousands(u.usage.files),
                        format_size(u.usage.total)
                    ),
                    DIM,
                ),
            ]));
            if u.lazy {
                more.push(Line::from(vec![
                    label(""),
                    Span::styled(
                        format!(
                            "{} here: git {} · kept {} · cached {}",
                            format_size(u.usage.local),
                            format_size(u.usage.git),
                            format_size(u.usage.pinned),
                            format_size(u.usage.cached),
                        ),
                        DIM,
                    ),
                ]));
            }
        }
        None => {
            let note = match (v.mode.is_lazy(), v.cache_size) {
                (true, Some(c)) => format!("cache size {}; start the volume to see disk use", format_size(c)),
                _ => "start the volume to see disk use".to_string(),
            };
            f.render_widget(
                Paragraph::new(Line::from(vec![label("Disk"), Span::styled(note, DIM)])),
                gauge_area,
            );
        }
    }
    more.push(Line::from(""));
    match app.conflicts.get(&v.key) {
        None | Some(Loaded::Loading) => more.push(Line::from(vec![
            label("Conflicts"),
            Span::styled("checking…", DIM),
        ])),
        Some(Loaded::Failed(e)) => {
            more.push(Line::from(vec![label("Conflicts"), Span::styled(e.clone(), DIM)]))
        }
        Some(Loaded::Ready(c)) if c.files.is_empty() && c.branches.is_empty() => more.push(Line::from(vec![
            label("Conflicts"),
            Span::styled("none", Style::new().fg(Color::Green)),
        ])),
        Some(Loaded::Ready(c)) => {
            more.push(Line::from(vec![
                label("Conflicts"),
                Span::styled(
                    format!("{} to look at", c.files.len() + c.branches.len()),
                    Style::new().fg(Color::Yellow).bold(),
                ),
            ]));
            for file in &c.files {
                more.push(Line::from(vec![label(""), Span::raw(format!("copy   {file}"))]));
            }
            for b in &c.branches {
                more.push(Line::from(vec![
                    label(""),
                    Span::raw(format!("branch {} in {}", b.branch, b.repo)),
                ]));
            }
            more.push(Line::from(vec![
                label(""),
                Span::styled("Keep what you want from each copy, then delete it.", DIM),
            ]));
        }
    }
    f.render_widget(Paragraph::new(more).wrap(Wrap { trim: false }), rest);
}

fn state_note(app: &App, key: &str, mode: Mode) -> String {
    match app.state(key) {
        State::Syncing => format!("  as a {}", app.services.describe()),
        State::Starting => "  not answering yet; press l for its log".into(),
        State::Elsewhere => "  run by the Mac app".into(),
        State::Stopped if mode == Mode::Finder => "  Finder volumes run from the Mac app".into(),
        State::Stopped => "  press s to start".into(),
    }
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn breadcrumb(parts: &[String]) -> Line<'static> {
    let mut spans = vec![Span::styled("/", DIM)];
    for p in parts {
        spans.push(Span::raw(p.clone()));
        spans.push(Span::styled("/", DIM));
    }
    Line::from(spans)
}

fn loading_or_error<T>(f: &mut Frame, loaded: &Loaded<T>, area: Rect) -> bool {
    match loaded {
        Loaded::Loading => {
            f.render_widget(Paragraph::new(Span::styled("Loading…", DIM)), area);
            true
        }
        Loaded::Failed(e) => {
            f.render_widget(
                Paragraph::new(Span::styled(e.clone(), Style::new().fg(Color::Red)))
                    .wrap(Wrap { trim: false }),
                area,
            );
            true
        }
        Loaded::Ready(_) => false,
    }
}

fn draw_files(f: &mut Frame, app: &App, area: Rect) {
    let Some(files) = &app.files else { return };
    let [crumb, table_area] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
    let names: Vec<String> = files.path.iter().map(|(_, n)| n.clone()).collect();
    f.render_widget(Paragraph::new(breadcrumb(&names)), crumb);
    if app.state(&files.key) == State::Stopped {
        f.render_widget(
            Paragraph::new(Span::styled(
                "The volume isn't running; press s to start it, then r.",
                DIM,
            )),
            table_area,
        );
        return;
    }
    if loading_or_error(f, &files.items, table_area) {
        return;
    }
    let items = files.items.ready().unwrap();
    let rows = items.iter().map(|i| {
        let (here, style) = match (i.kind, i.downloaded) {
            (Kind::Dir, _) => ("", Style::new()),
            (_, true) => ("✓ here", Style::new().fg(Color::Green)),
            (_, false) => ("☁ bucket", Style::new().fg(Color::Blue)),
        };
        let mut note = Vec::new();
        if i.pinned {
            note.push("kept");
        }
        if !i.synced {
            note.push("this machine only");
        }
        let name = match i.kind {
            Kind::Dir => format!("{}/", i.name),
            Kind::Symlink => format!("{} → {}", i.name, i.target.as_deref().unwrap_or("")),
            Kind::File => i.name.clone(),
        };
        Row::new(vec![
            Cell::from(name).style(if i.kind == Kind::Dir {
                Style::new().bold()
            } else {
                Style::new()
            }),
            Cell::from(if i.kind == Kind::Dir {
                String::new()
            } else {
                format_size(i.size)
            }),
            Cell::from(here).style(style),
            Cell::from(note.join(", ")).style(Style::new().fg(Color::Magenta)),
        ])
    });
    draw_table(
        f,
        rows,
        ["Name", "Size", "Where", ""],
        [
            Constraint::Min(20),
            Constraint::Length(10),
            Constraint::Length(9),
            Constraint::Length(20),
        ],
        files.selected,
        items.is_empty(),
        app.focus == Focus::Pane,
        table_area,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_table<'a>(
    f: &mut Frame,
    rows: impl IntoIterator<Item = Row<'a>>,
    header: [&'a str; 4],
    widths: [Constraint; 4],
    selected: usize,
    empty: bool,
    focused: bool,
    area: Rect,
) {
    if empty {
        f.render_widget(Paragraph::new(Span::styled("Empty folder.", DIM)), area);
        return;
    }
    let table = Table::new(rows, widths)
        .header(Row::new(header).style(DIM.add_modifier(Modifier::BOLD)))
        .row_highlight_style(if focused {
            Style::new().bg(ACCENT).fg(Color::Black)
        } else {
            Style::new().add_modifier(Modifier::REVERSED)
        })
        .column_spacing(2);
    let mut state = TableState::default().with_selected(Some(selected));
    f.render_stateful_widget(table, area, &mut state);
}

fn draw_bucket(f: &mut Frame, app: &App, area: Rect) {
    let Some(bucket) = &app.bucket else { return };
    let [crumb, table_area] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
    f.render_widget(Paragraph::new(breadcrumb(&bucket.path)), crumb);
    if loading_or_error(f, &bucket.entries, table_area) {
        return;
    }
    let entries = bucket.entries.ready().unwrap();
    let rows = entries.iter().map(|e| {
        let name = match e.kind {
            Kind::Dir => format!("{}/", e.name),
            Kind::Symlink => format!("{} → {}", e.name, e.target.as_deref().unwrap_or("")),
            Kind::File => e.name.clone(),
        };
        Row::new(vec![
            Cell::from(name).style(if e.kind == Kind::Dir {
                Style::new().bold()
            } else {
                Style::new()
            }),
            Cell::from(if e.kind == Kind::Dir {
                String::new()
            } else {
                format_size(e.size)
            }),
            Cell::from(date(e.mtime_ns)).style(DIM),
            Cell::from(""),
        ])
    });
    draw_table(
        f,
        rows,
        ["Name", "Size", "Modified (UTC)", ""],
        [
            Constraint::Min(20),
            Constraint::Length(10),
            Constraint::Length(16),
            Constraint::Length(0),
        ],
        bucket.selected,
        entries.is_empty(),
        app.focus == Focus::Pane,
        table_area,
    );
}

/// Nanoseconds since 1970 as `YYYY-MM-DD HH:MM` (UTC).
pub fn date(ns: i64) -> String {
    let secs = ns.div_euclid(1_000_000_000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", rem / 3600, rem % 3600 / 60)
}

fn draw_connection(f: &mut Frame, app: &App, area: Rect) {
    let Some(c) = app.selected_connection() else {
        return;
    };
    let block = pane_block(&c.name, app.focus == Focus::Pane);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let token = app.secrets.get(crate::secrets::Kind::HubToken, &c.id).is_some();
    let mut lines = vec![
        Line::from(vec![label("Hub"), Span::raw(c.hub.clone())]),
        Line::from(vec![
            label("Token"),
            if token {
                Span::styled("saved", Style::new().fg(Color::Green))
            } else {
                Span::styled("none (press e to add one)", Style::new().fg(Color::Yellow))
            },
        ]),
    ];
    if let Some(ca) = &c.ca_file {
        lines.push(Line::from(vec![label("Hub CA"), Span::styled(ca.clone(), DIM)]));
    }
    match &c.s3 {
        Some(s3) => lines.push(Line::from(vec![
            label("Bucket"),
            Span::raw(s3.bucket.clone()),
            Span::styled(
                format!(
                    "  {}{}",
                    s3.endpoint.as_deref().unwrap_or("AWS"),
                    s3.prefix.as_deref().map(|p| format!(" /{p}")).unwrap_or_default()
                ),
                DIM,
            ),
        ])),
        None => lines.push(Line::from(vec![
            label("Bucket"),
            Span::styled("through the hub", DIM),
        ])),
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Volumes in the bucket",
        Style::new().bold(),
    )));
    let [top, list_area] =
        Layout::vertical([Constraint::Length(lines.len() as u16), Constraint::Min(1)]).areas(inner);
    f.render_widget(Paragraph::new(lines), top);
    match app.remote.get(&c.id) {
        None => {}
        Some(loaded) if loading_or_error(f, loaded, list_area) => {}
        Some(Loaded::Ready(names)) if names.is_empty() => f.render_widget(
            Paragraph::new(Span::styled("None yet. Press n to make one from a folder.", DIM)),
            list_area,
        ),
        Some(Loaded::Ready(names)) => {
            let items: Vec<ListItem> = names
                .iter()
                .map(|n| {
                    let used: Vec<&str> = app
                        .lib
                        .volumes
                        .iter()
                        .filter(|v| v.name == *n && v.connection.eq_ignore_ascii_case(&c.id))
                        .map(|v| v.key.as_str())
                        .collect();
                    let mut spans = vec![Span::raw(format!("{n:<24}"))];
                    if !used.is_empty() {
                        spans.push(Span::styled(format!("added as {}", used.join(", ")), DIM));
                    }
                    ListItem::new(Line::from(spans))
                })
                .collect();
            let list = List::new(items).highlight_style(if app.focus == Focus::Pane {
                Style::new().bg(ACCENT).fg(Color::Black)
            } else {
                Style::new()
            });
            let mut state = ListState::default().with_selected(Some(app.remote_selected));
            f.render_stateful_widget(list, list_area, &mut state);
        }
        Some(_) => {}
    }
}

fn keys_for(app: &App) -> Vec<(&'static str, &'static str)> {
    if app.modal.is_some() {
        return match app.modal {
            Some(Modal::Form(..)) => vec![
                ("Tab", "next"),
                ("Ctrl-S", "save"),
                ("←→", "choose"),
                ("Esc", "cancel"),
            ],
            Some(Modal::Confirm { .. }) => vec![("y", "yes"), ("n", "no")],
            Some(Modal::Log { .. }) => vec![("↑↓ PgUp PgDn", "scroll"), ("r", "reload"), ("Esc", "close")],
            _ => vec![("any key", "close")],
        };
    }
    let mut keys = vec![];
    match app.entry() {
        Some(Entry::Volume(_)) => {
            match (app.tab, app.focus) {
                (Tab::Files, Focus::Pane) => keys.extend([
                    ("Enter", "open"),
                    ("⌫", "up"),
                    ("p", "keep"),
                    ("f", "free"),
                    ("d", "download"),
                ]),
                (Tab::Bucket, Focus::Pane) => keys.extend([("Enter", "open"), ("⌫", "up"), ("g", "get")]),
                _ => keys.push(("1 2 3", "tabs")),
            }
            keys.extend([("s", "start/stop"), ("c", "cache"), ("l", "log"), ("x", "remove")]);
        }
        Some(Entry::Connection(_)) => {
            if app.focus == Focus::Pane {
                keys.push(("Enter", "add volume"));
            } else {
                keys.push(("Tab", "volumes"));
            }
            keys.extend([("n", "new volume"), ("e", "edit"), ("x", "remove")]);
        }
        None => {}
    }
    keys.extend([
        ("a", "add connection"),
        ("S", "settings"),
        ("?", "help"),
        ("q", "quit"),
    ]);
    keys
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let [status, keys] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    let line = if let Some(m) = &app.message {
        Line::from(Span::styled(
            format!(" {}", m.text),
            if m.error {
                Style::new().fg(Color::Red).bold()
            } else {
                Style::new().fg(Color::Green)
            },
        ))
    } else if !app.busy.is_empty() {
        let spinner = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let i = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            / 100) as usize
            % spinner.len();
        Line::from(Span::styled(
            format!(" {} {}…", spinner[i], app.busy.join(", ")),
            Style::new().fg(Color::Yellow),
        ))
    } else {
        Line::from("")
    };
    f.render_widget(Paragraph::new(line), status);
    let mut spans = Vec::new();
    for (k, what) in keys_for(app) {
        spans.push(Span::styled(
            format!(" {k} "),
            Style::new().fg(Color::Black).bg(Color::Gray),
        ));
        spans.push(Span::styled(format!(" {what}  "), DIM));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), keys);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn modal_block(title: &str) -> Block<'static> {
    Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(Span::styled(format!(" {title} "), Style::new().bold()))
}

fn draw_form(f: &mut Frame, form: &Form) {
    let label_w = form
        .fields
        .iter()
        .map(|x| x.label.chars().count())
        .max()
        .unwrap_or(0) as u16
        + 2;
    let width = 76;
    let note_lines = wrapped_height(&form.note, width - 4);
    let error_lines = form
        .error
        .as_ref()
        .map_or(0, |e| wrapped_height(e, width - 4) + 1);
    let height = form.fields.len() as u16 + note_lines + error_lines + 4;
    let area = centered(f.area(), width, height);
    f.render_widget(Clear, area);
    let block = modal_block(&form.title);
    let inner = block.inner(area).inner(Margin::new(1, 0));
    f.render_widget(block, area);
    let mut y = inner.y;
    for (i, field) in form.fields.iter().enumerate() {
        let focused = i == form.focus;
        let row = Rect {
            x: inner.x,
            y,
            width: inner.width,
            height: 1,
        };
        let [l, v] = Layout::horizontal([Constraint::Length(label_w), Constraint::Min(5)]).areas(row);
        f.render_widget(
            Paragraph::new(Span::styled(
                field.label.clone(),
                if focused {
                    Style::new().fg(ACCENT).bold()
                } else {
                    DIM
                },
            )),
            l,
        );
        // A dark grey that dimmed hints still show on.
        let value_style = if focused && !matches!(field.kind, FieldKind::Choice(_)) {
            Style::new().bg(Color::Indexed(236))
        } else {
            Style::new()
        };
        let value = match (&field.kind, field.value.is_empty()) {
            (FieldKind::Choice(options), _) => Line::from(
                options
                    .iter()
                    .flat_map(|o| {
                        let on = *o == field.value;
                        [
                            Span::styled(
                                format!(" {} {o} ", if on { "◉" } else { "○" }),
                                if on {
                                    Style::new().fg(ACCENT).bold()
                                } else {
                                    Style::new().fg(Color::Gray)
                                },
                            ),
                            Span::raw(" "),
                        ]
                    })
                    .collect::<Vec<_>>(),
            ),
            (_, true) => Line::from(Span::styled(
                field.hint.clone(),
                DIM.add_modifier(Modifier::ITALIC),
            )),
            (_, false) => Line::from(Span::raw(field.shown())),
        };
        f.render_widget(Paragraph::new(value).style(value_style), v);
        if focused && !matches!(field.kind, FieldKind::Choice(_)) {
            let x = v.x + (field.cursor as u16).min(v.width.saturating_sub(1));
            f.set_cursor_position(Position::new(x, y));
        }
        y += 1;
    }
    y += 1;
    if let Some(e) = &form.error {
        let h = wrapped_height(e, inner.width);
        f.render_widget(
            Paragraph::new(Span::styled(e.clone(), Style::new().fg(Color::Red))).wrap(Wrap { trim: true }),
            Rect {
                x: inner.x,
                y,
                width: inner.width,
                height: h,
            },
        );
        y += h + 1;
    }
    let rest = Rect {
        x: inner.x,
        y,
        width: inner.width,
        height: (inner.y + inner.height).saturating_sub(y),
    };
    f.render_widget(
        Paragraph::new(Span::styled(form.note.clone(), DIM)).wrap(Wrap { trim: true }),
        rest,
    );
}

fn wrapped_height(text: &str, width: u16) -> u16 {
    if text.is_empty() {
        return 0;
    }
    text.lines()
        .map(|l| (l.chars().count() as u16).div_ceil(width.max(1)).max(1))
        .sum()
}

fn draw_confirm(f: &mut Frame, text: &str) {
    let area = centered(f.area(), 60, wrapped_height(text, 56) + 4);
    f.render_widget(Clear, area);
    let block = modal_block("Are you sure?");
    let inner = block.inner(area).inner(Margin::new(1, 0));
    f.render_widget(block, area);
    let mut lines = vec![Line::from(text.to_string())];
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(" y ", Style::new().bg(Color::Red).fg(Color::White)),
        Span::raw(" yes   "),
        Span::styled(" n ", Style::new().bg(Color::Gray).fg(Color::Black)),
        Span::raw(" no"),
    ]));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

fn draw_log(f: &mut Frame, key: &str, lines: &[String], scroll: usize) {
    let outer = f.area();
    let area = Rect {
        x: outer.x + 2,
        y: outer.y + 1,
        width: outer.width.saturating_sub(4),
        height: outer.height.saturating_sub(3),
    };
    f.render_widget(Clear, area);
    let block = modal_block(&format!("Log of {key}"));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let h = inner.height as usize;
    // `scroll` is the bottom line shown.
    let end = (scroll + 1).min(lines.len());
    let start = end.saturating_sub(h);
    let shown: Vec<Line> = lines[start..end]
        .iter()
        .map(|l| {
            let style = if l.contains("ERROR") {
                Style::new().fg(Color::Red)
            } else if l.contains("WARN") {
                Style::new().fg(Color::Yellow)
            } else {
                Style::new()
            };
            Line::from(Span::styled(l.clone(), style))
        })
        .collect();
    f.render_widget(Paragraph::new(shown), inner);
}

fn draw_help(f: &mut Frame) {
    let rows = [
        ("Everywhere", ""),
        ("↑ ↓  j k", "move"),
        ("Tab", "between the list and the page"),
        ("a", "add a connection to a hub"),
        ("S", "settings: default cache size"),
        ("r", "refresh"),
        ("q", "quit (volumes keep syncing)"),
        ("", ""),
        ("A volume", ""),
        ("1 2 3", "Overview, Files, In the bucket"),
        ("s", "start or stop syncing in the background"),
        ("c", "cache size (mounts)"),
        ("l", "the daemon's log"),
        ("x", "stop and forget it (files stay)"),
        ("p f d", "Files: keep downloaded, free, download"),
        ("g", "In the bucket: download a copy elsewhere"),
        ("", ""),
        ("A connection", ""),
        ("Enter", "add the volume picked from the bucket"),
        ("n", "make a new volume from a folder"),
        ("e  x", "edit, remove"),
    ];
    let area = centered(f.area(), 64, rows.len() as u16 + 2);
    f.render_widget(Clear, area);
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, what)| {
            if what.is_empty() {
                Line::from(Span::styled(k.to_string(), Style::new().bold()))
            } else {
                Line::from(vec![
                    Span::styled(format!("  {k:<12}"), Style::new().fg(ACCENT)),
                    Span::raw(what.to_string()),
                ])
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines).block(modal_block("Keys")), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(date(0), "1970-01-01 00:00");
        assert_eq!(date(1_792_500_000_000_000_000), "2026-10-20 12:40");
        assert_eq!(thousands(48_210), "48,210");
        assert_eq!(thousands(999), "999");
    }
}

#[cfg(test)]
mod render_tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::app::App;
    use crate::library::Library;
    use crate::places::Places;
    use crate::secrets::Secrets;
    use crate::service::Services;

    fn text(t: &Terminal<TestBackend>) -> String {
        let buf = t.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// Every screen and dialog draws, at a usual size and a tiny one.
    #[test]
    fn screens_draw() {
        let dir = tempfile::tempdir().unwrap();
        let places = Places::in_dir(dir.path());
        let lib: Library = serde_json::from_str(include_str!(
            "../../../macos/RustFuseKit/Tests/RustFuseKitTests/library.json"
        ))
        .unwrap();
        lib.save(&places.library).unwrap();
        let services = Services::Detached {
            dir: dir.path().join("run"),
        };
        let mut app = App::new(
            places.clone(),
            Secrets::File(places.secrets.clone()),
            services,
            "/bin/false".into(),
        )
        .unwrap();
        app.settle(std::time::Duration::from_secs(5));
        let press = |app: &mut App, code| app.key(KeyEvent::new(code, KeyModifiers::NONE));
        for size in [(120, 36), (30, 8)] {
            let mut t = Terminal::new(TestBackend::new(size.0, size.1)).unwrap();
            let mut screens = Vec::new();
            for keys in [
                vec![],
                vec![KeyCode::Char('2')],
                vec![KeyCode::Char('3')],
                vec![KeyCode::Char('c')],
                vec![KeyCode::Esc, KeyCode::Char('l')],
                vec![KeyCode::Esc, KeyCode::Char('x')],
                vec![KeyCode::Char('n'), KeyCode::Char('?')],
                vec![KeyCode::Esc, KeyCode::Char('S')],
                vec![
                    KeyCode::Esc,
                    KeyCode::Tab,
                    KeyCode::Down,
                    KeyCode::Down,
                    KeyCode::Down,
                ],
                vec![KeyCode::Char('e')],
                vec![KeyCode::Esc, KeyCode::Char('a')],
                vec![KeyCode::Esc],
            ] {
                for k in keys {
                    press(&mut app, k);
                }
                app.settle(std::time::Duration::from_secs(5));
                t.draw(|f| super::draw(f, &app)).unwrap();
                screens.push(text(&t));
            }
            if size.0 == 120 {
                assert!(
                    screens[0].contains("Overview") && screens[0].contains("proj"),
                    "{}",
                    screens[0]
                );
                assert!(screens[3].contains("Cache size of proj"), "{}", screens[3]);
                assert!(screens[5].contains("Are you sure?"), "{}", screens[5]);
                assert!(screens[8].contains("Office"), "{}", screens[8]);
                assert!(screens[9].contains("Edit connection"), "{}", screens[9]);
            }
            press(&mut app, KeyCode::Esc);
            press(&mut app, KeyCode::Tab);
            for _ in 0..5 {
                press(&mut app, KeyCode::Up);
            }
        }
    }
}

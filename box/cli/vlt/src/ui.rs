use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{
    App, FormFocus, FormMode, FormState, GenState, ListState as AppListState, RowKind, Screen,
    SetupFocus, SetupState, StatusKind, UnlockState,
};

const ACCENT: Color = Color::Yellow;
const MUTED: Color = Color::DarkGray;
const SOFT: Color = Color::Gray;

pub fn render(f: &mut Frame, app: &App) {
    let area = f.area();
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(area);

    render_topbar(f, chunks[0], app);
    match &app.screen {
        Screen::Setup(s) => render_setup(f, chunks[1], s),
        Screen::Unlock(s) => render_unlock(f, chunks[1], s),
        Screen::List(s) => render_list(f, chunks[1], app, s),
        Screen::Detail(s) => render_detail(f, chunks[1], app, &s.id, s.reveal),
        Screen::Form(s) => render_form(f, chunks[1], s),
        Screen::Generator(s) => render_generator(f, chunks[1], s),
    }
    render_statusbar(f, chunks[2], app);

    if let Screen::List(l) = &app.screen
        && let Some(id) = l.confirm_delete_id.clone()
    {
        render_delete_modal(f, area, app, &id);
    }

    if app.show_help {
        render_help_modal(f, area);
    }
}

// =====================================================================
// Topbar / statusbar
// =====================================================================

fn render_topbar(f: &mut Frame, area: Rect, app: &App) {
    let mut left = vec![
        Span::styled("▣ ", Style::default().fg(ACCENT)),
        Span::styled("vlt", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled(
            format!("v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(MUTED),
        ),
    ];
    let path = app.vault_path.display().to_string();
    if !path.is_empty() {
        left.push(Span::raw("  "));
        left.push(Span::styled("vault ", Style::default().fg(MUTED)));
        left.push(Span::styled(path, Style::default().fg(SOFT)));
    }

    let right = if app.vault.is_some() {
        "unlocked"
    } else {
        "locked"
    };

    let layout = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(u16::try_from(right.len() + 2).unwrap_or(u16::MAX)),
    ])
    .split(area);
    f.render_widget(Paragraph::new(Line::from(left)), layout[0]);
    f.render_widget(
        Paragraph::new(Span::styled(
            right,
            Style::default()
                .fg(if app.vault.is_some() { ACCENT } else { MUTED })
                .add_modifier(Modifier::BOLD),
        ))
        .alignment(Alignment::Right),
        layout[1],
    );
}

fn render_statusbar(f: &mut Frame, area: Rect, app: &App) {
    let hints = match &app.screen {
        Screen::Setup(_) => "Tab next  Enter submit  Ctrl+R reveal  Esc quit",
        Screen::Unlock(_) => "Enter unlock  Ctrl+R reveal  Esc quit",
        Screen::List(l) if l.searching => "type to filter  Enter/↓ confirm  Esc cancel",
        Screen::List(_) => {
            "↵ open  n new  e edit  d delete  c/y copy pw/user  : search  L lock  ? help  q quit"
        }
        Screen::Detail(_) => {
            "r reveal  c/y/u copy pw/user/url  gx open url  N copy link N  gN open link N  e edit  d delete  q back"
        }
        Screen::Form(_) => {
            "Tab next  Ctrl+S save  Ctrl+N add link  Ctrl+X del link  Ctrl+G generate  Ctrl+R reveal  Esc"
        }
        Screen::Generator(_) => "+/- length  s symbols  n numbers  g/↵ regen  c copy  q back",
    };

    let layout = Layout::horizontal([Constraint::Min(0), Constraint::Length(60)]).split(area);

    f.render_widget(
        Paragraph::new(Span::styled(hints, Style::default().fg(MUTED))),
        layout[0],
    );

    if let Some((msg, _, kind)) = &app.status {
        let style = match kind {
            StatusKind::Info => Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            StatusKind::Error => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        };
        f.render_widget(
            Paragraph::new(Span::styled(msg.clone(), style)).alignment(Alignment::Right),
            layout[1],
        );
    }
}

// =====================================================================
// Setup / unlock
// =====================================================================

fn centered(width: u16, height: u16, area: Rect) -> Rect {
    let h = (area.width.saturating_sub(width)) / 2;
    let v = (area.height.saturating_sub(height)) / 2;
    Rect {
        x: area.x + h,
        y: area.y + v,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

fn input_block(title: &str, focused: bool) -> Block<'_> {
    let style = if focused {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(MUTED)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(style)
        .title(Span::styled(title, style))
}

fn render_setup(f: &mut Frame, area: Rect, s: &SetupState) {
    let area = centered(54, 14, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(MUTED))
        .title(Span::styled(
            " Set vault password ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Clear, area);
    f.render_widget(block, area);

    let inner = area.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    let chunks = Layout::vertical([
        Constraint::Length(1), // hint
        Constraint::Length(3), // password
        Constraint::Length(3), // confirm
        Constraint::Length(1), // error
        Constraint::Min(0),
    ])
    .split(inner);

    f.render_widget(
        Paragraph::new(Span::styled(
            "encrypts the local vault — no recovery if forgotten",
            Style::default().fg(MUTED),
        )),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new(masked(&s.password, s.reveal)).block(input_block(
            " Vault password ",
            s.focus == SetupFocus::Password,
        )),
        chunks[1],
    );
    f.render_widget(
        Paragraph::new(masked(&s.confirm, s.reveal))
            .block(input_block(" Confirm ", s.focus == SetupFocus::Confirm)),
        chunks[2],
    );
    if let Some(err) = &s.error {
        f.render_widget(
            Paragraph::new(Span::styled(err.clone(), Style::default().fg(Color::Red))),
            chunks[3],
        );
    }
}

fn render_unlock(f: &mut Frame, area: Rect, s: &UnlockState) {
    let area = centered(54, 11, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(MUTED))
        .title(Span::styled(
            " Unlock vault ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Clear, area);
    f.render_widget(block, area);

    let inner = area.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(inner);

    f.render_widget(
        Paragraph::new(Span::styled(
            "session lasts 1 hour after unlock",
            Style::default().fg(MUTED),
        )),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new(masked(&s.password, s.reveal)).block(input_block(" Vault password ", true)),
        chunks[1],
    );
    if let Some(err) = &s.error {
        f.render_widget(
            Paragraph::new(Span::styled(err.clone(), Style::default().fg(Color::Red))),
            chunks[2],
        );
    }
}

fn masked(value: &str, reveal: bool) -> Line<'_> {
    if reveal {
        Line::from(value.to_string())
    } else {
        Line::from("•".repeat(value.chars().count()))
    }
}

// =====================================================================
// List
// =====================================================================

#[allow(clippy::too_many_lines)]
fn render_list(f: &mut Frame, area: Rect, app: &App, l: &AppListState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(MUTED));
    f.render_widget(block.clone(), area);
    let inner = area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });

    let chunks = Layout::vertical([
        Constraint::Length(3), // search
        Constraint::Min(0),    // tree
    ])
    .split(inner);

    // search input
    let search_focus = l.searching;
    let search_line = if l.search.is_empty() && !search_focus {
        Line::from(Span::styled(
            "press : to search",
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        ))
    } else {
        Line::from(vec![
            Span::styled(
                ":",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(l.search.clone()),
        ])
    };
    f.render_widget(
        Paragraph::new(search_line).block(input_block(" Search ", search_focus)),
        chunks[0],
    );

    // tree rows
    let rows = app.visible_rows(l);
    if rows.is_empty() {
        let msg = if app.vault.as_ref().is_some_and(|v| v.items().is_empty()) {
            "vault is empty — press n to add a credential"
        } else {
            "no matches"
        };
        f.render_widget(
            Paragraph::new(Span::styled(msg, Style::default().fg(MUTED)))
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true }),
            chunks[1],
        );
        return;
    }

    // Fixed-width columns so username/date align across rows regardless of
    // group depth. The whole `indent + title` block is padded to `title_col_w`
    // so subsequent columns start at the same x position.
    let user_w = 24usize;
    let date_w = 16usize;
    let gap = 2usize;
    let total_w = chunks[1].width as usize;
    let title_col_w = total_w
        .saturating_sub(user_w + date_w + gap * 2 + 2) // 2 = block borders
        .max(20);

    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| {
            let indent = "  ".repeat(row.depth);
            match &row.kind {
                RowKind::Group {
                    name,
                    count,
                    expanded,
                    ..
                } => {
                    let chev = if *expanded { "▾" } else { "▸" };
                    ListItem::new(Line::from(vec![
                        Span::raw(indent),
                        Span::styled(
                            format!("{chev} "),
                            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(name.clone(), Style::default().add_modifier(Modifier::BOLD)),
                        Span::raw("  "),
                        Span::styled(format!("({count})"), Style::default().fg(MUTED)),
                    ]))
                }
                RowKind::Item(it) => {
                    let prefix_w = row.depth * 2 + 2; // indent + "  " spacer
                    let title_avail = title_col_w.saturating_sub(prefix_w);
                    let title_text = truncate(it.title.clone(), title_avail);
                    let title_block = pad_to_width(&format!("{indent}  {title_text}"), title_col_w);
                    let user = pad_to_width(&truncate(it.username.clone(), user_w), user_w);
                    let date = pad_to_width(&short_date(&it.updated_at.to_rfc3339()), date_w);
                    ListItem::new(Line::from(vec![
                        Span::raw(title_block),
                        Span::raw(" ".repeat(gap)),
                        Span::styled(user, Style::default().fg(SOFT)),
                        Span::raw(" ".repeat(gap)),
                        Span::styled(date, Style::default().fg(MUTED)),
                    ]))
                }
            }
        })
        .collect();

    let mut state = ListState::default();
    if !items.is_empty() {
        state.select(Some(l.selected.min(items.len() - 1)));
    }
    let list = List::new(items).highlight_style(
        Style::default()
            .bg(ACCENT)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD),
    );
    f.render_stateful_widget(list, chunks[1], &mut state);
}

/// Truncate a string so that its terminal display width does not exceed `max`.
/// Appends `…` when truncated. Handles wide (CJK) characters correctly.
fn truncate(s: String, max: usize) -> String {
    if s.width() <= max {
        return s;
    }
    let mut out = String::new();
    let mut w = 0usize;
    let cap = max.saturating_sub(1); // reserve 1 cell for the ellipsis
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > cap {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// Right-pad a string with spaces so that its terminal display width is `target`.
/// If the string is already at or beyond `target`, it is returned unchanged.
fn pad_to_width(s: &str, target: usize) -> String {
    let w = s.width();
    if w >= target {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + (target - w));
    out.push_str(s);
    for _ in 0..(target - w) {
        out.push(' ');
    }
    out
}

fn short_date(iso: &str) -> String {
    if iso.is_empty() {
        return String::new();
    }
    let s: String = iso.chars().take(16).collect();
    s.replace('T', " ")
}

// =====================================================================
// Detail
// =====================================================================

#[allow(clippy::too_many_lines)]
fn render_detail(f: &mut Frame, area: Rect, app: &App, id: &str, reveal: bool) {
    let item = app
        .vault
        .as_ref()
        .and_then(|v| v.find_item(id).ok().cloned());
    let Some(item) = item else {
        f.render_widget(
            Paragraph::new("(item not found)").style(Style::default().fg(MUTED)),
            area,
        );
        return;
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(MUTED))
        .title(" Detail ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::vertical([
        Constraint::Length(1), // title
        Constraint::Length(1), // meta
        Constraint::Length(1), // blank
        Constraint::Length(3), // 3 field rows
        Constraint::Min(0),    // notes
    ])
    .split(inner);

    // Title + crumb
    let crumb = if item.group.is_empty() {
        String::new()
    } else {
        format!(
            "    {}",
            item.group
                .split('/')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" › ")
        )
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                item.title.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(crumb, Style::default().fg(MUTED)),
        ])),
        chunks[0],
    );

    // Meta
    let meta = format!(
        "created {}   updated {}",
        short_date(&item.created_at.to_rfc3339()),
        short_date(&item.updated_at.to_rfc3339())
    );
    f.render_widget(
        Paragraph::new(Span::styled(meta, Style::default().fg(MUTED))),
        chunks[1],
    );

    // Field rows with right-aligned action-key hints
    let total_w = chunks[3].width as usize;
    let pwd_display: String = if item.password.is_empty() {
        String::new()
    } else if reveal {
        item.password.clone()
    } else {
        "•".repeat(item.password.chars().count().min(24))
    };
    let pwd_style = Style::default().fg(if reveal { Color::Reset } else { SOFT });
    let field_lines = vec![
        detail_row(
            "Username",
            &item.username,
            &["y"],
            Style::default(),
            total_w,
        ),
        detail_row("Password", &pwd_display, &["c"], pwd_style, total_w),
        detail_row(
            "URL",
            &item.url,
            if item.url.is_empty() {
                &[]
            } else {
                &["u", "gx"]
            },
            Style::default(),
            total_w,
        ),
    ];
    f.render_widget(Paragraph::new(field_lines), chunks[3]);

    // Notes + Links — same column as field rows
    let mut tail_lines: Vec<Line> = Vec::new();

    if !item.notes.is_empty() {
        let mut first = true;
        for nl in item.notes.lines() {
            if first {
                tail_lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {label:<10}", label = "Notes"),
                        Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(nl.to_string()),
                ]));
                first = false;
            } else {
                tail_lines.push(Line::from(vec![
                    Span::raw(" ".repeat(12)),
                    Span::raw(nl.to_string()),
                ]));
            }
        }
    }

    if !item.links.is_empty() {
        if !tail_lines.is_empty() {
            tail_lines.push(Line::from(""));
        }
        for (i, link) in item.links.iter().enumerate() {
            let label_str = if i == 0 {
                format!("  {label:<10}", label = "Links")
            } else {
                " ".repeat(12)
            };
            let n = i + 1;
            let mut header = vec![
                Span::styled(
                    label_str,
                    Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("[{n}] "),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
            ];
            let name = if link.name.is_empty() {
                "(unnamed)".to_string()
            } else {
                link.name.clone()
            };
            header.push(Span::styled(
                name,
                Style::default().add_modifier(Modifier::BOLD),
            ));
            if !link.description.is_empty() {
                header.push(Span::styled(
                    format!(" — {}", link.description),
                    Style::default().fg(MUTED),
                ));
            }
            tail_lines.push(Line::from(header));
            // URL line with copy/open hints — digit alone copies link N's URL,
            // `g{N}` opens link N in the default browser.
            let copy_key = format!("{n}");
            let open_key = format!("g{n}");
            tail_lines.push(detail_row(
                "",
                &link.url,
                &[copy_key.as_str(), open_key.as_str()],
                Style::default().fg(SOFT),
                total_w,
            ));
        }
    }

    f.render_widget(
        Paragraph::new(tail_lines).wrap(Wrap { trim: false }),
        chunks[4],
    );
}

fn detail_row(
    label: &str,
    value: &str,
    hints: &[&str],
    value_style: Style,
    total_w: usize,
) -> Line<'static> {
    let prefix = format!("  {label:<10}");
    let prefix_w: usize = 12;
    let show_hints = !value.is_empty() && !hints.is_empty();
    // Each hint renders inline as " [key]". Reserve that width up front so a
    // long value gets truncated instead of pushing the brackets off-screen.
    let hints_w: usize = if show_hints {
        hints.iter().map(|h| h.chars().count() + 3).sum()
    } else {
        0
    };
    let max_value_w = total_w.saturating_sub(prefix_w + hints_w);
    let value_str = if value.chars().count() > max_value_w {
        let mut s: String = value.chars().take(max_value_w.saturating_sub(1)).collect();
        s.push('…');
        s
    } else {
        value.to_string()
    };
    let mut spans = vec![
        Span::styled(
            prefix,
            Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
        ),
        Span::styled(value_str, value_style),
    ];
    if show_hints {
        for h in hints {
            spans.push(Span::raw(" "));
            spans.push(Span::styled("[", Style::default().fg(MUTED)));
            spans.push(Span::styled(
                h.to_string(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled("]", Style::default().fg(MUTED)));
        }
    }
    Line::from(spans)
}

// =====================================================================
// Form
// =====================================================================

fn render_form(f: &mut Frame, area: Rect, s: &FormState) {
    let title = match &s.mode {
        FormMode::Create => " New credential ",
        FormMode::Edit(_) => " Edit credential ",
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(MUTED))
        .title(Span::styled(
            title,
            Style::default().add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();

    push_field_row(&mut lines, s, FormFocus::Title, "Title", &s.title);
    push_field_row(&mut lines, s, FormFocus::Username, "Username", &s.username);
    push_password_row(&mut lines, s);
    push_field_row(&mut lines, s, FormFocus::Url, "URL", &s.url);
    push_field_row(&mut lines, s, FormFocus::Group, "Group", &s.group);
    push_notes_rows(&mut lines, s);

    lines.push(Line::from(""));

    push_links_section(&mut lines, s);

    if let Some(err) = &s.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("  {err}"),
            Style::default().fg(Color::Red),
        )));
    }

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn label_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(MUTED).add_modifier(Modifier::BOLD)
    }
}

fn push_field_row(
    lines: &mut Vec<Line<'static>>,
    s: &FormState,
    focus: FormFocus,
    label: &str,
    value: &str,
) {
    let focused = s.focus == focus;
    let mut value_str = value.to_string();
    if focused {
        value_str.push('▌');
    }
    lines.push(Line::from(vec![
        Span::styled(format!("  {label:<10}"), label_style(focused)),
        Span::raw(value_str),
    ]));
}

fn push_password_row(lines: &mut Vec<Line<'static>>, s: &FormState) {
    let focused = s.focus == FormFocus::Password;
    let display = if s.reveal {
        s.password.clone()
    } else {
        "•".repeat(s.password.chars().count().min(24))
    };
    let mut value_str = display;
    if focused {
        value_str.push('▌');
    }
    let mut spans = vec![
        Span::styled(
            format!("  {label:<10}", label = "Password"),
            label_style(focused),
        ),
        Span::styled(
            value_str,
            Style::default().fg(if s.reveal { Color::Reset } else { SOFT }),
        ),
    ];
    if focused {
        spans.push(Span::styled(
            "    Ctrl+R reveal · Ctrl+G generate",
            Style::default().fg(MUTED),
        ));
    }
    lines.push(Line::from(spans));
}

fn push_notes_rows(lines: &mut Vec<Line<'static>>, s: &FormState) {
    let focused = s.focus == FormFocus::Notes;
    let raw_lines: Vec<&str> = if s.notes.is_empty() {
        vec![""]
    } else {
        s.notes.split('\n').collect()
    };
    let last = raw_lines.len() - 1;
    for (i, raw) in raw_lines.iter().enumerate() {
        let value_str = if focused && i == last {
            format!("{raw}▌")
        } else {
            (*raw).to_string()
        };
        let prefix = if i == 0 {
            Span::styled(
                format!("  {label:<10}", label = "Notes"),
                label_style(focused),
            )
        } else {
            Span::raw(" ".repeat(12))
        };
        lines.push(Line::from(vec![prefix, Span::raw(value_str)]));
    }
}

fn push_links_section(lines: &mut Vec<Line<'static>>, s: &FormState) {
    let any_link_focus = matches!(
        s.focus,
        FormFocus::LinkName(_)
            | FormFocus::LinkDescription(_)
            | FormFocus::LinkUrl(_)
            | FormFocus::AddLink
    );

    for (i, link) in s.links.iter().enumerate() {
        let show_main_label = i == 0;
        let main_label_focused = i == 0 && any_link_focus;
        push_link_subrow(
            lines,
            show_main_label,
            true,
            i + 1,
            "Name",
            &link.name,
            s,
            FormFocus::LinkName(i),
            main_label_focused,
        );
        push_link_subrow(
            lines,
            false,
            false,
            i + 1,
            "Description",
            &link.description,
            s,
            FormFocus::LinkDescription(i),
            false,
        );
        push_link_subrow(
            lines,
            false,
            false,
            i + 1,
            "URL",
            &link.url,
            s,
            FormFocus::LinkUrl(i),
            false,
        );
    }

    // Trailing "[+] add link" row
    let no_links = s.links.is_empty();
    let main_label_focused = no_links && s.focus == FormFocus::AddLink;
    let main_prefix = if no_links {
        Span::styled(
            format!("  {label:<10}", label = "Links"),
            label_style(main_label_focused),
        )
    } else {
        Span::raw(" ".repeat(12))
    };
    let focused = s.focus == FormFocus::AddLink;
    let bracket_style = if focused {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(MUTED)
    };
    let text = if focused {
        "[+] add link▌"
    } else {
        "[+] add link"
    };
    lines.push(Line::from(vec![
        main_prefix,
        Span::styled(text.to_string(), bracket_style),
    ]));
}

#[allow(clippy::too_many_arguments)]
fn push_link_subrow(
    lines: &mut Vec<Line<'static>>,
    show_main_label: bool,
    show_link_num: bool,
    link_num: usize,
    sub_label: &str,
    value: &str,
    s: &FormState,
    focus: FormFocus,
    main_label_focused: bool,
) {
    let focused = s.focus == focus;
    let mut value_str = value.to_string();
    if focused {
        value_str.push('▌');
    }
    let main_prefix = if show_main_label {
        Span::styled(
            format!("  {label:<10}", label = "Links"),
            label_style(main_label_focused),
        )
    } else {
        Span::raw(" ".repeat(12))
    };
    let n_span = if show_link_num {
        Span::styled(
            format!("[{link_num}] "),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("    ")
    };
    let sub_label_style = if focused {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(MUTED)
    };
    lines.push(Line::from(vec![
        main_prefix,
        n_span,
        Span::styled(format!("{sub_label:<12}"), sub_label_style),
        Span::raw(value_str),
    ]));
}

// =====================================================================
// Generator
// =====================================================================

fn render_generator(f: &mut Frame, area: Rect, g: &GenState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(MUTED))
        .title(Span::styled(
            " Password generator ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
    f.render_widget(block, area);
    let inner = area.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });

    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(inner);

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            g.output.clone(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(MUTED))
                .title(" output "),
        ),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("length   ", Style::default().fg(MUTED)),
            Span::styled(
                g.length.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ])),
        chunks[2],
    );
    f.render_widget(Paragraph::new(toggle_line("symbols", g.symbols)), chunks[3]);
    f.render_widget(Paragraph::new(toggle_line("numbers", g.numbers)), chunks[4]);
}

fn toggle_line(label: &str, on: bool) -> Line<'_> {
    let (mark, style) = if on {
        (
            "on ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )
    } else {
        ("off", Style::default().fg(MUTED))
    };
    Line::from(vec![
        Span::styled(format!("{label}  "), Style::default().fg(MUTED)),
        Span::styled(mark.to_string(), style),
    ])
}

fn render_help_modal(f: &mut Frame, area: Rect) {
    let area = centered(64, 24, area);
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            " Help ",
            Style::default().add_modifier(Modifier::BOLD).fg(ACCENT),
        ));
    let lines = vec![
        Line::from("List"),
        Line::from("  ↵            open detail / toggle group"),
        Line::from("  j/k  ↑/↓     navigate one row"),
        Line::from("  PgUp/PgDn    navigate ten rows"),
        Line::from("  g/G  Home/End top / bottom"),
        Line::from("  →/←          expand / collapse group (← on item: jump to parent)"),
        Line::from("  Space/Tab    toggle group"),
        Line::from("  :            search"),
        Line::from("  n / e / d    new / edit / delete"),
        Line::from("  c / y        copy password / username"),
        Line::from("  L            lock vault"),
        Line::from("  q            quit"),
        Line::from(""),
        Line::from("Detail"),
        Line::from("  r            reveal password"),
        Line::from("  c / y / u    copy password / username / url"),
        Line::from("  gx           open url in default browser (vim convention)"),
        Line::from("  1..9         copy Nth link's URL"),
        Line::from("  g1..g9       open Nth link's URL"),
        Line::from("  e / d        edit / delete"),
        Line::from(""),
        Line::from("Form"),
        Line::from("  Tab/↓  ⇧Tab/↑   next / prev field"),
        Line::from("  Ctrl+S        save     Ctrl+R reveal     Ctrl+G generate"),
        Line::from("  Ctrl+L        manage links (multiple per item)"),
        Line::from(""),
        Line::from(Span::styled(
            "press ?, Esc, or q to dismiss",
            Style::default().fg(MUTED),
        )),
    ];
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_delete_modal(f: &mut Frame, area: Rect, app: &App, id: &str) {
    let title = app
        .vault
        .as_ref()
        .and_then(|v| v.find_item(id).ok().map(|i| i.title.clone()))
        .unwrap_or_else(|| "(unknown)".into());
    let area = centered(54, 7, area);
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red))
        .title(Span::styled(
            " Delete credential ",
            Style::default().add_modifier(Modifier::BOLD).fg(Color::Red),
        ));
    let lines = vec![
        Line::from(""),
        Line::from(format!("  delete \"{title}\" ?")),
        Line::from(""),
        Line::from(Span::styled(
            "  press y or Enter to confirm · any other key cancels",
            Style::default().fg(MUTED),
        )),
    ];
    f.render_widget(Paragraph::new(lines).block(block), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{DetailState, StatusKind};
    use crate::test_support::{
        item_id, link, locked_app, new_item, render_to_string, setup_app, unlocked_app,
    };
    use crate::vault::NewItem;

    const W: u16 = 120;
    const H: u16 = 40;

    fn render(app: &App) -> String {
        render_to_string(app, W, H)
    }

    fn set_setup(app: &mut App, f: impl FnOnce(&mut SetupState)) {
        if let Screen::Setup(s) = &mut app.screen {
            f(s);
        }
    }

    fn set_list(app: &mut App, f: impl FnOnce(&mut AppListState)) {
        if let Screen::List(l) = &mut app.screen {
            f(l);
        }
    }

    fn set_form(app: &mut App, f: impl FnOnce(&mut FormState)) {
        if let Screen::Form(s) = &mut app.screen {
            f(s);
        }
    }

    fn detail_for(item: NewItem, reveal: bool) -> String {
        let title = item.title.clone();
        let (_dir, mut app) = unlocked_app(vec![item]);
        let id = item_id(&app, &title);
        app.screen = Screen::Detail(DetailState {
            id,
            reveal,
            pending_g: false,
        });
        render(&app)
    }

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn setup_screen_masks_and_shows_error() {
        let (_dir, mut app) = setup_app();
        set_setup(&mut app, |s| {
            s.password = "abc".into();
            s.confirm = "abcd".into();
            s.error = Some("passwords do not match".into());
        });
        let out = render(&app);
        assert!(out.contains("Set vault password"));
        assert!(out.contains("•••"));
        assert!(!out.contains("abc"));
        assert!(out.contains("passwords do not match"));
        assert!(out.contains("locked"));
        assert!(out.contains("Tab next"));
        assert!(out.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))));
    }

    #[test]
    fn setup_screen_reveals_on_request() {
        let (_dir, mut app) = setup_app();
        set_setup(&mut app, |s| {
            s.password = "visible1".into();
            s.reveal = true;
            s.focus = SetupFocus::Confirm;
        });
        assert!(render(&app).contains("visible1"));
    }

    #[test]
    fn unlock_screen_renders_error() {
        let (_dir, mut app) = locked_app();
        if let Screen::Unlock(s) = &mut app.screen {
            s.password = "typed-in".into();
            s.error = Some("invalid master password".into());
        }
        let out = render(&app);
        assert!(out.contains("Unlock vault"));
        assert!(out.contains("invalid master password"));
        assert!(out.contains("Enter unlock"));
        assert!(!out.contains("typed-in"));
        if let Screen::Unlock(s) = &mut app.screen {
            s.reveal = true;
        }
        assert!(render(&app).contains("typed-in"));
    }

    #[test]
    fn list_empty_vault_message() {
        let (_dir, app) = unlocked_app(Vec::new());
        let out = render(&app);
        assert!(out.contains("vault is empty"));
        assert!(out.contains("press : to search"));
        assert!(out.contains("unlocked"));
    }

    #[test]
    fn list_no_matches_message_and_search_hint() {
        let (_dir, mut app) = unlocked_app(vec![new_item("Alpha", "")]);
        set_list(&mut app, |l| {
            l.search = "zzz".into();
            l.searching = true;
        });
        let out = render(&app);
        assert!(out.contains("no matches"));
        assert!(out.contains(":zzz"));
        assert!(out.contains("type to filter"));
    }

    #[test]
    fn list_renders_tree_rows() {
        let (_dir, mut app) = unlocked_app(vec![
            new_item("Mail", "work"),
            new_item("Deploy", "work/ci"),
            new_item("Solo", ""),
        ]);
        set_list(&mut app, |l| {
            l.collapsed.insert("work/ci".into());
            l.selected = 99;
        });
        let out = render(&app);
        assert!(out.contains("▾ work"));
        assert!(out.contains("(2)"));
        assert!(out.contains("▸ ci"));
        assert!(!out.contains("Deploy"));
        assert!(out.contains("Mail"));
        assert!(out.contains("mail-user"));
        assert!(out.contains("Solo"));
        assert!(out.contains("n new"));
    }

    #[test]
    fn list_truncates_long_titles() {
        let long = "x".repeat(200);
        let (_dir, app) = unlocked_app(vec![new_item(&long, "")]);
        let out = render(&app);
        assert!(out.contains('…'));
        assert!(!out.contains(&long));
    }

    #[test]
    fn delete_modal_shows_title_or_unknown() {
        let (_dir, mut app) = unlocked_app(vec![new_item("Alpha", "")]);
        let id = item_id(&app, "Alpha");
        set_list(&mut app, |l| l.confirm_delete_id = Some(id));
        let out = render(&app);
        assert!(out.contains("Delete credential"));
        assert!(out.contains("delete \"Alpha\" ?"));
        set_list(&mut app, |l| l.confirm_delete_id = Some("missing".into()));
        assert!(render(&app).contains("(unknown)"));
    }

    #[test]
    fn help_modal_overlays_screen() {
        let (_dir, mut app) = unlocked_app(Vec::new());
        app.show_help = true;
        let out = render(&app);
        assert!(out.contains("Help"));
        assert!(out.contains("open detail / toggle group"));
        assert!(out.contains("copy Nth link's URL"));
    }

    #[test]
    fn status_bar_shows_info_and_error() {
        let (_dir, mut app) = unlocked_app(Vec::new());
        app.set_status("saved", StatusKind::Info);
        assert!(render(&app).contains("saved"));
        app.set_status("broken", StatusKind::Error);
        assert!(render(&app).contains("broken"));
    }

    #[test]
    fn detail_missing_item() {
        let (_dir, mut app) = unlocked_app(Vec::new());
        app.open_detail("missing".into());
        assert!(render(&app).contains("(item not found)"));
    }

    #[test]
    fn detail_full_item_masked() {
        let mut item = new_item("Alpha", "work / ci");
        item.url = "https://example.com".into();
        item.notes = "line one\nline two".into();
        item.links = vec![
            link("docs", "manual", "https://example.com/docs"),
            link("", "", "https://example.com/x"),
        ];
        let out = detail_for(item, false);
        assert!(out.contains("Detail"));
        assert!(out.contains("Alpha"));
        assert!(out.contains("work › ci"));
        assert!(out.contains("created "));
        assert!(out.contains("alpha-user"));
        assert!(out.contains("[gx]"));
        assert!(!out.contains("Alpha-pw"));
        assert!(out.contains("line one"));
        assert!(out.contains("line two"));
        assert!(out.contains("[1] docs"));
        assert!(out.contains("manual"));
        assert!(out.contains("[2] (unnamed)"));
        assert!(out.contains("[g2]"));
        assert!(out.contains("gx open url"));
    }

    #[test]
    fn detail_reveals_password() {
        assert!(detail_for(new_item("Alpha", ""), true).contains("Alpha-pw"));
    }

    #[test]
    fn detail_links_without_notes_and_empty_password() {
        let mut item = new_item("Alpha", "");
        item.password = String::new();
        item.links = vec![link("only", "", "")];
        let out = detail_for(item, false);
        assert!(out.contains("Links"));
        assert!(out.contains("[1] only"));
        assert!(!out.contains("[u]"));
    }

    #[test]
    fn detail_row_truncates_long_values() {
        let value = "v".repeat(100);
        let line = detail_row("URL", &value, &["u", "gx"], Style::default(), 40);
        let text = line_text(&line);
        assert!(text.contains('…'));
        assert!(text.ends_with("[gx]"));
        let plain = detail_row("Empty", "", &["u"], Style::default(), 40);
        assert!(!line_text(&plain).contains('['));
    }

    fn form_app() -> (crate::test_support::TempDir, App) {
        let (dir, mut app) = unlocked_app(Vec::new());
        app.start_new();
        set_form(&mut app, |f| {
            f.title = "Alpha".into();
            f.password = "form-secret".into();
            f.notes = "first\nsecond".into();
        });
        (dir, app)
    }

    #[test]
    fn form_create_masks_password() {
        let (_dir, app) = form_app();
        let out = render(&app);
        assert!(out.contains("New credential"));
        assert!(out.contains("Alpha▌"));
        assert!(!out.contains("form-secret"));
        assert!(out.contains("first"));
        assert!(out.contains("second"));
        assert!(out.contains("[+] add link"));
        assert!(out.contains("Ctrl+S save"));
    }

    #[test]
    fn form_password_focus_shows_hint_and_reveal() {
        let (_dir, mut app) = form_app();
        set_form(&mut app, |f| {
            f.focus = FormFocus::Password;
            f.reveal = true;
            f.error = Some("title is required".into());
        });
        let out = render(&app);
        assert!(out.contains("form-secret▌"));
        assert!(out.contains("Ctrl+G generate"));
        assert!(out.contains("title is required"));
    }

    #[test]
    fn form_notes_focus_puts_cursor_on_last_line() {
        let (_dir, mut app) = form_app();
        set_form(&mut app, |f| f.focus = FormFocus::Notes);
        assert!(render(&app).contains("second▌"));
        set_form(&mut app, |f| f.notes.clear());
        assert!(render(&app).contains("Notes     ▌"));
    }

    #[test]
    fn form_add_link_focus_without_links() {
        let (_dir, mut app) = form_app();
        set_form(&mut app, |f| f.focus = FormFocus::AddLink);
        assert!(render(&app).contains("[+] add link▌"));
    }

    #[test]
    fn form_edit_with_links() {
        let mut item = new_item("Alpha", "");
        item.links = vec![
            link("docs", "manual", "https://example.com/docs"),
            link("wiki", "", ""),
        ];
        let (_dir, mut app) = unlocked_app(vec![item]);
        let id = item_id(&app, "Alpha");
        app.start_edit(&id).unwrap();
        set_form(&mut app, |f| f.focus = FormFocus::LinkDescription(0));
        let out = render(&app);
        assert!(out.contains("Edit credential"));
        assert!(out.contains("[1] Name"));
        assert!(out.contains("[2] Name"));
        assert!(out.contains("manual▌"));
        assert!(out.contains("https://example.com/docs"));
        set_form(&mut app, |f| f.focus = FormFocus::LinkUrl(1));
        assert!(render(&app).contains("URL         ▌"));
    }

    #[test]
    fn generator_screen() {
        let (_dir, mut app) = unlocked_app(Vec::new());
        let mut g = GenState::new();
        g.symbols = false;
        g.output = "generated-output".into();
        app.screen = Screen::Generator(g);
        let out = render(&app);
        assert!(out.contains("Password generator"));
        assert!(out.contains("generated-output"));
        assert!(out.contains("length   24"));
        assert!(out.contains("symbols  off"));
        assert!(out.contains("numbers  on"));
        assert!(out.contains("+/- length"));
    }

    #[test]
    fn tiny_terminal_does_not_panic() {
        let (_dir, mut app) = setup_app();
        assert_ne!(render_to_string(&app, 20, 6), "");
        app.show_help = true;
        assert_ne!(render_to_string(&app, 20, 6), "");
    }

    #[test]
    fn truncate_respects_display_width() {
        assert_eq!(truncate("short".into(), 10), "short");
        assert_eq!(truncate("abcdefgh".into(), 5), "abcd…");
        assert_eq!(truncate("한국어텍스트".into(), 5), "한국…");
        assert_eq!(truncate("abc".into(), 0), "…");
    }

    #[test]
    fn pad_to_width_handles_wide_chars() {
        assert_eq!(pad_to_width("ab", 4), "ab  ");
        assert_eq!(pad_to_width("한", 3), "한 ");
        assert_eq!(pad_to_width("abcdef", 3), "abcdef");
    }

    #[test]
    fn short_date_formats_rfc3339() {
        assert_eq!(short_date(""), "");
        assert_eq!(short_date("2024-01-02T03:04:05+00:00"), "2024-01-02 03:04");
    }

    #[test]
    fn centered_clamps_to_area() {
        let area = Rect::new(0, 0, 100, 50);
        assert_eq!(centered(20, 10, area), Rect::new(40, 20, 20, 10));
        assert_eq!(centered(200, 100, area), Rect::new(0, 0, 100, 50));
    }

    #[test]
    fn masked_and_toggle_lines() {
        assert_eq!(line_text(&masked("abc", false)), "•••");
        assert_eq!(line_text(&masked("abc", true)), "abc");
        assert_eq!(line_text(&toggle_line("x", true)), "x  on ");
        assert_eq!(line_text(&toggle_line("x", false)), "x  off");
    }
}

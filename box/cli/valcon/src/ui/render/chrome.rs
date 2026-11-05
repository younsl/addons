//! Header with breadcrumb and session facts, footer with key hints or the input line.

use std::fmt::Write;

use chrono::{DateTime, Local};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};

use crate::ui::nav::{InputKind, Screen, Ui};
use crate::ui::theme;

pub fn header(f: &mut Frame, area: Rect, ui: &Ui, now: DateTime<Local>) {
    let mut path = vec![Span::styled("valcon", theme::strong())];
    let crumbs = ui.crumbs();
    let last = crumbs.len().saturating_sub(1);
    for (i, crumb) in crumbs.into_iter().enumerate() {
        path.push(Span::styled(" / ", theme::subtle()));
        path.push(Span::styled(
            crumb,
            if i == last {
                theme::strong()
            } else {
                theme::muted()
            },
        ));
    }

    let env = &ui.env;
    let mut facts = vec![
        Span::styled("aws ", theme::subtle()),
        Span::styled(format!("{}@{}", env.profile, env.region), theme::muted()),
        Span::styled("   kube ", theme::subtle()),
        Span::styled(format!("{}/{}", env.context, env.scope), theme::muted()),
    ];
    if let Some(t) = &env.target {
        facts.push(Span::styled("   target ", theme::subtle()));
        facts.push(Span::styled(t.clone(), theme::muted()));
    }
    facts.push(Span::styled(
        format!("   {}", status(ui, now)),
        theme::subtle(),
    ));

    f.render_widget(
        Paragraph::new(vec![Line::from(path), Line::from(facts)])
            .block(Block::new().padding(Padding::new(2, 2, 1, 0))),
        area,
    );
}

fn status(ui: &Ui, now: DateTime<Local>) -> String {
    let s = &ui.state;
    let mut out = match (&s.latest, s.collecting_since) {
        (_, Some(started)) => format!("refreshing {}s", started.elapsed().as_secs()),
        (Some(snap), None) => {
            let elapsed = u64::try_from((now - snap.at).num_seconds()).unwrap_or(0);
            format!(
                "updated {}  took {:.1}s  next {}s",
                snap.at.format("%H:%M:%S"),
                snap.took.as_secs_f32(),
                ui.env.interval.as_secs().saturating_sub(elapsed)
            )
        }
        (None, None) => "waiting for first refresh".into(),
    };
    if let Some(snap) = &s.latest
        && !snap.errors.is_empty()
    {
        let _ = write!(out, "   {} errors (:events)", snap.errors.len());
    }
    out
}

const fn hints(screen: &Screen) -> &'static [(&'static str, &'static str)] {
    match screen {
        Screen::Groups => &[
            ("enter", "open"),
            ("/", "filter"),
            (":", "pods events"),
            ("r", "refresh"),
            ("q", "quit"),
        ],
        Screen::Group { .. } => &[
            ("enter", "open"),
            ("tab", "nodes clients pods"),
            ("/", "filter"),
            ("esc", "back"),
            ("q", "quit"),
        ],
        Screen::Node { .. } => &[
            ("enter", "browse db"),
            ("r", "reload"),
            ("esc", "back"),
            ("q", "quit"),
        ],
        Screen::Keys { .. } => &[
            ("enter", "value"),
            ("/", "match"),
            ("n p", "next prev page"),
            ("r", "reload"),
            ("esc", "back"),
        ],
        Screen::Value { .. } => &[("r", "reload"), ("esc", "back"), ("q", "quit")],
        Screen::Clients { .. } | Screen::Pods => &[
            ("enter", "pod"),
            ("/", "filter"),
            ("esc", "back"),
            ("q", "quit"),
        ],
        Screen::Pod { .. } => &[("esc", "back"), ("r", "refresh"), ("q", "quit")],
        Screen::Events => &[
            ("/", "filter"),
            ("esc", "back"),
            (":", "groups pods"),
            ("q", "quit"),
        ],
    }
}

pub fn footer(f: &mut Frame, area: Rect, ui: &Ui) {
    let line = match (&ui.input, &ui.flash) {
        (Some(input), _) => {
            let prefix = match input.kind {
                InputKind::Command => ":",
                InputKind::Filter => "/",
                InputKind::Pattern => "match ",
            };
            Line::from(vec![
                Span::styled(format!("  {prefix}"), theme::strong()),
                Span::styled(format!("{}_", input.text), theme::text()),
                Span::styled("    enter apply   esc cancel", theme::subtle()),
            ])
        }
        (None, Some(msg)) => Line::from(Span::styled(format!("  {msg}"), theme::strong())),
        (None, None) => {
            let mut spans = vec![Span::raw("  ")];
            for (key, desc) in hints(ui.screen()) {
                spans.push(Span::styled(*key, theme::muted()));
                spans.push(Span::styled(format!(" {desc}    "), theme::subtle()));
            }
            Line::from(spans)
        }
    };
    f.render_widget(Paragraph::new(line), area);
}

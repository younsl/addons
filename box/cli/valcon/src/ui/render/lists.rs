use chrono::{DateTime, Local};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, List, ListItem, Paragraph, Row, Table, Wrap};

use super::columns::{self, Column};
use super::{filter_note, num, placeholder, table, title};
use crate::ui::format::{ago, count, pct};
use crate::ui::nav::Ui;
use crate::ui::rows;
use crate::ui::theme;
use crate::ui::trend;

const GROUPS: [Column; 11] = [
    Column::fill("REPLICATION GROUP", 24, 10),
    Column::fixed("ENGINE", 6, 6),
    Column::fixed("NODE TYPE", 16, 4),
    Column::fixed("MODE", 7, 2),
    Column::fixed("NODES", 5, 5),
    Column::fixed("STATUS", 10, 8),
    Column::fixed("ITEMS", 7, 9),
    Column::fixed("CONNS", 6, 9),
    Column::fixed("CPU", 7, 7),
    Column::fixed("MEM", 7, 7),
    Column::fixed("PODS", 4, 8),
];

const GROUP_PODS: [Column; 6] = [
    Column::fixed("NAMESPACE", 14, 7),
    Column::fill("POD", 24, 10),
    Column::fixed("APP", 18, 5),
    Column::fixed("CONNS", 6, 9),
    Column::fixed("LAST SEEN", 9, 4),
    Column::fill("NODES", 20, 8),
];

const ALL_PODS: [Column; 6] = [
    Column::fixed("NAMESPACE", 14, 7),
    Column::fill("POD", 24, 10),
    Column::fixed("APP", 18, 5),
    Column::fixed("CONNS", 6, 9),
    Column::fixed("PROBE", 5, 3),
    Column::fill("GROUPS / ERROR", 24, 8),
];

pub fn groups(f: &mut Frame, area: Rect, ui: &mut Ui, now: DateTime<Local>) {
    if ui.state.latest.is_none() {
        placeholder(f, area, "replication groups", "waiting for first refresh");
        return;
    }
    let rows = rows::groups(&ui.state, ui.filter());
    let block = theme::panel_line(title(
        "replication groups",
        &format!("{}{}", rows.len(), filter_note(ui)),
    ));
    let fitted = columns::fit(&GROUPS, block.inner(area).width);
    let body = rows
        .iter()
        .map(|r| {
            let g = r.group;
            let s = r.stats;
            let pods = u32::try_from(s.pods).unwrap_or(u32::MAX);
            fitted.row(vec![
                Cell::from(g.id.clone()).style(theme::strong()),
                Cell::from(g.engine.clone()).style(theme::muted()),
                Cell::from(g.node_type.clone()).style(theme::muted()),
                Cell::from(if g.cluster_mode { "cluster" } else { "single" })
                    .style(theme::subtle()),
                Cell::from(g.nodes.len().to_string()).style(theme::muted()),
                Cell::from(g.status.clone()).style(theme::status(&g.status)),
                num(
                    ui,
                    &trend::group(&g.id, "items"),
                    count(s.items),
                    theme::text(),
                    now,
                ),
                num(
                    ui,
                    &trend::group(&g.id, "conns"),
                    count(s.connections),
                    theme::text(),
                    now,
                ),
                num(
                    ui,
                    &trend::group(&g.id, "cpu"),
                    pct(s.cpu),
                    theme::muted(),
                    now,
                ),
                num(
                    ui,
                    &trend::group(&g.id, "mem"),
                    pct(s.memory),
                    theme::muted(),
                    now,
                ),
                num(
                    ui,
                    &trend::group(&g.id, "pods"),
                    s.pods.to_string(),
                    theme::count(pods),
                    now,
                ),
            ])
        })
        .collect();
    f.render_stateful_widget(table(&fitted, body, block), area, ui.table_mut());
}

pub fn group_pods(
    f: &mut Frame,
    area: Rect,
    ui: &mut Ui,
    group: &str,
    app: Option<(&str, &str)>,
    now: DateTime<Local>,
) {
    let label = app.map_or_else(
        || group.to_owned(),
        |(ns, a)| format!("{ns}/{a} on {group}"),
    );
    let block = theme::panel_line(title(&label, &filter_note(ui)));
    pod_table(f, area, ui, group, app, block, now);
}

pub fn pod_table(
    f: &mut Frame,
    area: Rect,
    ui: &mut Ui,
    group: &str,
    app: Option<(&str, &str)>,
    block: Block<'_>,
    now: DateTime<Local>,
) {
    let fitted = columns::fit(&GROUP_PODS, block.inner(area).width);
    let body = rows::group_pods(&ui.state, group, app, ui.filter())
        .iter()
        .map(|r| {
            let links = r
                .links
                .iter()
                .map(|(n, c)| format!("{n}={c}"))
                .collect::<Vec<_>>()
                .join("  ");
            fitted.row(vec![
                Cell::from(r.pod.namespace.clone()).style(theme::muted()),
                Cell::from(r.pod.name.clone()).style(theme::text()),
                Cell::from(r.pod.app.clone()).style(theme::muted()),
                num(
                    ui,
                    &trend::group_pod(group, &r.pod.key()),
                    r.conns.to_string(),
                    theme::count(r.conns),
                    now,
                ),
                Cell::from(ago(r.last_seen, now)).style(theme::muted()),
                Cell::from(links).style(theme::muted()),
            ])
        })
        .collect();
    f.render_stateful_widget(table(&fitted, body, block), area, ui.table_mut());
}

pub fn all_pods(f: &mut Frame, area: Rect, ui: &mut Ui, now: DateTime<Local>) {
    let rows = rows::all_pods(&ui.state, ui.filter());
    let linked = rows.iter().filter(|r| r.conns > 0).count();
    let facts = format!(
        "{} scanned  {linked} connected{}",
        rows.len(),
        filter_note(ui)
    );
    let block = theme::panel_line(title("pods", &facts));
    let fitted = columns::fit(&ALL_PODS, block.inner(area).width);
    let body = rows
        .iter()
        .map(|r| {
            let (detail, style) = r.error.map_or_else(
                || {
                    let s = r
                        .links
                        .iter()
                        .map(|(g, c)| format!("{g}={c}"))
                        .collect::<Vec<_>>()
                        .join("  ");
                    (s, theme::muted())
                },
                |e| (e.to_owned(), theme::strong()),
            );
            fitted.row(vec![
                Cell::from(r.pod.namespace.clone()).style(theme::muted()),
                Cell::from(r.pod.name.clone()).style(theme::text()),
                Cell::from(r.pod.app.clone()).style(theme::muted()),
                num(
                    ui,
                    &trend::pod(&r.pod.key()),
                    r.conns.to_string(),
                    theme::count(r.conns),
                    now,
                ),
                Cell::from(if r.pod.probe { "yes" } else { "" }).style(theme::subtle()),
                Cell::from(detail).style(style),
            ])
        })
        .collect();
    f.render_stateful_widget(table(&fitted, body, block), area, ui.table_mut());
}

pub fn pod(f: &mut Frame, area: Rect, ui: &Ui, key: &str) {
    let Some(scan) = ui
        .state
        .latest
        .as_ref()
        .and_then(|s| s.pods.iter().find(|p| p.pod.key() == key))
    else {
        placeholder(
            f,
            area,
            key,
            "pod is no longer running (gone from the last refresh)",
        );
        return;
    };
    let [facts, nodes] =
        Layout::horizontal([Constraint::Length(48), Constraint::Min(30)]).areas(area);
    let row = |k: &str, v: String| {
        Line::from(vec![
            Span::styled(format!("{k:<10}"), theme::subtle()),
            Span::styled(v, theme::text()),
        ])
    };
    let mut lines = vec![
        row("namespace", scan.pod.namespace.clone()),
        row("app", scan.pod.app.clone()),
        row(
            "probe",
            if scan.pod.probe {
                "running".into()
            } else {
                "none".into()
            },
        ),
    ];
    if scan.pod.deleting {
        lines.push(row("phase", "terminating".into()));
    }
    if let Err(e) = &scan.result {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(e.clone(), theme::strong())));
    }
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(theme::panel(key)),
        facts,
    );

    let per_node = ui.state.topo.per_node(scan);
    let body: Vec<Row> = per_node
        .iter()
        .map(|(node, n)| {
            Row::new(vec![
                Cell::from(ui.state.topo.group_of(node).unwrap_or("-").to_owned())
                    .style(theme::muted()),
                Cell::from(node.clone()).style(theme::text()),
                Cell::from(n.to_string()).style(theme::count(*n)),
            ])
        })
        .collect();
    f.render_widget(
        Table::new(
            body,
            [
                Constraint::Min(20),
                Constraint::Min(22),
                Constraint::Length(6),
            ],
        )
        .header(Row::new(["GROUP", "NODE", "CONNS"]).style(theme::header_row()))
        .column_spacing(columns::SPACING)
        .block(theme::panel("connections")),
        nodes,
    );
}

pub fn events(f: &mut Frame, area: Rect, ui: &Ui) {
    let events = rows::events(&ui.state, ui.filter());
    let items: Vec<ListItem> = events
        .iter()
        .map(|e| {
            ListItem::new(Line::from(vec![
                Span::styled(e.at.format("%H:%M:%S  ").to_string(), theme::subtle()),
                Span::styled(e.text.clone(), theme::level(e.level)),
            ]))
        })
        .collect();
    let block = theme::panel_line(title(
        "events",
        &format!("{}{}", events.len(), filter_note(ui)),
    ));
    f.render_widget(List::new(items).block(block), area);
}

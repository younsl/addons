use chrono::{DateTime, Local};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Sparkline};

use super::columns::{self, Column};
use super::{filter_note, lists, num, placeholder, table, title};
use crate::state::Series;
use crate::ui::format::{ago, count, pct};
use crate::ui::nav::{GroupTab, Ui};
use crate::ui::rows;
use crate::ui::theme;
use crate::ui::trend;

const NODES: [Column; 10] = [
    Column::fill("NODE", 22, 10),
    Column::fixed("ROLE", 7, 9),
    Column::fixed("SHARD", 5, 3),
    Column::fixed("AZ", 15, 2),
    Column::fixed("IP", 15, 4),
    Column::fixed("ITEMS", 7, 8),
    Column::fixed("CONNS", 6, 8),
    Column::fixed("CPU", 7, 7),
    Column::fixed("MEM", 7, 7),
    Column::fixed("POD CONNS", 9, 6),
];

const CLIENTS: [Column; 5] = [
    Column::fixed("NAMESPACE", 16, 8),
    Column::fill("APP", 20, 10),
    Column::fixed("PODS", 4, 7),
    Column::fixed("CONNS", 6, 9),
    Column::fixed("LAST SEEN", 9, 5),
];

pub fn draw(f: &mut Frame, area: Rect, ui: &mut Ui, id: &str, tab: GroupTab, now: DateTime<Local>) {
    let Some(group) = ui.state.latest.as_ref().and_then(|s| s.group(id)).cloned() else {
        placeholder(f, area, id, "group not in the last refresh");
        return;
    };
    let [summary, body] = Layout::vertical([Constraint::Length(8), Constraint::Min(5)]).areas(area);
    let [facts, conns, items] = Layout::horizontal([
        Constraint::Length(46),
        Constraint::Min(20),
        Constraint::Min(20),
    ])
    .areas(summary);

    let stats = ui
        .state
        .latest
        .as_ref()
        .map_or_default(|s| s.stats(&group, &ui.state.topo));
    let fact = |k: &str, v: String| {
        Line::from(vec![
            Span::styled(format!("{k:<9}"), theme::subtle()),
            Span::styled(v, theme::text()),
        ])
    };
    let lines = vec![
        fact("engine", format!("{}  {}", group.engine, group.node_type)),
        fact(
            "layout",
            format!(
                "{}  {} shards  {} nodes",
                if group.cluster_mode {
                    "cluster"
                } else {
                    "single"
                },
                group.shard_count(),
                group.nodes.len()
            ),
        ),
        Line::from(vec![
            Span::styled(format!("{:<9}", "status"), theme::subtle()),
            Span::styled(group.status.clone(), theme::status(&group.status)),
        ]),
        fact(
            "load",
            format!("cpu {}  mem {}", pct(stats.cpu), pct(stats.memory)),
        ),
        fact(
            "pods",
            format!("{} pods  {} conns", stats.pods, stats.pod_conns),
        ),
        fact("about", group.description.clone()),
    ];
    f.render_widget(Paragraph::new(lines).block(theme::panel(&group.id)), facts);

    let empty = Series::new(1);
    let series = ui.state.series.get(id);
    spark(
        f,
        conns,
        "connections",
        series.map_or(&empty, |s| &s.connections),
        stats.connections,
    );
    spark(
        f,
        items,
        "items",
        series.map_or(&empty, |s| &s.items),
        stats.items,
    );

    let tabs: Vec<Span> = GroupTab::ALL
        .iter()
        .flat_map(|t| {
            let style = if *t == tab {
                theme::strong().underlined()
            } else {
                theme::subtle()
            };
            [Span::styled(t.label(), style), Span::raw("  ")]
        })
        .collect();
    let mut head = vec![Span::raw(" ")];
    head.extend(tabs);
    head.push(Span::styled(filter_note(ui), theme::subtle()));
    let block = theme::panel_line(Line::from(head));

    match tab {
        GroupTab::Nodes => nodes(f, body, ui, id, block, now),
        GroupTab::Clients => clients(f, body, ui, id, block, now),
        GroupTab::Pods => lists::pod_table(f, body, ui, id, None, block, now),
    }
}

fn nodes(f: &mut Frame, area: Rect, ui: &mut Ui, id: &str, block: Block<'_>, now: DateTime<Local>) {
    let fitted = columns::fit(&NODES, block.inner(area).width);
    let rows = rows::nodes(&ui.state, id, ui.filter())
        .iter()
        .map(|r| {
            let (n, m) = (r.node.id.as_str(), r.metrics);
            fitted.row(vec![
                Cell::from(r.node.id.clone()).style(theme::text()),
                Cell::from(r.node.role.label()).style(theme::role(r.node.role)),
                Cell::from(r.node.shard.clone()).style(theme::muted()),
                Cell::from(r.node.az.clone()).style(theme::muted()),
                Cell::from(r.ip.map_or_else(|| "-".into(), |ip| ip.to_string()))
                    .style(theme::muted()),
                num(
                    ui,
                    &trend::node(n, "items"),
                    count(m.items),
                    theme::text(),
                    now,
                ),
                num(
                    ui,
                    &trend::node(n, "conns"),
                    count(m.connections),
                    theme::text(),
                    now,
                ),
                num(ui, &trend::node(n, "cpu"), pct(m.cpu), theme::muted(), now),
                num(
                    ui,
                    &trend::node(n, "mem"),
                    pct(m.memory),
                    theme::muted(),
                    now,
                ),
                num(
                    ui,
                    &trend::node(n, "pod-conns"),
                    r.pod_conns.to_string(),
                    theme::count(r.pod_conns),
                    now,
                ),
            ])
        })
        .collect();
    f.render_stateful_widget(table(&fitted, rows, block), area, ui.table_mut());
}

fn clients(
    f: &mut Frame,
    area: Rect,
    ui: &mut Ui,
    id: &str,
    block: Block<'_>,
    now: DateTime<Local>,
) {
    let fitted = columns::fit(&CLIENTS, block.inner(area).width);
    let rows = rows::clients(&ui.state, id, ui.filter())
        .iter()
        .map(|r| {
            fitted.row(vec![
                Cell::from(r.namespace.clone()).style(theme::muted()),
                Cell::from(r.app.clone()).style(theme::text()),
                num(
                    ui,
                    &trend::client(id, &r.namespace, &r.app, "pods"),
                    r.pods.to_string(),
                    theme::muted(),
                    now,
                ),
                num(
                    ui,
                    &trend::client(id, &r.namespace, &r.app, "conns"),
                    r.conns.to_string(),
                    theme::count(r.conns),
                    now,
                ),
                Cell::from(ago(r.last_seen, now)).style(theme::muted()),
            ])
        })
        .collect();
    f.render_stateful_widget(table(&fitted, rows, block), area, ui.table_mut());
}

fn spark(f: &mut Frame, area: Rect, label: &str, series: &Series, current: Option<f64>) {
    let width = usize::from(area.width.saturating_sub(4));
    f.render_widget(
        Sparkline::default()
            .data(series.tail(width))
            .max(series.max().max(1))
            .style(theme::text())
            .block(theme::panel_line(title(label, &count(current)))),
        area,
    );
}

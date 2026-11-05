use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, Wrap};

use super::columns::{self, Column};
use super::{placeholder, table, title};
use crate::db::model::printable;
use crate::ui::format::{bytes, pct, ttl};
use crate::ui::nav::{Load, Ui};
use crate::ui::rows;
use crate::ui::theme;

const KEYSPACE: [Column; 4] = [
    Column::fixed("DB", 4, 10),
    Column::fixed("KEYS", 10, 9),
    Column::fixed("EXPIRES", 10, 8),
    Column::fill("AVG TTL", 10, 5),
];

const KEYS: [Column; 4] = [
    Column::fill("KEY", 30, 10),
    Column::fixed("TYPE", 6, 8),
    Column::fixed("TTL", 8, 7),
    Column::fixed("SIZE", 7, 6),
];

fn loading<T>(f: &mut Frame, area: Rect, label: &str, load: &Load<T>) -> bool {
    match load {
        Load::Ready(_) => false,
        Load::Failed(e) => {
            placeholder(f, area, label, &format!("{e}\n\nr to retry"));
            true
        }
        Load::Idle | Load::Loading => {
            placeholder(
                f,
                area,
                label,
                "querying through a probe container in a connected pod",
            );
            true
        }
    }
}

fn via(ui: &Ui) -> String {
    if ui.db.via.is_empty() {
        String::new()
    } else {
        format!("via {}", ui.db.via)
    }
}

pub fn node(f: &mut Frame, area: Rect, ui: &mut Ui, node: &str) {
    if loading(f, area, node, &ui.db.info) {
        return;
    }
    let Load::Ready(info) = &ui.db.info else {
        return;
    };
    let [facts, keyspace] =
        Layout::horizontal([Constraint::Length(44), Constraint::Min(30)]).areas(area);
    let row = |k: &str, v: String| {
        Line::from(vec![
            Span::styled(format!("{k:<10}"), theme::subtle()),
            Span::styled(v, theme::text()),
        ])
    };
    let lines = vec![
        row("version", info.version.clone()),
        row("role", info.role.clone()),
        row("uptime", format!("{} days", info.uptime_days)),
        row(
            "memory",
            format!("{} of {}", info.used_memory, info.max_memory),
        ),
        row("clients", info.clients.clone()),
        row("ops/s", info.ops_per_sec.clone()),
        row("hit rate", pct(info.hit_rate)),
        row("evicted", info.evicted.clone()),
        row("expired", info.expired.clone()),
        Line::raw(""),
        Line::from(Span::styled(via(ui), theme::subtle())),
    ];
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(theme::panel(node)),
        facts,
    );

    let block = theme::panel_line(title("keyspace", "enter to browse keys"));
    let fitted = columns::fit(&KEYSPACE, block.inner(keyspace).width);
    let body = rows::keyspace(ui)
        .iter()
        .map(|d| {
            fitted.row(vec![
                Cell::from(format!("db{}", d.db)).style(theme::strong()),
                Cell::from(d.keys.to_string()).style(theme::text()),
                Cell::from(d.expires.to_string()).style(theme::muted()),
                Cell::from(ttl(i64::try_from(d.avg_ttl_ms).unwrap_or(i64::MAX)))
                    .style(theme::muted()),
            ])
        })
        .collect();
    f.render_stateful_widget(table(&fitted, body, block), keyspace, ui.table_mut());
}

pub fn keys(f: &mut Frame, area: Rect, ui: &mut Ui, db: u32) {
    let label = format!("db{db}");
    if loading(f, area, &label, &ui.db.keys) {
        return;
    }
    let Load::Ready(page) = &ui.db.keys else {
        return;
    };
    let pattern = if ui.db.pattern.is_empty() {
        "*"
    } else {
        ui.db.pattern.as_str()
    };
    let more = if page.cursor == "0" {
        "end"
    } else {
        "more (n)"
    };
    let facts = format!(
        "match {pattern}  page {}  {}  {more}  {}",
        ui.db.cursors.len(),
        page.keys.len(),
        via(ui)
    );
    let block = theme::panel_line(title(&label, &facts));
    let fitted = columns::fit(&KEYS, block.inner(area).width);
    let body = rows::keys(ui)
        .iter()
        .map(|k| {
            fitted.row(vec![
                Cell::from(rows::key_label(k)).style(theme::text()),
                Cell::from(k.kind.clone()).style(theme::muted()),
                Cell::from(ttl(k.ttl_ms)).style(theme::muted()),
                Cell::from(bytes(k.size)).style(theme::muted()),
            ])
        })
        .collect();
    f.render_stateful_widget(table(&fitted, body, block), area, ui.table_mut());
}

pub fn value(f: &mut Frame, area: Rect, ui: &Ui) {
    if loading(f, area, "value", &ui.db.value) {
        return;
    }
    let Load::Ready(v) = &ui.db.value else {
        return;
    };
    let shown = v.rows.len();
    let len = v.len.map_or_else(|| "-".into(), |l| l.to_string());
    let note = if v.truncated {
        format!("first {shown} of {len}")
    } else {
        format!("{len} total")
    };
    let facts = format!("{}  {note}  read only  {}", v.kind, via(ui));
    let block = theme::panel_line(title(&printable(&v.key), &facts));
    let labelled = v.rows.iter().any(|(l, _)| !l.is_empty());
    let body: Vec<Row> = v
        .rows
        .iter()
        .map(|(l, val)| {
            if labelled {
                Row::new(vec![
                    Cell::from(l.clone()).style(theme::muted()),
                    Cell::from(val.clone()).style(theme::text()),
                ])
            } else {
                Row::new(vec![Cell::from(val.clone()).style(theme::text())])
            }
        })
        .collect();
    let widths = if labelled {
        vec![Constraint::Length(24), Constraint::Min(20)]
    } else {
        vec![Constraint::Min(20)]
    };
    f.render_widget(
        Table::new(body, widths)
            .column_spacing(columns::SPACING)
            .block(block),
        area,
    );
}

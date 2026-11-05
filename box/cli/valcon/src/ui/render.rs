mod chrome;
mod columns;
mod db;
mod group;
mod lists;
#[cfg(test)]
mod screenshots;

use chrono::{DateTime, Local};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Table};

use super::nav::{Screen, Ui};
use super::theme;
use columns::Fitted;

pub fn draw(f: &mut Frame, ui: &mut Ui) {
    let now = Local::now();
    f.render_widget(Block::new().style(theme::base()), f.area());
    let [head, body, foot] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(6),
        Constraint::Length(1),
    ])
    .areas(f.area());

    chrome::header(f, head, ui, now);
    chrome::footer(f, foot, ui);
    match ui.screen().clone() {
        Screen::Groups => lists::groups(f, body, ui, now),
        Screen::Group { id, tab } => group::draw(f, body, ui, &id, tab, now),
        Screen::Clients {
            group,
            namespace,
            app,
        } => lists::group_pods(f, body, ui, &group, Some((&namespace, &app)), now),
        Screen::Pods => lists::all_pods(f, body, ui, now),
        Screen::Pod { key } => lists::pod(f, body, ui, &key),
        Screen::Events => lists::events(f, body, ui),
        Screen::Node { node, .. } => db::node(f, body, ui, &node),
        Screen::Keys { db, .. } => db::keys(f, body, ui, db),
        Screen::Value { .. } => db::value(f, body, ui),
    }
}

/// Panel title: label, then muted facts.
fn title(label: &str, facts: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw(" "),
        Span::styled(label.to_owned(), theme::strong()),
        Span::styled(format!("   {facts} "), theme::subtle()),
    ])
}

fn filter_note(ui: &Ui) -> String {
    if ui.filter().is_empty() {
        String::new()
    } else {
        format!("  /{}", ui.filter())
    }
}

/// A numeric cell with the arrow of its last refresh change on the right.
fn num(ui: &Ui, key: &str, shown: String, style: Style, now: DateTime<Local>) -> Cell<'static> {
    let mut spans = vec![Span::styled(shown, style)];
    if let Some(d) = ui.trends.at(key, now) {
        spans.push(Span::styled(d.arrow(), theme::strong()));
    }
    Cell::from(Line::from(spans))
}

fn placeholder(f: &mut Frame, area: Rect, label: &str, message: &str) {
    f.render_widget(
        Paragraph::new(Span::styled(message.to_owned(), theme::muted())).block(theme::panel(label)),
        area,
    );
}

fn table<'a>(fitted: &Fitted, rows: Vec<ratatui::widgets::Row<'a>>, block: Block<'a>) -> Table<'a> {
    Table::new(rows, fitted.constraints())
        .header(fitted.header().style(theme::header_row()))
        .column_spacing(columns::SPACING)
        .block(block)
        .row_highlight_style(theme::selected_row())
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    use super::*;
    use crate::collector::Msg;
    use crate::db::DbReply;
    use crate::db::model::KeyValue;
    use crate::ui::nav::tests::{info, page, press, type_text, ui};
    use crate::ui::nav::{Command, GroupTab};

    fn render_at(ui: &mut Ui, width: u16) -> (String, Buffer) {
        let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
        terminal.draw(|f| draw(f, ui)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let w = usize::from(buf.area.width);
        let text = buf
            .content()
            .chunks(w)
            .map(|row| {
                row.iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        (text, buf)
    }

    fn render(ui: &mut Ui) -> String {
        render_at(ui, 170).0
    }

    fn seq(cmd: &Command) -> u64 {
        match cmd {
            Command::Db { seq, .. } => *seq,
            other => panic!("{other:?}"),
        }
    }

    fn assert_monochrome(ui: &mut Ui) {
        let (_, buf) = render_at(ui, 170);
        for cell in buf.content() {
            assert!(
                theme::is_monochrome(cell.fg),
                "fg {:?} at {:?}",
                cell.fg,
                cell.symbol()
            );
            assert!(
                theme::is_monochrome(cell.bg),
                "bg {:?} at {:?}",
                cell.bg,
                cell.symbol()
            );
        }
    }

    #[test]
    fn refresh_marks_changed_values() {
        let mut ui = ui();
        assert!(!render(&mut ui).contains('↑'));
        let mut next = crate::types::tests::snapshot();
        for m in next.metrics.values_mut() {
            m.connections = m.connections.map(|c| c + 100.0);
            m.cpu = m.cpu.map(|c| c - 1.0);
        }
        ui.on_msg(Msg::Snapshot(Box::new(next)));
        let text = render(&mut ui);
        assert!(text.contains('↑'), "{text}");
        assert!(text.contains('↓'), "{text}");
        assert_monochrome(&mut ui);
    }

    #[test]
    fn walks_every_screen() {
        let mut ui = ui();
        ui.on_msg(Msg::RoundStarted);
        let groups = render(&mut ui);
        assert!(groups.contains("REPLICATION GROUP") && groups.contains("orders"));
        assert!(groups.contains("1000"), "{groups}");
        assert!(groups.contains("prod@us-east-1"));
        assert!(groups.contains("refreshing"));
        assert_monochrome(&mut ui);

        press(&mut ui, KeyCode::Char('j'));
        press(&mut ui, KeyCode::Enter);
        for tab in GroupTab::ALL {
            let text = render(&mut ui);
            assert!(text.contains(tab.label()), "{text}");
            assert!(text.contains("connections"));
            assert_monochrome(&mut ui);
            press(&mut ui, KeyCode::Tab);
        }
        press(&mut ui, KeyCode::Char('/'));
        type_text(&mut ui, "zzz");
        press(&mut ui, KeyCode::Enter);
        assert!(render(&mut ui).contains("/zzz"));
    }

    #[test]
    fn group_tabs_render() {
        let mut ui = ui();
        press(&mut ui, KeyCode::Char('j'));
        press(&mut ui, KeyCode::Enter);
        let nodes = render(&mut ui);
        assert!(
            nodes.contains("orders-001") && nodes.contains("primary") && nodes.contains("10.0.0.1")
        );
        assert!(nodes.contains("valkey") && nodes.contains("cache.m7g.large"));
        press(&mut ui, KeyCode::Tab);
        let clients = render(&mut ui);
        assert!(clients.contains("shop") && clients.contains("ledger"));
        press(&mut ui, KeyCode::Enter);
        let app = render(&mut ui);
        assert!(app.contains("api-1") && app.contains("orders-001=3"));
        assert_monochrome(&mut ui);
        press(&mut ui, KeyCode::Enter);
        let pod = render(&mut ui);
        assert!(pod.contains("shop/api-1") && pod.contains("orders-002"));
        assert_monochrome(&mut ui);
    }

    #[test]
    fn db_screens_render() {
        let mut ui = ui();
        press(&mut ui, KeyCode::Char('j'));
        press(&mut ui, KeyCode::Enter);
        let cmd = press(&mut ui, KeyCode::Enter);
        assert!(render(&mut ui).contains("querying"));
        ui.on_db(seq(&cmd), Ok((DbReply::Info(info()), "shop/api-1".into())));
        let node = render(&mut ui);
        assert!(node.contains("7.2.6") && node.contains("db3") && node.contains("via shop/api-1"));
        assert_monochrome(&mut ui);

        let cmd = press(&mut ui, KeyCode::Enter);
        ui.on_db(
            seq(&cmd),
            Ok((DbReply::Keys(page("42")), "shop/api-1".into())),
        );
        let keys = render(&mut ui);
        assert!(
            keys.contains("user:1")
                && keys.contains("hash")
                && keys.contains("1m30s")
                && keys.contains("more")
        );
        assert_monochrome(&mut ui);

        let cmd = press(&mut ui, KeyCode::Enter);
        let value = KeyValue {
            key: b"user:1".to_vec(),
            kind: "hash".into(),
            len: Some(500),
            rows: vec![("name".into(), "kim".into())],
            truncated: true,
        };
        ui.on_db(seq(&cmd), Ok((DbReply::Value(value), "shop/api-1".into())));
        let v = render(&mut ui);
        assert!(v.contains("name") && v.contains("kim") && v.contains("first 1 of 500"));
        assert_monochrome(&mut ui);
        let cmd = press(&mut ui, KeyCode::Char('r'));
        ui.on_db(seq(&cmd), Err("every gateway pod failed".into()));
        assert!(render(&mut ui).contains("every gateway pod failed"));
        press(&mut ui, KeyCode::Esc);
        press(&mut ui, KeyCode::Char('/'));
        type_text(&mut ui, "user:*");
        assert!(render(&mut ui).contains("match user:*"));
    }

    #[test]
    fn pods_events_and_input_render() {
        let mut ui = ui();
        press(&mut ui, KeyCode::Char(':'));
        type_text(&mut ui, "po");
        assert!(render(&mut ui).contains(":po"));
        press(&mut ui, KeyCode::Enter);
        let pods = render(&mut ui);
        assert!(
            pods.contains("batch-1")
                && pods.contains("cat: not found")
                && pods.contains("orders=5")
        );
        assert_monochrome(&mut ui);
        press(&mut ui, KeyCode::Char('/'));
        type_text(&mut ui, "ledger");
        assert!(render(&mut ui).contains("/ledger"));
        press(&mut ui, KeyCode::Enter);
        press(&mut ui, KeyCode::Char(':'));
        type_text(&mut ui, "events");
        press(&mut ui, KeyCode::Enter);
        assert!(render(&mut ui).contains("started"));
        press(&mut ui, KeyCode::Char(':'));
        type_text(&mut ui, "zzz");
        press(&mut ui, KeyCode::Enter);
        assert!(render(&mut ui).contains("unknown command"));
        assert_monochrome(&mut ui);
    }

    #[test]
    fn empty_and_missing_states_render() {
        let mut ui = Ui::new(crate::ui::nav::tests::env());
        assert!(render(&mut ui).contains("waiting for first refresh"));
        let mut ui = crate::ui::nav::tests::ui();
        press(&mut ui, KeyCode::Char(':'));
        type_text(&mut ui, "po");
        press(&mut ui, KeyCode::Enter);
        press(&mut ui, KeyCode::Char('/'));
        type_text(&mut ui, "batch");
        press(&mut ui, KeyCode::Enter);
        press(&mut ui, KeyCode::Enter);
        assert!(render(&mut ui).contains("cat: not found"));
        let (narrow, _) = render_at(&mut ui, 90);
        assert!(narrow.contains("batch"));
    }
}

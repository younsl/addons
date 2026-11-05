//! Renders every screen from fixture data into `docs/assets/screenshots/*.svg`.
//! Run with `make screenshots`. Fixture values only, never a real environment.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Local;
use crossterm::event::KeyCode;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};

use super::draw;
use crate::collector::Msg;
use crate::db::model::{KeyInfo, KeyPage, KeyValue};
use crate::db::{DbReply, info::node_info};
use crate::types::tests::{group, pod, scan};
use crate::types::{CacheGroup, NodeMetrics, PodScan, Snapshot};
use crate::ui::nav::tests::{env, press, type_text};
use crate::ui::nav::{Command, Ui};

const WIDTH: u16 = 150;
const HEIGHT: u16 = 34;
const CELL_W: f64 = 8.4;
const CELL_H: f64 = 18.0;
const GATEWAY: &str = "shop/api-7d9f8-abcde";

fn sharded(id: &str) -> CacheGroup {
    use crate::types::Role;
    use crate::types::tests::node;
    let mut g = group(id);
    g.cluster_mode = true;
    g.node_type = "cache.r7g.xlarge".into();
    g.nodes = vec![
        node(&format!("{id}-0001-001"), "0001", Role::Primary),
        node(&format!("{id}-0001-002"), "0001", Role::Replica),
        node(&format!("{id}-0002-001"), "0002", Role::Primary),
        node(&format!("{id}-0002-002"), "0002", Role::Replica),
    ];
    for (i, n) in g.nodes.iter_mut().enumerate() {
        if i % 2 == 1 {
            n.az = "us-east-1b".into();
        }
    }
    g
}

fn snapshot(round: u32, batch_connected: bool) -> Snapshot {
    let wave = f64::from(round % 6);
    let metrics = |items: f64, conns: f64, cpu: f64| NodeMetrics {
        items: Some(wave.mul_add(items * 0.01, items)),
        connections: Some(wave.mul_add(3.0, conns)),
        cpu: Some(cpu + wave),
        memory: Some(cpu * 2.5),
    };
    let mut pods = vec![
        scan(
            "shop",
            "api-7d9f8-abcde",
            "api",
            &[("10.0.1.11", 4), ("10.0.2.12", 2), ("10.0.3.21", 1)],
        ),
        scan(
            "shop",
            "api-7d9f8-fghij",
            "api",
            &[("10.0.1.11", 3), ("10.0.2.12", 3)],
        ),
        scan(
            "shop",
            "cart-5c6b7-klmno",
            "cart",
            &[("10.0.3.21", 2), ("10.0.3.31", 2)],
        ),
        scan(
            "pay",
            "ledger-0",
            "ledger",
            &[("10.0.1.51", 5), ("10.0.1.11", 1)],
        ),
        scan("pay", "ledger-1", "ledger", &[("10.0.1.51", 4)]),
        PodScan {
            pod: pod("pay", "audit-6f7d8-pqrst", "audit"),
            result: Err("cat: not found".into()),
        },
    ];
    pods[0].pod.probe = true;
    if batch_connected {
        pods.push(scan(
            "pay",
            "settle-29384756-uvwxy",
            "settle",
            &[("10.0.1.51", 1)],
        ));
    }
    let mut sessions = sharded("sessions");
    if (6..8).contains(&round) {
        sessions.status = "modifying".into();
    }
    Snapshot {
        at: Local::now() - chrono::Duration::seconds(i64::from((11 - round) * 15)),
        took: Duration::from_millis(1400),
        groups: vec![group("billing"), group("orders"), sessions],
        ips: HashMap::from([
            ("orders-001".into(), "10.0.1.11".parse().unwrap()),
            ("orders-002".into(), "10.0.2.12".parse().unwrap()),
            ("billing-001".into(), "10.0.1.51".parse().unwrap()),
            ("billing-002".into(), "10.0.2.52".parse().unwrap()),
            ("sessions-0001-001".into(), "10.0.3.21".parse().unwrap()),
            ("sessions-0001-002".into(), "10.0.3.22".parse().unwrap()),
            ("sessions-0002-001".into(), "10.0.3.31".parse().unwrap()),
            ("sessions-0002-002".into(), "10.0.3.32".parse().unwrap()),
        ]),
        metrics: HashMap::from([
            ("orders-001".into(), metrics(182_400.0, 46.0, 14.0)),
            ("orders-002".into(), metrics(182_400.0, 31.0, 9.0)),
            ("billing-001".into(), metrics(9_820.0, 18.0, 4.0)),
            ("billing-002".into(), metrics(9_820.0, 6.0, 2.0)),
            ("sessions-0001-001".into(), metrics(1_204_000.0, 88.0, 22.0)),
            ("sessions-0001-002".into(), metrics(1_204_000.0, 40.0, 11.0)),
            ("sessions-0002-001".into(), metrics(1_198_500.0, 84.0, 21.0)),
            ("sessions-0002-002".into(), metrics(1_198_500.0, 38.0, 10.0)),
        ]),
        pods,
        pods_listed: true,
        errors: vec![],
    }
}

fn demo() -> Ui {
    let mut ui = Ui::new(env());
    for round in 0..12 {
        ui.on_msg(Msg::RoundStarted);
        ui.on_msg(Msg::Snapshot(Box::new(snapshot(
            round,
            (4..9).contains(&round),
        ))));
    }
    ui
}

const INFO: &str = "valkey_version:8.1.4\r\nrole:master\r\nuptime_in_days:41\r\nused_memory_human:1.82G\r\nmaxmemory_human:10.50G\r\nconnected_clients:46\r\ninstantaneous_ops_per_sec:3120\r\nkeyspace_hits:982310\r\nkeyspace_misses:21877\r\nevicted_keys:0\r\nexpired_keys:51203\r\ndb0:keys=182211,expires=170034,avg_ttl=1795000\r\ndb1:keys=189,expires=0,avg_ttl=0\r\n";

fn keys() -> KeyPage {
    let key = |k: &str, kind: &str, ttl_ms: i64, size: i64| KeyInfo {
        key: k.as_bytes().to_vec(),
        kind: kind.into(),
        ttl_ms,
        size: Some(size),
    };
    KeyPage {
        cursor: "1536".into(),
        keys: vec![
            key("order:10001", "hash", 1_740_000, 312),
            key("order:10002", "hash", 1_512_000, 296),
            key("order:10003", "hash", -1, 344),
            key("cart:user:42", "list", 86_400_000, 1_024),
            key("rank:daily", "zset", 3_600_000, 8_192),
            key("session:9f2c", "string", 900_000, 184),
            key("stream:checkout", "stream", -1, 40_960),
            key("tags:order:10001", "set", -1, 220),
        ],
    }
}

fn value() -> KeyValue {
    let row = |k: &str, v: &str| (k.to_owned(), v.to_owned());
    KeyValue {
        key: b"order:10001".to_vec(),
        kind: "hash".into(),
        len: Some(7),
        rows: vec![
            row("id", "10001"),
            row("status", "PAID"),
            row("amount", "42000"),
            row("currency", "USD"),
            row("items", "3"),
            row("created_at", "2026-11-05T09:12:44Z"),
            row("updated_at", "2026-11-05T09:13:02Z"),
        ],
        truncated: false,
    }
}

fn answer(ui: &mut Ui, cmd: &Command, reply: DbReply) {
    let Command::Db { seq, .. } = cmd else {
        panic!("expected a db command, got {cmd:?}")
    };
    ui.on_db(*seq, Ok((reply, GATEWAY.into())));
}

fn rgb(c: Color, fallback: &str) -> String {
    match c {
        Color::Indexed(16) | Color::Black => "#000000".into(),
        Color::Indexed(i @ 232..=255) => {
            let v = 8 + 10 * (i - 232);
            format!("#{v:02x}{v:02x}{v:02x}")
        }
        Color::Indexed(231) | Color::White => "#ffffff".into(),
        Color::Reset => fallback.into(),
        other => panic!("non-monochrome color {other:?}"),
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn block_eighths(symbol: &str) -> Option<u8> {
    let c = symbol.chars().next()?;
    ('\u{2581}'..='\u{2588}')
        .contains(&c)
        .then(|| u8::try_from(u32::from(c) - 0x2580).unwrap_or(8))
}

fn col(i: usize) -> f64 {
    f64::from(u16::try_from(i).expect("terminal width fits u16"))
}

fn words(out: &mut String, cells: &[&str], left: f64, top: f64, fg: &str, bold: bool) {
    let weight = if bold { " font-weight=\"600\"" } else { "" };
    let mut i = 0;
    while i < cells.len() {
        let x = col(i).mul_add(CELL_W, left);
        if let Some(eighths) = block_eighths(cells[i]) {
            let h = CELL_H * f64::from(eighths) / 8.0;
            let _ = writeln!(
                out,
                "<rect x=\"{x}\" y=\"{}\" width=\"{CELL_W}\" height=\"{h}\" fill=\"{fg}\"/>",
                top + CELL_H - h
            );
            i += 1;
            continue;
        }
        if cells[i] == " " {
            i += 1;
            continue;
        }
        let start = i;
        while i < cells.len() && cells[i] != " " && block_eighths(cells[i]).is_none() {
            i += 1;
        }
        let text: String = cells[start..i].concat();
        let len = col(i - start) * CELL_W;
        let _ = writeln!(
            out,
            "<text x=\"{x}\" y=\"{:.1}\" fill=\"{fg}\"{weight} textLength=\"{len}\" lengthAdjust=\"spacingAndGlyphs\">{}</text>",
            CELL_H.mul_add(0.72, top),
            escape(&text)
        );
    }
}

fn svg(buf: &Buffer) -> String {
    let (w, h) = (buf.area.width, buf.area.height);
    let pad = 16.0;
    let (vw, vh) = (
        f64::from(w).mul_add(CELL_W, pad * 2.0),
        f64::from(h).mul_add(CELL_H, pad * 2.0),
    );
    let mut out = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {vw} {vh}\">\n<style>text{{font-family:\"Geist Mono\",ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;font-size:14px;white-space:pre}}</style>\n<rect width=\"{vw}\" height=\"{vh}\" rx=\"10\" fill=\"#000000\" stroke=\"#262626\"/>\n"
    );
    for y in 0..h {
        let top = f64::from(y).mul_add(CELL_H, pad);
        let mut x = 0;
        while x < w {
            let cell = &buf[(x, y)];
            let bg = rgb(cell.bg, "#000000");
            let fg = rgb(cell.fg, "#d0d0d0");
            let bold = cell.modifier.contains(Modifier::BOLD);
            let start = x;
            let mut cells = Vec::new();
            while x < w {
                let c = &buf[(x, y)];
                if rgb(c.bg, "#000000") != bg
                    || rgb(c.fg, "#d0d0d0") != fg
                    || c.modifier.contains(Modifier::BOLD) != bold
                {
                    break;
                }
                cells.push(c.symbol());
                x += 1;
            }
            let left = f64::from(start).mul_add(CELL_W, pad);
            if bg != "#000000" {
                let len = f64::from(x - start) * CELL_W;
                let _ = writeln!(
                    out,
                    "<rect x=\"{left}\" y=\"{top}\" width=\"{len}\" height=\"{CELL_H}\" fill=\"{bg}\"/>"
                );
            }
            words(&mut out, &cells, left, top, &fg, bold);
        }
    }
    out.push_str("</svg>\n");
    out
}

fn shoot(ui: &mut Ui, dir: &Path, name: &str) {
    let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
    terminal.draw(|f| draw(f, ui)).unwrap();
    std::fs::write(
        dir.join(format!("{name}.svg")),
        svg(terminal.backend().buffer()),
    )
    .unwrap();
}

#[test]
#[ignore = "writes docs/assets/screenshots, run with make screenshots"]
fn screenshots() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/assets/screenshots");
    std::fs::create_dir_all(&dir).unwrap();

    let mut ui = demo();
    shoot(&mut ui, &dir, "01-groups");

    press(&mut ui, KeyCode::Char('j'));
    press(&mut ui, KeyCode::Enter);
    shoot(&mut ui, &dir, "02-group-nodes");
    press(&mut ui, KeyCode::Tab);
    shoot(&mut ui, &dir, "03-group-clients");
    press(&mut ui, KeyCode::Enter);
    shoot(&mut ui, &dir, "04-app-pods");
    press(&mut ui, KeyCode::Enter);
    shoot(&mut ui, &dir, "05-pod");
    press(&mut ui, KeyCode::Esc);
    press(&mut ui, KeyCode::Esc);
    press(&mut ui, KeyCode::Tab);
    shoot(&mut ui, &dir, "06-group-pods");
    press(&mut ui, KeyCode::Tab);

    let cmd = press(&mut ui, KeyCode::Enter);
    answer(&mut ui, &cmd, DbReply::Info(node_info(INFO)));
    shoot(&mut ui, &dir, "07-node");
    let cmd = press(&mut ui, KeyCode::Enter);
    answer(&mut ui, &cmd, DbReply::Keys(keys()));
    shoot(&mut ui, &dir, "08-keys");
    let cmd = press(&mut ui, KeyCode::Enter);
    answer(&mut ui, &cmd, DbReply::Value(value()));
    shoot(&mut ui, &dir, "09-value");

    let mut ui = demo();
    press(&mut ui, KeyCode::Char(':'));
    type_text(&mut ui, "pods");
    press(&mut ui, KeyCode::Enter);
    shoot(&mut ui, &dir, "10-pods");
    press(&mut ui, KeyCode::Char(':'));
    type_text(&mut ui, "events");
    press(&mut ui, KeyCode::Enter);
    shoot(&mut ui, &dir, "11-events");
}

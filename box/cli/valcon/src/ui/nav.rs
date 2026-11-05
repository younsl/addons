//! Screen stack, input modes, and key handling. Pure: keys become `Command`s for the event
//! loop, so the whole flow is testable without a terminal.

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::TableState;

use super::backend::DbTarget;
use super::rows;
use super::trend::{self, Trends};
use crate::collector::Msg;
use crate::db::model::printable;
use crate::db::{DbReply, DbRequest, KeyPage, KeyValue, NodeInfo};
use crate::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupTab {
    Nodes,
    Clients,
    Pods,
}

impl GroupTab {
    pub const ALL: [Self; 3] = [Self::Nodes, Self::Clients, Self::Pods];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Nodes => "nodes",
            Self::Clients => "clients",
            Self::Pods => "pods",
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::Nodes => Self::Clients,
            Self::Clients => Self::Pods,
            Self::Pods => Self::Nodes,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    Groups,
    Group {
        id: String,
        tab: GroupTab,
    },
    Node {
        group: String,
        node: String,
    },
    Keys {
        group: String,
        node: String,
        db: u32,
    },
    Value {
        group: String,
        node: String,
        db: u32,
        key: Vec<u8>,
        kind: String,
    },
    Clients {
        group: String,
        namespace: String,
        app: String,
    },
    Pods,
    Pod {
        key: String,
    },
    Events,
}

impl Screen {
    pub fn crumb(&self) -> String {
        match self {
            Self::Groups => "groups".into(),
            Self::Group { id, tab } => format!("{id} ({})", tab.label()),
            Self::Node { node, .. } => node.clone(),
            Self::Keys { db, .. } => format!("db{db}"),
            Self::Value { key, .. } => printable(key),
            Self::Clients { namespace, app, .. } => format!("{namespace}/{app}"),
            Self::Pods => "pods".into(),
            Self::Pod { key } => key.clone(),
            Self::Events => "events".into(),
        }
    }

    /// Screens whose rows can be narrowed with `/`.
    const fn filterable(&self) -> bool {
        matches!(
            self,
            Self::Groups | Self::Group { .. } | Self::Clients { .. } | Self::Pods | Self::Events
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Load<T> {
    Idle,
    Loading,
    Ready(T),
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Command,
    Filter,
    Pattern,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    pub kind: InputKind,
    pub text: String,
}

/// Database browser state for the node, keys, and value screens.
#[derive(Debug, Clone, PartialEq)]
pub struct DbView {
    pub info: Load<NodeInfo>,
    pub keys: Load<KeyPage>,
    pub value: Load<KeyValue>,
    pub via: String,
    pub pattern: String,
    /// Cursors of the pages visited, the current page last.
    pub cursors: Vec<String>,
    seq: u64,
}

impl Default for DbView {
    fn default() -> Self {
        Self {
            info: Load::Idle,
            keys: Load::Idle,
            value: Load::Idle,
            via: String::new(),
            pattern: String::new(),
            cursors: vec!["0".into()],
            seq: 0,
        }
    }
}

/// Fixed facts about the session, shown in the header.
#[derive(Debug, Clone, Default)]
pub struct Env {
    pub target: Option<String>,
    pub context: String,
    pub scope: String,
    pub profile: String,
    pub region: String,
    pub interval: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    None,
    Quit,
    Refresh,
    Db {
        seq: u64,
        target: DbTarget,
        request: DbRequest,
    },
}

struct Level {
    screen: Screen,
    table: TableState,
    filter: String,
}

impl Level {
    fn new(screen: Screen) -> Self {
        Self {
            screen,
            table: TableState::default().with_selected(0),
            filter: String::new(),
        }
    }
}

pub struct Ui {
    pub env: Env,
    pub state: AppState,
    pub input: Option<Input>,
    pub db: DbView,
    pub flash: Option<String>,
    pub trends: Trends,
    levels: Vec<Level>,
}

impl Ui {
    pub fn new(env: Env) -> Self {
        Self {
            env,
            state: AppState::new(),
            input: None,
            db: DbView::default(),
            flash: None,
            trends: Trends::default(),
            levels: vec![Level::new(Screen::Groups)],
        }
    }

    pub fn screen(&self) -> &Screen {
        &self.level().screen
    }

    pub fn filter(&self) -> &str {
        &self.level().filter
    }

    pub fn table_mut(&mut self) -> &mut TableState {
        &mut self.level_mut().table
    }

    pub fn crumbs(&self) -> Vec<String> {
        self.levels.iter().map(|l| l.screen.crumb()).collect()
    }

    fn level(&self) -> &Level {
        self.levels.last().expect("root level is never popped")
    }

    fn level_mut(&mut self) -> &mut Level {
        self.levels.last_mut().expect("root level is never popped")
    }

    fn push(&mut self, screen: Screen) {
        self.levels.push(Level::new(screen));
    }

    pub fn on_msg(&mut self, msg: Msg) {
        let refreshed = matches!(msg, Msg::Snapshot(_));
        self.state.on_msg(msg);
        if refreshed {
            self.trends
                .observe(chrono::Local::now(), trend::values(&self.state));
        }
    }

    pub fn on_db(&mut self, seq: u64, result: Result<(DbReply, String), String>) {
        if seq != self.db.seq {
            return;
        }
        match result {
            Ok((reply, via)) => {
                self.db.via = via;
                match reply {
                    DbReply::Info(i) => self.db.info = Load::Ready(i),
                    DbReply::Keys(p) => self.db.keys = Load::Ready(p),
                    DbReply::Value(v) => self.db.value = Load::Ready(v),
                }
            }
            Err(e) => match self.screen() {
                Screen::Node { .. } => self.db.info = Load::Failed(e),
                Screen::Keys { .. } => self.db.keys = Load::Failed(e),
                _ => self.db.value = Load::Failed(e),
            },
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Command {
        self.flash = None;
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Command::Quit;
        }
        if self.input.is_some() {
            return self.edit_input(key.code);
        }
        match key.code {
            KeyCode::Char('q') => Command::Quit,
            KeyCode::Char(':') => self.open_input(InputKind::Command),
            KeyCode::Char('/') => match self.screen() {
                Screen::Keys { .. } => {
                    let text = self.db.pattern.clone();
                    self.input = Some(Input {
                        kind: InputKind::Pattern,
                        text,
                    });
                    Command::None
                }
                s if s.filterable() => self.open_input(InputKind::Filter),
                _ => Command::None,
            },
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => self.back(),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.drill(),
            KeyCode::Tab | KeyCode::BackTab => {
                self.next_tab();
                Command::None
            }
            KeyCode::Char('r') => self.reload(),
            KeyCode::Char('n') => self.page(true),
            KeyCode::Char('p') => self.page(false),
            code => {
                let len = rows::len(self);
                move_selection(self.table_mut(), code, len);
                Command::None
            }
        }
    }

    fn open_input(&mut self, kind: InputKind) -> Command {
        let text = if kind == InputKind::Filter {
            self.filter().to_owned()
        } else {
            String::new()
        };
        self.input = Some(Input { kind, text });
        Command::None
    }

    fn edit_input(&mut self, code: KeyCode) -> Command {
        let Some(input) = self.input.as_mut() else {
            return Command::None;
        };
        match code {
            KeyCode::Char(c) => input.text.push(c),
            KeyCode::Backspace => {
                input.text.pop();
            }
            KeyCode::Esc => {
                if input.kind == InputKind::Filter {
                    self.level_mut().filter.clear();
                }
                self.input = None;
                return Command::None;
            }
            KeyCode::Enter => {
                let Input { kind, text } = self.input.take().unwrap_or(Input {
                    kind: InputKind::Command,
                    text: String::new(),
                });
                return match kind {
                    InputKind::Command => self.run_command(text.trim()),
                    InputKind::Filter => Command::None,
                    InputKind::Pattern => {
                        self.db.pattern = text;
                        self.db.cursors = vec!["0".into()];
                        self.load_keys()
                    }
                };
            }
            _ => return Command::None,
        }
        if input.kind == InputKind::Filter {
            let text = input.text.clone();
            let level = self.level_mut();
            level.filter = text;
            level.table.select(Some(0));
        }
        Command::None
    }

    fn run_command(&mut self, cmd: &str) -> Command {
        let root = match cmd {
            "groups" | "group" | "g" | "rg" => Screen::Groups,
            "pods" | "pod" | "po" => Screen::Pods,
            "events" | "event" | "ev" => Screen::Events,
            "q" | "quit" => return Command::Quit,
            other => {
                self.flash = Some(format!(
                    "unknown command: {other} (groups, pods, events, quit)"
                ));
                return Command::None;
            }
        };
        self.levels = vec![Level::new(root)];
        Command::None
    }

    fn back(&mut self) -> Command {
        if !self.filter().is_empty() {
            self.level_mut().filter.clear();
            return Command::None;
        }
        if self.levels.len() > 1 {
            self.levels.pop();
        }
        Command::None
    }

    fn next_tab(&mut self) {
        if let Screen::Group { tab, .. } = &mut self.level_mut().screen {
            *tab = tab.next();
            let level = self.level_mut();
            level.table.select(Some(0));
            level.filter.clear();
        }
    }

    fn reload(&mut self) -> Command {
        match self.screen().clone() {
            Screen::Node { group, node } => self.load_info(&group, &node),
            Screen::Keys { .. } => self.load_keys(),
            Screen::Value { .. } => self.load_value(),
            _ => Command::Refresh,
        }
    }

    fn page(&mut self, forward: bool) -> Command {
        if !matches!(self.screen(), Screen::Keys { .. }) {
            return Command::None;
        }
        if forward {
            let Load::Ready(page) = &self.db.keys else {
                return Command::None;
            };
            if page.cursor == "0" {
                self.flash = Some("last page".into());
                return Command::None;
            }
            let next = page.cursor.clone();
            self.db.cursors.push(next);
        } else if self.db.cursors.len() > 1 {
            self.db.cursors.pop();
        } else {
            return Command::None;
        }
        self.table_mut().select(Some(0));
        self.load_keys()
    }

    fn drill(&mut self) -> Command {
        let selected = self.level().table.selected().unwrap_or(0);
        let Some(next) = rows::target(self, selected) else {
            return Command::None;
        };
        self.push(next.clone());
        match next {
            Screen::Node { group, node } => {
                self.db = DbView::default();
                self.load_info(&group, &node)
            }
            Screen::Keys { .. } => {
                self.db.cursors = vec!["0".into()];
                self.db.pattern.clear();
                self.load_keys()
            }
            Screen::Value { .. } => self.load_value(),
            _ => Command::None,
        }
    }

    fn db_command(&mut self, group: &str, node: &str, request: DbRequest) -> Command {
        let target = match rows::db_target(&self.state, group, node) {
            Ok(t) => t,
            Err(e) => {
                self.on_db_failure(e);
                return Command::None;
            }
        };
        self.db.seq += 1;
        Command::Db {
            seq: self.db.seq,
            target,
            request,
        }
    }

    fn on_db_failure(&mut self, e: String) {
        match self.screen() {
            Screen::Node { .. } => self.db.info = Load::Failed(e),
            Screen::Keys { .. } => self.db.keys = Load::Failed(e),
            _ => self.db.value = Load::Failed(e),
        }
    }

    fn load_info(&mut self, group: &str, node: &str) -> Command {
        self.db.info = Load::Loading;
        self.db_command(group, node, DbRequest::Info)
    }

    fn load_keys(&mut self) -> Command {
        let Screen::Keys { group, node, db } = self.screen().clone() else {
            return Command::None;
        };
        self.db.keys = Load::Loading;
        let request = DbRequest::Keys {
            db,
            pattern: self.db.pattern.clone(),
            cursor: self
                .db
                .cursors
                .last()
                .cloned()
                .unwrap_or_else(|| "0".into()),
        };
        self.db_command(&group, &node, request)
    }

    fn load_value(&mut self) -> Command {
        let Screen::Value {
            group,
            node,
            db,
            key,
            kind,
        } = self.screen().clone()
        else {
            return Command::None;
        };
        self.db.value = Load::Loading;
        self.db_command(&group, &node, DbRequest::Value { db, key, kind })
    }
}

fn move_selection(table: &mut TableState, code: KeyCode, len: usize) {
    if len == 0 {
        table.select(None);
        return;
    }
    let last = len - 1;
    let current = table.selected().unwrap_or(0).min(last);
    let next = match code {
        KeyCode::Down | KeyCode::Char('j') => (current + 1).min(last),
        KeyCode::Up | KeyCode::Char('k') => current.saturating_sub(1),
        KeyCode::PageDown => (current + 10).min(last),
        KeyCode::PageUp => current.saturating_sub(10),
        KeyCode::Home | KeyCode::Char('g') => 0,
        KeyCode::End | KeyCode::Char('G') => last,
        _ => current,
    };
    table.select(Some(next));
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::db::model::{DbStat, KeyInfo};
    use crate::types::tests::snapshot;

    pub fn env() -> Env {
        Env {
            target: Some("prod".into()),
            context: "prod".into(),
            scope: "shop,pay".into(),
            profile: "prod".into(),
            region: "us-east-1".into(),
            interval: Duration::from_secs(15),
        }
    }

    pub fn ui() -> Ui {
        let mut ui = Ui::new(env());
        ui.on_msg(Msg::Snapshot(Box::new(snapshot())));
        ui
    }

    pub fn press(ui: &mut Ui, code: KeyCode) -> Command {
        ui.handle_key(KeyEvent::from(code))
    }

    pub fn type_text(ui: &mut Ui, text: &str) {
        for c in text.chars() {
            press(ui, KeyCode::Char(c));
        }
    }

    pub fn info() -> NodeInfo {
        crate::db::info::node_info(
            "valkey_version:7.2.6\r\nrole:master\r\ndb0:keys=2,expires=1,avg_ttl=10\r\ndb3:keys=1,expires=0,avg_ttl=0\r\n",
        )
    }

    pub fn page(cursor: &str) -> KeyPage {
        KeyPage {
            cursor: cursor.into(),
            keys: vec![
                KeyInfo {
                    key: b"user:1".to_vec(),
                    kind: "hash".into(),
                    ttl_ms: -1,
                    size: Some(120),
                },
                KeyInfo {
                    key: b"session:9".to_vec(),
                    kind: "string".into(),
                    ttl_ms: 90_000,
                    size: Some(56),
                },
            ],
        }
    }

    fn seq_of(cmd: &Command) -> u64 {
        match cmd {
            Command::Db { seq, .. } => *seq,
            other => panic!("expected db command, got {other:?}"),
        }
    }

    #[test]
    fn drills_group_node_db_key_value() {
        let mut ui = ui();
        press(&mut ui, KeyCode::Char('j'));
        press(&mut ui, KeyCode::Enter);
        assert_eq!(
            ui.screen(),
            &Screen::Group {
                id: "orders".into(),
                tab: GroupTab::Nodes
            }
        );

        let cmd = press(&mut ui, KeyCode::Enter);
        let Command::Db {
            seq,
            target,
            request,
        } = &cmd
        else {
            panic!("{cmd:?}")
        };
        assert_eq!(request, &DbRequest::Info);
        assert_eq!(target.host, "10.0.0.1".parse::<std::net::IpAddr>().unwrap());
        assert_eq!(target.gateways[0].name, "api-1", "pods on the node first");
        assert_eq!(target.gateways.len(), 3);
        assert_eq!(ui.db.info, Load::Loading);
        ui.on_db(*seq, Ok((DbReply::Info(info()), "shop/api-1".into())));
        assert!(matches!(ui.db.info, Load::Ready(_)));

        press(&mut ui, KeyCode::Char('j'));
        let cmd = press(&mut ui, KeyCode::Enter);
        assert_eq!(
            ui.screen(),
            &Screen::Keys {
                group: "orders".into(),
                node: "orders-001".into(),
                db: 3
            }
        );
        let seq = seq_of(&cmd);
        ui.on_db(seq, Ok((DbReply::Keys(page("42")), "shop/api-1".into())));

        let cmd = press(&mut ui, KeyCode::Char('n'));
        assert_eq!(ui.db.cursors, ["0", "42"]);
        ui.on_db(seq_of(&cmd), Ok((DbReply::Keys(page("0")), String::new())));
        assert_eq!(press(&mut ui, KeyCode::Char('n')), Command::None);
        assert_eq!(ui.flash.as_deref(), Some("last page"));
        let cmd = press(&mut ui, KeyCode::Char('p'));
        assert_eq!(ui.db.cursors, ["0"]);
        ui.on_db(seq_of(&cmd), Ok((DbReply::Keys(page("42")), String::new())));
        assert_eq!(press(&mut ui, KeyCode::Char('p')), Command::None);

        press(&mut ui, KeyCode::Char('/'));
        type_text(&mut ui, "user:*");
        let cmd = press(&mut ui, KeyCode::Enter);
        assert_eq!(ui.db.pattern, "user:*");
        let Command::Db {
            request:
                DbRequest::Keys {
                    pattern,
                    cursor,
                    db,
                },
            ..
        } = &cmd
        else {
            panic!()
        };
        assert_eq!((pattern.as_str(), cursor.as_str(), *db), ("user:*", "0", 3));
        ui.on_db(seq_of(&cmd), Ok((DbReply::Keys(page("0")), String::new())));

        let cmd = press(&mut ui, KeyCode::Enter);
        let Command::Db {
            request: DbRequest::Value { key, kind, .. },
            ..
        } = &cmd
        else {
            panic!()
        };
        assert_eq!((key.as_slice(), kind.as_str()), (&b"user:1"[..], "hash"));
        let stale = seq_of(&cmd);
        let cmd = press(&mut ui, KeyCode::Char('r'));
        ui.on_db(stale, Err("late".into()));
        assert_eq!(ui.db.value, Load::Loading, "stale reply ignored");
        ui.on_db(seq_of(&cmd), Err("timeout".into()));
        assert_eq!(ui.db.value, Load::Failed("timeout".into()));
        assert_eq!(
            ui.crumbs(),
            ["groups", "orders (nodes)", "orders-001", "db3", "user:1"]
        );
    }

    #[test]
    fn db_failures_land_on_the_current_screen() {
        let mut ui = ui();
        ui.push(Screen::Node {
            group: "orders".into(),
            node: "orders-001".into(),
        });
        let cmd = press(&mut ui, KeyCode::Char('r'));
        ui.on_db(seq_of(&cmd), Err("no nc".into()));
        assert_eq!(ui.db.info, Load::Failed("no nc".into()));
        ui.push(Screen::Keys {
            group: "orders".into(),
            node: "orders-001".into(),
            db: 0,
        });
        let cmd = press(&mut ui, KeyCode::Char('r'));
        ui.on_db(seq_of(&cmd), Err("x".into()));
        assert_eq!(ui.db.keys, Load::Failed("x".into()));
        ui.push(Screen::Node {
            group: "orders".into(),
            node: "unknown-001".into(),
        });
        assert_eq!(press(&mut ui, KeyCode::Char('r')), Command::None);
        assert!(matches!(ui.db.info, Load::Failed(ref e) if e.contains("unknown-001")));
        ui.push(Screen::Keys {
            group: "x".into(),
            node: "unknown-001".into(),
            db: 0,
        });
        press(&mut ui, KeyCode::Char('r'));
        assert!(matches!(ui.db.keys, Load::Failed(_)));
        ui.push(Screen::Value {
            group: "x".into(),
            node: "unknown-001".into(),
            db: 0,
            key: b"k".to_vec(),
            kind: "string".into(),
        });
        press(&mut ui, KeyCode::Char('r'));
        assert!(matches!(ui.db.value, Load::Failed(_)));
        ui.on_db(
            ui.db.seq,
            Ok((
                DbReply::Value(KeyValue {
                    key: vec![],
                    kind: "string".into(),
                    len: None,
                    rows: vec![],
                    truncated: false,
                }),
                String::new(),
            )),
        );
        assert!(matches!(ui.db.value, Load::Ready(_)));
    }

    #[test]
    fn tabs_clients_and_pods() {
        let mut ui = ui();
        press(&mut ui, KeyCode::Char('j'));
        press(&mut ui, KeyCode::Enter);
        press(&mut ui, KeyCode::Tab);
        assert_eq!(
            ui.screen(),
            &Screen::Group {
                id: "orders".into(),
                tab: GroupTab::Clients
            }
        );
        press(&mut ui, KeyCode::Enter);
        assert_eq!(
            ui.screen(),
            &Screen::Clients {
                group: "orders".into(),
                namespace: "shop".into(),
                app: "api".into()
            }
        );
        press(&mut ui, KeyCode::Enter);
        assert_eq!(
            ui.screen(),
            &Screen::Pod {
                key: "shop/api-1".into()
            }
        );
        press(&mut ui, KeyCode::Esc);
        press(&mut ui, KeyCode::Esc);
        press(&mut ui, KeyCode::BackTab);
        assert_eq!(
            ui.screen(),
            &Screen::Group {
                id: "orders".into(),
                tab: GroupTab::Pods
            }
        );
        press(&mut ui, KeyCode::Enter);
        assert_eq!(
            ui.screen(),
            &Screen::Pod {
                key: "shop/api-1".into()
            }
        );
        assert_eq!(
            press(&mut ui, KeyCode::Enter),
            Command::None,
            "pod is a leaf"
        );
        assert_eq!(press(&mut ui, KeyCode::Char('r')), Command::Refresh);
        assert_eq!(press(&mut ui, KeyCode::Char('n')), Command::None);
    }

    #[test]
    fn command_mode_switches_roots() {
        let mut ui = ui();
        press(&mut ui, KeyCode::Char(':'));
        type_text(&mut ui, "pods");
        press(&mut ui, KeyCode::Enter);
        assert_eq!(ui.screen(), &Screen::Pods);
        press(&mut ui, KeyCode::Enter);
        assert!(matches!(ui.screen(), Screen::Pod { .. }));
        press(&mut ui, KeyCode::Char(':'));
        type_text(&mut ui, "ev");
        press(&mut ui, KeyCode::Enter);
        assert_eq!(ui.crumbs(), ["events"]);
        press(&mut ui, KeyCode::Char(':'));
        type_text(&mut ui, "nope");
        press(&mut ui, KeyCode::Backspace);
        press(&mut ui, KeyCode::Enter);
        assert!(
            ui.flash
                .as_deref()
                .unwrap()
                .contains("unknown command: nop")
        );
        press(&mut ui, KeyCode::Char(':'));
        press(&mut ui, KeyCode::Esc);
        assert!(ui.input.is_none());
        press(&mut ui, KeyCode::Char(':'));
        type_text(&mut ui, "g");
        press(&mut ui, KeyCode::Enter);
        assert_eq!(ui.screen(), &Screen::Groups);
        press(&mut ui, KeyCode::Char(':'));
        type_text(&mut ui, "q");
        assert_eq!(press(&mut ui, KeyCode::Enter), Command::Quit);
    }

    #[test]
    fn filter_narrows_and_esc_clears_before_back() {
        let mut ui = ui();
        press(&mut ui, KeyCode::Char('/'));
        type_text(&mut ui, "bil");
        press(&mut ui, KeyCode::Enter);
        assert_eq!(ui.filter(), "bil");
        assert_eq!(rows::len(&ui), 1);
        press(&mut ui, KeyCode::Enter);
        assert_eq!(
            ui.screen(),
            &Screen::Group {
                id: "billing".into(),
                tab: GroupTab::Nodes
            }
        );
        press(&mut ui, KeyCode::Esc);
        press(&mut ui, KeyCode::Esc);
        assert_eq!(ui.filter(), "");
        assert_eq!(rows::len(&ui), 2);
        press(&mut ui, KeyCode::Char('/'));
        type_text(&mut ui, "x");
        press(&mut ui, KeyCode::Esc);
        assert_eq!(ui.filter(), "");
        press(&mut ui, KeyCode::Esc);
        assert_eq!(ui.screen(), &Screen::Groups, "root stays");
        ui.push(Screen::Pod { key: "x".into() });
        press(&mut ui, KeyCode::Char('/'));
        assert!(ui.input.is_none(), "leaf screens have no filter");
        press(&mut ui, KeyCode::Char('x'));
        assert_eq!(press(&mut ui, KeyCode::Char('q')), Command::Quit);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(ui.handle_key(ctrl_c), Command::Quit);
    }

    #[test]
    fn selection_moves_within_bounds() {
        let mut ui = ui();
        for (code, expected) in [
            (KeyCode::Char('G'), 1),
            (KeyCode::Char('j'), 1),
            (KeyCode::Char('k'), 0),
            (KeyCode::PageDown, 1),
            (KeyCode::PageUp, 0),
            (KeyCode::End, 1),
            (KeyCode::Home, 0),
        ] {
            press(&mut ui, code);
            assert_eq!(ui.table_mut().selected(), Some(expected), "{code:?}");
        }
        let mut t = TableState::default();
        move_selection(&mut t, KeyCode::Down, 0);
        assert_eq!(t.selected(), None);
        let stat = DbStat {
            db: 0,
            keys: 0,
            expires: 0,
            avg_ttl_ms: 0,
        };
        assert_eq!(stat.db, 0);
        assert_eq!(
            GroupTab::ALL.map(GroupTab::label),
            ["nodes", "clients", "pods"]
        );
    }
}

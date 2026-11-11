use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::app::{App, GenState, Screen, SetupFocus, SetupState, UnlockState};
use crate::session;
use crate::vault::{Item, Link, NewItem, Vault};

pub const MASTER: &str = "correct horse battery";

pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("vlt-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn vault_path(&self) -> PathBuf {
        self.0.join("vault.json")
    }

    pub fn session_path(&self) -> PathBuf {
        self.0.join("session.json")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub const fn no_keyring() -> Option<[u8; 32]> {
    None
}

#[allow(clippy::unnecessary_wraps)]
pub const fn fixed_keyring() -> Option<[u8; 32]> {
    Some([7u8; 32])
}

pub fn store(dir: &TempDir) -> session::Store {
    session::Store::new(Some(dir.session_path()), no_keyring)
}

pub fn new_item(title: &str, group: &str) -> NewItem {
    NewItem {
        title: title.to_string(),
        username: format!("{}-user", title.to_lowercase()),
        password: format!("{title}-pw"),
        url: String::new(),
        notes: String::new(),
        group: group.to_string(),
        links: Vec::new(),
    }
}

pub fn link(name: &str, description: &str, url: &str) -> Link {
    Link {
        name: name.to_string(),
        description: description.to_string(),
        url: url.to_string(),
    }
}

fn base_app(dir: &TempDir, vault: Option<Vault>, screen: Screen) -> App {
    App {
        vault_path: dir.vault_path(),
        vault,
        screen,
        status: None,
        clipboard: None,
        clipboard_clear_at: None,
        should_quit: false,
        show_help: false,
        session: store(dir),
    }
}

pub fn unlocked_app(items: Vec<NewItem>) -> (TempDir, App) {
    let dir = TempDir::new();
    let mut vault = Vault::create(&dir.vault_path(), MASTER).expect("create vault");
    for it in items {
        vault.add_item(Item::from_new(it));
    }
    vault.persist().expect("persist vault");
    let mut app = base_app(&dir, Some(vault), Screen::Generator(GenState::default()));
    app.back_to_list();
    (dir, app)
}

pub fn setup_app() -> (TempDir, App) {
    let dir = TempDir::new();
    let screen = Screen::Setup(SetupState {
        password: String::new(),
        confirm: String::new(),
        focus: SetupFocus::Password,
        error: None,
        reveal: false,
    });
    let app = base_app(&dir, None, screen);
    (dir, app)
}

pub fn locked_app() -> (TempDir, App) {
    let dir = TempDir::new();
    Vault::create(&dir.vault_path(), MASTER).expect("create vault");
    let screen = Screen::Unlock(UnlockState {
        password: String::new(),
        error: None,
        reveal: false,
    });
    let app = base_app(&dir, None, screen);
    (dir, app)
}

pub fn item_id(app: &App, title: &str) -> String {
    app.vault
        .as_ref()
        .and_then(|v| v.items().iter().find(|i| i.title == title))
        .map(|i| i.id.clone())
        .expect("item exists")
}

pub const fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub const fn ch(c: char) -> KeyEvent {
    key(KeyCode::Char(c))
}

pub const fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

pub fn render_to_string(app: &App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|f| crate::ui::render(f, app))
        .expect("draw frame");
    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

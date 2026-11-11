use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::crypto;
use crate::error::{VltError, VltResult};
use crate::session;
use crate::vault::{Item, ItemSummary, Link, NewItem, UpdateItem, Vault};

pub const STATUS_TTL: Duration = Duration::from_secs(3);
pub const CLIPBOARD_CLEAR: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    Info,
    Error,
}

pub struct App {
    pub vault_path: PathBuf,
    pub vault: Option<Vault>,
    pub screen: Screen,
    pub status: Option<(String, Instant, StatusKind)>,
    pub clipboard: Option<arboard::Clipboard>,
    pub clipboard_clear_at: Option<Instant>,
    pub should_quit: bool,
    pub show_help: bool,
    pub session: session::Store,
}

pub enum Screen {
    Setup(SetupState),
    Unlock(UnlockState),
    List(ListState),
    Detail(DetailState),
    Form(FormState),
    Generator(GenState),
}

pub struct SetupState {
    pub password: String,
    pub confirm: String,
    pub focus: SetupFocus,
    pub error: Option<String>,
    pub reveal: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SetupFocus {
    Password,
    Confirm,
}

pub struct UnlockState {
    pub password: String,
    pub error: Option<String>,
    pub reveal: bool,
}

pub struct ListState {
    pub selected: usize,
    pub search: String,
    pub searching: bool,
    pub collapsed: BTreeSet<String>,
    pub confirm_delete_id: Option<String>,
}

pub struct DetailState {
    pub id: String,
    pub reveal: bool,
    pub pending_g: bool,
}

#[derive(Clone)]
pub enum FormMode {
    Create,
    Edit(String),
}

pub struct FormState {
    pub mode: FormMode,
    pub focus: FormFocus,
    pub title: String,
    pub username: String,
    pub password: String,
    pub url: String,
    pub group: String,
    pub notes: String,
    pub links: Vec<Link>,
    pub reveal: bool,
    pub error: Option<String>,
}

/// Where the cursor lives inside a Form: one of the six main fields, a
/// sub-field of an existing link, or the trailing `+ add link` row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FormFocus {
    Title,
    Username,
    Password,
    Url,
    Group,
    Notes,
    LinkName(usize),
    LinkDescription(usize),
    LinkUrl(usize),
    AddLink,
}

impl FormState {
    pub const fn focus_next(&mut self) {
        self.focus = match self.focus {
            FormFocus::Title => FormFocus::Username,
            FormFocus::Username => FormFocus::Password,
            FormFocus::Password => FormFocus::Url,
            FormFocus::Url => FormFocus::Group,
            FormFocus::Group => FormFocus::Notes,
            FormFocus::Notes => self.first_link_focus(),
            FormFocus::LinkName(i) => FormFocus::LinkDescription(i),
            FormFocus::LinkDescription(i) => FormFocus::LinkUrl(i),
            FormFocus::LinkUrl(i) => {
                if i + 1 < self.links.len() {
                    FormFocus::LinkName(i + 1)
                } else {
                    FormFocus::AddLink
                }
            }
            FormFocus::AddLink => FormFocus::Title,
        };
    }

    pub const fn focus_prev(&mut self) {
        self.focus = match self.focus {
            FormFocus::Title => FormFocus::AddLink,
            FormFocus::Username => FormFocus::Title,
            FormFocus::Password => FormFocus::Username,
            FormFocus::Url => FormFocus::Password,
            FormFocus::Group => FormFocus::Url,
            FormFocus::Notes => FormFocus::Group,
            FormFocus::LinkName(0) => FormFocus::Notes,
            FormFocus::LinkName(i) => FormFocus::LinkUrl(i - 1),
            FormFocus::LinkDescription(i) => FormFocus::LinkName(i),
            FormFocus::LinkUrl(i) => FormFocus::LinkDescription(i),
            FormFocus::AddLink => {
                if self.links.is_empty() {
                    FormFocus::Notes
                } else {
                    FormFocus::LinkUrl(self.links.len() - 1)
                }
            }
        };
    }

    const fn first_link_focus(&self) -> FormFocus {
        if self.links.is_empty() {
            FormFocus::AddLink
        } else {
            FormFocus::LinkName(0)
        }
    }

    pub fn add_link(&mut self) {
        let i = self.links.len();
        self.links.push(Link::default());
        self.focus = FormFocus::LinkName(i);
    }

    /// Delete the link the cursor is currently inside. No-op outside link
    /// rows.
    pub fn delete_focused_link(&mut self) {
        let (FormFocus::LinkName(i) | FormFocus::LinkDescription(i) | FormFocus::LinkUrl(i)) =
            self.focus
        else {
            return;
        };
        if i >= self.links.len() {
            return;
        }
        self.links.remove(i);
        if self.links.is_empty() {
            self.focus = FormFocus::AddLink;
        } else if i >= self.links.len() {
            self.focus = FormFocus::LinkName(self.links.len() - 1);
        } else {
            self.focus = FormFocus::LinkName(i);
        }
    }

    pub fn current_value_mut(&mut self) -> Option<&mut String> {
        match self.focus {
            FormFocus::Title => Some(&mut self.title),
            FormFocus::Username => Some(&mut self.username),
            FormFocus::Password => Some(&mut self.password),
            FormFocus::Url => Some(&mut self.url),
            FormFocus::Group => Some(&mut self.group),
            FormFocus::Notes => Some(&mut self.notes),
            FormFocus::LinkName(i) => self.links.get_mut(i).map(|l| &mut l.name),
            FormFocus::LinkDescription(i) => self.links.get_mut(i).map(|l| &mut l.description),
            FormFocus::LinkUrl(i) => self.links.get_mut(i).map(|l| &mut l.url),
            FormFocus::AddLink => None,
        }
    }
}

pub struct GenState {
    pub length: usize,
    pub symbols: bool,
    pub numbers: bool,
    pub output: String,
}

impl Default for GenState {
    fn default() -> Self {
        Self::new()
    }
}

impl GenState {
    pub fn new() -> Self {
        let mut s = Self {
            length: 24,
            symbols: true,
            numbers: true,
            output: String::new(),
        };
        s.regenerate();
        s
    }

    pub fn regenerate(&mut self) {
        self.output = crypto::generate_password(self.length, self.symbols, self.numbers);
    }
}

#[derive(Clone)]
pub struct RenderRow {
    pub depth: usize,
    pub kind: RowKind,
}

#[derive(Clone)]
pub enum RowKind {
    Group {
        path: String,
        name: String,
        count: usize,
        expanded: bool,
    },
    Item(ItemSummary),
}

/// Resolve the vault file path following the XDG Base Directory Specification:
///   1. `$VLT_VAULT_PATH` overrides everything.
///   2. `$XDG_DATA_HOME/vlt/vault.json` if `XDG_DATA_HOME` is set and non-empty.
///   3. Fallback `$HOME/.local/share/vlt/vault.json`.
fn resolve_vault_path() -> VltResult<PathBuf> {
    vault_path_from(
        std::env::var_os("VLT_VAULT_PATH"),
        std::env::var_os("XDG_DATA_HOME"),
        std::env::var_os("HOME"),
    )
}

fn vault_path_from(
    vault: Option<OsString>,
    xdg_data: Option<OsString>,
    home: Option<OsString>,
) -> VltResult<PathBuf> {
    if let Some(p) = vault.filter(|s| !s.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    let base = xdg_data
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            home.filter(|s| !s.is_empty())
                .map(|h| PathBuf::from(h).join(".local").join("share"))
        })
        .ok_or(VltError::NoConfigDir)?;
    Ok(base.join("vlt").join("vault.json"))
}

/// One-time migration of a vault file from the previous macOS-native location
/// (`~/Library/Application Support/io.younsl.vlt/vault.json`) to the XDG path.
/// Runs only when the new path does not exist and the legacy file does. Failures
/// are silent — vlt will simply offer to create a fresh vault.
fn migrate_legacy(new_path: &Path) {
    if new_path.exists() {
        return;
    }
    if !cfg!(target_os = "macos") {
        return;
    }
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let legacy = PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("io.younsl.vlt")
        .join("vault.json");
    move_legacy(&legacy, new_path);
}

fn move_legacy(legacy: &Path, new_path: &Path) {
    if !legacy.exists() {
        return;
    }
    if let Some(parent) = new_path.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return;
    }
    let _ = std::fs::rename(legacy, new_path);
}

impl App {
    pub fn new() -> VltResult<Self> {
        let vault_path = resolve_vault_path()?;
        migrate_legacy(&vault_path);
        Ok(Self::launch(
            vault_path,
            session::Store::from_env(),
            arboard::Clipboard::new().ok(),
        ))
    }

    fn launch(
        vault_path: PathBuf,
        session: session::Store,
        clipboard: Option<arboard::Clipboard>,
    ) -> Self {
        // Try to silently unlock from a non-expired cached session.
        let mut vault: Option<Vault> = None;
        let mut cache_status: Option<String> = None;
        if vault_path.exists()
            && let session::LoadResult::Hit { master, encrypted } = session.load(&vault_path)
        {
            match Vault::open_vault(&vault_path, &master) {
                Ok(v) => {
                    vault = Some(v);
                    cache_status = Some(if encrypted {
                        "session restored (encrypted via keyring)".to_string()
                    } else {
                        "session restored (plaintext fallback)".to_string()
                    });
                }
                Err(_) => {
                    // Stale cache (password changed, vault rotated, etc.)
                    session.clear();
                }
            }
        }

        let screen = if vault.is_some() {
            Screen::List(default_list())
        } else if vault_path.exists() {
            Screen::Unlock(UnlockState {
                password: String::new(),
                error: None,
                reveal: false,
            })
        } else {
            Screen::Setup(SetupState {
                password: String::new(),
                confirm: String::new(),
                focus: SetupFocus::Password,
                error: None,
                reveal: false,
            })
        };

        let mut app = Self {
            vault_path,
            vault,
            screen,
            status: None,
            clipboard,
            clipboard_clear_at: None,
            should_quit: false,
            show_help: false,
            session,
        };
        if let Some(msg) = cache_status {
            app.set_status(msg, StatusKind::Info);
        }
        app
    }

    pub fn set_status<S: Into<String>>(&mut self, msg: S, kind: StatusKind) {
        self.status = Some((msg.into(), Instant::now(), kind));
    }

    pub fn tick(&mut self) {
        if let Some((_, t, _)) = &self.status
            && t.elapsed() > STATUS_TTL
        {
            self.status = None;
        }
        if let Some(at) = self.clipboard_clear_at
            && Instant::now() >= at
        {
            if let Some(cb) = self.clipboard.as_mut() {
                let _ = cb.set_text(String::new());
            }
            self.clipboard_clear_at = None;
        }
    }

    pub fn try_setup(&mut self) {
        let Screen::Setup(s) = &mut self.screen else {
            return;
        };
        if s.password.len() < 8 {
            s.error = Some("vault password must be at least 8 characters".into());
            return;
        }
        if s.password != s.confirm {
            s.error = Some("passwords do not match".into());
            return;
        }
        let pwd = s.password.clone();
        match Vault::create(&self.vault_path, &pwd) {
            Ok(v) => {
                self.vault = Some(v);
                let outcome = self.session.save(&pwd, &self.vault_path);
                self.screen = Screen::List(default_list());
                self.set_status(
                    format!("vault created · {}", session_outcome_label(outcome)),
                    StatusKind::Info,
                );
            }
            Err(e) => s.error = Some(e.to_string()),
        }
    }

    pub fn try_unlock(&mut self) {
        let Screen::Unlock(s) = &mut self.screen else {
            return;
        };
        let pwd = s.password.clone();
        match Vault::open_vault(&self.vault_path, &pwd) {
            Ok(v) => {
                self.vault = Some(v);
                let outcome = self.session.save(&pwd, &self.vault_path);
                self.screen = Screen::List(default_list());
                self.set_status(
                    format!("unlocked · {}", session_outcome_label(outcome)),
                    StatusKind::Info,
                );
            }
            Err(e) => s.error = Some(e.to_string()),
        }
    }

    pub fn lock_vault(&mut self) {
        self.vault = None;
        self.session.clear();
        self.screen = Screen::Unlock(UnlockState {
            password: String::new(),
            error: None,
            reveal: false,
        });
    }

    pub fn open_url(&mut self, url: &str) {
        if url.is_empty() {
            self.set_status("no URL", StatusKind::Error);
            return;
        }
        match open::that_detached(url) {
            Ok(()) => self.set_status(format!("opened {url}"), StatusKind::Info),
            Err(e) => self.set_status(format!("open failed: {e}"), StatusKind::Error),
        }
    }

    pub fn copy_value(&mut self, value: &str, label: &str) {
        let Some(cb) = self.clipboard.as_mut() else {
            self.set_status("clipboard unavailable", StatusKind::Error);
            return;
        };
        match cb.set_text(value.to_string()) {
            Ok(()) => {
                self.clipboard_clear_at = Some(Instant::now() + CLIPBOARD_CLEAR);
                self.set_status(
                    format!("{label} copied (clears in {}s)", CLIPBOARD_CLEAR.as_secs()),
                    StatusKind::Info,
                );
            }
            Err(e) => self.set_status(format!("clipboard: {e}"), StatusKind::Error),
        }
    }

    pub fn open_detail(&mut self, id: String) {
        self.screen = Screen::Detail(DetailState {
            id,
            reveal: false,
            pending_g: false,
        });
    }

    pub fn back_to_list(&mut self) {
        self.screen = Screen::List(default_list());
    }

    pub fn start_new(&mut self) {
        self.screen = Screen::Form(FormState {
            mode: FormMode::Create,
            focus: FormFocus::Title,
            title: String::new(),
            username: String::new(),
            password: String::new(),
            url: String::new(),
            group: String::new(),
            notes: String::new(),
            links: Vec::new(),
            reveal: false,
            error: None,
        });
    }

    pub fn start_edit(&mut self, id: &str) -> VltResult<()> {
        let v = self.vault.as_ref().ok_or(VltError::NotInitialized)?;
        let it = v.find_item(id)?.clone();
        self.screen = Screen::Form(FormState {
            mode: FormMode::Edit(it.id),
            focus: FormFocus::Title,
            title: it.title,
            username: it.username,
            password: it.password,
            url: it.url,
            group: it.group,
            notes: it.notes,
            links: it.links,
            reveal: false,
            error: None,
        });
        Ok(())
    }

    pub fn save_form(&mut self) {
        let Screen::Form(f) = &mut self.screen else {
            return;
        };
        if f.title.trim().is_empty() {
            f.error = Some("title is required".into());
            return;
        }
        if f.password.is_empty() {
            f.error = Some("password is required".into());
            return;
        }
        let mode = f.mode.clone();
        let title = f.title.clone();
        let username = f.username.clone();
        let password = f.password.clone();
        let url = f.url.clone();
        let group = f.group.clone();
        let notes = f.notes.clone();
        let links = f.links.clone();
        let result: VltResult<String> = (|| {
            let v = self.vault.as_mut().ok_or(VltError::NotInitialized)?;
            match mode {
                FormMode::Create => {
                    let item = Item::from_new(NewItem {
                        title,
                        username,
                        password,
                        url,
                        notes,
                        group,
                        links,
                    });
                    let id = item.id.clone();
                    v.add_item(item);
                    v.persist()?;
                    Ok(id)
                }
                FormMode::Edit(id) => {
                    v.update_item(&id, |it| {
                        it.apply_update(UpdateItem {
                            title: Some(title),
                            username: Some(username),
                            password: Some(password),
                            url: Some(url),
                            notes: Some(notes),
                            group: Some(group),
                            links: Some(links),
                        });
                    })?;
                    v.persist()?;
                    Ok(id)
                }
            }
        })();
        match result {
            Ok(id) => {
                self.set_status("saved", StatusKind::Info);
                self.open_detail(id);
            }
            Err(e) => {
                if let Screen::Form(f) = &mut self.screen {
                    f.error = Some(e.to_string());
                }
            }
        }
    }

    pub fn delete_item(&mut self, id: &str) -> VltResult<()> {
        let v = self.vault.as_mut().ok_or(VltError::NotInitialized)?;
        v.delete_item(id)?;
        v.persist()?;
        Ok(())
    }

    pub fn visible_rows(&self, list: &ListState) -> Vec<RenderRow> {
        let Some(vault) = self.vault.as_ref() else {
            return Vec::new();
        };
        let q = list.search.trim().to_lowercase();
        let force_expand = !q.is_empty();
        let items: Vec<ItemSummary> = vault
            .items()
            .iter()
            .filter(|i| {
                if q.is_empty() {
                    true
                } else {
                    i.title.to_lowercase().contains(&q)
                        || i.username.to_lowercase().contains(&q)
                        || i.url.to_lowercase().contains(&q)
                        || i.group.to_lowercase().contains(&q)
                }
            })
            .map(ItemSummary::from)
            .collect();
        let tree = build_tree(&items);
        let mut rows = Vec::new();
        emit_rows(&tree, 0, &list.collapsed, force_expand, &mut rows);
        rows
    }
}

const fn session_outcome_label(o: session::SaveOutcome) -> &'static str {
    match o {
        session::SaveOutcome::Encrypted => "cached for 1h (encrypted via keyring)",
        session::SaveOutcome::Plaintext => "cached for 1h (plaintext, keyring unavailable)",
        session::SaveOutcome::Failed => "session caching failed",
    }
}

const fn default_list() -> ListState {
    ListState {
        selected: 0,
        search: String::new(),
        searching: false,
        collapsed: BTreeSet::new(),
        confirm_delete_id: None,
    }
}

struct TreeNode {
    name: String,
    path: String,
    children: std::collections::BTreeMap<String, Self>,
    items: Vec<ItemSummary>,
}

fn build_tree(items: &[ItemSummary]) -> TreeNode {
    let mut root = TreeNode {
        name: String::new(),
        path: String::new(),
        children: std::collections::BTreeMap::new(),
        items: Vec::new(),
    };
    for it in items {
        let path = it.group.trim();
        if path.is_empty() {
            root.items.push(it.clone());
            continue;
        }
        let mut node = &mut root;
        for part in path.split('/').map(str::trim).filter(|p| !p.is_empty()) {
            let parent_path = node.path.clone();
            node = node.children.entry(part.to_string()).or_insert_with(|| {
                let p = if parent_path.is_empty() {
                    part.to_string()
                } else {
                    format!("{parent_path}/{part}")
                };
                TreeNode {
                    name: part.to_string(),
                    path: p,
                    children: std::collections::BTreeMap::new(),
                    items: Vec::new(),
                }
            });
        }
        node.items.push(it.clone());
    }
    root
}

fn count_tree(node: &TreeNode) -> usize {
    let mut n = node.items.len();
    for c in node.children.values() {
        n += count_tree(c);
    }
    n
}

fn emit_rows(
    node: &TreeNode,
    depth: usize,
    collapsed: &BTreeSet<String>,
    force_expand: bool,
    out: &mut Vec<RenderRow>,
) {
    for child in node.children.values() {
        let expanded = force_expand || !collapsed.contains(&child.path);
        out.push(RenderRow {
            depth,
            kind: RowKind::Group {
                path: child.path.clone(),
                name: child.name.clone(),
                count: count_tree(child),
                expanded,
            },
        });
        if expanded {
            emit_rows(child, depth + 1, collapsed, force_expand, out);
        }
    }
    let mut items: Vec<ItemSummary> = node.items.clone();
    items.sort_by_key(|i| i.title.to_lowercase());
    for it in items {
        out.push(RenderRow {
            depth: depth + 1,
            kind: RowKind::Item(it),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        MASTER, TempDir, fixed_keyring, item_id, link, locked_app, new_item, no_keyring, setup_app,
        store, unlocked_app,
    };

    fn form(links: usize) -> FormState {
        FormState {
            mode: FormMode::Create,
            focus: FormFocus::Title,
            title: String::new(),
            username: String::new(),
            password: String::new(),
            url: String::new(),
            group: String::new(),
            notes: String::new(),
            links: (0..links).map(|_| Link::default()).collect(),
            reveal: false,
            error: None,
        }
    }

    fn past(secs: u64) -> Instant {
        Instant::now()
            .checked_sub(Duration::from_secs(secs))
            .expect("instant in the past")
    }

    fn status_text(app: &App) -> String {
        app.status.as_ref().map(|s| s.0.clone()).unwrap_or_default()
    }

    fn status_kind(app: &App) -> Option<StatusKind> {
        app.status.as_ref().map(|s| s.2)
    }

    fn form_mut(app: &mut App) -> &mut FormState {
        match &mut app.screen {
            Screen::Form(f) => f,
            _ => panic!("expected form screen"),
        }
    }

    fn form_error(app: &App) -> Option<String> {
        match &app.screen {
            Screen::Form(f) => f.error.clone(),
            _ => None,
        }
    }

    fn row_labels(app: &App, list: &ListState) -> Vec<String> {
        app.visible_rows(list)
            .iter()
            .map(|r| match &r.kind {
                RowKind::Group {
                    path,
                    count,
                    expanded,
                    ..
                } => format!("{}G:{path}:{count}:{expanded}", r.depth),
                RowKind::Item(it) => format!("{}I:{}", r.depth, it.title),
            })
            .collect()
    }

    #[test]
    fn focus_next_cycles_without_links() {
        let mut f = form(0);
        let expected = [
            FormFocus::Username,
            FormFocus::Password,
            FormFocus::Url,
            FormFocus::Group,
            FormFocus::Notes,
            FormFocus::AddLink,
            FormFocus::Title,
        ];
        for want in expected {
            f.focus_next();
            assert_eq!(f.focus, want);
        }
    }

    #[test]
    fn focus_next_walks_link_rows() {
        let mut f = form(2);
        f.focus = FormFocus::Notes;
        let expected = [
            FormFocus::LinkName(0),
            FormFocus::LinkDescription(0),
            FormFocus::LinkUrl(0),
            FormFocus::LinkName(1),
            FormFocus::LinkDescription(1),
            FormFocus::LinkUrl(1),
            FormFocus::AddLink,
        ];
        for want in expected {
            f.focus_next();
            assert_eq!(f.focus, want);
        }
    }

    #[test]
    fn focus_prev_cycles_through_links() {
        let mut f = form(2);
        let expected = [
            FormFocus::AddLink,
            FormFocus::LinkUrl(1),
            FormFocus::LinkDescription(1),
            FormFocus::LinkName(1),
            FormFocus::LinkUrl(0),
            FormFocus::LinkDescription(0),
            FormFocus::LinkName(0),
            FormFocus::Notes,
            FormFocus::Group,
            FormFocus::Url,
            FormFocus::Password,
            FormFocus::Username,
            FormFocus::Title,
        ];
        for want in expected {
            f.focus_prev();
            assert_eq!(f.focus, want);
        }
    }

    #[test]
    fn focus_prev_from_add_link_without_links_goes_to_notes() {
        let mut f = form(0);
        f.focus = FormFocus::AddLink;
        f.focus_prev();
        assert_eq!(f.focus, FormFocus::Notes);
    }

    #[test]
    fn add_link_focuses_new_link() {
        let mut f = form(1);
        f.add_link();
        assert_eq!(f.links.len(), 2);
        assert_eq!(f.focus, FormFocus::LinkName(1));
    }

    #[test]
    fn delete_focused_link_ignores_non_link_focus() {
        let mut f = form(1);
        f.delete_focused_link();
        assert_eq!(f.links.len(), 1);
        f.focus = FormFocus::LinkName(5);
        f.delete_focused_link();
        assert_eq!(f.links.len(), 1);
    }

    #[test]
    fn delete_last_remaining_link_moves_to_add_link() {
        let mut f = form(1);
        f.focus = FormFocus::LinkUrl(0);
        f.delete_focused_link();
        assert!(f.links.is_empty());
        assert_eq!(f.focus, FormFocus::AddLink);
    }

    #[test]
    fn delete_trailing_link_moves_to_previous() {
        let mut f = form(2);
        f.focus = FormFocus::LinkDescription(1);
        f.delete_focused_link();
        assert_eq!(f.links.len(), 1);
        assert_eq!(f.focus, FormFocus::LinkName(0));
    }

    #[test]
    fn delete_middle_link_keeps_index() {
        let mut f = form(3);
        f.focus = FormFocus::LinkName(1);
        f.delete_focused_link();
        assert_eq!(f.links.len(), 2);
        assert_eq!(f.focus, FormFocus::LinkName(1));
    }

    #[test]
    fn current_value_mut_targets_focused_field() {
        let mut f = form(1);
        let focuses = [
            FormFocus::Title,
            FormFocus::Username,
            FormFocus::Password,
            FormFocus::Url,
            FormFocus::Group,
            FormFocus::Notes,
            FormFocus::LinkName(0),
            FormFocus::LinkDescription(0),
            FormFocus::LinkUrl(0),
        ];
        for (i, focus) in focuses.into_iter().enumerate() {
            f.focus = focus;
            f.current_value_mut().unwrap().push_str(&i.to_string());
        }
        assert_eq!(f.title, "0");
        assert_eq!(f.username, "1");
        assert_eq!(f.password, "2");
        assert_eq!(f.url, "3");
        assert_eq!(f.group, "4");
        assert_eq!(f.notes, "5");
        assert_eq!(f.links[0].name, "6");
        assert_eq!(f.links[0].description, "7");
        assert_eq!(f.links[0].url, "8");
        f.focus = FormFocus::AddLink;
        assert!(f.current_value_mut().is_none());
        f.focus = FormFocus::LinkName(9);
        assert!(f.current_value_mut().is_none());
    }

    #[test]
    fn gen_state_defaults() {
        let g = GenState::default();
        assert_eq!(g.length, 24);
        assert!(g.symbols && g.numbers);
        assert_eq!(g.output.len(), 24);
    }

    #[test]
    fn gen_state_regenerate_respects_options() {
        let mut g = GenState::new();
        g.length = 40;
        g.symbols = false;
        g.numbers = false;
        g.regenerate();
        assert_eq!(g.output.len(), 40);
        assert!(g.output.chars().all(|c| c.is_ascii_alphabetic()));
    }

    #[test]
    fn vault_path_prefers_override() {
        let p = vault_path_from(Some("/v.json".into()), Some("/d".into()), Some("/h".into()));
        assert_eq!(p.unwrap(), PathBuf::from("/v.json"));
    }

    #[test]
    fn vault_path_uses_xdg_then_home() {
        assert_eq!(
            vault_path_from(Some("".into()), Some("/d".into()), None).unwrap(),
            PathBuf::from("/d/vlt/vault.json")
        );
        assert_eq!(
            vault_path_from(None, Some("".into()), Some("/h".into())).unwrap(),
            PathBuf::from("/h/.local/share/vlt/vault.json")
        );
        assert!(matches!(
            vault_path_from(None, None, Some("".into())),
            Err(VltError::NoConfigDir)
        ));
    }

    #[test]
    fn resolve_vault_path_reads_environment() {
        let from_env = vault_path_from(
            std::env::var_os("VLT_VAULT_PATH"),
            std::env::var_os("XDG_DATA_HOME"),
            std::env::var_os("HOME"),
        );
        assert_eq!(resolve_vault_path().ok(), from_env.ok());
    }

    #[test]
    fn migrate_legacy_skips_existing_vault() {
        let dir = TempDir::new();
        std::fs::write(dir.vault_path(), b"{}").unwrap();
        migrate_legacy(&dir.vault_path());
        assert_eq!(std::fs::read(dir.vault_path()).unwrap(), b"{}");
    }

    #[test]
    fn move_legacy_without_source_is_noop() {
        let dir = TempDir::new();
        move_legacy(&dir.path().join("legacy.json"), &dir.vault_path());
        assert!(!dir.vault_path().exists());
    }

    #[test]
    fn move_legacy_moves_file_into_new_location() {
        let dir = TempDir::new();
        let legacy = dir.path().join("legacy.json");
        std::fs::write(&legacy, b"legacy").unwrap();
        let target = dir.path().join("nested").join("vault.json");
        move_legacy(&legacy, &target);
        assert!(!legacy.exists());
        assert_eq!(std::fs::read(&target).unwrap(), b"legacy");
    }

    #[test]
    fn move_legacy_gives_up_when_parent_cannot_be_created() {
        let dir = TempDir::new();
        let legacy = dir.path().join("legacy.json");
        std::fs::write(&legacy, b"legacy").unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        move_legacy(&legacy, &blocker.join("vault.json"));
        assert!(legacy.exists());
    }

    #[test]
    fn launch_without_vault_shows_setup() {
        let dir = TempDir::new();
        let app = App::launch(dir.vault_path(), store(&dir), None);
        assert!(matches!(app.screen, Screen::Setup(_)));
        assert!(app.vault.is_none());
        assert!(app.status.is_none());
    }

    #[test]
    fn launch_with_vault_and_no_cache_shows_unlock() {
        let dir = TempDir::new();
        Vault::create(&dir.vault_path(), MASTER).unwrap();
        let app = App::launch(dir.vault_path(), store(&dir), None);
        assert!(matches!(app.screen, Screen::Unlock(_)));
    }

    #[test]
    fn launch_restores_plaintext_session() {
        let dir = TempDir::new();
        Vault::create(&dir.vault_path(), MASTER).unwrap();
        store(&dir).save(MASTER, &dir.vault_path());
        let app = App::launch(dir.vault_path(), store(&dir), None);
        assert!(matches!(app.screen, Screen::List(_)));
        assert!(app.vault.is_some());
        assert!(status_text(&app).contains("plaintext fallback"));
    }

    #[test]
    fn launch_restores_encrypted_session() {
        let dir = TempDir::new();
        Vault::create(&dir.vault_path(), MASTER).unwrap();
        let keyed = || session::Store::new(Some(dir.session_path()), fixed_keyring);
        keyed().save(MASTER, &dir.vault_path());
        let app = App::launch(dir.vault_path(), keyed(), None);
        assert!(matches!(app.screen, Screen::List(_)));
        assert!(status_text(&app).contains("encrypted via keyring"));
    }

    #[test]
    fn launch_clears_stale_session() {
        let dir = TempDir::new();
        Vault::create(&dir.vault_path(), MASTER).unwrap();
        let stale = session::Store::new(Some(dir.session_path()), no_keyring);
        stale.save("not the master", &dir.vault_path());
        let app = App::launch(dir.vault_path(), store(&dir), None);
        assert!(matches!(app.screen, Screen::Unlock(_)));
        assert!(!dir.session_path().exists());
    }

    #[test]
    fn tick_expires_old_status() {
        let (_dir, mut app) = unlocked_app(Vec::new());
        app.set_status("fresh", StatusKind::Info);
        app.tick();
        assert!(app.status.is_some());
        app.status = Some(("old".into(), past(10), StatusKind::Info));
        app.tick();
        assert!(app.status.is_none());
    }

    #[test]
    fn tick_clears_clipboard_deadline() {
        let (_dir, mut app) = unlocked_app(Vec::new());
        app.clipboard_clear_at = Some(Instant::now() + CLIPBOARD_CLEAR);
        app.tick();
        assert!(app.clipboard_clear_at.is_some());
        app.clipboard_clear_at = Some(past(1));
        app.tick();
        assert!(app.clipboard_clear_at.is_none());
    }

    fn setup_with(app: &mut App, password: &str, confirm: &str) {
        if let Screen::Setup(s) = &mut app.screen {
            s.password = password.into();
            s.confirm = confirm.into();
        }
        app.try_setup();
    }

    fn setup_error(app: &App) -> Option<String> {
        match &app.screen {
            Screen::Setup(s) => s.error.clone(),
            _ => None,
        }
    }

    #[test]
    fn try_setup_rejects_short_password() {
        let (_dir, mut app) = setup_app();
        setup_with(&mut app, "short", "short");
        assert!(setup_error(&app).unwrap().contains("at least 8"));
    }

    #[test]
    fn try_setup_rejects_mismatch() {
        let (_dir, mut app) = setup_app();
        setup_with(&mut app, "long enough 1", "long enough 2");
        assert_eq!(setup_error(&app).unwrap(), "passwords do not match");
    }

    #[test]
    fn try_setup_creates_vault_and_caches_session() {
        let (dir, mut app) = setup_app();
        setup_with(&mut app, MASTER, MASTER);
        assert!(matches!(app.screen, Screen::List(_)));
        assert!(app.vault.is_some());
        assert!(dir.vault_path().exists());
        assert!(dir.session_path().exists());
        assert!(status_text(&app).starts_with("vault created"));
        assert!(status_text(&app).contains("plaintext"));
    }

    #[test]
    fn try_setup_reports_existing_vault() {
        let (dir, mut app) = setup_app();
        Vault::create(&dir.vault_path(), MASTER).unwrap();
        setup_with(&mut app, MASTER, MASTER);
        assert!(setup_error(&app).unwrap().contains("already initialized"));
    }

    #[test]
    fn try_setup_and_unlock_ignore_other_screens() {
        let (_dir, mut app) = unlocked_app(Vec::new());
        app.try_setup();
        app.try_unlock();
        assert!(matches!(app.screen, Screen::List(_)));
    }

    fn unlock_with(app: &mut App, password: &str) {
        if let Screen::Unlock(s) = &mut app.screen {
            s.password = password.into();
        }
        app.try_unlock();
    }

    #[test]
    fn try_unlock_with_wrong_password_sets_error() {
        let (_dir, mut app) = locked_app();
        unlock_with(&mut app, "wrong password");
        match &app.screen {
            Screen::Unlock(s) => assert_eq!(s.error.as_deref(), Some("invalid master password")),
            _ => panic!("expected unlock screen"),
        }
    }

    #[test]
    fn try_unlock_opens_vault() {
        let (_dir, mut app) = locked_app();
        unlock_with(&mut app, MASTER);
        assert!(matches!(app.screen, Screen::List(_)));
        assert!(status_text(&app).starts_with("unlocked"));
    }

    #[test]
    fn lock_vault_clears_session() {
        let (dir, mut app) = unlocked_app(Vec::new());
        app.session.save(MASTER, &dir.vault_path());
        app.lock_vault();
        assert!(app.vault.is_none());
        assert!(matches!(app.screen, Screen::Unlock(_)));
        assert!(!dir.session_path().exists());
    }

    #[test]
    fn open_empty_url_reports_error() {
        let (_dir, mut app) = unlocked_app(Vec::new());
        app.open_url("");
        assert_eq!(status_text(&app), "no URL");
        assert!(status_kind(&app) == Some(StatusKind::Error));
    }

    #[test]
    fn copy_without_clipboard_reports_error() {
        let (_dir, mut app) = unlocked_app(Vec::new());
        app.copy_value("value", "label");
        assert_eq!(status_text(&app), "clipboard unavailable");
        assert!(app.clipboard_clear_at.is_none());
    }

    #[test]
    fn navigation_helpers_switch_screens() {
        let (_dir, mut app) = unlocked_app(Vec::new());
        app.open_detail("abc".into());
        assert!(matches!(&app.screen, Screen::Detail(d) if d.id == "abc" && !d.reveal));
        app.start_new();
        assert!(matches!(&app.screen, Screen::Form(f) if matches!(f.mode, FormMode::Create)));
        app.back_to_list();
        assert!(matches!(app.screen, Screen::List(_)));
    }

    #[test]
    fn start_edit_loads_item_into_form() {
        let mut item = new_item("Alpha", "work");
        item.links = vec![link("docs", "", "")];
        let (_dir, mut app) = unlocked_app(vec![item]);
        let id = item_id(&app, "Alpha");
        app.start_edit(&id).unwrap();
        let f = form_mut(&mut app);
        assert!(matches!(&f.mode, FormMode::Edit(e) if *e == id));
        assert_eq!(f.title, "Alpha");
        assert_eq!(f.group, "work");
        assert_eq!(f.links.len(), 1);
    }

    #[test]
    fn start_edit_errors() {
        let (_dir, mut app) = unlocked_app(Vec::new());
        assert!(matches!(
            app.start_edit("missing"),
            Err(VltError::ItemNotFound(_))
        ));
        app.vault = None;
        assert!(matches!(
            app.start_edit("missing"),
            Err(VltError::NotInitialized)
        ));
    }

    #[test]
    fn save_form_requires_title_and_password() {
        let (_dir, mut app) = unlocked_app(Vec::new());
        app.start_new();
        app.save_form();
        assert_eq!(form_error(&app).as_deref(), Some("title is required"));
        form_mut(&mut app).title = "Alpha".into();
        app.save_form();
        assert_eq!(form_error(&app).as_deref(), Some("password is required"));
    }

    #[test]
    fn save_form_creates_and_persists_item() {
        let (dir, mut app) = unlocked_app(Vec::new());
        app.start_new();
        let f = form_mut(&mut app);
        f.title = "Alpha".into();
        f.password = "pw-123".into();
        f.group = " team ".into();
        app.save_form();
        assert!(matches!(app.screen, Screen::Detail(_)));
        assert_eq!(status_text(&app), "saved");
        let reopened = Vault::open_vault(&dir.vault_path(), MASTER).unwrap();
        assert_eq!(reopened.items().len(), 1);
        assert_eq!(reopened.items()[0].group, "team");
    }

    #[test]
    fn save_form_updates_existing_item() {
        let (dir, mut app) = unlocked_app(vec![new_item("Alpha", "")]);
        let id = item_id(&app, "Alpha");
        app.start_edit(&id).unwrap();
        form_mut(&mut app).title = "Beta".into();
        app.save_form();
        assert!(matches!(&app.screen, Screen::Detail(d) if d.id == id));
        let reopened = Vault::open_vault(&dir.vault_path(), MASTER).unwrap();
        assert_eq!(reopened.items()[0].title, "Beta");
    }

    #[test]
    fn save_form_reports_missing_item_and_locked_vault() {
        let (_dir, mut app) = unlocked_app(vec![new_item("Alpha", "")]);
        let id = item_id(&app, "Alpha");
        app.start_edit(&id).unwrap();
        form_mut(&mut app).mode = FormMode::Edit("gone".into());
        app.save_form();
        assert!(form_error(&app).unwrap().contains("item not found"));
        app.vault = None;
        app.save_form();
        assert_eq!(
            form_error(&app).as_deref(),
            Some("vault is not initialized")
        );
    }

    #[test]
    fn save_form_ignores_other_screens() {
        let (_dir, mut app) = unlocked_app(Vec::new());
        app.save_form();
        assert!(matches!(app.screen, Screen::List(_)));
    }

    #[test]
    fn delete_item_persists_and_errors() {
        let (dir, mut app) = unlocked_app(vec![new_item("Alpha", "")]);
        let id = item_id(&app, "Alpha");
        app.delete_item(&id).unwrap();
        assert!(
            Vault::open_vault(&dir.vault_path(), MASTER)
                .unwrap()
                .items()
                .is_empty()
        );
        assert!(app.delete_item(&id).is_err());
        app.vault = None;
        assert!(matches!(
            app.delete_item(&id),
            Err(VltError::NotInitialized)
        ));
    }

    #[test]
    fn visible_rows_builds_sorted_tree() {
        let (_dir, app) = unlocked_app(vec![
            new_item("zeta", ""),
            new_item("Alpha", ""),
            new_item("Deploy", "work/ci"),
            new_item("Mail", "work"),
            new_item("Bank", " home // money "),
        ]);
        let labels = row_labels(&app, &default_list());
        assert_eq!(
            labels,
            [
                "0G:home:1:true",
                "1G:home/money:1:true",
                "3I:Bank",
                "0G:work:2:true",
                "1G:work/ci:1:true",
                "3I:Deploy",
                "2I:Mail",
                "1I:Alpha",
                "1I:zeta",
            ]
        );
    }

    #[test]
    fn visible_rows_hides_collapsed_groups() {
        let (_dir, app) = unlocked_app(vec![new_item("Mail", "work"), new_item("Solo", "")]);
        let mut list = default_list();
        list.collapsed.insert("work".into());
        assert_eq!(row_labels(&app, &list), ["0G:work:1:false", "1I:Solo"]);
    }

    #[test]
    fn visible_rows_search_filters_and_expands() {
        let mut ci = new_item("Deploy", "work/ci");
        ci.url = "https://ci.example.com".into();
        let (_dir, app) = unlocked_app(vec![ci, new_item("Mail", "work"), new_item("Solo", "")]);
        let mut list = default_list();
        list.collapsed.insert("work".into());
        list.search = " CI.EXAMPLE ".into();
        assert_eq!(
            row_labels(&app, &list),
            ["0G:work:1:true", "1G:work/ci:1:true", "3I:Deploy"]
        );
        list.search = "solo-user".into();
        assert_eq!(row_labels(&app, &list), ["1I:Solo"]);
        list.search = "work".into();
        assert_eq!(row_labels(&app, &list).len(), 4);
    }

    #[test]
    fn visible_rows_without_vault_is_empty() {
        let (_dir, app) = setup_app();
        assert!(app.visible_rows(&default_list()).is_empty());
    }

    #[test]
    fn session_outcome_labels() {
        assert!(session_outcome_label(session::SaveOutcome::Encrypted).contains("encrypted"));
        assert!(session_outcome_label(session::SaveOutcome::Plaintext).contains("plaintext"));
        assert!(session_outcome_label(session::SaveOutcome::Failed).contains("failed"));
    }
}

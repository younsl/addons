use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{
    App, FormFocus, FormMode, GenState, ListState, RenderRow, RowKind, Screen, SetupFocus,
    StatusKind,
};
use crate::crypto;

pub fn handle(app: &mut App, key: KeyEvent) {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.should_quit = true;
        return;
    }
    if app.show_help {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('?' | 'q')) {
            app.show_help = false;
        }
        return;
    }
    if let Screen::List(l) = &app.screen
        && l.confirm_delete_id.is_some()
    {
        return handle_confirm_delete(app, key);
    }
    if key.code == KeyCode::Char('?') && !typing_screen(app) {
        app.show_help = true;
        return;
    }
    match &app.screen {
        Screen::Setup(_) => handle_setup(app, key),
        Screen::Unlock(_) => handle_unlock(app, key),
        Screen::List(_) => handle_list(app, key),
        Screen::Detail(_) => handle_detail(app, key),
        Screen::Form(_) => handle_form(app, key),
        Screen::Generator(_) => handle_generator(app, key),
    }
}

const fn typing_screen(app: &App) -> bool {
    matches!(
        app.screen,
        Screen::Setup(_) | Screen::Unlock(_) | Screen::Form(_)
    ) || matches!(&app.screen, Screen::List(l) if l.searching)
}

fn clone_list(l: &ListState) -> ListState {
    ListState {
        selected: l.selected,
        search: l.search.clone(),
        searching: l.searching,
        collapsed: l.collapsed.clone(),
        confirm_delete_id: l.confirm_delete_id.clone(),
    }
}

fn handle_setup(app: &mut App, key: KeyEvent) {
    let Screen::Setup(s) = &mut app.screen else {
        return;
    };
    match key.code {
        KeyCode::Esc => app.should_quit = true,
        KeyCode::Tab | KeyCode::Down | KeyCode::BackTab | KeyCode::Up => {
            s.focus = match s.focus {
                SetupFocus::Password => SetupFocus::Confirm,
                SetupFocus::Confirm => SetupFocus::Password,
            }
        }
        KeyCode::Enter => {
            if s.focus == SetupFocus::Password {
                s.focus = SetupFocus::Confirm;
            } else {
                app.try_setup();
            }
        }
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            s.reveal = !s.reveal;
        }
        KeyCode::Backspace => match s.focus {
            SetupFocus::Password => {
                s.password.pop();
            }
            SetupFocus::Confirm => {
                s.confirm.pop();
            }
        },
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => match s.focus {
            SetupFocus::Password => s.password.push(c),
            SetupFocus::Confirm => s.confirm.push(c),
        },
        _ => {}
    }
}

fn handle_unlock(app: &mut App, key: KeyEvent) {
    let Screen::Unlock(s) = &mut app.screen else {
        return;
    };
    match key.code {
        KeyCode::Esc => app.should_quit = true,
        KeyCode::Enter => app.try_unlock(),
        KeyCode::Backspace => {
            s.password.pop();
        }
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            s.reveal = !s.reveal;
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => s.password.push(c),
        _ => {}
    }
}

#[allow(clippy::too_many_lines)]
fn handle_list(app: &mut App, key: KeyEvent) {
    if matches!(&app.screen, Screen::List(l) if l.searching) {
        return handle_search_input(app, key);
    }

    // Snapshot what we need from the list state and the visible rows so we can
    // mutate `app` later without borrow conflicts.
    let (rows_len, cur_row, all_rows): (usize, Option<RenderRow>, Vec<RenderRow>) = {
        let Screen::List(l) = &app.screen else {
            return;
        };
        let snap = clone_list(l);
        let rows = app.visible_rows(&snap);
        let cur = rows.get(snap.selected).cloned();
        (rows.len(), cur, rows)
    };

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => {
            if let Screen::List(l) = &mut app.screen {
                if l.search.is_empty() {
                    app.should_quit = true;
                } else {
                    l.search.clear();
                    l.selected = 0;
                }
            }
        }
        KeyCode::Char('j') | KeyCode::Down => {
            if rows_len > 0
                && let Screen::List(l) = &mut app.screen
            {
                l.selected = (l.selected + 1).min(rows_len - 1);
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if let Screen::List(l) = &mut app.screen {
                l.selected = l.selected.saturating_sub(1);
            }
        }
        KeyCode::PageDown => {
            if rows_len > 0
                && let Screen::List(l) = &mut app.screen
            {
                l.selected = (l.selected + 10).min(rows_len - 1);
            }
        }
        KeyCode::PageUp => {
            if let Screen::List(l) = &mut app.screen {
                l.selected = l.selected.saturating_sub(10);
            }
        }
        KeyCode::Char('g') | KeyCode::Home => {
            if let Screen::List(l) = &mut app.screen {
                l.selected = 0;
            }
        }
        KeyCode::Char('G') | KeyCode::End => {
            if rows_len > 0
                && let Screen::List(l) = &mut app.screen
            {
                l.selected = rows_len - 1;
            }
        }
        KeyCode::Right => {
            // Expand current group, or no-op on item.
            if let Some(row) = &cur_row
                && let RowKind::Group { path, .. } = &row.kind
                && let Screen::List(l) = &mut app.screen
            {
                l.collapsed.remove(path);
            }
        }
        KeyCode::Left => {
            // Collapse current group, or jump cursor up to its parent group on item rows.
            match cur_row.as_ref().map(|r| &r.kind) {
                Some(RowKind::Group { path, .. }) => {
                    if let Screen::List(l) = &mut app.screen {
                        l.collapsed.insert(path.clone());
                    }
                }
                Some(RowKind::Item(_)) => {
                    if let Screen::List(l) = &mut app.screen {
                        let cur = l.selected;
                        for i in (0..cur).rev() {
                            if matches!(
                                all_rows.get(i).map(|r| &r.kind),
                                Some(RowKind::Group { .. })
                            ) {
                                l.selected = i;
                                break;
                            }
                        }
                    }
                }
                None => {}
            }
        }
        KeyCode::Char(':') => {
            if let Screen::List(l) = &mut app.screen {
                l.searching = true;
            }
        }
        KeyCode::Char('n') => app.start_new(),
        KeyCode::Char('p') => app.screen = Screen::Generator(GenState::new()),
        KeyCode::Char('L') => {
            app.lock_vault();
            app.set_status("locked", StatusKind::Info);
        }
        KeyCode::Char(' ') | KeyCode::Tab => {
            if let Some(row) = &cur_row
                && let RowKind::Group { path, .. } = &row.kind
                && let Screen::List(l) = &mut app.screen
            {
                if l.collapsed.contains(path) {
                    l.collapsed.remove(path);
                } else {
                    l.collapsed.insert(path.clone());
                }
            }
        }
        KeyCode::Enter => match cur_row.as_ref().map(|r| &r.kind) {
            Some(RowKind::Item(it)) => {
                let id = it.id.clone();
                app.open_detail(id);
            }
            Some(RowKind::Group { path, .. }) => {
                if let Screen::List(l) = &mut app.screen {
                    if l.collapsed.contains(path) {
                        l.collapsed.remove(path);
                    } else {
                        l.collapsed.insert(path.clone());
                    }
                }
            }
            None => {}
        },
        KeyCode::Char('e') => {
            if let Some(row) = &cur_row
                && let RowKind::Item(it) = &row.kind
            {
                let id = it.id.clone();
                if let Err(e) = app.start_edit(&id) {
                    app.set_status(e.to_string(), StatusKind::Error);
                }
            }
        }
        KeyCode::Char('d') => {
            if let Some(row) = &cur_row
                && let RowKind::Item(it) = &row.kind
            {
                let id = it.id.clone();
                if let Screen::List(l) = &mut app.screen {
                    l.confirm_delete_id = Some(id);
                }
            }
        }
        KeyCode::Char('c') => {
            if let Some(row) = &cur_row
                && let RowKind::Item(it) = &row.kind
            {
                let id = it.id.clone();
                let pwd = app
                    .vault
                    .as_ref()
                    .and_then(|v| v.find_item(&id).ok().map(|i| i.password.clone()));
                if let Some(p) = pwd {
                    app.copy_value(&p, "password");
                }
            }
        }
        KeyCode::Char('y') => {
            if let Some(row) = &cur_row
                && let RowKind::Item(it) = &row.kind
            {
                let user = it.username.clone();
                app.copy_value(&user, "username");
            }
        }
        _ => {}
    }
}

fn handle_search_input(app: &mut App, key: KeyEvent) {
    let Screen::List(l) = &mut app.screen else {
        return;
    };
    match key.code {
        KeyCode::Esc => {
            l.searching = false;
            l.search.clear();
            l.selected = 0;
        }
        KeyCode::Enter | KeyCode::Down => {
            l.searching = false;
            l.selected = 0;
        }
        KeyCode::Backspace => {
            l.search.pop();
            l.selected = 0;
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            l.search.push(c);
            l.selected = 0;
        }
        _ => {}
    }
}

fn handle_confirm_delete(app: &mut App, key: KeyEvent) {
    let id = match &app.screen {
        Screen::List(l) => l.confirm_delete_id.clone(),
        _ => return,
    };
    let Some(id) = id else { return };
    match key.code {
        KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
            match app.delete_item(&id) {
                Ok(()) => app.set_status("deleted", StatusKind::Info),
                Err(e) => app.set_status(e.to_string(), StatusKind::Error),
            }
            if let Screen::List(l) = &mut app.screen {
                l.confirm_delete_id = None;
                l.selected = 0;
            }
        }
        _ => {
            if let Screen::List(l) = &mut app.screen {
                l.confirm_delete_id = None;
            }
        }
    }
}

fn handle_detail(app: &mut App, key: KeyEvent) {
    let Screen::Detail(d) = &app.screen else {
        return;
    };
    let id = d.id.clone();
    let pending_g = d.pending_g;
    let Some(item) = app
        .vault
        .as_ref()
        .and_then(|v| v.find_item(&id).ok().cloned())
    else {
        app.back_to_list();
        return;
    };

    // Always clear the pending `g` prefix; specific handlers below set it again.
    if let Screen::Detail(d) = &mut app.screen {
        d.pending_g = false;
    }

    // `g`-prefix chords (vim convention).
    //   `gx`        → open the primary URL
    //   `g{1..=9}`  → open the Nth registered link
    if pending_g {
        match key.code {
            KeyCode::Char('x') => app.open_url(&item.url),
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let n = c.to_digit(10).unwrap_or(0) as usize;
                if (1..=9).contains(&n)
                    && let Some(link) = item.links.get(n - 1)
                {
                    let url = link.url.clone();
                    app.open_url(&url);
                }
            }
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => app.back_to_list(),
        KeyCode::Char('r') => {
            if let Screen::Detail(d) = &mut app.screen {
                d.reveal = !d.reveal;
            }
        }
        KeyCode::Char('g') => {
            if let Screen::Detail(d) = &mut app.screen {
                d.pending_g = true;
            }
        }
        KeyCode::Char('c') => app.copy_value(&item.password, "password"),
        KeyCode::Char('y') => app.copy_value(&item.username, "username"),
        KeyCode::Char('u') => app.copy_value(&item.url, "url"),
        KeyCode::Char(c) if c.is_ascii_digit() => {
            let n = c.to_digit(10).unwrap_or(0) as usize;
            if (1..=9).contains(&n)
                && let Some(link) = item.links.get(n - 1)
            {
                let url = link.url.clone();
                app.copy_value(&url, &format!("link {n} url"));
            }
        }
        KeyCode::Char('e') => {
            if let Err(e) = app.start_edit(&id) {
                app.set_status(e.to_string(), StatusKind::Error);
            }
        }
        KeyCode::Char('d') => {
            app.back_to_list();
            if let Screen::List(l) = &mut app.screen {
                l.confirm_delete_id = Some(id);
            }
        }
        KeyCode::Char('L') => {
            app.lock_vault();
            app.set_status("locked", StatusKind::Info);
        }
        _ => {}
    }
}

fn handle_form(app: &mut App, key: KeyEvent) {
    let Screen::Form(f) = &mut app.screen else {
        return;
    };
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('s') => {
                app.save_form();
                return;
            }
            KeyCode::Char('r') => {
                f.reveal = !f.reveal;
                return;
            }
            KeyCode::Char('g') => {
                f.password = crypto::generate_password(24, true, true);
                return;
            }
            KeyCode::Char('n') => {
                f.add_link();
                return;
            }
            KeyCode::Char('x') => {
                f.delete_focused_link();
                return;
            }
            _ => {}
        }
    }
    match key.code {
        KeyCode::Esc => match &f.mode {
            FormMode::Create => app.back_to_list(),
            FormMode::Edit(id) => {
                let id = id.clone();
                app.open_detail(id);
            }
        },
        KeyCode::Tab | KeyCode::Down => f.focus_next(),
        KeyCode::BackTab | KeyCode::Up => f.focus_prev(),
        KeyCode::Enter => match f.focus {
            FormFocus::Notes => f.notes.push('\n'),
            FormFocus::AddLink => f.add_link(),
            _ => f.focus_next(),
        },
        KeyCode::Backspace => {
            if let Some(v) = f.current_value_mut() {
                v.pop();
            }
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(v) = f.current_value_mut() {
                v.push(c);
            }
        }
        _ => {}
    }
}

fn handle_generator(app: &mut App, key: KeyEvent) {
    let Screen::Generator(g) = &mut app.screen else {
        return;
    };
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => app.back_to_list(),
        KeyCode::Char('+' | '=' | 'l') | KeyCode::Right => {
            g.length = (g.length + 1).min(128);
            g.regenerate();
        }
        KeyCode::Char('-' | 'h') | KeyCode::Left => {
            g.length = g.length.saturating_sub(1).max(4);
            g.regenerate();
        }
        KeyCode::Char('s') => {
            g.symbols = !g.symbols;
            g.regenerate();
        }
        KeyCode::Char('n') => {
            g.numbers = !g.numbers;
            g.regenerate();
        }
        KeyCode::Char('g') | KeyCode::Enter => g.regenerate(),
        KeyCode::Char('c' | 'y') => {
            let out = g.output.clone();
            app.copy_value(&out, "password");
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{DetailState, FormState, SetupState, UnlockState};
    use crate::test_support::{
        TempDir, ch, ctrl, item_id, key, link, locked_app, master, new_item, random_secret,
        setup_app, unlocked_app,
    };
    use crate::vault::NewItem;

    fn press(app: &mut App, keys: &[KeyEvent]) {
        for k in keys {
            handle(app, *k);
        }
    }

    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            handle(app, ch(c));
        }
    }

    fn status_text(app: &App) -> String {
        app.status.as_ref().map(|s| s.0.clone()).unwrap_or_default()
    }

    fn setup(app: &App) -> &SetupState {
        match &app.screen {
            Screen::Setup(s) => s,
            _ => panic!("expected setup screen"),
        }
    }

    fn unlock(app: &App) -> &UnlockState {
        match &app.screen {
            Screen::Unlock(s) => s,
            _ => panic!("expected unlock screen"),
        }
    }

    fn list(app: &App) -> &ListState {
        match &app.screen {
            Screen::List(l) => l,
            _ => panic!("expected list screen"),
        }
    }

    fn list_mut(app: &mut App) -> &mut ListState {
        match &mut app.screen {
            Screen::List(l) => l,
            _ => panic!("expected list screen"),
        }
    }

    fn detail(app: &App) -> &DetailState {
        match &app.screen {
            Screen::Detail(d) => d,
            _ => panic!("expected detail screen"),
        }
    }

    fn form(app: &App) -> &FormState {
        match &app.screen {
            Screen::Form(f) => f,
            _ => panic!("expected form screen"),
        }
    }

    fn generator(app: &App) -> &GenState {
        match &app.screen {
            Screen::Generator(g) => g,
            _ => panic!("expected generator screen"),
        }
    }

    fn grouped_app() -> (TempDir, App) {
        unlocked_app(vec![
            new_item("Mail", "work"),
            new_item("Wiki", "work"),
            new_item("Solo", ""),
        ])
    }

    fn detail_app(item: NewItem) -> (TempDir, App) {
        let title = item.title.clone();
        let (dir, mut app) = unlocked_app(vec![item]);
        let id = item_id(&app, &title);
        app.open_detail(id);
        (dir, app)
    }

    #[test]
    fn ctrl_c_quits_everywhere() {
        let (_dir, mut app) = setup_app();
        handle(&mut app, ctrl('c'));
        assert!(app.should_quit);
    }

    #[test]
    fn help_opens_and_closes() {
        let (_dir, mut app) = grouped_app();
        handle(&mut app, ch('?'));
        assert!(app.show_help);
        handle(&mut app, ch('j'));
        assert!(app.show_help);
        assert_eq!(list(&app).selected, 0);
        handle(&mut app, key(KeyCode::Esc));
        assert!(!app.show_help);
        assert!(!app.should_quit);
    }

    #[test]
    fn question_mark_is_typed_on_input_screens() {
        let (_dir, mut app) = setup_app();
        handle(&mut app, ch('?'));
        assert!(!app.show_help);
        assert_eq!(setup(&app).password, "?");
    }

    #[test]
    fn setup_typing_and_focus() {
        let (_dir, mut app) = setup_app();
        type_str(&mut app, "abc");
        handle(&mut app, key(KeyCode::Backspace));
        handle(&mut app, key(KeyCode::Tab));
        type_str(&mut app, "xyz");
        handle(&mut app, key(KeyCode::Backspace));
        handle(&mut app, ctrl('r'));
        handle(&mut app, ctrl('z'));
        handle(&mut app, key(KeyCode::F(1)));
        let s = setup(&app);
        assert_eq!(s.password, "ab");
        assert_eq!(s.confirm, "xy");
        assert!(s.focus == SetupFocus::Confirm);
        assert!(s.reveal);
        handle(&mut app, key(KeyCode::Up));
        assert!(setup(&app).focus == SetupFocus::Password);
    }

    #[test]
    fn setup_enter_moves_focus_then_submits() {
        let (dir, mut app) = setup_app();
        type_str(&mut app, master());
        handle(&mut app, key(KeyCode::Enter));
        assert!(setup(&app).focus == SetupFocus::Confirm);
        type_str(&mut app, master());
        handle(&mut app, key(KeyCode::Enter));
        assert!(matches!(app.screen, Screen::List(_)));
        assert!(dir.vault_path().exists());
    }

    #[test]
    fn setup_esc_quits() {
        let (_dir, mut app) = setup_app();
        handle(&mut app, key(KeyCode::Esc));
        assert!(app.should_quit);
    }

    #[test]
    fn unlock_typing_and_reveal() {
        let (_dir, mut app) = locked_app();
        type_str(&mut app, "abcd");
        handle(&mut app, key(KeyCode::Backspace));
        handle(&mut app, ctrl('r'));
        handle(&mut app, ctrl('z'));
        handle(&mut app, key(KeyCode::F(2)));
        let s = unlock(&app);
        assert_eq!(s.password, "abc");
        assert!(s.reveal);
    }

    #[test]
    fn unlock_enter_with_wrong_then_right_password() {
        let (_dir, mut app) = locked_app();
        type_str(&mut app, &random_secret(16));
        handle(&mut app, key(KeyCode::Enter));
        assert!(unlock(&app).error.is_some());
        if let Screen::Unlock(s) = &mut app.screen {
            s.password.clear();
        }
        type_str(&mut app, master());
        handle(&mut app, key(KeyCode::Enter));
        assert!(matches!(app.screen, Screen::List(_)));
    }

    #[test]
    fn unlock_esc_quits() {
        let (_dir, mut app) = locked_app();
        handle(&mut app, key(KeyCode::Esc));
        assert!(app.should_quit);
    }

    #[test]
    fn list_quit_and_clear_search() {
        let (_dir, mut app) = grouped_app();
        list_mut(&mut app).search = "mail".into();
        list_mut(&mut app).selected = 1;
        handle(&mut app, ch('q'));
        assert!(!app.should_quit);
        assert_eq!(list(&app).search, "");
        assert_eq!(list(&app).selected, 0);
        handle(&mut app, key(KeyCode::Esc));
        assert!(app.should_quit);
    }

    #[test]
    fn list_vertical_navigation() {
        let (_dir, mut app) = grouped_app();
        press(&mut app, &[ch('j'), key(KeyCode::Down)]);
        assert_eq!(list(&app).selected, 2);
        press(&mut app, &[ch('j'), ch('j'), ch('j')]);
        assert_eq!(list(&app).selected, 3);
        press(&mut app, &[ch('k'), key(KeyCode::Up)]);
        assert_eq!(list(&app).selected, 1);
        handle(&mut app, key(KeyCode::PageDown));
        assert_eq!(list(&app).selected, 3);
        handle(&mut app, key(KeyCode::PageUp));
        assert_eq!(list(&app).selected, 0);
        handle(&mut app, ch('G'));
        assert_eq!(list(&app).selected, 3);
        handle(&mut app, ch('g'));
        assert_eq!(list(&app).selected, 0);
        handle(&mut app, key(KeyCode::End));
        assert_eq!(list(&app).selected, 3);
        handle(&mut app, key(KeyCode::Home));
        assert_eq!(list(&app).selected, 0);
    }

    #[test]
    fn list_navigation_on_empty_vault_stays_put() {
        let (_dir, mut app) = unlocked_app(Vec::new());
        press(
            &mut app,
            &[
                ch('j'),
                key(KeyCode::PageDown),
                ch('G'),
                key(KeyCode::Left),
                key(KeyCode::Right),
                key(KeyCode::Enter),
                ch('e'),
                ch('d'),
                ch('c'),
                ch('y'),
                ch(' '),
                key(KeyCode::F(5)),
            ],
        );
        let l = list(&app);
        assert_eq!(l.selected, 0);
        assert!(l.confirm_delete_id.is_none());
        assert!(app.status.is_none());
    }

    #[test]
    fn list_collapse_and_expand_groups() {
        let (_dir, mut app) = grouped_app();
        handle(&mut app, key(KeyCode::Left));
        assert!(list(&app).collapsed.contains("work"));
        handle(&mut app, key(KeyCode::Right));
        assert!(list(&app).collapsed.is_empty());
        handle(&mut app, ch(' '));
        assert!(list(&app).collapsed.contains("work"));
        handle(&mut app, key(KeyCode::Tab));
        assert!(list(&app).collapsed.is_empty());
        handle(&mut app, key(KeyCode::Enter));
        assert!(list(&app).collapsed.contains("work"));
        handle(&mut app, key(KeyCode::Enter));
        assert!(list(&app).collapsed.is_empty());
    }

    #[test]
    fn list_left_on_item_jumps_to_parent_group() {
        let (_dir, mut app) = grouped_app();
        list_mut(&mut app).selected = 2;
        handle(&mut app, key(KeyCode::Left));
        assert_eq!(list(&app).selected, 0);
        list_mut(&mut app).selected = 2;
        handle(&mut app, key(KeyCode::Right));
        assert_eq!(list(&app).selected, 2);
    }

    #[test]
    fn list_left_on_root_item_without_group_stays() {
        let (_dir, mut app) = unlocked_app(vec![new_item("Solo", "")]);
        handle(&mut app, key(KeyCode::Left));
        assert_eq!(list(&app).selected, 0);
    }

    #[test]
    fn list_search_mode_editing() {
        let (_dir, mut app) = grouped_app();
        handle(&mut app, ch(':'));
        assert!(list(&app).searching);
        type_str(&mut app, "wiki?");
        handle(&mut app, key(KeyCode::Backspace));
        handle(&mut app, ctrl('a'));
        handle(&mut app, key(KeyCode::F(3)));
        assert_eq!(list(&app).search, "wiki");
        assert!(!app.show_help);
        handle(&mut app, key(KeyCode::Enter));
        assert!(!list(&app).searching);
        assert_eq!(list(&app).search, "wiki");

        handle(&mut app, ch(':'));
        handle(&mut app, key(KeyCode::Down));
        assert!(!list(&app).searching);

        handle(&mut app, ch(':'));
        handle(&mut app, key(KeyCode::Esc));
        assert!(!list(&app).searching);
        assert_eq!(list(&app).search, "");
    }

    #[test]
    fn list_screen_switches() {
        let (_dir, mut app) = grouped_app();
        handle(&mut app, ch('n'));
        assert!(matches!(app.screen, Screen::Form(_)));
        app.back_to_list();
        handle(&mut app, ch('p'));
        assert!(matches!(app.screen, Screen::Generator(_)));
        app.back_to_list();
        handle(&mut app, ch('L'));
        assert!(matches!(app.screen, Screen::Unlock(_)));
        assert_eq!(status_text(&app), "locked");
    }

    #[test]
    fn list_item_actions() {
        let (_dir, mut app) = grouped_app();
        list_mut(&mut app).selected = 1;
        handle(&mut app, ch('c'));
        assert_eq!(status_text(&app), "clipboard unavailable");
        app.status = None;
        handle(&mut app, ch('y'));
        assert_eq!(status_text(&app), "clipboard unavailable");

        handle(&mut app, ch('e'));
        assert_eq!(form(&app).title, "Mail");
        app.back_to_list();
        list_mut(&mut app).selected = 1;
        handle(&mut app, key(KeyCode::Enter));
        assert_eq!(detail(&app).id, item_id(&app, "Mail"));
    }

    #[test]
    fn list_keys_without_vault_do_nothing() {
        let (_dir, mut app) = grouped_app();
        list_mut(&mut app).selected = 1;
        app.vault = None;
        press(&mut app, &[ch('e'), ch('d'), ch('c')]);
        assert!(app.status.is_none());
        assert!(matches!(app.screen, Screen::List(_)));
    }

    #[test]
    fn list_delete_confirm_and_cancel() {
        let (_dir, mut app) = grouped_app();
        list_mut(&mut app).selected = 1;
        handle(&mut app, ch('d'));
        assert!(list(&app).confirm_delete_id.is_some());
        handle(&mut app, ch('?'));
        assert!(list(&app).confirm_delete_id.is_none());
        assert!(!app.show_help);

        handle(&mut app, ch('d'));
        handle(&mut app, ch('y'));
        assert_eq!(status_text(&app), "deleted");
        assert_eq!(app.vault.as_ref().unwrap().items().len(), 2);
        assert_eq!(list(&app).selected, 0);
    }

    #[test]
    fn list_delete_of_missing_item_reports_error() {
        let (_dir, mut app) = grouped_app();
        list_mut(&mut app).confirm_delete_id = Some("missing".into());
        handle(&mut app, key(KeyCode::Enter));
        assert!(status_text(&app).contains("item not found"));
        assert!(list(&app).confirm_delete_id.is_none());
    }

    #[test]
    fn detail_with_missing_item_returns_to_list() {
        let (_dir, mut app) = unlocked_app(Vec::new());
        app.open_detail("missing".into());
        handle(&mut app, ch('r'));
        assert!(matches!(app.screen, Screen::List(_)));
    }

    #[test]
    fn detail_back_keys() {
        for k in [key(KeyCode::Esc), ch('q'), key(KeyCode::Backspace)] {
            let (_dir, mut app) = detail_app(new_item("Alpha", ""));
            handle(&mut app, k);
            assert!(matches!(app.screen, Screen::List(_)));
        }
    }

    #[test]
    fn detail_reveal_toggle() {
        let (_dir, mut app) = detail_app(new_item("Alpha", ""));
        handle(&mut app, ch('r'));
        assert!(detail(&app).reveal);
        handle(&mut app, ch('r'));
        assert!(!detail(&app).reveal);
    }

    #[test]
    fn detail_copy_keys_need_clipboard() {
        let mut item = new_item("Alpha", "");
        item.links = vec![link("docs", "", "https://example.com/docs")];
        let (_dir, mut app) = detail_app(item);
        for k in [ch('c'), ch('y'), ch('u'), ch('1')] {
            app.status = None;
            handle(&mut app, k);
            assert_eq!(status_text(&app), "clipboard unavailable");
        }
        app.status = None;
        press(&mut app, &[ch('0'), ch('2'), key(KeyCode::F(4))]);
        assert!(app.status.is_none());
    }

    #[test]
    fn detail_g_chords() {
        let mut item = new_item("Alpha", "");
        item.links = vec![link("empty", "", "")];
        let (_dir, mut app) = detail_app(item);
        handle(&mut app, ch('g'));
        assert!(detail(&app).pending_g);
        handle(&mut app, ch('x'));
        assert!(!detail(&app).pending_g);
        assert_eq!(status_text(&app), "no URL");

        app.status = None;
        press(&mut app, &[ch('g'), ch('1')]);
        assert_eq!(status_text(&app), "no URL");

        app.status = None;
        press(
            &mut app,
            &[ch('g'), ch('5'), ch('g'), ch('0'), ch('g'), ch('z')],
        );
        assert!(app.status.is_none());
        assert!(!detail(&app).pending_g);
    }

    #[test]
    fn detail_edit_delete_and_lock() {
        let (_dir, mut app) = detail_app(new_item("Alpha", ""));
        let id = item_id(&app, "Alpha");
        handle(&mut app, ch('e'));
        assert!(matches!(&form(&app).mode, FormMode::Edit(e) if *e == id));

        app.open_detail(id.clone());
        handle(&mut app, ch('d'));
        assert_eq!(list(&app).confirm_delete_id.as_deref(), Some(id.as_str()));

        app.open_detail(id);
        handle(&mut app, ch('L'));
        assert!(matches!(app.screen, Screen::Unlock(_)));
    }

    #[test]
    fn form_typing_and_navigation() {
        let (_dir, mut app) = unlocked_app(Vec::new());
        app.start_new();
        type_str(&mut app, "Titlex");
        handle(&mut app, key(KeyCode::Backspace));
        handle(&mut app, key(KeyCode::Enter));
        assert_eq!(form(&app).focus, FormFocus::Username);
        handle(&mut app, key(KeyCode::Down));
        handle(&mut app, key(KeyCode::Tab));
        handle(&mut app, key(KeyCode::BackTab));
        handle(&mut app, key(KeyCode::Up));
        assert_eq!(form(&app).focus, FormFocus::Username);
        handle(&mut app, ctrl('a'));
        handle(&mut app, key(KeyCode::F(6)));
        assert_eq!(form(&app).title, "Title");
        assert_eq!(form(&app).username, "");
    }

    #[test]
    fn form_notes_enter_inserts_newline() {
        let (_dir, mut app) = unlocked_app(Vec::new());
        app.start_new();
        if let Screen::Form(f) = &mut app.screen {
            f.focus = FormFocus::Notes;
        }
        type_str(&mut app, "a");
        handle(&mut app, key(KeyCode::Enter));
        type_str(&mut app, "b");
        assert_eq!(form(&app).notes, "a\nb");
    }

    #[test]
    fn form_link_management() {
        let (_dir, mut app) = unlocked_app(Vec::new());
        app.start_new();
        if let Screen::Form(f) = &mut app.screen {
            f.focus = FormFocus::AddLink;
        }
        type_str(&mut app, "z");
        handle(&mut app, key(KeyCode::Backspace));
        handle(&mut app, key(KeyCode::Enter));
        assert_eq!(form(&app).focus, FormFocus::LinkName(0));
        handle(&mut app, ctrl('n'));
        assert_eq!(form(&app).links.len(), 2);
        handle(&mut app, ctrl('x'));
        assert_eq!(form(&app).links.len(), 1);
    }

    #[test]
    fn form_ctrl_shortcuts() {
        let (_dir, mut app) = unlocked_app(Vec::new());
        app.start_new();
        handle(&mut app, ctrl('r'));
        assert!(form(&app).reveal);
        handle(&mut app, ctrl('g'));
        assert_eq!(form(&app).password.len(), 24);
        handle(&mut app, ctrl('s'));
        assert_eq!(form(&app).error.as_deref(), Some("title is required"));
        type_str(&mut app, "Alpha");
        handle(&mut app, ctrl('s'));
        assert!(matches!(app.screen, Screen::Detail(_)));
    }

    #[test]
    fn form_esc_returns_to_origin() {
        let (_dir, mut app) = unlocked_app(vec![new_item("Alpha", "")]);
        app.start_new();
        handle(&mut app, key(KeyCode::Esc));
        assert!(matches!(app.screen, Screen::List(_)));

        let id = item_id(&app, "Alpha");
        app.start_edit(&id).unwrap();
        handle(&mut app, key(KeyCode::Esc));
        assert_eq!(detail(&app).id, id);
    }

    #[test]
    fn generator_length_bounds() {
        let (_dir, mut app) = unlocked_app(Vec::new());
        handle(&mut app, ch('p'));
        press(&mut app, &[ch('+'), ch('='), ch('l'), key(KeyCode::Right)]);
        assert_eq!(generator(&app).length, 28);
        assert_eq!(generator(&app).output.len(), 28);
        press(&mut app, &[ch('-'), ch('h'), key(KeyCode::Left)]);
        assert_eq!(generator(&app).length, 25);
        if let Screen::Generator(g) = &mut app.screen {
            g.length = 128;
        }
        handle(&mut app, ch('+'));
        assert_eq!(generator(&app).length, 128);
        if let Screen::Generator(g) = &mut app.screen {
            g.length = 4;
        }
        handle(&mut app, ch('-'));
        assert_eq!(generator(&app).length, 4);
    }

    #[test]
    fn generator_toggles_and_copy() {
        let (_dir, mut app) = unlocked_app(Vec::new());
        handle(&mut app, ch('p'));
        handle(&mut app, ch('s'));
        handle(&mut app, ch('n'));
        let g = generator(&app);
        assert!(!g.symbols && !g.numbers);
        assert!(g.output.chars().all(|c| c.is_ascii_alphabetic()));
        press(
            &mut app,
            &[ch('g'), key(KeyCode::Enter), key(KeyCode::F(7))],
        );
        assert_eq!(generator(&app).output.len(), 24);
        handle(&mut app, ch('c'));
        assert_eq!(status_text(&app), "clipboard unavailable");
        handle(&mut app, ch('q'));
        assert!(matches!(app.screen, Screen::List(_)));
        handle(&mut app, ch('p'));
        handle(&mut app, key(KeyCode::Esc));
        assert!(matches!(app.screen, Screen::List(_)));
    }
}

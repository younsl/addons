mod backend;
mod format;
mod nav;
mod render;
mod rows;
mod theme;
mod trend;

use std::future::pending;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyEventKind};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use tokio::signal::unix::{SignalKind, signal};

pub use backend::{Backend, DbReplyRx, DbTarget, Watch};
pub use nav::Env;

use crate::collector::Msg;
use crate::db::DbReply;
use nav::{Command, Ui};

const TICK: Duration = Duration::from_millis(500);

type DbResult = Result<(DbReply, String), String>;

/// Owns the channels the UI is waiting on.
struct Runtime<'a> {
    backend: &'a dyn Backend,
    watch: Watch,
    db: Option<(u64, DbReplyRx)>,
}

impl Runtime<'_> {
    /// Returns true when the command asks to quit.
    fn exec(&mut self, cmd: Command) -> bool {
        match cmd {
            Command::None => {}
            Command::Quit => return true,
            Command::Refresh => {
                let _ = self.watch.refresh.try_send(());
            }
            Command::Db {
                seq,
                target,
                request,
            } => self.db = Some((seq, self.backend.db(target, request))),
        }
        false
    }
}

/// Free functions over single fields so `select!` can borrow both channels at once.
async fn next_db(slot: &mut Option<(u64, DbReplyRx)>) -> (u64, DbResult) {
    let result = match slot {
        Some((seq, rx)) => (
            *seq,
            rx.await
                .unwrap_or_else(|_| Err("query was cancelled".into())),
        ),
        None => pending().await,
    };
    *slot = None;
    result
}

async fn next_msg(watch: &mut Watch) -> Option<Msg> {
    watch.rx.recv().await
}

/// Draws until the user quits or SIGTERM arrives.
pub async fn run(
    mut terminal: DefaultTerminal,
    backend: &dyn Backend,
    env: Env,
) -> std::io::Result<()> {
    let mut ui = Ui::new(env);
    let mut rt = Runtime {
        backend,
        watch: backend.watch(),
        db: None,
    };
    let mut input = EventStream::new();
    let mut tick = tokio::time::interval(TICK);
    let mut sigterm = signal(SignalKind::terminate())?;

    loop {
        terminal.draw(|f| render::draw(f, &mut ui))?;
        tokio::select! {
            (seq, result) = next_db(&mut rt.db) => ui.on_db(seq, result),
            msg = next_msg(&mut rt.watch) => match msg {
                Some(m) => ui.on_msg(m),
                None => return Ok(()),
            },
            ev = input.next() => match ev {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                    if rt.exec(ui.handle_key(key)) {
                        return Ok(());
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e),
                None => return Ok(()),
            },
            _ = tick.tick() => {}
            _ = sigterm.recv() => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tokio::sync::{mpsc, oneshot};

    use super::*;
    use crate::db::DbRequest;
    use crate::db::model::KeyPage;

    #[derive(Default)]
    struct FakeBackend {
        queries: Mutex<Vec<DbRequest>>,
    }

    impl Backend for FakeBackend {
        fn watch(&self) -> Watch {
            let (tx, rx) = mpsc::channel(4);
            let (refresh_tx, mut refresh_rx) = mpsc::channel(1);
            let task = tokio::spawn(async move {
                tx.send(Msg::RoundStarted).await.ok();
                while refresh_rx.recv().await.is_some() {
                    tx.send(Msg::RoundStarted).await.ok();
                }
            });
            Watch::new(rx, refresh_tx, task)
        }

        fn db(&self, _target: DbTarget, request: DbRequest) -> DbReplyRx {
            self.queries.lock().unwrap().push(request);
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(Ok((
                DbReply::Keys(KeyPage {
                    cursor: "0".into(),
                    keys: vec![],
                }),
                "ns/pod".into(),
            )));
            rx
        }
    }

    #[tokio::test]
    async fn runtime_executes_commands() {
        let backend = FakeBackend::default();
        let mut rt = Runtime {
            backend: &backend,
            watch: backend.watch(),
            db: None,
        };
        assert!(!rt.exec(Command::None));
        assert!(rt.exec(Command::Quit));
        assert!(matches!(
            next_msg(&mut rt.watch).await,
            Some(Msg::RoundStarted)
        ));
        rt.exec(Command::Refresh);
        assert!(matches!(
            next_msg(&mut rt.watch).await,
            Some(Msg::RoundStarted)
        ));

        let target = DbTarget {
            host: "10.0.0.1".parse().unwrap(),
            gateways: vec![],
        };
        rt.exec(Command::Db {
            seq: 7,
            target,
            request: DbRequest::Info,
        });
        let (seq, result) = next_db(&mut rt.db).await;
        assert_eq!(seq, 7);
        assert_eq!(result.unwrap().1, "ns/pod");
        assert!(rt.db.is_none());
        assert_eq!(backend.queries.lock().unwrap().len(), 1);

        let (tx, rx) = oneshot::channel::<DbResult>();
        drop(tx);
        rt.db = Some((8, rx));
        assert!(next_db(&mut rt.db).await.1.is_err());
    }
}

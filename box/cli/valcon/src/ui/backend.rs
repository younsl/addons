use std::net::IpAddr;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::collector::Msg;
use crate::db::{DbReply, DbRequest};
use crate::types::PodRef;

pub type DbReplyRx = oneshot::Receiver<Result<(DbReply, String), String>>;

/// Where a database request goes: the node, and the pods that can reach it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbTarget {
    pub host: IpAddr,
    pub gateways: Vec<PodRef>,
}

/// The running collector. Dropping it stops the collector.
pub struct Watch {
    pub rx: mpsc::Receiver<Msg>,
    pub refresh: mpsc::Sender<()>,
    task: JoinHandle<()>,
}

impl Watch {
    pub const fn new(
        rx: mpsc::Receiver<Msg>,
        refresh: mpsc::Sender<()>,
        task: JoinHandle<()>,
    ) -> Self {
        Self { rx, refresh, task }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// What the UI needs from the outside world. Calls return channels immediately, so the event
/// loop never waits on the network.
pub trait Backend: Sync {
    fn watch(&self) -> Watch;
    fn db(&self, target: DbTarget, request: DbRequest) -> DbReplyRx;
}

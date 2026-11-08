//! The detection store: the newest `retention` detections in memory,
//! appended to a JSON lines file on the manager volume and compacted when
//! the file grows past twice the retention. Handlers only touch memory and
//! a channel, the writer task owns the file.

use std::collections::VecDeque;
use std::io::{BufRead as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::Deserialize;
use tokio::sync::mpsc;

use crate::policy::{Detection, Severity};

pub const FILE: &str = "detections.jsonl";

#[derive(Debug, Default, Deserialize)]
pub struct Query {
    pub limit: Option<usize>,
    pub severity: Option<String>,
    pub namespace: Option<String>,
    pub node: Option<String>,
    pub engine: Option<String>,
}

pub struct Store {
    items: RwLock<VecDeque<Detection>>,
    retention: usize,
    writer: mpsc::UnboundedSender<Vec<Detection>>,
}

/// Reads the newest `retention` detections of a store file.
pub fn load(path: &Path, retention: usize) -> (VecDeque<Detection>, usize) {
    let mut out = VecDeque::with_capacity(retention.min(100_000));
    let mut lines = 0usize;
    if let Ok(f) = std::fs::File::open(path) {
        for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
            lines += 1;
            if let Ok(d) = serde_json::from_str::<Detection>(&line) {
                if out.len() == retention {
                    out.pop_front();
                }
                out.push_back(d);
            }
        }
    }
    (out, lines)
}

fn rewrite(path: &Path, items: &[Detection]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
    for d in items {
        serde_json::to_writer(&mut f, d)?;
        f.write_all(b"\n")?;
    }
    f.flush()?;
    drop(f);
    std::fs::rename(tmp, path)
}

/// Appends batches and compacts the file. Runs until the channel closes.
pub async fn writer(
    path: PathBuf,
    retention: usize,
    mut lines: usize,
    mut rx: mpsc::UnboundedReceiver<Vec<Detection>>,
) {
    while let Some(batch) = rx.recv().await {
        let p = path.clone();
        let n = batch.len();
        let compact = lines + n > retention * 2;
        let res = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            if compact {
                let (mut keep, _) = load(&p, retention);
                keep.extend(batch);
                while keep.len() > retention {
                    keep.pop_front();
                }
                return rewrite(&p, &keep.into_iter().collect::<Vec<_>>());
            }
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&p)?;
            for d in &batch {
                serde_json::to_writer(&mut f, d)?;
                f.write_all(b"\n")?;
            }
            Ok(())
        })
        .await;
        match res {
            Ok(Ok(())) => {
                lines = if compact {
                    retention.min(lines + n)
                } else {
                    lines + n
                }
            }
            Ok(Err(e)) => tracing::warn!(error = %e, "write detections"),
            Err(e) => tracing::warn!(error = %e, "detection writer"),
        }
    }
}

impl Store {
    pub const fn new(
        items: VecDeque<Detection>,
        retention: usize,
        writer: mpsc::UnboundedSender<Vec<Detection>>,
    ) -> Self {
        Self {
            items: RwLock::new(items),
            retention,
            writer,
        }
    }

    pub fn len(&self) -> usize {
        self.items.read().map_or(0, |i| i.len())
    }

    pub fn add(&self, batch: Vec<Detection>) {
        if let Ok(mut items) = self.items.write() {
            for d in &batch {
                if items.len() == self.retention {
                    items.pop_front();
                }
                items.push_back(d.clone());
            }
        }
        let _ = self.writer.send(batch);
    }

    /// Newest first.
    pub fn query(&self, q: &Query) -> Vec<Detection> {
        let min = q.severity.as_deref().and_then(Severity::parse);
        let limit = q.limit.unwrap_or(100).min(1000);
        let Ok(items) = self.items.read() else {
            return Vec::new();
        };
        items
            .iter()
            .rev()
            .filter(|d| min.is_none_or(|m| d.severity >= m))
            .filter(|d| q.node.as_ref().is_none_or(|n| &d.node == n))
            .filter(|d| {
                q.engine
                    .as_ref()
                    .is_none_or(|e| d.engine.as_str().eq_ignore_ascii_case(e))
            })
            .filter(|d| {
                q.namespace
                    .as_ref()
                    .is_none_or(|ns| d.workload.as_ref().is_some_and(|w| &w.namespace == ns))
            })
            .take(limit)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{Engine, Workload};

    fn det(node: &str, sev: Severity, ns: Option<&str>) -> Detection {
        let mut d = Detection::new(node, Engine::Behavior, "r", sev, String::new());
        d.workload = ns.map(|n| Workload {
            namespace: n.into(),
            ..Workload::default()
        });
        d
    }

    #[test]
    fn query_filters_newest_first() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let s = Store::new(VecDeque::new(), 3, tx);
        s.add(vec![
            det("a", Severity::Low, None),
            det("b", Severity::High, Some("shop")),
            det("a", Severity::Critical, Some("pay")),
            det("c", Severity::Medium, None),
        ]);
        assert_eq!(s.len(), 3);
        let all = s.query(&Query::default());
        assert_eq!(all[0].node, "c");
        assert_eq!(
            s.query(&Query {
                severity: Some("high".into()),
                ..Query::default()
            })
            .len(),
            2
        );
        assert_eq!(
            s.query(&Query {
                node: Some("a".into()),
                ..Query::default()
            })
            .len(),
            1
        );
        assert_eq!(
            s.query(&Query {
                namespace: Some("shop".into()),
                ..Query::default()
            })[0]
                .node,
            "b"
        );
        assert_eq!(
            s.query(&Query {
                engine: Some("behavior".into()),
                limit: Some(1),
                ..Query::default()
            })
            .len(),
            1
        );
        assert!(
            s.query(&Query {
                engine: Some("Integrity".into()),
                ..Query::default()
            })
            .is_empty()
        );
    }

    #[tokio::test]
    async fn writer_appends_and_compacts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(writer(path.clone(), 2, 0, rx));
        tx.send(vec![
            det("a", Severity::Low, None),
            det("b", Severity::Low, None),
        ])
        .unwrap();
        tx.send(vec![
            det("c", Severity::Low, None),
            det("d", Severity::Low, None),
        ])
        .unwrap();
        tx.send(vec![det("e", Severity::Low, None)]).unwrap();
        drop(tx);
        task.await.unwrap();
        let (items, lines) = load(&path, 10);
        assert!(lines <= 4, "{lines}");
        assert_eq!(items.back().unwrap().node, "e");
        assert!(load(&dir.path().join("missing"), 5).0.is_empty());
    }
}

use std::net::IpAddr;
use std::sync::Arc;

use thiserror::Error;

use super::command::{ReadCommand, pipeline};
use super::info::node_info;
use super::model::{DbReply, DbRequest, KeyInfo, KeyPage, KeyValue, printable};
use crate::resp::{self, RespError, Value};
use crate::source::Tunnel;
use crate::types::PodRef;

const PREVIEW_BYTES: i64 = 4096;

#[derive(Debug, Error)]
pub enum DbError {
    #[error("no connected pod to reach the node through (scan pods first)")]
    NoGateway,
    #[error("every gateway pod failed, last error: {0}")]
    Tunnel(String),
    #[error(transparent)]
    Resp(#[from] RespError),
    #[error("server: {0}")]
    Server(String),
}

/// Talks to one cache node through pods that already reach it.
pub struct DbClient<T> {
    tunnel: Arc<T>,
    gateways: Vec<PodRef>,
    host: IpAddr,
    port: u16,
}

impl<T: Tunnel> DbClient<T> {
    pub const fn new(tunnel: Arc<T>, gateways: Vec<PodRef>, host: IpAddr, port: u16) -> Self {
        Self {
            tunnel,
            gateways,
            host,
            port,
        }
    }

    /// Returns the reply and the `namespace/pod` that carried it.
    pub async fn run(&self, request: &DbRequest) -> Result<(DbReply, String), DbError> {
        match request {
            DbRequest::Info => {
                let (v, via) = self.call(vec![ReadCommand::Info]).await?;
                Ok((DbReply::Info(node_info(&v[0].text())), via))
            }
            DbRequest::Keys {
                db,
                pattern,
                cursor,
            } => {
                let (page, via) = self.keys(*db, pattern, cursor).await?;
                Ok((DbReply::Keys(page), via))
            }
            DbRequest::Value { db, key, kind } => {
                let (value, via) = self.value(*db, key, kind).await?;
                Ok((DbReply::Value(value), via))
            }
        }
    }

    async fn keys(
        &self,
        db: u32,
        pattern: &str,
        cursor: &str,
    ) -> Result<(KeyPage, String), DbError> {
        let scan = ReadCommand::Scan {
            cursor: cursor.to_owned(),
            pattern: if pattern.is_empty() {
                "*".into()
            } else {
                pattern.to_owned()
            },
        };
        let (v, via) = self.call_in(db, vec![scan]).await?;
        let reply = &v[0];
        let next = reply
            .items()
            .first()
            .map_or_else(|| "0".into(), Value::text);
        let names: Vec<Vec<u8>> = reply.items().get(1).map_or_default(|a| {
            a.items()
                .iter()
                .filter_map(|k| k.bytes().map(<[u8]>::to_vec))
                .collect()
        });
        if names.is_empty() {
            return Ok((
                KeyPage {
                    cursor: next,
                    keys: Vec::new(),
                },
                via,
            ));
        }
        let cmds = names
            .iter()
            .flat_map(|k| {
                [
                    ReadCommand::Type(k.clone()),
                    ReadCommand::Pttl(k.clone()),
                    ReadCommand::MemoryUsage(k.clone()),
                ]
            })
            .collect();
        let (meta, via) = self.call_in(db, cmds).await?;
        let keys = names
            .into_iter()
            .zip(meta.chunks(3))
            .map(|(key, m)| KeyInfo {
                key,
                kind: m[0].text(),
                ttl_ms: m[1].int().unwrap_or(-2),
                size: m[2].int(),
            })
            .collect();
        Ok((KeyPage { cursor: next, keys }, via))
    }

    async fn value(&self, db: u32, key: &[u8], kind: &str) -> Result<(KeyValue, String), DbError> {
        let k = key.to_vec();
        let cmds = match kind {
            "string" => vec![ReadCommand::Strlen(k.clone()), ReadCommand::GetRange(k)],
            "hash" => vec![ReadCommand::Hlen(k.clone()), ReadCommand::Hscan(k)],
            "list" => vec![ReadCommand::Llen(k.clone()), ReadCommand::Lrange(k)],
            "set" => vec![ReadCommand::Scard(k.clone()), ReadCommand::Sscan(k)],
            "zset" => vec![ReadCommand::Zcard(k.clone()), ReadCommand::Zrange(k)],
            "stream" => vec![ReadCommand::Xlen(k.clone()), ReadCommand::Xrange(k)],
            _ => {
                let empty = KeyValue {
                    key: key.to_vec(),
                    kind: kind.to_owned(),
                    len: None,
                    rows: Vec::new(),
                    truncated: false,
                };
                return Ok((empty, String::new()));
            }
        };
        let (v, via) = self.call_in(db, cmds).await?;
        let len = v[0].int();
        let rows = rows(kind, &v[1]);
        let shown = i64::try_from(rows.len()).unwrap_or(i64::MAX);
        let truncated = match kind {
            "string" => len.is_some_and(|l| l > PREVIEW_BYTES),
            _ => len.is_some_and(|l| l > shown),
        };
        let value = KeyValue {
            key: key.to_vec(),
            kind: kind.to_owned(),
            len,
            rows,
            truncated,
        };
        Ok((value, via))
    }

    /// Prepends `SELECT` for non-zero databases and strips its reply.
    async fn call_in(
        &self,
        db: u32,
        mut cmds: Vec<ReadCommand>,
    ) -> Result<(Vec<Value>, String), DbError> {
        if db == 0 {
            return self.call(cmds).await;
        }
        cmds.insert(0, ReadCommand::Select(db));
        let (mut v, via) = self.call(cmds).await?;
        let select = v.remove(0);
        if select.is_error() {
            return Err(DbError::Server(select.text()));
        }
        Ok((v, via))
    }

    async fn call(&self, cmds: Vec<ReadCommand>) -> Result<(Vec<Value>, String), DbError> {
        if self.gateways.is_empty() {
            return Err(DbError::NoGateway);
        }
        let payload = pipeline(&cmds);
        let mut last = String::new();
        for gw in &self.gateways {
            match self
                .tunnel
                .roundtrip(gw, self.host, self.port, payload.clone())
                .await
            {
                Ok(bytes) => {
                    let mut values = resp::parse_all(&bytes)?;
                    // One more reply than commands: the trailing QUIT, proof the reply is whole.
                    if values.len() <= cmds.len() {
                        last = format!("{}: {} of {} replies", gw.key(), values.len(), cmds.len());
                        continue;
                    }
                    values.truncate(cmds.len());
                    return Ok((values, gw.key()));
                }
                Err(e) => last = format!("{}: {e}", gw.key()),
            }
        }
        Err(DbError::Tunnel(last))
    }
}

fn pairs(items: &[Value]) -> Vec<(String, String)> {
    items
        .chunks(2)
        .map(|c| {
            let a = c.first().and_then(Value::bytes).map_or_default(printable);
            let b = c
                .get(1)
                .map_or_default(|v| v.bytes().map_or_else(|| v.text(), printable));
            (a, b)
        })
        .collect()
}

fn rows(kind: &str, reply: &Value) -> Vec<(String, String)> {
    let text = |v: &Value| v.bytes().map_or_else(|| v.text(), printable);
    match kind {
        "string" => vec![(String::new(), text(reply))],
        "hash" => pairs(reply.items().get(1).map_or_default(Value::items)),
        "list" => reply
            .items()
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), text(v)))
            .collect(),
        "set" => reply
            .items()
            .get(1)
            .map_or_default(Value::items)
            .iter()
            .map(|v| (String::new(), text(v)))
            .collect(),
        "zset" => pairs(reply.items())
            .into_iter()
            .map(|(m, s)| (s, m))
            .collect(),
        "stream" => reply
            .items()
            .iter()
            .map(|e| {
                let id = e.items().first().map_or_default(Value::text);
                let fields = e
                    .items()
                    .get(1)
                    .map_or_default(|f| pairs(f.items()))
                    .into_iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join("  ");
                (id, fields)
            })
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::future::{Future, ready};
    use std::sync::Mutex;

    use super::*;
    use crate::types::tests::pod;

    /// Answers each pipeline with canned replies chosen by the first command verb.
    struct FakeTunnel {
        replies: HashMap<&'static str, Vec<u8>>,
        failing: Vec<String>,
        seen: Mutex<Vec<String>>,
    }

    impl Tunnel for FakeTunnel {
        type Error = String;

        fn roundtrip(
            &self,
            via: &PodRef,
            _host: IpAddr,
            _port: u16,
            payload: Vec<u8>,
        ) -> impl Future<Output = Result<Vec<u8>, String>> + Send {
            self.seen.lock().unwrap().push(via.key());
            if self.failing.contains(&via.name) {
                return ready(Err("exec: nc not found".into()));
            }
            let cmds = resp::parse_all(&payload).unwrap();
            let verbs: Vec<String> = cmds.iter().map(|c| c.items()[0].text()).collect();
            let key = verbs
                .iter()
                .find(|v| *v != "SELECT")
                .cloned()
                .unwrap_or_default();
            let mut out = Vec::new();
            if verbs[0] == "SELECT" {
                out.extend_from_slice(
                    self.replies
                        .get("SELECT")
                        .map_or(b"+OK\r\n".as_slice(), Vec::as_slice),
                );
            }
            out.extend_from_slice(&self.replies[key.as_str()]);
            out.extend_from_slice(b"+OK\r\n");
            ready(Ok(out))
        }
    }

    const INFO_REPLY: &str = "$33\r\nrole:master\r\ndb0:keys=3,expires=1\r\n";

    fn tunnel(replies: &[(&'static str, &str)], failing: &[&str]) -> Arc<FakeTunnel> {
        Arc::new(FakeTunnel {
            replies: replies
                .iter()
                .map(|(k, v)| (*k, v.as_bytes().to_vec()))
                .collect(),
            failing: failing.iter().map(|s| (*s).to_owned()).collect(),
            seen: Mutex::new(Vec::new()),
        })
    }

    fn client(t: &Arc<FakeTunnel>) -> DbClient<FakeTunnel> {
        DbClient::new(
            Arc::clone(t),
            vec![pod("shop", "api-1", "api"), pod("shop", "api-2", "api")],
            "10.0.0.1".parse().unwrap(),
            6379,
        )
    }

    #[tokio::test]
    async fn info_falls_back_to_next_gateway() {
        let t = tunnel(&[("INFO", INFO_REPLY)], &["api-1"]);
        let (reply, via) = client(&t).run(&DbRequest::Info).await.unwrap();
        assert_eq!(via, "shop/api-2");
        let DbReply::Info(info) = reply else { panic!() };
        assert_eq!(info.role, "master");
        assert_eq!(info.dbs[0].keys, 3);
        assert_eq!(t.seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn keys_page_with_metadata() {
        let t = tunnel(
            &[
                (
                    "SCAN",
                    "*2\r\n$2\r\n17\r\n*2\r\n$6\r\nuser:1\r\n$6\r\nuser:2\r\n",
                ),
                (
                    "TYPE",
                    "+string\r\n:-1\r\n:56\r\n+hash\r\n:5000\r\n:120\r\n",
                ),
            ],
            &[],
        );
        let req = DbRequest::Keys {
            db: 2,
            pattern: String::new(),
            cursor: "0".into(),
        };
        let (reply, _) = client(&t).run(&req).await.unwrap();
        let DbReply::Keys(page) = reply else { panic!() };
        assert_eq!(page.cursor, "17");
        assert_eq!(page.keys.len(), 2);
        assert_eq!(
            (
                page.keys[1].kind.as_str(),
                page.keys[1].ttl_ms,
                page.keys[1].size
            ),
            ("hash", 5000, Some(120))
        );
    }

    #[tokio::test]
    async fn empty_scan_page() {
        let t = tunnel(&[("SCAN", "*2\r\n$1\r\n0\r\n*0\r\n")], &[]);
        let req = DbRequest::Keys {
            db: 0,
            pattern: "x*".into(),
            cursor: "0".into(),
        };
        let (reply, _) = client(&t).run(&req).await.unwrap();
        assert_eq!(
            reply,
            DbReply::Keys(KeyPage {
                cursor: "0".into(),
                keys: vec![]
            })
        );
    }

    #[tokio::test]
    async fn values_by_type() {
        let cases: [(&str, &'static str, &str, usize, bool); 6] = [
            ("string", "STRLEN", ":9000\r\n$5\r\nhello\r\n", 1, true),
            (
                "hash",
                "HLEN",
                ":2\r\n*2\r\n$1\r\n0\r\n*4\r\n$1\r\na\r\n$1\r\n1\r\n$1\r\nb\r\n$1\r\n2\r\n",
                2,
                false,
            ),
            (
                "list",
                "LLEN",
                ":3\r\n*2\r\n$1\r\nx\r\n$1\r\ny\r\n",
                2,
                true,
            ),
            (
                "set",
                "SCARD",
                ":1\r\n*2\r\n$1\r\n0\r\n*1\r\n$1\r\nm\r\n",
                1,
                false,
            ),
            (
                "zset",
                "ZCARD",
                ":1\r\n*2\r\n$1\r\nm\r\n$3\r\n1.5\r\n",
                1,
                false,
            ),
            (
                "stream",
                "XLEN",
                ":1\r\n*1\r\n*2\r\n$3\r\n1-0\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n",
                1,
                false,
            ),
        ];
        for (kind, verb, reply, rows, truncated) in cases {
            let t = tunnel(&[(verb, reply)], &[]);
            let req = DbRequest::Value {
                db: 0,
                key: b"k".to_vec(),
                kind: kind.into(),
            };
            let (reply, _) = client(&t).run(&req).await.unwrap();
            let DbReply::Value(v) = reply else { panic!() };
            assert_eq!((v.rows.len(), v.truncated), (rows, truncated), "{kind}");
            if kind == "zset" {
                assert_eq!(v.rows[0], ("1.5".into(), "m".into()));
            }
            if kind == "stream" {
                assert_eq!(v.rows[0], ("1-0".into(), "f=v".into()));
            }
        }
        let t = tunnel(&[], &[]);
        let req = DbRequest::Value {
            db: 0,
            key: b"k".to_vec(),
            kind: "none".into(),
        };
        let (DbReply::Value(v), _) = client(&t).run(&req).await.unwrap() else {
            panic!()
        };
        assert_eq!(v.rows.len(), 0);
    }

    #[tokio::test]
    async fn errors_surface() {
        let t = tunnel(
            &[
                ("SELECT", "-ERR SELECT is not allowed in cluster mode\r\n"),
                ("SCAN", "*2\r\n$1\r\n0\r\n*0\r\n"),
            ],
            &[],
        );
        let req = DbRequest::Keys {
            db: 1,
            pattern: String::new(),
            cursor: "0".into(),
        };
        assert!(matches!(
            client(&t).run(&req).await,
            Err(DbError::Server(_))
        ));

        let t = tunnel(&[], &["api-1", "api-2"]);
        let err = client(&t).run(&DbRequest::Info).await.unwrap_err();
        assert!(
            err.to_string().contains("shop/api-2: exec: nc not found"),
            "{err}"
        );

        let none = DbClient::new(t, vec![], "10.0.0.1".parse().unwrap(), 6379);
        assert!(matches!(
            none.run(&DbRequest::Info).await,
            Err(DbError::NoGateway)
        ));

        let short = tunnel(&[("INFO", "")], &[]);
        assert!(matches!(
            client(&short).run(&DbRequest::Info).await,
            Err(DbError::Tunnel(_))
        ));
    }
}

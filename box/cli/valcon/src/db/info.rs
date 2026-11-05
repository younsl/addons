use std::collections::BTreeMap;

use super::model::{DbStat, NodeInfo};

/// Parses `INFO` output into section-less key/value pairs.
pub fn parse(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

/// `db0:keys=1,expires=0,avg_ttl=0` lines.
pub fn keyspace(fields: &BTreeMap<String, String>) -> Vec<DbStat> {
    fields
        .iter()
        .filter_map(|(k, v)| {
            let db = k.strip_prefix("db")?.parse().ok()?;
            let get = |name: &str| {
                v.split(',')
                    .filter_map(|kv| kv.split_once('='))
                    .find(|(n, _)| *n == name)
                    .and_then(|(_, val)| val.parse().ok())
                    .unwrap_or(0)
            };
            Some(DbStat {
                db,
                keys: get("keys"),
                expires: get("expires"),
                avg_ttl_ms: get("avg_ttl"),
            })
        })
        .collect()
}

pub fn node_info(text: &str) -> NodeInfo {
    let fields = parse(text);
    let get = |k: &str| fields.get(k).cloned().unwrap_or_else(|| "-".into());
    let hits: f64 = fields
        .get("keyspace_hits")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);
    let misses: f64 = fields
        .get("keyspace_misses")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);
    let hit_rate = (hits + misses > 0.0).then(|| hits / (hits + misses) * 100.0);
    let version = fields
        .get("valkey_version")
        .or_else(|| fields.get("redis_version"))
        .cloned()
        .unwrap_or_else(|| "-".into());
    let mut dbs = keyspace(&fields);
    dbs.sort_by_key(|d| d.db);
    NodeInfo {
        version,
        role: get("role"),
        uptime_days: get("uptime_in_days"),
        used_memory: get("used_memory_human"),
        max_memory: get("maxmemory_human"),
        clients: get("connected_clients"),
        ops_per_sec: get("instantaneous_ops_per_sec"),
        hit_rate,
        evicted: get("evicted_keys"),
        expired: get("expired_keys"),
        dbs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO: &str = "# Server\r\nvalkey_version:7.2.6\r\nuptime_in_days:12\r\n\r\n# Clients\r\nconnected_clients:578\r\n# Memory\r\nused_memory_human:1.2G\r\nmaxmemory_human:5.0G\r\n# Stats\r\ninstantaneous_ops_per_sec:900\r\nkeyspace_hits:75\r\nkeyspace_misses:25\r\nevicted_keys:0\r\nexpired_keys:10\r\n# Replication\r\nrole:master\r\n# Keyspace\r\ndb0:keys=1002,expires=1002,avg_ttl=39476555\r\ndb2:keys=5,expires=0,avg_ttl=0\r\n";

    #[test]
    fn extracts_summary_and_keyspace() {
        let n = node_info(INFO);
        assert_eq!(n.version, "7.2.6");
        assert_eq!((n.role.as_str(), n.clients.as_str()), ("master", "578"));
        assert_eq!(n.hit_rate, Some(75.0));
        assert_eq!(n.dbs.len(), 2);
        assert_eq!(
            (n.dbs[0].db, n.dbs[0].keys, n.dbs[0].expires),
            (0, 1002, 1002)
        );
        assert_eq!(n.dbs[1].db, 2);
    }

    #[test]
    fn tolerates_missing_fields() {
        let n = node_info("redis_version:6.2.0\r\n");
        assert_eq!(n.version, "6.2.0");
        assert_eq!(n.role, "-");
        assert_eq!(n.hit_rate, None);
        assert_eq!(n.dbs.len(), 0);
        assert_eq!(node_info("").version, "-");
    }
}

//! Per-receiver batching of approval alarms over a window, and the grouped
//! alarm template.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use bytes::Bytes;

use super::{
    AlarmField, ApprovalEvt, ApprovalPayload, Notifier, Target, TargetBatch, cve_link,
    evt_packages, render_alarm, timestamp,
};

impl Notifier {
    /// Buffers an approval event for each target and (re)arms the flush timer.
    /// Called only when batching is enabled.
    pub(crate) fn enqueue(self: &Arc<Self>, targets: &[Target], evt: ApprovalEvt) {
        let window = self.settings.read().batch_window;
        let mut st = self.state.lock();
        let batch = st.batch.get_or_insert_with(HashMap::new);
        for t in targets {
            if t.url.is_empty() {
                continue;
            }
            batch
                .entry(t.url.clone())
                .or_insert_with(|| TargetBatch {
                    name: t.name.clone(),
                    evts: Vec::new(),
                })
                .evts
                .push(evt.clone());
        }
        // First event of a window arms the timer; later events ride the same one.
        if st.flush_timer.is_none() {
            let n = Arc::clone(self);
            st.flush_timer = Some(tokio::spawn(async move {
                tokio::time::sleep(window).await;
                n.flush();
            }));
        }
    }

    /// Drains the buffered events and delivers one message per receiver: a
    /// grouped summary when several packages queued in the window, or the
    /// ordinary single-package alarm when only one did.
    pub(crate) fn flush(self: &Arc<Self>) {
        let batch = {
            let mut st = self.state.lock();
            st.flush_timer = None;
            st.batch.take()
        };
        let Some(batch) = batch else {
            return;
        };
        for (url, b) in batch {
            if b.evts.is_empty() {
                continue;
            }
            let payload = if b.evts.len() == 1 {
                self.build_approval_single(&b.evts[0])
            } else {
                self.build_approval_grouped(&b.evts)
            };
            let body = match serde_json::to_vec(&payload) {
                Ok(body) => Bytes::from(body),
                Err(e) => {
                    tracing::error!(err = %e, "notify: marshal grouped approval payload failed");
                    continue;
                }
            };
            tokio::spawn(Arc::clone(self).post(
                Target { name: b.name, url },
                body,
                evt_packages(&b.evts),
            ));
        }
    }

    /// Renders one alarm summarising several quarantined packages, grouped by
    /// repository and de-duplicated. It names the requester(s) (a burst is
    /// usually one install session, so typically one username) and lists each
    /// affected repository; the structured `packages`/`requesters` carry the
    /// full sets for machine consumers.
    pub(crate) fn build_approval_grouped(&self, evts: &[ApprovalEvt]) -> ApprovalPayload {
        struct PkgRef {
            repo_id: i64,
            pkg: String,
            version: String,
        }
        let mut seen: HashSet<String> = HashSet::new();
        let mut repo_seen: HashSet<String> = HashSet::new();
        let mut req_seen: HashSet<String> = HashSet::new();
        let mut repo_id: HashMap<String, i64> = HashMap::new();
        let mut repo_format: HashMap<String, String> = HashMap::new();
        let mut repos: Vec<String> = Vec::new();
        let mut packages: Vec<String> = Vec::new();
        let mut requesters: Vec<String> = Vec::new();
        let mut refs: Vec<PkgRef> = Vec::new();
        for e in evts {
            let coord = if e.version.is_empty() {
                e.pkg.clone()
            } else {
                format!("{}@{}", e.pkg, e.version)
            };
            let who = if e.requested_by.is_empty() {
                "anonymous"
            } else {
                e.requested_by.as_str()
            };
            if req_seen.insert(who.to_string()) {
                requesters.push(who.to_string());
            }
            if repo_seen.insert(e.repo.clone()) {
                repos.push(e.repo.clone());
                repo_id.insert(e.repo.clone(), e.repo_id);
                repo_format.insert(e.repo.clone(), e.repo_format.clone());
            }
            let key = format!("{}/{coord}", e.repo);
            if !seen.insert(key.clone()) {
                continue;
            }
            packages.push(key);
            refs.push(PkgRef {
                repo_id: e.repo_id,
                pkg: e.pkg.clone(),
                version: e.version.clone(),
            });
        }
        repos.sort();
        packages.sort();
        requesters.sort();
        let count = packages.len() as i64;

        // Deep-link each affected repository, joined with " / " (no commas).
        let repo_links: Vec<String> = repos
            .iter()
            .map(|r| {
                self.repo_field(
                    r,
                    repo_id.get(r).copied().unwrap_or(0),
                    repo_format.get(r).map(String::as_str).unwrap_or(""),
                )
            })
            .collect();

        // Pending packages carries only the count (scale); the actionable risk detail
        // (the top-CVE package) lives in the Vulnerability field, and the full list is
        // one click away in Forklift.
        let mut fields = vec![
            AlarmField::new("Repository", repo_links.join(" / ")),
            AlarmField::new("Pending packages", count.to_string()),
        ];

        // Clean vs Dirty breakdown in one field, when a scan lookup is wired. Clean =
        // scanned with no known advisories; Dirty = the rest (vulnerable or
        // unscanned). The highest-severity package is named in parentheses with its
        // CVE severity, score and linked id, so a reviewer sees what to look at.
        let clean_checker = self.settings.read().clean_checker.clone();
        let mut clean: i64 = 0;
        let mut top_rank = 0;
        let (mut top_severity, mut top_score, mut top_id, mut top_pkg) =
            (String::new(), String::new(), String::new(), String::new());
        if let Some(check) = &clean_checker {
            for r in &refs {
                let (severity, score, id, scanned) = check(r.repo_id, &r.pkg, &r.version);
                if scanned && severity == "none" {
                    clean += 1;
                }
                let rank = sev_rank(&severity);
                if rank > top_rank {
                    let coord = if r.version.is_empty() {
                        r.pkg.clone()
                    } else {
                        format!("{}@{}", r.pkg, r.version)
                    };
                    top_rank = rank;
                    top_severity = severity;
                    top_score = score;
                    top_id = id;
                    top_pkg = coord;
                }
            }
        }
        let dirty = count - clean;
        if clean_checker.is_some() {
            // "Vulnerability: N clean / M dirty (pkg@ver severity score CVE-link)". No
            // commas (space-separated) to keep the alarm free of commas and em-dashes.
            let mut val = format!("{clean} clean / {dirty} dirty");
            if !top_severity.is_empty() {
                let mut detail = format!("{top_pkg} {top_severity}");
                if !top_score.is_empty() {
                    detail.push(' ');
                    detail.push_str(&top_score);
                }
                if !top_id.is_empty() {
                    detail.push(' ');
                    detail.push_str(&cve_link(&top_id));
                }
                val.push_str(" (");
                val.push_str(&detail);
                val.push(')');
            }
            fields.push(AlarmField::new("Vulnerability", val));
        }
        fields.push(AlarmField::new(
            "Requested by",
            summarise_requesters(&requesters),
        ));

        let mut payload = ApprovalPayload {
            text: render_alarm(
                "Packages pending approval",
                &format!(
                    "Packages are quarantined and awaiting review. Review in {}.",
                    self.forklift()
                ),
                &fields,
            ),
            event: "approval.request".to_string(),
            count,
            packages,
            requesters,
            timestamp: timestamp(),
            ..Default::default()
        };
        if clean_checker.is_some() {
            payload.clean_count = clean;
            payload.dirty_count = dirty;
            payload.max_severity = top_severity;
            payload.max_score = top_score;
            payload.top_cve = top_id;
            payload.top_package = top_pkg;
        }
        payload
    }

    /// Renders the exact alarm that would be sent for the given packages
    /// (grouped when several, single when one), so the UI preview shows the
    /// real template rather than a stand-in. Uses real coordinates from the
    /// caller.
    pub fn preview_approval(
        &self,
        repo: &str,
        repo_id: i64,
        repo_format: &str,
        pkgs: &[PreviewPackage],
    ) -> ApprovalPayload {
        let evts: Vec<ApprovalEvt> = pkgs
            .iter()
            .map(|p| ApprovalEvt {
                repo: repo.to_string(),
                repo_id,
                repo_format: repo_format.to_string(),
                pkg: p.package.clone(),
                version: p.version.clone(),
                requested_by: p.requested_by.clone(),
            })
            .collect();
        if evts.len() == 1 {
            return self.build_approval_single(&evts[0]);
        }
        self.build_approval_grouped(&evts)
    }
}

/// Ranks a severity label for the grouped alarm's top-CVE pick; unknown labels
/// (including "none" and "") rank lowest.
fn sev_rank(severity: &str) -> i64 {
    match severity {
        "low" => 1,
        "medium" => 2,
        "high" => 3,
        "critical" => 4,
        _ => 0,
    }
}

/// One coordinate fed to [`Notifier::preview_approval`] to render a preview.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreviewPackage {
    pub package: String,
    pub version: String,
    pub requested_by: String,
}

/// Renders the distinct requester set compactly with " / " separators (no
/// commas): up to three names in full, otherwise the first two plus a
/// "(+N others)" count.
fn summarise_requesters(names: &[String]) -> String {
    match names.len() {
        0 => "anonymous".to_string(),
        1..=3 => names.join(" / "),
        n => format!("{} (+{} others)", names[..2].join(" / "), n - 2),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use crate::notify::tests::webhook::install_crypto;
    use crate::notify::*;

    #[test]
    fn build_approval_grouped() {
        install_crypto();
        let n = Notifier::new(Duration::ZERO);
        n.set_external_url("https://forklift.example.com");
        let evt = |repo: &str, format: &str, pkg: &str, version: &str, who: &str| ApprovalEvt {
            repo: repo.into(),
            repo_format: format.into(),
            pkg: pkg.into(),
            version: version.into(),
            requested_by: who.into(),
            ..Default::default()
        };
        let evts = [
            evt("npmjs", "npm", "lodash", "4.17.21", "alice"),
            evt("npmjs", "npm", "axios", "1.6.0", "bob"),
            evt("pypi", "pypi", "requests", "2.31.0", ""),
            // duplicate coordinate
            evt("npmjs", "npm", "lodash", "4.17.21", "carol"),
        ];
        let p = n.build_approval_grouped(&evts);
        assert!(
            p.count == 3 && p.packages.len() == 3,
            "count/packages = {}/{:?}, want 3 (deduped)",
            p.count,
            p.packages
        );
        assert!(
            p.text.contains("*Packages pending approval*"),
            "missing bold title: {:?}",
            p.text
        );
        assert!(
            p.text.contains("*Pending packages*: 3"),
            "missing count field: {:?}",
            p.text
        );
        assert!(
            p.text.contains("*Repository*: npmjs (npm) / pypi (pypi)"),
            "repository field not rendered/sorted: {:?}",
            p.text
        );
        assert!(
            p.text.contains("*Requested by*: "),
            "missing requester field: {:?}",
            p.text
        );
        assert!(
            p.text.contains("<https://forklift.example.com|Forklift>"),
            "missing Forklift link: {:?}",
            p.text
        );
        // Constraints: no emoji, no comma, no em-dash anywhere in the alarm text.
        assert!(
            !p.text.contains(['📦', '🧪', ',', '—']),
            "text must not contain emoji/comma/em-dash: {:?}",
            p.text
        );
        // Four distinct requesters (the deduped duplicate coordinate still adds carol).
        assert_eq!(
            p.requesters.len(),
            4,
            "requesters = {:?}, want 4",
            p.requesters
        );
        assert_eq!(p.requesters, ["alice", "anonymous", "bob", "carol"]);
        assert_eq!(p.event, "approval.request");
        // More than three requesters collapse to two names plus a count.
        assert!(
            p.text
                .contains("*Requested by*: alice / anonymous (+2 others)"),
            "{:?}",
            p.text
        );
    }

    #[test]
    fn grouped_pending_count_only() {
        install_crypto();
        let n = Notifier::new(Duration::ZERO);
        let evts: Vec<ApprovalEvt> = (0..8)
            .map(|i| ApprovalEvt {
                repo: "npmjs".into(),
                pkg: format!("{}pkg", (b'a' + i) as char),
                version: "1.0.0".into(),
                ..Default::default()
            })
            .collect();
        let p = n.build_approval_grouped(&evts);
        assert_eq!(p.count, 8, "count, want 8");
        // Pending packages is count only (no per-package enumeration), no commas.
        assert!(
            p.text.contains("*Pending packages*: 8"),
            "count field: {:?}",
            p.text
        );
        assert!(
            !p.text.contains(','),
            "text must not contain commas: {:?}",
            p.text
        );
    }

    #[test]
    fn grouped_clean_dirty_field() {
        install_crypto();
        let n = Notifier::new(Duration::ZERO);
        // clean-pkg: scanned none; vuln-pkg: critical 9.8; unknown-pkg: unscanned.
        n.set_clean_checker(Arc::new(|_repo_id, pkg, _version| match pkg {
            "clean-pkg" => ("none".into(), String::new(), String::new(), true),
            "vuln-pkg" => (
                "critical".into(),
                "9.8".into(),
                "CVE-2021-23358".into(),
                true,
            ),
            _ => (String::new(), String::new(), String::new(), false),
        }));
        let evt = |pkg: &str, version: &str| ApprovalEvt {
            repo: "npmjs".into(),
            pkg: pkg.into(),
            version: version.into(),
            ..Default::default()
        };
        let evts = [
            evt("clean-pkg", "1.0.0"),
            evt("vuln-pkg", "2.0.0"),
            evt("unknown-pkg", "3.0.0"),
        ];
        let p = n.build_approval_grouped(&evts);
        assert!(
            p.clean_count == 1 && p.dirty_count == 2,
            "clean/dirty = {}/{}, want 1/2",
            p.clean_count,
            p.dirty_count
        );
        assert!(
            p.max_severity == "critical" && p.max_score == "9.8" && p.top_cve == "CVE-2021-23358",
            "top CVE = {} {} {}, want critical 9.8 CVE-2021-23358",
            p.max_severity,
            p.max_score,
            p.top_cve
        );
        assert_eq!(p.top_package, "vuln-pkg@2.0.0", "top package");
        assert!(
        p.text.contains(
            "*Vulnerability*: 1 clean / 2 dirty (vuln-pkg@2.0.0 critical 9.8 <https://www.cve.org/CVERecord?id=CVE-2021-23358|CVE-2021-23358>)"
        ),
        "vulnerability field not rendered with package + CVE link: {:?}",
        p.text
    );
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["clean_count"], 1);
        assert_eq!(v["dirty_count"], 2);
        assert_eq!(v["max_severity"], "critical");
        assert_eq!(v["max_score"], "9.8");
        assert_eq!(v["top_cve"], "CVE-2021-23358");
        assert_eq!(v["top_package"], "vuln-pkg@2.0.0");
    }

    mod batch_window {
        use std::sync::Arc;
        use std::time::Duration;

        use tokio::sync::mpsc;
        use wiremock::MockServer;

        use crate::notify::tests::webhook::{await_requests, install_crypto, webhook};
        use crate::notify::*;

        /// Collects the alarm payloads posted to one webhook URL, waiting briefly for
        /// `want` of them since delivery is asynchronous.
        async fn await_payloads(srv: &MockServer, want: usize) -> Vec<ApprovalPayload> {
            await_requests(srv, want)
                .await
                .iter()
                .map(|r| serde_json::from_slice(&r.body).expect("decode alarm"))
                .collect()
        }

        /// The batch window exists so an install burst is one alarm per receiver instead
        /// of one per package: several events inside the window must arrive as a single
        /// grouped message, and every receiver must get it. Without this the queue for a
        /// large resolve is a wall of webhooks.
        #[tokio::test]
        async fn batch_window_groups_burst_per_receiver() {
            install_crypto();
            let (first, second) = (webhook(200).await, webhook(200).await);
            let n = Arc::new(Notifier::new(Duration::from_secs(1)));
            n.set_batch_window(Duration::from_millis(30));

            let targets = vec![
                Target {
                    name: "security".into(),
                    url: first.uri(),
                },
                Target {
                    name: "platform".into(),
                    url: second.uri(),
                },
                // A receiver with no URL is skipped rather than buffered forever.
                Target {
                    name: "misconfigured".into(),
                    url: String::new(),
                },
            ];
            n.notify_approval_request(&targets, "npmjs", 1, "npm", "lodash", "4.17.21", "alice");
            n.notify_approval_request(&targets, "npmjs", 1, "npm", "axios", "1.6.0", "alice");
            n.notify_approval_request(&targets, "pypi", 2, "pypi", "requests", "2.31.0", "bob");

            for (name, r) in [("security", &first), ("platform", &second)] {
                let payloads = await_payloads(r, 1).await;
                assert_eq!(
                    payloads.len(),
                    1,
                    "{name} received {} alarms, want one grouped alarm",
                    payloads.len()
                );
                let payload = &payloads[0];
                assert!(
                    payload.count == 3 && payload.packages.len() == 3,
                    "{name} alarm covers {} packages ({:?}), want 3",
                    payload.count,
                    payload.packages
                );
                assert_eq!(payload.event, "approval.request", "{name} alarm event");
                assert_eq!(
                    payload.packages,
                    [
                        "npmjs/axios@1.6.0",
                        "npmjs/lodash@4.17.21",
                        "pypi/requests@2.31.0"
                    ]
                );
            }

            // The window is over, so the buffer and its timer are cleared and a later
            // event starts a fresh window rather than riding a stale one.
            let st = n.state.lock();
            assert!(
                st.batch.is_none() && st.flush_timer.is_none(),
                "buffer not cleared after the window: batch={} timer={}",
                st.batch.is_some(),
                st.flush_timer.is_some()
            );
        }

        /// A window that happens to hold one event must send the ordinary single-package
        /// alarm, not a "1 package" summary: grouping is an optimisation for bursts, and a
        /// lone quarantine should read exactly as it does with batching off.
        #[tokio::test]
        async fn batch_window_single_event_sends_single_alarm() {
            install_crypto();
            let r = webhook(200).await;
            let n = Arc::new(Notifier::new(Duration::from_secs(1)));
            n.set_batch_window(Duration::from_millis(20));

            n.notify_approval_request(
                &[Target {
                    name: "security".into(),
                    url: r.uri(),
                }],
                "npmjs",
                1,
                "npm",
                "lodash",
                "4.17.21",
                "alice",
            );

            let payloads = await_payloads(&r, 1).await;
            assert_eq!(
                payloads.len(),
                1,
                "received {} alarms, want 1",
                payloads.len()
            );
            let single = n.build_approval_single(&ApprovalEvt {
                repo: "npmjs".into(),
                repo_id: 1,
                repo_format: "npm".into(),
                pkg: "lodash".into(),
                version: "4.17.21".into(),
                requested_by: "alice".into(),
            });
            assert_eq!(
                payloads[0].text, single.text,
                "batched single alarm text, want the ordinary single-package text"
            );
        }

        /// The delivery recorder is how an approval row learns whether its alarm went out,
        /// so a grouped message has to report every package it covered: a package missing
        /// from the report would show as never notified.
        #[tokio::test]
        async fn batched_delivery_records_every_package() {
            install_crypto();
            let r = webhook(200).await;
            let n = Arc::new(Notifier::new(Duration::from_secs(1)));
            n.set_batch_window(Duration::from_millis(20));

            let (tx, mut rx) = mpsc::unbounded_channel::<(Vec<DeliveredPackage>, String)>();
            n.set_delivery_recorder(Arc::new(move |pkgs, result, _detail, _ms| {
                let _ = tx.send((pkgs, result.to_string()));
            }));

            let targets = vec![Target {
                name: "security".into(),
                url: r.uri(),
            }];
            n.notify_approval_request(&targets, "npmjs", 1, "npm", "lodash", "4.17.21", "alice");
            n.notify_approval_request(&targets, "pypi", 2, "pypi", "requests", "2.31.0", "bob");

            let (pkgs, result) = tokio::time::timeout(Duration::from_secs(3), rx.recv())
                .await
                .expect("no delivery was recorded")
                .unwrap();
            assert_eq!(
                result, DELIVERY_DELIVERED,
                "delivery result, want delivered"
            );
            assert_eq!(
                pkgs.len(),
                2,
                "recorded {pkgs:?}, want both packages of the grouped alarm"
            );
            assert!(
                pkgs[0].package == "lodash" && pkgs[1].package == "requests",
                "recorded packages = {pkgs:?}"
            );
        }

        /// The console's preview must render the real template, so it follows the same
        /// single-versus-grouped rule the delivery path uses.
        #[test]
        fn preview_approval_matches_delivered_shape() {
            install_crypto();
            let n = Notifier::new(Duration::ZERO);

            let single = n.preview_approval(
                "npmjs",
                1,
                "npm",
                &[PreviewPackage {
                    package: "lodash".into(),
                    version: "4.17.21".into(),
                    requested_by: "alice".into(),
                }],
            );
            let want = n.build_approval_single(&ApprovalEvt {
                repo: "npmjs".into(),
                repo_id: 1,
                repo_format: "npm".into(),
                pkg: "lodash".into(),
                version: "4.17.21".into(),
                requested_by: "alice".into(),
            });
            assert_eq!(single.text, want.text, "single preview");

            let grouped = n.preview_approval(
                "npmjs",
                1,
                "npm",
                &[
                    PreviewPackage {
                        package: "lodash".into(),
                        version: "4.17.21".into(),
                        requested_by: "alice".into(),
                    },
                    PreviewPackage {
                        package: "axios".into(),
                        version: "1.6.0".into(),
                        requested_by: "bob".into(),
                    },
                ],
            );
            assert!(
                grouped.count == 2 && grouped.packages.len() == 2,
                "grouped preview covers {} packages ({:?}), want 2",
                grouped.count,
                grouped.packages
            );
        }
    }
}

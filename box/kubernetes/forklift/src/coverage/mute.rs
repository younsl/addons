//! Muting: which checks an operator has taken out of a project's verdict, and
//! what that does to the verdict underneath.

use std::collections::HashMap;

use crate::coverage::types::{
    AppliedState, EXCLUDE_MUTED, EXCLUDE_TOPIC_PREFIX, Project, STATE_APPLIED, STATE_ERROR,
    STATE_NOT_APPLIED, STATE_PARTIAL,
};
use crate::coverage::{Error, Res, Scanner};

/// MuteScope is one half of the wiring an operator can take out of a project's
/// verdict on its own. Muting one check leaves the project in the measurement
/// and stops requiring that half; muting both is what takes the whole project
/// out, which is what a bare "mute" used to mean.
///
/// MuteScopeCI waives the CI pipeline half.
pub const MUTE_SCOPE_CI: &str = "ci";
/// MuteScopeRegistry waives the package-manager or image-build half.
pub const MUTE_SCOPE_REGISTRY: &str = "registry";

/// MuteScopes is what an operator has silenced on one project. The zero value is
/// a project nobody has touched.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MuteScopes {
    pub ci: bool,
    pub registry: bool,
}

impl MuteScopes {
    /// Any reports whether at least one check is muted.
    pub fn any(&self) -> bool {
        self.ci || self.registry
    }

    /// All reports whether every check is muted, which is the whole project
    /// being out of the measurement.
    pub fn all(&self) -> bool {
        self.ci && self.registry
    }

    /// List returns the muted checks in a stable order, always a non-empty-typed
    /// vector so it travels as `[]` rather than `null`.
    pub fn list(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.ci {
            out.push(MUTE_SCOPE_CI.to_string());
        }
        if self.registry {
            out.push(MUTE_SCOPE_REGISTRY.to_string());
        }
        out
    }
}

/// MutedProject is one project's mute state as the store holds it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MutedProject {
    pub path: String,
    pub scopes: MuteScopes,
}

/// ParseMuteScopes reads a list of check names. An unknown name is an error
/// rather than a silent no-op: a typo would otherwise read as "mute nothing",
/// which is the opposite of what was asked for.
pub fn parse_mute_scopes(input: &[String]) -> Res<MuteScopes> {
    let mut out = MuteScopes::default();
    for raw in input {
        match raw.trim().to_lowercase().as_str() {
            MUTE_SCOPE_CI => out.ci = true,
            MUTE_SCOPE_REGISTRY => out.registry = true,
            _ => {
                return Err(Error::Msg(format!(
                    "unknown mute scope {raw:?}: expected {MUTE_SCOPE_CI:?} or {MUTE_SCOPE_REGISTRY:?}"
                )));
            }
        }
    }
    Ok(out)
}

/// VerdictFor derives a project's state from the two halves of the wiring and
/// the checks muted on it. A muted check is neither required nor credited, so a
/// project with one check muted is applied once the other half is present, and
/// is never partial: with a single requirement there is no half-way.
pub fn verdict_for(ci_wired: bool, registry_pinned: bool, muted: MuteScopes) -> AppliedState {
    let (mut required, mut met) = (0, 0);
    if !muted.ci {
        required += 1;
        if ci_wired {
            met += 1;
        }
    }
    if !muted.registry {
        required += 1;
        if registry_pinned {
            met += 1;
        }
    }
    if required == 0 || met == required {
        STATE_APPLIED
    } else if met > 0 {
        STATE_PARTIAL
    } else {
        STATE_NOT_APPLIED
    }
}

/// applyMuted stamps the mute state onto an already-scanned project and derives
/// the verdict again from the halves the scan recorded. That is what makes the
/// toggle reversible without a rescan: the evidence stays put and only what is
/// required of it changes.
///
/// A topic exclusion is the repository's own decision and is left alone; an
/// errored or CI-less project has no verdict to re-derive.
pub(crate) fn apply_muted(p: &mut Project, muted: MuteScopes) {
    if p.exclude_reason.starts_with(EXCLUDE_TOPIC_PREFIX) {
        return;
    }
    p.muted_scopes = muted.list();
    p.exclude_reason = String::new();
    if muted.all() {
        p.exclude_reason = EXCLUDE_MUTED.to_string();
        return;
    }
    if p.skipped || p.applied == STATE_ERROR {
        return;
    }
    p.applied = verdict_for(p.ci_wired, p.registry_pinned, muted);
}

impl Scanner {
    /// RefreshMuted re-reads what has been silenced from the console and
    /// restamps the projects already in memory, so a mute change lands on the
    /// current picture instead of waiting for the next scan.
    pub async fn refresh_muted(&self) -> Res<()> {
        let muted = self.store.list_coverage_muted().await?;
        let set: HashMap<String, MuteScopes> =
            muted.into_iter().map(|m| (m.path, m.scopes)).collect();
        let mut state = self.state.write();
        state.muted = set;
        for i in 0..state.projects.len() {
            let scopes = state
                .muted
                .get(&state.projects[i].path)
                .copied()
                .unwrap_or_default();
            apply_muted(&mut state.projects[i], scopes);
        }
        Ok(())
    }

    /// MutedScopes reports which checks are waived on one project.
    pub fn muted_scopes(&self, project_path: &str) -> MuteScopes {
        self.state
            .read()
            .muted
            .get(project_path)
            .copied()
            .unwrap_or_default()
    }

    /// SetMuted silences the named checks on a project, or brings all of them
    /// back into the measurement.
    ///
    /// Muting also restamps an already-scanned project in memory, so the page and
    /// the coverage number react immediately instead of waiting for the next
    /// scan. The evidence stays on the record either way, which is what makes the
    /// toggle reversible without a rescan: only what is required of the evidence
    /// changes. Unmuting cannot invent a verdict, so a project that was never
    /// scanned only reappears in the table after the next run.
    pub async fn set_muted(&self, project_path: &str, scopes: MuteScopes, actor: &str) -> Res<()> {
        if scopes.any() {
            self.store
                .add_coverage_muted(project_path, actor, scopes)
                .await?;
        } else {
            self.store.remove_coverage_muted(project_path).await?;
        }
        // Restamps every project in memory against the new state, this one
        // included.
        self.refresh_muted().await?;

        {
            let mut state = self.state.write();
            let found = state.projects.iter().any(|p| p.path == project_path);
            if !found && !scopes.all() {
                // Never scanned, only listed as out of scope. It returns to the
                // table on the next scan.
                state
                    .excluded_projects
                    .retain(|p| !(p.path == project_path && p.reason == EXCLUDE_MUTED));
            }
        }

        self.persist().await?;
        tracing::info!(
            project = project_path,
            scopes = scopes.list().join(","),
            by = actor,
            "coverage: mute changed"
        );
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::coverage::scan::tests::{
        HOST, fake_gitlab, fake_project, new_mem_store, new_test_scanner, with_host,
    };
    use crate::coverage::types::{STATE_APPLIED, STATE_NOT_APPLIED, STATE_PARTIAL};
    use crate::coverage::{MUTE_SCOPE_REGISTRY, MuteScopes, verdict_for};

    #[tokio::test]
    async fn muting_moves_the_number_without_a_rescan() {
        let srv = fake_gitlab(vec![
            fake_project(
                1,
                "team/wired",
                &["main"],
                &[
                    (
                        "main:.gitlab-ci.yml",
                        format!("image: {HOST}/docker/base:1\n"),
                    ),
                    (
                        "main:.npmrc",
                        format!("registry=https://{HOST}/npm/npmjs/\n"),
                    ),
                ],
            ),
            fake_project(
                2,
                "team/bare",
                &["main"],
                &[("main:.gitlab-ci.yml", "script: echo hi\n".to_string())],
            ),
        ])
        .await;
        let store = new_mem_store(with_host(HOST));
        let scanner = new_test_scanner(store, &srv, None);
        scanner.scan("tester").await.expect("Scan");
        assert_eq!(scanner.summary().percent, 50, "want 50 before the opt-out");

        scanner
            .set_muted(
                "team/bare",
                MuteScopes {
                    ci: true,
                    registry: true,
                },
                "admin",
            )
            .await
            .expect("SetMuted");
        let summary = scanner.summary();
        assert!(
            summary.percent == 100 && summary.target == 1 && summary.excluded == 1,
            "summary after excluding = {summary:?}, want the project out of the denominator immediately"
        );

        // The verdict stays on the record, which is what makes it reversible.
        scanner
            .set_muted("team/bare", MuteScopes::default(), "admin")
            .await
            .expect("SetMuted");
        assert_eq!(
            scanner.summary().percent,
            50,
            "want 50 again after re-including"
        );
    }

    /// Muting one check leaves the project in the denominator and stops asking for
    /// that half, which is the difference between "this project is not our business"
    /// and "this project has no packages to pin".
    #[tokio::test]
    async fn muting_one_check_keeps_the_project_measured() {
        let srv = fake_gitlab(vec![fake_project(
            1,
            "team/ci-only",
            &["main"],
            &[(
                "main:.gitlab-ci.yml",
                format!("image: {HOST}/docker/base:1\n"),
            )],
        )])
        .await;
        let store = new_mem_store(with_host(HOST));
        let scanner = new_test_scanner(store, &srv, None);
        scanner.scan("tester").await.expect("Scan");
        let got = scanner.summary();
        assert!(
            got.partial == 1 && got.percent == 0,
            "summary = {got:?}, want one partial project before the waiver"
        );

        scanner
            .set_muted(
                "team/ci-only",
                MuteScopes {
                    registry: true,
                    ..MuteScopes::default()
                },
                "admin",
            )
            .await
            .expect("SetMuted");
        let summary = scanner.summary();
        assert!(
            summary.target == 1
                && summary.applied == 1
                && summary.excluded == 0
                && summary.percent == 100,
            "summary after waiving the registry half = {summary:?}, want it applied and still counted"
        );
        let p = scanner.project("team/ci-only").expect("the project");
        assert_eq!(
            p.muted_scopes,
            vec![MUTE_SCOPE_REGISTRY.to_string()],
            "project muted scopes"
        );

        // Waiving the other half as well is the whole project going out, the same
        // state a bare mute used to produce.
        scanner
            .set_muted(
                "team/ci-only",
                MuteScopes {
                    ci: true,
                    registry: true,
                },
                "admin",
            )
            .await
            .expect("SetMuted");
        let summary = scanner.summary();
        assert!(
            summary.target == 0 && summary.excluded == 1,
            "summary after waiving both = {summary:?}, want the project out of the measurement"
        );

        // And a scan run while one check is waived reaches the same verdict, so the
        // number does not move under a rescan.
        scanner
            .set_muted(
                "team/ci-only",
                MuteScopes {
                    registry: true,
                    ..MuteScopes::default()
                },
                "admin",
            )
            .await
            .expect("SetMuted");
        scanner.scan("tester").await.expect("rescan");
        let got = scanner.summary();
        assert!(
            got.applied == 1 && got.percent == 100,
            "summary after a rescan = {got:?}, want the waiver to survive it"
        );
    }

    #[test]
    fn verdict_for_waived_checks() {
        let cases = [
            (
                "both halves",
                true,
                true,
                MuteScopes {
                    ci: false,
                    registry: false,
                },
                STATE_APPLIED,
            ),
            (
                "ci only",
                true,
                false,
                MuteScopes {
                    ci: false,
                    registry: false,
                },
                STATE_PARTIAL,
            ),
            (
                "neither",
                false,
                false,
                MuteScopes {
                    ci: false,
                    registry: false,
                },
                STATE_NOT_APPLIED,
            ),
            (
                "ci only, registry waived",
                true,
                false,
                MuteScopes {
                    ci: false,
                    registry: true,
                },
                STATE_APPLIED,
            ),
            (
                "ci only, ci waived",
                true,
                false,
                MuteScopes {
                    ci: true,
                    registry: false,
                },
                STATE_NOT_APPLIED,
            ),
            (
                "registry only, ci waived",
                false,
                true,
                MuteScopes {
                    ci: true,
                    registry: false,
                },
                STATE_APPLIED,
            ),
        ];
        for (name, ci_wired, registry_pinned, muted, want) in cases {
            assert_eq!(
                verdict_for(ci_wired, registry_pinned, muted),
                want,
                "{name}: verdict_for"
            );
        }
    }
}

//! Reads made for one project on demand rather than during the crawl: its CI
//! definitions and the tip commit behind its verdict.

use serde::Deserialize;

use crate::coverage::evidence::{CI_FILE_RE, VENDOR_RE, body_matches};
use crate::coverage::gitlab::{GitLabClient, get_pages};
use crate::coverage::types::{LastCommit, Pipeline, PipelineFile};
use crate::coverage::views::{parse_gitlab_time, path_escape};
use crate::coverage::{Error, Res, Scanner};

/// pipelineFileMaxBytes caps one file in the pipeline viewer so a single huge
/// YAML cannot flood the console.
const PIPELINE_FILE_MAX_BYTES: usize = 128 << 10;
/// pipelineMaxFiles caps how many CI files one ref returns.
const PIPELINE_MAX_FILES: usize = 20;

#[derive(Debug, Clone, Default, Deserialize)]
struct RawCommit {
    #[serde(default)]
    short_id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    author_name: String,
    #[serde(default)]
    committed_date: String,
    #[serde(default)]
    web_url: String,
}

/// Deserialized from the same tree listing the crawl reads.
#[derive(Debug, Clone, Default, Deserialize)]
struct RawTreeEntry {
    #[serde(default, rename = "type")]
    entry_type: String,
    #[serde(default)]
    path: String,
}

impl Scanner {
    /// resolveProject finds a project's id and default branch, from the last
    /// scan when possible and from GitLab otherwise. The third result is the
    /// branch the verdict came from, empty when there is no scanned verdict.
    async fn resolve_project(
        &self,
        client: &GitLabClient,
        project_path: &str,
    ) -> Res<(i64, String, String)> {
        if let Some(known) = self.project(project_path) {
            return Ok((known.id, known.default_branch, known.branch));
        }
        let raw: crate::coverage::scan::RawProject = client
            .get_json(&format!("projects/{}", path_escape(project_path)))
            .await?;
        Ok((raw.id, raw.default_branch, String::new()))
    }

    /// Pipeline returns the CI definitions of one ref for the pipeline viewer.
    ///
    /// Only paths matching the GitLab CI pattern are ever read. That allowlist is
    /// the whole security boundary of this call: it must never widen to arbitrary
    /// repository files, since the viewer exists to show how a project builds,
    /// not to expose its source.
    pub async fn pipeline(&self, project_path: &str, requested_ref: &str) -> Res<Pipeline> {
        if !self.enabled() {
            return Err(Error::Msg(
                "coverage scanning is not configured".to_string(),
            ));
        }
        let client = self.new_client();
        let (project_id, default_branch, verdict_branch) =
            self.resolve_project(&client, project_path).await?;

        // Default to the branch the wiring was found on, so the viewer shows the
        // pipeline the verdict was actually based on.
        let mut r#ref = requested_ref.to_string();
        if r#ref.is_empty() {
            r#ref = verdict_branch;
        }
        if r#ref.is_empty() {
            r#ref = default_branch;
        }
        if r#ref.is_empty() {
            r#ref = "HEAD".to_string();
        }

        let tree: Vec<RawTreeEntry> = get_pages(
            &client,
            &format!(
                "projects/{project_id}/repository/tree?ref={}&recursive=true&per_page=100",
                path_escape(&r#ref)
            ),
        )
        .await?;

        let mut ci_paths: Vec<String> = Vec::with_capacity(8);
        for entry in &tree {
            if entry.entry_type == "blob"
                && !VENDOR_RE.is_match(&entry.path)
                && CI_FILE_RE.is_match(&entry.path)
            {
                ci_paths.push(entry.path.clone());
            }
        }
        ci_paths.sort();
        if ci_paths.len() > PIPELINE_MAX_FILES {
            ci_paths.truncate(PIPELINE_MAX_FILES);
        }

        let host = self.match_host();
        let mut files: Vec<PipelineFile> = Vec::with_capacity(ci_paths.len());
        for file_path in &ci_paths {
            // Re-check the allowlist on the exact path about to be read.
            if !CI_FILE_RE.is_match(file_path) {
                continue;
            }
            let body = match client
                .get_text(&format!(
                    "projects/{project_id}/repository/files/{}/raw?ref={}",
                    path_escape(file_path),
                    path_escape(&r#ref)
                ))
                .await
            {
                Ok(body) => body,
                Err(e) if e.is_not_found() => continue,
                Err(e) => return Err(e),
            };
            let truncated = body.len() > PIPELINE_FILE_MAX_BYTES;
            let content = if truncated {
                truncate_at_char_boundary(&body, PIPELINE_FILE_MAX_BYTES).to_string()
            } else {
                body.clone()
            };
            files.push(PipelineFile {
                path: file_path.clone(),
                content,
                truncated,
                matches_forklift: body_matches(&host, file_path, &body),
            });
        }
        Ok(Pipeline {
            project_path: project_path.to_string(),
            ref_: r#ref,
            files,
        })
    }

    /// LastCommit returns the tip commit of the branch the verdict came from, or
    /// of the default branch. `None` when the project has no branch to read.
    pub async fn last_commit(&self, project_path: &str) -> Res<Option<LastCommit>> {
        if !self.enabled() {
            return Err(Error::Msg(
                "coverage scanning is not configured".to_string(),
            ));
        }
        let client = self.new_client();
        let (project_id, default_branch, verdict_branch) =
            self.resolve_project(&client, project_path).await?;
        let mut r#ref = verdict_branch;
        if r#ref.is_empty() {
            r#ref = default_branch;
        }
        if r#ref.is_empty() {
            return Ok(None);
        }

        let commits: Vec<RawCommit> = match client
            .get_json(&format!(
                "projects/{project_id}/repository/commits?ref_name={}&per_page=1",
                path_escape(&r#ref)
            ))
            .await
        {
            Ok(commits) => commits,
            Err(e) if e.is_not_found() => return Ok(None),
            Err(e) => return Err(e),
        };
        let c = match commits.first() {
            Some(c) => c,
            None => return Ok(None),
        };
        Ok(Some(LastCommit {
            ref_: r#ref,
            short_id: c.short_id.clone(),
            title: c.title.clone(),
            author_name: c.author_name.clone(),
            committed_at: parse_gitlab_time(&c.committed_date),
            web_url: c.web_url.clone(),
        }))
    }
}

fn truncate_at_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

//! Idle retention for OCI repositories, applied per tag.
//!
//! Deleting manifest or blob rows one by one would break still-tagged images
//! (see `oci_prune`), so the reaper removes only the tag of an image that has
//! not been pulled within `idle_ttl`. The reachability prune then collects the
//! untagged manifest and the layers nothing else references.

use chrono::{DateTime, Utc};
use http::StatusCode;

use crate::audit;
use crate::meta::{self, OCITag, Repository};

use super::Manager;
use super::oci::oci_manifest_path;

/// One tag with the most recent time it was pushed or pulled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OciTagActivity {
    pub name: String,
    pub tag: String,
    pub digest: String,
    pub last_active: DateTime<Utc>,
}

/// Returns when an artifact idle since `last_active` becomes eligible for
/// deletion, `None` when retention is off.
pub fn idle_expiry(ttl: std::time::Duration, last_active: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if ttl.is_zero() {
        return None;
    }
    let ttl = chrono::TimeDelta::from_std(ttl).ok()?;
    last_active.checked_add_signed(ttl)
}

/// The subject digest of a cosign-style attachment tag
/// (`sha256-<hex>.sig`, `.att`, `.sbom`, or the bare referrers fallback tag).
fn attachment_subject(tag: &str) -> Option<String> {
    let rest = tag.strip_prefix("sha256-")?;
    let hex = rest.get(..64)?;
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let suffix = &rest[64..];
    (suffix.is_empty() || suffix.starts_with('.')).then(|| format!("sha256:{hex}"))
}

/// Lifts every attachment tag to the activity of the image it signs, so a
/// signature or SBOM lives as long as its image instead of expiring because
/// clients that skip verification never pull it.
pub(crate) fn propagate_attachment_activity(entries: &mut [OciTagActivity]) {
    let lifted: Vec<Option<DateTime<Utc>>> = entries
        .iter()
        .map(|entry| {
            let subject = attachment_subject(&entry.tag)?;
            entries
                .iter()
                .filter(|other| other.name == entry.name && other.digest == subject)
                .map(|other| other.last_active)
                .max()
        })
        .collect();
    for (entry, lifted) in entries.iter_mut().zip(lifted) {
        if let Some(at) = lifted
            && at > entry.last_active
        {
            entry.last_active = at;
        }
    }
}

impl Manager {
    /// Computes when a tag was last pushed or pulled: the tag write, the tagged
    /// manifest's last serve, and for an index the last serve of any child
    /// manifest (a digest-pinned platform pull never touches the index). The
    /// children are read only when the cheaper signals fall before `floor`.
    /// `None` when the manifest is missing or unreadable, so the tag is left
    /// alone.
    pub(crate) async fn oci_tag_last_active(
        &self,
        repo_id: i64,
        tag: &OCITag,
        floor: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        let art = self
            .store
            .get_artifact(repo_id, &oci_manifest_path(&tag.name, &tag.manifest_digest))
            .await
            .ok()?;
        let mut last = tag.updated_at.max(art.last_accessed_at);
        if last >= floor {
            return Some(last);
        }
        let doc = self.read_oci_manifest(&art).await.ok()?;
        for child in &doc.manifests {
            if let Ok(child_art) = self
                .store
                .get_artifact(repo_id, &oci_manifest_path(&tag.name, &child.digest))
                .await
            {
                last = last.max(child_art.last_accessed_at);
            }
        }
        Some(last)
    }

    /// Lists the tags of one OCI repository that have not been pushed or pulled
    /// since `cutoff`, least recently active first.
    pub(crate) async fn idle_oci_tags(
        &self,
        repo_id: i64,
        cutoff: DateTime<Utc>,
    ) -> Result<Vec<OciTagActivity>, meta::Error> {
        let tags = self.store.list_oci_tag_rows(repo_id).await?;
        let mut entries = Vec::with_capacity(tags.len());
        for tag in &tags {
            if let Some(last_active) = self.oci_tag_last_active(repo_id, tag, cutoff).await {
                entries.push(OciTagActivity {
                    name: tag.name.clone(),
                    tag: tag.tag.clone(),
                    digest: tag.manifest_digest.clone(),
                    last_active,
                });
            }
        }
        propagate_attachment_activity(&mut entries);
        entries.retain(|e| e.last_active < cutoff);
        entries.sort_by(|a, b| {
            a.last_active
                .cmp(&b.last_active)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.tag.cmp(&b.tag))
        });
        Ok(entries)
    }

    /// Removes the tags of one OCI repository that have not been pushed or
    /// pulled since `cutoff`, returning how many it removed.
    pub(crate) async fn reap_oci_tags(
        &self,
        repo: &Repository,
        cutoff: DateTime<Utc>,
        ttl: std::time::Duration,
    ) -> Result<i64, meta::Error> {
        let mut removed = 0;
        for entry in self.idle_oci_tags(repo.id, cutoff).await? {
            match self
                .store
                .delete_oci_tag_if_digest(repo.id, &entry.name, &entry.tag, &entry.digest)
                .await
            {
                Ok(()) => {}
                Err(e) if e.is_not_found() => continue,
                Err(e) => return Err(e),
            }
            removed += 1;
            self.ttl_expired.with_label_values(&[&repo.name]).inc();
            if let Some(rec) = &self.rec {
                rec.record(audit::Event {
                    repo: repo.name.clone(),
                    action: meta::EVENT_TTL_EXPIRE.to_string(),
                    path: oci_manifest_path(&entry.name, &entry.tag),
                    username: "system".to_string(),
                    status: i64::from(StatusCode::OK.as_u16()),
                    detail_json: super::reaper::retention_detail(
                        ttl,
                        entry.last_active,
                        Some(&entry.digest),
                    ),
                    ..Default::default()
                });
            }
            tracing::debug!(
                repo = %repo.name, name = %entry.name, tag = %entry.tag,
                "idle oci tag reaped"
            );
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn at(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, hour, 0, 0)
            .single()
            .expect("valid instant")
    }

    fn entry(name: &str, tag: &str, digest: &str, hour: u32) -> OciTagActivity {
        OciTagActivity {
            name: name.to_string(),
            tag: tag.to_string(),
            digest: digest.to_string(),
            last_active: at(hour),
        }
    }

    #[test]
    fn attachment_subject_cases() {
        let hex = "a".repeat(64);
        let want = Some(format!("sha256:{hex}"));
        assert_eq!(attachment_subject(&format!("sha256-{hex}.sig")), want);
        assert_eq!(attachment_subject(&format!("sha256-{hex}.att")), want);
        assert_eq!(attachment_subject(&format!("sha256-{hex}")), want);
        assert_eq!(attachment_subject(&format!("sha256-{hex}x")), None);
        assert_eq!(attachment_subject("sha256-abc.sig"), None);
        assert_eq!(
            attachment_subject(&format!("sha256-{}.sig", "z".repeat(64))),
            None
        );
        assert_eq!(attachment_subject("v1.0.0"), None);
    }

    #[test]
    fn attachment_tags_follow_their_image() {
        let hex = "b".repeat(64);
        let subject = format!("sha256:{hex}");
        let mut entries = vec![
            entry("app", "v1", &subject, 10),
            entry("app", &format!("sha256-{hex}.sig"), "sha256:sig", 1),
            entry("other", &format!("sha256-{hex}.sig"), "sha256:sig2", 2),
            entry("app", "old", "sha256:old", 3),
        ];
        propagate_attachment_activity(&mut entries);
        assert_eq!(
            entries[1].last_active,
            at(10),
            "signature lifted to its image"
        );
        assert_eq!(
            entries[2].last_active,
            at(2),
            "subject in another name ignored"
        );
        assert_eq!(entries[3].last_active, at(3), "plain tag unchanged");
    }

    #[test]
    fn idle_expiry_cases() {
        assert_eq!(idle_expiry(std::time::Duration::ZERO, at(1)), None);
        assert_eq!(
            idle_expiry(std::time::Duration::from_hours(2), at(1)),
            Some(at(3))
        );
    }
}

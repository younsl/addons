use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Link {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub title: String,
    pub username: String,
    pub password: String,
    pub url: String,
    pub notes: String,
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub links: Vec<Link>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ItemSummary {
    pub id: String,
    pub title: String,
    pub username: String,
    pub url: String,
    pub group: String,
    pub updated_at: DateTime<Utc>,
}

impl From<&Item> for ItemSummary {
    fn from(i: &Item) -> Self {
        Self {
            id: i.id.clone(),
            title: i.title.clone(),
            username: i.username.clone(),
            url: i.url.clone(),
            group: i.group.clone(),
            updated_at: i.updated_at,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewItem {
    pub title: String,
    #[serde(default)]
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub links: Vec<Link>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateItem {
    pub title: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub url: Option<String>,
    pub notes: Option<String>,
    pub group: Option<String>,
    pub links: Option<Vec<Link>>,
}

impl Item {
    pub fn from_new(input: NewItem) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4().to_string(),
            title: input.title,
            username: input.username,
            password: input.password,
            url: input.url,
            notes: input.notes,
            group: input.group.trim().to_string(),
            links: input.links,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn apply_update(&mut self, u: UpdateItem) {
        if let Some(v) = u.title {
            self.title = v;
        }
        if let Some(v) = u.username {
            self.username = v;
        }
        if let Some(v) = u.password {
            self.password = v;
        }
        if let Some(v) = u.url {
            self.url = v;
        }
        if let Some(v) = u.notes {
            self.notes = v;
        }
        if let Some(v) = u.group {
            self.group = v.trim().to_string();
        }
        if let Some(v) = u.links {
            self.links = v;
        }
        self.updated_at = Utc::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> NewItem {
        NewItem {
            title: "GitHub".into(),
            username: "octo".into(),
            password: "hunter2".into(),
            url: "https://example.com".into(),
            notes: "note".into(),
            group: "  work/dev  ".into(),
            links: vec![Link {
                name: "docs".into(),
                description: String::new(),
                url: "https://example.com/docs".into(),
            }],
        }
    }

    #[test]
    fn from_new_trims_group_and_sets_timestamps() {
        let item = Item::from_new(sample());
        assert_eq!(item.group, "work/dev");
        assert_eq!(item.title, "GitHub");
        assert_eq!(item.links.len(), 1);
        assert_eq!(item.created_at, item.updated_at);
        assert!(Uuid::parse_str(&item.id).is_ok());
    }

    #[test]
    fn from_new_generates_unique_ids() {
        assert_ne!(Item::from_new(sample()).id, Item::from_new(sample()).id);
    }

    #[test]
    fn apply_update_replaces_every_field() {
        let mut item = Item::from_new(sample());
        let before = item.updated_at;
        item.apply_update(UpdateItem {
            title: Some("GitLab".into()),
            username: Some("tanuki".into()),
            password: Some("new-secret".into()),
            url: Some("https://example.com/gl".into()),
            notes: Some("n2".into()),
            group: Some(" personal ".into()),
            links: Some(Vec::new()),
        });
        assert_eq!(item.title, "GitLab");
        assert_eq!(item.username, "tanuki");
        assert_eq!(item.password, "new-secret");
        assert_eq!(item.url, "https://example.com/gl");
        assert_eq!(item.notes, "n2");
        assert_eq!(item.group, "personal");
        assert!(item.links.is_empty());
        assert!(item.updated_at >= before);
    }

    #[test]
    fn apply_update_with_none_keeps_fields() {
        let mut item = Item::from_new(sample());
        item.apply_update(UpdateItem {
            title: None,
            username: None,
            password: None,
            url: None,
            notes: None,
            group: None,
            links: None,
        });
        assert_eq!(item.title, "GitHub");
        assert_eq!(item.username, "octo");
        assert_eq!(item.group, "work/dev");
        assert_eq!(item.links.len(), 1);
    }

    #[test]
    fn summary_copies_public_fields() {
        let item = Item::from_new(sample());
        let summary = ItemSummary::from(&item);
        assert_eq!(summary.id, item.id);
        assert_eq!(summary.title, item.title);
        assert_eq!(summary.username, item.username);
        assert_eq!(summary.url, item.url);
        assert_eq!(summary.group, item.group);
        assert_eq!(summary.updated_at, item.updated_at);
    }

    #[test]
    fn deserialize_new_item_applies_defaults() {
        let parsed: NewItem = serde_json::from_str(r#"{"title":"t","password":"p"}"#).unwrap();
        assert_eq!(parsed.username, "");
        assert_eq!(parsed.url, "");
        assert!(parsed.links.is_empty());
    }

    #[test]
    fn deserialize_item_without_group_or_links() {
        let raw = r#"{"id":"1","title":"t","username":"u","password":"p","url":"","notes":"",
            "created_at":"2024-01-01T00:00:00Z","updated_at":"2024-01-01T00:00:00Z"}"#;
        let item: Item = serde_json::from_str(raw).unwrap();
        assert_eq!(item.group, "");
        assert!(item.links.is_empty());
        let link: Link = serde_json::from_str("{}").unwrap();
        assert!(link.name.is_empty() && link.url.is_empty());
    }
}

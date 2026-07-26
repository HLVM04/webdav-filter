use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Resource {
    pub source_path: String,
    pub name: String,
    pub is_collection: bool,
    pub size: u64,
    pub modified: Option<DateTime<Utc>>,
    pub etag: Option<String>,
    pub content_type: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceItem {
    pub id: String,
    pub root: Resource,
    pub descendants: Vec<Resource>,
}

impl SourceItem {
    pub fn total_size(&self) -> u64 {
        if self.root.is_collection {
            self.descendants
                .iter()
                .filter(|r| !r.is_collection)
                .map(|r| r.size)
                .sum()
        } else {
            self.root.size
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct VirtualResource {
    pub virtual_path: String,
    pub source_path: Option<String>,
    pub item_root: Option<String>,
    pub resource: Resource,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub generation: u64,
    pub refreshed_at: Option<DateTime<Utc>>,
    pub resources: HashMap<String, VirtualResource>,
    pub children: HashMap<String, Vec<String>>,
}

impl Snapshot {
    pub fn empty() -> Self {
        let root = Resource {
            source_path: "/".into(),
            name: String::new(),
            is_collection: true,
            size: 0,
            modified: None,
            etag: None,
            content_type: None,
        };
        let mut snapshot = Self::default();
        snapshot.resources.insert(
            "/".into(),
            VirtualResource {
                virtual_path: "/".into(),
                source_path: None,
                item_root: None,
                resource: root,
            },
        );
        snapshot.children.insert("/".into(), Vec::new());
        snapshot
    }

    pub fn get(&self, path: &str) -> Option<&VirtualResource> {
        self.resources.get(path)
    }

    pub fn children_of(&self, path: &str) -> impl Iterator<Item = &VirtualResource> {
        self.children
            .get(path)
            .into_iter()
            .flatten()
            .filter_map(|child| self.resources.get(child))
    }
}

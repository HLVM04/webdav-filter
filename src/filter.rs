use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result, bail};
use regex::{Regex, RegexBuilder};
use sha2::{Digest, Sha256};

use crate::{
    config::{DirectoryConfig, FilterConfig},
    model::{Resource, Snapshot, SourceItem, VirtualResource},
};

#[derive(Debug)]
pub struct Classifier {
    directories: Vec<CompiledDirectory>,
    episode: Regex,
}

#[derive(Debug)]
struct CompiledDirectory {
    name: String,
    group: String,
    group_order: i64,
    biggest_only: bool,
    size_lte: Option<u64>,
    size_gte: Option<u64>,
    filters: Vec<Filter>,
}

#[derive(Debug)]
enum Filter {
    Id(String),
    Regex(Regex, bool),
    Contains(String, bool, bool),
    FileRegex(Regex, bool),
    FileContains(String, bool, bool),
    HasEpisodes(bool),
    Size(u64, Compare),
    FileSize(u64, Compare),
    And(Vec<Filter>),
    Or(Vec<Filter>),
}

#[derive(Debug, Clone, Copy)]
enum Compare {
    Gte,
    Lte,
}

impl Classifier {
    pub fn compile(configs: &BTreeMap<String, DirectoryConfig>) -> Result<Self> {
        let mut directories = Vec::with_capacity(configs.len());
        let mut names = HashSet::new();
        for (name, config) in configs {
            if config.filters.is_empty() {
                bail!("directory {name:?} must have at least one filter");
            }
            let filters = config
                .filters
                .iter()
                .map(Filter::compile)
                .collect::<Result<Vec<_>>>()
                .with_context(|| format!("compile filters for directory {name:?}"))?;
            let cleaned_name = clean_name(name);
            if !names.insert(cleaned_name.clone()) {
                bail!("multiple directory names normalize to {cleaned_name:?}");
            }
            directories.push(CompiledDirectory {
                name: cleaned_name,
                group: config
                    .group
                    .clone()
                    .unwrap_or_else(|| format!("__directory__{name}")),
                group_order: config.group_order,
                biggest_only: config.only_show_the_biggest_file,
                size_lte: config.only_show_files_with_size_lte,
                size_gte: config.only_show_files_with_size_gte,
                filters,
            });
        }
        directories.sort_by(|a, b| {
            a.group
                .cmp(&b.group)
                .then(a.group_order.cmp(&b.group_order))
                .then(a.name.cmp(&b.name))
        });
        Ok(Self {
            directories,
            episode: Regex::new(r"(?i)(?:^|[ ._\-\[])(?:s\d{1,3}e\d{1,3}(?:e\d{1,3})*|\d{1,3}x\d{1,3})(?:[ ._\-\]]|$)").unwrap(),
        })
    }

    pub fn build_snapshot(&self, items: &[SourceItem], generation: u64) -> Snapshot {
        let mut snapshot = Snapshot::empty();
        snapshot.generation = generation;
        snapshot.refreshed_at = Some(chrono::Utc::now());
        let mut groups_by_item: HashMap<&str, HashSet<&str>> = HashMap::new();

        for directory in &self.directories {
            add_category(&mut snapshot, &directory.name);
        }

        for item in items {
            for directory in &self.directories {
                let used = groups_by_item.entry(&item.id).or_default();
                if used.contains(directory.group.as_str())
                    || !directory.matches(item, &self.episode)
                {
                    continue;
                }
                used.insert(&directory.group);
                add_item(&mut snapshot, directory, item);
            }
        }
        for children in snapshot.children.values_mut() {
            children.sort();
            children.dedup();
        }
        snapshot
    }
}

impl CompiledDirectory {
    fn matches(&self, item: &SourceItem, episode: &Regex) -> bool {
        self.filters
            .iter()
            .any(|filter| filter.matches(item, episode))
    }

    fn visible_files<'a>(&self, item: &'a SourceItem) -> HashSet<&'a str> {
        let mut files: Vec<&Resource> = item
            .descendants
            .iter()
            .filter(|r| !r.is_collection)
            .filter(|r| self.size_lte.is_none_or(|limit| r.size <= limit))
            .filter(|r| self.size_gte.is_none_or(|limit| r.size >= limit))
            .collect();
        if !item.root.is_collection {
            files.push(&item.root);
        }
        if self.biggest_only {
            files
                .into_iter()
                .max_by_key(|r| r.size)
                .map(|r| HashSet::from([r.source_path.as_str()]))
                .unwrap_or_default()
        } else {
            files.into_iter().map(|r| r.source_path.as_str()).collect()
        }
    }
}

impl Filter {
    fn compile(config: &FilterConfig) -> Result<Self> {
        Ok(match config {
            FilterConfig::Id { id } => Self::Id(id.clone()),
            FilterConfig::Regex { regex } => Self::Regex(compile_regex(regex)?, false),
            FilterConfig::NotRegex { not_regex } => Self::Regex(compile_regex(not_regex)?, true),
            FilterConfig::Contains { contains } => {
                Self::Contains(contains.to_lowercase(), false, false)
            }
            FilterConfig::ContainsStrict { contains_strict } => {
                Self::Contains(contains_strict.clone(), true, false)
            }
            FilterConfig::NotContains { not_contains } => {
                Self::Contains(not_contains.to_lowercase(), false, true)
            }
            FilterConfig::NotContainsStrict {
                not_contains_strict,
            } => Self::Contains(not_contains_strict.clone(), true, true),
            FilterConfig::AnyFileInsideRegex {
                any_file_inside_regex,
            } => Self::FileRegex(compile_regex(any_file_inside_regex)?, false),
            FilterConfig::AnyFileInsideNotRegex {
                any_file_inside_not_regex,
            } => Self::FileRegex(compile_regex(any_file_inside_not_regex)?, true),
            FilterConfig::AnyFileInsideContains {
                any_file_inside_contains,
            } => Self::FileContains(any_file_inside_contains.to_lowercase(), false, false),
            FilterConfig::AnyFileInsideContainsStrict {
                any_file_inside_contains_strict,
            } => Self::FileContains(any_file_inside_contains_strict.clone(), true, false),
            FilterConfig::AnyFileInsideNotContains {
                any_file_inside_not_contains,
            } => Self::FileContains(any_file_inside_not_contains.to_lowercase(), false, true),
            FilterConfig::AnyFileInsideNotContainsStrict {
                any_file_inside_not_contains_strict,
            } => Self::FileContains(any_file_inside_not_contains_strict.clone(), true, true),
            FilterConfig::HasEpisodes { has_episodes } => Self::HasEpisodes(*has_episodes),
            FilterConfig::SizeGte { size_gte } => Self::Size(*size_gte, Compare::Gte),
            FilterConfig::SizeLte { size_lte } => Self::Size(*size_lte, Compare::Lte),
            FilterConfig::AnyFileInsideSizeGte {
                any_file_inside_size_gte,
            } => Self::FileSize(*any_file_inside_size_gte, Compare::Gte),
            FilterConfig::AnyFileInsideSizeLte {
                any_file_inside_size_lte,
            } => Self::FileSize(*any_file_inside_size_lte, Compare::Lte),
            FilterConfig::And { and } => {
                Self::And(and.iter().map(Self::compile).collect::<Result<_>>()?)
            }
            FilterConfig::Or { or } => {
                Self::Or(or.iter().map(Self::compile).collect::<Result<_>>()?)
            }
        })
    }

    fn matches(&self, item: &SourceItem, episode: &Regex) -> bool {
        match self {
            Self::Id(id) => &item.id == id,
            Self::Regex(re, invert) => re.is_match(&item.root.name) != *invert,
            Self::Contains(needle, strict, invert) => {
                contains(&item.root.name, needle, *strict) != *invert
            }
            Self::FileRegex(re, invert) => {
                item_files(item).any(|r| re.is_match(&r.name)) != *invert
            }
            Self::FileContains(needle, strict, invert) => {
                item_files(item).any(|r| contains(&r.name, needle, *strict)) != *invert
            }
            Self::HasEpisodes(expected) => {
                item_files(item)
                    .filter(|r| is_video(&r.name))
                    .any(|r| episode.is_match(&r.name))
                    == *expected
            }
            Self::Size(value, compare) => compare.matches(item.total_size(), *value),
            Self::FileSize(value, compare) => {
                item_files(item).any(|r| compare.matches(r.size, *value))
            }
            Self::And(filters) => filters.iter().all(|f| f.matches(item, episode)),
            Self::Or(filters) => filters.iter().any(|f| f.matches(item, episode)),
        }
    }
}

fn item_files(item: &SourceItem) -> impl Iterator<Item = &Resource> {
    item.descendants
        .iter()
        .chain((!item.root.is_collection).then_some(&item.root))
        .filter(|resource| !resource.is_collection)
}

impl Compare {
    fn matches(self, actual: u64, expected: u64) -> bool {
        match self {
            Self::Gte => actual >= expected,
            Self::Lte => actual <= expected,
        }
    }
}

fn compile_regex(input: &str) -> Result<Regex> {
    let (pattern, flags) = if let Some(rest) = input.strip_prefix('/') {
        let slash = rest
            .rfind('/')
            .context("slash-delimited regex is missing closing slash")?;
        (&rest[..slash], &rest[slash + 1..])
    } else {
        (input, "")
    };
    if flags.chars().any(|flag| flag != 'i' && flag != 'x') {
        bail!("unsupported regex flags {flags:?}; supported flags are i and x");
    }
    RegexBuilder::new(pattern)
        .case_insensitive(flags.contains('i'))
        .ignore_whitespace(flags.contains('x'))
        .build()
        .context("invalid regex")
}

fn contains(haystack: &str, needle: &str, strict: bool) -> bool {
    if strict {
        haystack.contains(needle)
    } else {
        haystack.to_lowercase().contains(needle)
    }
}

fn is_video(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".mkv", ".mp4", ".m4v", ".avi", ".mov", ".ts", ".webm"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

fn add_category(snapshot: &mut Snapshot, category: &str) {
    let path = format!("/{category}");
    if snapshot.resources.contains_key(&path) {
        return;
    }
    let resource = Resource {
        source_path: String::new(),
        name: category.into(),
        is_collection: true,
        size: 0,
        modified: None,
        etag: None,
        content_type: None,
    };
    insert(
        snapshot,
        "/",
        VirtualResource {
            virtual_path: path,
            source_path: None,
            item_root: None,
            resource,
        },
    );
}

fn add_item(snapshot: &mut Snapshot, directory: &CompiledDirectory, item: &SourceItem) {
    let category = format!("/{}", directory.name);
    let base_name = unique_name(snapshot, &category, &clean_name(&item.root.name), &item.id);
    let item_path = format!("{category}/{base_name}");
    let mut root = item.root.clone();
    root.name = base_name;
    insert(
        snapshot,
        &category,
        VirtualResource {
            virtual_path: item_path.clone(),
            source_path: Some(item.root.source_path.clone()),
            item_root: Some(item.root.source_path.clone()),
            resource: root,
        },
    );
    if !item.root.is_collection {
        return;
    }

    let visible = directory.visible_files(item);
    let resources: HashMap<&str, &Resource> = item
        .descendants
        .iter()
        .map(|r| (r.source_path.as_str(), r))
        .collect();
    let mut selected = HashSet::new();
    for path in visible {
        selected.insert(path);
        let mut current = parent_path(path);
        while current != item.root.source_path && current.starts_with(&item.root.source_path) {
            selected.insert(current);
            current = parent_path(current);
        }
    }
    let mut ordered: Vec<&str> = selected.into_iter().collect();
    ordered.sort_by_key(|path| path.matches('/').count());
    for source_path in ordered {
        let Some(resource) = resources.get(source_path) else {
            continue;
        };
        let relative = source_path
            .strip_prefix(item.root.source_path.trim_end_matches('/'))
            .unwrap_or(source_path)
            .trim_start_matches('/');
        if relative.is_empty() {
            continue;
        }
        let virtual_path = format!("{item_path}/{relative}");
        let parent = parent_path(&virtual_path).to_owned();
        insert(
            snapshot,
            &parent,
            VirtualResource {
                virtual_path,
                source_path: Some(source_path.into()),
                item_root: Some(item.root.source_path.clone()),
                resource: (*resource).clone(),
            },
        );
    }
}

fn insert(snapshot: &mut Snapshot, parent: &str, resource: VirtualResource) {
    snapshot
        .children
        .entry(parent.into())
        .or_default()
        .push(resource.virtual_path.clone());
    if resource.resource.is_collection {
        snapshot
            .children
            .entry(resource.virtual_path.clone())
            .or_default();
    }
    snapshot
        .resources
        .insert(resource.virtual_path.clone(), resource);
}

fn unique_name(snapshot: &Snapshot, parent: &str, desired: &str, id: &str) -> String {
    let path = format!("{parent}/{desired}");
    if !snapshot.resources.contains_key(&path) {
        return desired.into();
    }
    let digest = Sha256::digest(id.as_bytes());
    format!("{desired}~{}", &format!("{digest:x}")[..8])
}

fn clean_name(name: &str) -> String {
    let cleaned = name.trim_matches('/').replace('/', "∕");
    if cleaned.is_empty() {
        "_".into()
    } else {
        cleaned
    }
}

fn parent_path(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(index) => &path[..index],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, files: &[(&str, u64)]) -> SourceItem {
        SourceItem {
            id: name.into(),
            root: Resource {
                source_path: format!("/{name}"),
                name: name.into(),
                is_collection: true,
                size: 0,
                modified: None,
                etag: None,
                content_type: None,
            },
            descendants: files
                .iter()
                .map(|(file, size)| Resource {
                    source_path: format!("/{name}/{file}"),
                    name: (*file).into(),
                    is_collection: false,
                    size: *size,
                    modified: None,
                    etag: None,
                    content_type: None,
                })
                .collect(),
        }
    }

    #[test]
    fn group_priority_consumes_and_biggest_file_wins() {
        let yaml = r#"
anime:
  group: media
  group_order: 10
  only_show_the_biggest_file: true
  filters:
    - any_file_inside_regex: /\b[a-fA-F0-9]{8}\b/
movies:
  group: media
  group_order: 30
  filters:
    - regex: /.*/
"#;
        let configs: BTreeMap<String, DirectoryConfig> = serde_yaml::from_str(yaml).unwrap();
        let classifier = Classifier::compile(&configs).unwrap();
        let snapshot = classifier.build_snapshot(
            &[item(
                "Release",
                &[("show ABCDEF12.mkv", 10), ("sample.txt", 1)],
            )],
            1,
        );
        assert!(snapshot.get("/anime/Release/show ABCDEF12.mkv").is_some());
        assert!(snapshot.get("/movies/Release").is_none());
    }

    #[test]
    fn zurg_regex_flags_work() {
        let re = compile_regex("/hello world/ix").unwrap();
        assert!(re.is_match("HELLOWORLD"));
    }

    #[test]
    fn empty_categories_and_top_level_episodes_are_supported() {
        let yaml = r#"
shows:
  group: media
  filters:
    - has_episodes: true
"#;
        let configs: BTreeMap<String, DirectoryConfig> = serde_yaml::from_str(yaml).unwrap();
        let classifier = Classifier::compile(&configs).unwrap();
        assert!(classifier.build_snapshot(&[], 1).get("/shows").is_some());

        let episode = SourceItem {
            id: "/Series.S01E02.mkv".into(),
            root: Resource {
                source_path: "/Series.S01E02.mkv".into(),
                name: "Series.S01E02.mkv".into(),
                is_collection: false,
                size: 42,
                modified: None,
                etag: None,
                content_type: Some("video/x-matroska".into()),
            },
            descendants: Vec::new(),
        };
        assert!(
            classifier
                .build_snapshot(&[episode], 2)
                .get("/shows/Series.S01E02.mkv")
                .is_some()
        );
    }
}

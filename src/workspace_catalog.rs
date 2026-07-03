use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;

use crate::codex::WorkspacePolicy;

pub const DEFAULT_WORKSPACE_CATALOG_FILENAME: &str = "workspaces.json";

#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct WorkspaceCatalog {
    entries: Vec<WorkspaceCatalogEntry>,
}

impl WorkspaceCatalog {
    pub fn load_optional(path: Option<&Path>) -> Result<Self, WorkspaceCatalogError> {
        match path {
            Some(path) => Self::from_path(path),
            None => Ok(Self::default()),
        }
    }

    pub fn from_path(path: &Path) -> Result<Self, WorkspaceCatalogError> {
        let content = fs::read_to_string(path)?;
        Self::from_json(&content)
    }

    pub fn from_json(content: &str) -> Result<Self, WorkspaceCatalogError> {
        let file = serde_json::from_str::<WorkspaceCatalogFile>(content)?;
        Self::from_entries(
            file.workspaces
                .into_iter()
                .map(WorkspaceCatalogEntry::from)
                .collect(),
        )
    }

    pub fn from_entries(
        entries: Vec<WorkspaceCatalogEntry>,
    ) -> Result<Self, WorkspaceCatalogError> {
        let mut aliases = HashSet::new();
        let mut default_count = 0;

        for entry in &entries {
            if !is_valid_alias(&entry.alias) {
                return Err(WorkspaceCatalogError::InvalidAlias {
                    alias: entry.alias.clone(),
                });
            }
            if !aliases.insert(entry.alias.clone()) {
                return Err(WorkspaceCatalogError::DuplicateAlias {
                    alias: entry.alias.clone(),
                });
            }
            if entry.is_default {
                default_count += 1;
            }
        }

        if default_count > 1 {
            return Err(WorkspaceCatalogError::MultipleDefaults);
        }

        Ok(Self { entries })
    }

    pub fn entries(&self) -> &[WorkspaceCatalogEntry] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn find(&self, alias: &str) -> Option<&WorkspaceCatalogEntry> {
        self.entries.iter().find(|entry| entry.alias == alias)
    }

    pub fn validate_paths(&self, policy: &WorkspacePolicy) -> Result<(), WorkspaceCatalogError> {
        for entry in &self.entries {
            policy.validate(Some(&entry.path)).map_err(|_| {
                WorkspaceCatalogError::WorkspaceNotAllowed {
                    alias: entry.alias.clone(),
                }
            })?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct WorkspaceCatalogEntry {
    pub alias: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub path: PathBuf,
    pub is_default: bool,
}

impl WorkspaceCatalogEntry {
    pub fn new(alias: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            alias: alias.into(),
            display_name: None,
            description: None,
            path: path.into(),
            is_default: false,
        }
    }
}

impl From<WorkspaceCatalogFileEntry> for WorkspaceCatalogEntry {
    fn from(entry: WorkspaceCatalogFileEntry) -> Self {
        Self {
            alias: entry.alias.trim().to_owned(),
            display_name: entry.display_name.and_then(non_empty_trimmed),
            description: entry.description.and_then(non_empty_trimmed),
            path: entry.path,
            is_default: entry.is_default,
        }
    }
}

fn non_empty_trimmed(value: String) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

pub fn is_valid_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

#[derive(Debug, Deserialize)]
struct WorkspaceCatalogFile {
    #[serde(default)]
    workspaces: Vec<WorkspaceCatalogFileEntry>,
}

#[derive(Debug, Deserialize)]
struct WorkspaceCatalogFileEntry {
    alias: String,
    #[serde(default, alias = "name")]
    display_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    path: PathBuf,
    #[serde(default, rename = "default")]
    is_default: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceCatalogError {
    #[error("workspace catalog file could not be read: {0}")]
    Io(#[from] std::io::Error),
    #[error("workspace catalog JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("workspace catalog alias is invalid: {alias:?}")]
    InvalidAlias { alias: String },
    #[error("workspace catalog alias {alias:?} is duplicated")]
    DuplicateAlias { alias: String },
    #[error("workspace catalog has more than one default workspace alias")]
    MultipleDefaults,
    #[error("workspace catalog alias {alias:?} is outside CODEX_ALLOWED_WORKSPACES")]
    WorkspaceNotAllowed { alias: String },
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use crate::codex::WorkspacePolicy;

    use super::*;

    fn unique_temp_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!("slack-codex-catalog-{name}-{stamp}"))
    }

    #[test]
    fn parses_workspace_catalog_json() {
        let catalog = WorkspaceCatalog::from_json(
            r#"{
                "workspaces": [
                    {
                        "alias": "slack",
                        "display_name": "Slack Codex",
                        "description": "Local bridge",
                        "path": "C:/workspace/slack-codex",
                        "default": true
                    }
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(
            catalog.entries(),
            &[WorkspaceCatalogEntry {
                alias: "slack".to_owned(),
                display_name: Some("Slack Codex".to_owned()),
                description: Some("Local bridge".to_owned()),
                path: PathBuf::from("C:/workspace/slack-codex"),
                is_default: true,
            }]
        );
    }

    #[test]
    fn rejects_invalid_and_duplicate_aliases() {
        assert!(matches!(
            WorkspaceCatalog::from_entries(vec![WorkspaceCatalogEntry::new("bad alias", ".")]),
            Err(WorkspaceCatalogError::InvalidAlias { .. })
        ));
        assert!(matches!(
            WorkspaceCatalog::from_entries(vec![
                WorkspaceCatalogEntry::new("repo", "."),
                WorkspaceCatalogEntry::new("repo", ".")
            ]),
            Err(WorkspaceCatalogError::DuplicateAlias { .. })
        ));
    }

    #[test]
    fn rejects_multiple_defaults() {
        let mut first = WorkspaceCatalogEntry::new("a", ".");
        let mut second = WorkspaceCatalogEntry::new("b", ".");
        first.is_default = true;
        second.is_default = true;

        assert!(matches!(
            WorkspaceCatalog::from_entries(vec![first, second]),
            Err(WorkspaceCatalogError::MultipleDefaults)
        ));
    }

    #[test]
    fn validates_catalog_paths_against_workspace_policy() {
        let allowed = unique_temp_dir("allowed");
        let outside = unique_temp_dir("outside");
        fs::create_dir_all(&allowed).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let catalog =
            WorkspaceCatalog::from_entries(vec![WorkspaceCatalogEntry::new("outside", outside)])
                .unwrap();
        let policy = WorkspacePolicy::new(vec![allowed], None);

        assert!(matches!(
            catalog.validate_paths(&policy),
            Err(WorkspaceCatalogError::WorkspaceNotAllowed { alias }) if alias == "outside"
        ));
    }
}

use std::collections::HashMap;
use std::sync::Arc;

use codex_exec_server::ExecutorFileSystem;
use codex_protocol::protocol::SkillScope;
use codex_skills::ParsedSkillFrontmatter;
use codex_skills::SkillMetadata;
use codex_skills::parse_skill_frontmatter_metadata;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use futures::StreamExt;
use tracing::error;

use super::SKILLS_METADATA_DIR;
use super::SKILLS_METADATA_FILENAME;
use super::discovery::DirectorySymlinkPolicy;
use super::discovery::DiscoveredSkill;
use super::discovery::HiddenDirectoryPolicy;
use super::discovery::MAX_CONCURRENT_SKILL_LOADS;
use super::discovery::SkillDiscovery;
use super::discovery::SkillDiscoveryOptions;
use super::discovery::SkillMetadataDiscovery;
use super::discovery::discover_skills;
use super::metadata::LoadedSkillMetadata;
use super::metadata::load_host_skill_metadata;
use super::metadata::sanitize_single_line;

/// A resolved host skill root ready for filesystem discovery.
pub struct HostSkillRoot {
    pub path: AbsolutePathBuf,
    pub scope: SkillScope,
    pub file_system: Arc<dyn ExecutorFileSystem>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostSkillError {
    pub path: AbsolutePathBuf,
    pub message: String,
}

/// Skills and errors loaded from one canonical host root.
#[derive(Clone)]
pub struct HostSkillRootSnapshot {
    pub root: AbsolutePathBuf,
    pub skills: Vec<SkillMetadata>,
    pub skill_discovery_path_by_path: Arc<HashMap<AbsolutePathBuf, AbsolutePathBuf>>,
    pub errors: Vec<HostSkillError>,
    pub file_system: Arc<dyn ExecutorFileSystem>,
}

struct ResolvedDiscoveredSkill {
    skill: DiscoveredSkill,
    path: AbsolutePathBuf,
    path_uri: PathUri,
}

pub async fn load_host_skill_root(root: HostSkillRoot) -> HostSkillRootSnapshot {
    let canonical_root =
        canonicalize_for_skill_identity(root.file_system.as_ref(), &root.path).await;
    let (skills, skill_discovery_path_by_path, errors) =
        load_skills_under_root(&root, &canonical_root).await;
    HostSkillRootSnapshot {
        root: canonical_root,
        skills,
        skill_discovery_path_by_path,
        errors,
        file_system: root.file_system,
    }
}

async fn load_skills_under_root(
    skill_root: &HostSkillRoot,
    root: &AbsolutePathBuf,
) -> (
    Vec<SkillMetadata>,
    Arc<HashMap<AbsolutePathBuf, AbsolutePathBuf>>,
    Vec<HostSkillError>,
) {
    let file_system = skill_root.file_system.as_ref();
    let directory_symlinks = match skill_root.scope {
        SkillScope::User | SkillScope::Repo | SkillScope::Admin => DirectorySymlinkPolicy::Follow,
        SkillScope::System => DirectorySymlinkPolicy::Ignore,
    };
    let SkillDiscovery { skills, warnings } = discover_skills(
        file_system,
        &PathUri::from_abs_path(root),
        SkillDiscoveryOptions {
            directory_symlinks,
            hidden_directories: HiddenDirectoryPolicy::Skip,
        },
    )
    .await;
    for warning in warnings {
        error!("{warning}");
    }
    if skills.is_empty() {
        return (Vec::new(), Arc::default(), Vec::new());
    }

    let resolved_skills = futures::stream::iter(skills)
        .map(|skill| async move {
            let path_uri = file_system
                .canonicalize(&skill.path, /*sandbox*/ None)
                .await
                .unwrap_or_else(|_| skill.path.clone());
            let path = match path_uri.to_abs_path() {
                Ok(path) => path,
                Err(error) => {
                    error!("failed to convert discovered skill path {path_uri}: {error}");
                    return None;
                }
            };
            Some(ResolvedDiscoveredSkill {
                skill,
                path,
                path_uri,
            })
        })
        .buffered(MAX_CONCURRENT_SKILL_LOADS)
        .filter_map(futures::future::ready)
        .collect::<Vec<_>>()
        .await;
    let skill_results = futures::stream::iter(resolved_skills)
        .map(|skill| async move {
            let discovery_path = skill
                .skill
                .path
                .to_abs_path()
                .unwrap_or_else(|_| skill.path.clone());
            let result = parse_skill_file(
                file_system,
                &skill.skill,
                &skill.path,
                &skill.path_uri,
                skill_root.scope,
            )
            .await;
            (skill.path, discovery_path, result)
        })
        .buffered(MAX_CONCURRENT_SKILL_LOADS)
        .collect::<Vec<_>>()
        .await;
    let mut loaded_skills = Vec::new();
    let mut skill_discovery_path_by_path = HashMap::new();
    let mut errors = Vec::new();
    for (path, discovery_path, result) in skill_results {
        match result {
            Ok(skill) => {
                skill_discovery_path_by_path
                    .insert(skill.path_to_skills_md.clone(), discovery_path);
                loaded_skills.push(skill);
            }
            Err(message) if skill_root.scope != SkillScope::System => {
                errors.push(HostSkillError { path, message });
            }
            Err(_) => {}
        }
    }
    (
        loaded_skills,
        Arc::new(skill_discovery_path_by_path),
        errors,
    )
}

async fn parse_skill_file(
    file_system: &dyn ExecutorFileSystem,
    skill: &DiscoveredSkill,
    path: &AbsolutePathBuf,
    path_uri: &PathUri,
    scope: SkillScope,
) -> Result<SkillMetadata, String> {
    let metadata_path = path_uri
        .parent()
        .and_then(|parent| parent.join(SKILLS_METADATA_DIR).ok())
        .and_then(|directory| directory.join(SKILLS_METADATA_FILENAME).ok());
    let metadata = match &skill.metadata {
        SkillMetadataDiscovery::Present(_) => metadata_path.map(SkillMetadataDiscovery::Present),
        SkillMetadataDiscovery::Probe(_) => metadata_path.map(SkillMetadataDiscovery::Probe),
        SkillMetadataDiscovery::Absent => None,
    }
    .unwrap_or(SkillMetadataDiscovery::Absent);
    let (contents, loaded_metadata) = tokio::join!(
        file_system.read_file_text(path_uri, /*sandbox*/ None),
        load_host_skill_metadata(file_system, path, &metadata),
    );
    let contents = contents.map_err(|error| format!("failed to read file: {error}"))?;
    let ParsedSkillFrontmatter {
        name,
        description,
        short_description,
    } = parse_skill_frontmatter_metadata(&contents, || default_skill_name(path))
        .map_err(|error| error.to_string())?;
    let LoadedSkillMetadata {
        interface,
        dependencies,
        policy,
    } = loaded_metadata;

    Ok(SkillMetadata {
        name,
        description,
        short_description,
        interface,
        dependencies,
        policy,
        path_to_skills_md: path.clone(),
        scope,
    })
}

fn default_skill_name(path: &AbsolutePathBuf) -> String {
    path.parent()
        .and_then(|parent| {
            parent
                .file_name()
                .and_then(|name| name.to_str())
                .map(sanitize_single_line)
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "skill".to_string())
}

async fn canonicalize_for_skill_identity(
    file_system: &dyn ExecutorFileSystem,
    path: &AbsolutePathBuf,
) -> AbsolutePathBuf {
    let path_uri = PathUri::from_abs_path(path);
    file_system
        .canonicalize(&path_uri, /*sandbox*/ None)
        .await
        .and_then(|path| path.to_abs_path())
        .unwrap_or_else(|_| path.clone())
}

#[cfg(test)]
#[path = "host_tests.rs"]
mod tests;

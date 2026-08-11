use std::io;

use codex_exec_server_protocol::CapabilityRootDiscoverRequest;
use codex_exec_server_protocol::CapabilityRootDiscovery;
use codex_exec_server_protocol::CapabilityRootsDiscoverParams;
use codex_exec_server_protocol::CapabilityRootsDiscoverResponse;
use codex_exec_server_protocol::CapabilityTextFile;
use codex_exec_server_protocol::DiscoveredSkillFiles;
use codex_file_system::ExecutorFileSystem;
use codex_file_system::FileSystemSandboxContext;
use codex_file_system::WalkEntryKind;
use codex_file_system::WalkOptions;
use codex_utils_path_uri::PathUri;
use futures::StreamExt;

pub(crate) const MAX_ROOTS_PER_REQUEST: usize = 128;
const MAX_SCAN_DEPTH: usize = 6;
const MAX_DIRECTORIES_PER_ROOT: usize = 2_000;
const MAX_ENTRIES_PER_ROOT: usize = 20_000;
const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_BUNDLE_BYTES_PER_ROOT: usize = 16 * 1024 * 1024;
const MAX_CONCURRENT_ROOTS: usize = 8;
const SKILL_FILE_NAME: &str = "SKILL.md";
const SKILL_METADATA_PATH: &str = "agents/openai.yaml";

#[derive(Debug, thiserror::Error)]
pub enum CapabilityDiscoveryError {
    #[error("capability root discovery accepts at most {MAX_ROOTS_PER_REQUEST} roots")]
    TooManyRoots,
}

/// Discovers and materializes capability manifests using one executor-local filesystem.
///
/// Product parsing and policy intentionally remain with the caller. This operation owns the
/// filesystem-expensive portion: bounded traversal, recognized-file selection, and reads.
#[tracing::instrument(
    name = "capability_roots.discover_v1",
    skip_all,
    fields(root_count = params.roots.len())
)]
pub async fn discover_capability_roots(
    file_system: &dyn ExecutorFileSystem,
    params: CapabilityRootsDiscoverParams,
) -> Result<CapabilityRootsDiscoverResponse, CapabilityDiscoveryError> {
    if params.roots.len() > MAX_ROOTS_PER_REQUEST {
        return Err(CapabilityDiscoveryError::TooManyRoots);
    }

    let roots = futures::stream::iter(params.roots)
        .map(|root| discover_root(file_system, root))
        .buffered(MAX_CONCURRENT_ROOTS)
        .collect()
        .await;
    Ok(CapabilityRootsDiscoverResponse { roots })
}

async fn discover_root(
    file_system: &dyn ExecutorFileSystem,
    request: CapabilityRootDiscoverRequest,
) -> CapabilityRootDiscovery {
    let CapabilityRootDiscoverRequest { id, path, sandbox } = request;
    let sandbox = sandbox.as_ref();
    let mut discovery = CapabilityRootDiscovery {
        id,
        path: path.clone(),
        skills: Vec::new(),
        warnings: Vec::new(),
        error: None,
    };

    match file_system.get_metadata(&path, sandbox).await {
        Ok(metadata) if metadata.is_directory => {}
        Ok(_) => {
            discovery.error = Some(format!("capability root {path} is not a directory"));
            return discovery;
        }
        Err(error) => {
            discovery.error = Some(format!("failed to inspect capability root {path}: {error}"));
            return discovery;
        }
    }

    let walk = match file_system
        .walk(
            &path,
            WalkOptions {
                max_depth: MAX_SCAN_DEPTH,
                max_directories: MAX_DIRECTORIES_PER_ROOT,
                max_entries: MAX_ENTRIES_PER_ROOT,
                follow_directory_symlinks: true,
                prune_hidden_directories: false,
            },
            sandbox,
        )
        .await
    {
        Ok(walk) => walk,
        Err(error) => {
            discovery.error = Some(format!("failed to scan capability root {path}: {error}"));
            return discovery;
        }
    };
    discovery
        .warnings
        .extend(walk.errors.into_iter().map(|error| {
            format!(
                "failed to scan capability path {}: {}",
                error.path, error.message
            )
        }));
    if walk.truncated {
        discovery.warnings.push(format!(
            "capability scan reached its traversal limit (root: {path})"
        ));
    }

    let mut skill_paths = Vec::new();
    for entry in walk.entries {
        if entry.kind != WalkEntryKind::File {
            continue;
        }
        if entry.path.basename().as_deref() == Some(SKILL_FILE_NAME) {
            skill_paths.push(entry.path.clone());
        }
    }
    skill_paths.sort_unstable_by_key(PathUri::to_string);

    let mut budget = BundleBudget::default();

    for skill_path in skill_paths {
        let Some(instructions) = read_optional_text_file(
            file_system,
            skill_path.clone(),
            sandbox,
            &mut budget,
            &mut discovery.warnings,
        )
        .await
        else {
            continue;
        };
        let metadata = match skill_path
            .parent()
            .and_then(|skill_dir| skill_dir.join(SKILL_METADATA_PATH).ok())
        {
            Some(metadata_path) => {
                read_optional_text_file(
                    file_system,
                    metadata_path,
                    sandbox,
                    &mut budget,
                    &mut discovery.warnings,
                )
                .await
            }
            None => None,
        };
        discovery.skills.push(DiscoveredSkillFiles {
            instructions,
            metadata,
        });
    }

    discovery
}

async fn read_optional_text_file(
    file_system: &dyn ExecutorFileSystem,
    path: PathUri,
    sandbox: Option<&FileSystemSandboxContext>,
    budget: &mut BundleBudget,
    warnings: &mut Vec<String>,
) -> Option<CapabilityTextFile> {
    let metadata = match file_system.get_metadata(&path, sandbox).await {
        Ok(metadata) if metadata.is_file => metadata,
        Ok(_) => return None,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => {
            warnings.push(format!("failed to inspect capability file {path}: {error}"));
            return None;
        }
    };
    let Ok(size) = usize::try_from(metadata.size) else {
        warnings.push(format!("capability file {path} is too large"));
        return None;
    };
    if size > MAX_FILE_BYTES {
        warnings.push(format!(
            "capability file {path} exceeds the {MAX_FILE_BYTES}-byte limit"
        ));
        return None;
    }
    if !budget.can_add(size) {
        warnings.push(format!(
            "capability root bundle exceeds the {MAX_BUNDLE_BYTES_PER_ROOT}-byte limit"
        ));
        return None;
    }
    let contents = if sandbox.is_some_and(FileSystemSandboxContext::should_run_in_sandbox) {
        match file_system.read_file(&path, sandbox).await {
            Ok(contents) if contents.len() <= MAX_FILE_BYTES && budget.can_add(contents.len()) => {
                contents
            }
            Ok(_) => {
                warnings.push(format!("capability file {path} exceeded its read limit"));
                return None;
            }
            Err(error) => {
                warnings.push(format!("failed to read capability file {path}: {error}"));
                return None;
            }
        }
    } else {
        let mut stream = match file_system.read_file_stream(&path, sandbox).await {
            Ok(stream) => stream,
            Err(error) => {
                warnings.push(format!("failed to read capability file {path}: {error}"));
                return None;
            }
        };
        let mut contents = Vec::with_capacity(size);
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    warnings.push(format!("failed to read capability file {path}: {error}"));
                    return None;
                }
            };
            let Some(new_len) = contents.len().checked_add(chunk.len()) else {
                warnings.push(format!("capability file {path} exceeded its read limit"));
                return None;
            };
            if new_len > MAX_FILE_BYTES || !budget.can_add(new_len) {
                warnings.push(format!("capability file {path} exceeded its read limit"));
                return None;
            }
            contents.extend_from_slice(&chunk);
        }
        contents
    };
    let contents = match String::from_utf8(contents) {
        Ok(contents) => contents,
        Err(error) => {
            warnings.push(format!("capability file {path} is not UTF-8: {error}"));
            return None;
        }
    };
    budget.add(contents.len());
    Some(CapabilityTextFile { path, contents })
}

#[derive(Default)]
struct BundleBudget {
    bytes: usize,
}

impl BundleBudget {
    fn can_add(&self, bytes: usize) -> bool {
        self.bytes
            .checked_add(bytes)
            .is_some_and(|total| total <= MAX_BUNDLE_BYTES_PER_ROOT)
    }

    fn add(&mut self, bytes: usize) {
        self.bytes += bytes;
    }
}

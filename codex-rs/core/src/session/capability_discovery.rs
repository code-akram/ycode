use super::*;
use codex_exec_server::ExecutorCapabilityDiscoveryCache;
use codex_exec_server::ExecutorCapabilityDiscoverySnapshot;
use codex_exec_server::FileSystemSandboxContext;
use codex_exec_server::MAX_SELECTED_CAPABILITY_ROOTS;
use codex_exec_server::ResolvedSelectedCapabilityRoot;
use codex_protocol::capabilities::CapabilityRootLocation;
use codex_protocol::capabilities::SelectedCapabilityRoot;

impl Session {
    #[tracing::instrument(
        name = "capability_roots.snapshot_for_step",
        skip_all,
        fields(root_count = ready_selected_capability_roots.len())
    )]
    pub(crate) async fn executor_capability_discovery_for_step(
        &self,
        config: &Config,
        ready_selected_capability_roots: &[SelectedCapabilityRoot],
        environments: &TurnEnvironmentSnapshot,
        windows_sandbox_level: WindowsSandboxLevel,
    ) -> Option<Arc<ExecutorCapabilityDiscoverySnapshot>> {
        let restricted_file_system = environments.primary().map_or_else(
            || {
                !config
                    .permissions
                    .file_system_sandbox_policy()
                    .has_full_disk_read_access()
            },
            |_| {
                environments.turn_environments().any(|environment| {
                    !environment
                        .permission_profile()
                        .file_system_sandbox_policy()
                        .has_full_disk_read_access()
                })
            },
        );
        if !restricted_file_system
            && !config
                .features
                .enabled(Feature::ExecutorCapabilityDiscovery)
        {
            return None;
        }
        let sandbox_contexts = if restricted_file_system {
            environments
                .turn_environments()
                .map(|environment| {
                    let mut sandbox = FileSystemSandboxContext::from_permission_profile_with_cwd(
                        environment.permission_profile().clone(),
                        environment.cwd().clone(),
                    );
                    sandbox.workspace_roots = environment.workspace_roots().to_vec();
                    sandbox.windows_sandbox_level = windows_sandbox_level;
                    sandbox.windows_sandbox_private_desktop =
                        config.permissions.windows_sandbox_private_desktop;
                    sandbox.use_legacy_landlock = config.features.use_legacy_landlock();
                    (environment.environment_id.clone(), sandbox)
                })
                .collect::<HashMap<_, _>>()
        } else {
            HashMap::new()
        };
        let environment_manager = self.services.turn_environments.environment_manager();
        let cache = self
            .services
            .thread_extension_data
            .get_or_init(|| ExecutorCapabilityDiscoveryCache::new(environment_manager));
        let selected_capability_roots = ready_selected_capability_roots
            .iter()
            .filter(|selected_root| {
                if !restricted_file_system {
                    return true;
                }
                let CapabilityRootLocation::Environment { environment_id, .. } =
                    &selected_root.location;
                if sandbox_contexts.contains_key(environment_id) {
                    return true;
                }
                warn!(
                    selected_root = selected_root.id,
                    environment_id, "skipping capability root without a filesystem sandbox context"
                );
                false
            })
            .cloned()
            .collect::<Vec<_>>();
        Some(Arc::new(
            cache
                .snapshot(&selected_capability_roots, &sandbox_contexts)
                .await,
        ))
    }

    pub(crate) async fn resolve_selected_capability_roots_for_step(
        &self,
        environments: &TurnEnvironmentSnapshot,
    ) -> Vec<ResolvedSelectedCapabilityRoot> {
        let thread_root_count = self.services.selected_capability_roots.len();
        let mut root_locations_by_id = HashMap::new();
        let mut selected_capability_roots = Vec::new();
        let mut ready_environment_root_count = 0;
        for (index, root) in self
            .services
            .selected_capability_roots
            .iter()
            .cloned()
            .chain(
                environments
                    .turn_environments()
                    .flat_map(|environment| environment.environment.selected_capability_roots()),
            )
            .enumerate()
        {
            if let Some(kept_location) = root_locations_by_id.get(&root.id) {
                if kept_location != &root.location {
                    tracing::warn!(
                        root_id = root.id,
                        ?kept_location,
                        ignored_location = ?root.location,
                        "ignoring selected capability root with conflicting location"
                    );
                }
                continue;
            }
            if index >= thread_root_count {
                if ready_environment_root_count == MAX_SELECTED_CAPABILITY_ROOTS {
                    tracing::warn!(
                        max_root_count = MAX_SELECTED_CAPABILITY_ROOTS,
                        "ignoring excess selected capability roots from ready environments"
                    );
                    break;
                }
                ready_environment_root_count += 1;
            }
            root_locations_by_id.insert(root.id.clone(), root.location.clone());
            selected_capability_roots.push(root);
        }
        self.services
            .turn_environments
            .environment_manager()
            .resolve_selected_capability_roots(
                &selected_capability_roots,
                &environments.captured_environments(),
            )
            .await
    }

    pub(crate) fn ready_selected_capability_roots(
        selected_capability_roots: &[ResolvedSelectedCapabilityRoot],
    ) -> Vec<SelectedCapabilityRoot> {
        selected_capability_roots
            .iter()
            .map(|root| root.selected_root().clone())
            .collect()
    }
}

use crate::policy_transforms::effective_permission_profile;
use crate::policy_transforms::should_require_platform_sandbox;
use codex_network_proxy::ManagedNetworkSandboxContext;
use codex_network_proxy::NetworkProxy;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::SandboxPolicy;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxType {
    None,
    MacosSeatbelt,
}

impl SandboxType {
    pub fn as_metric_tag(self) -> &'static str {
        match self {
            SandboxType::None => "none",
            SandboxType::MacosSeatbelt => "seatbelt",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxablePreference {
    Auto,
    Require,
    Forbid,
}

pub fn get_platform_sandbox(_windows_sandbox_enabled: bool) -> Option<SandboxType> {
    Some(SandboxType::MacosSeatbelt)
}

pub fn with_managed_mitm_ca_readable_root(
    permission_profile: PermissionProfile,
    managed_mitm_ca_trust_bundle_path: Option<&AbsolutePathBuf>,
    sandbox_policy_cwd: &Path,
) -> PermissionProfile {
    let Some(managed_mitm_ca_trust_bundle_path) = managed_mitm_ca_trust_bundle_path else {
        return permission_profile;
    };
    let (file_system_sandbox_policy, network_sandbox_policy) =
        permission_profile.to_runtime_permissions();
    let file_system_sandbox_policy = file_system_sandbox_policy.with_additional_readable_roots(
        sandbox_policy_cwd,
        std::slice::from_ref(managed_mitm_ca_trust_bundle_path),
    );
    PermissionProfile::from_runtime_permissions_with_enforcement(
        permission_profile.enforcement(),
        &file_system_sandbox_policy,
        network_sandbox_policy,
    )
}

#[derive(Debug)]
pub struct SandboxCommand {
    pub program: OsString,
    pub args: Vec<String>,
    pub cwd: PathUri,
    pub env: HashMap<String, String>,
    pub managed_network: Option<ManagedNetworkSandboxContext>,
    pub additional_permissions: Option<AdditionalPermissionProfile>,
}

/// A host-native launch request produced after [`SandboxManager::transform`] validates URI inputs.
/// Build this only at the execution boundary: in exec-server, or in its logical equivalent within
/// app-server. Orchestration and transport code should retain [`PathUri`] values and defer
/// conversion to native paths until this request is created.
#[derive(Debug)]
pub struct SandboxExecRequest {
    pub command: Vec<String>,
    pub cwd: PathUri,
    pub sandbox_policy_cwd: PathUri,
    pub env: HashMap<String, String>,
    pub network: Option<NetworkProxy>,
    pub network_environment_id: Option<String>,
    pub sandbox: SandboxType,
    pub windows_sandbox_level: WindowsSandboxLevel,
    pub windows_sandbox_private_desktop: bool,
    pub permission_profile: PermissionProfile,
    pub arg0: Option<String>,
}

/// Bundled arguments for sandbox transformation.
///
/// This keeps call sites self-documenting when several fields are optional.
pub struct SandboxTransformRequest<'a> {
    pub command: SandboxCommand,
    pub permissions: &'a PermissionProfile,
    pub sandbox: SandboxType,
    pub enforce_managed_network: bool,
    pub environment_id: Option<&'a str>,
    // TODO(viyatb): Evaluate switching this to Option<Arc<NetworkProxy>>
    // to make shared ownership explicit across runtime/sandbox plumbing.
    pub network: Option<&'a NetworkProxy>,
    pub sandbox_policy_cwd: &'a PathUri,
    pub windows_sandbox_level: WindowsSandboxLevel,
    pub windows_sandbox_private_desktop: bool,
}

/// Bundled arguments for a sandbox transformation whose result will be spawned
/// directly from argv.
///
/// Direct-spawn callers will not run a later platform-specific launcher, so the
/// returned command must encode any sandbox wrapper it needs.
pub struct SandboxDirectSpawnTransformRequest<'a> {
    pub transform: SandboxTransformRequest<'a>,
    pub workspace_roots: &'a [AbsolutePathBuf],
}

// TODO(anp): Revisit this preparation type once this module's PathUri migration is complete.
struct PendingSandboxedExecRequest {
    native_sandbox_policy_cwd: AbsolutePathBuf,
    effective_permission_profile: PermissionProfile,
}

impl PendingSandboxedExecRequest {
    fn new(
        sandbox_policy_cwd: &PathUri,
        effective_permission_profile: PermissionProfile,
        managed_mitm_ca_trust_bundle_path: Option<&AbsolutePathBuf>,
    ) -> Result<Self, SandboxTransformError> {
        let native_sandbox_policy_cwd = sandbox_policy_cwd.to_abs_path().map_err(|source| {
            SandboxTransformError::InvalidSandboxPolicyCwd {
                cwd: sandbox_policy_cwd.clone(),
                source,
            }
        })?;
        let effective_permission_profile = with_managed_mitm_ca_readable_root(
            effective_permission_profile,
            managed_mitm_ca_trust_bundle_path,
            native_sandbox_policy_cwd.as_path(),
        );
        Ok(Self {
            native_sandbox_policy_cwd,
            effective_permission_profile,
        })
    }
}

#[derive(Debug)]
pub enum SandboxTransformError {
    InvalidSandboxPolicyCwd { cwd: PathUri, source: io::Error },
    EnvironmentNetworkProxy(String),
}

impl std::fmt::Display for SandboxTransformError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSandboxPolicyCwd { cwd, source } => write!(
                f,
                "sandbox policy cwd URI `{cwd}` is not valid on this host: {source}"
            ),
            Self::EnvironmentNetworkProxy(err) => {
                write!(f, "failed to prepare environment network proxy: {err}")
            }
        }
    }
}

impl std::error::Error for SandboxTransformError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidSandboxPolicyCwd { source, .. } => Some(source),
            Self::EnvironmentNetworkProxy(_) => None,
        }
    }
}

#[derive(Default)]
pub struct SandboxManager;

impl SandboxManager {
    pub fn new() -> Self {
        Self
    }

    pub fn select_initial(
        &self,
        permission_profile: &PermissionProfile,
        pref: SandboxablePreference,
        windows_sandbox_level: WindowsSandboxLevel,
        has_managed_network_requirements: bool,
    ) -> SandboxType {
        if self.should_sandbox(permission_profile, pref, has_managed_network_requirements) {
            get_platform_sandbox(windows_sandbox_level != WindowsSandboxLevel::Disabled)
                .unwrap_or(SandboxType::None)
        } else {
            SandboxType::None
        }
    }

    /// Returns whether the request needs a sandbox, independently of whether
    /// this host can provide a concrete sandbox implementation.
    pub fn should_sandbox(
        &self,
        permission_profile: &PermissionProfile,
        pref: SandboxablePreference,
        has_managed_network_requirements: bool,
    ) -> bool {
        match pref {
            SandboxablePreference::Forbid => false,
            SandboxablePreference::Require => true,
            SandboxablePreference::Auto => {
                let (file_system_policy, network_policy) =
                    permission_profile.to_runtime_permissions();
                should_require_platform_sandbox(
                    &file_system_policy,
                    network_policy,
                    has_managed_network_requirements,
                )
            }
        }
    }

    pub fn transform(
        &self,
        request: SandboxTransformRequest<'_>,
    ) -> Result<SandboxExecRequest, SandboxTransformError> {
        let SandboxTransformRequest {
            mut command,
            permissions,
            sandbox,
            enforce_managed_network,
            environment_id,
            network,
            sandbox_policy_cwd,
            windows_sandbox_level,
            windows_sandbox_private_desktop,
        } = request;
        let managed_network = command.managed_network.as_ref();
        let additional_permissions = command.additional_permissions.take();
        let managed_mitm_ca_trust_bundle_path =
            network.and_then(NetworkProxy::managed_mitm_ca_trust_bundle_path);
        let base_effective_permission_profile =
            effective_permission_profile(permissions, additional_permissions.as_ref());
        let pending_sandboxed_request = PendingSandboxedExecRequest::new(
            sandbox_policy_cwd,
            base_effective_permission_profile.clone(),
            managed_mitm_ca_trust_bundle_path.as_ref(),
        );
        let mut argv = Vec::with_capacity(1 + command.args.len());
        argv.push(command.program);
        argv.extend(command.args.into_iter().map(OsString::from));

        let (argv, arg0_override, pending_sandboxed_request) = match sandbox {
            SandboxType::None => (os_argv_to_strings(argv), None, None),
            SandboxType::MacosSeatbelt => {
                use crate::seatbelt::CreateSeatbeltCommandArgsParams;
                use crate::seatbelt::MACOS_PATH_TO_SEATBELT_EXECUTABLE;
                use crate::seatbelt::create_seatbelt_command_args;

                let pending = pending_sandboxed_request?;
                let (file_system_sandbox_policy, network_sandbox_policy) = pending
                    .effective_permission_profile
                    .to_runtime_permissions();
                let mut args = create_seatbelt_command_args(CreateSeatbeltCommandArgsParams {
                    command: os_argv_to_strings(argv),
                    file_system_sandbox_policy: &file_system_sandbox_policy,
                    network_sandbox_policy,
                    sandbox_policy_cwd: pending.native_sandbox_policy_cwd.as_path(),
                    enforce_managed_network,
                    managed_network,
                    environment_id,
                    network,
                    extra_allow_unix_sockets: &[],
                })
                .map_err(SandboxTransformError::EnvironmentNetworkProxy)?;
                let mut full_command = Vec::with_capacity(1 + args.len());
                full_command.push(MACOS_PATH_TO_SEATBELT_EXECUTABLE.to_string());
                full_command.append(&mut args);
                (full_command, None, Some(pending))
            }
        };

        // Unsandboxed exec-server requests may have foreign cwd values that cannot be prepared
        // locally, but their effective permissions must still be preserved. In that case, carry
        // forward the base profile.
        let permission_profile = pending_sandboxed_request
            .map_or(base_effective_permission_profile, |pending| {
                pending.effective_permission_profile
            });

        Ok(SandboxExecRequest {
            command: argv,
            cwd: command.cwd,
            sandbox_policy_cwd: sandbox_policy_cwd.clone(),
            env: command.env,
            network: network.cloned(),
            network_environment_id: environment_id.map(str::to_string),
            sandbox,
            windows_sandbox_level,
            windows_sandbox_private_desktop,
            permission_profile,
            arg0: arg0_override,
        })
    }

    pub fn transform_for_direct_spawn(
        &self,
        request: SandboxDirectSpawnTransformRequest<'_>,
    ) -> Result<SandboxExecRequest, SandboxTransformError> {
        self.transform(request.transform)
    }
}

pub fn compatibility_sandbox_policy_for_permission_profile(
    permissions: &PermissionProfile,
    cwd: &Path,
) -> SandboxPolicy {
    permissions
        .to_legacy_sandbox_policy(cwd)
        .unwrap_or_else(|_| {
            let (file_system_policy, network_policy) = permissions.to_runtime_permissions();
            compatibility_workspace_write_policy(file_system_policy, network_policy, cwd)
        })
}

fn compatibility_workspace_write_policy(
    file_system_policy: FileSystemSandboxPolicy,
    network_policy: NetworkSandboxPolicy,
    cwd: &Path,
) -> SandboxPolicy {
    let cwd_abs = AbsolutePathBuf::from_absolute_path(cwd).ok();
    let writable_roots = file_system_policy
        .get_writable_roots_with_cwd(cwd)
        .into_iter()
        .map(|root| root.root)
        .filter(|root| cwd_abs.as_ref() != Some(root))
        .collect();
    let tmpdir_writable = std::env::var_os("TMPDIR")
        .filter(|tmpdir| !tmpdir.is_empty())
        .and_then(|tmpdir| {
            AbsolutePathBuf::from_absolute_path(std::path::PathBuf::from(tmpdir)).ok()
        })
        .is_some_and(|tmpdir| file_system_policy.can_write_path_with_cwd(tmpdir.as_path(), cwd));
    let slash_tmp = Path::new("/tmp");
    let slash_tmp_writable = slash_tmp.is_absolute()
        && slash_tmp.is_dir()
        && file_system_policy.can_write_path_with_cwd(slash_tmp, cwd);

    SandboxPolicy::WorkspaceWrite {
        writable_roots,
        network_access: network_policy.is_enabled(),
        exclude_tmpdir_env_var: !tmpdir_writable,
        exclude_slash_tmp: !slash_tmp_writable,
    }
}

fn os_argv_to_strings(argv: Vec<OsString>) -> Vec<String> {
    argv.into_iter()
        .map(os_string_to_command_component)
        .collect()
}

fn os_string_to_command_component(value: OsString) -> String {
    value
        .into_string()
        .unwrap_or_else(|value| value.to_string_lossy().into_owned())
}

#[cfg(test)]
#[path = "manager_tests.rs"]
mod tests;

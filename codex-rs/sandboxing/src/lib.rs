mod denial;
mod manager;
pub mod policy_transforms;
pub mod seatbelt;
mod spawn;
mod violation;

pub use denial::is_likely_sandbox_denied;
pub use manager::SandboxCommand;
pub use manager::SandboxDirectSpawnTransformRequest;
pub use manager::SandboxExecRequest;
pub use manager::SandboxManager;
pub use manager::SandboxTransformError;
pub use manager::SandboxTransformRequest;
pub use manager::SandboxType;
pub use manager::SandboxablePreference;
pub use manager::compatibility_sandbox_policy_for_permission_profile;
pub use manager::get_platform_sandbox;
pub use manager::with_managed_mitm_ca_readable_root;
pub use spawn::SpawnRequest;
pub use spawn::spawn_process;
pub use violation::FileSystemSandboxViolation;
pub use violation::FileSystemSandboxViolationReason;
pub use violation::NetworkSandboxViolation;
pub use violation::SandboxViolationBackend;
pub use violation::SandboxViolationEvent;
pub use violation::record_filesystem_sandbox_violation;
pub use violation::record_network_sandbox_violation;
pub use violation::record_sandbox_violation;

use codex_protocol::error::CodexErr;

impl From<SandboxTransformError> for CodexErr {
    fn from(err: SandboxTransformError) -> Self {
        match err {
            error @ SandboxTransformError::InvalidSandboxPolicyCwd { .. } => {
                CodexErr::InvalidRequest(error.to_string())
            }
            SandboxTransformError::EnvironmentNetworkProxy(message) => {
                CodexErr::UnsupportedOperation(message)
            }
        }
    }
}

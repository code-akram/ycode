mod common;

use codex_exec_server::CAPABILITY_ROOTS_DISCOVER_METHOD;
use codex_exec_server::CapabilityRootDiscovery;
use codex_exec_server::CapabilityRootsDiscoverParams;
use codex_exec_server::CapabilityRootsDiscoverResponse;
use codex_exec_server::FileSystemSandboxContext;
use codex_exec_server::InitializeParams;
use codex_exec_server::InitializeResponse;
use codex_exec_server_protocol::CapabilityRootDiscoverRequest;
use codex_exec_server_protocol::JSONRPCMessage;
use codex_exec_server_protocol::JSONRPCResponse;
#[cfg(unix)]
use codex_protocol::models::PermissionProfile;
#[cfg(unix)]
use codex_protocol::permissions::FileSystemAccessMode;
#[cfg(unix)]
use codex_protocol::permissions::FileSystemPath;
#[cfg(unix)]
use codex_protocol::permissions::FileSystemSandboxEntry;
#[cfg(unix)]
use codex_protocol::permissions::FileSystemSandboxPolicy;
#[cfg(unix)]
use codex_protocol::permissions::NetworkSandboxPolicy;
#[cfg(unix)]
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use common::exec_server::exec_server;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovers_a_complete_capability_bundle_in_one_request() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    write_file(
        &root.path().join("skills/deploy/SKILL.md"),
        "---\nname: deploy\ndescription: Deploy the service.\n---\n\nDeploy instructions.\n",
    )?;
    write_file(
        &root.path().join("skills/deploy/agents/openai.yaml"),
        "policy:\n  allow_implicit_invocation: false\n",
    )?;
    write_file(
        &root.path().join("nested/skills/audit/SKILL.md"),
        "---\nname: audit\ndescription: Audit the service.\n---\n",
    )?;
    write_file(
        &root.path().join("nested-cursor/skills/review/SKILL.md"),
        "---\nname: review\ndescription: Review the service.\n---\n",
    )?;

    let mut server = exec_server().await?;
    initialize(&mut server).await?;
    let root_uri = PathUri::from_host_native_path(root.path())?;
    let discovery = discover_root(&mut server, "demo@1", root_uri.clone()).await?;

    assert_eq!(discovery.id, "demo@1");
    assert_eq!(discovery.path, root_uri);
    assert_eq!(discovery.error, None);
    assert_eq!(discovery.warnings, Vec::<String>::new());
    assert_eq!(
        discovery
            .skills
            .iter()
            .map(|skill| (
                skill.instructions.path.clone(),
                skill
                    .metadata
                    .as_ref()
                    .map(|metadata| metadata.path.clone()),
            ))
            .collect::<Vec<_>>(),
        vec![
            (root_uri.join("nested-cursor/skills/review/SKILL.md")?, None,),
            (root_uri.join("nested/skills/audit/SKILL.md")?, None,),
            (
                root_uri.join("skills/deploy/SKILL.md")?,
                Some(root_uri.join("skills/deploy/agents/openai.yaml")?),
            ),
        ]
    );

    server.shutdown().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sandboxed_discovery_follows_only_permitted_external_symlinks() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let external = tempfile::tempdir()?;
    write_file(
        &root.path().join(".codex-plugin/plugin.json"),
        r#"{"name":"linked-plugin"}"#,
    )?;
    write_file(
        &external.path().join("skill/SKILL.md"),
        "---\nname: linked\ndescription: Linked external skill.\n---\n",
    )?;
    std::fs::create_dir_all(root.path().join("skills"))?;
    std::os::unix::fs::symlink(
        external.path().join("skill"),
        root.path().join("skills/linked"),
    )?;

    let mut server = exec_server().await?;
    initialize(&mut server).await?;
    let root_uri = PathUri::from_host_native_path(root.path())?;
    let root_path = AbsolutePathBuf::from_absolute_path(root.path())?;
    let external_root = AbsolutePathBuf::from_absolute_path(external.path())?;
    let path_entry =
        |path, access| FileSystemSandboxEntry::new(FileSystemPath::Path { path }, access);
    let read_root = path_entry(root_path, FileSystemAccessMode::Read);
    let read_external = path_entry(external_root.clone(), FileSystemAccessMode::Read);
    let deny_external_skill = path_entry(external_root.join("skill"), FileSystemAccessMode::Deny);
    let cases = [
        (
            "permitted symlink",
            vec![read_root.clone(), read_external.clone()],
            true,
        ),
        ("denied external root", vec![read_root.clone()], false),
        (
            "denied external skill",
            vec![read_root, read_external, deny_external_skill],
            false,
        ),
    ];

    for (scenario, entries, has_skill) in cases {
        let policy = FileSystemSandboxPolicy::restricted(entries);
        let sandbox = FileSystemSandboxContext::from_permission_profile_with_cwd(
            PermissionProfile::from_runtime_permissions(&policy, NetworkSandboxPolicy::Restricted),
            root_uri.clone(),
        );
        let discovery =
            discover_root_with_sandbox(&mut server, "linked", root_uri.clone(), Some(sandbox))
                .await?;

        assert_eq!(discovery.error, None, "{scenario}");
        assert_eq!(discovery.skills.len(), usize::from(has_skill), "{scenario}");
    }

    server.shutdown().await?;
    Ok(())
}

async fn discover_root(
    server: &mut common::exec_server::ExecServerHarness,
    id: &str,
    path: PathUri,
) -> anyhow::Result<CapabilityRootDiscovery> {
    discover_root_with_sandbox(server, id, path, /*sandbox*/ None).await
}

async fn discover_root_with_sandbox(
    server: &mut common::exec_server::ExecServerHarness,
    id: &str,
    path: PathUri,
    sandbox: Option<FileSystemSandboxContext>,
) -> anyhow::Result<CapabilityRootDiscovery> {
    let request_id = server
        .send_request(
            CAPABILITY_ROOTS_DISCOVER_METHOD,
            serde_json::to_value(CapabilityRootsDiscoverParams {
                roots: vec![CapabilityRootDiscoverRequest {
                    id: id.to_string(),
                    path,
                    sandbox,
                }],
            })?,
        )
        .await?;
    let response = server.next_event().await?;
    let JSONRPCMessage::Response(JSONRPCResponse { id, result }) = response else {
        anyhow::bail!("expected discovery response, received {response:?}");
    };
    assert_eq!(id, request_id);
    let response: CapabilityRootsDiscoverResponse = serde_json::from_value(result)?;
    let [discovery] = response.roots.as_slice() else {
        anyhow::bail!("expected exactly one discovered root");
    };
    Ok(discovery.clone())
}

async fn initialize(server: &mut common::exec_server::ExecServerHarness) -> anyhow::Result<()> {
    let initialize_id = server
        .send_request(
            "initialize",
            serde_json::to_value(InitializeParams {
                client_name: "capability-discovery-test".to_string(),
                resume_session_id: None,
            })?,
        )
        .await?;
    let response = server
        .wait_for_event(|event| {
            matches!(event, JSONRPCMessage::Response(response) if response.id == initialize_id)
        })
        .await?;
    let JSONRPCMessage::Response(JSONRPCResponse { result, .. }) = response else {
        unreachable!("wait predicate only accepts a response");
    };
    let _: InitializeResponse = serde_json::from_value(result)?;
    server
        .send_notification("initialized", serde_json::json!({}))
        .await?;
    Ok(())
}

fn write_file(path: &std::path::Path, contents: &str) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("test file should have a parent"))?;
    std::fs::create_dir_all(parent)?;
    std::fs::write(path, contents)?;
    Ok(())
}

use assert_cmd::prelude::*;
use codex_apply_patch::CODEX_CORE_APPLY_PATCH_ARG1;
use pretty_assertions::assert_eq;
use std::fs;
use std::process::Command;
use tempfile::tempdir;

#[test]
fn standalone_exec_cli_can_use_apply_patch() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let relative_path = "source.txt";
    let absolute_path = tmp.path().join(relative_path);
    fs::write(&absolute_path, "original content\n")?;

    Command::new(codex_utils_cargo_bin::cargo_bin("codex-exec")?)
        .arg(CODEX_CORE_APPLY_PATCH_ARG1)
        .arg(
            r#"*** Begin Patch
*** Update File: source.txt
@@
-original content
+modified by apply_patch
*** End Patch"#,
        )
        .current_dir(tmp.path())
        .assert()
        .success()
        .stdout("Success. Updated the following files:\nM source.txt\n")
        .stderr(predicates::str::is_empty());
    assert_eq!(
        fs::read_to_string(absolute_path)?,
        "modified by apply_patch\n"
    );
    Ok(())
}

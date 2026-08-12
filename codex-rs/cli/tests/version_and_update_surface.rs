use anyhow::Result;

fn codex_command() -> Result<assert_cmd::Command> {
    Ok(assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin(
        "codex",
    )?))
}

#[test]
fn version_remains_available() -> Result<()> {
    let output = codex_command()?.arg("--version").output()?;

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains(env!("CARGO_PKG_VERSION")));
    Ok(())
}

#[test]
fn help_has_no_update_command() -> Result<()> {
    let output = codex_command()?.arg("--help").output()?;

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout)?;
    assert!(!stdout.lines().any(|line| {
        line.trim_start()
            .strip_prefix("update")
            .is_some_and(|suffix| suffix.starts_with(char::is_whitespace))
    }));
    Ok(())
}

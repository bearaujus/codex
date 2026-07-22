use std::path::Path;

use anyhow::Result;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use tempfile::TempDir;

fn codex_command(codex_home: &Path) -> Result<assert_cmd::Command> {
    let mut cmd = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?);
    cmd.env("CODEX_HOME", codex_home);
    Ok(cmd)
}

#[test]
fn top_level_help_omits_removed_setup_subcommand() -> Result<()> {
    let codex_home = TempDir::new()?;

    let mut cmd = codex_command(codex_home.path())?;
    cmd.args(["--help"])
        .assert()
        .success()
        .stdout(contains(" setup ").not());

    Ok(())
}

#[test]
fn top_level_help_omits_login_subcommand() -> Result<()> {
    let codex_home = TempDir::new()?;

    let mut cmd = codex_command(codex_home.path())?;
    cmd.args(["--help"])
        .assert()
        .success()
        .stdout(contains(" login ").not());

    Ok(())
}

#[test]
fn top_level_help_omits_logout_subcommand() -> Result<()> {
    let codex_home = TempDir::new()?;

    let mut cmd = codex_command(codex_home.path())?;
    cmd.args(["--help"])
        .assert()
        .success()
        .stdout(contains(" logout ").not());

    Ok(())
}

use assert_cmd::Command;
use pretty_assertions::assert_eq;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn apply_patch_command() -> anyhow::Result<Command> {
    Ok(Command::new(codex_utils_cargo_bin::cargo_bin(
        "apply_patch",
    )?))
}

fn run_patch_in_dir(dir: &Path, patch: String) -> anyhow::Result<assert_cmd::assert::Assert> {
    Ok(apply_patch_command()?.arg(patch).current_dir(dir).assert())
}

fn assert_json_patch_updates_file(
    original: &[u8],
    patch_body: &str,
    expected: &[u8],
) -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let file = "messages.json";
    let path = tmp.path().join(file);
    fs::write(&path, original)?;

    let patch = format!("*** Begin Patch\n*** Update File: {file}\n{patch_body}\n*** End Patch");
    run_patch_in_dir(tmp.path(), patch)?
        .success()
        .stdout(format!("Success. Updated the following files:\nM {file}\n"));
    assert_eq!(fs::read(&path)?, expected);
    Ok(())
}

#[test]
fn test_apply_patch_cli_add_and_update() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let file = "cli_test.txt";
    let absolute_path = tmp.path().join(file);

    // 1) Add a file
    let add_patch = format!(
        r#"*** Begin Patch
*** Add File: {file}
+hello
*** End Patch"#
    );
    apply_patch_command()?
        .arg(add_patch)
        .current_dir(tmp.path())
        .assert()
        .success()
        .stdout(format!("Success. Updated the following files:\nA {file}\n"));
    assert_eq!(fs::read_to_string(&absolute_path)?, "hello\n");

    // 2) Update the file
    let update_patch = format!(
        r#"*** Begin Patch
*** Update File: {file}
@@
-hello
+world
*** End Patch"#
    );
    apply_patch_command()?
        .arg(update_patch)
        .current_dir(tmp.path())
        .assert()
        .success()
        .stdout(format!("Success. Updated the following files:\nM {file}\n"));
    assert_eq!(fs::read_to_string(&absolute_path)?, "world\n");

    Ok(())
}

#[test]
fn test_apply_patch_cli_stdin_add_and_update() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let file = "cli_test_stdin.txt";
    let absolute_path = tmp.path().join(file);

    // 1) Add a file via stdin
    let add_patch = format!(
        r#"*** Begin Patch
*** Add File: {file}
+hello
*** End Patch"#
    );
    apply_patch_command()?
        .current_dir(tmp.path())
        .write_stdin(add_patch)
        .assert()
        .success()
        .stdout(format!("Success. Updated the following files:\nA {file}\n"));
    assert_eq!(fs::read_to_string(&absolute_path)?, "hello\n");

    // 2) Update the file via stdin
    let update_patch = format!(
        r#"*** Begin Patch
*** Update File: {file}
@@
-hello
+world
*** End Patch"#
    );
    apply_patch_command()?
        .current_dir(tmp.path())
        .write_stdin(update_patch)
        .assert()
        .success()
        .stdout(format!("Success. Updated the following files:\nM {file}\n"));
    assert_eq!(fs::read_to_string(&absolute_path)?, "world\n");

    Ok(())
}

#[test]
fn test_apply_patch_cli_updates_middle_json_line() -> anyhow::Result<()> {
    assert_json_patch_updates_file(
        b"{\n  \"first\": \"keep\",\n  \"middle\": \"before\",\n  \"last\": \"keep\"\n}\n",
        r#"@@
-  "middle": "before",
+  "middle": "after",
"#,
        b"{\n  \"first\": \"keep\",\n  \"middle\": \"after\",\n  \"last\": \"keep\"\n}\n",
    )
}

#[test]
fn test_apply_patch_cli_updates_penultimate_json_property_before_closing_brace()
-> anyhow::Result<()> {
    assert_json_patch_updates_file(
        b"{\n  \"first\": \"keep\",\n  \"penultimate\": \"before\",\n  \"last\": \"keep\"\n}\n",
        r#"@@
   "first": "keep",
-  "penultimate": "before",
+  "penultimate": "after",
   "last": "keep"
"#,
        b"{\n  \"first\": \"keep\",\n  \"penultimate\": \"after\",\n  \"last\": \"keep\"\n}\n",
    )
}

#[test]
fn test_apply_patch_cli_updates_final_json_property_with_closing_brace_context()
-> anyhow::Result<()> {
    assert_json_patch_updates_file(
        b"{\n  \"first\": \"keep\",\n  \"final\": \"before\"\n}\n",
        r#"@@
-  "final": "before"
+  "final": "after"
 }
*** End of File"#,
        b"{\n  \"first\": \"keep\",\n  \"final\": \"after\"\n}\n",
    )
}

#[test]
fn test_apply_patch_cli_updates_json_string_containing_backslash_n() -> anyhow::Result<()> {
    assert_json_patch_updates_file(
        b"{\n  \"copy\": \"line1\\nline2\",\n  \"tail\": \"keep\"\n}\n",
        r#"@@
-  "copy": "line1\nline2",
+  "copy": "line1\nline2\nline3",
"#,
        b"{\n  \"copy\": \"line1\\nline2\\nline3\",\n  \"tail\": \"keep\"\n}\n",
    )
}

#[test]
fn test_apply_patch_cli_updates_json_tail_with_crlf_line_endings() -> anyhow::Result<()> {
    assert_json_patch_updates_file(
        b"{\r\n  \"first\": \"keep\",\r\n  \"final\": \"before\"\r\n}\r\n",
        r#"@@
-  "final": "before"
+  "final": "after"
 }
*** End of File"#,
        b"{\r\n  \"first\": \"keep\",\r\n  \"final\": \"after\"\r\n}\r\n",
    )
}

use super::*;
use codex_apply_patch::MaybeApplyPatchVerified;
use codex_exec_server::LOCAL_FS;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::protocol::FileChange;
use core_test_support::PathBufExt;
use core_test_support::PathExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::Mutex;

use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::tools::context::ToolInvocation;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::turn_diff_tracker::TurnDiffTracker;

fn sample_patch() -> &'static str {
    r#"*** Begin Patch
*** Add File: hello.txt
+hello
*** End Patch"#
}

async fn invocation_for_payload(payload: ToolPayload) -> ToolInvocation {
    let (session, turn) = make_session_and_context().await;
    let turn = Arc::new(turn);
    ToolInvocation {
        session: session.into(),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        turn,
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: "call-apply-patch".to_string(),
        tool_name: codex_tools::ToolName::plain("apply_patch"),
        source: crate::tools::context::ToolCallSource::Direct,
        payload,
    }
}

#[tokio::test]
async fn pre_tool_use_payload_uses_freeform_patch_input() {
    let patch = sample_patch();
    let payload = ToolPayload::Custom {
        input: patch.to_string(),
    };
    let invocation = invocation_for_payload(payload).await;
    let handler = ApplyPatchHandler::default();

    assert_eq!(
        handler.pre_tool_use_payload(&invocation),
        Some(PreToolUsePayload {
            tool_name: HookToolName::apply_patch(),
            tool_input: json!({ "command": patch }),
        })
    );
}

#[tokio::test]
async fn post_tool_use_payload_uses_patch_input_and_tool_output() {
    let patch = sample_patch();
    let payload = ToolPayload::Custom {
        input: patch.to_string(),
    };
    let invocation = invocation_for_payload(payload).await;
    let output = ApplyPatchToolOutput::from_text("Success. Updated files.".to_string());
    let handler = ApplyPatchHandler::default();

    assert_eq!(
        handler.post_tool_use_payload(&invocation, &output),
        Some(PostToolUsePayload {
            tool_name: HookToolName::apply_patch(),
            tool_use_id: "call-apply-patch".to_string(),
            tool_input: json!({ "command": patch }),
            tool_response: json!("Success. Updated files."),
        })
    );
}

#[test]
fn diff_consumer_streams_apply_patch_changes() {
    let mut consumer = ApplyPatchArgumentDiffConsumer::default();
    assert!(
        consumer
            .push_delta("call-1".to_string(), "*** Begin Patch\n")
            .is_none()
    );

    let event = consumer
        .push_delta("call-1".to_string(), "*** Add File: hello.txt\n+hello")
        .expect("progress event");
    assert_eq!(
        (event.call_id, event.changes),
        (
            "call-1".to_string(),
            HashMap::from([(
                PathBuf::from("hello.txt"),
                FileChange::Add {
                    content: String::new(),
                },
            )]),
        )
    );

    assert!(
        consumer
            .push_delta("call-1".to_string(), "\n+world")
            .is_none()
    );
    assert!(
        consumer
            .push_delta("call-1".to_string(), "\n*** End Patch")
            .is_none()
    );

    let event = consumer
        .finish_update_on_complete()
        .expect("finish parser")
        .expect("progress event");
    assert_eq!(
        (event.call_id, event.changes),
        (
            "call-1".to_string(),
            HashMap::from([(
                PathBuf::from("hello.txt"),
                FileChange::Add {
                    content: "hello\nworld\n".to_string(),
                },
            )]),
        )
    );
}

#[test]
fn diff_consumer_streams_apply_patch_changes_with_environment_header() {
    let mut consumer = ApplyPatchArgumentDiffConsumer::default();
    assert!(
        consumer
            .push_delta(
                "call-1".to_string(),
                "*** Begin Patch\n*** Environment ID: remote\n",
            )
            .is_none()
    );

    let event = consumer
        .push_delta("call-1".to_string(), "*** Add File: hello.txt\n+hello")
        .expect("progress event");
    assert_eq!(
        event.changes,
        HashMap::from([(
            PathBuf::from("hello.txt"),
            FileChange::Add {
                content: String::new(),
            },
        )])
    );
}

#[test]
fn diff_consumer_skips_replace_file_streaming_preview_until_verification() {
    let mut consumer = ApplyPatchArgumentDiffConsumer::default();
    assert!(
        consumer
            .push_delta(
                "call-1".to_string(),
                "*** Begin Patch\n*** Replace File: hello.txt\n+hello"
            )
            .is_none()
    );
    assert!(
        consumer
            .push_delta("call-1".to_string(), "\n+world\n*** End Patch")
            .is_none()
    );
    assert!(
        consumer
            .finish_update_on_complete()
            .expect("finish parser")
            .is_none()
    );
}

#[test]
fn diff_consumer_sends_next_update_after_buffer_interval() {
    let mut consumer = ApplyPatchArgumentDiffConsumer::default();
    consumer.push_delta("call-1".to_string(), "*** Begin Patch\n");
    let first = consumer
        .push_delta("call-1".to_string(), "*** Add File: hello.txt\n+hello")
        .expect("first progress event");
    assert_eq!(
        first.changes,
        HashMap::from([(
            PathBuf::from("hello.txt"),
            FileChange::Add {
                content: String::new(),
            },
        )])
    );

    consumer.last_sent_at =
        Some(std::time::Instant::now() - APPLY_PATCH_ARGUMENT_DIFF_BUFFER_INTERVAL);
    let second = consumer
        .push_delta("call-1".to_string(), "\n+world")
        .expect("second progress event");
    assert_eq!(
        second.changes,
        HashMap::from([(
            PathBuf::from("hello.txt"),
            FileChange::Add {
                content: "hello\n".to_string(),
            },
        )])
    );
}

#[test]
fn reconcile_environment_id_requires_selection_when_enabled() {
    assert_eq!(
        require_environment_id(Some("remote"), /*allow_environment_id*/ false),
        Err(FunctionCallError::RespondToModel(
            "apply_patch environment selection is unavailable for this turn".to_string(),
        ))
    );
    assert_eq!(
        require_environment_id(
            /*parsed_environment_id*/ None, /*allow_environment_id*/ true
        ),
        Ok(None)
    );
}

#[tokio::test]
async fn approval_keys_include_move_destination() {
    let tmp = TempDir::new().expect("tmp");
    let cwd_path = tmp.path();
    let cwd = cwd_path.abs();
    std::fs::create_dir_all(cwd_path.join("old")).expect("create old dir");
    std::fs::create_dir_all(cwd_path.join("renamed/dir")).expect("create dest dir");
    std::fs::write(cwd_path.join("old/name.txt"), "old content\n").expect("write old file");
    let patch = r#"*** Begin Patch
*** Update File: old/name.txt
*** Move to: renamed/dir/name.txt
@@
-old content
+new content
*** End Patch"#;
    let argv = vec!["apply_patch".to_string(), patch.to_string()];
    // TODO(anp): Keep apply_patch handler test cwd values as PathUri.
    let cwd = PathUri::from_abs_path(&cwd);
    let action = match codex_apply_patch::maybe_parse_apply_patch_verified(
        &argv,
        &cwd,
        LOCAL_FS.as_ref(),
        /*sandbox*/ None,
    )
    .await
    {
        MaybeApplyPatchVerified::Body(action) => action,
        other => panic!("expected patch body, got: {other:?}"),
    };

    let keys = file_paths_for_action(&action);
    assert_eq!(keys.len(), 2);
}

#[test]
fn write_permissions_for_paths_skip_dirs_already_writable_under_workspace_root() {
    let tmp = TempDir::new().expect("tmp");
    let cwd_path = tmp.path();
    let cwd = cwd_path.abs();
    let nested = cwd_path.join("nested");
    std::fs::create_dir_all(&nested).expect("create nested dir");
    let file_path = AbsolutePathBuf::try_from(nested.join("file.txt"))
        .expect("nested file path should be absolute");
    let sandbox_policy = FileSystemSandboxPolicy::workspace_write(
        &[],
        /*exclude_tmpdir_env_var*/ true,
        /*exclude_slash_tmp*/ false,
    );

    let permissions = write_permissions_for_paths(&[file_path], &sandbox_policy, &cwd);

    assert_eq!(permissions, None);
}

#[test]
fn write_permissions_for_paths_keep_dirs_outside_workspace_root() {
    let tmp = TempDir::new().expect("tmp");
    let cwd = tmp.path().join("workspace");
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    std::fs::create_dir_all(&outside).expect("create outside dir");
    let file_path = AbsolutePathBuf::try_from(outside.join("file.txt"))
        .expect("outside file path should be absolute");
    let cwd_abs = cwd.abs();
    let sandbox_policy = FileSystemSandboxPolicy::workspace_write(
        &[],
        /*exclude_tmpdir_env_var*/ true,
        /*exclude_slash_tmp*/ true,
    );

    let permissions = write_permissions_for_paths(&[file_path], &sandbox_policy, &cwd_abs);
    let expected_outside =
        dunce::simplified(&outside.canonicalize().expect("canonicalize outside dir")).abs();

    assert_eq!(
        permissions
            .and_then(|profile| profile.file_system)
            .and_then(|fs| fs.legacy_read_write_roots())
            .and_then(|roots| roots.write),
        Some(vec![expected_outside])
    );
}

#[test]
fn apply_patch_verification_errors_are_bounded_for_model_context() {
    let error = apply_patch_verification_error("diagnostic ".repeat(10_000));
    let FunctionCallError::RespondToModel(message) = error else {
        panic!("expected a model-facing verification error");
    };

    assert!(message.contains("tokens truncated"));
    assert!(
        codex_utils_output_truncation::approx_token_count(&message)
            <= APPLY_PATCH_DIAGNOSTIC_MAX_TOKENS + 10
    );
}

#[tokio::test]
async fn ambiguous_path_diagnostic_caps_reported_paths() {
    let tmp = TempDir::new().expect("tmp");
    let cwd = PathUri::from_abs_path(&tmp.path().abs());
    let mut hunks = Vec::new();
    let mut tracked_delta = codex_apply_patch::AppliedPatchDelta::default();
    for index in 0..AMBIGUOUS_PATH_DIAGNOSTIC_MAX_PATHS + 3 {
        let relative_path = format!("ambiguous-{index}.txt");
        let path = tmp.path().join(&relative_path);
        std::fs::write(&path, "same\nsame\n").expect("write ambiguous file");
        let patch = format!(
            "*** Begin Patch\n*** Update File: {relative_path}\n@@\n-same\n+changed\n*** End Patch"
        );
        hunks.extend(
            codex_apply_patch::parse_patch(&patch)
                .expect("patch should parse")
                .hunks,
        );
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        tracked_delta.append(
            codex_apply_patch::apply_patch(
                &patch,
                &cwd,
                &mut stdout,
                &mut stderr,
                LOCAL_FS.as_ref(),
                /*sandbox*/ None,
            )
            .await
            .expect("initial update should apply"),
        );
        std::fs::write(path, "same\nsame\n").expect("restore ambiguous contents");
    }
    let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
    tracker.lock().await.track_delta("local", &tracked_delta);

    let error = ensure_exact_match_updates_use_fresh_context(
        &tracker,
        &hunks,
        &cwd,
        LOCAL_FS.as_ref(),
        /*sandbox*/ None,
    )
    .await
    .expect_err("repeated anchors should require fresh context");
    let FunctionCallError::RespondToModel(message) = error else {
        panic!("expected a model-facing ambiguity error");
    };

    assert!(message.contains("[3 additional ambiguous paths omitted]"));
}

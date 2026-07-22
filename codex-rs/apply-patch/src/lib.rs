mod invocation;
mod parser;
mod seek_sequence;
mod standalone_executable;
mod streaming_parser;

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use codex_exec_server::CreateDirectoryOptions;
use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::FileSystemSandboxContext;
use codex_exec_server::RemoveOptions;
use codex_utils_path_uri::PathUri;
use codex_utils_path_uri::PathUriParseError;
use codex_utils_string::take_bytes_at_char_boundary;
pub use parser::Hunk;
pub use parser::ParseError;
use parser::ParseError::*;
pub use parser::UpdateFileChunk;
pub use parser::parse_patch;
use similar::TextDiff;
pub use streaming_parser::StreamingPatchParser;
use thiserror::Error;

pub use invocation::maybe_parse_apply_patch_verified;
pub use invocation::verify_apply_patch_args;
pub use standalone_executable::main;

use crate::invocation::ExtractHeredocError;

/// Special argv[1] flag used when the Codex executable self-invokes to run the
/// internal `apply_patch` path.
///
/// Although this constant lives in `codex-apply-patch` (to avoid forcing
/// `codex-arg0` to depend on `codex-core`), it remains part of the "codex core"
/// process-invocation contract for the standalone `apply_patch` command
/// surface.
pub const CODEX_CORE_APPLY_PATCH_ARG1: &str = "--codex-run-as-apply-patch";

#[derive(Debug, Error, PartialEq)]
pub enum ApplyPatchError {
    #[error(transparent)]
    ParseError(#[from] ParseError),
    #[error(transparent)]
    IoError(#[from] IoError),
    /// Error that occurs while computing replacements when applying patch chunks
    #[error("{0}")]
    ComputeReplacements(String),
    /// A patch path could not be resolved as a path URI.
    #[error(transparent)]
    PathUri(#[from] PathUriParseError),
    /// A raw patch body was provided without an explicit `apply_patch` invocation.
    #[error(
        "patch detected without explicit call to apply_patch. Rerun as [\"apply_patch\", \"<patch>\"]"
    )]
    ImplicitInvocation,
}

impl From<std::io::Error> for ApplyPatchError {
    fn from(err: std::io::Error) -> Self {
        ApplyPatchError::IoError(IoError {
            context: "I/O error".to_string(),
            source: err,
        })
    }
}

impl From<&std::io::Error> for ApplyPatchError {
    fn from(err: &std::io::Error) -> Self {
        ApplyPatchError::IoError(IoError {
            context: "I/O error".to_string(),
            source: std::io::Error::new(err.kind(), err.to_string()),
        })
    }
}

#[derive(Debug, Error)]
#[error("{context}: {source}")]
pub struct IoError {
    context: String,
    #[source]
    source: std::io::Error,
}

impl PartialEq for IoError {
    fn eq(&self, other: &Self) -> bool {
        self.context == other.context && self.source.to_string() == other.source.to_string()
    }
}

/// Both the raw PATCH argument to `apply_patch` as well as the PATCH argument
/// parsed into hunks.
#[derive(Debug, PartialEq)]
pub struct ApplyPatchArgs {
    pub patch: String,
    pub hunks: Vec<Hunk>,
    pub workdir: Option<String>,
    pub environment_id: Option<String>,
}

#[derive(Debug, PartialEq)]
pub enum ApplyPatchFileChange {
    Add {
        content: String,
    },
    Delete {
        content: String,
    },
    Update {
        unified_diff: String,
        move_path: Option<PathUri>,
        /// new_content that will result after the unified_diff is applied.
        new_content: String,
    },
}

#[derive(Debug, PartialEq)]
pub enum MaybeApplyPatchVerified {
    /// `argv` corresponded to an `apply_patch` invocation, and these are the
    /// resulting proposed file changes.
    Body(ApplyPatchAction),
    /// `argv` could not be parsed to determine whether it corresponds to an
    /// `apply_patch` invocation.
    ShellParseError(ExtractHeredocError),
    /// `argv` corresponded to an `apply_patch` invocation, but it could not
    /// be fulfilled due to the specified error.
    CorrectnessError(ApplyPatchError),
    /// `argv` decidedly did not correspond to an `apply_patch` invocation.
    NotApplyPatch,
}

/// ApplyPatchAction is the result of parsing an `apply_patch` command. By
/// construction, all paths should be absolute paths.
#[derive(Debug, PartialEq)]
pub struct ApplyPatchAction {
    changes: HashMap<PathUri, ApplyPatchFileChange>,

    /// The raw patch argument that can be used to apply the patch. i.e., if the
    /// original arg was parsed in "lenient" mode with a
    /// heredoc, this should be the value without the heredoc wrapper.
    pub patch: String,

    /// The working directory that was used to resolve relative paths in the patch.
    pub cwd: PathUri,
}

impl ApplyPatchAction {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Returns the changes that would be made by applying the patch.
    pub fn changes(&self) -> &HashMap<PathUri, ApplyPatchFileChange> {
        &self.changes
    }

    /// Should be used exclusively for testing. (Not worth the overhead of
    /// creating a feature flag for this.)
    pub fn new_add_for_test(path: &PathUri, content: String) -> Self {
        #[expect(clippy::expect_used)]
        let filename = path.basename().expect("path should not be empty");
        let patch = format!(
            r#"*** Begin Patch
*** Update File: {filename}
@@
+ {content}
*** End Patch"#,
        );
        let changes = HashMap::from([(path.clone(), ApplyPatchFileChange::Add { content })]);
        #[expect(clippy::expect_used)]
        Self {
            changes,
            cwd: path.parent().expect("path should have parent"),
            patch,
        }
    }
}

/// Textual file changes that were actually committed while applying a patch.
#[derive(Clone, Debug, PartialEq)]
pub struct AppliedPatchDelta {
    changes: Vec<AppliedPatchChange>,
    exact: bool,
}

impl AppliedPatchDelta {
    fn new(changes: Vec<AppliedPatchChange>, exact: bool) -> Self {
        Self { changes, exact }
    }

    fn empty() -> Self {
        Self::new(Vec::new(), /*exact*/ true)
    }

    pub fn changes(&self) -> &[AppliedPatchChange] {
        &self.changes
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn is_exact(&self) -> bool {
        self.exact
    }

    /// Appends a later committed prefix while preserving the aggregate exactness.
    pub fn append(&mut self, other: Self) {
        self.changes.extend(other.changes);
        self.exact &= other.exact;
    }
}

impl Default for AppliedPatchDelta {
    fn default() -> Self {
        Self::empty()
    }
}

/// A committed file change, preserved in the order it was applied.
#[derive(Clone, Debug, PartialEq)]
pub struct AppliedPatchChange {
    pub path: PathBuf,
    pub change: AppliedPatchFileChange,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AppliedPatchFileChange {
    Add {
        content: String,
        overwritten_content: Option<String>,
    },
    Delete {
        content: String,
    },
    Update {
        move_path: Option<PathBuf>,
        old_content: String,
        overwritten_move_content: Option<String>,
        new_content: String,
    },
}

/// A failed patch application together with the textual mutations that were
/// definitely committed before the failure was observed.
#[derive(Debug, Error)]
#[error("{error}")]
pub struct ApplyPatchFailure {
    #[source]
    error: ApplyPatchError,
    delta: AppliedPatchDelta,
}

impl ApplyPatchFailure {
    fn new(error: ApplyPatchError, delta: AppliedPatchDelta) -> Self {
        Self { error, delta }
    }

    fn without_delta(error: ApplyPatchError) -> Self {
        Self::new(error, AppliedPatchDelta::empty())
    }

    pub fn delta(&self) -> &AppliedPatchDelta {
        &self.delta
    }

    pub fn into_parts(self) -> (ApplyPatchError, AppliedPatchDelta) {
        (self.error, self.delta)
    }
}

/// Applies the patch and prints the result to stdout/stderr.
pub async fn apply_patch(
    patch: &str,
    cwd: &PathUri,
    stdout: &mut impl std::io::Write,
    stderr: &mut impl std::io::Write,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> Result<AppliedPatchDelta, ApplyPatchFailure> {
    let hunks = match parse_patch(patch) {
        Ok(source) => source.hunks,
        Err(e) => {
            match &e {
                InvalidPatchError(message) => {
                    writeln!(stderr, "Invalid patch: {message}")
                        .map_err(ApplyPatchError::from)
                        .map_err(ApplyPatchFailure::without_delta)?;
                }
                InvalidHunkError {
                    message,
                    line_number,
                } => {
                    writeln!(
                        stderr,
                        "Invalid patch hunk on line {line_number}: {message}"
                    )
                    .map_err(ApplyPatchError::from)
                    .map_err(ApplyPatchFailure::without_delta)?;
                }
            }
            return Err(ApplyPatchFailure::without_delta(
                ApplyPatchError::ParseError(e),
            ));
        }
    };

    apply_hunks(&hunks, cwd, stdout, stderr, fs, sandbox).await
}

/// Applies hunks and continues to update stdout/stderr
pub async fn apply_hunks(
    hunks: &[Hunk],
    cwd: &PathUri,
    stdout: &mut impl std::io::Write,
    stderr: &mut impl std::io::Write,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> Result<AppliedPatchDelta, ApplyPatchFailure> {
    let mut delta = AppliedPatchDelta::empty();
    match apply_hunks_to_files(hunks, cwd, fs, sandbox, &mut delta).await {
        Ok(affected_paths) => {
            print_summary(&affected_paths, stdout).map_err(|error| {
                ApplyPatchFailure::new(ApplyPatchError::from(error), delta.clone())
            })?;
            Ok(delta)
        }
        Err(error) => {
            let msg = error.to_string();
            writeln!(stderr, "{msg}").map_err(|error| {
                ApplyPatchFailure::new(ApplyPatchError::from(error), delta.clone())
            })?;
            let error = if let Some(io) = error.downcast_ref::<std::io::Error>() {
                ApplyPatchError::from(io)
            } else {
                ApplyPatchError::IoError(IoError {
                    context: msg,
                    source: std::io::Error::other(error),
                })
            };
            Err(ApplyPatchFailure::new(error, delta))
        }
    }
}

const STALE_PATCH_REFRESH_HINT: &str = "This usually means the file changed since the patch was drafted or was edited earlier in the turn. Re-read the live file and retry with current context.";
const STALE_PATCH_LIVE_CONTEXT_MAX_LINES: usize = 8;
const STALE_PATCH_LIVE_CONTEXT_MAX_LINE_BYTES: usize = 256;

/// Applies each parsed patch hunk to the filesystem.
/// Returns an error if any of the changes could not be applied.
/// Tracks file paths affected by applying a patch, preserving the path spelling
/// from the patch for user-facing summaries.
pub struct AffectedPaths {
    pub added: Vec<PathBuf>,
    pub modified: Vec<PathBuf>,
    pub deleted: Vec<PathBuf>,
}

/// Apply the hunks to the filesystem, returning which files were added, modified, or deleted.
/// Returns an error if the patch could not be applied.
async fn apply_hunks_to_files(
    hunks: &[Hunk],
    cwd: &PathUri,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
    delta: &mut AppliedPatchDelta,
) -> anyhow::Result<AffectedPaths> {
    if hunks.is_empty() {
        anyhow::bail!("No files were modified.");
    }

    let plan = stage_hunks_to_files(hunks, cwd, fs, sandbox).await?;
    delta.exact = plan.exact;
    commit_staged_patch_plan(plan, fs, sandbox, delta).await
}

#[derive(Clone, Copy, Debug)]
enum AffectedKind {
    Add,
    Modify,
    Delete,
}

#[derive(Clone, Debug)]
enum PriorFileState {
    Missing,
    Existing {
        bytes: Vec<u8>,
        text: Option<String>,
    },
    UnreadableExisting,
}

impl PriorFileState {
    fn from_text(contents: String) -> Self {
        let bytes = contents.clone().into_bytes();
        Self::Existing {
            bytes,
            text: Some(contents),
        }
    }

    fn from_bytes(bytes: Vec<u8>, exact: &mut bool) -> Self {
        let text = match String::from_utf8(bytes.clone()) {
            Ok(contents) => Some(contents),
            Err(_) => {
                *exact = false;
                None
            }
        };
        Self::Existing { bytes, text }
    }

    fn cloned_known_text(&self) -> Option<String> {
        match self {
            Self::Missing | Self::UnreadableExisting => None,
            Self::Existing { text, .. } => text.clone(),
        }
    }
}

#[derive(Clone, Debug)]
enum CommitOp {
    Write {
        path: PathUri,
        new_contents: String,
        old_state: PriorFileState,
    },
    Delete {
        path: PathUri,
        old_state: PriorFileState,
    },
}

#[derive(Clone, Debug)]
struct PlannedChangeGroup {
    commit_ops: Vec<CommitOp>,
    affected_kind: AffectedKind,
    affected_path: PathBuf,
    delta_change: Option<AppliedPatchChange>,
}

#[derive(Debug)]
struct StagedPatchPlan {
    groups: Vec<PlannedChangeGroup>,
    exact: bool,
}

async fn stage_hunks_to_files(
    hunks: &[Hunk],
    cwd: &PathUri,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> anyhow::Result<StagedPatchPlan> {
    let mut groups = Vec::new();
    let mut exact = true;
    let mut staged_contents = HashMap::<PathBuf, Option<String>>::new();

    // TODO(anp): Carry PathUri through committed patch deltas and the turn diff tracker.
    for hunk in hunks {
        let affected_path = hunk.path().to_path_buf();
        let path_uri = hunk.resolve_path(cwd)?;
        match hunk {
            Hunk::AddFile { contents, .. } => {
                let overwritten_state = read_optional_file_state_for_plan(
                    &path_uri,
                    &staged_contents,
                    fs,
                    sandbox,
                    &mut exact,
                )
                .await;
                let overwritten_content = overwritten_state.cloned_known_text();
                staged_contents.insert(path_uri.to_path_buf(), Some(contents.clone()));
                groups.push(PlannedChangeGroup {
                    commit_ops: vec![CommitOp::Write {
                        path: path_uri.clone(),
                        new_contents: contents.clone(),
                        old_state: overwritten_state,
                    }],
                    affected_kind: AffectedKind::Add,
                    affected_path,
                    delta_change: Some(AppliedPatchChange {
                        path: path_uri.to_path_buf(),
                        change: AppliedPatchFileChange::Add {
                            content: contents.clone(),
                            overwritten_content,
                        },
                    }),
                });
            }
            Hunk::DeleteFile { .. } => {
                let deleted_state = read_deleted_file_state_for_plan(
                    &path_uri,
                    &staged_contents,
                    fs,
                    sandbox,
                    &mut exact,
                )
                .await?;
                let deleted_content = deleted_state.cloned_known_text();
                staged_contents.insert(path_uri.to_path_buf(), None);
                groups.push(PlannedChangeGroup {
                    commit_ops: vec![CommitOp::Delete {
                        path: path_uri.clone(),
                        old_state: deleted_state,
                    }],
                    affected_kind: AffectedKind::Delete,
                    affected_path,
                    delta_change: deleted_content.map(|content| AppliedPatchChange {
                        path: path_uri.to_path_buf(),
                        change: AppliedPatchFileChange::Delete { content },
                    }),
                });
            }
            Hunk::ReplaceFile { contents, .. } => {
                let original_contents = read_required_file_text_for_plan(
                    &path_uri,
                    &staged_contents,
                    fs,
                    sandbox,
                    &mut exact,
                    "Failed to read file to replace",
                )
                .await?;
                let new_contents =
                    normalize_full_file_replacement_contents(contents, &original_contents);
                staged_contents.insert(path_uri.to_path_buf(), Some(new_contents.clone()));
                groups.push(PlannedChangeGroup {
                    commit_ops: vec![CommitOp::Write {
                        path: path_uri.clone(),
                        new_contents: new_contents.clone(),
                        old_state: PriorFileState::from_text(original_contents.clone()),
                    }],
                    affected_kind: AffectedKind::Modify,
                    affected_path,
                    delta_change: Some(AppliedPatchChange {
                        path: path_uri.to_path_buf(),
                        change: AppliedPatchFileChange::Update {
                            move_path: None,
                            old_content: original_contents,
                            overwritten_move_content: None,
                            new_content: new_contents,
                        },
                    }),
                });
            }
            Hunk::UpdateFile {
                move_path, chunks, ..
            } => {
                let original_contents = read_required_file_text_for_plan(
                    &path_uri,
                    &staged_contents,
                    fs,
                    sandbox,
                    &mut exact,
                    "Failed to read file to update",
                )
                .await?;
                let new_contents = derive_new_contents_from_original_contents(
                    &original_contents,
                    &path_uri.inferred_native_path_string(),
                    chunks,
                )?;
                let dest_uri = match move_path {
                    Some(dest) => Some(cwd.join(&dest.to_string_lossy())?),
                    None => None,
                };
                if let Some(dest_uri) = dest_uri.filter(|dest_uri| dest_uri != &path_uri) {
                    let overwritten_move_state = read_optional_file_state_for_plan(
                        &dest_uri,
                        &staged_contents,
                        fs,
                        sandbox,
                        &mut exact,
                    )
                    .await;
                    let overwritten_move_content = overwritten_move_state.cloned_known_text();
                    ensure_not_directory(&path_uri, fs, sandbox)
                        .await
                        .with_context(|| {
                            format!(
                                "Failed to remove original {}",
                                path_uri.inferred_native_path_string()
                            )
                        })?;
                    staged_contents.insert(path_uri.to_path_buf(), None);
                    staged_contents.insert(dest_uri.to_path_buf(), Some(new_contents.clone()));
                    groups.push(PlannedChangeGroup {
                        commit_ops: vec![
                            CommitOp::Write {
                                path: dest_uri.clone(),
                                new_contents: new_contents.clone(),
                                old_state: overwritten_move_state,
                            },
                            CommitOp::Delete {
                                path: path_uri.clone(),
                                old_state: PriorFileState::from_text(original_contents.clone()),
                            },
                        ],
                        affected_kind: AffectedKind::Modify,
                        affected_path,
                        delta_change: Some(AppliedPatchChange {
                            path: path_uri.to_path_buf(),
                            change: AppliedPatchFileChange::Update {
                                move_path: Some(dest_uri.to_path_buf()),
                                old_content: original_contents,
                                overwritten_move_content,
                                new_content: new_contents,
                            },
                        }),
                    });
                } else {
                    staged_contents.insert(path_uri.to_path_buf(), Some(new_contents.clone()));
                    groups.push(PlannedChangeGroup {
                        commit_ops: vec![CommitOp::Write {
                            path: path_uri.clone(),
                            new_contents: new_contents.clone(),
                            old_state: PriorFileState::from_text(original_contents.clone()),
                        }],
                        affected_kind: AffectedKind::Modify,
                        affected_path,
                        delta_change: Some(AppliedPatchChange {
                            path: path_uri.to_path_buf(),
                            change: AppliedPatchFileChange::Update {
                                move_path: None,
                                old_content: original_contents,
                                overwritten_move_content: None,
                                new_content: new_contents,
                            },
                        }),
                    });
                }
            }
        }
    }

    Ok(StagedPatchPlan { groups, exact })
}

async fn commit_staged_patch_plan(
    plan: StagedPatchPlan,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
    delta: &mut AppliedPatchDelta,
) -> anyhow::Result<AffectedPaths> {
    let mut added = Vec::new();
    let mut modified = Vec::new();
    let mut deleted = Vec::new();
    let mut executed_ops = Vec::<CommitOp>::new();

    for group in plan.groups {
        let committed_before_group = executed_ops.len();
        for op in &group.commit_ops {
            if let Err(error) = execute_commit_op(op, fs, sandbox).await {
                let failed_op_rollback_success = rollback_failed_commit_op(op, fs, sandbox).await;
                let executed_ops_rollback_success =
                    rollback_commit_ops(&executed_ops, fs, sandbox).await;
                if executed_ops_rollback_success {
                    delta.changes.clear();
                }
                delta.exact = failed_op_rollback_success && executed_ops_rollback_success;
                executed_ops.truncate(committed_before_group);
                return Err(error);
            }
            executed_ops.push(op.clone());
        }

        if let Some(change) = group.delta_change {
            delta.changes.push(change);
        }
        match group.affected_kind {
            AffectedKind::Add => added.push(group.affected_path),
            AffectedKind::Modify => modified.push(group.affected_path),
            AffectedKind::Delete => deleted.push(group.affected_path),
        }
    }

    Ok(AffectedPaths {
        added,
        modified,
        deleted,
    })
}

async fn execute_commit_op(
    op: &CommitOp,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> anyhow::Result<()> {
    match op {
        CommitOp::Write {
            path, new_contents, ..
        } => {
            write_file_with_missing_parent_retry(
                fs,
                path,
                new_contents.clone().into_bytes(),
                sandbox,
            )
            .await
        }
        CommitOp::Delete { path, .. } => fs
            .remove(
                path,
                RemoveOptions {
                    recursive: false,
                    force: false,
                },
                sandbox,
            )
            .await
            .with_context(|| {
                format!(
                    "Failed to delete file {}",
                    path.inferred_native_path_string()
                )
            }),
    }
}

async fn rollback_commit_ops(
    executed_ops: &[CommitOp],
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> bool {
    let mut success = true;
    for op in executed_ops.iter().rev() {
        if rollback_commit_op(op, fs, sandbox).await.is_err() {
            success = false;
        }
    }
    success
}

async fn rollback_failed_commit_op(
    op: &CommitOp,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> bool {
    rollback_commit_op(op, fs, sandbox).await.is_ok()
}

async fn rollback_commit_op(
    op: &CommitOp,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> anyhow::Result<()> {
    match op {
        CommitOp::Write {
            path, old_state, ..
        } => match old_state {
            PriorFileState::Existing { bytes, .. } => {
                write_file_with_missing_parent_retry(fs, path, bytes.clone(), sandbox).await
            }
            PriorFileState::Missing => remove_file_if_present(path, fs, sandbox).await,
            PriorFileState::UnreadableExisting => anyhow::bail!(
                "cannot roll back overwrite of {}",
                path.inferred_native_path_string()
            ),
        },
        CommitOp::Delete { path, old_state } => match old_state {
            PriorFileState::Existing { bytes, .. } => {
                write_file_with_missing_parent_retry(fs, path, bytes.clone(), sandbox).await
            }
            PriorFileState::Missing | PriorFileState::UnreadableExisting => anyhow::bail!(
                "cannot roll back delete of {}",
                path.inferred_native_path_string()
            ),
        },
    }
}

async fn ensure_not_directory(
    path: &PathUri,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> io::Result<()> {
    let metadata = fs.get_metadata(path, sandbox).await?;
    if metadata.is_directory {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path is a directory",
        ));
    }
    Ok(())
}

async fn read_optional_file_state_for_plan(
    path: &PathUri,
    staged_contents: &HashMap<PathBuf, Option<String>>,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
    exact: &mut bool,
) -> PriorFileState {
    if let Some(contents) = staged_contents.get(&path.to_path_buf()) {
        return match contents {
            Some(contents) => PriorFileState::from_text(contents.clone()),
            None => PriorFileState::Missing,
        };
    }

    note_existing_path_delta_support(path, fs, sandbox, exact).await;
    match fs.read_file(path, sandbox).await {
        Ok(bytes) => PriorFileState::from_bytes(bytes, exact),
        Err(source) if source.kind() == io::ErrorKind::NotFound => PriorFileState::Missing,
        Err(_) => {
            *exact = false;
            PriorFileState::UnreadableExisting
        }
    }
}

async fn read_deleted_file_state_for_plan(
    path: &PathUri,
    staged_contents: &HashMap<PathBuf, Option<String>>,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
    exact: &mut bool,
) -> anyhow::Result<PriorFileState> {
    if let Some(contents) = staged_contents.get(&path.to_path_buf()) {
        return match contents {
            Some(contents) => Ok(PriorFileState::from_text(contents.clone())),
            None => anyhow::bail!(
                "Failed to delete file {}",
                path.inferred_native_path_string()
            ),
        };
    }

    note_existing_path_delta_support(path, fs, sandbox, exact).await;
    let deleted_state = match fs.read_file(path, sandbox).await {
        Ok(bytes) => PriorFileState::from_bytes(bytes, exact),
        Err(source) if source.kind() == io::ErrorKind::NotFound => PriorFileState::Missing,
        Err(_) => {
            *exact = false;
            PriorFileState::UnreadableExisting
        }
    };
    ensure_not_directory(path, fs, sandbox)
        .await
        .with_context(|| {
            format!(
                "Failed to delete file {}",
                path.inferred_native_path_string()
            )
        })?;
    Ok(deleted_state)
}

async fn read_required_file_text_for_plan(
    path: &PathUri,
    staged_contents: &HashMap<PathBuf, Option<String>>,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
    exact: &mut bool,
    context: &str,
) -> std::result::Result<String, ApplyPatchError> {
    if let Some(contents) = staged_contents.get(&path.to_path_buf()) {
        return match contents {
            Some(contents) => Ok(contents.clone()),
            None => Err(ApplyPatchError::IoError(IoError {
                context: format!("{context} {}", path.inferred_native_path_string()),
                source: io::Error::from(io::ErrorKind::NotFound),
            })),
        };
    }

    note_existing_path_delta_support(path, fs, sandbox, exact).await;
    fs.read_file_text(path, sandbox).await.map_err(|source| {
        ApplyPatchError::IoError(IoError {
            context: format!("{context} {}", path.inferred_native_path_string()),
            source,
        })
    })
}

async fn note_existing_path_delta_support(
    path: &PathUri,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
    exact: &mut bool,
) {
    match fs.get_metadata(path, sandbox).await {
        Ok(metadata) if metadata.is_file && !metadata.is_symlink => {}
        Ok(_) => *exact = false,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(_) => *exact = false,
    }
}

async fn remove_file_if_present(
    path: &PathUri,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> anyhow::Result<()> {
    match fs
        .remove(
            path,
            RemoveOptions {
                recursive: false,
                force: false,
            },
            sandbox,
        )
        .await
    {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(source).with_context(|| {
            format!(
                "Failed to delete file {}",
                path.inferred_native_path_string()
            )
        }),
    }
}

async fn write_file_with_missing_parent_retry(
    fs: &dyn ExecutorFileSystem,
    path: &PathUri,
    contents: Vec<u8>,
    sandbox: Option<&FileSystemSandboxContext>,
) -> anyhow::Result<()> {
    match fs.write_file(path, contents.clone(), sandbox).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                fs.create_directory(&parent, CreateDirectoryOptions { recursive: true }, sandbox)
                    .await
                    .with_context(|| {
                        format!(
                            "Failed to create parent directories for {}",
                            path.inferred_native_path_string()
                        )
                    })?;
            }
            fs.write_file(path, contents, sandbox)
                .await
                .with_context(|| {
                    format!(
                        "Failed to write file {}",
                        path.inferred_native_path_string()
                    )
                })?;
            Ok(())
        }
        Err(err) => Err(err).with_context(|| {
            format!(
                "Failed to write file {}",
                path.inferred_native_path_string()
            )
        }),
    }
}

struct AppliedPatch {
    original_contents: String,
    new_contents: String,
}

/// Detect the dominant line ending of `contents` so a rewritten file keeps the
/// ending it had on disk. Returns `"\r\n"` only when CRLF is the strict majority
/// of newlines; otherwise `"\n"` (the safe default for LF files and files with
/// no newline at all).
fn detect_line_ending(contents: &str) -> &'static str {
    let crlf = contents.matches("\r\n").count();
    let lone_lf = contents.matches('\n').count().saturating_sub(crlf);
    if crlf > lone_lf { "\r\n" } else { "\n" }
}

fn normalize_full_file_replacement_contents(
    replacement_contents: &str,
    original_contents: &str,
) -> String {
    if replacement_contents.is_empty() {
        return String::new();
    }

    let line_ending = detect_line_ending(original_contents);
    let mut replacement_lines = replacement_contents.split('\n').collect::<Vec<_>>();
    if replacement_lines.last().is_some_and(|line| line.is_empty()) {
        replacement_lines.pop();
    }

    if replacement_lines.is_empty() {
        String::new()
    } else {
        format!("{}{line_ending}", replacement_lines.join(line_ending))
    }
}

fn build_unified_diff(original_contents: &str, new_contents: &str, context: usize) -> String {
    TextDiff::from_lines(original_contents, new_contents)
        .unified_diff()
        .context_radius(context)
        .to_string()
}

async fn derive_replaced_file_contents(
    path: &PathUri,
    replacement_contents: &str,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> std::result::Result<AppliedPatch, ApplyPatchError> {
    let original_contents = fs.read_file_text(path, sandbox).await.map_err(|err| {
        ApplyPatchError::IoError(IoError {
            context: format!(
                "Failed to read file to replace {}",
                path.inferred_native_path_string()
            ),
            source: err,
        })
    })?;
    let new_contents =
        normalize_full_file_replacement_contents(replacement_contents, &original_contents);
    Ok(AppliedPatch {
        original_contents,
        new_contents,
    })
}

fn derive_new_contents_from_original_contents(
    original_contents: &str,
    path: &str,
    chunks: &[UpdateFileChunk],
) -> std::result::Result<String, ApplyPatchError> {
    let line_ending = detect_line_ending(original_contents);
    let mut original_lines: Vec<String> = original_contents
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
        .collect();

    if original_lines.last().is_some_and(String::is_empty) {
        original_lines.pop();
    }

    let replacements = compute_replacements(&original_lines, path, chunks)?;
    let mut new_lines = apply_replacements(original_lines, &replacements);
    if !new_lines.last().is_some_and(String::is_empty) {
        new_lines.push(String::new());
    }
    Ok(new_lines.join(line_ending))
}

pub(crate) async fn full_file_update_from_replacement(
    path: &PathUri,
    replacement_contents: &str,
    context: usize,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> std::result::Result<ApplyPatchFileUpdate, ApplyPatchError> {
    let AppliedPatch {
        original_contents,
        new_contents,
    } = derive_replaced_file_contents(path, replacement_contents, fs, sandbox).await?;
    Ok(ApplyPatchFileUpdate {
        unified_diff: build_unified_diff(&original_contents, &new_contents, context),
        original_content: original_contents,
        content: new_contents,
    })
}

/// Return *only* the new file contents (joined into a single `String`) after
/// applying the chunks to the file at `path`.
async fn derive_new_contents_from_chunks(
    path: &PathUri,
    chunks: &[UpdateFileChunk],
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> std::result::Result<AppliedPatch, ApplyPatchError> {
    let original_contents = fs.read_file_text(path, sandbox).await.map_err(|err| {
        ApplyPatchError::IoError(IoError {
            context: format!(
                "Failed to read file to update {}",
                path.inferred_native_path_string()
            ),
            source: err,
        })
    })?;

    // Patches authored by the model are always normalised to LF: the parser
    // splits them with `str::lines()`, which strips a trailing `\r`. Strip the
    // file's line endings the same way before matching, then re-apply the
    // file's dominant ending on write. Without this, a CRLF file only matched
    // via the trailing-whitespace fallback in `seek_sequence`, and the
    // rewritten region was written back as LF, leaving the file with mixed
    // endings that broke subsequent patches.
    let path_text = path.inferred_native_path_string();
    let new_contents =
        derive_new_contents_from_original_contents(&original_contents, &path_text, chunks)?;
    Ok(AppliedPatch {
        original_contents,
        new_contents,
    })
}

/// Compute a list of replacements needed to transform `original_lines` into the
/// new lines, given the patch `chunks`. Each replacement is returned as
/// `(start_index, old_len, new_lines)`.
fn compute_replacements(
    original_lines: &[String],
    path: &str,
    chunks: &[UpdateFileChunk],
) -> std::result::Result<Vec<(usize, usize, Vec<String>)>, ApplyPatchError> {
    let mut replacements: Vec<(usize, usize, Vec<String>)> = Vec::new();
    let mut line_index: usize = 0;

    for chunk in chunks {
        // If a chunk has a `change_context`, we use seek_sequence to find it, then
        // adjust our `line_index` to continue from there.
        if let Some(ctx_line) = &chunk.change_context {
            if let Some(idx) = seek_sequence::seek_sequence(
                original_lines,
                std::slice::from_ref(ctx_line),
                line_index,
                /*eof*/ false,
            ) {
                line_index = idx + 1;
            } else {
                return Err(ApplyPatchError::ComputeReplacements(
                    missing_context_diagnostic(original_lines, line_index, ctx_line, path),
                ));
            }
        }

        if chunk.old_lines.is_empty() {
            // Pure addition (no old lines). We'll add them at the end or just
            // before the final empty line if one exists.
            let insertion_idx = if original_lines.last().is_some_and(String::is_empty) {
                original_lines.len() - 1
            } else {
                original_lines.len()
            };
            replacements.push((insertion_idx, 0, chunk.new_lines.clone()));
            continue;
        }

        // Otherwise, try to match the existing lines in the file with the old lines
        // from the chunk. If found, schedule that region for replacement.
        // Attempt to locate the `old_lines` verbatim within the file.  In many
        // real‑world diffs the last element of `old_lines` is an *empty* string
        // representing the terminating newline of the region being replaced.
        // This sentinel is not present in `original_lines` because we strip the
        // trailing empty slice emitted by `split('\n')`.  If a direct search
        // fails and the pattern ends with an empty string, retry without that
        // final element so that modifications touching the end‑of‑file can be
        // located reliably.

        let mut pattern: &[String] = &chunk.old_lines;
        let mut found =
            seek_sequence::seek_sequence(original_lines, pattern, line_index, chunk.is_end_of_file);

        let mut new_slice: &[String] = &chunk.new_lines;

        if found.is_none() && pattern.last().is_some_and(String::is_empty) {
            // Retry without the trailing empty line which represents the final
            // newline in the file.
            pattern = &pattern[..pattern.len() - 1];
            if new_slice.last().is_some_and(String::is_empty) {
                new_slice = &new_slice[..new_slice.len() - 1];
            }

            found = seek_sequence::seek_sequence(
                original_lines,
                pattern,
                line_index,
                chunk.is_end_of_file,
            );
        }

        if let Some(start_idx) = found {
            replacements.push((start_idx, pattern.len(), new_slice.to_vec()));
            line_index = start_idx + pattern.len();
        } else {
            return Err(ApplyPatchError::ComputeReplacements(
                missing_expected_lines_diagnostic(
                    original_lines,
                    line_index,
                    &chunk.old_lines,
                    path,
                ),
            ));
        }
    }

    replacements.sort_by_key(|(index, _, _)| *index);

    Ok(replacements)
}

/// Apply the `(start_index, old_len, new_lines)` replacements to `original_lines`,
/// returning the modified file contents as a vector of lines.
fn apply_replacements(
    mut lines: Vec<String>,
    replacements: &[(usize, usize, Vec<String>)],
) -> Vec<String> {
    // We must apply replacements in descending order so that earlier replacements
    // don't shift the positions of later ones.
    for (start_idx, old_len, new_segment) in replacements.iter().rev() {
        let start_idx = *start_idx;
        let old_len = *old_len;

        // Remove old lines.
        for _ in 0..old_len {
            if start_idx < lines.len() {
                lines.remove(start_idx);
            }
        }

        // Insert new lines.
        for (offset, new_line) in new_segment.iter().enumerate() {
            lines.insert(start_idx + offset, new_line.clone());
        }
    }

    lines
}

/// Intended result of a file update for apply_patch.
#[derive(Debug, Eq, PartialEq)]
pub struct ApplyPatchFileUpdate {
    unified_diff: String,
    original_content: String,
    content: String,
}

pub async fn unified_diff_from_chunks(
    path: &PathUri,
    chunks: &[UpdateFileChunk],
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> std::result::Result<ApplyPatchFileUpdate, ApplyPatchError> {
    unified_diff_from_chunks_with_context(path, chunks, /*context*/ 1, fs, sandbox).await
}

pub async fn unified_diff_from_chunks_with_context(
    path: &PathUri,
    chunks: &[UpdateFileChunk],
    context: usize,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> std::result::Result<ApplyPatchFileUpdate, ApplyPatchError> {
    let AppliedPatch {
        original_contents,
        new_contents,
    } = derive_new_contents_from_chunks(path, chunks, fs, sandbox).await?;
    Ok(ApplyPatchFileUpdate {
        unified_diff: build_unified_diff(&original_contents, &new_contents, context),
        original_content: original_contents,
        content: new_contents,
    })
}

fn missing_context_diagnostic(
    original_lines: &[String],
    search_start: usize,
    expected_line: &str,
    path: &str,
) -> String {
    let mut message = format!("Failed to find context '{expected_line}' in {path}");
    let expected_line = expected_line.to_string();
    append_refresh_hint_and_live_context(
        &mut message,
        original_lines,
        search_start,
        std::slice::from_ref(&expected_line),
    );
    message
}

fn missing_expected_lines_diagnostic(
    original_lines: &[String],
    search_start: usize,
    expected_lines: &[String],
    path: &str,
) -> String {
    let mut message = format!(
        "Failed to find expected lines in {path}:\n{}",
        expected_lines.join("\n")
    );
    append_refresh_hint_and_live_context(
        &mut message,
        original_lines,
        search_start,
        expected_lines,
    );
    message
}

fn append_refresh_hint_and_live_context(
    message: &mut String,
    original_lines: &[String],
    search_start: usize,
    expected_lines: &[String],
) {
    message.push('\n');
    message.push_str(STALE_PATCH_REFRESH_HINT);

    if original_lines.is_empty() {
        return;
    }

    let snippet = if let Some(best_start) =
        find_best_matching_window_start(original_lines, search_start, expected_lines)
    {
        let window_len = expected_lines.len().max(1).min(original_lines.len());
        let snippet_start = best_start.saturating_sub(1);
        let snippet_end = (best_start + window_len + 1).min(original_lines.len());
        format!(
            "\nClosest live block starts at line {}:\n{}",
            best_start + 1,
            render_numbered_lines(original_lines, snippet_start, snippet_end)
        )
    } else {
        let snippet_start = search_start.min(original_lines.len().saturating_sub(1));
        let snippet_end = (snippet_start + 3).min(original_lines.len());
        format!(
            "\nNearby live lines from line {}:\n{}",
            snippet_start + 1,
            render_numbered_lines(original_lines, snippet_start, snippet_end)
        )
    };
    message.push_str(&snippet);
}

fn find_best_matching_window_start(
    original_lines: &[String],
    search_start: usize,
    expected_lines: &[String],
) -> Option<usize> {
    if original_lines.is_empty() {
        return None;
    }

    let window_len = expected_lines.len().max(1).min(original_lines.len());
    let last_start = original_lines.len().saturating_sub(window_len);
    let search_start = search_start.min(last_start);
    let mut best: Option<(usize, usize)> = None;
    for start in search_start..=last_start {
        let score = block_match_score(
            &expected_lines[..expected_lines.len().min(window_len)],
            &original_lines[start..start + window_len],
        );
        match best {
            Some((best_score, _)) if score <= best_score => {}
            _ => best = Some((score, start)),
        }
    }

    best.and_then(|(score, start)| (score > 0).then_some(start))
}

fn block_match_score(expected_lines: &[String], actual_lines: &[String]) -> usize {
    expected_lines
        .iter()
        .zip(actual_lines.iter())
        .map(|(expected_line, actual_line)| {
            approximate_line_match_score(expected_line, actual_line)
        })
        .sum()
}

fn approximate_line_match_score(expected_line: &str, actual_line: &str) -> usize {
    let expected = seek_sequence::normalize_for_fuzzy_match(expected_line);
    let actual = seek_sequence::normalize_for_fuzzy_match(actual_line);
    let shared_prefix = expected
        .chars()
        .zip(actual.chars())
        .take_while(|(lhs, rhs)| lhs == rhs)
        .count();
    let shared_suffix = expected
        .chars()
        .rev()
        .zip(actual.chars().rev())
        .take_while(|(lhs, rhs)| lhs == rhs)
        .count();
    let shared_tokens = expected
        .split_whitespace()
        .filter(|token| !token.is_empty() && actual.contains(token))
        .count()
        * 8;
    shared_prefix + shared_suffix + shared_tokens
}

fn render_numbered_lines(original_lines: &[String], start: usize, end: usize) -> String {
    let mut rendered = String::new();
    let capped_end = end.min(start.saturating_add(STALE_PATCH_LIVE_CONTEXT_MAX_LINES));
    for (idx, line) in original_lines
        .iter()
        .enumerate()
        .take(capped_end)
        .skip(start)
    {
        if !rendered.is_empty() {
            rendered.push('\n');
        }
        let line_prefix =
            take_bytes_at_char_boundary(line, STALE_PATCH_LIVE_CONTEXT_MAX_LINE_BYTES);
        rendered.push_str(&format!("{}| {line_prefix}", idx + 1));
        if line_prefix.len() < line.len() {
            rendered.push_str("… [line truncated]");
        }
    }
    let omitted_lines = end.saturating_sub(capped_end);
    if omitted_lines > 0 {
        if !rendered.is_empty() {
            rendered.push('\n');
        }
        rendered.push_str(&format!("… [{omitted_lines} live lines omitted]"));
    }
    rendered
}

/// Print the summary of changes in git-style format.
/// Write a summary of changes to the given writer.
pub fn print_summary(
    affected: &AffectedPaths,
    out: &mut impl std::io::Write,
) -> std::io::Result<()> {
    writeln!(out, "Success. Updated the following files:")?;
    for path in &affected.added {
        writeln!(out, "A {}", path.display())?;
    }
    for path in &affected.modified {
        writeln!(out, "M {}", path.display())?;
    }
    for path in &affected.deleted {
        writeln!(out, "D {}", path.display())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_exec_server::LOCAL_FS;
    use pretty_assertions::assert_eq;
    use std::fs;
    use std::string::ToString;
    use tempfile::tempdir;

    /// Helper to construct a patch with the given body.
    fn wrap_patch(body: &str) -> String {
        format!("*** Begin Patch\n{body}\n*** End Patch")
    }

    #[tokio::test]
    async fn test_add_file_hunk_creates_file_with_contents() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("add.txt");
        let patch = wrap_patch(&format!(
            r#"*** Add File: {}
+ab
+cd"#,
            path.display()
        ));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).expect("absolute test path"),
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();
        // Verify expected stdout and stderr outputs.
        let stdout_str = String::from_utf8(stdout).unwrap();
        let stderr_str = String::from_utf8(stderr).unwrap();
        let expected_out = format!(
            "Success. Updated the following files:\nA {}\n",
            path.display()
        );
        assert_eq!(stdout_str, expected_out);
        assert_eq!(stderr_str, "");
        let contents = fs::read_to_string(path).unwrap();
        assert_eq!(contents, "ab\ncd\n");
    }

    #[tokio::test]
    async fn test_apply_patch_hunks_accept_relative_and_absolute_paths() {
        let dir = tempdir().unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).expect("absolute test path");
        let relative_add = dir.path().join("relative-add.txt");
        let absolute_add = dir.path().join("absolute-add.txt");
        let relative_delete = dir.path().join("relative-delete.txt");
        let absolute_delete = dir.path().join("absolute-delete.txt");
        let relative_update = dir.path().join("relative-update.txt");
        let absolute_update = dir.path().join("absolute-update.txt");
        fs::write(&relative_delete, "delete relative\n").unwrap();
        fs::write(&absolute_delete, "delete absolute\n").unwrap();
        fs::write(&relative_update, "relative old\n").unwrap();
        fs::write(&absolute_update, "absolute old\n").unwrap();

        let patch = wrap_patch(&format!(
            r#"*** Add File: relative-add.txt
+relative add
*** Add File: {}
+absolute add
*** Delete File: relative-delete.txt
*** Delete File: {}
*** Update File: relative-update.txt
@@
-relative old
+relative new
*** Update File: {}
@@
-absolute old
+absolute new"#,
            absolute_add.display(),
            absolute_delete.display(),
            absolute_update.display(),
        ));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        apply_patch(
            &patch,
            &cwd,
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();

        assert_eq!(fs::read_to_string(&relative_add).unwrap(), "relative add\n");
        assert_eq!(fs::read_to_string(&absolute_add).unwrap(), "absolute add\n");
        assert!(!relative_delete.exists());
        assert!(!absolute_delete.exists());
        assert_eq!(
            fs::read_to_string(&relative_update).unwrap(),
            "relative new\n"
        );
        assert_eq!(
            fs::read_to_string(&absolute_update).unwrap(),
            "absolute new\n"
        );
        assert_eq!(String::from_utf8(stderr).unwrap(), "");
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            format!(
                "Success. Updated the following files:\nA relative-add.txt\nA {}\nM relative-update.txt\nM {}\nD relative-delete.txt\nD {}\n",
                absolute_add.display(),
                absolute_update.display(),
                absolute_delete.display(),
            )
        );
    }

    #[tokio::test]
    async fn test_delete_file_hunk_removes_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("del.txt");
        fs::write(&path, "x").unwrap();
        let patch = wrap_patch(&format!("*** Delete File: {}", path.display()));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).expect("absolute test path"),
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();
        let stdout_str = String::from_utf8(stdout).unwrap();
        let stderr_str = String::from_utf8(stderr).unwrap();
        let expected_out = format!(
            "Success. Updated the following files:\nD {}\n",
            path.display()
        );
        assert_eq!(stdout_str, expected_out);
        assert_eq!(stderr_str, "");
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn test_update_file_hunk_modifies_content() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("update.txt");
        fs::write(&path, "foo\nbar\n").unwrap();
        let patch = wrap_patch(&format!(
            r#"*** Update File: {}
@@
 foo
-bar
+baz"#,
            path.display()
        ));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).expect("absolute test path"),
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();
        // Validate modified file contents and expected stdout/stderr.
        let stdout_str = String::from_utf8(stdout).unwrap();
        let stderr_str = String::from_utf8(stderr).unwrap();
        let expected_out = format!(
            "Success. Updated the following files:\nM {}\n",
            path.display()
        );
        assert_eq!(stdout_str, expected_out);
        assert_eq!(stderr_str, "");
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "foo\nbaz\n");
    }

    #[tokio::test]
    async fn test_update_file_hunk_can_move_file() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dest = dir.path().join("dst.txt");
        fs::write(&src, "line\n").unwrap();
        let patch = wrap_patch(&format!(
            r#"*** Update File: {}
*** Move to: {}
@@
-line
+line2"#,
            src.display(),
            dest.display()
        ));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).expect("absolute test path"),
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();
        // Validate move semantics and expected stdout/stderr.
        let stdout_str = String::from_utf8(stdout).unwrap();
        let stderr_str = String::from_utf8(stderr).unwrap();
        let expected_out = format!(
            "Success. Updated the following files:\nM {}\n",
            dest.display()
        );
        assert_eq!(stdout_str, expected_out);
        assert_eq!(stderr_str, "");
        assert!(!src.exists());
        let contents = fs::read_to_string(&dest).unwrap();
        assert_eq!(contents, "line2\n");
    }

    #[tokio::test]
    async fn test_update_file_hunk_move_to_same_path_keeps_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("same.txt");
        fs::write(&path, "before\n").unwrap();
        let patch =
            wrap_patch("*** Update File: same.txt\n*** Move to: same.txt\n@@\n-before\n+after");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let delta = apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).expect("absolute test path"),
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "after\n");
        assert_eq!(
            delta,
            AppliedPatchDelta::new(
                vec![AppliedPatchChange {
                    path,
                    change: AppliedPatchFileChange::Update {
                        move_path: None,
                        old_content: "before\n".to_string(),
                        overwritten_move_content: None,
                        new_content: "after\n".to_string(),
                    },
                }],
                /*exact*/ true,
            )
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_failed_move_rolls_back_destination() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let source_dir = dir.path().join("locked");
        let dest_dir = dir.path().join("out");
        fs::create_dir(&source_dir).unwrap();
        fs::create_dir(&dest_dir).unwrap();
        let src = source_dir.join("src.txt");
        let dest = dest_dir.join("dst.txt");
        fs::write(&src, "line\n").unwrap();
        fs::set_permissions(&source_dir, fs::Permissions::from_mode(0o555)).unwrap();

        let patch = wrap_patch(
            "*** Update File: locked/src.txt\n*** Move to: out/dst.txt\n@@\n-line\n+line2",
        );
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let failure = apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).expect("absolute test path"),
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .expect_err("source removal should fail after destination write");

        fs::set_permissions(&source_dir, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            String::from_utf8(stderr)
                .unwrap()
                .contains(&format!("Failed to delete file {}", src.display()))
        );
        assert_eq!(failure.delta(), &AppliedPatchDelta::empty());
        assert_eq!(fs::read_to_string(src).unwrap(), "line\n");
        assert!(!dest.exists());
    }

    /// Verify that a single `Update File` hunk with multiple change chunks can update different
    /// parts of a file and that the file is listed only once in the summary.
    #[tokio::test]
    async fn test_multiple_update_chunks_apply_to_single_file() {
        // Start with a file containing four lines.
        let dir = tempdir().unwrap();
        let path = dir.path().join("multi.txt");
        fs::write(&path, "foo\nbar\nbaz\nqux\n").unwrap();
        // Construct an update patch with two separate change chunks.
        // The first chunk uses the line `foo` as context and transforms `bar` into `BAR`.
        // The second chunk uses `baz` as context and transforms `qux` into `QUX`.
        let patch = wrap_patch(&format!(
            r#"*** Update File: {}
@@
 foo
-bar
+BAR
@@
 baz
-qux
+QUX"#,
            path.display()
        ));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).expect("absolute test path"),
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();
        let stdout_str = String::from_utf8(stdout).unwrap();
        let stderr_str = String::from_utf8(stderr).unwrap();
        let expected_out = format!(
            "Success. Updated the following files:\nM {}\n",
            path.display()
        );
        assert_eq!(stdout_str, expected_out);
        assert_eq!(stderr_str, "");
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "foo\nBAR\nbaz\nQUX\n");
    }

    /// A more involved `Update File` hunk that exercises additions, deletions and
    /// replacements in separate chunks that appear in non‑adjacent parts of the
    /// file.  Verifies that all edits are applied and that the summary lists the
    /// file only once.
    #[tokio::test]
    async fn test_update_file_hunk_interleaved_changes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("interleaved.txt");

        // Original file: six numbered lines.
        fs::write(&path, "a\nb\nc\nd\ne\nf\n").unwrap();

        // Patch performs:
        //  • Replace `b` → `B`
        //  • Replace `e` → `E` (using surrounding context)
        //  • Append new line `g` at the end‑of‑file
        let patch = wrap_patch(&format!(
            r#"*** Update File: {}
@@
 a
-b
+B
@@
 c
 d
-e
+E
@@
 f
+g
*** End of File"#,
            path.display()
        ));

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).expect("absolute test path"),
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();

        let stdout_str = String::from_utf8(stdout).unwrap();
        let stderr_str = String::from_utf8(stderr).unwrap();

        let expected_out = format!(
            "Success. Updated the following files:\nM {}\n",
            path.display()
        );
        assert_eq!(stdout_str, expected_out);
        assert_eq!(stderr_str, "");

        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "a\nB\nc\nd\nE\nf\ng\n");
    }

    #[tokio::test]
    async fn test_pure_addition_chunk_followed_by_removal() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("panic.txt");
        fs::write(&path, "line1\nline2\nline3\n").unwrap();
        let patch = wrap_patch(&format!(
            r#"*** Update File: {}
@@
+after-context
+second-line
@@
 line1
-line2
-line3
+line2-replacement"#,
            path.display()
        ));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).expect("absolute test path"),
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();
        let contents = fs::read_to_string(path).unwrap();
        assert_eq!(
            contents,
            "line1\nline2-replacement\nafter-context\nsecond-line\n"
        );
    }

    /// Ensure that patches authored with ASCII characters can update lines that
    /// contain typographic Unicode punctuation (e.g. EN DASH, NON-BREAKING
    /// HYPHEN). Historically `git apply` succeeds in such scenarios but our
    /// internal matcher failed requiring an exact byte-for-byte match.  The
    /// fuzzy-matching pass that normalises common punctuation should now bridge
    /// the gap.
    #[tokio::test]
    async fn test_update_line_with_unicode_dash() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("unicode.py");

        // Original line contains EN DASH (\u{2013}) and NON-BREAKING HYPHEN (\u{2011}).
        let original = "import asyncio  # local import \u{2013} avoids top\u{2011}level dep\n";
        std::fs::write(&path, original).unwrap();

        // Patch uses plain ASCII dash / hyphen.
        let patch = wrap_patch(&format!(
            r#"*** Update File: {}
@@
-import asyncio  # local import - avoids top-level dep
+import asyncio  # HELLO"#,
            path.display()
        ));

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).expect("absolute test path"),
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();

        // File should now contain the replaced comment.
        let expected = "import asyncio  # HELLO\n";
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, expected);

        // Ensure success summary lists the file as modified.
        let stdout_str = String::from_utf8(stdout).unwrap();
        let expected_out = format!(
            "Success. Updated the following files:\nM {}\n",
            path.display()
        );
        assert_eq!(stdout_str, expected_out);

        // No stderr expected.
        assert_eq!(String::from_utf8(stderr).unwrap(), "");
    }

    #[test]
    fn test_detect_line_ending() {
        assert_eq!(detect_line_ending("foo\r\nbar\r\n"), "\r\n");
        assert_eq!(detect_line_ending("foo\nbar\n"), "\n");
        // Ties and LF-majority fall back to LF.
        assert_eq!(detect_line_ending("foo\r\nbar\n"), "\n");
        // No newline at all.
        assert_eq!(detect_line_ending("foo"), "\n");
        assert_eq!(detect_line_ending(""), "\n");
    }

    /// A CRLF file should match cleanly and be written back with CRLF endings,
    /// touching only the replaced line rather than normalising the whole file
    /// to LF.
    #[tokio::test]
    async fn test_update_preserves_crlf_line_endings() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("crlf.txt");
        fs::write(&path, "foo\r\nbar\r\nbaz\r\n").unwrap();
        let patch = wrap_patch(&format!(
            "*** Update File: {}\n@@\n foo\n-bar\n+BAR",
            path.display()
        ));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).expect("absolute test path"),
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "foo\r\nBAR\r\nbaz\r\n");
        assert_eq!(String::from_utf8(stderr).unwrap(), "");
    }

    /// Inserting at end-of-file on a CRLF file should keep CRLF endings,
    /// including for the newly appended line.
    #[tokio::test]
    async fn test_eof_insertion_preserves_crlf_line_endings() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("crlf_eof.txt");
        fs::write(&path, "foo\r\nbar\r\nbaz\r\n").unwrap();
        let patch = wrap_patch(&format!(
            "*** Update File: {}\n@@\n+quux\n*** End of File",
            path.display()
        ));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).expect("absolute test path"),
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "foo\r\nbar\r\nbaz\r\nquux\r\n");
        assert_eq!(String::from_utf8(stderr).unwrap(), "");
    }

    #[tokio::test]
    async fn test_unified_diff() {
        // Start with a file containing four lines.
        let dir = tempdir().unwrap();
        let path = dir.path().join("multi.txt");
        fs::write(&path, "foo\nbar\nbaz\nqux\n").unwrap();
        let patch = wrap_patch(&format!(
            r#"*** Update File: {}
@@
 foo
-bar
+BAR
@@
 baz
-qux
+QUX"#,
            path.display()
        ));
        let patch = parse_patch(&patch).unwrap();

        let update_file_chunks = match patch.hunks.as_slice() {
            [Hunk::UpdateFile { chunks, .. }] => chunks,
            _ => panic!("Expected a single UpdateFile hunk"),
        };
        let path_uri = PathUri::from_host_native_path(&path).expect("absolute test path");
        let diff = unified_diff_from_chunks(
            &path_uri,
            update_file_chunks,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();
        let expected_diff = r#"@@ -1,4 +1,4 @@
 foo
-bar
+BAR
 baz
-qux
+QUX
"#;
        let expected = ApplyPatchFileUpdate {
            unified_diff: expected_diff.to_string(),
            original_content: "foo\nbar\nbaz\nqux\n".to_string(),
            content: "foo\nBAR\nbaz\nQUX\n".to_string(),
        };
        assert_eq!(expected, diff);
    }

    #[tokio::test]
    async fn test_unified_diff_first_line_replacement() {
        // Replace the very first line of the file.
        let dir = tempdir().unwrap();
        let path = dir.path().join("first.txt");
        fs::write(&path, "foo\nbar\nbaz\n").unwrap();

        let patch = wrap_patch(&format!(
            r#"*** Update File: {}
@@
-foo
+FOO
 bar
"#,
            path.display()
        ));

        let patch = parse_patch(&patch).unwrap();
        let chunks = match patch.hunks.as_slice() {
            [Hunk::UpdateFile { chunks, .. }] => chunks,
            _ => panic!("Expected a single UpdateFile hunk"),
        };

        let resolved_path = PathUri::from_host_native_path(&path).expect("absolute test path");
        let diff = unified_diff_from_chunks(
            &resolved_path,
            chunks,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();
        let expected_diff = r#"@@ -1,2 +1,2 @@
-foo
+FOO
 bar
"#;
        let expected = ApplyPatchFileUpdate {
            unified_diff: expected_diff.to_string(),
            original_content: "foo\nbar\nbaz\n".to_string(),
            content: "FOO\nbar\nbaz\n".to_string(),
        };
        assert_eq!(expected, diff);
    }

    #[tokio::test]
    async fn test_unified_diff_last_line_replacement() {
        // Replace the very last line of the file.
        let dir = tempdir().unwrap();
        let path = dir.path().join("last.txt");
        fs::write(&path, "foo\nbar\nbaz\n").unwrap();

        let patch = wrap_patch(&format!(
            r#"*** Update File: {}
@@
 foo
 bar
-baz
+BAZ
"#,
            path.display()
        ));

        let patch = parse_patch(&patch).unwrap();
        let chunks = match patch.hunks.as_slice() {
            [Hunk::UpdateFile { chunks, .. }] => chunks,
            _ => panic!("Expected a single UpdateFile hunk"),
        };

        let resolved_path = PathUri::from_host_native_path(&path).expect("absolute test path");
        let diff = unified_diff_from_chunks(
            &resolved_path,
            chunks,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();
        let expected_diff = r#"@@ -2,2 +2,2 @@
 bar
-baz
+BAZ
"#;
        let expected = ApplyPatchFileUpdate {
            unified_diff: expected_diff.to_string(),
            original_content: "foo\nbar\nbaz\n".to_string(),
            content: "foo\nbar\nBAZ\n".to_string(),
        };
        assert_eq!(expected, diff);
    }

    #[tokio::test]
    async fn test_unified_diff_insert_at_eof() {
        // Insert a new line at end‑of‑file.
        let dir = tempdir().unwrap();
        let path = dir.path().join("insert.txt");
        fs::write(&path, "foo\nbar\nbaz\n").unwrap();

        let patch = wrap_patch(&format!(
            r#"*** Update File: {}
@@
+quux
*** End of File
"#,
            path.display()
        ));

        let patch = parse_patch(&patch).unwrap();
        let chunks = match patch.hunks.as_slice() {
            [Hunk::UpdateFile { chunks, .. }] => chunks,
            _ => panic!("Expected a single UpdateFile hunk"),
        };

        let path_uri = PathUri::from_host_native_path(&path).expect("absolute test path");
        let diff =
            unified_diff_from_chunks(&path_uri, chunks, LOCAL_FS.as_ref(), /*sandbox*/ None)
                .await
                .unwrap();
        let expected_diff = r#"@@ -3 +3,2 @@
 baz
+quux
"#;
        let expected = ApplyPatchFileUpdate {
            unified_diff: expected_diff.to_string(),
            original_content: "foo\nbar\nbaz\n".to_string(),
            content: "foo\nbar\nbaz\nquux\n".to_string(),
        };
        assert_eq!(expected, diff);
    }

    #[tokio::test]
    async fn test_unified_diff_interleaved_changes() {
        // Original file with six lines.
        let dir = tempdir().unwrap();
        let path = dir.path().join("interleaved.txt");
        fs::write(&path, "a\nb\nc\nd\ne\nf\n").unwrap();

        // Patch replaces two separate lines and appends a new one at EOF using
        // three distinct chunks.
        let patch_body = format!(
            r#"*** Update File: {}
@@
 a
-b
+B
@@
 d
-e
+E
@@
 f
+g
*** End of File"#,
            path.display()
        );
        let patch = wrap_patch(&patch_body);

        // Extract chunks then build the unified diff.
        let parsed = parse_patch(&patch).unwrap();
        let chunks = match parsed.hunks.as_slice() {
            [Hunk::UpdateFile { chunks, .. }] => chunks,
            _ => panic!("Expected a single UpdateFile hunk"),
        };

        let path_uri = PathUri::from_host_native_path(&path).expect("absolute test path");
        let diff =
            unified_diff_from_chunks(&path_uri, chunks, LOCAL_FS.as_ref(), /*sandbox*/ None)
                .await
                .unwrap();

        let expected_diff = r#"@@ -1,6 +1,7 @@
 a
-b
+B
 c
 d
-e
+E
 f
+g
"#;

        let expected = ApplyPatchFileUpdate {
            unified_diff: expected_diff.to_string(),
            original_content: "a\nb\nc\nd\ne\nf\n".to_string(),
            content: "a\nB\nc\nd\nE\nf\ng\n".to_string(),
        };

        assert_eq!(expected, diff);

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).expect("absolute test path"),
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();
        let contents = fs::read_to_string(path).unwrap();
        assert_eq!(
            contents,
            r#"a
B
c
d
E
f
g
"#
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_apply_patch_fails_on_write_error() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let locked_dir = dir.path().join("locked");
        fs::create_dir(&locked_dir).unwrap();
        fs::set_permissions(&locked_dir, fs::Permissions::from_mode(0o555)).unwrap();

        let patch = wrap_patch("*** Add File: locked/new.txt\n+after");

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let result = apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).expect("absolute test path"),
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await;
        let failure = result.expect_err("write should fail");

        fs::set_permissions(&locked_dir, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(failure.delta().is_exact());
        assert!(failure.delta().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_apply_patch_rolls_back_earlier_writes_when_later_write_fails() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let updated_path = dir.path().join("updated.txt");
        fs::write(&updated_path, "before\n").unwrap();
        let locked_dir = dir.path().join("locked");
        fs::create_dir(&locked_dir).unwrap();
        fs::set_permissions(&locked_dir, fs::Permissions::from_mode(0o555)).unwrap();

        let patch = wrap_patch(
            "*** Update File: updated.txt\n@@\n-before\n+after\n*** Add File: locked/new.txt\n+blocked",
        );

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let result = apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).expect("absolute test path"),
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await;
        let failure = result.expect_err("write should fail");

        fs::set_permissions(&locked_dir, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(fs::read_to_string(&updated_path).unwrap(), "before\n");
        assert!(!dir.path().join("locked/new.txt").exists());
        assert!(failure.delta().is_exact());
        assert!(failure.delta().is_empty());
    }

    #[tokio::test]
    async fn test_apply_patch_rolls_back_earlier_writes_when_failed_target_is_unreadable() {
        let dir = tempdir().unwrap();
        let updated_path = dir.path().join("updated.txt");
        fs::write(&updated_path, "before\n").unwrap();
        fs::create_dir(dir.path().join("existing-dir")).unwrap();
        let patch = wrap_patch(
            "*** Update File: updated.txt\n@@\n-before\n+after\n*** Add File: existing-dir\n+blocked",
        );
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let failure = apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).expect("absolute test path"),
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .expect_err("writing over a directory should fail");

        assert_eq!(fs::read_to_string(updated_path).unwrap(), "before\n");
        assert!(dir.path().join("existing-dir").is_dir());
        assert!(failure.delta().is_empty());
        assert!(!failure.delta().is_exact());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_apply_patch_rolls_back_binary_overwrite_when_later_write_fails() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let binary_path = dir.path().join("binary.dat");
        let original_bytes = vec![0xff, 0xfe, 0xfd];
        fs::write(&binary_path, &original_bytes).unwrap();
        let locked_dir = dir.path().join("locked");
        fs::create_dir(&locked_dir).unwrap();
        fs::set_permissions(&locked_dir, fs::Permissions::from_mode(0o555)).unwrap();

        let patch =
            wrap_patch("*** Add File: binary.dat\n+text\n*** Add File: locked/new.txt\n+blocked");

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let result = apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).expect("absolute test path"),
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await;
        let failure = result.expect_err("write should fail");

        fs::set_permissions(&locked_dir, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(fs::read(&binary_path).unwrap(), original_bytes);
        assert!(!dir.path().join("locked/new.txt").exists());
        assert!(failure.delta().is_exact());
        assert!(failure.delta().is_empty());
    }

    #[tokio::test]
    async fn test_non_utf8_destinations_return_inexact_delta() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("binary.dat");
        fs::write(dir.path().join("source.txt"), "before\n").unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).expect("absolute test path");

        for patch in [
            wrap_patch("*** Add File: binary.dat\n+text"),
            wrap_patch("*** Update File: source.txt\n*** Move to: binary.dat\n@@\n-before\n+after"),
        ] {
            fs::write(&path, [0xff, 0xfe, 0xfd]).unwrap();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let delta = apply_patch(
                &patch,
                &cwd,
                &mut stdout,
                &mut stderr,
                LOCAL_FS.as_ref(),
                /*sandbox*/ None,
            )
            .await
            .unwrap();

            assert!(!delta.is_exact());
        }
    }

    #[test]
    fn stale_patch_live_context_is_bounded() {
        let long_line = "x".repeat(STALE_PATCH_LIVE_CONTEXT_MAX_LINE_BYTES * 2);
        let lines = vec![long_line; STALE_PATCH_LIVE_CONTEXT_MAX_LINES + 4];

        let rendered = render_numbered_lines(&lines, 0, lines.len());

        assert!(rendered.contains("… [line truncated]"));
        assert!(rendered.contains("… [4 live lines omitted]"));
        assert_eq!(
            rendered.lines().count(),
            STALE_PATCH_LIVE_CONTEXT_MAX_LINES + 1
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_delete_symlink_returns_inexact_delta() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        fs::write(dir.path().join("target.txt"), "target\n").unwrap();
        symlink(dir.path().join("target.txt"), dir.path().join("link.txt")).unwrap();
        let patch = wrap_patch("*** Delete File: link.txt");

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let delta = apply_patch(
            &patch,
            &PathUri::from_host_native_path(dir.path()).expect("absolute test path"),
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .unwrap();

        assert!(!delta.is_exact());
    }
}

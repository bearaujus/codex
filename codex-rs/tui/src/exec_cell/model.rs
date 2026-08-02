//! Data model for grouped exec-call history cells in the TUI transcript.
//!
//! An `ExecCell` can represent either a single command or an "exploring" group of related read/
//! list/search commands. The chat widget relies on stable `call_id` matching to route progress and
//! end events into the right cell, and it treats "call id not found" as a real signal (for
//! example, an orphan end that should render as a separate history entry).

use std::borrow::Cow;
use std::time::Duration;
use std::time::Instant;

use super::live_output::LiveCommandOutput;
use codex_ansi_escape::ansi_escape_line;
use codex_app_server_protocol::CommandExecutionSource as ExecCommandSource;
use codex_protocol::parse_command::ParsedCommand;
use itertools::Either;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum CommandPresentation {
    #[default]
    Command,
    Exploration,
    Inspection {
        target: String,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct OutputStats {
    characters: usize,
    newline_count: usize,
    has_content: bool,
    ends_with_newline: bool,
}

impl OutputStats {
    fn from_text(text: &str) -> Self {
        let mut stats = Self::default();
        stats.append(text);
        stats
    }

    fn append(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.characters += text.chars().count();
        self.newline_count += text.bytes().filter(|byte| *byte == b'\n').count();
        self.has_content = true;
        self.ends_with_newline = text.ends_with('\n');
    }

    pub(crate) fn characters(self) -> usize {
        self.characters
    }

    pub(crate) fn lines(self) -> usize {
        if !self.has_content {
            return 0;
        }
        self.newline_count + usize::from(!self.ends_with_newline)
    }
}

#[derive(Debug, Default)]
pub(crate) struct CommandOutput {
    pub(crate) exit_code: i32,
    /// The finalized, interleaved stderr and stdout that replaces any streamed preview.
    aggregated_output: String,
    /// The live preview while command-output deltas are still arriving.
    live_output: Option<LiveCommandOutput>,
    stats: OutputStats,
}

impl CommandOutput {
    pub(crate) fn new(exit_code: i32, aggregated_output: String) -> Self {
        let stats = OutputStats::from_text(&aggregated_output);
        Self {
            exit_code,
            aggregated_output,
            live_output: None,
            stats,
        }
    }

    pub(crate) fn stats(&self) -> OutputStats {
        self.stats
    }

    pub(crate) fn has_diagnostic_signal(&self) -> bool {
        self.aggregated_output.split(['\n', '\r']).any(|raw| {
            let visible =
                ansi_escape_line(raw)
                    .spans
                    .into_iter()
                    .fold(String::new(), |mut text, span| {
                        text.push_str(span.content.as_ref());
                        text
                    });
            let line = visible.trim_start().to_ascii_lowercase();
            let candidate = line.trim_start_matches(|ch: char| {
                ch.is_ascii_whitespace() || matches!(ch, '[' | '(' | '{' | '!' | '*')
            });
            let starts_with_keyword = |keyword: &str| {
                candidate.strip_prefix(keyword).is_some_and(|rest| {
                    rest.is_empty()
                        || rest
                            .chars()
                            .next()
                            .is_some_and(|ch| !ch.is_ascii_alphanumeric() && ch != '_')
                })
            };
            starts_with_keyword("warning")
                || starts_with_keyword("warn")
                || starts_with_keyword("error")
                || starts_with_keyword("failed")
                || starts_with_keyword("failure")
                || starts_with_keyword("fatal")
                || starts_with_keyword("panic")
                || starts_with_keyword("deprecated")
                || line.starts_with("npm warn")
                || line.starts_with("(!)")
                || line.starts_with('⚠')
                || candidate.starts_with("command failed")
                || candidate.starts_with("deprecationwarning")
        })
    }

    fn append(&mut self, chunk: &str) {
        self.stats.append(chunk);
        self.live_output
            .get_or_insert_with(LiveCommandOutput::default)
            .push_str(chunk);
    }

    /// Returns the total number of logical lines and the number retained for rendering.
    pub(super) fn line_counts(&self) -> (usize, usize) {
        match self.live_output.as_ref() {
            Some(output) => (output.total_lines(), output.retained_lines()),
            None => {
                let total = self.aggregated_output.lines().count();
                (total, total)
            }
        }
    }

    /// Returns retained preview lines with reverse traversal for efficient tail rendering.
    pub(super) fn lines(&self) -> impl DoubleEndedIterator<Item = Cow<'_, str>> {
        match self.live_output.as_ref() {
            Some(output) => Either::Left(output.lines()),
            None => Either::Right(self.aggregated_output.lines().map(Cow::Borrowed)),
        }
    }

    /// Returns lines for the expanded transcript, including any storage-level omission marker.
    pub(super) fn transcript_lines(&self) -> impl Iterator<Item = Cow<'_, str>> {
        match self.live_output.as_ref() {
            Some(output) => Either::Left(output.transcript_lines()),
            None => Either::Right(self.aggregated_output.lines().map(Cow::Borrowed)),
        }
    }
}

#[derive(Debug)]
pub(crate) struct ExecCall {
    pub(crate) call_id: String,
    pub(crate) command: Vec<String>,
    pub(crate) parsed: Vec<ParsedCommand>,
    pub(crate) presentation: CommandPresentation,
    pub(crate) output: Option<CommandOutput>,
    pub(crate) source: ExecCommandSource,
    pub(crate) start_time: Option<Instant>,
    pub(crate) duration: Option<Duration>,
    pub(crate) interaction_input: Option<String>,
}

#[derive(Debug)]
pub(crate) struct ExecCell {
    pub(crate) calls: Vec<ExecCall>,
    animations_enabled: bool,
    force_standalone: bool,
}

impl ExecCell {
    pub(crate) fn new(call: ExecCall, animations_enabled: bool) -> Self {
        Self {
            calls: vec![call],
            animations_enabled,
            force_standalone: false,
        }
    }

    pub(crate) fn new_standalone(call: ExecCall, animations_enabled: bool) -> Self {
        Self {
            calls: vec![call],
            animations_enabled,
            force_standalone: true,
        }
    }

    pub(crate) fn add_call(
        &mut self,
        call_id: String,
        command: Vec<String>,
        parsed: Vec<ParsedCommand>,
        presentation: CommandPresentation,
        source: ExecCommandSource,
        interaction_input: Option<String>,
    ) -> bool {
        let call = ExecCall {
            call_id,
            command,
            parsed,
            presentation,
            output: None,
            source,
            start_time: Some(Instant::now()),
            duration: None,
            interaction_input,
        };
        let can_join_exploration = self.is_exploring_cell() && Self::is_exploring_call(&call);
        if can_join_exploration || self.is_active() {
            self.calls.push(call);
            true
        } else {
            false
        }
    }

    /// Marks the most recently matching call as finished and returns whether a call was found.
    ///
    /// Callers should treat `false` as a routing mismatch rather than silently ignoring it. The
    /// chat widget uses that signal to avoid attaching an orphan `exec_end` event to an unrelated
    /// active exploring cell, which would incorrectly collapse two transcript entries together.
    pub(crate) fn complete_call(
        &mut self,
        call_id: &str,
        output: CommandOutput,
        duration: Duration,
    ) -> bool {
        let Some(call) = self.calls.iter_mut().rev().find(|c| c.call_id == call_id) else {
            return false;
        };
        call.output = Some(output);
        call.duration = Some(duration);
        call.start_time = None;
        true
    }

    pub(crate) fn should_flush(&self) -> bool {
        !self.is_exploring_cell() && self.calls.iter().all(|c| c.duration.is_some())
    }

    pub(crate) fn take_call(&mut self, call_id: &str) -> Option<ExecCall> {
        let index = self
            .calls
            .iter()
            .rposition(|call| call.call_id == call_id)?;
        Some(self.calls.remove(index))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    pub(crate) fn call_is_exploring(&self, call_id: &str) -> bool {
        self.calls
            .iter()
            .rev()
            .find(|call| call.call_id == call_id)
            .is_some_and(Self::is_exploring_call)
    }

    pub(crate) fn call_is_clean_success(&self, call_id: &str) -> bool {
        self.calls
            .iter()
            .rev()
            .find(|call| call.call_id == call_id)
            .and_then(|call| call.output.as_ref())
            .is_some_and(|output| output.exit_code == 0 && !output.has_diagnostic_signal())
    }

    pub(crate) fn mark_failed(&mut self) {
        for call in self.calls.iter_mut() {
            if call.duration.is_none() {
                let elapsed = call
                    .start_time
                    .map(|st| st.elapsed())
                    .unwrap_or_else(|| Duration::from_millis(0));
                call.start_time = None;
                call.duration = Some(elapsed);
                call.output
                    .get_or_insert_with(CommandOutput::default)
                    .exit_code = 1;
            }
        }
    }

    pub(crate) fn is_exploring_cell(&self) -> bool {
        !self.force_standalone
            && !self.calls.is_empty()
            && self.calls.iter().all(Self::is_exploring_call)
    }

    pub(crate) fn is_parallel_cell(&self) -> bool {
        self.calls.len() > 1 && !self.is_exploring_cell()
    }

    pub(crate) fn is_active(&self) -> bool {
        self.calls.iter().any(|c| c.duration.is_none())
    }

    pub(crate) fn active_start_time(&self) -> Option<Instant> {
        self.calls
            .iter()
            .find(|c| c.duration.is_none())
            .and_then(|c| c.start_time)
    }

    pub(crate) fn active_call_count(&self) -> usize {
        self.calls
            .iter()
            .filter(|call| call.duration.is_none())
            .count()
    }

    pub(crate) fn active_calls(&self) -> impl Iterator<Item = &ExecCall> {
        self.calls.iter().filter(|call| call.duration.is_none())
    }

    pub(crate) fn animations_enabled(&self) -> bool {
        self.animations_enabled
    }

    pub(crate) fn iter_calls(&self) -> impl Iterator<Item = &ExecCall> {
        self.calls.iter()
    }

    pub(crate) fn append_output(&mut self, call_id: &str, chunk: &str) -> bool {
        if chunk.is_empty() {
            return false;
        }
        let Some(call) = self.calls.iter_mut().rev().find(|c| c.call_id == call_id) else {
            return false;
        };
        let output = call.output.get_or_insert_with(CommandOutput::default);
        output.append(chunk);
        true
    }

    pub(super) fn is_exploring_call(call: &ExecCall) -> bool {
        !matches!(call.source, ExecCommandSource::UserShell)
            && matches!(call.presentation, CommandPresentation::Exploration)
            && !call.parsed.is_empty()
            && call.parsed.iter().all(|p| {
                matches!(
                    p,
                    ParsedCommand::Read { .. }
                        | ParsedCommand::ListFiles { .. }
                        | ParsedCommand::Search { .. }
                )
            })
    }
}

impl ExecCall {
    pub(crate) fn is_user_shell_command(&self) -> bool {
        matches!(self.source, ExecCommandSource::UserShell)
    }

    pub(crate) fn is_unified_exec_interaction(&self) -> bool {
        matches!(self.source, ExecCommandSource::UnifiedExecInteraction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_stats_track_unicode_and_chunk_boundaries() {
        let mut output = CommandOutput::default();
        output.append("αβ\n");
        output.append("猫");

        assert_eq!(output.stats().characters(), 4);
        assert_eq!(output.stats().lines(), 2);
    }

    #[test]
    fn output_stats_match_returned_lines_with_a_trailing_newline() {
        let output = CommandOutput::new(0, "one\r\ntwo\n".to_string());

        assert_eq!(output.stats().characters(), 9);
        assert_eq!(output.stats().lines(), 2);
    }

    #[test]
    fn empty_completed_output_has_zero_totals() {
        let output = CommandOutput::new(0, String::new());

        assert_eq!(output.stats().characters(), 0);
        assert_eq!(output.stats().lines(), 0);
    }

    #[test]
    fn diagnostic_signal_ignores_ansi_styling() {
        let warning = CommandOutput::new(
            0,
            "\u{1b}[33mwarning: generated artifact is stale\u{1b}[0m".to_string(),
        );
        let clean = CommandOutput::new(0, "generated artifact is current".to_string());

        assert!(warning.has_diagnostic_signal());
        assert!(!clean.has_diagnostic_signal());
    }

    #[test]
    fn diagnostic_signal_handles_carriage_returns_and_bracketed_levels() {
        let warning = CommandOutput::new(0, "progress 90%\r[WARN] partial index".to_string());
        let fatal = CommandOutput::new(0, "{fatal}: corrupt cache".to_string());
        let clean = CommandOutput::new(
            0,
            concat!(
                "warnings: 0\n",
                "errors: 0\n",
                "extension/src/reducer.ts:23: warning: this is matched source text\n",
                "extension/src/reducer.ts-24-error: this is surrounding source text\n",
            )
            .to_string(),
        );

        assert!(warning.has_diagnostic_signal());
        assert!(fatal.has_diagnostic_signal());
        assert!(!clean.has_diagnostic_signal());
    }
}

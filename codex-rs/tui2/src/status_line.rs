use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::ui_consts::FOOTER_INDENT_COLS;
use codex_core::config::types::StatusLineSettings;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use serde::Serialize;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio::time::sleep_until;
use tracing::debug;

const STATUS_LINE_MIN_INTERVAL: Duration = Duration::from_millis(300);
const STATUS_LINE_HOOK_EVENT: &str = "Status";

#[derive(Clone, Serialize)]
pub(crate) struct StatusLineInput {
    pub(crate) hook_event_name: &'static str,
    pub(crate) session_id: Option<String>,
    pub(crate) transcript_path: Option<String>,
    pub(crate) cwd: String,
    pub(crate) workspace: StatusLineWorkspace,
    pub(crate) model: StatusLineModel,
    pub(crate) version: String,
    pub(crate) output_style: StatusLineOutputStyle,
    pub(crate) cost: StatusLineCost,
    pub(crate) context_window: Option<StatusLineContextWindow>,
}

pub(crate) struct StatusLineInputArgs {
    pub(crate) session_id: Option<String>,
    pub(crate) transcript_path: Option<String>,
    pub(crate) cwd: String,
    pub(crate) project_dir: String,
    pub(crate) model_id: String,
    pub(crate) model_display_name: String,
    pub(crate) reasoning_effort: Option<ReasoningEffortConfig>,
    pub(crate) version: String,
    pub(crate) context_window: Option<StatusLineContextWindow>,
}

impl StatusLineInput {
    pub(crate) fn for_codex(args: StatusLineInputArgs) -> Self {
        let StatusLineInputArgs {
            session_id,
            transcript_path,
            cwd,
            project_dir,
            model_id,
            model_display_name,
            reasoning_effort,
            version,
            context_window,
        } = args;
        let display_name_with_effort = match reasoning_effort {
            Some(ReasoningEffortConfig::None) | None => model_display_name.clone(),
            Some(effort) => format!("{model_display_name} {effort}"),
        };
        Self {
            hook_event_name: STATUS_LINE_HOOK_EVENT,
            session_id,
            transcript_path,
            cwd: cwd.clone(),
            workspace: StatusLineWorkspace {
                current_dir: cwd,
                project_dir,
            },
            model: StatusLineModel {
                id: model_id,
                display_name: model_display_name,
                display_name_with_effort,
                reasoning_effort,
            },
            version,
            output_style: StatusLineOutputStyle {
                name: "default".to_string(),
            },
            cost: StatusLineCost::default(),
            context_window,
        }
    }
}

#[derive(Clone, Serialize)]
pub(crate) struct StatusLineWorkspace {
    pub(crate) current_dir: String,
    pub(crate) project_dir: String,
}

#[derive(Clone, Serialize)]
pub(crate) struct StatusLineModel {
    pub(crate) id: String,
    pub(crate) display_name: String,
    pub(crate) display_name_with_effort: String,
    pub(crate) reasoning_effort: Option<ReasoningEffortConfig>,
}

#[derive(Clone, Serialize)]
pub(crate) struct StatusLineOutputStyle {
    pub(crate) name: String,
}

#[derive(Clone, Default, Serialize)]
pub(crate) struct StatusLineCost {
    pub(crate) total_cost_usd: Option<f64>,
    pub(crate) total_duration_ms: Option<u64>,
    pub(crate) total_api_duration_ms: Option<u64>,
    pub(crate) total_lines_added: Option<i64>,
    pub(crate) total_lines_removed: Option<i64>,
}

#[derive(Clone, Serialize)]
pub(crate) struct StatusLineContextWindow {
    pub(crate) context_window_size: Option<i64>,
    pub(crate) current_usage: Option<StatusLineContextUsage>,
    pub(crate) percent_remaining: Option<i64>,
}

#[derive(Clone, Serialize)]
pub(crate) struct StatusLineContextUsage {
    pub(crate) input_tokens: i64,
    pub(crate) cache_creation_input_tokens: i64,
    pub(crate) cache_read_input_tokens: i64,
    pub(crate) output_tokens: i64,
    pub(crate) reasoning_output_tokens: i64,
    pub(crate) total_tokens: i64,
}

pub(crate) struct StatusLineManager {
    update_tx: mpsc::UnboundedSender<StatusLineInput>,
}

impl StatusLineManager {
    pub(crate) fn new(
        settings: StatusLineSettings,
        app_event_tx: AppEventSender,
        cwd: PathBuf,
    ) -> Self {
        let (update_tx, mut update_rx) = mpsc::unbounded_channel();
        let padding = settings.padding.unwrap_or(FOOTER_INDENT_COLS);
        let command = settings.command;

        tokio::spawn(async move {
            let mut last_run = Instant::now()
                .checked_sub(STATUS_LINE_MIN_INTERVAL)
                .unwrap_or_else(Instant::now);
            let mut pending: Option<StatusLineInput> = None;

            loop {
                let next_input = match pending.take() {
                    Some(input) => input,
                    None => match update_rx.recv().await {
                        Some(input) => input,
                        None => break,
                    },
                };
                pending = Some(next_input);

                while let Some(input) = pending.take() {
                    let next_at = last_run + STATUS_LINE_MIN_INTERVAL;
                    let now = Instant::now();
                    if now < next_at {
                        sleep_until(next_at).await;
                    }

                    let output = run_status_line_command(&command, &cwd, &input, padding).await;
                    app_event_tx.send(AppEvent::UpdateStatusLine { line: output });
                    last_run = Instant::now();

                    while let Ok(next) = update_rx.try_recv() {
                        pending = Some(next);
                    }
                }
            }
        });

        Self { update_tx }
    }

    pub(crate) fn update(&self, input: StatusLineInput) {
        let _ = self.update_tx.send(input);
    }
}

async fn run_status_line_command(
    command: &[String],
    cwd: &PathBuf,
    input: &StatusLineInput,
    padding: usize,
) -> Option<String> {
    let payload = serde_json::to_vec(input).ok()?;
    let mut cmd = Command::new(command.first()?);
    if command.len() > 1 {
        cmd.args(&command[1..]);
    }
    let mut child = cmd
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    if let Some(mut stdin) = child.stdin.take() {
        if stdin.write_all(&payload).await.is_err() {
            return None;
        }
        let _ = stdin.write_all(b"\n").await;
    }

    let output = child.wait_with_output().await.ok()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.trim().is_empty() {
            debug!("status line command failed: {}", stderr.trim());
        }
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().next()?;
    if line.trim().is_empty() {
        return None;
    }
    let padding_spaces = " ".repeat(padding);
    Some(format!("{padding_spaces}{line}"))
}

use crate::app_backtrack::BacktrackState;
use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::chatwidget::ChatWidget;
use crate::file_search::FileSearchManager;
use crate::history_cell::HistoryCell;
use crate::markdown::append_markdown;
use crate::pager_overlay::Overlay;
use crate::tui;
use crate::tui::TuiEvent;
use codex_ansi_escape::ansi_escape_line;
use codex_core::ConversationManager;
use codex_core::config::Config;
use codex_core::protocol::TokenUsage;
use codex_login::AuthManager;
use color_eyre::eyre::Result;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::terminal::supports_keyboard_enhancement;
use ratatui::style::Stylize;
use ratatui::text::Line;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use tokio::select;
use tokio::sync::mpsc::unbounded_channel;
// use uuid::Uuid;

pub(crate) struct App {
    pub(crate) server: Arc<ConversationManager>,
    pub(crate) app_event_tx: AppEventSender,
    pub(crate) chat_widget: ChatWidget,

    /// Config is stored here so we can recreate ChatWidgets as needed.
    pub(crate) config: Config,

    pub(crate) file_search: FileSearchManager,

    pub(crate) transcript_lines: Vec<Line<'static>>,

    // Pager overlay state (Transcript or Static like Diff)
    pub(crate) overlay: Option<Overlay>,
    pub(crate) deferred_history_lines: Vec<Line<'static>>,

    pub(crate) enhanced_keys_supported: bool,

    /// Controls the animation thread that sends CommitTick events.
    pub(crate) commit_anim_running: Arc<AtomicBool>,

    // Esc-backtracking state grouped
    pub(crate) backtrack: crate::app_backtrack::BacktrackState,
}

impl App {
    pub async fn run(
        tui: &mut tui::Tui,
        auth_manager: Arc<AuthManager>,
        config: Config,
        initial_prompt: Option<String>,
        initial_images: Vec<PathBuf>,
    ) -> Result<TokenUsage> {
        use tokio_stream::StreamExt;
        let (app_event_tx, mut app_event_rx) = unbounded_channel();
        let app_event_tx = AppEventSender::new(app_event_tx);

        let conversation_manager = Arc::new(ConversationManager::new(auth_manager.clone()));

        let enhanced_keys_supported = supports_keyboard_enhancement().unwrap_or(false);

        let chat_widget = ChatWidget::new(
            config.clone(),
            conversation_manager.clone(),
            tui.frame_requester(),
            app_event_tx.clone(),
            initial_prompt,
            initial_images,
            enhanced_keys_supported,
        );

        let file_search = FileSearchManager::new(config.cwd.clone(), app_event_tx.clone());

        let mut app = Self {
            server: conversation_manager,
            app_event_tx,
            chat_widget,
            config,
            file_search,
            enhanced_keys_supported,
            transcript_lines: Vec::new(),
            overlay: None,
            deferred_history_lines: Vec::new(),
            commit_anim_running: Arc::new(AtomicBool::new(false)),
            backtrack: BacktrackState::default(),
        };

        let tui_events = tui.event_stream();
        tokio::pin!(tui_events);

        tui.frame_requester().schedule_frame();

        while select! {
            Some(event) = app_event_rx.recv() => {
                app.handle_event(tui, event).await?
            }
            Some(event) = tui_events.next() => {
                app.handle_tui_event(tui, event).await?
            }
        } {}
        tui.terminal.clear()?;
        Ok(app.token_usage())
    }

    pub(crate) async fn handle_tui_event(
        &mut self,
        tui: &mut tui::Tui,
        event: TuiEvent,
    ) -> Result<bool> {
        if self.overlay.is_some() {
            let _ = self.handle_backtrack_overlay_event(tui, event).await?;
        } else {
            match event {
                TuiEvent::Key(key_event) => {
                    self.handle_key_event(tui, key_event).await;
                }
                TuiEvent::Paste(pasted) => {
                    // Many terminals convert newlines to \r when pasting (e.g., iTerm2),
                    // but tui-textarea expects \n. Normalize CR to LF.
                    // [tui-textarea]: https://github.com/rhysd/tui-textarea/blob/4d18622eeac13b309e0ff6a55a46ac6706da68cf/src/textarea.rs#L782-L783
                    // [iTerm2]: https://github.com/gnachman/iTerm2/blob/5d0c0d9f68523cbd0494dad5422998964a2ecd8d/sources/iTermPasteHelper.m#L206-L216
                    let pasted = pasted.replace("\r", "\n");
                    self.chat_widget.handle_paste(pasted);
                }
                TuiEvent::Draw => {
                    if self
                        .chat_widget
                        .handle_paste_burst_tick(tui.frame_requester())
                    {
                        return Ok(true);
                    }
                    tui.draw(
                        self.chat_widget.desired_height(tui.terminal.size()?.width),
                        |frame| {
                            frame.render_widget_ref(&self.chat_widget, frame.area());
                            if let Some((x, y)) = self.chat_widget.cursor_pos(frame.area()) {
                                frame.set_cursor_position((x, y));
                            }
                        },
                    )?;
                }
                TuiEvent::AttachImage {
                    path,
                    width,
                    height,
                    format_label,
                } => {
                    self.chat_widget
                        .attach_image(path, width, height, format_label);
                }
            }
        }
        Ok(true)
    }

    async fn handle_event(&mut self, tui: &mut tui::Tui, event: AppEvent) -> Result<bool> {
        match event {
            AppEvent::NewSession => {
                self.chat_widget = ChatWidget::new(
                    self.config.clone(),
                    self.server.clone(),
                    tui.frame_requester(),
                    self.app_event_tx.clone(),
                    None,
                    Vec::new(),
                    self.enhanced_keys_supported,
                );
                tui.frame_requester().schedule_frame();
            }
            AppEvent::UpdateRepoInfo {
                repo_name,
                git_branch,
            } => {
                self.chat_widget.apply_repo_info(repo_name, git_branch);
            }
            AppEvent::ResumeSession(path) => {
                self.config.experimental_resume = Some(path);
                self.chat_widget = ChatWidget::new(
                    self.config.clone(),
                    self.server.clone(),
                    tui.frame_requester(),
                    self.app_event_tx.clone(),
                    None,
                    Vec::new(),
                    self.enhanced_keys_supported,
                );
                tui.frame_requester().schedule_frame();
            }
            AppEvent::InsertHistoryLines(lines) => {
                if let Some(Overlay::Transcript(t)) = &mut self.overlay {
                    t.insert_lines(lines.clone());
                    tui.frame_requester().schedule_frame();
                }
                self.transcript_lines.extend(lines.clone());
                if self.overlay.is_some() {
                    self.deferred_history_lines.extend(lines);
                } else {
                    tui.insert_history_lines(lines);
                }
            }
            AppEvent::InsertHistoryCell(cell) => {
                let cell_transcript = cell.transcript_lines();
                if let Some(Overlay::Transcript(t)) = &mut self.overlay {
                    t.insert_lines(cell_transcript.clone());
                    tui.frame_requester().schedule_frame();
                }
                self.transcript_lines.extend(cell_transcript.clone());
                let display = cell.display_lines();
                if !display.is_empty() {
                    if self.overlay.is_some() {
                        self.deferred_history_lines.extend(display);
                    } else {
                        tui.insert_history_lines(display);
                    }
                }
            }
            AppEvent::StartCommitAnimation => {
                if self
                    .commit_anim_running
                    .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    let tx = self.app_event_tx.clone();
                    let running = self.commit_anim_running.clone();
                    thread::spawn(move || {
                        while running.load(Ordering::Relaxed) {
                            thread::sleep(Duration::from_millis(50));
                            tx.send(AppEvent::CommitTick);
                        }
                    });
                }
            }
            AppEvent::StopCommitAnimation => {
                self.commit_anim_running.store(false, Ordering::Release);
            }
            AppEvent::CommitTick => {
                self.chat_widget.on_commit_tick();
            }
            AppEvent::CodexEvent(event) => {
                self.chat_widget.handle_codex_event(event);
            }
            AppEvent::ConversationHistory(ev) => {
                // If a backtrack is pending, delegate to the existing flow.
                if self.backtrack.pending.is_some() {
                    self.on_conversation_history_for_backtrack(tui, ev).await?;
                } else {
                    // Otherwise, this is likely a resume request. If the conversation id
                    // matches the current session and we were started with a resume path,
                    // render the restored history into the transcript so the user sees it.
                    if self.config.experimental_resume.is_some()
                        && self.chat_widget.session_id() == Some(ev.conversation_id)
                    {
                        self.render_resumed_history(tui, ev);
                        // Avoid re‑rendering if another GetHistory arrives.
                        self.config.experimental_resume = None;
                    }
                }
            }
            AppEvent::ExitRequest => {
                return Ok(false);
            }
            AppEvent::CodexOp(op) => self.chat_widget.submit_op(op),
            AppEvent::DiffResult(text) => {
                // Clear the in-progress state in the bottom pane
                self.chat_widget.on_diff_complete();
                // Enter alternate screen using TUI helper and build pager lines
                let _ = tui.enter_alt_screen();
                let pager_lines: Vec<ratatui::text::Line<'static>> = if text.trim().is_empty() {
                    vec!["No changes detected.".italic().into()]
                } else {
                    text.lines().map(ansi_escape_line).collect()
                };
                self.overlay = Some(Overlay::new_static_with_title(
                    pager_lines,
                    "D I F F".to_string(),
                ));
                tui.frame_requester().schedule_frame();
            }
            AppEvent::StartFileSearch(query) => {
                if !query.is_empty() {
                    self.file_search.on_user_query(query);
                }
            }
            AppEvent::FileSearchResult { query, matches } => {
                self.chat_widget.apply_file_search_result(query, matches);
            }
            AppEvent::UpdateReasoningEffort(effort) => {
                self.chat_widget.set_reasoning_effort(effort);
            }
            AppEvent::UpdateModel(model) => {
                self.chat_widget.set_model(model);
            }
            AppEvent::UpdateAskForApprovalPolicy(policy) => {
                self.chat_widget.set_approval_policy(policy);
            }
            AppEvent::UpdateSandboxPolicy(policy) => {
                self.chat_widget.set_sandbox_policy(policy);
            }
        }
        Ok(true)
    }

    pub(crate) fn token_usage(&self) -> codex_core::protocol::TokenUsage {
        self.chat_widget.token_usage().clone()
    }

    /// Render a restored conversation (from a resumed session) into the transcript.
    /// This displays prior user and assistant text so the visible history matches
    /// the resumed context.
    fn render_resumed_history(
        &mut self,
        tui: &mut tui::Tui,
        ev: codex_core::protocol::ConversationHistoryResponseEvent,
    ) {
        use ratatui::style::Stylize;
        // Keep restored transcript hidden by default but available in Ctrl‑T overlay.
        let resume_path = self.config.experimental_resume.as_deref();
        let lines = render_lines_for_resumed_history(
            ev.entries.clone(),
            self.chat_widget.config_ref(),
            resume_path,
        );
        if !lines.is_empty() {
            self.transcript_lines.extend(lines);
        }
        // Show a single concise notice in the main view.
        let n = ev.entries.len();
        let mut notice: Vec<ratatui::text::Line<'static>> = Vec::new();
        notice.push("".into());
        notice.push(ratatui::text::Line::from(vec![
            "Restored session".magenta().bold(),
            " — ".into(),
            format!("{n} messages").cyan(),
            " • Press Ctrl-T to view transcript".dim(),
        ]));
        tui.insert_history_lines(notice);
        tui.frame_requester().schedule_frame();
    }

    // (helper for resume rendering moved to free function for testability)
    async fn handle_key_event(&mut self, tui: &mut tui::Tui, key_event: KeyEvent) {
        match key_event {
            KeyEvent {
                code: KeyCode::Char('t'),
                modifiers: crossterm::event::KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
                ..
            } => {
                let _ = tui.enter_alt_screen();
                self.overlay = Some(Overlay::new_transcript(self.transcript_lines.clone()));
                tui.frame_requester().schedule_frame();
            }
            KeyEvent {
                code: KeyCode::Esc,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            } => {
                if self.chat_widget.is_normal_backtrack_mode()
                    && self.chat_widget.composer_is_empty()
                {
                    self.handle_backtrack_esc_key(tui);
                } else {
                    self.chat_widget.handle_key_event(key_event);
                }
            }
            KeyEvent {
                code: KeyCode::Enter,
                kind: KeyEventKind::Press,
                ..
            } if self.backtrack.primed
                && self.backtrack.count > 0
                && self.chat_widget.composer_is_empty() =>
            {
                self.confirm_backtrack_from_main();
            }
            KeyEvent {
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            } => {
                if key_event.code != KeyCode::Esc && self.backtrack.primed {
                    self.reset_backtrack_state();
                }
                self.chat_widget.handle_key_event(key_event);
            }
            _ => {}
        };
    }
}

/// Pure helper so tests can validate resume rendering without a full TUI.
pub(crate) fn render_lines_for_resumed_history(
    entries: Vec<codex_protocol::models::ResponseItem>,
    cfg: &codex_core::config::Config,
    resume_path: Option<&std::path::Path>,
) -> Vec<ratatui::text::Line<'static>> {
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    use ratatui::style::Stylize;
    use std::collections::HashMap;
    // no durations used in minimal restore view

    let mut out: Vec<ratatui::text::Line<'static>> = Vec::new();

    // Optional recap header when resuming from a rollout path
    if let Some(path) = resume_path {
        let (created, id) = read_rollout_meta_first_line(path).unwrap_or_default();
        let stats = crate::session_meta::read_session_stats(path, 512 * 1024);
        let n = stats.message_count.unwrap_or(entries.len() as u32);

        out.push(ratatui::text::Line::from(""));
        let header = ratatui::text::Line::from(vec![
            "Restored".magenta().bold(),
            " — ".into(),
            created.clone().dim(),
            " ".into(),
            format!("({n})").cyan(),
        ]);
        out.push(header);

        // Quick highlights: show up to 6 recent actions (exec/tool)
        let mut highlights: Vec<ratatui::text::Line<'static>> = Vec::new();
        for item in entries.iter().rev() {
            match item {
                ResponseItem::LocalShellCall { action, .. } => {
                    if let codex_protocol::models::LocalShellAction::Exec(exec) = action {
                        let cmd = exec.command.join(" ");
                        highlights.push(ratatui::text::Line::from(vec![
                            "  • ".dim(),
                            "exec ".into(),
                            cmd.light_blue(),
                        ]));
                    }
                }
                ResponseItem::FunctionCall { name, .. } => {
                    let (server, tool) = name
                        .split_once("__")
                        .map(|(s, t)| (s.to_string(), t.to_string()))
                        .unwrap_or_else(|| (name.clone(), String::new()));
                    highlights.push(ratatui::text::Line::from(vec![
                        "  • ".dim(),
                        "tool ".into(),
                        format!("{server}/{tool}").into(),
                    ]));
                }
                _ => {}
            }
            if highlights.len() == 6 {
                break;
            }
        }
        if !highlights.is_empty() {
            out.extend(highlights.into_iter().rev());
        }

        // Meta footer (id and path) in dim text
        if let Some(id) = id {
            out.push(ratatui::text::Line::from(vec![
                "id: ".dim(),
                id.to_string().dim(),
            ]));
        }
        out.push(ratatui::text::Line::from(vec![
            "path: ".dim(),
            crate::exec_command::relativize_to_home(path)
                .unwrap_or_else(|| path.to_path_buf())
                .display()
                .to_string()
                .dim(),
        ]));
    }

    // Pre-index tool/exec outputs by call_id so we can attach them to their calls.
    let mut outputs_by_call: HashMap<String, codex_protocol::models::FunctionCallOutputPayload> =
        HashMap::new();
    for item in &entries {
        if let ResponseItem::FunctionCallOutput { call_id, output } = item {
            outputs_by_call.insert(call_id.clone(), output.clone());
        }
    }

    for item in entries {
        if let ResponseItem::Message { role, content, .. } = item {
            let mut text = String::new();
            for c in &content {
                match c {
                    ContentItem::InputText { text: t } | ContentItem::OutputText { text: t } => {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(t);
                    }
                    _ => {}
                }
            }
            // Strip noisy wrappers the model never needs to show on restore.
            let text = strip_wrappers(&text).unwrap_or_default();
            if text.is_empty() {
                continue;
            }
            if role == "user" {
                let cell = crate::history_cell::new_user_prompt(text);
                out.extend(cell.display_lines());
            } else {
                out.push(ratatui::text::Line::from(""));
                out.push(ratatui::text::Line::from("codex".magenta().bold()));
                let before = out.len();
                append_markdown(&text, &mut out, cfg);
                // Keep restore concise: cap assistant rendering to a small number of lines.
                const MAX_LINES: usize = 18;
                let after = out.len();
                let added = after.saturating_sub(before);
                if added > MAX_LINES {
                    // Remove extra lines and add a dim truncation note.
                    out.truncate(before + MAX_LINES);
                    out.push(ratatui::text::Line::from(
                        "… truncated; press Ctrl-T for full transcript".dim(),
                    ));
                }
            }
            continue;
        }

        // MCP tool call replay: FunctionCall paired with FunctionCallOutput.
        if let ResponseItem::FunctionCall {
            ref name,
            ref arguments,
            ref call_id,
            ..
        } = item
        {
            // Parse server/tool from qualified name: "<server>__<tool>".
            let (server, tool) = match name.split_once("__") {
                Some((s, t)) => (s.to_string(), t.to_string()),
                None => (name.clone(), String::new()),
            };
            // Parse arguments JSON if present.
            let _args_json: Option<serde_json::Value> = if arguments.trim().is_empty() {
                None
            } else {
                serde_json::from_str(arguments).ok()
            };

            if let Some(payload) = outputs_by_call.get(call_id) {
                // Minimal one-liner: "tool server/tool ✓|✗"
                let ok = payload.success.unwrap_or(true);
                out.push(ratatui::text::Line::from(""));
                let status = if ok { "✓".green() } else { "✗".red() };
                out.push(ratatui::text::Line::from(vec![
                    "tool".magenta(),
                    " ".into(),
                    format!("{server}/{tool}").into(),
                    " ".into(),
                    status,
                ]));
                continue;
            }
        }

        if let ResponseItem::Reasoning { .. } = item {
            out.push(ratatui::text::Line::from(""));
            out.push(ratatui::text::Line::from("thinking".magenta().italic()));
            continue;
        }

        if let ResponseItem::LocalShellCall {
            call_id,
            action: codex_protocol::models::LocalShellAction::Exec(exec),
            ..
        } = item
        {
            let cmd_tokens = exec.command;
            // Minimal one-liner for exec: status + command
            let payload = call_id
                .and_then(|id| outputs_by_call.get(&id))
                .cloned()
                .unwrap_or(codex_protocol::models::FunctionCallOutputPayload {
                    content: String::new(),
                    success: Some(true),
                });
            let ok = payload.success.unwrap_or(true);
            let status = if ok { "✓".green() } else { "✗".red() };
            let cmd_text = cmd_tokens.join(" ");
            out.push(ratatui::text::Line::from(""));
            out.push(ratatui::text::Line::from(vec![
                "  ".into(),
                status,
                " ".into(),
                format!("⌨️ {cmd_text}").light_blue(),
            ]));
            continue;
        }

        if let ResponseItem::FunctionCallOutput { call_id: _, output } = item {
            if !output.content.is_empty() {
                out.push(ratatui::text::Line::from(""));
                out.push(ratatui::text::Line::from("codex".magenta().bold()));
                append_markdown(&output.content, &mut out, cfg);
            }
            continue;
        }
    }

    out
}

fn read_rollout_meta_first_line(path: &std::path::Path) -> Option<(String, Option<uuid::Uuid>)> {
    use serde_json::Value;
    let text = std::fs::read_to_string(path).ok()?;
    let mut it = text.lines();
    let line = it.next()?.trim();
    let v: Value = serde_json::from_str(line).ok()?;
    let created = v
        .get("timestamp")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
        .replace('T', " ")
        .replace('Z', "");
    let id = v
        .get("id")
        .and_then(|x| x.as_str())
        .and_then(|s| uuid::Uuid::parse_str(s).ok());
    Some((created, id))
}

/// Remove XML-like wrappers we write into the transcript and skip entire
/// messages that are just environment context.
fn strip_wrappers(s: &str) -> Option<String> {
    let mut t = s.trim();
    // Skip environment context blocks entirely
    if t.contains("<environment_context>") {
        return None;
    }
    // Unwrap <user_instructions>…</user_instructions>
    if let Some(start) = t.find("<user_instructions>")
        && let Some(end) = t.find("</user_instructions>")
    {
        let inner = &t[start + "<user_instructions>".len()..end];
        t = inner.trim();
    }
    if let Some(start) = t.find("<user_interactions>")
        && let Some(end) = t.find("</user_interactions>")
    {
        let inner = &t[start + "<user_interactions>".len()..end];
        t = inner.trim();
    }
    Some(t.to_string())
}

#[cfg(test)]
mod tests {
    use super::render_lines_for_resumed_history;
    use codex_core::config::Config;
    use codex_core::config::ConfigOverrides;
    use codex_core::config::ConfigToml;
    use codex_protocol::models::ResponseItem;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn test_config() -> Config {
        codex_core::config::Config::load_from_base_config_with_overrides(
            ConfigToml::default(),
            ConfigOverrides::default(),
            std::env::temp_dir(),
        )
        .expect("config")
    }

    #[test]
    fn mcp_tool_call_replay_renders_text_output() {
        let cfg = test_config();
        let call_id = "call-123".to_string();
        let items = vec![
            ResponseItem::FunctionCall {
                id: None,
                name: "server__echo".to_string(),
                arguments: "{\"text\":\"hi\"}".to_string(),
                call_id: call_id.clone(),
            },
            ResponseItem::FunctionCallOutput {
                call_id: call_id.clone(),
                output: codex_protocol::models::FunctionCallOutputPayload {
                    // Minimal CallToolResult JSON with a single text block
                    content: "{\"content\":[{\"type\":\"text\",\"text\":\"hello from tool\"}],\"is_error\":false}".to_string(),
                    success: Some(true),
                },
            },
        ];

        let lines = render_lines_for_resumed_history(items, &cfg, None);
        let blob = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.clone())
            .collect::<String>();
        assert!(blob.contains("tool"), "expected tool header");
        assert!(
            blob.contains("hello from tool"),
            "expected tool output text"
        );
    }

    #[test]
    fn resume_renders_mixed_items_contains_expected_markers() {
        let cfg = test_config();
        let call_id_exec = "exec-1".to_string();
        let call_id_tool = "tool-1".to_string();
        let items = vec![
            ResponseItem::Message {
                id: None,
                role: "user".into(),
                content: vec![codex_protocol::models::ContentItem::InputText { text: "Hello".into() }],
            },
            ResponseItem::Message {
                id: None,
                role: "assistant".into(),
                content: vec![codex_protocol::models::ContentItem::OutputText { text: "Hi there".into() }],
            },
            ResponseItem::Reasoning {
                id: "r1".into(),
                summary: vec![codex_protocol::models::ReasoningItemReasoningSummary::SummaryText { text: "Plan".into() }],
                content: None,
                encrypted_content: None,
            },
            ResponseItem::LocalShellCall {
                id: None,
                call_id: Some(call_id_exec.clone()),
                status: codex_protocol::models::LocalShellStatus::Completed,
                action: codex_protocol::models::LocalShellAction::Exec(codex_protocol::models::LocalShellExecAction {
                    command: vec!["bash".into(), "-lc".into(), "echo hi".into()],
                    timeout_ms: None,
                    working_directory: None,
                    env: None,
                    user: None,
                }),
            },
            ResponseItem::FunctionCallOutput {
                call_id: call_id_exec,
                output: codex_protocol::models::FunctionCallOutputPayload { content: "hi".into(), success: Some(true) },
            },
            ResponseItem::FunctionCall {
                id: None,
                name: "server__echo".into(),
                arguments: "{\"text\":\"yo\"}".into(),
                call_id: call_id_tool.clone(),
            },
            ResponseItem::FunctionCallOutput {
                call_id: call_id_tool,
                output: codex_protocol::models::FunctionCallOutputPayload {
                    content: "{\"content\":[{\"type\":\"text\",\"text\":\"from tool\"}],\"is_error\":false}".into(),
                    success: Some(true),
                },
            },
        ];

        let lines = render_lines_for_resumed_history(items, &cfg, None);
        let blob = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(blob.contains("user"));
        assert!(blob.contains("Hello"));
        assert!(blob.contains("codex"));
        assert!(blob.contains("⌨️"));
        assert!(blob.contains("echo hi"));
        assert!(blob.contains("tool"));
        assert!(blob.contains("from tool"));
    }

    #[test]
    fn resume_recap_header_uses_sidecar_and_highlights() {
        let cfg = test_config();
        // Create a fake rollout file with a meta JSON line
        let mut tf = NamedTempFile::new().unwrap();
        writeln!(
            tf,
            "{{\"id\":\"00000000-0000-0000-0000-000000000000\",\"timestamp\":\"2025-09-01T12:00:00.000Z\"}}"
        )
        .unwrap();
        // Create matching sidecar with count only
        let sidecar_path = tf.path().with_file_name(format!(
            "{}.meta.json",
            tf.path().file_name().unwrap().to_string_lossy()
        ));
        std::fs::write(&sidecar_path, r#"{"message_count":42}"#)
        .unwrap();

        // Build entries with an exec and a tool call to test highlights
        let call_id_tool = "tool-abc".to_string();
        let items = vec![
            ResponseItem::LocalShellCall {
                id: None,
                call_id: Some("exec-123".into()),
                status: codex_protocol::models::LocalShellStatus::Completed,
                action: codex_protocol::models::LocalShellAction::Exec(
                    codex_protocol::models::LocalShellExecAction {
                        command: vec!["bash".into(), "-lc".into(), "rg foo".into()],
                        timeout_ms: None,
                        working_directory: None,
                        env: None,
                        user: None,
                    },
                ),
            },
            ResponseItem::FunctionCall {
                id: None,
                name: "server__echo".into(),
                arguments: "{\"text\":\"hi\"}".into(),
                call_id: call_id_tool.clone(),
            },
            ResponseItem::FunctionCallOutput {
                call_id: call_id_tool,
                output: codex_protocol::models::FunctionCallOutputPayload {
                    content:
                        "{\"content\":[{\"type\":\"text\",\"text\":\"hello\"}],\"is_error\":false}"
                            .into(),
                    success: Some(true),
                },
            },
        ];

        let lines = render_lines_for_resumed_history(items, &cfg, Some(tf.path()));
        let blob = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.clone())
            .collect::<String>();
        assert!(blob.contains("Restored"), "expected recap header");
        assert!(blob.contains("(42)"), "expected message count from sidecar");
        assert!(blob.contains("exec"), "expected exec highlight");
        assert!(blob.contains("server/echo"), "expected tool highlight");
    }
}

use std::collections::{HashMap, VecDeque};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use unicode_width::UnicodeWidthChar;

use crate::capture::SourceKind;
use crate::transcript::DisplayAction;

const MAX_NOTICES: usize = 8;
const MAX_TURNS_PER_SOURCE: usize = 512;

#[derive(Debug, Clone)]
pub enum AppEvent {
    DraftStatus {
        source: SourceKind,
        status: String,
    },
    FinalizerStatus {
        source: SourceKind,
        status: String,
    },
    QueueMetrics {
        source: SourceKind,
        queue_depth: usize,
        pending_audio_secs: f32,
    },
    CaptureLevel {
        source: SourceKind,
        rms: f32,
    },
    DraftTranscript {
        source: SourceKind,
        turn_id: String,
        at: Instant,
        text: String,
    },
    TurnPending {
        source: SourceKind,
        turn_id: String,
    },
    TurnFinalized {
        source: SourceKind,
        turn_id: String,
        text: String,
        display_action: DisplayAction,
    },
    TurnFailed {
        source: SourceKind,
        turn_id: String,
        error: String,
    },
    Usage {
        source: SourceKind,
        usage: UsageSnapshot,
    },
    SourceError {
        source: SourceKind,
        error: String,
    },
    Notice(String),
    Redraw,
    Quit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageSnapshot {
    pub prompt_token_count: u32,
    pub cached_content_token_count: u32,
    pub response_token_count: u32,
    pub tool_use_prompt_token_count: u32,
    pub thoughts_token_count: u32,
    pub total_token_count: u32,
}

#[derive(Debug)]
pub struct App {
    started_at: Instant,
    model: String,
    profile_summary: String,
    sources: Vec<SourceKind>,
    panes: HashMap<SourceKind, SourcePane>,
    notices: VecDeque<String>,
}

#[derive(Debug, Default)]
struct SourcePane {
    draft_status: String,
    finalizer_status: String,
    capture_rms: Option<f32>,
    queue_depth: usize,
    pending_audio_secs: f32,
    turns: VecDeque<TimestampedTurn>,
    usage: Option<UsageSnapshot>,
    error: Option<String>,
    content_revision: u64,
    layout_cache: Option<PaneLayoutCache>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnState {
    Draft,
    PendingFinalization,
    Final,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TimestampedTurn {
    turn_id: String,
    started_at: Duration,
    draft_text: String,
    final_text: Option<String>,
    state: TurnState,
    display_action: DisplayAction,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct PaneLayoutCache {
    width: u16,
    height: u16,
    revision: u64,
    scroll: u16,
    text: Text<'static>,
}

impl App {
    pub fn new(model: String, profile_summary: String, sources: Vec<SourceKind>) -> Self {
        let panes = sources
            .iter()
            .map(|source| {
                (
                    *source,
                    SourcePane {
                        draft_status: "starting".into(),
                        finalizer_status: "starting".into(),
                        ..SourcePane::default()
                    },
                )
            })
            .collect();

        Self {
            started_at: Instant::now(),
            model,
            profile_summary,
            sources,
            panes,
            notices: VecDeque::new(),
        }
    }

    pub fn apply(&mut self, event: AppEvent) -> bool {
        match event {
            AppEvent::DraftStatus { source, status } => {
                if let Some(pane) = self.panes.get_mut(&source) {
                    return pane.set_draft_status(status);
                }
            }
            AppEvent::FinalizerStatus { source, status } => {
                if let Some(pane) = self.panes.get_mut(&source) {
                    return pane.set_finalizer_status(status);
                }
            }
            AppEvent::QueueMetrics {
                source,
                queue_depth,
                pending_audio_secs,
            } => {
                if let Some(pane) = self.panes.get_mut(&source) {
                    return pane.set_queue_metrics(queue_depth, pending_audio_secs);
                }
            }
            AppEvent::CaptureLevel { source, rms } => {
                if let Some(pane) = self.panes.get_mut(&source) {
                    return pane.set_capture_rms(rms);
                }
            }
            AppEvent::DraftTranscript {
                source,
                turn_id,
                at,
                text,
            } => {
                if let Some(pane) = self.panes.get_mut(&source) {
                    return pane.update_draft(
                        turn_id,
                        normalize_transcript_text(&text),
                        at.checked_duration_since(self.started_at)
                            .unwrap_or(Duration::ZERO),
                    );
                }
            }
            AppEvent::TurnPending { source, turn_id } => {
                if let Some(pane) = self.panes.get_mut(&source) {
                    return pane.mark_pending(&turn_id);
                }
            }
            AppEvent::TurnFinalized {
                source,
                turn_id,
                text,
                display_action,
            } => {
                if let Some(pane) = self.panes.get_mut(&source) {
                    return pane.mark_final(
                        &turn_id,
                        normalize_transcript_text(&text),
                        display_action,
                    );
                }
            }
            AppEvent::TurnFailed {
                source,
                turn_id,
                error,
            } => {
                if let Some(pane) = self.panes.get_mut(&source) {
                    return pane.mark_failed(&turn_id, error);
                }
            }
            AppEvent::Usage { source, usage } => {
                if let Some(pane) = self.panes.get_mut(&source) {
                    return pane.set_usage(usage);
                }
            }
            AppEvent::SourceError { source, error } => {
                if let Some(pane) = self.panes.get_mut(&source) {
                    let pane_changed = pane.set_error(error.clone());
                    let notice_changed = self.push_notice(format!("{}: {error}", source.title()));
                    return pane_changed || notice_changed;
                }
            }
            AppEvent::Notice(message) => return self.push_notice(message),
            AppEvent::Redraw | AppEvent::Quit => {}
        }
        false
    }

    fn push_notice(&mut self, message: String) -> bool {
        if self.notices.len() == MAX_NOTICES {
            self.notices.pop_front();
        }
        self.notices.push_back(message);
        true
    }

    fn render(&mut self, frame: &mut ratatui::Frame) {
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(4),
                Constraint::Min(8),
                Constraint::Length(4),
            ])
            .split(inner_rect(frame.area()));

        self.render_status(frame, layout[0]);
        self.render_transcripts(frame, layout[1]);
        self.render_controls(frame, layout[2]);
    }

    fn render_status(&self, frame: &mut ratatui::Frame, area: Rect) {
        let latest_notice = self
            .notices
            .back()
            .cloned()
            .unwrap_or_else(|| "ready".into());
        let text = Text::from(vec![
            Line::from(Span::styled(
                format!("model: {} | capture: 16 kHz mono | q: quit", self.model),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                latest_notice,
                Style::default().fg(Color::Cyan),
            )),
        ]);

        let paragraph = Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title("status"))
            .wrap(Wrap { trim: false });

        frame.render_widget(paragraph, area);
    }

    fn render_transcripts(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        let constraints = vec![Constraint::Fill(1); self.sources.len()];
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(constraints)
            .split(area);

        for (index, source) in self.sources.iter().copied().enumerate() {
            if let Some(pane) = self.panes.get_mut(&source) {
                let area = columns[index];
                let block = Block::default()
                    .borders(Borders::ALL)
                    .title(pane.title(source))
                    .border_style(pane.border_style());

                let inner = inner_rect(area);
                let layout = pane.layout_for(inner.width, inner.height);

                let paragraph = Paragraph::new(layout.text.clone())
                    .block(block)
                    .scroll((layout.scroll, 0));

                frame.render_widget(paragraph, area);
            }
        }
    }

    fn render_controls(&self, frame: &mut ratatui::Frame, area: Rect) {
        let lines = vec![
            Line::from(format!("q quit | {}", self.profile_summary)),
            Line::from("Each source streams to Gemini Live independently."),
        ];

        let paragraph = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title("controls"))
            .wrap(Wrap { trim: false });

        frame.render_widget(paragraph, area);
    }
}

impl SourcePane {
    fn set_draft_status(&mut self, status: String) -> bool {
        if self.draft_status == status {
            return false;
        }
        self.draft_status = status;
        self.invalidate_layout();
        true
    }

    fn set_finalizer_status(&mut self, status: String) -> bool {
        if self.finalizer_status == status {
            return false;
        }
        self.finalizer_status = status;
        self.invalidate_layout();
        true
    }

    fn set_queue_metrics(&mut self, queue_depth: usize, pending_audio_secs: f32) -> bool {
        if self.queue_depth == queue_depth
            && (self.pending_audio_secs - pending_audio_secs).abs() < f32::EPSILON
        {
            return false;
        }
        self.queue_depth = queue_depth;
        self.pending_audio_secs = pending_audio_secs;
        self.invalidate_layout();
        true
    }

    fn set_capture_rms(&mut self, rms: f32) -> bool {
        let rms = quantize_rms(rms);
        if self.capture_rms == Some(rms) {
            return false;
        }
        self.capture_rms = Some(rms);
        self.invalidate_layout();
        true
    }

    fn set_error(&mut self, error: String) -> bool {
        if self.error.as_deref() == Some(error.as_str()) {
            return false;
        }

        self.draft_status = "error".into();
        self.finalizer_status = "error".into();
        self.error = Some(error);
        self.invalidate_layout();
        true
    }

    fn set_usage(&mut self, usage: UsageSnapshot) -> bool {
        let current = self.usage.get_or_insert(UsageSnapshot {
            prompt_token_count: 0,
            cached_content_token_count: 0,
            response_token_count: 0,
            tool_use_prompt_token_count: 0,
            thoughts_token_count: 0,
            total_token_count: 0,
        });

        let previous = current.clone();

        if usage.prompt_token_count > 0 {
            current.prompt_token_count = usage.prompt_token_count;
        }
        if usage.cached_content_token_count > 0 {
            current.cached_content_token_count = usage.cached_content_token_count;
        }
        if usage.response_token_count > 0 {
            current.response_token_count = usage.response_token_count;
        }
        if usage.tool_use_prompt_token_count > 0 {
            current.tool_use_prompt_token_count = usage.tool_use_prompt_token_count;
        }
        if usage.thoughts_token_count > 0 {
            current.thoughts_token_count = usage.thoughts_token_count;
        }
        if usage.total_token_count > 0 {
            current.total_token_count = usage.total_token_count;
        }

        *current != previous
    }

    fn update_draft(&mut self, turn_id: String, text: String, started_at: Duration) -> bool {
        if text.is_empty() {
            return false;
        }

        if let Some(turn) = self.turns.iter_mut().find(|turn| turn.turn_id == turn_id) {
            if turn.draft_text == text {
                return false;
            }
            turn.draft_text = text;
            turn.started_at = turn.started_at.min(started_at);
            turn.state = TurnState::Draft;
            self.invalidate_layout();
            return true;
        }

        if self.turns.len() == MAX_TURNS_PER_SOURCE {
            self.turns.pop_front();
        }
        self.turns.push_back(TimestampedTurn {
            turn_id,
            started_at,
            draft_text: text,
            final_text: None,
            state: TurnState::Draft,
            display_action: DisplayAction::NewBlock,
            error: None,
        });
        self.invalidate_layout();
        true
    }

    fn mark_pending(&mut self, turn_id: &str) -> bool {
        let Some(turn) = self.turns.iter_mut().find(|turn| turn.turn_id == turn_id) else {
            return false;
        };
        if turn.state == TurnState::PendingFinalization {
            return false;
        }
        turn.state = TurnState::PendingFinalization;
        self.invalidate_layout();
        true
    }

    fn mark_final(&mut self, turn_id: &str, text: String, display_action: DisplayAction) -> bool {
        let Some(turn) = self.turns.iter_mut().find(|turn| turn.turn_id == turn_id) else {
            return false;
        };
        turn.final_text = Some(text);
        turn.state = TurnState::Final;
        turn.display_action = display_action;
        turn.error = None;
        self.invalidate_layout();
        true
    }

    fn mark_failed(&mut self, turn_id: &str, error: String) -> bool {
        let Some(turn) = self.turns.iter_mut().find(|turn| turn.turn_id == turn_id) else {
            return false;
        };
        if turn.state == TurnState::Failed && turn.error.as_deref() == Some(error.as_str()) {
            return false;
        }
        turn.state = TurnState::Failed;
        turn.error = Some(error);
        self.invalidate_layout();
        true
    }

    fn border_style(&self) -> Style {
        if self.error.is_some() {
            Style::default().fg(Color::Red)
        } else if is_degraded_status(&self.draft_status)
            || is_degraded_status(&self.finalizer_status)
        {
            Style::default().fg(Color::Yellow)
        } else if self.draft_status == "listening" || self.draft_status == "capturing" {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::Gray)
        }
    }

    fn title(&self, source: SourceKind) -> String {
        let rms = self
            .capture_rms
            .map(|rms| format!("rms {rms:.3}"))
            .unwrap_or_else(|| "rms --".to_owned());
        let queue = if self.queue_depth == 0 {
            "q 0".to_owned()
        } else {
            format!("q {} | {:.1}s", self.queue_depth, self.pending_audio_secs)
        };
        match &self.usage {
            Some(usage) if usage.prompt_token_count > 0 => format!(
                "{} [d:{} | f:{} | {} | {} | ctx {}]",
                source.title(),
                self.draft_status,
                self.finalizer_status,
                rms,
                queue,
                abbreviate_token_count(usage.prompt_token_count)
            ),
            _ => format!(
                "{} [d:{} | f:{} | {} | {}]",
                source.title(),
                self.draft_status,
                self.finalizer_status,
                rms,
                queue
            ),
        }
    }

    fn layout_for(&mut self, width: u16, height: u16) -> &PaneLayoutCache {
        let needs_rebuild = self
            .layout_cache
            .as_ref()
            .is_none_or(|cache| cache.width != width || cache.revision != self.content_revision);

        if needs_rebuild {
            let lines = wrap_display_lines(&self.display_lines(), width);
            let scroll = compute_scroll(lines.len(), height);
            self.layout_cache = Some(PaneLayoutCache {
                width,
                height,
                revision: self.content_revision,
                scroll,
                text: Text::from(lines),
            });
        } else if let Some(cache) = &mut self.layout_cache
            && cache.height != height
        {
            cache.height = height;
            cache.scroll = compute_scroll(cache.text.lines.len(), height);
        }

        self.layout_cache
            .as_ref()
            .expect("layout cache should be initialized")
    }

    fn display_lines(&self) -> Vec<DisplayLine> {
        let mut lines = Vec::new();
        let mut previous_finalized_exists = false;

        for turn in &self.turns {
            let prefix_override = if turn.state == TurnState::Final
                && turn.display_action == DisplayAction::AppendPreviousBlock
                && previous_finalized_exists
            {
                Some(" ".repeat("00:00:00 | ".len()))
            } else {
                None
            };

            let (text, style) = match turn.state {
                TurnState::Draft => (
                    format!("[draft] {}", turn.draft_text),
                    Style::default().fg(Color::Yellow),
                ),
                TurnState::PendingFinalization => (
                    format!("[pending] {}", turn.draft_text),
                    Style::default().fg(Color::LightYellow),
                ),
                TurnState::Final => (
                    turn.final_text.clone().unwrap_or_default(),
                    Style::default().fg(Color::White),
                ),
                TurnState::Failed => (
                    format!("[failed] {}", turn.draft_text),
                    Style::default().fg(Color::LightRed),
                ),
            };

            lines.push(DisplayLine::transcript(
                turn.started_at,
                text,
                style,
                prefix_override,
            ));

            if let Some(error) = &turn.error {
                lines.push(DisplayLine::plain(
                    format!("           {}", error),
                    Style::default().fg(Color::Red),
                ));
            }

            previous_finalized_exists |= turn.state == TurnState::Final;
        }

        if lines.is_empty() {
            lines.push(DisplayLine::plain(
                "waiting for transcription…".into(),
                Style::default().fg(Color::DarkGray),
            ));
        }

        if let Some(error) = &self.error {
            lines.push(DisplayLine::plain(String::new(), Style::default()));
            lines.push(DisplayLine::plain(
                error.clone(),
                Style::default().fg(Color::Red),
            ));
        }

        lines
    }

    fn invalidate_layout(&mut self) {
        self.content_revision = self.content_revision.wrapping_add(1);
        self.layout_cache = None;
    }
}

fn is_degraded_status(status: &str) -> bool {
    matches!(
        status,
        "resuming" | "reconnecting" | "starting" | "connecting"
    )
}

fn quantize_rms(rms: f32) -> f32 {
    ((rms.max(0.0) * 1000.0).round()) / 1000.0
}

pub fn run(
    app: &mut App,
    events: mpsc::Receiver<AppEvent>,
    event_tx: mpsc::Sender<AppEvent>,
) -> Result<()> {
    let mut terminal = ratatui::init();
    let stop_input = Arc::new(AtomicBool::new(false));
    let input_handle = spawn_terminal_event_thread(event_tx, Arc::clone(&stop_input));

    let run_result = (|| -> Result<()> {
        terminal.draw(|frame| app.render(frame))?;

        while let Ok(event) = events.recv() {
            let mut should_draw = false;
            let mut should_quit = false;

            handle_ui_event(app, event, &mut should_draw, &mut should_quit);

            while let Ok(event) = events.try_recv() {
                handle_ui_event(app, event, &mut should_draw, &mut should_quit);
                if should_quit {
                    break;
                }
            }

            if should_quit {
                break;
            }

            if should_draw {
                terminal.draw(|frame| app.render(frame))?;
            }
        }

        Ok(())
    })();

    stop_input.store(true, Ordering::Relaxed);
    if let Err(error) = input_handle.join() {
        tracing::error!(?error, "terminal event thread panicked");
    }

    ratatui::restore();
    run_result
}

fn handle_ui_event(app: &mut App, event: AppEvent, should_draw: &mut bool, should_quit: &mut bool) {
    match event {
        AppEvent::Quit => *should_quit = true,
        AppEvent::Redraw => *should_draw = true,
        event => *should_draw |= app.apply(event),
    }
}

fn spawn_terminal_event_thread(
    event_tx: mpsc::Sender<AppEvent>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match event::poll(Duration::from_millis(250)) {
                Ok(true) => match event::read() {
                    Ok(Event::Key(key))
                        if key.kind == KeyEventKind::Press && key.code == KeyCode::Char('q') =>
                    {
                        let _ = event_tx.send(AppEvent::Quit);
                        break;
                    }
                    Ok(Event::Resize(_, _)) => {
                        let _ = event_tx.send(AppEvent::Redraw);
                    }
                    Ok(_) => {}
                    Err(error) => {
                        let _ = event_tx
                            .send(AppEvent::Notice(format!("terminal input error: {error}")));
                        break;
                    }
                },
                Ok(false) => {}
                Err(error) => {
                    let _ =
                        event_tx.send(AppEvent::Notice(format!("terminal poll error: {error}")));
                    break;
                }
            }
        }
    })
}

fn inner_rect(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

#[derive(Debug, Clone)]
struct DisplayLine {
    prefix: String,
    text: String,
    style: Style,
}

impl DisplayLine {
    fn plain(text: String, style: Style) -> Self {
        Self {
            prefix: String::new(),
            text,
            style,
        }
    }

    fn transcript(
        started_at: Duration,
        text: String,
        style: Style,
        prefix_override: Option<String>,
    ) -> Self {
        Self {
            prefix: prefix_override
                .unwrap_or_else(|| format!("{} | ", format_relative_time(started_at))),
            text,
            style,
        }
    }
}

fn compute_scroll(line_count: usize, height: u16) -> u16 {
    line_count.saturating_sub(height as usize) as u16
}

fn wrap_display_lines(lines: &[DisplayLine], width: u16) -> Vec<Line<'static>> {
    let wrap_width = usize::from(width.max(1));
    let mut wrapped = Vec::new();

    for line in lines {
        for segment in wrap_display_line(line, wrap_width) {
            wrapped.push(Line::from(Span::styled(segment, line.style)));
        }
    }

    if wrapped.is_empty() {
        wrapped.push(Line::from(""));
    }

    wrapped
}

fn wrap_display_line(line: &DisplayLine, width: usize) -> Vec<String> {
    if line.prefix.is_empty() {
        return wrap_line(&line.text, width);
    }

    let prefix_width = line.prefix.chars().map(char_display_width).sum::<usize>();
    if width <= prefix_width {
        return wrap_line(&format!("{}{}", line.prefix, line.text), width);
    }

    let body_width = width.saturating_sub(prefix_width).max(1);
    let body_segments = wrap_line(&line.text, body_width);
    let mut wrapped = Vec::with_capacity(body_segments.len().max(1));
    let continuation_prefix = " ".repeat(line.prefix.len());

    for (index, segment) in body_segments.into_iter().enumerate() {
        if index == 0 {
            wrapped.push(format!("{}{}", line.prefix, segment));
        } else {
            wrapped.push(format!("{}{}", continuation_prefix, segment));
        }
    }

    if wrapped.is_empty() {
        wrapped.push(line.prefix.clone());
    }

    wrapped
}

fn wrap_line(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }

    let mut wrapped = Vec::new();

    for raw_line in text.split('\n') {
        if raw_line.is_empty() {
            wrapped.push(String::new());
            continue;
        }

        let mut current = String::new();
        let mut current_width = 0usize;

        for ch in raw_line.chars() {
            let ch_width = char_display_width(ch);
            let exceeds = current_width + ch_width > width;

            if exceeds && !current.is_empty() {
                wrapped.push(std::mem::take(&mut current));
                current_width = 0;
            }

            current.push(ch);
            current_width += ch_width;

            if current_width >= width && !current.is_empty() {
                wrapped.push(std::mem::take(&mut current));
                current_width = 0;
            }
        }

        if !current.is_empty() {
            wrapped.push(current);
        }
    }

    if wrapped.is_empty() {
        wrapped.push(String::new());
    }

    wrapped
}

fn format_relative_time(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

fn abbreviate_token_count(tokens: u32) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn char_display_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

fn normalize_transcript_text(input: &str) -> String {
    let chars = input.chars().collect::<Vec<_>>();
    let mut normalized = String::with_capacity(input.len());

    for (index, ch) in chars.iter().enumerate() {
        if ch.is_whitespace() {
            let previous = chars[..index]
                .iter()
                .rev()
                .find(|candidate| !candidate.is_whitespace());
            let next = chars[index + 1..]
                .iter()
                .find(|candidate| !candidate.is_whitespace());

            if should_drop_inter_cjk_space(previous.copied(), next.copied()) {
                continue;
            }

            if next.copied().is_some_and(is_cjk_punctuation) {
                continue;
            }

            if !normalized.ends_with(' ') {
                normalized.push(' ');
            }
            continue;
        }

        normalized.push(*ch);
    }

    normalized.trim().to_owned()
}

fn should_drop_inter_cjk_space(previous: Option<char>, next: Option<char>) -> bool {
    match (previous, next) {
        (Some(previous), Some(next)) => {
            let previous_is_cjk = is_cjk(previous) || is_cjk_punctuation(previous);
            let next_is_cjk = is_cjk(next) || is_cjk_punctuation(next);
            previous_is_cjk && next_is_cjk
        }
        _ => false,
    }
}

fn is_cjk(ch: char) -> bool {
    matches!(
        ch,
        '\u{3400}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{3040}'..='\u{309F}'
            | '\u{30A0}'..='\u{30FF}'
            | '\u{AC00}'..='\u{D7AF}'
    )
}

fn is_cjk_punctuation(ch: char) -> bool {
    matches!(ch, '\u{3000}'..='\u{303F}')
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::capture::SourceKind;
    use crate::transcript::DisplayAction;

    use super::{
        App, AppEvent, SourcePane, UsageSnapshot, format_relative_time, normalize_transcript_text,
        wrap_display_line, wrap_line,
    };

    #[test]
    fn updates_existing_draft_turn_by_id() {
        let mut pane = SourcePane::default();
        pane.update_draft("turn-1".into(), "hello".into(), Duration::from_secs(5));
        pane.update_draft(
            "turn-1".into(),
            "hello world".into(),
            Duration::from_secs(6),
        );

        assert_eq!(pane.turns.len(), 1);
        assert_eq!(pane.turns[0].draft_text, "hello world");
        assert_eq!(pane.turns[0].state, super::TurnState::Draft);
    }

    #[test]
    fn marks_turn_pending_and_finalized() {
        let mut pane = SourcePane::default();
        pane.update_draft("turn-1".into(), "hello".into(), Duration::from_secs(1));
        pane.mark_pending("turn-1");
        pane.mark_final("turn-1", "hello world".into(), DisplayAction::NewBlock);

        assert_eq!(pane.turns[0].final_text.as_deref(), Some("hello world"));
        assert_eq!(pane.turns[0].state, super::TurnState::Final);
    }

    #[test]
    fn groups_append_previous_block_visually() {
        let mut pane = SourcePane::default();
        pane.update_draft("turn-1".into(), "first".into(), Duration::from_secs(1));
        pane.mark_final("turn-1", "first".into(), DisplayAction::NewBlock);
        pane.update_draft("turn-2".into(), "second".into(), Duration::from_secs(2));
        pane.mark_final(
            "turn-2",
            "second".into(),
            DisplayAction::AppendPreviousBlock,
        );

        let lines = pane.display_lines();
        assert_eq!(lines[1].prefix, "           ");
    }

    #[test]
    fn app_updates_queue_metrics() {
        let mut app = App::new(
            "model".into(),
            "lang: zh-Hant".into(),
            vec![SourceKind::Microphone],
        );

        assert!(app.apply(AppEvent::QueueMetrics {
            source: SourceKind::Microphone,
            queue_depth: 2,
            pending_audio_secs: 3.5,
        }));
    }

    #[test]
    fn app_updates_capture_rms() {
        let mut app = App::new(
            "model".into(),
            "lang: zh-Hant".into(),
            vec![SourceKind::Microphone],
        );

        assert!(app.apply(AppEvent::CaptureLevel {
            source: SourceKind::Microphone,
            rms: 0.0124,
        }));
        assert!(!app.apply(AppEvent::CaptureLevel {
            source: SourceKind::Microphone,
            rms: 0.01249,
        }));
    }

    #[test]
    fn removes_spaces_between_cjk_characters() {
        assert_eq!(
            normalize_transcript_text("我 不 知 道 這 樣 子 能 不 能 work 。"),
            "我不知道這樣子能不能 work。"
        );
    }

    #[test]
    fn preserves_spaces_between_cjk_and_latin_words() {
        assert_eq!(
            normalize_transcript_text("這 個 remix 很 奇 怪"),
            "這個 remix 很奇怪"
        );
    }

    #[test]
    fn wraps_long_lines_to_requested_width() {
        assert_eq!(
            wrap_line("abcdef", 3),
            vec!["abc".to_string(), "def".to_string()]
        );
    }

    #[test]
    fn wraps_timestamped_lines_with_hanging_indent() {
        let wrapped = wrap_display_line(
            &super::DisplayLine {
                prefix: "00:00:05 | ".into(),
                text: "abcdefgh".into(),
                style: ratatui::style::Style::default(),
            },
            14,
        );

        assert_eq!(
            wrapped,
            vec![
                "00:00:05 | abc".to_string(),
                "           def".to_string(),
                "           gh".to_string()
            ]
        );
    }

    #[test]
    fn formats_relative_time_as_hms() {
        assert_eq!(format_relative_time(Duration::from_secs(0)), "00:00:00");
        assert_eq!(format_relative_time(Duration::from_secs(3661)), "01:01:01");
    }

    #[test]
    fn includes_queue_and_usage_in_title() {
        let mut pane = SourcePane {
            draft_status: "listening".into(),
            finalizer_status: "ready".into(),
            capture_rms: Some(0.012),
            queue_depth: 2,
            pending_audio_secs: 1.5,
            ..SourcePane::default()
        };
        assert!(pane.set_usage(UsageSnapshot {
            prompt_token_count: 10,
            cached_content_token_count: 2,
            response_token_count: 5,
            tool_use_prompt_token_count: 0,
            thoughts_token_count: 0,
            total_token_count: 1500,
        }));

        assert_eq!(
            pane.title(SourceKind::Microphone),
            "microphone [d:listening | f:ready | rms 0.012 | q 2 | 1.5s | ctx 10]"
        );
    }
}

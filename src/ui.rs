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

const MAX_NOTICES: usize = 8;
const MAX_TURNS_PER_SOURCE: usize = 512;

#[derive(Debug, Clone)]
pub enum AppEvent {
    SourceStatus {
        source: SourceKind,
        status: String,
    },
    Transcript {
        source: SourceKind,
        at: Instant,
        text: String,
    },
    TurnComplete {
        source: SourceKind,
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
    preference_summary: String,
    sources: Vec<SourceKind>,
    panes: HashMap<SourceKind, SourcePane>,
    notices: VecDeque<String>,
}

#[derive(Debug, Default)]
struct SourcePane {
    status: String,
    turns: VecDeque<TimestampedTurn>,
    draft: Option<TimestampedTurn>,
    usage: Option<UsageSnapshot>,
    error: Option<String>,
    content_revision: u64,
    layout_cache: Option<PaneLayoutCache>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TimestampedTurn {
    started_at: Duration,
    text: String,
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
    pub fn new(model: String, preference_summary: String, sources: Vec<SourceKind>) -> Self {
        let panes = sources
            .iter()
            .map(|source| {
                (
                    *source,
                    SourcePane {
                        status: "starting".into(),
                        ..SourcePane::default()
                    },
                )
            })
            .collect();

        Self {
            started_at: Instant::now(),
            model,
            preference_summary,
            sources,
            panes,
            notices: VecDeque::new(),
        }
    }

    pub fn apply(&mut self, event: AppEvent) -> bool {
        match event {
            AppEvent::SourceStatus { source, status } => {
                if let Some(pane) = self.panes.get_mut(&source) {
                    return pane.set_status(status);
                }
            }
            AppEvent::Transcript { source, at, text } => {
                if let Some(pane) = self.panes.get_mut(&source) {
                    return pane.update_draft(
                        normalize_transcript_text(&text),
                        at.checked_duration_since(self.started_at)
                            .unwrap_or(Duration::ZERO),
                    );
                }
            }
            AppEvent::TurnComplete { source } => {
                if let Some(pane) = self.panes.get_mut(&source) {
                    return pane.commit_draft();
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
            Line::from(format!("q quit | {}", self.preference_summary)),
            Line::from("Each source streams to Gemini Live independently."),
        ];

        let paragraph = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title("controls"))
            .wrap(Wrap { trim: false });

        frame.render_widget(paragraph, area);
    }
}

impl SourcePane {
    fn set_status(&mut self, status: String) -> bool {
        if self.status == status {
            return false;
        }

        self.status = status;
        true
    }

    fn set_error(&mut self, error: String) -> bool {
        if self.status == "error" && self.error.as_deref() == Some(error.as_str()) {
            return false;
        }

        self.status = "error".into();
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

    fn update_draft(&mut self, text: String, started_at: Duration) -> bool {
        if text.is_empty() {
            return false;
        }

        match self.draft.as_mut() {
            Some(draft) if draft.text == text => false,
            Some(draft) if text.starts_with(&draft.text) || draft.text.starts_with(&text) => {
                draft.text = text;
                self.invalidate_layout();
                true
            }
            Some(_) => {
                let previous = self
                    .draft
                    .take()
                    .expect("draft should exist when starting a new turn");
                self.push_turn(previous);
                self.draft = Some(TimestampedTurn { started_at, text });
                self.invalidate_layout();
                true
            }
            None => {
                self.draft = Some(TimestampedTurn { started_at, text });
                self.invalidate_layout();
                true
            }
        }
    }

    fn commit_draft(&mut self) -> bool {
        if self.draft.is_none() {
            return false;
        }
        let draft = self
            .draft
            .take()
            .expect("draft should exist when committing a turn");
        self.push_turn(draft);
        self.invalidate_layout();
        true
    }

    fn push_turn(&mut self, turn: TimestampedTurn) {
        if self.turns.len() == MAX_TURNS_PER_SOURCE {
            self.turns.pop_front();
        }
        self.turns.push_back(turn);
    }

    fn border_style(&self) -> Style {
        if self.error.is_some() {
            Style::default().fg(Color::Red)
        } else if self.status == "resuming" || self.status == "reconnecting" {
            Style::default().fg(Color::Yellow)
        } else if self.status == "listening" || self.status == "capturing" {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::Gray)
        }
    }

    fn title(&self, source: SourceKind) -> String {
        match &self.usage {
            Some(usage) if usage.prompt_token_count > 0 => format!(
                "{} [{} | ctx {}]",
                source.title(),
                self.status,
                abbreviate_token_count(usage.prompt_token_count)
            ),
            None => format!("{} [{}]", source.title(), self.status),
            Some(_) => format!("{} [{}]", source.title(), self.status),
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
        let mut lines = self
            .turns
            .iter()
            .cloned()
            .map(|turn| DisplayLine::transcript(turn, Style::default()))
            .collect::<Vec<_>>();

        if let Some(draft) = &self.draft {
            lines.push(DisplayLine::transcript(
                draft.clone(),
                Style::default().fg(Color::Yellow),
            ));
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

    fn transcript(turn: TimestampedTurn, style: Style) -> Self {
        Self {
            prefix: format!("{} | ", format_relative_time(turn.started_at)),
            text: turn.text,
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

    use super::{
        SourcePane, UsageSnapshot, format_relative_time, normalize_transcript_text,
        wrap_display_line, wrap_line,
    };

    #[test]
    fn replaces_draft_when_server_expands_same_turn() {
        let mut pane = SourcePane::default();
        pane.update_draft("hello".into(), Duration::from_secs(5));
        pane.update_draft("hello world".into(), Duration::from_secs(6));

        assert!(pane.turns.is_empty());
        assert_eq!(
            pane.draft,
            Some(super::TimestampedTurn {
                started_at: Duration::from_secs(5),
                text: "hello world".into(),
            })
        );
    }

    #[test]
    fn commits_previous_draft_when_server_starts_new_segment() {
        let mut pane = SourcePane::default();
        pane.update_draft("hello".into(), Duration::from_secs(1));
        pane.update_draft("another sentence".into(), Duration::from_secs(4));

        assert_eq!(pane.turns.len(), 1);
        assert_eq!(pane.turns[0].text, "hello");
        assert_eq!(pane.turns[0].started_at, Duration::from_secs(1));
        assert_eq!(
            pane.draft,
            Some(super::TimestampedTurn {
                started_at: Duration::from_secs(4),
                text: "another sentence".into(),
            })
        );
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
    fn reuses_cached_layout_when_height_changes_only() {
        let mut pane = SourcePane::default();
        assert!(pane.update_draft("abcdefgh".into(), Duration::from_secs(8)));

        let first_text = pane.layout_for(4, 2).text.clone();
        let first_revision = pane.layout_cache.as_ref().map(|cache| cache.revision);

        let second_layout = pane.layout_for(4, 1);

        assert_eq!(first_revision, Some(second_layout.revision));
        assert_eq!(first_text, second_layout.text);
        assert_eq!(second_layout.scroll, 4);
    }

    #[test]
    fn formats_relative_time_as_hms() {
        assert_eq!(format_relative_time(Duration::from_secs(0)), "00:00:00");
        assert_eq!(format_relative_time(Duration::from_secs(3661)), "01:01:01");
    }

    #[test]
    fn includes_token_usage_in_pane_title() {
        let mut pane = SourcePane {
            status: "listening".into(),
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
            "microphone [listening | ctx 10]"
        );
    }

    #[test]
    fn preserves_prompt_context_count_when_response_only_usage_arrives() {
        let mut pane = SourcePane {
            status: "listening".into(),
            ..SourcePane::default()
        };

        assert!(pane.set_usage(UsageSnapshot {
            prompt_token_count: 653,
            cached_content_token_count: 0,
            response_token_count: 0,
            tool_use_prompt_token_count: 0,
            thoughts_token_count: 0,
            total_token_count: 0,
        }));
        assert!(pane.set_usage(UsageSnapshot {
            prompt_token_count: 0,
            cached_content_token_count: 0,
            response_token_count: 9,
            tool_use_prompt_token_count: 0,
            thoughts_token_count: 0,
            total_token_count: 9,
        }));

        assert_eq!(
            pane.title(SourceKind::Microphone),
            "microphone [listening | ctx 653]"
        );
    }
}

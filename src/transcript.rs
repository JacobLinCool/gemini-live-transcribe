use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::capture::SourceKind;

const DEFAULT_PRIMARY_LANGUAGE: &str = "zh-Hant";

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum NumeralPolicy {
    #[default]
    Preserve,
    Normalize,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TranscriptionConfig {
    pub primary_language: Option<String>,
    pub allowed_languages: Vec<String>,
    pub keep_disfluencies: bool,
    pub collapse_self_corrections: bool,
    pub dedupe_immediate_repetition: bool,
    pub numeral_policy: NumeralPolicy,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TranscriptionProfile {
    pub primary_language: String,
    pub allowed_languages: Vec<String>,
    pub keep_disfluencies: bool,
    pub collapse_self_corrections: bool,
    pub dedupe_immediate_repetition: bool,
    pub numeral_policy: NumeralPolicy,
}

impl Default for TranscriptionProfile {
    fn default() -> Self {
        Self {
            primary_language: DEFAULT_PRIMARY_LANGUAGE.to_owned(),
            allowed_languages: Vec::new(),
            keep_disfluencies: true,
            collapse_self_corrections: false,
            dedupe_immediate_repetition: false,
            numeral_policy: NumeralPolicy::Preserve,
        }
    }
}

impl TranscriptionProfile {
    pub fn from_config(config: Option<TranscriptionConfig>) -> Result<Self> {
        let Some(config) = config else {
            return Ok(Self::default());
        };

        let primary_language = sanitize_language(config.primary_language)
            .unwrap_or_else(|| DEFAULT_PRIMARY_LANGUAGE.to_owned());
        let mut allowed_languages = Vec::new();
        for language in config.allowed_languages {
            let Some(language) = sanitize_language(Some(language)) else {
                continue;
            };
            if language != primary_language && !allowed_languages.contains(&language) {
                allowed_languages.push(language);
            }
        }

        Ok(Self {
            primary_language,
            allowed_languages,
            keep_disfluencies: config.keep_disfluencies,
            collapse_self_corrections: config.collapse_self_corrections,
            dedupe_immediate_repetition: config.dedupe_immediate_repetition,
            numeral_policy: config.numeral_policy,
        })
    }

    pub fn summary(&self) -> String {
        if self.allowed_languages.is_empty() {
            format!("lang: {}", self.primary_language)
        } else {
            format!(
                "lang: {} | allow: {}",
                self.primary_language,
                self.allowed_languages.join(", ")
            )
        }
    }

    pub fn allowed_output_languages(&self) -> Vec<String> {
        let mut languages = Vec::with_capacity(1 + self.allowed_languages.len());
        languages.push(self.primary_language.clone());
        languages.extend(self.allowed_languages.iter().cloned());
        languages
    }

    pub fn finalizer_instruction(&self) -> String {
        let allowed = if self.allowed_languages.is_empty() {
            "none".to_owned()
        } else {
            self.allowed_languages.join(", ")
        };
        let output_languages = self.allowed_output_languages().join(", ");
        let numeral_policy = match self.numeral_policy {
            NumeralPolicy::Preserve => "preserve",
            NumeralPolicy::Normalize => "normalize",
        };

        format!(
            concat!(
                "You are a turn-level transcription finalizer.\n",
                "You may only respond by calling finalize_transcript.\n",
                "Produce a single final transcript text for the current audio turn.\n",
                "Primary output language: {primary}.\n",
                "Additional allowed languages that may remain as spoken: {allowed}.\n",
                "If speech is in any other language, translate that span into the primary output language.\n",
                "Prefer to keep text in the primary output language and the additional allowed languages.\n",
                "Set output_language as a best-effort hint for the dominant language of the final text.\n",
                "Preferred output_language values: {output_languages}.\n",
                "Keep disfluencies: {keep_disfluencies}.\n",
                "Collapse self corrections: {collapse_self_corrections}.\n",
                "Deduplicate immediate repetition: {dedupe_immediate_repetition}.\n",
                "Numeral policy: {numeral_policy}.\n",
                "Use prior turns only as context for understanding. Do not revise prior finalized turns.\n",
                "Do not output conversational text.\n",
                "If the audio is empty or only noise, set is_empty_or_noise to true and emit empty text."
            ),
            primary = self.primary_language,
            allowed = allowed,
            output_languages = output_languages,
            keep_disfluencies = bool_label(self.keep_disfluencies),
            collapse_self_corrections = bool_label(self.collapse_self_corrections),
            dedupe_immediate_repetition = bool_label(self.dedupe_immediate_repetition),
            numeral_policy = numeral_policy,
        )
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DisplayAction {
    NewBlock,
    AppendPreviousBlock,
}

#[derive(Debug, Clone)]
pub struct FinalizedTurnPayload {
    pub text: String,
    pub output_language: String,
    pub detected_languages: Vec<String>,
    pub is_empty_or_noise: bool,
    pub display_action: DisplayAction,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TurnState {
    Draft,
    Pending,
    Final,
    Failed,
    Unfinalized,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ExportedTurn {
    pub turn_id: String,
    pub turn_index: u64,
    pub state: TurnState,
    pub text: String,
    pub output_language: String,
    pub draft_text: String,
    pub detected_languages: Vec<String>,
    pub is_empty_or_noise: bool,
    pub display_action: DisplayAction,
    pub created_at_unix_ms: u128,
    pub finalized_at_unix_ms: Option<u128>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SourceTranscript {
    pub source: SourceKind,
    pub turns: Vec<ExportedTurn>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SessionTranscript {
    pub session_id: String,
    pub created_at_unix_ms: u128,
    pub ended_at_unix_ms: Option<u128>,
    pub draft_model: String,
    pub finalizer_model: String,
    pub transcription_profile: TranscriptionProfile,
    pub sources: Vec<SourceTranscript>,
}

#[derive(Debug)]
pub struct TranscriptStore {
    session: SessionTranscript,
}

impl TranscriptStore {
    pub fn new(
        session_id: String,
        draft_model: String,
        finalizer_model: String,
        transcription_profile: TranscriptionProfile,
        sources: Vec<SourceKind>,
    ) -> Self {
        let source_records = sources
            .into_iter()
            .map(|source| SourceTranscript {
                source,
                turns: Vec::new(),
            })
            .collect();

        Self {
            session: SessionTranscript {
                session_id,
                created_at_unix_ms: unix_timestamp_ms(),
                ended_at_unix_ms: None,
                draft_model,
                finalizer_model,
                transcription_profile,
                sources: source_records,
            },
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session.session_id
    }

    pub fn register_turn(
        &mut self,
        source: SourceKind,
        turn_id: &str,
        turn_index: u64,
        created_at_unix_ms: u128,
    ) {
        let turns = &mut self.source_mut(source).turns;
        if turns.iter().any(|turn| turn.turn_id == turn_id) {
            return;
        }

        turns.push(ExportedTurn {
            turn_id: turn_id.to_owned(),
            turn_index,
            state: TurnState::Draft,
            text: String::new(),
            output_language: String::new(),
            draft_text: String::new(),
            detected_languages: Vec::new(),
            is_empty_or_noise: false,
            display_action: DisplayAction::NewBlock,
            created_at_unix_ms,
            finalized_at_unix_ms: None,
            error: None,
        });
    }

    pub fn update_draft(&mut self, source: SourceKind, turn_id: &str, draft_text: String) {
        let turn = self.turn_mut(source, turn_id);
        turn.draft_text = draft_text;
    }

    pub fn mark_pending(&mut self, source: SourceKind, turn_id: &str) {
        let turn = self.turn_mut(source, turn_id);
        turn.state = TurnState::Pending;
    }

    pub fn mark_final(
        &mut self,
        source: SourceKind,
        turn_id: &str,
        payload: FinalizedTurnPayload,
        finalized_at_unix_ms: u128,
    ) {
        let turn = self.turn_mut(source, turn_id);
        turn.state = TurnState::Final;
        turn.text = payload.text;
        turn.output_language = payload.output_language;
        turn.detected_languages = payload.detected_languages;
        turn.is_empty_or_noise = payload.is_empty_or_noise;
        turn.display_action = payload.display_action;
        turn.finalized_at_unix_ms = Some(finalized_at_unix_ms);
        turn.error = None;
    }

    pub fn mark_failed(&mut self, source: SourceKind, turn_id: &str, error: String) {
        let turn = self.turn_mut(source, turn_id);
        turn.state = TurnState::Failed;
        turn.error = Some(error);
        turn.finalized_at_unix_ms = Some(unix_timestamp_ms());
    }

    pub fn mark_unfinalized(&mut self, source: SourceKind, turn_id: &str) {
        let turn = self.turn_mut(source, turn_id);
        turn.state = TurnState::Unfinalized;
        if turn.text.is_empty() {
            turn.text = turn.draft_text.clone();
        }
    }

    pub fn finish_session(&mut self) {
        self.session.ended_at_unix_ms = Some(unix_timestamp_ms());
    }

    pub fn write_json(&mut self, path: &Path) -> Result<()> {
        self.finish_session();

        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("transcript path has no parent: {}", path.display()))?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create transcript directory {}", parent.display()))?;
        let serialized =
            serde_json::to_vec_pretty(&self.session).context("serialize transcript")?;
        std::fs::write(path, serialized)
            .with_context(|| format!("write transcript export {}", path.display()))
    }

    fn source_mut(&mut self, source: SourceKind) -> &mut SourceTranscript {
        self.session
            .sources
            .iter_mut()
            .find(|entry| entry.source == source)
            .expect("source should exist in transcript store")
    }

    fn turn_mut(&mut self, source: SourceKind, turn_id: &str) -> &mut ExportedTurn {
        self.source_mut(source)
            .turns
            .iter_mut()
            .find(|turn| turn.turn_id == turn_id)
            .expect("turn should exist in transcript store")
    }
}

fn sanitize_language(language: Option<String>) -> Option<String> {
    let language = language?;
    let trimmed = language.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn bool_label(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

pub fn validate_export_path(path: &Path) -> Result<()> {
    if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
        bail!("transcript export path must end with .json");
    }
    Ok(())
}

pub fn unix_timestamp_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_millis()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        DisplayAction, FinalizedTurnPayload, NumeralPolicy, TranscriptStore, TranscriptionConfig,
        TranscriptionProfile, TurnState, validate_export_path,
    };
    use crate::capture::SourceKind;

    #[test]
    fn builds_default_profile() {
        let profile = TranscriptionProfile::from_config(None).expect("default should build");

        assert_eq!(profile.primary_language, "zh-Hant");
        assert!(profile.allowed_languages.is_empty());
        assert!(profile.keep_disfluencies);
        assert_eq!(profile.numeral_policy, NumeralPolicy::Preserve);
    }

    #[test]
    fn normalizes_transcription_profile_languages() {
        let profile = TranscriptionProfile::from_config(Some(TranscriptionConfig {
            primary_language: Some(" en ".into()),
            allowed_languages: vec!["zh-Hant".into(), "en".into(), " zh-Hant ".into()],
            keep_disfluencies: false,
            collapse_self_corrections: true,
            dedupe_immediate_repetition: true,
            numeral_policy: NumeralPolicy::Normalize,
        }))
        .expect("profile should build");

        assert_eq!(profile.primary_language, "en");
        assert_eq!(profile.allowed_languages, vec!["zh-Hant".to_string()]);
        assert!(!profile.keep_disfluencies);
        assert!(profile.collapse_self_corrections);
        assert!(profile.dedupe_immediate_repetition);
        assert_eq!(profile.numeral_policy, NumeralPolicy::Normalize);
    }

    #[test]
    fn stores_turn_lifecycle_for_export() {
        let mut store = TranscriptStore::new(
            "session".into(),
            "draft".into(),
            "finalizer".into(),
            TranscriptionProfile::default(),
            vec![SourceKind::Microphone],
        );

        store.register_turn(SourceKind::Microphone, "turn-1", 1, 10);
        store.update_draft(SourceKind::Microphone, "turn-1", "hello".into());
        store.mark_pending(SourceKind::Microphone, "turn-1");
        store.mark_final(
            SourceKind::Microphone,
            "turn-1",
            FinalizedTurnPayload {
                text: "hello world".into(),
                output_language: "en".into(),
                detected_languages: vec!["en".into()],
                is_empty_or_noise: false,
                display_action: DisplayAction::NewBlock,
            },
            20,
        );

        let source = &store.session.sources[0];
        assert_eq!(source.turns.len(), 1);
        assert_eq!(source.turns[0].state, TurnState::Final);
        assert_eq!(source.turns[0].draft_text, "hello");
        assert_eq!(source.turns[0].text, "hello world");
        assert_eq!(source.turns[0].output_language, "en");
    }

    #[test]
    fn marks_unfinalized_turns_with_draft_text() {
        let mut store = TranscriptStore::new(
            "session".into(),
            "draft".into(),
            "finalizer".into(),
            TranscriptionProfile::default(),
            vec![SourceKind::Microphone],
        );

        store.register_turn(SourceKind::Microphone, "turn-1", 1, 10);
        store.update_draft(SourceKind::Microphone, "turn-1", "draft".into());
        store.mark_unfinalized(SourceKind::Microphone, "turn-1");

        let turn = &store.session.sources[0].turns[0];
        assert_eq!(turn.state, TurnState::Unfinalized);
        assert_eq!(turn.text, "draft");
    }

    #[test]
    fn requires_json_export_path() {
        validate_export_path(Path::new("/tmp/transcript.json")).expect("json path should pass");
        let error = validate_export_path(Path::new("/tmp/transcript.md"))
            .expect_err("non-json path should fail");
        assert!(error.to_string().contains(".json"));
    }
}

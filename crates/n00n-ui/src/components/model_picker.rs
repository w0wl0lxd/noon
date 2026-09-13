use std::sync::Arc;

use arc_swap::ArcSwapOption;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};

use n00n_providers::ModelTier;
use n00n_providers::ThinkingConfig;
use n00n_providers::dynamic;
use n00n_providers::model_registry;
use n00n_providers::provider::ProviderKind;

use crate::components::Overlay;
use crate::components::keybindings::key::THINKING_ALT;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::theme;

const TITLE: &str = " Models ";
const RECENT_SECTION: &str = "Recent";

fn footer_line() -> Line<'static> {
    let t = theme::current();
    Line::from(vec![
        Span::styled("  Enter", t.keybind_key),
        Span::styled(" select", t.tool_dim),
        Span::styled("  !", t.keybind_key),
        Span::styled(" strong", t.tool_dim),
        Span::styled("  @", t.keybind_key),
        Span::styled(" medium", t.tool_dim),
        Span::styled("  #", t.keybind_key),
        Span::styled(" weak", t.tool_dim),
        Span::styled("  $", t.keybind_key),
        Span::styled(" compaction", t.tool_dim),
        Span::styled(format!("  {}", THINKING_ALT.label), t.keybind_key),
        Span::styled(" thinking", t.tool_dim),
    ])
}

fn tier_for_shortcut(key: KeyEvent) -> Option<ModelTier> {
    let digit = match (key.code, key.modifiers.contains(KeyModifiers::SHIFT)) {
        // Kitty protocol: Shift+digit reported with base key + SHIFT modifier
        (KeyCode::Char(c @ '1'..='4'), true) => c,
        // Legacy terminals: Shift+digit reported as the resulting character
        (KeyCode::Char('!' | '¡'), false) => '1', // US, ES
        (KeyCode::Char('@' | '"' | '™'), false) => '2', // US, UK/DE
        (KeyCode::Char('#' | '§' | '£'), false) => '3', // US, DE, UK
        (KeyCode::Char('$' | '€' | '¤'), false) => '4', // US, EU, Nordic
        _ => return None,
    };
    match digit {
        '1' => Some(ModelTier::Strong),
        '2' => Some(ModelTier::Medium),
        '3' => Some(ModelTier::Weak),
        '4' => Some(ModelTier::Compaction),
        _ => None,
    }
}

pub enum ModelPickerAction {
    Consumed,
    Select(String),
    AssignTier(String, ModelTier),
    UnassignTier(String, ModelTier),
    CycleThinking(String),
    Close,
}

struct ModelEntry {
    spec: String,
    id: String,
    name: Option<String>,
    provider_display: String,
    suffix: Option<String>,
    detail: String,
    override_tiers: Vec<ModelTier>,
}

impl PickerItem for ModelEntry {
    fn label(&self) -> &str {
        self.name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| &self.id)
    }

    fn suffix(&self) -> Option<&str> {
        self.suffix.as_deref()
    }

    fn detail(&self) -> Option<&str> {
        Some(&self.detail)
    }

    fn section(&self) -> Option<&str> {
        Some(self.provider_display.as_str())
    }

    fn is_highlighted(&self) -> bool {
        !self.override_tiers.is_empty()
    }
}

pub struct ModelPicker {
    picker: ListPicker<ModelEntry>,
    models: Arc<ArcSwapOption<Vec<String>>>,
    recents: Vec<String>,
    current_spec: String,
    // The specs list the open picker was last built from. A model refresh swaps
    // in a fresh `Arc`, so comparing identity (not just the length) catches
    // same-size replacements that only change which models are present. Holding
    // a strong clone keeps the pointer comparison sound by preventing the old
    // allocation from being freed and reused at the same address.
    last_specs: Option<Arc<Vec<String>>>,
    dirty: bool,
}

impl ModelPicker {
    pub fn new(models: Arc<ArcSwapOption<Vec<String>>>) -> Self {
        Self {
            picker: ListPicker::new().with_footer_builder(footer_line),
            models,
            recents: Vec::new(),
            current_spec: String::new(),
            last_specs: None,
            dirty: false,
        }
    }

    pub fn set_recents(&mut self, recents: Vec<String>) {
        self.recents = recents;
        self.dirty = true;
    }

    pub fn open(&mut self, current_spec: &str) {
        current_spec.clone_into(&mut self.current_spec);
        let (entries, idx) = self.load_entries();
        self.picker.open(entries, TITLE);
        self.picker.select(idx);
    }

    fn try_refresh(&mut self) {
        if !self.picker.is_open() {
            return;
        }
        let guard = self.models.load();
        let unchanged = match (guard.as_ref(), self.last_specs.as_ref()) {
            (Some(current), Some(last)) => Arc::ptr_eq(current, last),
            (None, None) => true,
            _ => false,
        };
        if unchanged && !self.dirty {
            return;
        }
        drop(guard);
        self.dirty = false;
        let (entries, idx) = self.load_entries();
        self.picker.replace_items(entries);
        self.picker.select(idx);
    }

    fn load_entries(&mut self) -> (Vec<ModelEntry>, usize) {
        let guard = self.models.load();
        self.last_specs = guard.as_ref().map(Arc::clone);
        let specs = guard.as_deref();
        let mut entries: Vec<ModelEntry> = Vec::new();
        let recent_specs = self.recents.clone();
        for spec in &recent_specs {
            if let Some(mut e) = parse_model_entry(spec) {
                e.suffix = Some(std::mem::take(&mut e.provider_display));
                e.provider_display = RECENT_SECTION.to_string();
                entries.push(e);
            }
        }
        let full: Vec<ModelEntry> = specs.map_or_else(Default::default, |s| {
            s.iter().filter_map(|s| parse_model_entry(s)).collect()
        });
        entries.extend(full);
        let idx = entries
            .iter()
            .position(|e| e.spec == self.current_spec)
            .unwrap_or_else(|| 0);
        (entries, idx)
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
        self.picker.close();
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.picker.contains(pos)
    }

    pub fn scroll(&mut self, delta: i32) {
        self.picker.scroll(delta);
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.picker.handle_paste(text)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ModelPickerAction {
        if THINKING_ALT.matches(key) {
            let Some(entry) = self.picker.selected_item() else {
                return ModelPickerAction::Consumed;
            };
            self.dirty = true;
            return ModelPickerAction::CycleThinking(entry.spec.clone());
        }
        if let Some(tier) = tier_for_shortcut(key)
            && let Some(entry) = self.picker.selected_item()
        {
            let spec = entry.spec.clone();
            self.dirty = true;
            if entry.override_tiers.contains(&tier) {
                return ModelPickerAction::UnassignTier(spec, tier);
            }
            return ModelPickerAction::AssignTier(spec, tier);
        }
        match self.picker.handle_key(key) {
            PickerAction::Select(_, entry) => ModelPickerAction::Select(entry.spec),
            PickerAction::Close => ModelPickerAction::Close,
            PickerAction::Consumed | PickerAction::Toggle(..) => ModelPickerAction::Consumed,
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.try_refresh();
        self.picker.view(frame, area)
    }
}

impl Overlay for ModelPicker {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close();
    }
}

fn parse_model_entry(spec: &str) -> Option<ModelEntry> {
    let (provider_str, model_id) = spec.split_once('/')?;

    let provider_display = if let Ok(kind) = provider_str.parse::<ProviderKind>() {
        kind.display_name().to_string()
    } else if let Some(name) = dynamic::display_name(provider_str) {
        name.to_string()
    } else {
        let config = n00n_config::providers::ProvidersConfig::load_or_exit();
        config.get(provider_str)?;
        n00n_config::providers::resolve_display_name(provider_str, config.get(provider_str))
    };

    let map = model_registry::model_registry()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let name = map
        .discovered(provider_str, model_id)
        .and_then(|info| info.name.clone());
    let override_tiers: Vec<ModelTier> = [
        ModelTier::Strong,
        ModelTier::Medium,
        ModelTier::Weak,
        ModelTier::Compaction,
    ]
    .into_iter()
    .filter(|&t| map.has_override(spec, t))
    .collect();
    let override_label = map.override_tier_label(spec);
    let remembered = map.remembered_thinking(spec);
    drop(map);
    let tier = override_label.unwrap_or_else(|| match n00n_providers::Model::from_spec(spec) {
        Ok(m) => m.tier.to_string(),
        Err(_) => String::new(),
    });
    let detail = match remembered {
        Some(stored) if tier.is_empty() => ThinkingConfig::from(stored).to_string(),
        Some(stored) => format!("{tier} · {}", ThinkingConfig::from(stored)),
        None => tier,
    };
    Some(ModelEntry {
        spec: spec.to_string(),
        id: model_id.to_string(),
        name,
        provider_display,
        suffix: None,
        detail,
        override_tiers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key;
    use crate::components::keybindings::key as kb;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use test_case::test_case;

    fn test_models() -> Arc<ArcSwapOption<Vec<String>>> {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        models
    }

    #[test_case(key(KeyCode::Esc)          ; "esc_closes")]
    #[test_case(kb::QUIT.to_key_event()    ; "ctrl_c_closes")]
    fn close_keys(cancel_key: KeyEvent) {
        let mut p = ModelPicker::new(test_models());
        p.open("");
        let action = p.handle_key(cancel_key);
        assert!(matches!(action, ModelPickerAction::Close));
        assert!(!p.is_open());
    }

    #[test]
    fn refresh_updates_items_and_preserves_search() {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
        ])));
        let mut p = ModelPicker::new(Arc::clone(&models));
        p.open("");

        p.handle_key(key(KeyCode::Char('o')));
        p.handle_key(key(KeyCode::Char('p')));

        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
        ])));
        p.try_refresh();

        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s.contains("opus")),
            "after refresh, 'op' filter should match opus"
        );
    }

    #[test]
    fn refresh_detects_same_size_replacement() {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
        ])));
        let mut p = ModelPicker::new(Arc::clone(&models));
        p.open("");

        // A refresh that yields the same number of specs but a different model
        // must still rebuild the open picker.
        models.store(Some(Arc::new(vec![
            "anthropic/claude-opus-4-6-20260101".into(),
        ])));
        p.try_refresh();

        p.handle_key(key(KeyCode::Char('o')));
        p.handle_key(key(KeyCode::Char('p')));
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s.contains("opus")),
            "same-size refresh should surface the newly present model"
        );
    }

    #[test]
    fn open_preselects_current_model() {
        let mut p = ModelPicker::new(test_models());
        p.open("anthropic/claude-opus-4-6-20260101");
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == "anthropic/claude-opus-4-6-20260101")
        );
    }

    #[test]
    fn parse_model_entry_valid() {
        let entry = parse_model_entry("anthropic/claude-sonnet-4-20250514").unwrap();
        assert_eq!(entry.id, "claude-sonnet-4-20250514");
        assert_eq!(entry.provider_display, "Anthropic");
        assert!(!entry.detail.is_empty());
    }

    #[test]
    fn parse_model_entry_no_slash() {
        assert!(parse_model_entry("no-slash").is_none());
    }

    #[test]
    fn alt_t_cycles_thinking_and_keeps_picker_open() {
        let mut p = ModelPicker::new(test_models());
        p.open("");
        let action = p.handle_key(kb::THINKING_ALT.to_key_event());
        assert!(matches!(action, ModelPickerAction::CycleThinking(_)));
        assert!(p.is_open());
    }

    #[test_case(key(KeyCode::Char('!')),           ModelTier::Strong     ; "legacy_bang_strong")]
    #[test_case(key(KeyCode::Char('$')),           ModelTier::Compaction ; "legacy_dollar_compaction")]
    #[test_case(key(KeyCode::Char('€')),           ModelTier::Compaction ; "legacy_euro_compaction")]
    #[test_case(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::SHIFT), ModelTier::Strong     ; "kitty_shift_1_strong")]
    #[test_case(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::SHIFT), ModelTier::Compaction ; "kitty_shift_4_compaction")]
    fn tier_shortcut_assigns_and_keeps_picker_open(k: KeyEvent, want: ModelTier) {
        let mut p = ModelPicker::new(test_models());
        p.open("");
        let action = p.handle_key(k);
        assert!(
            matches!(&action, ModelPickerAction::AssignTier(s, t)
                if s == "anthropic/claude-sonnet-4-20250514" && *t == want),
            "expected AssignTier(claude-sonnet, {want:?}), got something else",
        );
        assert!(p.is_open());
    }

    #[test]
    fn refresh_preserves_selection_for_current_model() {
        let models = Arc::new(ArcSwapOption::empty());
        let mut p = ModelPicker::new(Arc::clone(&models));
        p.open("anthropic/claude-opus-4-6-20260101");

        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        p.try_refresh();

        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == "anthropic/claude-opus-4-6-20260101"),
            "after async model arrival, current model should still be selected"
        );
    }

    #[test]
    fn recents_include_current_model_preselected() {
        let models = test_models();
        let mut p = ModelPicker::new(models);
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-opus-4-6-20260101");

        p.picker.select(0);
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == "zai/glm-5"),
            "first entry should be the most recent model",
        );

        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("zai/glm-5");
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == "zai/glm-5"),
            "current model should be preselected within Recent",
        );
    }
}

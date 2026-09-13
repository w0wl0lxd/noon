use std::collections::HashMap;
use std::sync::Arc;

use n00n_providers::System;
use strum::{Display, EnumIter, EnumString, IntoEnumIterator};
use tracing::warn;

pub trait ValidNames: IntoEnumIterator + std::fmt::Display {
    #[must_use]
    fn valid_names() -> String {
        Self::iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub const SYSTEM_PROMPT: &str = include_str!("prompts/system.md");
pub const PLAN_PROMPT: &str = include_str!("prompts/plan.md");
pub const RESEARCH_PROMPT: &str = include_str!("prompts/research.md");
pub const GENERAL_PROMPT: &str = include_str!("prompts/general.md");
pub const COMPACTION_SYSTEM: &str = include_str!("prompts/compaction.md");
pub const COMPACTION_USER: &str = include_str!("prompts/compaction_user.md");

pub const DEFAULT_IDENTITY: &str = r"You are n00n, an interactive CLI coding agent. Use the tools available to assist the user with software engineering tasks. Complete tasks successfully while minimizing token usage and tool calls to avoid context bloat.

You must NEVER generate or guess URLs unless they are for helping the user with programming.";

pub const DEFAULT_TONE: &str = r"- Be concise. Your output is displayed on a CLI rendered in monospace. Use GitHub-flavored markdown.
- Only use emojis if explicitly requested.
- Do not add comments to code unless asked.
- Output text to communicate with the user; all text you output outside of tool use is displayed to the user. Only use tools to complete tasks. NEVER use bash echo or other command-line tools to communicate thoughts, explanations, diagrams, or instructions to the user. Output all communication directly in your response text instead.
- NEVER create files unless absolutely necessary. ALWAYS prefer editing existing files.";

const NATIVE_EFFICIENT_TOOLS: &[&str] = &["explore_code", "index_file", "run_batch", "run_python"];
const INSTRUCTIONS_MARKER: &str = "{{instructions}}";
/// Max bytes for dynamic todo injection via `AfterInstructions`.
/// Todos ride as `System::Dynamic` which is never `cache_read` (see `assemble_system`), so every byte is billed as input.
/// At `CHARS_PER_TOKEN=4` (`scripts/tool_token_analysis.py`) this is ~512 tokens. Capped by truncating oldest `pending` first, keeping `in_progress`.
/// History injection via `compaction_state` would still be dynamic tail and lose visibility after `History` truncation; `System` Dynamic preserves visibility and survives compaction via plugin state.
const MAX_AFTER_INSTRUCTIONS_BYTES: usize = 2048;

/// Singleton: alphabetically last plugin wins, discarding all prior content
/// and built-in defaults.  Used for slots with opinionated defaults where
/// multiple contributors would conflict (identity, tone).
///
/// Aggregate: all entries are joined.  Used for genuinely additive slots
/// where multiple plugins contributing is the point (tool usage hints,
/// efficient tools, after-instructions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, Display)]
#[strum(serialize_all = "snake_case")]
pub enum SlotKind {
    Singleton,
    Aggregate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, Display, EnumIter)]
#[strum(serialize_all = "snake_case")]
pub enum Slot {
    Identity,
    Tone,
    Environment,
    ToolUsage,
    EfficientTools,
    Conventions,
    AfterInstructions,
}

impl Slot {
    fn marker(self) -> &'static str {
        match self {
            Slot::Identity => "{{identity}}",
            Slot::Tone => "{{tone}}",
            Slot::Environment => "{{environment}}",
            Slot::ToolUsage => "{{tool_usage}}",
            Slot::EfficientTools => "{{efficient_tools}}",
            Slot::Conventions => "{{conventions}}",
            Slot::AfterInstructions => "{{after_instructions}}",
        }
    }

    #[must_use]
    pub fn kind(self) -> SlotKind {
        match self {
            Slot::Identity | Slot::Tone | Slot::Environment => SlotKind::Singleton,
            Slot::ToolUsage
            | Slot::EfficientTools
            | Slot::Conventions
            | Slot::AfterInstructions => SlotKind::Aggregate,
        }
    }

    /// Built-in default content for singleton slots.  When no plugin
    /// registers content for a singleton slot, the default is used.
    /// Aggregate slots have no default (the template carries the static
    /// text around the marker).
    #[must_use]
    pub fn default_content(self) -> Option<&'static str> {
        match self {
            Slot::Identity => Some(DEFAULT_IDENTITY),
            Slot::Tone => Some(DEFAULT_TONE),
            _ => None,
        }
    }

    #[must_use]
    pub fn names_for_kind(kind: SlotKind) -> String {
        Self::iter()
            .filter(|s| s.kind() == kind)
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, Display, EnumIter)]
#[strum(serialize_all = "snake_case")]
pub enum PromptId {
    System,
    Research,
    General,
}

impl PromptId {
    pub const ALL: &[PromptId] = &[PromptId::System, PromptId::Research, PromptId::General];
}

impl ValidNames for Slot {}
impl ValidNames for PromptId {}

pub struct SlotEntry {
    pub plugin: Arc<str>,
    pub content: String,
}

#[derive(Default)]
pub struct ResolvedSlots {
    entries: HashMap<(PromptId, Slot), Vec<SlotEntry>>,
}

impl ResolvedSlots {
    #[must_use]
    pub fn get(&self, prompt: PromptId, slot: Slot) -> &[SlotEntry] {
        self.entries
            .get(&(prompt, slot))
            .map_or(&[], std::vec::Vec::as_slice)
    }

    pub fn insert(&mut self, prompt: PromptId, slot: Slot, entry: SlotEntry) {
        self.entries.entry((prompt, slot)).or_default().push(entry);
    }
}

impl PromptId {
    fn template(self) -> &'static str {
        match self {
            PromptId::System => SYSTEM_PROMPT,
            PromptId::Research => RESEARCH_PROMPT,
            PromptId::General => GENERAL_PROMPT,
        }
    }

    /// A slot exists for this prompt iff its marker is present in the template.
    /// Markers that are absent get no content (and we warn at collection time
    /// when a plugin targets them explicitly).
    #[must_use]
    pub fn has_slot(self, slot: Slot) -> bool {
        self.template().contains(slot.marker())
    }
}

fn render_slot(slots: &ResolvedSlots, prompt: PromptId, slot: Slot) -> String {
    if slot == Slot::EfficientTools {
        return render_efficient_tools(slots, prompt);
    }
    let entries = slots.get(prompt, slot);
    match slot.kind() {
        SlotKind::Singleton => {
            if let Some(last) = entries.last() {
                last.content.clone()
            } else if let Some(default) = slot.default_content() {
                default.to_string()
            } else {
                String::new()
            }
        }
        // Aggregate slots have no built-in defaults; content comes entirely from plugins.
        SlotKind::Aggregate => {
            let mut parts = Vec::new();
            for entry in entries {
                parts.push(entry.content.as_str());
            }
            let joined = parts.join("\n");
            if slot == Slot::AfterInstructions {
                cap_after_instructions(joined)
            } else {
                joined
            }
        }
    }
}

fn render_efficient_tools(slots: &ResolvedSlots, prompt: PromptId) -> String {
    let extras = slots.get(prompt, Slot::EfficientTools);
    let names = NATIVE_EFFICIENT_TOOLS
        .iter()
        .copied()
        .chain(extras.iter().map(|e| e.content.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    format!("Most efficient tools: {names}.")
}

fn cap_after_instructions(content: String) -> String {
    if content.len() <= MAX_AFTER_INSTRUCTIONS_BYTES {
        return content;
    }
    if !content.contains("# Current todos") {
        let truncated =
            crate::tools::truncate_output(&content, usize::MAX, MAX_AFTER_INSTRUCTIONS_BYTES);
        warn!(
            tool = "AfterInstructions",
            path = "",
            original_bytes = content.len(),
            truncated_bytes = truncated.len(),
            "truncated AfterInstructions to cap"
        );
        return truncated;
    }
    let lines: Vec<&str> = content.lines().collect();
    let mut header_end = 0;
    for (idx, line) in lines.iter().enumerate() {
        if line.trim_start().starts_with('{') {
            break;
        }
        header_end = idx + 1;
    }
    let header = lines[..header_end].join("\n");
    let mut entries: Vec<(String, String)> = Vec::new();
    for line in &lines[header_end..] {
        if line.trim().is_empty() {
            continue;
        }
        let status = match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) => v
                .get("status")
                .and_then(|s| s.as_str())
                .map_or_else(|| "pending".to_string(), std::string::ToString::to_string),
            Err(error) => {
                warn!(error = %error, "malformed todo line in AfterInstructions; treating as pending");
                "pending".to_string()
            }
        };
        entries.push(((*line).to_string(), status));
    }
    if entries.is_empty() {
        let truncated =
            crate::tools::truncate_output(&content, usize::MAX, MAX_AFTER_INSTRUCTIONS_BYTES);
        warn!(
            tool = "AfterInstructions",
            path = "",
            original_bytes = content.len(),
            truncated_bytes = truncated.len(),
            "truncated AfterInstructions to cap"
        );
        return truncated;
    }
    let mut total: usize = header.len() + entries.iter().map(|(l, _)| l.len() + 1).sum::<usize>();
    if total <= MAX_AFTER_INSTRUCTIONS_BYTES {
        let mut out = header;
        for (line, _) in &entries {
            out.push('\n');
            out.push_str(line);
        }
        return out;
    }
    let original_bytes = content.len();
    let mut keep = vec![true; entries.len()];
    let pending_indices: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter_map(|(idx, (_, status))| if status == "pending" { Some(idx) } else { None })
        .collect();
    let mut pending_pos = 0;
    while total > MAX_AFTER_INSTRUCTIONS_BYTES && pending_pos < pending_indices.len() {
        let idx = pending_indices[pending_pos];
        if keep[idx] {
            total = total.saturating_sub(entries[idx].0.len() + 1);
            keep[idx] = false;
        }
        pending_pos += 1;
    }
    if total > MAX_AFTER_INSTRUCTIONS_BYTES {
        let fallback: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter_map(|(idx, (_, status))| {
                if keep[idx] && status != "in_progress" {
                    Some(idx)
                } else {
                    None
                }
            })
            .collect();
        let mut fallback_pos = 0;
        while total > MAX_AFTER_INSTRUCTIONS_BYTES && fallback_pos < fallback.len() {
            let idx = fallback[fallback_pos];
            total = total.saturating_sub(entries[idx].0.len() + 1);
            keep[idx] = false;
            fallback_pos += 1;
        }
    }
    if total > MAX_AFTER_INSTRUCTIONS_BYTES {
        for (idx, (line, _)) in entries.iter_mut().enumerate() {
            if !keep[idx] || total <= MAX_AFTER_INSTRUCTIONS_BYTES {
                continue;
            }
            let avail =
                MAX_AFTER_INSTRUCTIONS_BYTES.saturating_sub(total.saturating_sub(line.len()));
            if let Some(shrunk) = shrink_todo_line(line, avail) {
                total = total.saturating_sub(line.len()) + shrunk.len();
                *line = shrunk;
            } else {
                total = total.saturating_sub(line.len() + 1);
                keep[idx] = false;
            }
        }
    }
    let header_len = header.len();
    let mut out = header;
    for (idx, (line, _)) in entries.into_iter().enumerate() {
        if keep[idx] {
            out.push('\n');
            out.push_str(&line);
        }
    }
    if out.len() > MAX_AFTER_INSTRUCTIONS_BYTES {
        // Never emit a partially cut line: the injected todo payload must stay
        // well-formed JSON. Drop trailing whole lines until the block fits.
        while out.len() > MAX_AFTER_INSTRUCTIONS_BYTES
            && let Some(nl) = out.rfind('\n')
            && nl >= header_len
        {
            out.truncate(nl);
        }
        if out.len() > MAX_AFTER_INSTRUCTIONS_BYTES {
            // Only the header region remains (other plugins' hint text sits
            // before the first JSON line). It is prose, not a todo payload,
            // so a hard char-boundary cut is safe and keeps the cap absolute.
            out.truncate(out.floor_char_boundary(MAX_AFTER_INSTRUCTIONS_BYTES));
        }
    }
    warn!(
        tool = "AfterInstructions",
        path = "",
        original_bytes,
        truncated_bytes = out.len(),
        "truncated AfterInstructions todo budget"
    );
    out
}

/// Shrink one serialized todo line to fit `avail` bytes. Returns `None` when the
/// line cannot fit even with an empty content payload.
fn shrink_todo_line(line: &str, avail: usize) -> Option<String> {
    if avail == 0 {
        return None;
    }
    if line.len() <= avail {
        return Some(line.to_string());
    }
    if let Ok(mut val) = serde_json::from_str::<serde_json::Value>(line)
        && let Some(original) = val
            .get("content")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    {
        // JSON escaping makes the re-encoded line longer than the decoded
        // content suggests, so size against the serialized form. Encoded
        // length is monotonic in the decoded prefix, hence binary search.
        // Search over char-boundary indices: lo/hi are positions in
        // `boundaries`, so each step strictly shrinks the range and the loop
        // cannot stall inside a multi-byte character.
        let boundaries: Vec<usize> = original
            .char_indices()
            .map(|(idx, _)| idx)
            .chain(std::iter::once(original.len()))
            .collect();
        let mut lo = 0usize;
        let mut hi = boundaries.len() - 1;
        let mut best = None;
        while lo <= hi {
            let idx = lo.midpoint(hi);
            let mid = boundaries[idx];
            if let Some(slot) = val.get_mut("content") {
                *slot = serde_json::Value::String(format!("{}...", &original[..mid]));
            }
            let Ok(candidate) = serde_json::to_string(&val) else {
                warn!("todo line re-serialization failed; dropping entry");
                return None;
            };
            if candidate.len() <= avail {
                best = Some(candidate);
                lo = idx + 1;
            } else if idx == 0 {
                break;
            } else {
                hi = idx - 1;
            }
        }
        return best;
    }
    if avail < 4 {
        return None;
    }
    let boundary = line.floor_char_boundary(avail - 3);
    Some(format!("{}...", &line[..boundary]))
}

/// Fill each `{{slot}}` marker in the template with its rendered content and
/// drop the project instructions (AGENTS.md and friends) into `{{instructions}}`.
#[must_use]
pub fn assemble(id: PromptId, slots: &ResolvedSlots, instructions: &str) -> String {
    let mut out = id.template().to_string();
    for slot in Slot::iter() {
        out = fill_marker(&out, slot.marker(), &render_slot(slots, id, slot));
    }
    out.replace(INSTRUCTIONS_MARKER, instructions)
}

/// Replace a slot marker with its content. When the content is empty, also drop
/// the marker's own line (the trailing newline) so empty slots leave no blank
/// gap, without touching any other whitespace in the prompt.
fn fill_marker(template: &str, marker: &str, content: &str) -> String {
    if content.is_empty() {
        return template
            .replace(&format!("{marker}\n"), "")
            .replace(marker, "");
    }
    template.replace(marker, content)
}

/// Build a `System` prompt from the template, grouping static content before
/// dynamic slots (`environment`, `after_instructions`) so providers can cache the
/// reusable prefix.
///
/// `AfterInstructions` is `Dynamic` and never `cache_read` — every byte is billed as input (see `System::cacheable_prefix_blocks`).
/// We cap it to `MAX_AFTER_INSTRUCTIONS_BYTES` (2048 bytes ≈512 tokens at 4 chars/token) by dropping oldest `pending` todos first, keeping `in_progress`.
/// Moving todos to history via `compaction_state` would still be a dynamic tail, lose visibility after `History::truncate`, and not improve cache hit rate; plugin state + `System` Dynamic is the correct layer.
#[must_use]
pub fn assemble_system(id: PromptId, slots: &ResolvedSlots, instructions: &str) -> System {
    let template = id.template();
    let mut system = System::new();
    let mut pending = String::new();
    let mut markers: Vec<(usize, &str, Option<Slot>)> = Vec::new();

    if template.contains(INSTRUCTIONS_MARKER) {
        for (pos, _) in template.match_indices(INSTRUCTIONS_MARKER) {
            markers.push((pos, INSTRUCTIONS_MARKER, None));
        }
    }

    for slot in Slot::iter() {
        if !id.has_slot(slot) {
            continue;
        }
        let marker = slot.marker();
        for (pos, _) in template.match_indices(marker) {
            markers.push((pos, marker, Some(slot)));
        }
    }

    markers.sort_by_key(|(pos, _, _)| *pos);

    let mut last_end = 0;
    for (pos, marker, slot) in markers {
        pending.push_str(&template[last_end..pos]);

        let content = match slot {
            None => instructions.to_string(),
            Some(s) => render_slot(slots, id, s),
        };

        let is_dynamic = slot.map_or(false, |s| {
            matches!(s, Slot::Environment | Slot::AfterInstructions)
        });

        if content.is_empty() {
            if is_dynamic {
                if !pending.is_empty() {
                    system.push_static(pending.clone());
                }
                pending.clear();
                system.mark_dynamic_boundary();
            }
            let marker_end = pos + marker.len();
            last_end = if template[marker_end..].starts_with('\n') {
                marker_end + 1
            } else {
                marker_end
            };
            continue;
        }

        if is_dynamic {
            if !pending.is_empty() {
                system.push_static(pending.clone());
            }
            pending.clear();
            system.push_dynamic(content);
        } else {
            pending.push_str(&content);
        }

        last_end = pos + marker.len();
    }

    pending.push_str(&template[last_end..]);
    if !pending.is_empty() {
        system.push_static(pending);
    }

    system.seal();
    system
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const NATIVE_EFFICIENT_LINE: &str =
        "Most efficient tools: explore_code, index_file, run_batch, run_python";

    fn slots(prompt: PromptId, entries: &[(Slot, &str)]) -> ResolvedSlots {
        let mut slots = ResolvedSlots::default();
        for &(slot, content) in entries {
            slots.insert(
                prompt,
                slot,
                SlotEntry {
                    plugin: Arc::from("p"),
                    content: content.into(),
                },
            );
        }
        slots
    }

    fn at(out: &str, needle: &str) -> usize {
        out.find(needle)
            .unwrap_or_else(|| panic!("missing: {needle}"))
    }

    #[test]
    fn empty_slots_emit_template_and_native_efficient_line() {
        let out = assemble(PromptId::System, &ResolvedSlots::default(), "");
        assert!(out.starts_with("You are n00n"));
        assert!(
            !out.contains("{{"),
            "unfilled marker left in output:\n{out}"
        );
        assert!(out.contains(&format!("{NATIVE_EFFICIENT_LINE}.")));
    }

    #[test]
    fn system_routes_codebase_questions_without_advertising_deferred_backends() {
        let out = assemble(PromptId::System, &ResolvedSlots::default(), "");
        assert!(
            out.contains("index_file"),
            "missing primary tool index_file"
        );
        assert!(
            at(&out, "explore_code") < at(&out, "read_file"),
            "explore_code must be the first codebase route"
        );
        for deferred in [
            "map_codegraph",
            "search_text",
            "run_task",
            "run_team",
            "run_workflow",
        ] {
            assert!(
                !out.contains(deferred),
                "deferred tool advertised: {deferred}"
            );
        }
        assert!(out.contains("tool discovery mechanism"));
    }

    /// One test to pin the whole System layout: every slot shows up, in order,
    /// around the instructions. Covers presence and ordering for all of them.
    #[test]
    fn system_sections_land_in_layout_order() {
        let s = slots(
            PromptId::System,
            &[
                (Slot::ToolUsage, "TOOL_USAGE"),
                (Slot::EfficientTools, "EXTRA_TOOL"),
                (Slot::Conventions, "CONVENTIONS"),
                (Slot::AfterInstructions, "AFTER"),
            ],
        );
        let out = assemble(PromptId::System, &s, "INSTR");
        let positions = ["TOOL_USAGE", "EXTRA_TOOL", "CONVENTIONS", "INSTR", "AFTER"]
            .map(|needle| at(&out, needle));
        assert!(
            positions.is_sorted(),
            "sections out of layout order ({positions:?}):\n{out}"
        );
    }

    #[test]
    fn system_cache_boundary_precedes_dynamic_slots() {
        let slots = slots(
            PromptId::System,
            &[
                (Slot::Environment, "ENVIRONMENT"),
                (Slot::AfterInstructions, "AFTER_INSTRUCTIONS"),
            ],
        );
        let system = assemble_system(PromptId::System, &slots, "INSTRUCTIONS");

        assert_eq!(
            system.to_string(),
            assemble(PromptId::System, &slots, "INSTRUCTIONS")
        );
        let blocks = system.blocks();
        let boundary = blocks
            .iter()
            .position(|block| block.cache == n00n_providers::CacheControl::Ephemeral)
            .unwrap_or_else(|| panic!("missing cache boundary"));
        assert_eq!(
            blocks
                .iter()
                .filter(|block| block.cache == n00n_providers::CacheControl::Ephemeral)
                .count(),
            1
        );
        assert!(!blocks[boundary].text.contains("ENVIRONMENT"));
        assert!(
            blocks[boundary + 1..]
                .iter()
                .any(|block| block.cache == n00n_providers::CacheControl::Dynamic)
        );
    }

    #[test]
    fn empty_dynamic_slot_preserves_cache_boundary() {
        let system = assemble_system(PromptId::System, &ResolvedSlots::default(), "INSTRUCTIONS");
        let openai_prefix = system
            .cacheable_prefix_blocks()
            .iter()
            .map(|block| block.text.as_str())
            .collect::<String>();
        let blocks = system.blocks();
        let provider_boundary = blocks
            .iter()
            .position(|block| block.cache == n00n_providers::CacheControl::Ephemeral)
            .unwrap_or_else(|| panic!("missing cache boundary"));
        let provider_prefix = blocks[..=provider_boundary]
            .iter()
            .map(|block| block.text.as_str())
            .collect::<String>();

        assert!(!openai_prefix.contains("# Tool usage"));
        assert!(provider_prefix.contains("# Tool usage"));
        assert!(system.to_string().contains("# Tool usage"));
    }

    /// Regression: a `tool_usage` hint must land inside the `# Tool usage`
    /// section, not be appended after the rest of the prompt.
    #[test]
    fn tool_usage_hint_lands_inside_tool_usage_section() {
        const HINT: &str = "- HINT_LINE";
        let s = slots(PromptId::System, &[(Slot::ToolUsage, HINT)]);
        let out = assemble(PromptId::System, &s, "");
        let hint = at(&out, HINT);
        assert!(
            at(&out, "# Tool usage") < hint,
            "hint before its section:\n{out}"
        );
        assert!(
            hint < at(&out, "# Conventions"),
            "hint leaked past section:\n{out}"
        );
    }

    #[test]
    fn efficient_tools_extras_join_native_list() {
        let s = slots(
            PromptId::System,
            &[(Slot::EfficientTools, "foo"), (Slot::EfficientTools, "bar")],
        );
        let out = assemble(PromptId::System, &s, "");
        assert!(out.contains(&format!("{NATIVE_EFFICIENT_LINE}, foo, bar.")));
    }

    #[test]
    fn same_slot_preserves_insertion_order() {
        let s = slots(
            PromptId::System,
            &[(Slot::ToolUsage, "FIRST"), (Slot::ToolUsage, "SECOND")],
        );
        let out = assemble(PromptId::System, &s, "");
        assert!(at(&out, "FIRST") < at(&out, "SECOND"));
    }

    /// Only System carries `AfterInstructions`, so the same content shows up there
    /// but never leaks into the subagent prompts.
    #[test]
    fn after_instructions_only_reaches_system() {
        let mut s = ResolvedSlots::default();
        for &pid in PromptId::ALL {
            s.insert(
                pid,
                Slot::AfterInstructions,
                SlotEntry {
                    plugin: Arc::from("p"),
                    content: "AFTER".into(),
                },
            );
        }
        assert!(assemble(PromptId::System, &s, "").contains("AFTER"));
        assert!(!assemble(PromptId::Research, &s, "").contains("AFTER"));
        assert!(!assemble(PromptId::General, &s, "").contains("AFTER"));
    }

    #[test]
    fn research_drops_conventions_but_keeps_efficient_extras() {
        let s = slots(
            PromptId::Research,
            &[
                (Slot::Conventions, "DROPPED"),
                (Slot::EfficientTools, "EXTRA"),
            ],
        );
        let out = assemble(PromptId::Research, &s, "");
        assert!(!out.contains("DROPPED"));
        assert!(out.contains(&format!("{NATIVE_EFFICIENT_LINE}, EXTRA.")));
    }

    #[test_case(PromptId::System, Slot::ToolUsage, true ; "system_tool_usage")]
    #[test_case(PromptId::System, Slot::EfficientTools, true ; "system_efficient")]
    #[test_case(PromptId::System, Slot::Conventions, true ; "system_conventions")]
    #[test_case(PromptId::System, Slot::AfterInstructions, true ; "system_after")]
    #[test_case(PromptId::System, Slot::Identity, true ; "system_identity")]
    #[test_case(PromptId::System, Slot::Tone, true ; "system_tone")]
    #[test_case(PromptId::Research, Slot::Conventions, false ; "research_no_conventions")]
    #[test_case(PromptId::Research, Slot::AfterInstructions, false ; "research_no_after")]
    #[test_case(PromptId::Research, Slot::Identity, false ; "research_no_identity")]
    #[test_case(PromptId::Research, Slot::Tone, false ; "research_no_tone")]
    #[test_case(PromptId::General, Slot::AfterInstructions, false ; "general_no_after")]
    #[test_case(PromptId::General, Slot::Identity, false ; "general_no_identity")]
    #[test_case(PromptId::General, Slot::Tone, false ; "general_no_tone")]
    fn has_slot(prompt: PromptId, slot: Slot, expected: bool) {
        assert_eq!(prompt.has_slot(slot), expected);
    }

    #[test_case("after_instructions", Some(Slot::AfterInstructions) ; "valid_slot")]
    #[test_case("tool_usagee", None ; "typo_slot")]
    #[test_case("identity", Some(Slot::Identity) ; "identity_slot")]
    #[test_case("tone", Some(Slot::Tone) ; "tone_slot")]
    fn slot_parse_is_plugin_contract(input: &str, expected: Option<Slot>) {
        assert_eq!(input.parse::<Slot>().ok(), expected);
    }

    #[test_case("system", Some(PromptId::System) ; "valid_prompt")]
    #[test_case("systm", None ; "typo_prompt")]
    fn prompt_parse_is_plugin_contract(input: &str, expected: Option<PromptId>) {
        assert_eq!(input.parse::<PromptId>().ok(), expected);
    }

    #[test_case(Slot::Identity, SlotKind::Singleton ; "identity_singleton")]
    #[test_case(Slot::Tone, SlotKind::Singleton ; "tone_singleton")]
    #[test_case(Slot::Conventions, SlotKind::Aggregate ; "conventions_aggregate")]
    #[test_case(Slot::ToolUsage, SlotKind::Aggregate ; "tool_usage_aggregate")]
    #[test_case(Slot::EfficientTools, SlotKind::Aggregate ; "efficient_aggregate")]
    #[test_case(Slot::AfterInstructions, SlotKind::Aggregate ; "after_aggregate")]
    fn slot_kind_matches_expectations(slot: Slot, expected: SlotKind) {
        assert_eq!(slot.kind(), expected);
    }

    #[test]
    fn singleton_default_used_when_empty() {
        let out = assemble(PromptId::System, &ResolvedSlots::default(), "");
        assert!(out.starts_with("You are n00n"));
    }

    #[test]
    fn singleton_entry_replaces_default() {
        let mut s = ResolvedSlots::default();
        s.insert(
            PromptId::System,
            Slot::Identity,
            SlotEntry {
                plugin: Arc::from("user"),
                content: "Custom identity".into(),
            },
        );
        let out = assemble(PromptId::System, &s, "");
        assert!(out.contains("Custom identity"));
        assert!(!out.contains("You are n00n"));
    }

    #[test]
    fn singleton_last_entry_wins() {
        let mut s = ResolvedSlots::default();
        s.insert(
            PromptId::System,
            Slot::Identity,
            SlotEntry {
                plugin: Arc::from("first"),
                content: "FIRST".into(),
            },
        );
        s.insert(
            PromptId::System,
            Slot::Identity,
            SlotEntry {
                plugin: Arc::from("second"),
                content: "SECOND".into(),
            },
        );
        let out = assemble(PromptId::System, &s, "");
        assert!(out.contains("SECOND"));
        assert!(!out.contains("FIRST"));
        assert!(!out.contains("You are n00n"));
    }

    fn todo_line(status: &str, content: &str) -> String {
        serde_json::json!({ "status": status, "content": content }).to_string()
    }

    #[test]
    fn todo_cap_enforced_with_multiple_oversized_in_progress() {
        let big = "x".repeat(MAX_AFTER_INSTRUCTIONS_BYTES);
        let content = format!(
            "# Current todos\n{}\n{}",
            todo_line("in_progress", &big),
            todo_line("in_progress", &big)
        );
        let out = cap_after_instructions(content);
        assert!(
            out.len() <= MAX_AFTER_INSTRUCTIONS_BYTES,
            "len={}",
            out.len()
        );
        assert!(out.contains("in_progress"));
    }

    #[test]
    fn todo_cap_includes_header_bytes() {
        let header = "h".repeat(MAX_AFTER_INSTRUCTIONS_BYTES - 100);
        let content = format!(
            "# Current todos\n{header}\n{}",
            todo_line("in_progress", "task")
        );
        let out = cap_after_instructions(content);
        assert!(
            out.len() <= MAX_AFTER_INSTRUCTIONS_BYTES,
            "len={}",
            out.len()
        );
    }

    #[test]
    fn todo_cap_drops_pending_before_in_progress() {
        let big = "p".repeat(MAX_AFTER_INSTRUCTIONS_BYTES);
        let content = format!(
            "# Current todos\n{}\n{}",
            todo_line("in_progress", "keep me"),
            todo_line("pending", &big)
        );
        let out = cap_after_instructions(content);
        assert!(out.contains("keep me"));
        assert!(!out.contains(&big));
        assert!(out.len() <= MAX_AFTER_INSTRUCTIONS_BYTES);
    }

    #[test]
    fn todo_cap_blank_line_overhead_rebuilt() {
        let padding = "\n\n\n".repeat(200);
        let content = format!(
            "# Current todos\n{padding}{}",
            todo_line("in_progress", &"x".repeat(MAX_AFTER_INSTRUCTIONS_BYTES))
        );
        let out = cap_after_instructions(content);
        assert!(
            out.len() <= MAX_AFTER_INSTRUCTIONS_BYTES,
            "len={}",
            out.len()
        );
    }

    #[test]
    fn todo_cap_escaped_content_stays_well_formed_json() {
        // Escapable chars inflate the serialized line; the cap must measure the
        // re-encoded form so every emitted line stays valid JSON.
        let escaped = "\"\\".repeat(MAX_AFTER_INSTRUCTIONS_BYTES);
        let content = format!("# Current todos\n{}", todo_line("in_progress", &escaped));
        let out = cap_after_instructions(content);
        assert!(
            out.len() <= MAX_AFTER_INSTRUCTIONS_BYTES,
            "len={}",
            out.len()
        );
        for line in out.lines().filter(|l| l.trim_start().starts_with('{')) {
            serde_json::from_str::<serde_json::Value>(line).unwrap_or_else(|_| {
                panic!("malformed todo line: {}", &line[..line.len().min(120)])
            });
        }
    }

    #[test]
    fn todo_cap_multibyte_content_shrinks_without_stall() {
        // Multi-byte chars made the old byte-index binary search converge onto
        // a char interior and loop forever. Any non-ASCII todo large enough to
        // need shrinking must still terminate and emit valid JSON.
        let multibyte = "é".repeat(MAX_AFTER_INSTRUCTIONS_BYTES);
        let content = format!("# Current todos\n{}", todo_line("in_progress", &multibyte));
        let out = cap_after_instructions(content);
        assert!(
            out.len() <= MAX_AFTER_INSTRUCTIONS_BYTES,
            "len={}",
            out.len()
        );
        for line in out.lines().filter(|l| l.trim_start().starts_with('{')) {
            serde_json::from_str::<serde_json::Value>(line).unwrap_or_else(|_| {
                panic!("malformed todo line: {}", &line[..line.len().min(120)])
            });
        }
    }

    #[test]
    fn todo_cap_multibyte_four_byte_char_shrinks() {
        let multibyte = "🙂".repeat(MAX_AFTER_INSTRUCTIONS_BYTES);
        let content = format!("# Current todos\n{}", todo_line("in_progress", &multibyte));
        let out = cap_after_instructions(content);
        assert!(
            out.len() <= MAX_AFTER_INSTRUCTIONS_BYTES,
            "len={}",
            out.len()
        );
        for line in out.lines().filter(|l| l.trim_start().starts_with('{')) {
            serde_json::from_str::<serde_json::Value>(line).unwrap_or_else(|_| {
                panic!("malformed todo line: {}", &line[..line.len().min(120)])
            });
        }
    }

    #[test]
    fn todo_cap_oversized_header_still_bounded() {
        // Hint text before the first JSON line is preserved, but it cannot be
        // allowed to push the whole block past the cap.
        let header = "m".repeat(MAX_AFTER_INSTRUCTIONS_BYTES + 512);
        let content = format!(
            "{header}\n# Current todos\n{}",
            todo_line("in_progress", "task")
        );
        let out = cap_after_instructions(content);
        assert!(
            out.len() <= MAX_AFTER_INSTRUCTIONS_BYTES,
            "len={}",
            out.len()
        );
    }

    #[test]
    fn identity_only_in_system_not_subagents() {
        assert!(PromptId::System.has_slot(Slot::Identity));
        assert!(!PromptId::Research.has_slot(Slot::Identity));
        assert!(!PromptId::General.has_slot(Slot::Identity));
    }

    #[test]
    fn tone_only_in_system_not_subagents() {
        assert!(PromptId::System.has_slot(Slot::Tone));
        assert!(!PromptId::Research.has_slot(Slot::Tone));
        assert!(!PromptId::General.has_slot(Slot::Tone));
    }

    #[test]
    fn conventions_entry_appends_to_template_defaults() {
        let mut s = ResolvedSlots::default();
        s.insert(
            PromptId::System,
            Slot::Conventions,
            SlotEntry {
                plugin: Arc::from("plugin"),
                content: "- Extra rule".into(),
            },
        );
        let out = assemble(PromptId::System, &s, "");
        assert!(out.contains("Never assume library availability"));
        assert!(out.contains("- Extra rule"));
    }

    #[test]
    fn prompt_templates_within_size_baselines() {
        // Baseline sizes before compression (from T061 audit, updated after origin/main merge).
        // Most prompts still aim for >=10% compression; system.md is intentionally capped
        // because it carries required static instructions that are not meant to shrink.
        const SYSTEM_BASELINE: usize = 1710;
        const GENERAL_BASELINE: usize = 1759;
        const RESEARCH_BASELINE: usize = 1530;
        const COMPACTION_USER_BASELINE: usize = 927;
        const COMPACTION_BASELINE: usize = 669;
        const PLAN_BASELINE: usize = 1031;

        let system_current = SYSTEM_PROMPT.len();
        let general_current = GENERAL_PROMPT.len();
        let research_current = RESEARCH_PROMPT.len();
        let compaction_user_current = COMPACTION_USER.len();
        let compaction_current = COMPACTION_SYSTEM.len();
        let plan_current = PLAN_PROMPT.len();

        // system.md is capped, not compressed, because its instructions are static content.
        assert!(
            system_current <= SYSTEM_BASELINE,
            "system.md size: {system_current} bytes (baseline: {SYSTEM_BASELINE})"
        );
        assert!(
            general_current <= (GENERAL_BASELINE * 9 / 10),
            "general.md not compressed enough: {general_current} bytes (baseline: {GENERAL_BASELINE}, target: {})",
            GENERAL_BASELINE * 9 / 10
        );
        assert!(
            research_current <= (RESEARCH_BASELINE * 9 / 10),
            "research.md not compressed enough: {research_current} bytes (baseline: {RESEARCH_BASELINE}, target: {})",
            RESEARCH_BASELINE * 9 / 10
        );
        assert!(
            compaction_user_current <= (COMPACTION_USER_BASELINE * 9 / 10),
            "compaction_user.md not compressed enough: {compaction_user_current} bytes (baseline: {COMPACTION_USER_BASELINE}, target: {})",
            COMPACTION_USER_BASELINE * 9 / 10
        );
        assert!(
            compaction_current <= (COMPACTION_BASELINE * 9 / 10),
            "compaction.md not compressed enough: {compaction_current} bytes (baseline: {COMPACTION_BASELINE}, target: {})",
            COMPACTION_BASELINE * 9 / 10
        );
        assert!(
            plan_current <= (PLAN_BASELINE * 9 / 10),
            "plan.md not compressed enough: {plan_current} bytes (baseline: {PLAN_BASELINE}, target: {})",
            PLAN_BASELINE * 9 / 10
        );
    }

    #[test]
    fn assemble_system_without_environment_has_no_heading_or_marker() {
        let out = assemble(PromptId::System, &ResolvedSlots::default(), "");
        assert!(!out.contains("# Environment"));
        assert!(!out.contains("{{environment}}"));
    }

    #[test]
    fn assemble_system_with_environment_shows_heading_and_content() {
        const ENV_CONTENT: &str = "# Environment\nCurrent date: 2026-07-21";
        let mut s = ResolvedSlots::default();
        s.insert(
            PromptId::System,
            Slot::Environment,
            SlotEntry {
                plugin: Arc::from("test"),
                content: ENV_CONTENT.into(),
            },
        );
        let out = assemble(PromptId::System, &s, "");
        assert!(out.contains("# Environment"));
        assert!(out.contains("Current date: 2026-07-21"));
        assert!(!out.contains("{{environment}}"));
    }

    #[test]
    fn environment_section_placed_between_objectivity_and_tool_usage() {
        const ENV_CONTENT: &str = "# Environment\nCurrent date: 2026-07-21";
        let mut s = ResolvedSlots::default();
        s.insert(
            PromptId::System,
            Slot::Environment,
            SlotEntry {
                plugin: Arc::from("test"),
                content: ENV_CONTENT.into(),
            },
        );
        let out = assemble(PromptId::System, &s, "");
        let obj_idx = out.find("# Professional objectivity").unwrap();
        let env_idx = out.find("# Environment").unwrap();
        let tool_idx = out.find("# Tool usage").unwrap();
        assert!(
            obj_idx < env_idx,
            "Environment should be after Professional objectivity"
        );
        assert!(
            env_idx < tool_idx,
            "Environment should be before Tool usage"
        );
    }
}

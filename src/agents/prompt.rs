use serde::de::DeserializeOwned;

use crate::domain::ProjectConfig;

/// Project context every lead sees, so all three judge work against the same rules.
pub(crate) fn project_context(project: &ProjectConfig) -> String {
    format!(
        "PROJECT:\n{}\n\nOBJECTIVE:\n{}\n\nPROJECT RULES:\n{}\n\nACCEPTANCE CRITERIA:\n{}",
        project.name,
        project.objective,
        bullets(&project.rules),
        bullets(&project.acceptance_criteria),
    )
}

pub(crate) fn bullets(items: &[String]) -> String {
    if items.is_empty() {
        return "- none".to_string();
    }
    items
        .iter()
        .map(|item| format!("- {item}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parses a model reply that should be a JSON object: strict JSON first, then the outermost
/// `{...}` (models often wrap JSON in prose or a ```json fence). `None` means unusable.
pub(crate) fn parse_json_reply<T: DeserializeOwned>(raw: &str) -> Option<T> {
    let trimmed = raw.trim();
    if let Ok(value) = serde_json::from_str(trimmed) {
        return Some(value);
    }
    let (start, end) = (trimmed.find('{')?, trimmed.rfind('}')?);
    if start >= end {
        return None;
    }
    serde_json::from_str(&trimmed[start..=end]).ok()
}

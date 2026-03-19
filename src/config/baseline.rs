use crate::engine::Diagnostic;
use serde::{Deserialize, Serialize};
use xxhash_rust::xxh3::xxh3_64;

#[derive(Debug, Serialize, Deserialize)]
pub struct Baseline {
    pub version: u32,
    pub violations: Vec<BaselineEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BaselineEntry {
    pub hash: String,
    pub file: String,
    pub rule: String,
    pub line_content_trimmed: String,
}

impl Baseline {
    pub fn new() -> Self {
        Baseline {
            version: 1,
            violations: Vec::new(),
        }
    }

    pub fn from_diagnostics(
        file_path: &str,
        diagnostics: &[&Diagnostic],
        source_lines: &[&str],
    ) -> Self {
        let mut violations = Vec::new();
        for (i, diag) in diagnostics.iter().enumerate() {
            // source_lines is treated as a parallel slice (one entry per diagnostic).
            // If source_lines is shorter than diagnostics, fall back to empty string.
            let line_content = source_lines.get(i).map(|l| l.trim()).unwrap_or("");
            let hash = compute_hash(file_path, &diag.rule_id, line_content);
            violations.push(BaselineEntry {
                hash,
                file: file_path.to_string(),
                rule: diag.rule_id.clone(),
                line_content_trimmed: line_content.to_string(),
            });
        }
        Baseline {
            version: 1,
            violations,
        }
    }

    pub fn is_suppressed(&self, file_path: &str, diag: &Diagnostic, source_line: &str) -> bool {
        let hash = compute_hash(file_path, &diag.rule_id, source_line.trim());
        self.violations.iter().any(|v| v.hash == hash)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap()
    }

    pub fn from_json(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| format!("Invalid baseline: {}", e))
    }
}

impl Default for Baseline {
    fn default() -> Self {
        Self::new()
    }
}

fn compute_hash(file_path: &str, rule_id: &str, trimmed_line: &str) -> String {
    let input = format!("{}:{}:{}", file_path, rule_id, trimmed_line);
    format!("{:016x}", xxh3_64(input.as_bytes()))
}

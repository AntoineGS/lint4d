pub mod baseline;
mod toml_config;

use crate::engine::Severity;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleSeverityOverride {
    Severity(Severity),
    Off,
}

#[derive(Debug)]
pub struct Config {
    pub version: u32,
    pub paths: Vec<String>,
    pub exclude: Vec<String>,
    dcu_paths: Vec<String>,
    rule_overrides: HashMap<String, RuleSeverityOverride>,
    constant_style: String,
    local_variable_style: String,
}

impl Config {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Result<Self, String> {
        let raw: toml_config::RawConfig =
            toml::from_str(s).map_err(|e| format!("Invalid config: {}", e))?;

        let mut rule_overrides = HashMap::new();
        if let Some(rules) = &raw.rules {
            for (key, value) in &rules.overrides {
                let override_val = if value == "off" {
                    RuleSeverityOverride::Off
                } else {
                    let severity: Severity = value
                        .parse()
                        .map_err(|_| format!("Invalid severity '{}' for rule '{}'", value, key))?;
                    RuleSeverityOverride::Severity(severity)
                };
                rule_overrides.insert(key.clone(), override_val);
            }
        }

        let constant_style = raw
            .rules
            .as_ref()
            .and_then(|r| r.naming.as_ref())
            .and_then(|n| n.constant_style.clone())
            .unwrap_or_else(|| "UPPER_CASE".to_string());

        let local_variable_style = raw
            .rules
            .as_ref()
            .and_then(|r| r.naming.as_ref())
            .and_then(|n| n.local_variable_style.clone())
            .unwrap_or_else(|| "camelCase".to_string());

        let lint4d = raw.lint4d.unwrap_or_default();

        Ok(Config {
            version: raw.version,
            paths: lint4d.paths,
            exclude: lint4d.exclude,
            dcu_paths: lint4d.dcu_paths,
            rule_overrides,
            constant_style,
            local_variable_style,
        })
    }

    pub fn rule_severity(&self, rule_id: &str) -> Option<RuleSeverityOverride> {
        self.rule_overrides.get(rule_id).cloned()
    }

    pub fn constant_style(&self) -> &str {
        &self.constant_style
    }

    pub fn local_variable_style(&self) -> &str {
        &self.local_variable_style
    }

    pub fn dcu_paths(&self) -> &[String] {
        &self.dcu_paths
    }

    pub fn discover(start_dir: &Path) -> Result<(Config, PathBuf), String> {
        let mut dir = start_dir.to_path_buf();
        loop {
            let config_path = dir.join(".lint4d.toml");
            if config_path.exists() {
                let content = fs::read_to_string(&config_path)
                    .map_err(|e| format!("Failed to read {}: {}", config_path.display(), e))?;
                let config = Config::from_str(&content)?;
                return Ok((config, dir));
            }
            if !dir.pop() {
                break;
            }
        }
        Ok((
            Config {
                version: 1,
                paths: Vec::new(),
                exclude: Vec::new(),
                dcu_paths: Vec::new(),
                rule_overrides: HashMap::new(),
                constant_style: "UPPER_CASE".to_string(),
                local_variable_style: "camelCase".to_string(),
            },
            start_dir.to_path_buf(),
        ))
    }
}

pub mod baseline;
mod toml_config;

use crate::engine::Severity;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

const VALID_CONSTANT_STYLES: &[&str] = &["UPPER_CASE", "PascalCase"];
const VALID_LOCAL_VARIABLE_STYLES: &[&str] = &["camelCase", "PascalCase"];

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
    platform: Option<String>,
    build_config: Option<String>,
    bds_path: Option<String>,
    rule_overrides: HashMap<String, RuleSeverityOverride>,
    constant_style: String,
    local_variable_style: String,
}

impl FromStr for Config {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
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

        if !VALID_CONSTANT_STYLES.contains(&constant_style.as_str()) {
            return Err(format!(
                "Invalid constant_style '{}' (expected one of: {})",
                constant_style,
                VALID_CONSTANT_STYLES.join(", ")
            ));
        }

        let local_variable_style = raw
            .rules
            .as_ref()
            .and_then(|r| r.naming.as_ref())
            .and_then(|n| n.local_variable_style.clone())
            .unwrap_or_else(|| "camelCase".to_string());

        if !VALID_LOCAL_VARIABLE_STYLES.contains(&local_variable_style.as_str()) {
            return Err(format!(
                "Invalid local_variable_style '{}' (expected one of: {})",
                local_variable_style,
                VALID_LOCAL_VARIABLE_STYLES.join(", ")
            ));
        }

        let lint4d = raw.lint4d.unwrap_or_default();

        Ok(Config {
            version: raw.version,
            paths: lint4d.paths,
            exclude: lint4d.exclude,
            dcu_paths: lint4d.dcu_paths,
            platform: lint4d.platform,
            build_config: lint4d.build_config,
            bds_path: lint4d.bds_path,
            rule_overrides,
            constant_style,
            local_variable_style,
        })
    }
}

impl Config {
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

    pub fn platform(&self) -> Option<&str> {
        self.platform.as_deref()
    }

    pub fn build_config(&self) -> Option<&str> {
        self.build_config.as_deref()
    }

    pub fn bds_path(&self) -> Option<&str> {
        self.bds_path.as_deref()
    }

    pub fn discover(start_dir: &Path) -> Result<(Config, PathBuf), String> {
        let mut dir = start_dir.to_path_buf();
        loop {
            let config_path = dir.join(".lint4d.toml");
            if config_path.exists() {
                let content = fs::read_to_string(&config_path)
                    .map_err(|e| format!("Failed to read {}: {}", config_path.display(), e))?;
                let config: Config = content.parse()?;
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
                platform: None,
                build_config: None,
                bds_path: None,
                rule_overrides: HashMap::new(),
                constant_style: "UPPER_CASE".to_string(),
                local_variable_style: "camelCase".to_string(),
            },
            start_dir.to_path_buf(),
        ))
    }
}

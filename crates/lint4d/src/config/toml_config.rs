use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Deserialize, Default)]
pub struct RawConfig {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub lint4d: Option<RawLint4dSection>,
    #[serde(default)]
    pub rules: Option<RawRulesSection>,
}

fn default_version() -> u32 {
    1
}

#[derive(Debug, Deserialize, Default)]
pub struct RawLint4dSection {
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub dcu_paths: Vec<String>,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub build_config: Option<String>,
    #[serde(default)]
    pub bds_path: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct RawRulesSection {
    #[serde(default)]
    pub naming: Option<RawNamingSection>,
    #[serde(flatten)]
    pub overrides: HashMap<String, String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct RawNamingSection {
    pub constant_style: Option<String>,
    pub local_variable_style: Option<String>,
}

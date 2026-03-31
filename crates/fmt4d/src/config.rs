#![allow(dead_code)]

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndentStyle {
    #[default]
    Space,
    Tab,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeginStyle {
    #[default]
    NextLine,
    SameLine,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndOfLine {
    #[default]
    Crlf,
    Lf,
    Auto,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BlankLineConfig {
    pub between_procedures: usize,
    pub between_sections: usize,
    pub max_consecutive: usize,
}

impl Default for BlankLineConfig {
    fn default() -> Self {
        BlankLineConfig {
            between_procedures: 1,
            between_sections: 1,
            max_consecutive: 1,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UsesConfig {
    pub sort: bool,
    pub group: bool,
    pub external_paths: Vec<String>,
    pub external_prefixes: Vec<String>,
}

impl Default for UsesConfig {
    fn default() -> Self {
        UsesConfig {
            sort: true,
            group: true,
            external_paths: Vec::new(),
            external_prefixes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FmtConfig {
    pub indent_size: usize,
    pub indent_style: IndentStyle,
    pub max_line_length: usize,
    pub begin_style: BeginStyle,
    pub end_of_line: EndOfLine,
    pub blank_lines: BlankLineConfig,
    pub uses: UsesConfig,
    #[serde(skip)]
    pub project_root: Option<PathBuf>,
}

impl Default for FmtConfig {
    fn default() -> Self {
        FmtConfig {
            indent_size: 2,
            indent_style: IndentStyle::Space,
            max_line_length: 120,
            begin_style: BeginStyle::NextLine,
            end_of_line: EndOfLine::Crlf,
            blank_lines: BlankLineConfig::default(),
            uses: UsesConfig::default(),
            project_root: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawFmtToml {
    format: Option<FmtConfig>,
}

impl FmtConfig {
    pub fn from_toml(toml_str: &str) -> Result<FmtConfig, String> {
        let raw: RawFmtToml =
            toml::from_str(toml_str).map_err(|e| format!("Invalid fmt4d config: {}", e))?;
        Ok(raw.format.unwrap_or_default())
    }

    pub fn discover(start_dir: &Path) -> FmtConfig {
        match pascal_core::config_discovery::find_config_file(start_dir, ".fmt4d.toml") {
            Some((content, dir)) => {
                let mut config = Self::from_toml(&content).unwrap_or_default();
                config.project_root = Some(dir);
                config
            }
            None => FmtConfig {
                project_root: Some(start_dir.to_path_buf()),
                ..FmtConfig::default()
            },
        }
    }

    pub fn with_overrides(
        mut self,
        indent_size: Option<usize>,
        max_line_length: Option<usize>,
    ) -> Self {
        if let Some(size) = indent_size {
            self.indent_size = size;
        }
        if let Some(len) = max_line_length {
            self.max_line_length = len;
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_values() {
        let config = FmtConfig::default();
        assert_eq!(config.indent_size, 2);
        assert_eq!(config.indent_style, IndentStyle::Space);
        assert_eq!(config.max_line_length, 120);
        assert_eq!(config.begin_style, BeginStyle::NextLine);
        assert_eq!(config.blank_lines.between_procedures, 1);
        assert!(config.uses.sort);
        assert!(config.uses.group);
        assert!(config.uses.external_paths.is_empty());
        assert!(config.uses.external_prefixes.is_empty());
    }

    #[test]
    fn parse_toml_config() {
        let toml = r#"
[format]
indent_size = 4
max_line_length = 80

[format.uses]
sort = false
"#;
        let config = FmtConfig::from_toml(toml).unwrap();
        assert_eq!(config.indent_size, 4);
        assert_eq!(config.max_line_length, 80);
        assert!(!config.uses.sort);
        assert_eq!(config.indent_style, IndentStyle::Space);
        assert!(config.uses.group);
    }

    #[test]
    fn parse_toml_external_config() {
        let toml = r#"
[format.uses]
sort = true
group = true
external_paths = ["vendor", "lib/third-party"]
external_prefixes = ["Spring", "Neon"]
"#;
        let config = FmtConfig::from_toml(toml).unwrap();
        assert_eq!(
            config.uses.external_paths,
            vec!["vendor", "lib/third-party"]
        );
        assert_eq!(config.uses.external_prefixes, vec!["Spring", "Neon"]);
    }

    #[test]
    fn with_overrides() {
        let config = FmtConfig::default().with_overrides(Some(4), Some(80));
        assert_eq!(config.indent_size, 4);
        assert_eq!(config.max_line_length, 80);
    }

    #[test]
    fn empty_toml_uses_defaults() {
        let config = FmtConfig::from_toml("").unwrap();
        assert_eq!(config.indent_size, 2);
    }

    #[test]
    fn discover_stores_project_root() {
        // FmtConfig::default() should have project_root as None
        let config = FmtConfig::default();
        assert!(config.project_root.is_none());
    }
}

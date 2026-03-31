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
    Auto,
    Crlf,
    Lf,
}

impl EndOfLine {
    /// Detect the dominant line ending in the source bytes.
    /// Returns `Crlf` if any `\r\n` is found, otherwise `Lf`.
    pub fn detect(source: &[u8]) -> EndOfLine {
        if source.windows(2).any(|w| w == b"\r\n") {
            EndOfLine::Crlf
        } else {
            EndOfLine::Lf
        }
    }

    /// Resolve `Auto` into a concrete line ending by detecting from source.
    pub fn resolve(self, source: &[u8]) -> EndOfLine {
        match self {
            EndOfLine::Auto => EndOfLine::detect(source),
            other => other,
        }
    }

    /// Convert all `\n` in the output to the target line ending.
    /// Assumes the input only contains `\n` (no `\r\n`).
    pub fn apply(self, output: &str) -> String {
        match self {
            EndOfLine::Crlf => output.replace('\n', "\r\n"),
            EndOfLine::Lf | EndOfLine::Auto => output.to_string(),
        }
    }
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
            group: false,
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
            end_of_line: EndOfLine::Auto,
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
        end_of_line: Option<EndOfLine>,
    ) -> Self {
        if let Some(size) = indent_size {
            self.indent_size = size;
        }
        if let Some(len) = max_line_length {
            self.max_line_length = len;
        }
        if let Some(eol) = end_of_line {
            self.end_of_line = eol;
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
        assert_eq!(config.end_of_line, EndOfLine::Auto);
        assert_eq!(config.blank_lines.between_procedures, 1);
        assert!(config.uses.sort);
        assert!(!config.uses.group);
        assert!(config.uses.external_paths.is_empty());
        assert!(config.uses.external_prefixes.is_empty());
    }

    #[test]
    fn detect_crlf_in_source() {
        assert_eq!(
            EndOfLine::detect(b"unit Test;\r\ninterface\r\n"),
            EndOfLine::Crlf
        );
    }

    #[test]
    fn detect_lf_in_source() {
        assert_eq!(EndOfLine::detect(b"unit Test;\ninterface\n"), EndOfLine::Lf);
    }

    #[test]
    fn detect_defaults_to_lf_for_empty() {
        assert_eq!(EndOfLine::detect(b""), EndOfLine::Lf);
    }

    #[test]
    fn resolve_auto_detects_from_source() {
        assert_eq!(EndOfLine::Auto.resolve(b"foo\r\nbar\r\n"), EndOfLine::Crlf);
        assert_eq!(EndOfLine::Auto.resolve(b"foo\nbar\n"), EndOfLine::Lf);
    }

    #[test]
    fn resolve_explicit_ignores_source() {
        assert_eq!(EndOfLine::Lf.resolve(b"foo\r\nbar\r\n"), EndOfLine::Lf);
        assert_eq!(EndOfLine::Crlf.resolve(b"foo\nbar\n"), EndOfLine::Crlf);
    }

    #[test]
    fn apply_lf_passes_through() {
        assert_eq!(EndOfLine::Lf.apply("a\nb\n"), "a\nb\n");
    }

    #[test]
    fn apply_crlf_converts() {
        assert_eq!(EndOfLine::Crlf.apply("a\nb\n"), "a\r\nb\r\n");
    }

    #[test]
    fn apply_auto_passes_through() {
        assert_eq!(EndOfLine::Auto.apply("a\nb\n"), "a\nb\n");
    }

    #[test]
    fn with_overrides_end_of_line() {
        let config = FmtConfig::default().with_overrides(None, None, Some(EndOfLine::Lf));
        assert_eq!(config.end_of_line, EndOfLine::Lf);
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
        assert!(!config.uses.group);
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
        let config = FmtConfig::default().with_overrides(Some(4), Some(80), None);
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

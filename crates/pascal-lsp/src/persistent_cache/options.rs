//! The persistent-cache options (spec §11.1 amendment 2026-11-03, D25),
//! parsed apart from `WorkspaceOptions` so a change never rebuilds the
//! workspace index.

use crate::workspace::{RuntimeOption, apply_runtime_field};
use pascal_cache::{
    DEFAULT_MAX_NAMESPACES, MAX_NAMESPACES, MAX_PAGE_CACHE_BYTES, MAX_READERS, MAX_ROW_BATCH_BYTES,
    MAX_SCRATCH_BYTES, MAX_WAIT, MIN_CONNECTION_PAGE_CACHE_BYTES, StoreLimits,
};
use serde_json::{Map, Value};
use std::path::PathBuf;
use std::time::Duration;

/// 256 KiB for each of the readers and the writer.
pub(crate) const MIN_PAGE_BYTES: u64 = MIN_CONNECTION_PAGE_CACHE_BYTES * (MAX_READERS as u64 + 1);
/// One row batch.
pub(crate) const MIN_SCRATCH_BYTES: u64 = MAX_ROW_BATCH_BYTES as u64;
const MAX_DIRECTORY_BYTES: usize = 4096;
const MAX_WAIT_MS: u64 = MAX_WAIT.as_millis() as u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CacheOptions {
    pub(crate) enabled: bool,
    /// `None`: the platform default (`pascal_cache::default_directory`).
    pub(crate) directory: Option<PathBuf>,
    pub(crate) page_bytes: u64,
    pub(crate) scratch_bytes: u64,
    pub(crate) wait: Duration,
    /// Live namespaces (builds) the store keeps, this one included
    /// (l4d-1ypi). An initialization setting only: a runtime value could
    /// come from a workspace file and retire other builds' facts.
    pub(crate) max_namespaces: usize,
    /// An invalid initialization value: persistence stays off for the
    /// session, and this actionable message is reported.
    pub(crate) invalid: Option<String>,
}

impl Default for CacheOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            directory: None,
            page_bytes: MAX_PAGE_CACHE_BYTES,
            scratch_bytes: MAX_SCRATCH_BYTES,
            wait: MAX_WAIT,
            max_namespaces: DEFAULT_MAX_NAMESPACES,
            invalid: None,
        }
    }
}

/// The `pascalLsp` (or `pascal-lsp`) object when present, else the value
/// itself, as `WorkspaceOptions::parse` reads it.
fn section(value: &Value) -> Option<&Map<String, Value>> {
    value
        .get("pascalLsp")
        .or_else(|| value.get("pascal-lsp"))
        .and_then(Value::as_object)
        .or_else(|| value.as_object())
}

enum Bytes {
    Value(u64),
    Clamped(u64, String),
    Invalid(String),
}

fn bytes(value: &Value, name: &str, minimum: u64, maximum: u64, floor: &str) -> Bytes {
    let Some(number) = value.as_u64() else {
        return Bytes::Invalid(format!("{name} must be a positive integer or null"));
    };
    if number < minimum {
        return Bytes::Invalid(format!("{name} must be at least {minimum} ({floor})"));
    }
    if number > maximum {
        return Bytes::Clamped(
            maximum,
            format!("{name}={number} exceeds the safe limit {maximum}; using {maximum}"),
        );
    }
    Bytes::Value(number)
}

fn parse_limit(
    object: &Map<String, Value>,
    field: &str,
    (minimum, maximum, floor): (u64, u64, &str),
    target: &mut u64,
    warnings: &mut Vec<String>,
    invalid: &mut Vec<String>,
) {
    match object.get(field) {
        None | Some(Value::Null) => {}
        Some(value) => match bytes(
            value,
            &format!("initializationOptions.{field}"),
            minimum,
            maximum,
            floor,
        ) {
            Bytes::Value(value) => *target = value,
            Bytes::Clamped(value, warning) => {
                *target = value;
                warnings.push(warning);
            }
            Bytes::Invalid(error) => invalid.push(error),
        },
    }
}

impl CacheOptions {
    pub(crate) fn active(&self) -> bool {
        self.enabled && self.invalid.is_none()
    }

    pub(crate) fn limits(&self) -> StoreLimits {
        StoreLimits {
            page_cache_bytes: self.page_bytes,
            scratch_bytes: self.scratch_bytes,
            wait: self.wait,
            max_namespaces: self.max_namespaces,
            ..StoreLimits::default()
        }
    }

    /// Parse `initializationOptions`. Returns the options and warnings
    /// (clamped values); invalid values set `invalid` instead of failing
    /// `initialize`.
    pub(crate) fn parse_initialization(value: Option<&Value>) -> (Self, Vec<String>) {
        let mut options = Self::default();
        let mut warnings = Vec::new();
        let mut invalid = Vec::new();
        let Some(object) = value.and_then(section) else {
            return (options, warnings);
        };
        let name = |field: &str| format!("initializationOptions.{field}");
        match object.get("persistentCacheEnabled") {
            None | Some(Value::Null) => {}
            Some(Value::Bool(enabled)) => options.enabled = *enabled,
            Some(_) => invalid.push(format!(
                "{} must be true or false",
                name("persistentCacheEnabled")
            )),
        }
        match object.get("persistentCacheDirectory") {
            None | Some(Value::Null) => {}
            Some(Value::String(text)) if text.len() > MAX_DIRECTORY_BYTES => invalid.push(format!(
                "{} is longer than {MAX_DIRECTORY_BYTES} bytes",
                name("persistentCacheDirectory")
            )),
            Some(Value::String(text))
                if !text.contains('\0') && std::path::Path::new(text).is_absolute() =>
            {
                options.directory = Some(PathBuf::from(text));
            }
            Some(_) => invalid.push(format!(
                "{} must be an absolute path of a private local directory (`~` is not expanded)",
                name("persistentCacheDirectory")
            )),
        }
        parse_limit(
            object,
            "persistentCachePageBytes",
            (
                MIN_PAGE_BYTES,
                MAX_PAGE_CACHE_BYTES,
                "256 KiB per connection",
            ),
            &mut options.page_bytes,
            &mut warnings,
            &mut invalid,
        );
        parse_limit(
            object,
            "persistentCacheScratchBytes",
            (MIN_SCRATCH_BYTES, MAX_SCRATCH_BYTES, "one 1 MiB row batch"),
            &mut options.scratch_bytes,
            &mut warnings,
            &mut invalid,
        );
        let mut wait_ms = MAX_WAIT_MS;
        parse_limit(
            object,
            "persistentCacheWaitMs",
            (0, MAX_WAIT_MS, "milliseconds"),
            &mut wait_ms,
            &mut warnings,
            &mut invalid,
        );
        options.wait = Duration::from_millis(wait_ms);
        let mut max_namespaces = DEFAULT_MAX_NAMESPACES as u64;
        parse_limit(
            object,
            "persistentCacheMaxNamespaces",
            (1, MAX_NAMESPACES as u64, "the current build's namespace"),
            &mut max_namespaces,
            &mut warnings,
            &mut invalid,
        );
        options.max_namespaces = max_namespaces as usize;
        if !invalid.is_empty() {
            options.invalid = Some(format!(
                "persistent cache disabled for this session: {}",
                invalid.join("; ")
            ));
        }
        (options, warnings)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CacheOptionsUpdate {
    enabled: RuntimeOption<bool>,
    page_bytes: RuntimeOption<u64>,
    scratch_bytes: RuntimeOption<u64>,
    wait: RuntimeOption<Duration>,
    /// Runtime settings may come from workspace-controlled files: the
    /// directory is an initialization setting only (spec §11.1).
    directory_present: bool,
    /// So is the namespace count (l4d-1ypi).
    max_namespaces_present: bool,
}

impl CacheOptionsUpdate {
    pub(crate) fn reset() -> Self {
        Self {
            enabled: RuntimeOption::Reset,
            page_bytes: RuntimeOption::Reset,
            scratch_bytes: RuntimeOption::Reset,
            wait: RuntimeOption::Reset,
            directory_present: false,
            max_namespaces_present: false,
        }
    }
}

fn runtime_bytes(
    object: &Map<String, Value>,
    field: &str,
    minimum: u64,
    maximum: u64,
    floor: &str,
) -> RuntimeOption<u64> {
    match object.get(field) {
        None => RuntimeOption::Absent,
        Some(Value::Null) => RuntimeOption::Reset,
        Some(value) => match bytes(value, field, minimum, maximum, floor) {
            Bytes::Value(value) | Bytes::Clamped(value, _) => RuntimeOption::Value(value),
            Bytes::Invalid(error) => RuntimeOption::Invalid(error),
        },
    }
}

pub(crate) fn parse_runtime_cache_options(object: &Map<String, Value>) -> CacheOptionsUpdate {
    CacheOptionsUpdate {
        enabled: match object.get("persistentCacheEnabled") {
            None => RuntimeOption::Absent,
            Some(Value::Null) => RuntimeOption::Reset,
            Some(Value::Bool(value)) => RuntimeOption::Value(*value),
            Some(_) => RuntimeOption::Invalid("must be true, false or null".to_string()),
        },
        page_bytes: runtime_bytes(
            object,
            "persistentCachePageBytes",
            MIN_PAGE_BYTES,
            MAX_PAGE_CACHE_BYTES,
            "256 KiB per connection",
        ),
        scratch_bytes: runtime_bytes(
            object,
            "persistentCacheScratchBytes",
            MIN_SCRATCH_BYTES,
            MAX_SCRATCH_BYTES,
            "one 1 MiB row batch",
        ),
        wait: match runtime_bytes(
            object,
            "persistentCacheWaitMs",
            0,
            MAX_WAIT_MS,
            "milliseconds",
        ) {
            RuntimeOption::Value(ms) => RuntimeOption::Value(Duration::from_millis(ms)),
            RuntimeOption::Absent => RuntimeOption::Absent,
            RuntimeOption::Reset => RuntimeOption::Reset,
            RuntimeOption::Invalid(error) => RuntimeOption::Invalid(error),
        },
        directory_present: object.contains_key("persistentCacheDirectory"),
        max_namespaces_present: object.contains_key("persistentCacheMaxNamespaces"),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CacheOptionsOverride {
    enabled: Option<bool>,
    page_bytes: Option<u64>,
    scratch_bytes: Option<u64>,
    wait: Option<Duration>,
}

impl CacheOptionsOverride {
    pub(crate) fn apply(&mut self, update: CacheOptionsUpdate) -> Vec<String> {
        let mut warnings = Vec::new();
        apply_runtime_field(
            &mut self.enabled,
            update.enabled,
            "persistentCacheEnabled",
            &mut warnings,
        );
        apply_runtime_field(
            &mut self.page_bytes,
            update.page_bytes,
            "persistentCachePageBytes",
            &mut warnings,
        );
        apply_runtime_field(
            &mut self.scratch_bytes,
            update.scratch_bytes,
            "persistentCacheScratchBytes",
            &mut warnings,
        );
        apply_runtime_field(
            &mut self.wait,
            update.wait,
            "persistentCacheWaitMs",
            &mut warnings,
        );
        if update.directory_present {
            warnings.push(
                "ignoring runtime persistentCacheDirectory: it is an initialization setting; \
                 restart with initializationOptions.persistentCacheDirectory to change it"
                    .to_string(),
            );
        }
        if update.max_namespaces_present {
            warnings.push(
                "ignoring runtime persistentCacheMaxNamespaces: it is an initialization setting; \
                 restart with initializationOptions.persistentCacheMaxNamespaces to change it"
                    .to_string(),
            );
        }
        warnings
    }

    /// `base.invalid` and `base.directory` always win (spec §11.1 amendment).
    pub(crate) fn effective(&self, base: &CacheOptions) -> CacheOptions {
        CacheOptions {
            enabled: self.enabled.unwrap_or(base.enabled),
            directory: base.directory.clone(),
            page_bytes: self.page_bytes.unwrap_or(base.page_bytes),
            scratch_bytes: self.scratch_bytes.unwrap_or(base.scratch_bytes),
            wait: self.wait.unwrap_or(base.wait),
            max_namespaces: base.max_namespaces,
            invalid: base.invalid.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn init(value: serde_json::Value) -> (CacheOptions, Vec<String>) {
        CacheOptions::parse_initialization(Some(&value))
    }

    #[test]
    fn defaults_follow_the_specification() {
        let (options, warnings) = CacheOptions::parse_initialization(None);
        assert!(warnings.is_empty());
        assert_eq!(options, CacheOptions::default());
        assert!(options.active());
        assert_eq!(options.directory, None);
        assert_eq!(options.limits(), pascal_cache::StoreLimits::default());
        assert_eq!(
            (options.page_bytes, options.scratch_bytes),
            (16 << 20, 16 << 20)
        );
        assert_eq!(options.wait, std::time::Duration::from_millis(25));
        assert_eq!(options.max_namespaces, 2);
    }

    #[test]
    fn the_namespace_count_is_validated_clamped_and_initialization_only() {
        let (two, warnings) = init(json!({"persistentCacheMaxNamespaces": 2}));
        assert!(warnings.is_empty() && two.active());
        assert_eq!(two.limits().max_namespaces, 2);
        let (many, warnings) = init(json!({"persistentCacheMaxNamespaces": 1000}));
        assert_eq!(many.max_namespaces, pascal_cache::MAX_NAMESPACES);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(many.active());
        for bad in [json!(0), json!(-1), json!("3"), json!(1.5)] {
            let (options, _) = init(json!({"persistentCacheMaxNamespaces": bad}));
            let message = options.invalid.expect("invalid");
            assert!(
                message.contains("persistentCacheMaxNamespaces"),
                "{message}"
            );
        }
        let mut runtime = CacheOptionsOverride::default();
        let warnings = runtime.apply(parse_runtime_cache_options(
            json!({"persistentCacheMaxNamespaces": 1})
                .as_object()
                .unwrap(),
        ));
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("initialization setting"),
            "{warnings:?}"
        );
        assert_eq!(runtime.effective(&two).max_namespaces, 2);
    }

    #[test]
    fn nested_and_direct_sections_are_read_like_the_workspace_options() {
        let (direct, _) = init(json!({"persistentCacheEnabled": false}));
        let (nested, _) = init(json!({"pascalLsp": {"persistentCacheEnabled": false}}));
        assert!(!direct.enabled && !nested.enabled);
    }

    #[test]
    fn cache_limits_clamp_downward_and_reject_values_below_the_floor() {
        let (clamped, warnings) = init(json!({
            "persistentCachePageBytes": 1u64 << 30,
            "persistentCacheScratchBytes": 1u64 << 30,
            "persistentCacheWaitMs": 1000
        }));
        assert_eq!(
            (clamped.page_bytes, clamped.scratch_bytes),
            (16 << 20, 16 << 20)
        );
        assert_eq!(clamped.wait, std::time::Duration::from_millis(25));
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(clamped.active());
        let (low, _) = init(json!({"persistentCachePageBytes": MIN_PAGE_BYTES - 1}));
        assert!(!low.active());
        let message = low.invalid.unwrap();
        assert!(
            message.contains("persistentCachePageBytes") && message.contains("786432"),
            "{message}"
        );
        let (low, _) = init(json!({"persistentCacheScratchBytes": 1}));
        assert!(low.invalid.unwrap().contains("1048576"));
        let (zero_wait, _) = init(json!({"persistentCacheWaitMs": 0}));
        assert_eq!(zero_wait.wait, std::time::Duration::ZERO);
        assert!(zero_wait.active());
    }

    #[test]
    fn invalid_initialization_values_turn_persistence_off_with_an_actionable_message() {
        for (value, needle) in [
            (
                json!({"persistentCacheEnabled": "yes"}),
                "persistentCacheEnabled must be true or false",
            ),
            (
                json!({"persistentCacheDirectory": "relative/cache"}),
                "persistentCacheDirectory must be an absolute path",
            ),
            (
                json!({"persistentCacheDirectory": ""}),
                "persistentCacheDirectory must be an absolute path",
            ),
            (
                json!({"persistentCacheDirectory": format!("/{}", "a".repeat(5000))}),
                "longer than 4096 bytes",
            ),
            (
                json!({"persistentCachePageBytes": "big"}),
                "persistentCachePageBytes must be a positive integer",
            ),
        ] {
            let (options, _) = init(value);
            assert!(!options.active());
            let message = options.invalid.unwrap();
            assert!(message.contains(needle), "{message}");
        }
        let (options, _) = init(json!({"persistentCacheDirectory": "/var/cache/me"}));
        assert_eq!(
            options.directory.as_deref(),
            Some(std::path::Path::new("/var/cache/me"))
        );
        let (options, _) = init(json!({"persistentCacheDirectory": null}));
        assert_eq!(options.directory, None);
    }

    #[test]
    fn runtime_cache_options_apply_keep_and_reset() {
        let base = CacheOptions::default();
        let mut runtime = CacheOptionsOverride::default();
        let object = |value: serde_json::Value| value.as_object().unwrap().clone();
        runtime.apply(parse_runtime_cache_options(&object(json!({
            "persistentCacheEnabled": false, "persistentCacheScratchBytes": 2097152
        }))));
        let effective = runtime.effective(&base);
        assert!(!effective.enabled);
        assert_eq!(effective.scratch_bytes, 2 << 20);
        let warnings = runtime.apply(parse_runtime_cache_options(&object(json!({
            "persistentCacheEnabled": false, "persistentCacheScratchBytes": 1
        }))));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(
            runtime.effective(&base).scratch_bytes,
            2 << 20,
            "an invalid value keeps the previous one"
        );
        runtime.apply(CacheOptionsUpdate::reset());
        assert_eq!(runtime.effective(&base), base);
    }

    #[test]
    fn a_runtime_cache_directory_is_ignored_with_a_warning() {
        let base = CacheOptions {
            directory: Some("/init/cache".into()),
            ..CacheOptions::default()
        };
        let mut runtime = CacheOptionsOverride::default();
        let update = parse_runtime_cache_options(
            json!({"persistentCacheDirectory": "/project/redirect"})
                .as_object()
                .unwrap(),
        );
        let warnings = runtime.apply(update);
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("initialization setting"),
            "{warnings:?}"
        );
        assert_eq!(
            runtime.effective(&base).directory.as_deref(),
            Some(std::path::Path::new("/init/cache"))
        );
    }

    #[test]
    fn an_invalid_initialization_value_is_not_repaired_at_runtime() {
        let (base, _) = init(json!({"persistentCachePageBytes": 1}));
        let mut runtime = CacheOptionsOverride::default();
        runtime.apply(parse_runtime_cache_options(
            json!({"persistentCachePageBytes": 16777216})
                .as_object()
                .unwrap(),
        ));
        assert!(!runtime.effective(&base).active());
    }
}

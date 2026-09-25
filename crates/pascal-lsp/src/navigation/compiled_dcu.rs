//! Conservative read-only declaration views for checked-in/authorized DCU bytes.
//!
//! This module deliberately accepts bytes rather than paths: path discovery and
//! read authorization belong to the selected-project workspace resolver. A
//! caller may index [`CompiledUnitDocument::text`] at its stable URI, but must
//! retain this object (or equivalent trusted state) to serve virtual text.

use lint4d::dcu::{DcuPlatform, DcuVersion};
use lsp_types::Url;
use pascal_project::{ProjectContext, ProjectPathEntry, ReadPolicy, content_hash_bytes};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const MAX_DCU_BYTES: usize = 16 * 1024 * 1024;
const MAX_DCU_TOTAL_BYTES: usize = 32 * 1024 * 1024;
const MAX_DCU_TOTAL_TEXT_BYTES: usize = 4 * 1024 * 1024;
const MAX_DCU_IMPORTS: usize = 128;
const MAX_DCU_SEARCH_PATHS: usize = 128;
const MAX_DCU_DIRECTORY_ENTRIES: usize = 4096;
const MAX_DCU_TOTAL_DIRECTORY_ENTRIES: usize = 16384;
const MAX_DCU_DECODED_RECORDS: usize = 32_768;
const MAX_DCU_DECODED_BYTES: usize = 4 * 1024 * 1024;
// A best-effort elapsed-time bound checked between synchronous filesystem
// operations. It cannot preempt a kernel call, but it refuses results as soon
// as a slow read/enumeration returns.
const MAX_DCU_DISCOVERY_TIME: Duration = Duration::from_secs(2);

/// A bounded, generated declaration view over a validated compiled unit.
#[derive(Debug, Clone)]
pub struct CompiledUnitDocument {
    uri: Url,
    text: String,
    unit_name: String,
    version: DcuVersion,
    platform: DcuPlatform,
}

impl CompiledUnitDocument {
    /// Upper bound for retained generated declaration text.
    pub const MAX_TEXT_BYTES: usize = 1024 * 1024;

    /// Parse a DCU byte slice for the one fully verified provider contract.
    ///
    /// Current support is intentionally limited to Delphi 13 Win64. Although
    /// the low-level parser recognizes more magic values, their type/member
    /// field semantics have not been verified as a complete navigation view.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.is_empty() || bytes.len() > MAX_DCU_BYTES {
            return Err("compiled unit exceeds the DCU byte limit".to_string());
        }
        let parsed = lint4d::dcu::types::parse_dcu_for_provider(
            bytes,
            lint4d::dcu::types::ProviderParseLimits {
                max_records: MAX_DCU_DECODED_RECORDS,
                max_decoded_bytes: MAX_DCU_DECODED_BYTES,
            },
        )
        .map_err(|error| error.to_string())?;
        let unit = parsed.unit;
        let exported_type_indices = parsed
            .exported_type_indices
            .into_iter()
            .collect::<HashSet<_>>();
        let class_definition_type_indices = parsed
            .class_definition_type_indices
            .into_iter()
            .collect::<HashSet<_>>();
        if unit.version != DcuVersion::D13 || unit.platform != DcuPlatform::Win64 {
            return Err(
                "compiled unit version/platform is not supported for navigation".to_string(),
            );
        }
        if !safe_unit_name(&unit.name) {
            return Err("compiled unit has an invalid name".to_string());
        }

        let mut type_name_counts = HashMap::<String, usize>::new();
        for (type_index, ty) in unit.types.iter().enumerate() {
            if exported_type_indices.contains(&type_index)
                && class_definition_type_indices.contains(&type_index)
                && safe_identifier(&ty.name)
            {
                *type_name_counts
                    .entry(ty.name.to_ascii_lowercase())
                    .or_default() += 1;
            }
        }

        let mut text = format!("unit {};\n\ninterface\n\n", unit.name);
        let mut emitted = 0usize;
        let mut emitted_names = HashSet::new();
        for (type_index, ty) in unit.types.iter().enumerate() {
            // The parser proves class type declarations for this version, but
            // does not prove the class member signatures/visibility. Emit a
            // type shell only, and refuse case-insensitively ambiguous names.
            let canonical_name = ty.name.to_ascii_lowercase();
            if !exported_type_indices.contains(&type_index)
                || !class_definition_type_indices.contains(&type_index)
                || !safe_identifier(&ty.name)
                || type_name_counts.get(&canonical_name) != Some(&1)
                || !emitted_names.insert(canonical_name)
            {
                continue;
            }
            text.push_str("type\n  ");
            text.push_str(&ty.name);
            text.push_str(" = class end;\n\n");
            emitted += 1;
            if text.len() > Self::MAX_TEXT_BYTES {
                return Err("generated compiled declaration exceeds text limit".to_string());
            }
        }
        if emitted == 0 {
            return Err("compiled unit has no unambiguous supported class types".to_string());
        }
        text.push_str("implementation\n\nend.\n");
        if text.len() > Self::MAX_TEXT_BYTES {
            return Err("generated compiled declaration exceeds text limit".to_string());
        }

        let uri = Url::parse(&format!(
            "lint4d-dcu://d13-win64/{}.pas",
            unit.name.to_ascii_lowercase()
        ))
        .map_err(|error| format!("could not create compiled-unit URI: {error}"))?;
        Ok(Self {
            uri,
            text,
            unit_name: unit.name.to_ascii_lowercase(),
            version: unit.version,
            platform: unit.platform,
        })
    }

    pub fn uri(&self) -> &Url {
        &self.uri
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    fn scope_to_context(&mut self, context: &ProjectContext, content_hash: u64) {
        self.uri.set_path(&format!(
            "/{:016x}/{content_hash:016x}/{}.pas",
            project_context_fingerprint(context),
            self.unit_name
        ));
    }

    pub fn version_name(&self) -> &'static str {
        match self.version {
            DcuVersion::D13 => "D13",
            _ => "unsupported",
        }
    }

    pub fn platform_name(&self) -> &'static str {
        match self.platform {
            DcuPlatform::Win64 => "Win64",
            DcuPlatform::Win32 => "unsupported",
        }
    }

    /// Generated DCU documents are deliberately never edit targets.
    pub const fn editable(&self) -> bool {
        false
    }
}

fn safe_identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// A DCU-backed declaration and the exact content observation used to build
/// it. Call [`Self::is_current`] before delivering queued results or virtual
/// text; content hashing detects same-size replacements with restored stamps.
#[derive(Debug, Clone)]
pub struct AuthorizedCompiledUnit {
    pub document: CompiledUnitDocument,
    path: PathBuf,
    path_entry: ProjectPathEntry,
    content_hash: u64,
}

impl AuthorizedCompiledUnit {
    pub(crate) fn retained_payload_bytes(&self) -> usize {
        let mut bytes = std::mem::size_of::<Self>()
            .saturating_add(self.document.text.len())
            .saturating_add(self.document.unit_name.len())
            .saturating_add(self.document.uri.as_str().len())
            .saturating_add(self.path.as_os_str().len());
        let _ = self
            .path_entry
            .visit_recovery_payload(&mut |payload_bytes| {
                bytes = bytes.saturating_add(payload_bytes);
                Ok(())
            });
        bytes
    }

    pub fn is_current(&self, policy: &ReadPolicy) -> bool {
        policy
            .read_payload_bytes(&self.path_entry, MAX_DCU_BYTES as u64)
            .is_ok_and(|bytes| content_hash_bytes(&bytes) == self.content_hash)
    }

    pub(crate) fn is_current_with_cancel(
        &self,
        policy: &ReadPolicy,
        cancel: &AtomicBool,
    ) -> Result<bool, String> {
        if cancel.load(Ordering::Relaxed) {
            return Err("request cancelled".to_string());
        }
        let bytes = match policy.read_payload_bytes(&self.path_entry, MAX_DCU_BYTES as u64) {
            Ok(bytes) => bytes,
            Err(_) => return Ok(false),
        };
        if cancel.load(Ordering::Relaxed) {
            return Err("request cancelled".to_string());
        }
        Ok(content_hash_bytes(&bytes) == self.content_hash)
    }

    pub fn source_path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn observation(&self) -> (&Path, &ProjectPathEntry, u64) {
        (&self.path, &self.path_entry, self.content_hash)
    }
}

/// Parse only the canonical URI form emitted by this provider. This is a
/// syntactic check, not authorization; a caller must also bind its project
/// fingerprint to a current selected project and validate the content hash.
pub fn virtual_unit_identity(uri: &Url) -> Option<(u64, u64, String)> {
    if uri.scheme() != "lint4d-dcu"
        || uri.host_str() != Some("d13-win64")
        || !uri.username().is_empty()
        || uri.password().is_some()
        || uri.port().is_some()
        || uri.query().is_some()
        || uri.fragment().is_some()
    {
        return None;
    }
    let mut components = uri.path().strip_prefix('/')?.split('/');
    let project_fingerprint = components.next()?;
    let content_fingerprint = components.next()?;
    let filename = components.next()?;
    if components.next().is_some()
        || project_fingerprint.len() != 16
        || !project_fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || project_fingerprint != project_fingerprint.to_ascii_lowercase()
        || content_fingerprint.len() != 16
        || !content_fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || content_fingerprint != content_fingerprint.to_ascii_lowercase()
        || !filename.ends_with(".pas")
    {
        return None;
    }
    let unit_name = filename.strip_suffix(".pas")?;
    if !safe_unit_name(unit_name) || unit_name != unit_name.to_ascii_lowercase() {
        return None;
    }
    let project_fingerprint = u64::from_str_radix(project_fingerprint, 16).ok()?;
    let content_fingerprint = u64::from_str_radix(content_fingerprint, 16).ok()?;
    let canonical = format!(
        "lint4d-dcu://d13-win64/{project_fingerprint:016x}/{content_fingerprint:016x}/{filename}"
    );
    (uri.as_str() == canonical).then(|| {
        (
            project_fingerprint,
            content_fingerprint,
            unit_name.to_string(),
        )
    })
}

fn safe_unit_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 256 && name.split('.').all(safe_identifier)
}

pub(crate) fn project_context_fingerprint(context: &ProjectContext) -> u64 {
    let mut hasher = DefaultHasher::new();
    context.project_file.hash(&mut hasher);
    context.main_source.hash(&mut hasher);
    context.search_path_entries.hash(&mut hasher);
    context.unit_namespaces.hash(&mut hasher);
    let mut aliases = context.unit_aliases.iter().collect::<Vec<_>>();
    aliases.sort_by(|left, right| left.0.cmp(right.0));
    for (alias, unit) in aliases {
        alias.hash(&mut hasher);
        unit.hash(&mut hasher);
    }
    context.defines.hash(&mut hasher);
    context.config.hash(&mut hasher);
    context.platform.hash(&mut hasher);
    context.overrides.hash(&mut hasher);
    context.discovery_complete.hash(&mut hasher);
    context.read_policy.hash(&mut hasher);
    hasher.finish()
}

/// Discover DCUs for imported unit names from one complete selected project
/// context. Only its provenance-bearing unit search paths and requester policy
/// are consulted. Any duplicate candidate or matching Pascal source is a
/// conservative refusal; no source provider is displaced.
pub fn discover_compiled_units(
    context: &ProjectContext,
    imported_units: &[String],
    cancel: &AtomicBool,
) -> Result<Vec<AuthorizedCompiledUnit>, String> {
    let started = Instant::now();
    if !context.discovery_complete
        || imported_units.len() > MAX_DCU_IMPORTS
        || context.search_path_entries.len() > MAX_DCU_SEARCH_PATHS
    {
        return Ok(Vec::new());
    }
    let mut total_bytes = 0usize;
    let mut total_text_bytes = 0usize;
    let mut total_entries = 0usize;
    let mut processed_units = HashSet::new();
    let mut loaded = Vec::new();
    for unit_name in imported_units {
        if cancel.load(Ordering::Relaxed) {
            return Err("request cancelled".to_string());
        }
        if started.elapsed() > MAX_DCU_DISCOVERY_TIME {
            return Ok(Vec::new());
        }
        if !safe_unit_name(unit_name) {
            continue;
        }
        if !processed_units.insert(unit_name.to_ascii_lowercase()) {
            continue;
        }
        let mut candidates = Vec::<PathBuf>::new();
        let mut source_exists = false;
        for search in &context.search_path_entries {
            if cancel.load(Ordering::Relaxed) {
                return Err("request cancelled".to_string());
            }
            if started.elapsed() > MAX_DCU_DISCOVERY_TIME {
                return Ok(Vec::new());
            }
            if !context.read_policy.allows_location(search) {
                continue;
            }
            let Ok(entries) = fs::read_dir(&search.path) else {
                continue;
            };
            let mut visited = 0usize;
            for entry in entries {
                if cancel.load(Ordering::Relaxed) {
                    return Err("request cancelled".to_string());
                }
                if started.elapsed() > MAX_DCU_DISCOVERY_TIME {
                    return Ok(Vec::new());
                }
                visited += 1;
                total_entries = total_entries.saturating_add(1);
                if visited > MAX_DCU_DIRECTORY_ENTRIES
                    || total_entries > MAX_DCU_TOTAL_DIRECTORY_ENTRIES
                {
                    return Ok(Vec::new());
                }
                let Ok(entry) = entry else { continue };
                let path = entry.path();
                let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                    continue;
                };
                if !stem.eq_ignore_ascii_case(unit_name) {
                    continue;
                }
                let extension = path.extension().and_then(|extension| extension.to_str());
                match extension {
                    Some(extension)
                        if extension.eq_ignore_ascii_case("pas")
                            || extension.eq_ignore_ascii_case("pp") =>
                    {
                        if context
                            .read_policy
                            .entry_for_path(&path)
                            .is_some_and(|entry| context.read_policy.allows_location(&entry))
                        {
                            source_exists = true;
                        }
                    }
                    Some(extension) if extension.eq_ignore_ascii_case("dcu") => {
                        if context
                            .read_policy
                            .entry_for_path(&path)
                            .is_some_and(|entry| context.read_policy.allows_location(&entry))
                        {
                            candidates.push(path);
                            if candidates.len() > 1 {
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }
            if candidates.len() > 1 {
                break;
            }
        }
        if source_exists || candidates.len() != 1 {
            continue;
        }
        let path = candidates.pop().expect("one candidate checked above");
        let Some(path_entry) = context.read_policy.entry_for_path(&path) else {
            continue;
        };
        match fs::symlink_metadata(&path) {
            Ok(metadata)
                if metadata.file_type().is_file() && metadata.len() <= MAX_DCU_BYTES as u64 => {}
            _ => continue,
        }
        let bytes = match context
            .read_policy
            .read_payload_bytes(&path_entry, MAX_DCU_BYTES as u64)
        {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        if cancel.load(Ordering::Relaxed) {
            return Err("request cancelled".to_string());
        }
        if started.elapsed() > MAX_DCU_DISCOVERY_TIME {
            return Ok(Vec::new());
        }
        total_bytes = total_bytes.saturating_add(bytes.len());
        if total_bytes > MAX_DCU_TOTAL_BYTES {
            return Ok(Vec::new());
        }
        let content_hash = content_hash_bytes(&bytes);
        let mut document = match CompiledUnitDocument::parse(&bytes) {
            Ok(document) => document,
            Err(_) => continue,
        };
        if cancel.load(Ordering::Relaxed) {
            return Err("request cancelled".to_string());
        }
        if started.elapsed() > MAX_DCU_DISCOVERY_TIME {
            return Ok(Vec::new());
        }
        if document.unit_name != unit_name.to_ascii_lowercase() {
            continue;
        }
        document.scope_to_context(context, content_hash);
        total_text_bytes = total_text_bytes.saturating_add(document.text().len());
        if total_text_bytes > MAX_DCU_TOTAL_TEXT_BYTES {
            return Ok(Vec::new());
        }
        loaded.push(AuthorizedCompiledUnit {
            document,
            path,
            path_entry,
            content_hash,
        });
    }
    Ok(loaded)
}

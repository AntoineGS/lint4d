pub(crate) mod class_parser;
pub mod const_add_info;
pub(crate) mod decl_parser;
pub mod header;
pub mod reader;
pub mod tags;
pub(crate) mod type_defs;
pub mod types;

pub use tags::DcuError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DcuVersion {
    D2010,
    DXE,
    DXE2,
    DXE3,
    DXE4,
    DXE5,
    DXE6,
    DXE7,
    DXE8,
    D10S,
    D101B,
    D102T,
    D103R,
    D104S,
    D11A,
    D12A,
    D13,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DcuPlatform {
    Win32,
    Win64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeKind {
    Class,
    Interface,
    Record,
    Enum,
    Alias,
    Set,
    Array,
    Pointer,
    Procedural,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeRef {
    Resolved(String),
    Unresolved(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Visibility {
    Private,
    Protected,
    Public,
    Published,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MethodKind {
    Constructor,
    Destructor,
    Procedure,
    Function,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamModifier {
    ByValue,
    Var,
    Const,
    Out,
}

#[derive(Debug, Clone)]
pub struct ParamInfo {
    pub name: String,
    pub type_ref: TypeRef,
    pub modifier: ParamModifier,
}

#[derive(Debug, Clone)]
pub struct FieldInfo {
    pub name: String,
    pub type_ref: TypeRef,
    pub visibility: Visibility,
}

#[derive(Debug, Clone)]
pub struct MethodInfo {
    pub name: String,
    pub kind: MethodKind,
    pub params: Vec<ParamInfo>,
    pub return_type: Option<TypeRef>,
}

#[derive(Debug, Clone)]
pub struct TypeInfo {
    pub name: String,
    pub kind: TypeKind,
    pub parent: Option<TypeRef>,
    pub fields: Vec<FieldInfo>,
    pub methods: Vec<MethodInfo>,
    pub interface_guid: Option<[u8; 16]>,
}

#[derive(Debug)]
pub struct DcuUnit {
    pub name: String,
    pub version: DcuVersion,
    pub platform: DcuPlatform,
    pub imported_units: Vec<String>,
    pub types: Vec<TypeInfo>,
}

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(test)]
#[derive(Default)]
struct LoadTestMetrics {
    read_attempts: AtomicUsize,
    warnings: AtomicUsize,
}

type UnitLoad = Arc<OnceLock<Result<Arc<DcuUnit>, DcuError>>>;

/// Cross-unit type resolution context with lazy DCU loading.
///
/// On construction, scans directories to build a unit name → file path index
/// without parsing any DCU files. Units are parsed on first access and cached.
/// Thread-safe: a short-lived mutex protects the unit-entry map and each entry
/// synchronizes its own lazy initialization.
pub struct ProjectContext {
    /// Unit name (case-folded) → file path, for units not yet loaded.
    index: HashMap<String, PathBuf>,
    /// Per-unit load results, keyed by case-folded unit name.
    /// Pre-parsed units get initialized entries at construction; indexed units
    /// get entries on first access; unknown names do not.
    units: Mutex<HashMap<String, UnitLoad>>,
    /// Successful preloaded and lazy-loaded entries; failed entries do not count.
    loaded_units: AtomicUsize,
    #[cfg(test)]
    load_test_metrics: LoadTestMetrics,
}

impl ProjectContext {
    /// Create a context from pre-parsed units (used in tests).
    pub fn from_units(units: Vec<DcuUnit>) -> Self {
        let mut map = HashMap::new();
        for unit in units {
            let key = unit.name.to_lowercase();
            let entry = OnceLock::new();
            entry
                .set(Ok(Arc::new(unit)))
                .expect("new unit cache entry is empty");
            map.insert(key, Arc::new(entry));
        }
        let loaded_units = map.len();
        Self {
            index: HashMap::new(),
            units: Mutex::new(map),
            loaded_units: AtomicUsize::new(loaded_units),
            #[cfg(test)]
            load_test_metrics: LoadTestMetrics::default(),
        }
    }

    /// Scan directories to build a unit name → path index without parsing.
    pub fn from_dcu_paths(paths: &[PathBuf]) -> Result<Self, DcuError> {
        let mut index = HashMap::new();
        for path in paths {
            if path.is_dir() {
                for entry in std::fs::read_dir(path).map_err(DcuError::Io)? {
                    let entry = entry.map_err(DcuError::Io)?;
                    let p = entry.path();
                    if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("dcu"))
                        && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
                    {
                        let key = stem.to_lowercase();
                        // First path wins (mimics Delphi search order)
                        index.entry(key).or_insert_with(|| p.clone());
                    }
                }
            }
        }
        let count = index.len();
        if count > 0 {
            eprintln!("Indexed {} DCU file(s) across search paths", count);
        }
        Ok(Self {
            index,
            units: Mutex::new(HashMap::new()),
            loaded_units: AtomicUsize::new(0),
            #[cfg(test)]
            load_test_metrics: LoadTestMetrics::default(),
        })
    }

    /// Number of DCU files available (indexed, not necessarily loaded).
    pub fn unit_count(&self) -> usize {
        self.index.len() + self.loaded_units.load(Ordering::Relaxed)
    }

    /// Ensure a unit is loaded, parsing its DCU on first access.
    fn ensure_loaded(&self, unit_name: &str) {
        let key = unit_name.to_lowercase();
        let Some(path) = self.index.get(&key).cloned() else {
            return;
        };
        let entry = {
            let mut units = self.units.lock().unwrap();
            Arc::clone(
                units
                    .entry(key.clone())
                    .or_insert_with(|| Arc::new(OnceLock::new())),
            )
        };

        // The per-unit initializer performs the filesystem read and parser
        // work in this entry's once-cell. Its result, including failures, is
        // retained for every caller that already has this indexed entry.
        let _ = entry.get_or_init(|| {
            #[cfg(test)]
            self.load_test_metrics
                .read_attempts
                .fetch_add(1, Ordering::Relaxed);
            let data = match std::fs::read(&path) {
                Ok(data) => data,
                Err(error) => {
                    #[cfg(test)]
                    self.load_test_metrics
                        .warnings
                        .fetch_add(1, Ordering::Relaxed);
                    eprintln!(
                        "lint4d: warning: failed to read {}: {}",
                        path.display(),
                        error
                    );
                    return Err(DcuError::Io(error));
                }
            };
            match types::parse_dcu(&data) {
                Ok(unit) => {
                    let unit = Arc::new(unit);
                    self.loaded_units.fetch_add(1, Ordering::Relaxed);
                    Ok(unit)
                }
                Err(error) => {
                    let wrapped = DcuError::InUnit {
                        unit: key.clone(),
                        source: Box::new(error),
                    };
                    #[cfg(test)]
                    self.load_test_metrics
                        .warnings
                        .fetch_add(1, Ordering::Relaxed);
                    eprintln!("lint4d: warning: skipping {}: {}", path.display(), wrapped);
                    Err(wrapped)
                }
            }
        });
    }

    /// Resolve a type by name, searching the given units in reverse order
    /// (last unit wins, matching Delphi semantics). Returns a clone.
    pub fn resolve_type(&self, name: &str, uses: &[String]) -> Option<TypeInfo> {
        for unit_name in uses {
            self.ensure_loaded(unit_name);
        }

        for unit_name in uses.iter().rev() {
            let key = unit_name.to_lowercase();
            let entry = { self.units.lock().unwrap().get(&key).cloned() };
            if let Some(entry) = entry
                && let Some(Ok(unit)) = entry.get()
            {
                for ty in &unit.types {
                    if ty.name.eq_ignore_ascii_case(name) {
                        return Some(ty.clone());
                    }
                }
            }
        }
        None
    }

    /// Get field names for a class type, searching across used units.
    pub fn get_field_names(&self, class_name: &str, uses: &[String]) -> Vec<String> {
        match self.resolve_type(class_name, uses) {
            Some(ty) => ty.fields.iter().map(|f| f.name.clone()).collect(),
            None => Vec::new(),
        }
    }

    pub fn is_class_type(&self, name: &str, uses: &[String]) -> Option<bool> {
        self.resolve_type(name, uses)
            .map(|t| t.kind == TypeKind::Class)
    }

    pub fn is_interface_type(&self, name: &str, uses: &[String]) -> Option<bool> {
        self.resolve_type(name, uses)
            .map(|t| t.kind == TypeKind::Interface)
    }

    pub fn get_constructor(&self, type_name: &str, uses: &[String]) -> Option<MethodInfo> {
        let ty = self.resolve_type(type_name, uses)?;
        ty.methods
            .into_iter()
            .find(|m| m.kind == MethodKind::Constructor)
    }

    /// Check whether `class_name` descends from `ancestor_name` by walking
    /// the parent chain through resolved DCU types.
    ///
    /// Returns `Some(true)` if the ancestor is found, `Some(false)` if the
    /// full chain was resolved without finding it, or `None` if the chain
    /// could not be fully resolved (type or parent not in loaded DCUs).
    pub fn descends_from(
        &self,
        class_name: &str,
        ancestor_name: &str,
        uses: &[String],
    ) -> Option<bool> {
        if class_name.eq_ignore_ascii_case(ancestor_name) {
            return Some(true);
        }

        let mut current = self.resolve_type(class_name, uses)?;

        // Walk up to 20 levels to avoid infinite loops from bad data.
        for _ in 0..20 {
            match &current.parent {
                Some(TypeRef::Resolved(parent_name)) => {
                    if parent_name.eq_ignore_ascii_case(ancestor_name) {
                        return Some(true);
                    }
                    {
                        let parent_type = self.resolve_type(parent_name, uses)?;
                        current = parent_type
                    }
                }
                _ => return Some(false), // No parent or unresolved — end of chain.
            }
        }
        Some(false)
    }
}

#[cfg(test)]
mod context_loading_tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    const CLASS_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/dcu/d13_win64/Win64/Debug/Lint4dFixture.Classes.dcu");
    const WORKERS: usize = 8;

    fn fixture_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temporary DCU directory");
        std::fs::write(dir.path().join("Retry.dcu"), CLASS_FIXTURE)
            .expect("write indexed DCU fixture");
        dir
    }

    fn uses_retry() -> Vec<String> {
        vec!["Retry".to_string()]
    }

    fn metric(value: &AtomicUsize) -> usize {
        value.load(Ordering::Relaxed)
    }

    #[test]
    fn concurrent_missing_indexed_unit_is_negative_cached_and_fresh_context_retries() {
        let dir = fixture_dir();
        let path = dir.path().join("Retry.dcu");
        let context =
            Arc::new(ProjectContext::from_dcu_paths(&[dir.path().to_path_buf()]).unwrap());
        assert_eq!(context.unit_count(), 1);
        std::fs::remove_file(&path).expect("remove indexed file before first load");

        let start = Arc::new(Barrier::new(WORKERS));
        let first_wave_finished = Arc::new(Barrier::new(WORKERS));
        let workers: Vec<_> = (0..WORKERS)
            .map(|_| {
                let context = Arc::clone(&context);
                let start = Arc::clone(&start);
                let first_wave_finished = Arc::clone(&first_wave_finished);
                thread::spawn(move || {
                    let uses = uses_retry();
                    start.wait();
                    assert!(context.resolve_type("TSimpleClass", &uses).is_none());
                    first_wave_finished.wait();
                    assert!(context.resolve_type("TSimpleClass", &uses).is_none());
                })
            })
            .collect();
        for worker in workers {
            worker.join().expect("worker resolves missing indexed unit");
        }

        assert_eq!(metric(&context.load_test_metrics.read_attempts), 1);
        assert_eq!(metric(&context.load_test_metrics.warnings), 1);
        assert_eq!(
            context.unit_count(),
            1,
            "failed entries are not loaded units"
        );

        std::fs::write(&path, CLASS_FIXTURE).expect("restore indexed DCU fixture");
        assert!(
            context
                .resolve_type("TSimpleClass", &uses_retry())
                .is_none()
        );
        assert_eq!(metric(&context.load_test_metrics.read_attempts), 1);
        assert_eq!(metric(&context.load_test_metrics.warnings), 1);

        let fresh = ProjectContext::from_dcu_paths(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(
            fresh
                .resolve_type("TSimpleClass", &uses_retry())
                .as_ref()
                .map(|ty| ty.name.as_str()),
            Some("TSimpleClass")
        );
        assert_eq!(metric(&fresh.load_test_metrics.read_attempts), 1);
        assert_eq!(metric(&fresh.load_test_metrics.warnings), 0);
        assert_eq!(
            fresh.unit_count(),
            2,
            "loaded indexed units retain old count semantics"
        );
    }

    #[test]
    fn concurrent_successful_indexed_load_is_reused() {
        let dir = fixture_dir();
        let context =
            Arc::new(ProjectContext::from_dcu_paths(&[dir.path().to_path_buf()]).unwrap());
        let start = Arc::new(Barrier::new(WORKERS));
        let workers: Vec<_> = (0..WORKERS)
            .map(|_| {
                let context = Arc::clone(&context);
                let start = Arc::clone(&start);
                thread::spawn(move || {
                    start.wait();
                    context
                        .resolve_type("TSimpleClass", &uses_retry())
                        .expect("fixture type resolves")
                })
            })
            .collect();
        for worker in workers {
            assert_eq!(worker.join().unwrap().name, "TSimpleClass");
        }

        for _ in 0..WORKERS {
            assert_eq!(
                context
                    .resolve_type("TSimpleClass", &uses_retry())
                    .unwrap()
                    .name,
                "TSimpleClass"
            );
        }
        assert_eq!(metric(&context.load_test_metrics.read_attempts), 1);
        assert_eq!(metric(&context.load_test_metrics.warnings), 0);
        assert_eq!(context.unit_count(), 2);
    }

    #[test]
    fn malformed_indexed_unit_failure_is_cached_with_one_warning() {
        let dir = tempfile::tempdir().expect("temporary DCU directory");
        std::fs::write(dir.path().join("Broken.dcu"), b"not a DCU").unwrap();
        let context = ProjectContext::from_dcu_paths(&[dir.path().to_path_buf()]).unwrap();

        for _ in 0..WORKERS {
            assert!(
                context
                    .resolve_type("TAbsent", &["Broken".to_string()])
                    .is_none()
            );
        }

        assert_eq!(metric(&context.load_test_metrics.read_attempts), 1);
        assert_eq!(metric(&context.load_test_metrics.warnings), 1);
        assert_eq!(context.unit_count(), 1);
    }
}

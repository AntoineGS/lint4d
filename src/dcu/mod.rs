pub mod header;
pub mod reader;
pub mod tags;
pub mod types;

pub use tags::DcuError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DcuVersion {
    D13,
    Unknown(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DcuPlatform {
    Win32,
    Win64,
    Osx32,
    IOSSimulator,
    IOSDevice,
    Android,
    Unknown,
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
use std::sync::Mutex;

/// Cross-unit type resolution context with lazy DCU loading.
///
/// On construction, scans directories to build a unit name → file path index
/// without parsing any DCU files. Units are parsed on first access and cached.
/// Thread-safe: uses `Mutex` for the loaded units cache so it can be shared
/// across rayon worker threads.
pub struct ProjectContext {
    /// Unit name (case-folded) → file path, for units not yet loaded.
    index: HashMap<String, PathBuf>,
    /// Loaded units, keyed by case-folded unit name.
    units: Mutex<HashMap<String, DcuUnit>>,
}

impl ProjectContext {
    /// Create a context from pre-parsed units (used in tests).
    pub fn from_units(units: Vec<DcuUnit>) -> Self {
        let mut map = HashMap::new();
        for unit in units {
            map.insert(unit.name.to_lowercase(), unit);
        }
        Self {
            index: HashMap::new(),
            units: Mutex::new(map),
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
                    if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("dcu")) {
                        if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                            let key = stem.to_lowercase();
                            // First path wins (mimics Delphi search order)
                            index.entry(key).or_insert_with(|| p.clone());
                        }
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
        })
    }

    /// Number of DCU files available (indexed, not necessarily loaded).
    pub fn unit_count(&self) -> usize {
        self.index.len() + self.units.lock().unwrap().len()
    }

    /// Ensure a unit is loaded, parsing its DCU on first access.
    fn ensure_loaded(&self, unit_name: &str) {
        let key = unit_name.to_lowercase();
        if self.units.lock().unwrap().contains_key(&key) {
            return;
        }
        let Some(path) = self.index.get(&key) else {
            return;
        };
        let path = path.clone();
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("lint4d: warning: failed to read {}: {}", path.display(), e);
                return;
            }
        };
        match types::parse_dcu(&data) {
            Ok(unit) => {
                self.units.lock().unwrap().insert(key, unit);
            }
            Err(e) => {
                eprintln!("lint4d: warning: skipping {}: {}", path.display(), e);
            }
        }
    }

    /// Resolve a type by name, searching the given units in reverse order
    /// (last unit wins, matching Delphi semantics). Returns a clone.
    pub fn resolve_type(&self, name: &str, uses: &[String]) -> Option<TypeInfo> {
        for unit_name in uses {
            self.ensure_loaded(unit_name);
        }

        let units = self.units.lock().unwrap();
        for unit_name in uses.iter().rev() {
            let key = unit_name.to_lowercase();
            if let Some(unit) = units.get(&key) {
                for ty in &unit.types {
                    if ty.name.eq_ignore_ascii_case(name) {
                        return Some(ty.clone());
                    }
                }
            }
        }
        None
    }

    pub fn is_class_type(&self, name: &str, uses: &[String]) -> Option<bool> {
        self.resolve_type(name, uses).map(|t| t.kind == TypeKind::Class)
    }

    pub fn is_interface_type(&self, name: &str, uses: &[String]) -> Option<bool> {
        self.resolve_type(name, uses).map(|t| t.kind == TypeKind::Interface)
    }

    pub fn get_constructor(&self, type_name: &str, uses: &[String]) -> Option<MethodInfo> {
        let ty = self.resolve_type(type_name, uses)?;
        ty.methods.into_iter().find(|m| m.kind == MethodKind::Constructor)
    }
}

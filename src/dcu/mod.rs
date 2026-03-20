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

/// Cross-unit type resolution context built from DCU files.
pub struct ProjectContext {
    units: HashMap<String, DcuUnit>,
}

impl ProjectContext {
    pub fn from_units(units: Vec<DcuUnit>) -> Self {
        let mut map = HashMap::new();
        for unit in units {
            map.insert(unit.name.clone(), unit);
        }
        Self { units: map }
    }

    pub fn from_dcu_paths(paths: &[PathBuf]) -> Result<Self, DcuError> {
        let mut units = Vec::new();
        for path in paths {
            if path.is_dir() {
                for entry in std::fs::read_dir(path).map_err(DcuError::Io)? {
                    let entry = entry.map_err(DcuError::Io)?;
                    let p = entry.path();
                    if p.extension().map_or(false, |e| e.eq_ignore_ascii_case("dcu")) {
                        let data = std::fs::read(&p).map_err(DcuError::Io)?;
                        match types::parse_dcu(&data) {
                            Ok(unit) => units.push(unit),
                            Err(e) => {
                                eprintln!("lint4d: warning: skipping {}: {}", p.display(), e);
                            }
                        }
                    }
                }
            }
        }
        Ok(Self::from_units(units))
    }

    pub fn unit_count(&self) -> usize {
        self.units.len()
    }

    pub fn resolve_type<'a>(&'a self, name: &str, uses: &[String]) -> Option<&'a TypeInfo> {
        for unit_name in uses.iter().rev() {
            if let Some(unit) = self.units.get(unit_name) {
                for ty in &unit.types {
                    if ty.name.eq_ignore_ascii_case(name) {
                        return Some(ty);
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

    pub fn get_constructor<'a>(&'a self, type_name: &str, uses: &[String]) -> Option<&'a MethodInfo> {
        let ty = self.resolve_type(type_name, uses)?;
        ty.methods.iter().find(|m| m.kind == MethodKind::Constructor)
    }
}

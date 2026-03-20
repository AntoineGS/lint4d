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

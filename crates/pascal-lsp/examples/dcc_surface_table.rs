#[path = "../src/navigation/dcc_surface/format.rs"]
mod format;

use format::TableText;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const USAGE: &str = "Usage: dcc_surface_table --banner <text> --system <System.pas> --sysinit <SysInit.pas> --candidates <file> --surface <file> --defines <file> --members TObject=<file> --members IInterface=<file> --members IUnknown=<file> --members TInterfacedObject=<file> --out <table>\n\nEach names file contains one Pascal identifier per line. Names are deduplicated case-insensitively, preserving the first spelling, and sorted by canonical name.\n";

struct Arguments {
    banner: String,
    system: PathBuf,
    sysinit: PathBuf,
    candidates: PathBuf,
    surface: PathBuf,
    defines: PathBuf,
    members: BTreeMap<String, PathBuf>,
    out: PathBuf,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("dcc_surface_table: {error}\n\n{USAGE}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    let raw_args: Vec<_> = env::args().skip(1).collect();
    if raw_args == ["--help"] {
        print!("{USAGE}");
        return Ok(());
    }

    let args = parse_args(&raw_args)?;
    let candidates = read_names(&args.candidates)?;
    let surface = read_names(&args.surface)?;
    let defines = read_names(&args.defines)?;
    let mut members = BTreeMap::new();
    for type_name in format::MEMBER_TYPES {
        let path = args
            .members
            .get(type_name)
            .expect("parse_args validates every member input");
        members.insert(type_name.to_owned(), read_names(path)?);
    }

    let system = read_bytes(&args.system)?;
    let sysinit = read_bytes(&args.sysinit)?;
    let table = TableText {
        banner: args.banner,
        script_version: 1,
        system_blake3: blake3_hex(&system),
        sysinit_blake3: blake3_hex(&sysinit),
        candidates: candidates.len(),
        surface,
        defines,
        members,
    };
    let text = format::write(&table);
    format::parse(&text).map_err(|error| format!("generated table failed validation: {error}"))?;
    fs::write(&args.out, text)
        .map_err(|error| format!("could not write {}: {error}", args.out.display()))
}

fn parse_args(args: &[String]) -> Result<Arguments, String> {
    let mut banner = None;
    let mut system = None;
    let mut sysinit = None;
    let mut candidates = None;
    let mut surface = None;
    let mut defines = None;
    let mut members = BTreeMap::new();
    let mut out = None;
    let mut index = 0;

    while index < args.len() {
        let option = args[index].as_str();
        index += 1;
        match option {
            "--banner" => set_once(&mut banner, take_value(args, &mut index, option)?, option)?,
            "--system" => set_once(
                &mut system,
                PathBuf::from(take_value(args, &mut index, option)?),
                option,
            )?,
            "--sysinit" => set_once(
                &mut sysinit,
                PathBuf::from(take_value(args, &mut index, option)?),
                option,
            )?,
            "--candidates" => set_once(
                &mut candidates,
                PathBuf::from(take_value(args, &mut index, option)?),
                option,
            )?,
            "--surface" => set_once(
                &mut surface,
                PathBuf::from(take_value(args, &mut index, option)?),
                option,
            )?,
            "--defines" => set_once(
                &mut defines,
                PathBuf::from(take_value(args, &mut index, option)?),
                option,
            )?,
            "--members" => {
                let value = take_value(args, &mut index, option)?;
                let (type_name, path) = value
                    .split_once('=')
                    .ok_or_else(|| "--members requires TypeName=<file>".to_owned())?;
                if !format::MEMBER_TYPES.contains(&type_name) {
                    return Err(format!("unsupported member type {type_name:?}"));
                }
                if path.is_empty() {
                    return Err(format!("--members {type_name} requires a file path"));
                }
                if members
                    .insert(type_name.to_owned(), PathBuf::from(path))
                    .is_some()
                {
                    return Err(format!("duplicate --members entry for {type_name}"));
                }
            }
            "--out" => set_once(
                &mut out,
                PathBuf::from(take_value(args, &mut index, option)?),
                option,
            )?,
            _ => return Err(format!("unknown option {option:?}")),
        }
    }

    for type_name in format::MEMBER_TYPES {
        if !members.contains_key(type_name) {
            return Err(format!("missing --members {type_name}=<file>"));
        }
    }

    Ok(Arguments {
        banner: required(banner, "--banner")?,
        system: required(system, "--system")?,
        sysinit: required(sysinit, "--sysinit")?,
        candidates: required(candidates, "--candidates")?,
        surface: required(surface, "--surface")?,
        defines: required(defines, "--defines")?,
        members,
        out: required(out, "--out")?,
    })
}

fn take_value(args: &[String], index: &mut usize, option: &str) -> Result<String, String> {
    let value = args
        .get(*index)
        .cloned()
        .ok_or_else(|| format!("missing value for {option}"))?;
    *index += 1;
    Ok(value)
}

fn set_once<T>(slot: &mut Option<T>, value: T, option: &str) -> Result<(), String> {
    if slot.replace(value).is_some() {
        return Err(format!("duplicate {option}"));
    }
    Ok(())
}

fn required<T>(value: Option<T>, option: &str) -> Result<T, String> {
    value.ok_or_else(|| format!("missing required option {option}"))
}

fn read_names(path: &Path) -> Result<Vec<String>, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    sorted_unique_names(text.lines().map(str::to_owned))
        .map_err(|error| format!("{}: {error}", path.display()))
}

fn sorted_unique_names(names: impl IntoIterator<Item = String>) -> Result<Vec<String>, String> {
    let mut unique = BTreeMap::new();
    for (index, name) in names.into_iter().enumerate() {
        let canonical = format::canonical_name(&name)
            .ok_or_else(|| format!("line {}: invalid identifier {name:?}", index + 1))?;
        unique.entry(canonical).or_insert(name);
    }
    Ok(unique.into_values().collect())
}

fn read_bytes(path: &Path) -> Result<Vec<u8>, String> {
    fs::read(path).map_err(|error| format!("could not read {}: {error}", path.display()))
}

fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deduplicates_case_insensitively_and_keeps_first_spelling() {
        let names = sorted_unique_names(
            ["Writeln", "FREE", "free", "Length"]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap();

        assert_eq!(names, ["FREE", "Length", "Writeln"]);
    }

    #[test]
    fn rejects_invalid_names_with_their_input_line_number() {
        let error = sorted_unique_names(["Valid", "1abc"].map(str::to_owned)).unwrap_err();

        assert!(error.contains("line 2"), "{error}");
    }
}

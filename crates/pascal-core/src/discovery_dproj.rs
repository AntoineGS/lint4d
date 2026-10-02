use crate::types::{FileInfo, FileType};
use quick_xml::events::{BytesRef, Event};
use quick_xml::{Reader, XmlVersion};
use std::path::{Path, PathBuf};

/// Parse a Delphi `.dproj` (MSBuild XML) file and return the list of source
/// files referenced via `<DCCReference Include="..."/>` elements.
///
/// Paths stored in `.dproj` files use Windows backslash separators. This
/// function normalises them to forward slashes and resolves them relative to
/// the directory containing the `.dproj` file.
///
/// Results are sorted by path for deterministic output.
pub fn parse_dproj(dproj_path: &Path) -> Result<Vec<FileInfo>, String> {
    let content = std::fs::read_to_string(dproj_path)
        .map_err(|e| format!("Failed to read {}: {}", dproj_path.display(), e))?;

    let base_dir = dproj_path.parent().unwrap_or_else(|| Path::new("."));

    let mut reader = Reader::from_str(&content);
    reader.config_mut().trim_text(true);

    let mut results: Vec<FileInfo> = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Empty(e)) | Ok(Event::Start(e)) => {
                // Match local name ignoring any namespace prefix.
                let local_name = e.local_name();
                if local_name.as_ref() == "DCCReference" {
                    for attr in e.attributes() {
                        let attr = attr.map_err(|err| {
                            format!("Attribute error in {}: {}", dproj_path.display(), err)
                        })?;
                        if attr.key.local_name().as_ref() == "Include" {
                            let value =
                                attr.normalized_value(XmlVersion::Implicit1_0)
                                    .map_err(|err| {
                                        format!(
                                            "Failed to unescape attribute in {}: {}",
                                            dproj_path.display(),
                                            err
                                        )
                                    })?;
                            // Normalise Windows path separators.
                            let normalised = value.replace('\\', "/");
                            let file_path = base_dir.join(PathBuf::from(&normalised));

                            // Only include files with recognised Delphi extensions.
                            let file_type = file_path
                                .extension()
                                .and_then(|ext| ext.to_str())
                                .and_then(FileType::from_extension);

                            if let Some(file_type) = file_type {
                                results.push(FileInfo {
                                    path: file_path,
                                    file_type,
                                });
                            }
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(format!(
                    "XML parse error in {}: {}",
                    dproj_path.display(),
                    e
                ));
            }
            _ => {}
        }
    }

    results.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(results)
}

/// Extract the `<ProjectVersion>` property from a `.dproj` file.
///
/// Returns `None` if the element is not found. This value maps to a
/// BDS (RAD Studio) version for locating the correct IDE installation.
pub fn parse_project_version(dproj_path: &Path) -> Result<Option<String>, String> {
    let content = std::fs::read_to_string(dproj_path)
        .map_err(|e| format!("Failed to read {}: {}", dproj_path.display(), e))?;

    // Collected until the closing tag: entity references arrive as separate
    // events between text runs.
    let mut reader = Reader::from_str(&content);

    let mut version: Option<String> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                if e.local_name().as_ref() == "ProjectVersion" {
                    version = Some(String::new());
                }
            }
            Ok(Event::Text(e)) => {
                if let Some(version) = version.as_mut() {
                    version.push_str(&e);
                }
            }
            Ok(Event::GeneralRef(e)) => {
                if let Some(version) = version.as_mut() {
                    let resolved = reference_text(&e).map_err(|err| {
                        format!(
                            "Failed to unescape text in {}: {}",
                            dproj_path.display(),
                            err
                        )
                    })?;
                    version.push_str(&resolved);
                }
            }
            Ok(Event::End(_)) if version.is_some() => {
                let version = version.take().unwrap_or_default();
                let version = version.trim();
                return Ok((!version.is_empty()).then(|| version.to_string()));
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(format!(
                    "XML parse error in {}: {}",
                    dproj_path.display(),
                    e
                ));
            }
            _ => {}
        }
    }
    Ok(None)
}

/// The text an XML entity or character reference stands for.
fn reference_text(reference: &BytesRef<'_>) -> Result<String, String> {
    if let Some(character) = reference
        .resolve_char_ref()
        .map_err(|error| error.to_string())?
    {
        return Ok(character.to_string());
    }
    quick_xml::escape::resolve_predefined_entity(reference)
        .map(str::to_owned)
        .ok_or_else(|| format!("unknown XML entity `&{};`", &**reference))
}

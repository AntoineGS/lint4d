use crate::types::{FileInfo, FileType};
use quick_xml::Reader;
use quick_xml::events::Event;
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
                if local_name.as_ref() == b"DCCReference" {
                    for attr in e.attributes() {
                        let attr = attr.map_err(|err| {
                            format!("Attribute error in {}: {}", dproj_path.display(), err)
                        })?;
                        if attr.key.local_name().as_ref() == b"Include" {
                            let value = attr.unescape_value().map_err(|err| {
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

    let mut reader = Reader::from_str(&content);
    reader.config_mut().trim_text(true);

    let mut in_project_version = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                if e.local_name().as_ref() == b"ProjectVersion" {
                    in_project_version = true;
                }
            }
            Ok(Event::Text(e)) if in_project_version => {
                let text = e.unescape().map_err(|err| {
                    format!(
                        "Failed to unescape text in {}: {}",
                        dproj_path.display(),
                        err
                    )
                })?;
                return Ok(Some(text.trim().to_string()));
            }
            Ok(Event::End(_)) if in_project_version => {
                return Ok(None);
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

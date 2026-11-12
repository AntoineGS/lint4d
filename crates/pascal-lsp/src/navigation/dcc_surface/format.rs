use std::collections::BTreeMap;

pub(crate) const MEMBER_TYPES: [&str; 4] =
    ["TObject", "IInterface", "IUnknown", "TInterfacedObject"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TableText {
    pub(crate) banner: String,
    pub(crate) script_version: u32,
    pub(crate) system_blake3: String,
    pub(crate) sysinit_blake3: String,
    pub(crate) candidates: usize,
    pub(crate) surface: Vec<String>,
    pub(crate) defines: Vec<String>,
    pub(crate) members: BTreeMap<String, Vec<String>>,
}

pub(crate) fn parse(text: &str) -> Result<TableText, String> {
    let lines: Vec<_> = text.lines().collect();
    let mut cursor = 0;

    expect_line(&lines, &mut cursor, "# dcc-surface v1")?;
    let banner = header_value(&lines, &mut cursor, "# banner: ")?;
    if banner.is_empty() {
        return Err(line_error(cursor, "banner must not be empty"));
    }

    let script_version = parse_integer::<u32>(
        header_value(&lines, &mut cursor, "# script-version: ")?,
        cursor,
        "script version",
    )?;
    let system_blake3 = header_value(&lines, &mut cursor, "# system-blake3: ")?;
    validate_hash(&system_blake3, cursor)?;
    let sysinit_blake3 = header_value(&lines, &mut cursor, "# sysinit-blake3: ")?;
    validate_hash(&sysinit_blake3, cursor)?;
    let candidates = parse_integer::<usize>(
        header_value(&lines, &mut cursor, "# candidates: ")?,
        cursor,
        "candidate count",
    )?;

    let surface = parse_section(&lines, &mut cursor, "[surface]")?;
    let defines = parse_section(&lines, &mut cursor, "[defines]")?;
    let mut members = BTreeMap::new();
    for type_name in MEMBER_TYPES {
        members.insert(
            type_name.to_owned(),
            parse_section(&lines, &mut cursor, &format!("[members {type_name}]"))?,
        );
    }

    if cursor < lines.len() {
        return Err(line_error(
            cursor + 1,
            "unexpected content after final section",
        ));
    }

    Ok(TableText {
        banner,
        script_version,
        system_blake3,
        sysinit_blake3,
        candidates,
        surface,
        defines,
        members,
    })
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn write(table: &TableText) -> String {
    let mut output = format!(
        "# dcc-surface v1\n# banner: {}\n# script-version: {}\n# system-blake3: {}\n# sysinit-blake3: {}\n# candidates: {}\n",
        table.banner,
        table.script_version,
        table.system_blake3,
        table.sysinit_blake3,
        table.candidates
    );

    push_section(&mut output, "[surface]", &table.surface);
    push_section(&mut output, "[defines]", &table.defines);
    for type_name in MEMBER_TYPES {
        let section = format!("[members {type_name}]");
        let names = table
            .members
            .get(type_name)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        push_section(&mut output, &section, names);
    }

    output
}

pub(crate) fn canonical_name(name: &str) -> Option<String> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 63 {
        return None;
    }

    let first = bytes[0];
    if !first.is_ascii_alphabetic() && first != b'_' {
        return None;
    }
    if !bytes[1..]
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        return None;
    }

    Some(name.to_ascii_lowercase())
}

fn expect_line(lines: &[&str], cursor: &mut usize, expected: &str) -> Result<(), String> {
    match lines.get(*cursor) {
        Some(line) if *line == expected => {
            *cursor += 1;
            Ok(())
        }
        Some(line) => Err(line_error(
            *cursor + 1,
            &format!("expected {expected:?}, found {line:?}"),
        )),
        None => Err(line_error(
            *cursor + 1,
            &format!("expected {expected:?}, found end of file"),
        )),
    }
}

fn header_value(lines: &[&str], cursor: &mut usize, prefix: &str) -> Result<String, String> {
    let line_number = *cursor + 1;
    match lines
        .get(*cursor)
        .and_then(|line| line.strip_prefix(prefix))
    {
        Some(value) => {
            *cursor += 1;
            Ok(value.to_owned())
        }
        None => Err(line_error(
            line_number,
            &format!("expected header beginning with {prefix:?}"),
        )),
    }
}

fn parse_integer<T>(value: String, line_number: usize, description: &str) -> Result<T, String>
where
    T: std::str::FromStr,
{
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(line_error(
            line_number,
            &format!("invalid {description} {value:?}"),
        ));
    }
    value
        .parse()
        .map_err(|_| line_error(line_number, &format!("invalid {description} {value:?}")))
}

fn validate_hash(hash: &str, line_number: usize) -> Result<(), String> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(line_error(
            line_number,
            "BLAKE3 hash must be 64 lowercase hexadecimal characters",
        ));
    }
    Ok(())
}

fn parse_section(lines: &[&str], cursor: &mut usize, section: &str) -> Result<Vec<String>, String> {
    expect_line(lines, cursor, section)?;
    let mut names = Vec::new();
    let mut previous: Option<String> = None;

    while let Some(name) = lines.get(*cursor) {
        if name.starts_with('[') {
            break;
        }
        let line_number = *cursor + 1;
        let canonical = canonical_name(name)
            .ok_or_else(|| line_error(line_number, &format!("invalid identifier {name:?}")))?;
        match previous.as_deref() {
            Some(previous) if canonical.as_str() == previous => {
                return Err(line_error(
                    line_number,
                    &format!("duplicate canonical name {name:?}"),
                ));
            }
            Some(previous) if canonical.as_str() < previous => {
                return Err(line_error(
                    line_number,
                    &format!("name {name:?} is not sorted by canonical name"),
                ));
            }
            _ => {}
        }
        previous = Some(canonical);
        names.push((*name).to_owned());
        *cursor += 1;
    }

    Ok(names)
}

fn push_section(output: &mut String, section: &str, names: &[String]) {
    output.push_str(section);
    output.push('\n');

    let mut sorted: Vec<_> = names.iter().collect();
    sorted.sort_by_key(|name| name.to_ascii_lowercase());
    for name in sorted {
        output.push_str(name);
        output.push('\n');
    }
}

fn line_error(line_number: usize, message: &str) -> String {
    format!("line {line_number}: {message}")
}

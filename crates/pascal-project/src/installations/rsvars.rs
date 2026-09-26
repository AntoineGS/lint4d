use super::PropertyMap;

const MAX_RS_VARS_BYTES: usize = 4 * 1024 * 1024;
const MAX_EXPANSION_BYTES: usize = 1024 * 1024;
const MAX_TOTAL_PROPERTY_BYTES: usize = 16 * 1024 * 1024;

pub(crate) fn parse_rsvars(text: &str, initial: &PropertyMap) -> Result<PropertyMap, String> {
    if text.len() > MAX_RS_VARS_BYTES {
        return Err("rsvars input exceeds 4 MiB".to_owned());
    }
    let mut properties = PropertyMap::new();
    for (name, value) in initial {
        let name = name.to_ascii_lowercase();
        validate_name(&name)?;
        properties.insert(name, value.clone());
    }
    check_total(&properties)?;

    for line in text.lines() {
        let mut line = line.trim();
        if let Some(rest) = line.strip_prefix('@') {
            line = rest.trim_start();
        }
        let Some(rest) = line
            .get(..3)
            .filter(|prefix| prefix.eq_ignore_ascii_case("set"))
        else {
            continue;
        };
        let _ = rest;
        let after_set = &line[3..];
        if !after_set.starts_with(char::is_whitespace) {
            continue;
        }
        let mut assignment = after_set.trim();
        if assignment.starts_with('"') && assignment.ends_with('"') && assignment.len() >= 2 {
            assignment = &assignment[1..assignment.len() - 1];
        }
        let (name, value) = assignment
            .split_once('=')
            .ok_or_else(|| "malformed SET assignment".to_owned())?;
        let key = name.trim().to_ascii_lowercase();
        validate_name(&key)?;
        let expanded = expand_once(value, &properties)?;
        properties.insert(key, expanded);
        check_total(&properties)?;
    }
    Ok(properties)
}

fn validate_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    if !matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(format!("invalid property name `{name}`"));
    }
    Ok(())
}

fn expand_once(value: &str, properties: &PropertyMap) -> Result<String, String> {
    let mut result = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find('%') {
        result.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('%') else {
            result.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let name = after[..end].to_ascii_lowercase();
        if let Some(replacement) = properties.get(&name) {
            result.push_str(replacement);
        } else {
            result.push('%');
            result.push_str(&after[..end]);
            result.push('%');
        }
        rest = &after[end + 1..];
        if result.len() > MAX_EXPANSION_BYTES {
            return Err("rsvars property expansion exceeds 1 MiB".to_owned());
        }
    }
    result.push_str(rest);
    if result.len() > MAX_EXPANSION_BYTES {
        return Err("rsvars property expansion exceeds 1 MiB".to_owned());
    }
    Ok(result)
}

fn check_total(properties: &PropertyMap) -> Result<(), String> {
    let bytes = properties.iter().try_fold(0usize, |total, (name, value)| {
        total.checked_add(name.len())?.checked_add(value.len())
    });
    if match bytes {
        Some(bytes) => bytes > MAX_TOTAL_PROPERTY_BYTES,
        None => true,
    } {
        return Err("rsvars properties exceed 16 MiB".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::PropertyMap;
    use super::parse_rsvars;

    #[test]
    fn rsvars_expands_assignments_in_order() {
        let parsed = parse_rsvars(
            "@SET BDS=C:\\Old\\37.0\r\nSET \"BDSLIB=%BDS%\\lib\"\r\n",
            &PropertyMap::new(),
        )
        .unwrap();
        assert_eq!(parsed["bdslib"], r"C:\Old\37.0\lib");
    }

    #[test]
    fn rsvars_keeps_unknown_expansions_and_accepts_empty_values() {
        let parsed = parse_rsvars(
            "SET EMPTY=\r\nSET MIXED=%MISSING%\\tail\r\necho ignored by command interpreter\r\n",
            &PropertyMap::new(),
        )
        .unwrap();
        assert_eq!(parsed["empty"], "");
        assert_eq!(parsed["mixed"], r"%MISSING%\tail");
        assert!(!parsed.contains_key("path"));
    }

    #[test]
    fn rsvars_rejects_malformed_supported_assignments() {
        assert!(parse_rsvars("SET broken\n", &PropertyMap::new()).is_err());
    }
}

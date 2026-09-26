use super::PropertyMap;

const MAX_RS_VARS_BYTES: usize = 4 * 1024 * 1024;
const MAX_EXPANSION_BYTES: usize = 1024 * 1024;
const MAX_TOTAL_PROPERTY_BYTES: usize = 16 * 1024 * 1024;

pub(crate) fn parse_rsvars(text: &str, initial: &PropertyMap) -> Result<PropertyMap, String> {
    if text.len() > MAX_RS_VARS_BYTES {
        return Err("rsvars input exceeds 4 MiB".to_owned());
    }
    let mut properties = PropertyMap::new();
    let mut total_property_bytes = 0;
    for (name, value) in initial {
        let name = name.to_ascii_lowercase();
        validate_name(&name)?;
        insert_bounded(
            &mut properties,
            name,
            value.clone(),
            &mut total_property_bytes,
        )?;
    }

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
        insert_bounded(&mut properties, key, expanded, &mut total_property_bytes)?;
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

fn insert_bounded(
    properties: &mut PropertyMap,
    name: String,
    value: String,
    total: &mut usize,
) -> Result<(), String> {
    let replaced = match properties.get(&name) {
        Some(previous) => name
            .len()
            .checked_add(previous.len())
            .ok_or_else(|| "rsvars properties exceed 16 MiB".to_owned())?,
        None => 0,
    };
    let Some(updated_total) = total
        .checked_sub(replaced)
        .and_then(|bytes| bytes.checked_add(name.len()))
        .and_then(|bytes| bytes.checked_add(value.len()))
    else {
        return Err("rsvars properties exceed 16 MiB".to_owned());
    };
    if updated_total > MAX_TOTAL_PROPERTY_BYTES {
        return Err("rsvars properties exceed 16 MiB".to_owned());
    }
    *total = updated_total;
    properties.insert(name, value);
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

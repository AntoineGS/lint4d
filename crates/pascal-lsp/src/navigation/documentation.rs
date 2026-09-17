use super::{Origin, Span, Symbol, canonical_name};
use lsp_types::MarkupKind;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const MAX_DOCUMENTATION_SOURCE_BYTES: usize = 2 * 1024 * 1024;
const MAX_DOCUMENTATION_COMMENTS: usize = 8 * 1024;
const MAX_DOCUMENTATION_COMMENT_BYTES: usize = 64 * 1024;
const MAX_DOCUMENTATION_TAG_BYTES: usize = 4096;
const MAX_DOCUMENTATION_NESTING: usize = 64;
pub(crate) const MAX_DOCUMENTATION_RENDERED_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Documentation {
    summary: RichText,
    remarks: RichText,
    parameters: BTreeMap<String, RichText>,
    returns: RichText,
}

impl Documentation {
    pub(crate) fn render(
        &self,
        format: MarkupKind,
        cancel: &AtomicBool,
    ) -> Result<Option<String>, String> {
        self.render_with_parameters(format, cancel, true)
    }

    pub(crate) fn render_signature(
        &self,
        format: MarkupKind,
        cancel: &AtomicBool,
    ) -> Result<Option<String>, String> {
        self.render_with_parameters(format, cancel, false)
    }

    fn render_with_parameters(
        &self,
        format: MarkupKind,
        cancel: &AtomicBool,
        include_parameters: bool,
    ) -> Result<Option<String>, String> {
        let mut sections = Vec::new();
        if let Some(summary) = self.summary.render(format.clone(), cancel)? {
            sections.push(summary);
        }
        if !self.remarks.is_empty() {
            if let Some(section) = render_section("Remarks", &self.remarks, format.clone(), cancel)?
            {
                sections.push(section);
            }
        }
        if include_parameters && !self.parameters.is_empty() {
            check_cancel(cancel)?;
            let mut parameter_lines = Vec::new();
            for (name, value) in &self.parameters {
                check_cancel(cancel)?;
                let rendered = value.render(format.clone(), cancel)?.unwrap_or_default();
                if rendered.is_empty() {
                    continue;
                }
                parameter_lines.push(match format {
                    MarkupKind::Markdown => {
                        format!("- {} — {rendered}", markdown_code_span(name))
                    }
                    MarkupKind::PlainText => {
                        format!("{name}: {rendered}")
                    }
                });
            }
            if !parameter_lines.is_empty() {
                sections.push(match format {
                    MarkupKind::Markdown => {
                        format!("**Parameters**\n\n{}", parameter_lines.join("\n"))
                    }
                    MarkupKind::PlainText => {
                        format!("Parameters:\n{}", parameter_lines.join("\n"))
                    }
                });
            }
        }
        if !self.returns.is_empty() {
            if let Some(section) = render_section("Returns", &self.returns, format, cancel)? {
                sections.push(section);
            }
        }
        if sections.is_empty() {
            return Ok(None);
        }
        let result = sections
            .into_iter()
            .filter(|section| !section.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
            .trim()
            .to_owned();
        if result.len() > MAX_DOCUMENTATION_RENDERED_BYTES {
            return Err(format!(
                "documentation exceeds the {MAX_DOCUMENTATION_RENDERED_BYTES}-byte limit"
            ));
        }
        Ok((!result.is_empty()).then_some(result))
    }

    pub(crate) fn parameter(
        &self,
        name: &str,
        format: MarkupKind,
        cancel: &AtomicBool,
    ) -> Result<Option<String>, String> {
        let Some(value) = self.parameters.get(&canonical_name(name)) else {
            return Ok(None);
        };
        value.render(format, cancel)
    }
}

fn render_section(
    title: &str,
    value: &RichText,
    format: MarkupKind,
    cancel: &AtomicBool,
) -> Result<Option<String>, String> {
    let Some(rendered) = value.render(format.clone(), cancel)? else {
        return Ok(None);
    };
    Ok(Some(match format {
        MarkupKind::Markdown => format!("**{title}**\n\n{rendered}"),
        MarkupKind::PlainText => format!("{title}:\n{rendered}"),
    }))
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("request cancelled".to_string())
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RichText {
    fragments: Vec<Fragment>,
}

impl RichText {
    fn is_empty(&self) -> bool {
        self.fragments.iter().all(|fragment| match fragment {
            Fragment::Text(value) | Fragment::Code(value) => value.trim().is_empty(),
        })
    }

    fn push_text(&mut self, value: &str) {
        let value = collapse_whitespace(&decode_entities(value));
        if value.is_empty() {
            return;
        }
        match self.fragments.last_mut() {
            Some(Fragment::Text(current)) => current.push_str(&value),
            _ => self.fragments.push(Fragment::Text(value)),
        }
    }

    fn push_code(&mut self, value: &str) {
        let value = sanitize_control(&decode_entities(value));
        if value.is_empty() {
            return;
        }
        match self.fragments.last_mut() {
            Some(Fragment::Code(current)) => current.push_str(&value),
            _ => self.fragments.push(Fragment::Code(value)),
        }
    }

    fn append(&mut self, other: Self) {
        for fragment in other.fragments {
            match (self.fragments.last_mut(), fragment) {
                (Some(Fragment::Text(current)), Fragment::Text(value)) => current.push_str(&value),
                (Some(Fragment::Code(current)), Fragment::Code(value)) => current.push_str(&value),
                (_, fragment) => self.fragments.push(fragment),
            }
        }
    }

    fn render(&self, format: MarkupKind, cancel: &AtomicBool) -> Result<Option<String>, String> {
        let mut result = String::new();
        for fragment in &self.fragments {
            check_cancel(cancel)?;
            match fragment {
                Fragment::Text(value) => match format {
                    MarkupKind::Markdown => result.push_str(&escape_markdown(value)),
                    MarkupKind::PlainText => result.push_str(&sanitize_control(value)),
                },
                Fragment::Code(value) => match format {
                    MarkupKind::Markdown => result.push_str(&markdown_code_span(value)),
                    MarkupKind::PlainText => result.push_str(&sanitize_control(value)),
                },
            }
            if result.len() > MAX_DOCUMENTATION_RENDERED_BYTES {
                return Err(format!(
                    "documentation exceeds the {MAX_DOCUMENTATION_RENDERED_BYTES}-byte limit"
                ));
            }
        }
        let result = result.trim().to_owned();
        Ok((!result.is_empty()).then_some(result))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Fragment {
    Text(String),
    Code(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    General,
    Summary,
    Remarks,
    Parameter(Vec<String>),
    Returns,
}

impl Target {
    fn documentation_target<'a>(
        &self,
        documentation: &'a mut Documentation,
    ) -> Option<&'a mut RichText> {
        match self {
            Self::General => None,
            Self::Summary => Some(&mut documentation.summary),
            Self::Remarks => Some(&mut documentation.remarks),
            Self::Returns => Some(&mut documentation.returns),
            Self::Parameter(_) => None,
        }
    }
}

struct MarkupParser {
    documentation: Documentation,
    general: RichText,
    target: Target,
    stack: Vec<OpenTag>,
    code: Option<(Target, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OpenTag {
    name: String,
    previous_target: Target,
    kind: OpenTagKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenTagKind {
    Section,
    Code,
    Other,
}

impl MarkupParser {
    fn new() -> Self {
        Self {
            documentation: Documentation {
                summary: RichText::default(),
                remarks: RichText::default(),
                parameters: BTreeMap::new(),
                returns: RichText::default(),
            },
            general: RichText::default(),
            target: Target::General,
            stack: Vec::new(),
            code: None,
        }
    }

    fn push_text(&mut self, value: &str) {
        if let Some((_, current)) = self.code.as_mut() {
            current.push_str(value);
            return;
        }
        if let Some(target) = self.target.documentation_target(&mut self.documentation) {
            target.push_text(value);
        } else if let Target::Parameter(names) = &self.target {
            let value = collapse_whitespace(&decode_entities(value));
            for name in names {
                self.documentation
                    .parameters
                    .entry(name.clone())
                    .or_default()
                    .push_text(&value);
            }
        } else {
            self.general.push_text(value);
        }
    }

    fn push_code(&mut self, value: &str) {
        if let Some(target) = self.target.documentation_target(&mut self.documentation) {
            target.push_code(value);
        } else if let Target::Parameter(names) = &self.target {
            for name in names {
                self.documentation
                    .parameters
                    .entry(name.clone())
                    .or_default()
                    .push_code(value);
            }
        } else {
            self.general.push_code(value);
        }
    }

    fn handle_tag(&mut self, tag: Tag) {
        if tag.closing {
            let Some(open) = self.stack.last().filter(|open| open.name == tag.name) else {
                self.push_text(&tag.literal());
                return;
            };
            let open = open.clone();
            self.stack.pop();
            if open.kind == OpenTagKind::Code {
                if let Some((_, value)) = self.code.take() {
                    self.push_code(&value);
                }
            }
            self.target = open.previous_target;
            return;
        }

        if tag.self_closing {
            match tag.name.as_str() {
                "paramref" => {
                    if let Some(name) = tag.attrs.get("name") {
                        self.push_code(name);
                    }
                }
                "see" => {
                    if let Some(label) = see_label(&tag.attrs) {
                        self.push_code(&label);
                    }
                }
                "br" => self.push_text("\n"),
                _ => {}
            }
            return;
        }

        if self.stack.len() >= MAX_DOCUMENTATION_NESTING {
            self.push_text(&tag.literal());
            return;
        }
        let previous_target = self.target.clone();
        let kind = match tag.name.as_str() {
            "summary" => {
                self.target = Target::Summary;
                OpenTagKind::Section
            }
            "remarks" => {
                self.target = Target::Remarks;
                OpenTagKind::Section
            }
            "returns" => {
                self.target = Target::Returns;
                OpenTagKind::Section
            }
            "param" => {
                self.target = tag.attrs.get("name").map_or(Target::General, |name| {
                    let names = name
                        .split(|character: char| character == ',' || character.is_whitespace())
                        .filter(|name| !name.is_empty())
                        .map(canonical_name)
                        .collect::<Vec<_>>();
                    if names.is_empty() {
                        Target::General
                    } else {
                        Target::Parameter(names)
                    }
                });
                OpenTagKind::Section
            }
            "c" | "code" => {
                self.code = Some((previous_target.clone(), String::new()));
                OpenTagKind::Code
            }
            _ => OpenTagKind::Other,
        };
        self.stack.push(OpenTag {
            name: tag.name,
            previous_target,
            kind,
        });
    }

    fn finish(mut self) -> Documentation {
        if let Some((_, value)) = self.code.take() {
            self.push_code(&value);
        }
        if self.documentation.summary.is_empty() {
            self.documentation.summary = self.general;
        } else {
            self.documentation.summary.append(self.general);
        }
        self.documentation
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Tag {
    name: String,
    attrs: BTreeMap<String, String>,
    closing: bool,
    self_closing: bool,
}

impl Tag {
    fn literal(&self) -> String {
        let mut value = String::from("<");
        if self.closing {
            value.push('/');
        }
        value.push_str(&self.name);
        value.push('>');
        value
    }
}

fn parse_markup(value: &str) -> Option<Documentation> {
    let mut parser = MarkupParser::new();
    let mut cursor = 0usize;
    while cursor < value.len() {
        let Some(relative_start) = value[cursor..].find('<') else {
            parser.push_text(&value[cursor..]);
            break;
        };
        let start = cursor.saturating_add(relative_start);
        if start > cursor {
            parser.push_text(&value[cursor..start]);
        }
        let Some(end) = find_tag_end(value, start) else {
            parser.push_text(&value[start..]);
            break;
        };
        if end.saturating_sub(start) > MAX_DOCUMENTATION_TAG_BYTES {
            parser.push_text(&value[start..=start]);
            cursor = start.saturating_add(1);
            continue;
        }
        let Some(tag) = parse_tag(&value[start.saturating_add(1)..end]) else {
            parser.push_text(&value[start..=start]);
            cursor = start.saturating_add(1);
            continue;
        };
        parser.handle_tag(tag);
        cursor = end.saturating_add(1);
    }
    let documentation = parser.finish();
    (!documentation.summary.is_empty()
        || !documentation.remarks.is_empty()
        || !documentation.returns.is_empty()
        || documentation
            .parameters
            .values()
            .any(|value| !value.is_empty()))
    .then_some(documentation)
}

fn find_tag_end(value: &str, start: usize) -> Option<usize> {
    let mut quote = None;
    for (offset, character) in value[start.saturating_add(1)..].char_indices() {
        match (quote, character) {
            (None, '\'' | '"') => quote = Some(character),
            (Some(current), character) if current == character => quote = None,
            (None, '>') => return Some(start.saturating_add(1).saturating_add(offset)),
            _ => {}
        }
    }
    None
}

fn parse_tag(value: &str) -> Option<Tag> {
    let mut value = value.trim();
    if value.is_empty() || value.starts_with('!') || value.starts_with('?') {
        return None;
    }
    let closing = value.starts_with('/');
    if closing {
        value = value[1..].trim_start();
    }
    let self_closing = !closing && value.ends_with('/');
    if self_closing {
        value = value[..value.len().saturating_sub(1)].trim_end();
    }
    let name_end = value
        .char_indices()
        .find_map(|(index, character)| {
            (!character.is_ascii_alphanumeric()
                && character != '_'
                && character != ':'
                && character != '-')
                .then_some(index)
        })
        .unwrap_or(value.len());
    if name_end == 0 {
        return None;
    }
    let name = value[..name_end].to_ascii_lowercase();
    let mut rest = value[name_end..].trim_start();
    let mut attrs = BTreeMap::new();
    while !rest.is_empty() {
        let key_end = rest
            .char_indices()
            .find_map(|(index, character)| {
                (character.is_whitespace() || character == '=').then_some(index)
            })
            .unwrap_or(rest.len());
        if key_end == 0 {
            return None;
        }
        let key = rest[..key_end].to_ascii_lowercase();
        rest = rest[key_end..].trim_start();
        if !rest.starts_with('=') {
            return None;
        }
        rest = rest[1..].trim_start();
        let quote = rest.chars().next()?;
        if quote != '\'' && quote != '"' {
            return None;
        }
        let end = rest[quote.len_utf8()..]
            .find(quote)
            .map(|index| index.saturating_add(quote.len_utf8()))?;
        let value = decode_entities(&rest[quote.len_utf8()..end]);
        attrs.insert(key, value);
        rest = rest[end.saturating_add(quote.len_utf8())..].trim_start();
    }
    Some(Tag {
        name,
        attrs,
        closing,
        self_closing,
    })
}

fn see_label(attrs: &BTreeMap<String, String>) -> Option<String> {
    let value = attrs
        .get("cref")
        .or_else(|| attrs.get("name"))
        .or_else(|| attrs.get("href"))
        .or_else(|| attrs.get("langword"))?
        .trim();
    if value.is_empty() {
        return None;
    }
    Some(
        value
            .split_once(':')
            .map_or(value, |(_, label)| label)
            .to_owned(),
    )
}

fn collapse_whitespace(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut pending_space = false;
    for character in value.chars() {
        if character.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space {
            result.push(' ');
            pending_space = false;
        }
        result.push(character);
    }
    if pending_space && !result.is_empty() {
        result.push(' ');
    }
    result
}

fn sanitize_control(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() && !matches!(character, '\n' | '\r' | '\t') {
                '�'
            } else {
                character
            }
        })
        .collect()
}

fn escape_markdown(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    for character in sanitize_control(value).chars() {
        if matches!(
            character,
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '#' | '|' | '~'
        ) {
            result.push('\\');
        }
        result.push(character);
    }
    result
}

fn markdown_code_span(value: &str) -> String {
    let value = sanitize_control(value).replace(['\n', '\r'], " ");
    let mut run = 0usize;
    let mut max_run = 0usize;
    for character in value.chars() {
        if character == '`' {
            run = run.saturating_add(1);
            max_run = max_run.max(run);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat(max_run.saturating_add(1).max(1));
    if value.starts_with(' ') || value.ends_with(' ') {
        format!("{fence} {value} {fence}")
    } else {
        format!("{fence}{value}{fence}")
    }
}

fn decode_entities(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut cursor = 0usize;
    while cursor < value.len() {
        let Some(relative) = value[cursor..].find('&') else {
            result.push_str(&value[cursor..]);
            break;
        };
        let start = cursor.saturating_add(relative);
        result.push_str(&value[cursor..start]);
        let Some(end_relative) = value[start..].find(';') else {
            result.push('&');
            cursor = start.saturating_add(1);
            continue;
        };
        let end = start.saturating_add(end_relative);
        let entity = &value[start.saturating_add(1)..end];
        let decoded = match entity {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                u32::from_str_radix(&entity[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
            }
            _ if entity.starts_with('#') => {
                entity[1..].parse::<u32>().ok().and_then(char::from_u32)
            }
            _ => None,
        };
        if let Some(decoded) = decoded {
            result.push(decoded);
            cursor = end.saturating_add(1);
        } else {
            result.push('&');
            cursor = start.saturating_add(1);
        }
    }
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommentKind {
    Line,
    Brace,
    ParenStar,
}

#[derive(Debug, Clone)]
struct RawComment {
    start: usize,
    end: usize,
    content_start: usize,
    content_end: usize,
    kind: CommentKind,
    documented: bool,
}

#[derive(Debug, Clone)]
struct CommentAttachment {
    end: usize,
    body: String,
    documented: bool,
}

pub(crate) fn collect(
    source: &str,
    symbols: &[Symbol],
    cancel: &AtomicBool,
) -> Result<Vec<Option<Arc<Documentation>>>, String> {
    check_cancel(cancel)?;
    let comments = scan_comments(source, cancel)?;
    let mut direct = HashMap::<Span, Option<Arc<Documentation>>>::new();
    for symbol in symbols {
        check_cancel(cancel)?;
        if symbol.generic_parameter.is_some() {
            continue;
        }
        if direct.contains_key(&symbol.declaration_span) {
            continue;
        }
        let documentation =
            find_documentation(source, &comments, symbol.declaration_span.start, cancel)?
                .map(Arc::new);
        direct.insert(symbol.declaration_span, documentation);
    }

    let mut declaration_groups = HashMap::<String, Vec<usize>>::new();
    let mut definition_groups = HashMap::<String, Vec<usize>>::new();
    for (index, symbol) in symbols.iter().enumerate() {
        let Some(key) = symbol.routine_key.as_ref() else {
            continue;
        };
        let groups = match symbol.origin {
            Origin::Declaration => &mut declaration_groups,
            Origin::Definition => &mut definition_groups,
        };
        groups.entry(key.clone()).or_default().push(index);
    }

    let mut result = vec![None; symbols.len()];
    for (index, symbol) in symbols.iter().enumerate() {
        check_cancel(cancel)?;
        if symbol.generic_parameter.is_some() {
            continue;
        }
        let direct_documentation = direct.get(&symbol.declaration_span).cloned().flatten();
        let selected = if let Some(key) = symbol.routine_key.as_ref() {
            if symbol.origin == Origin::Definition {
                if declaration_groups
                    .get(key)
                    .is_some_and(|indices| indices.len() == 1)
                {
                    let declaration = declaration_groups[key][0];
                    direct
                        .get(&symbols[declaration].declaration_span)
                        .cloned()
                        .flatten()
                        .or(direct_documentation)
                } else {
                    direct_documentation
                }
            } else if direct_documentation.is_none()
                && definition_groups
                    .get(key)
                    .is_some_and(|indices| indices.len() == 1)
            {
                let definition = definition_groups[key][0];
                direct
                    .get(&symbols[definition].declaration_span)
                    .cloned()
                    .flatten()
            } else {
                direct_documentation
            }
        } else {
            direct_documentation
        };
        result[index] = selected;
    }
    Ok(result)
}

fn find_documentation(
    source: &str,
    comments: &[CommentAttachment],
    declaration_start: usize,
    cancel: &AtomicBool,
) -> Result<Option<Documentation>, String> {
    let Some(index) = comments
        .partition_point(|comment| comment.end <= declaration_start)
        .checked_sub(1)
    else {
        return Ok(None);
    };
    let comment = &comments[index];
    if !comment.documented
        || !is_adjacent(source, comment.end, declaration_start)
        || comment.body.len() > MAX_DOCUMENTATION_COMMENT_BYTES
    {
        return Ok(None);
    }
    check_cancel(cancel)?;
    Ok(parse_markup(&comment.body))
}

fn scan_comments(source: &str, cancel: &AtomicBool) -> Result<Vec<CommentAttachment>, String> {
    let bytes = source.as_bytes();
    let limit = source.len().min(MAX_DOCUMENTATION_SOURCE_BYTES);
    let mut raw = Vec::new();
    let mut index = 0usize;
    while index < limit {
        if index & 0x3ff == 0 {
            check_cancel(cancel)?;
        }
        match bytes[index] {
            b'\'' => index = skip_string(bytes, index, limit),
            b'/' if bytes.get(index.saturating_add(1)) == Some(&b'/') => {
                let end = line_end(bytes, index.saturating_add(2), limit);
                let documented = bytes.get(index.saturating_add(2)) == Some(&b'/');
                raw.push(RawComment {
                    start: index,
                    end,
                    content_start: index.saturating_add(if documented { 3 } else { 2 }),
                    content_end: end,
                    kind: CommentKind::Line,
                    documented,
                });
                index = end;
            }
            b'{' => {
                let end = brace_end(bytes, index.saturating_add(1), limit);
                let content_end = end.saturating_sub(usize::from(
                    end <= limit && end > index && bytes.get(end.saturating_sub(1)) == Some(&b'}'),
                ));
                let body = source
                    .get(index.saturating_add(1)..content_end)
                    .unwrap_or_default();
                raw.push(RawComment {
                    start: index,
                    end,
                    content_start: index.saturating_add(1),
                    content_end,
                    kind: CommentKind::Brace,
                    documented: is_documented_block(body),
                });
                index = end.max(index.saturating_add(1));
            }
            b'(' if bytes.get(index.saturating_add(1)) == Some(&b'*') => {
                let end = paren_star_end(bytes, index.saturating_add(2), limit);
                let content_end = end.saturating_sub(
                    2 * usize::from(
                        end <= limit
                            && end >= index.saturating_add(2)
                            && bytes.get(end.saturating_sub(2)) == Some(&b'*')
                            && bytes.get(end.saturating_sub(1)) == Some(&b')'),
                    ),
                );
                let body = source
                    .get(index.saturating_add(2)..content_end)
                    .unwrap_or_default();
                raw.push(RawComment {
                    start: index,
                    end,
                    content_start: index.saturating_add(2),
                    content_end,
                    kind: CommentKind::ParenStar,
                    documented: is_documented_block(body),
                });
                index = end.max(index.saturating_add(2));
            }
            _ => index = index.saturating_add(1),
        }
        if raw.len() >= MAX_DOCUMENTATION_COMMENTS {
            break;
        }
    }

    let mut attachments = Vec::with_capacity(raw.len());
    for comment in raw {
        let body = source
            .get(comment.content_start..comment.content_end)
            .unwrap_or_default();
        let documented = comment.documented && is_standalone_comment(source, comment.start);
        if comment.kind == CommentKind::Line && documented {
            let body = body.strip_prefix([' ', '\t']).unwrap_or(body).to_owned();
            if let Some(previous) =
                attachments
                    .last_mut()
                    .filter(|previous: &&mut CommentAttachment| {
                        previous.documented
                            && previous.end < comment.start
                            && is_single_line_gap(source, previous.end, comment.start)
                    })
            {
                previous.end = comment.end;
                previous.body.push('\n');
                previous.body.push_str(&body);
                continue;
            }
            let documented = !is_license_comment(&body);
            attachments.push(CommentAttachment {
                end: comment.end,
                body,
                documented,
            });
        } else {
            attachments.push(CommentAttachment {
                end: comment.end,
                body: normalize_block_body(body),
                documented,
            });
        }
    }
    Ok(attachments)
}

fn is_documented_block(value: &str) -> bool {
    let value = value.trim();
    if value.starts_with('$') || value.to_ascii_lowercase().contains("spdx-license") {
        return false;
    }
    let lower = value.to_ascii_lowercase();
    [
        "<summary",
        "<remarks",
        "<param",
        "<returns",
        "<code",
        "<c>",
        "<paramref",
        "<see",
    ]
    .iter()
    .any(|tag| lower.contains(tag))
}

fn is_license_comment(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    [
        "copyright",
        "spdx-license",
        "licensed under",
        "permission is hereby",
        "all rights reserved",
    ]
    .iter()
    .any(|term| lower.contains(term))
}

fn normalize_block_body(value: &str) -> String {
    value
        .lines()
        .map(|line| {
            let line = line.trim_start();
            line.strip_prefix('*').map_or(line, str::trim_start)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_adjacent(source: &str, start: usize, end: usize) -> bool {
    let Some(gap) = source.get(start..end) else {
        return false;
    };
    gap.chars().all(char::is_whitespace)
        && gap.chars().filter(|character| *character == '\n').count() <= 1
}

fn is_standalone_comment(source: &str, start: usize) -> bool {
    let line_start = source[..start]
        .rfind(['\n', '\r'])
        .map_or(0, |index| index + 1);
    source
        .get(line_start..start)
        .is_some_and(|prefix| prefix.chars().all(char::is_whitespace))
}

fn is_single_line_gap(source: &str, start: usize, end: usize) -> bool {
    let Some(gap) = source.get(start..end) else {
        return false;
    };
    if !gap.chars().all(char::is_whitespace) {
        return false;
    }
    let newlines = gap.chars().filter(|character| *character == '\n').count();
    newlines == 1 && !gap.contains("\n\n")
}

fn line_end(bytes: &[u8], mut index: usize, limit: usize) -> usize {
    while index < limit && bytes[index] != b'\n' && bytes[index] != b'\r' {
        index = index.saturating_add(1);
    }
    index
}

fn skip_string(bytes: &[u8], mut index: usize, limit: usize) -> usize {
    index = index.saturating_add(1);
    while index < limit {
        if bytes[index] == b'\'' {
            if bytes.get(index.saturating_add(1)) == Some(&b'\'') {
                index = index.saturating_add(2);
            } else {
                return index.saturating_add(1);
            }
        } else {
            index = index.saturating_add(1);
        }
    }
    limit
}

fn brace_end(bytes: &[u8], mut index: usize, limit: usize) -> usize {
    while index < limit {
        if bytes[index] == b'}' {
            return index.saturating_add(1);
        }
        index = index.saturating_add(1);
    }
    limit
}

fn paren_star_end(bytes: &[u8], mut index: usize, limit: usize) -> usize {
    while index < limit {
        if bytes[index] == b'*' && bytes.get(index.saturating_add(1)) == Some(&b')') {
            return index.saturating_add(2);
        }
        index = index.saturating_add(1);
    }
    limit
}

#[cfg(test)]
mod tests {
    use super::{
        Documentation, MAX_DOCUMENTATION_RENDERED_BYTES, find_documentation, parse_markup,
        scan_comments,
    };
    use lsp_types::MarkupKind;
    use std::sync::atomic::AtomicBool;

    fn not_cancelled() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn render(documentation: &Documentation, format: MarkupKind) -> Option<String> {
        let cancel = not_cancelled();
        documentation
            .render(format, &cancel)
            .expect("documentation renders")
    }

    #[test]
    fn parses_supported_xml_sections_and_inline_labels() {
        let documentation = parse_markup(
            "<summary>Reads <c>A_B</c> &amp; <paramref name=\"Name\"/> and <see cref=\"T:Thing\"/>.</summary><remarks>More details.</remarks><param name=\"Name, Other\">An argument.</param><returns>The result.</returns>",
        )
        .expect("structured documentation");

        assert_eq!(
            render(&documentation, MarkupKind::Markdown),
            Some("Reads `A_B` & `Name` and `Thing`.\n\n**Remarks**\n\nMore details.\n\n**Parameters**\n\n- `name` — An argument.\n- `other` — An argument.\n\n**Returns**\n\nThe result.".to_owned())
        );
        assert_eq!(
            documentation
                .parameter("OTHER", MarkupKind::PlainText, &not_cancelled())
                .expect("parameter documentation"),
            Some("An argument.".to_owned())
        );
    }

    #[test]
    fn markdown_escapes_text_and_uses_safe_code_fences() {
        let documentation =
            parse_markup("<summary>Use *literal* [text] <c>a`b</c> &lt;tag&gt;.</summary>")
                .expect("escaped documentation");

        assert_eq!(
            render(&documentation, MarkupKind::Markdown),
            Some("Use \\*literal\\* \\[text\\] ``a`b`` \\<tag\\>.".to_owned())
        );
        assert_eq!(
            render(&documentation, MarkupKind::PlainText),
            Some("Use *literal* [text] a`b <tag>.".to_owned())
        );
    }

    #[test]
    fn malformed_markup_degrades_to_safe_text() {
        let documentation = parse_markup("<summary>Unclosed &lt;tag").expect("plain fallback");

        assert_eq!(
            render(&documentation, MarkupKind::Markdown),
            Some("Unclosed \\<tag".to_owned())
        );
    }

    #[test]
    fn attachment_requires_documented_adjacency_and_ignores_directives() {
        let source = "/// <summary>Good</summary>\nprocedure Good;\n\n/// <summary>Blank</summary>\n\nprocedure Blank;\n{ $IFDEF TEST }\nprocedure Directive;\nprocedure Previous; /// <summary>Trailing</summary>\nprocedure Trailing;\n";
        let comments = scan_comments(source, &not_cancelled()).expect("comments scan");
        let good = find_documentation(
            source,
            &comments,
            source.find("procedure Good").expect("good declaration"),
            &not_cancelled(),
        )
        .expect("good lookup")
        .expect("good documentation");
        assert_eq!(
            render(&good, MarkupKind::PlainText),
            Some("Good".to_owned())
        );
        assert!(
            find_documentation(
                source,
                &comments,
                source.find("procedure Blank").expect("blank declaration"),
                &not_cancelled(),
            )
            .expect("blank lookup")
            .is_none()
        );
        assert!(
            find_documentation(
                source,
                &comments,
                source
                    .find("procedure Directive")
                    .expect("directive declaration"),
                &not_cancelled(),
            )
            .expect("directive lookup")
            .is_none()
        );
        assert!(
            find_documentation(
                source,
                &comments,
                source
                    .find("procedure Trailing")
                    .expect("trailing declaration"),
                &not_cancelled(),
            )
            .expect("trailing lookup")
            .is_none()
        );
    }

    #[test]
    fn block_documentation_is_supported_but_license_comments_are_not() {
        let source = "(* <summary>Block</summary> *)\nprocedure Block;\n/// Copyright 2026\nprocedure Licensed;\n";
        let comments = scan_comments(source, &not_cancelled()).expect("comments scan");
        let block = find_documentation(
            source,
            &comments,
            source.find("procedure Block").expect("block declaration"),
            &not_cancelled(),
        )
        .expect("block lookup")
        .expect("block documentation");
        assert_eq!(
            render(&block, MarkupKind::PlainText),
            Some("Block".to_owned())
        );
        assert!(
            find_documentation(
                source,
                &comments,
                source
                    .find("procedure Licensed")
                    .expect("licensed declaration"),
                &not_cancelled(),
            )
            .expect("license lookup")
            .is_none()
        );
    }

    #[test]
    fn rendering_honors_cancellation_and_size_bound() {
        let documentation = parse_markup(&format!(
            "<summary>{}</summary>",
            "x".repeat(MAX_DOCUMENTATION_RENDERED_BYTES + 1)
        ))
        .expect("large documentation");
        let cancel = not_cancelled();
        assert!(documentation.render(MarkupKind::Markdown, &cancel).is_err());

        let documentation = parse_markup("<summary>cancelled</summary>").expect("documentation");
        let cancel = AtomicBool::new(true);
        assert!(documentation.render(MarkupKind::Markdown, &cancel).is_err());
    }
}

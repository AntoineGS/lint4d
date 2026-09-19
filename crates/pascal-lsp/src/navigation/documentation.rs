use super::{Origin, Span, Symbol, canonical_name};
use lsp_types::MarkupKind;
#[cfg(test)]
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const MAX_DOCUMENTATION_SOURCE_BYTES: usize = 2 * 1024 * 1024;
const MAX_DOCUMENTATION_COMMENTS: usize = 8 * 1024;
const MAX_DOCUMENTATION_COMMENT_BYTES: usize = 64 * 1024;
const MAX_DOCUMENTATION_TAG_BYTES: usize = 4096;
const MAX_DOCUMENTATION_NESTING: usize = 64;
const MAX_DOCUMENTATION_PARSE_BYTES: usize = 128 * 1024;
const MAX_DOCUMENTATION_PARSE_WORK: usize = 256 * 1024;
const DOCUMENTATION_FRAGMENT_OVERHEAD: usize = 24;
const DOCUMENTATION_PARAMETER_OVERHEAD: usize = 64;
pub(crate) const MAX_DOCUMENTATION_RENDERED_BYTES: usize = 64 * 1024;

#[cfg(test)]
thread_local! {
    static DOCUMENTATION_PARSE_WORK: Cell<usize> = const { Cell::new(0) };
}

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
        let mut result = String::new();
        if let Some(summary) = self.summary.render(format.clone(), cancel)? {
            append_rendered_section(&mut result, &summary, cancel)?;
        }
        if !self.remarks.is_empty() {
            if let Some(section) = render_section("Remarks", &self.remarks, format.clone(), cancel)?
            {
                append_rendered_section(&mut result, &section, cancel)?;
            }
        }
        if include_parameters && !self.parameters.is_empty() {
            check_cancel(cancel)?;
            let mut parameter_section = match format {
                MarkupKind::Markdown => "**Parameters**\n\n".to_owned(),
                MarkupKind::PlainText => "Parameters:\n".to_owned(),
            };
            let mut parameter_count = 0usize;
            for (name, value) in &self.parameters {
                check_cancel(cancel)?;
                let rendered = value.render(format.clone(), cancel)?.unwrap_or_default();
                if rendered.is_empty() {
                    continue;
                }
                let line = match format {
                    MarkupKind::Markdown => {
                        format!("- {} — {rendered}", markdown_code_span(name, cancel)?)
                    }
                    MarkupKind::PlainText => {
                        format!("{name}: {rendered}")
                    }
                };
                let separator = usize::from(parameter_count != 0);
                if parameter_section
                    .len()
                    .saturating_add(separator)
                    .saturating_add(line.len())
                    > MAX_DOCUMENTATION_RENDERED_BYTES
                {
                    return Err(format!(
                        "documentation exceeds the {MAX_DOCUMENTATION_RENDERED_BYTES}-byte limit"
                    ));
                }
                if separator != 0 {
                    parameter_section.push('\n');
                }
                parameter_section.push_str(&line);
                parameter_count = parameter_count.saturating_add(1);
            }
            if parameter_count != 0 {
                append_rendered_section(&mut result, &parameter_section, cancel)?;
            }
        }
        if !self.returns.is_empty() {
            if let Some(section) = render_section("Returns", &self.returns, format, cancel)? {
                append_rendered_section(&mut result, &section, cancel)?;
            }
        }
        Ok((!result.trim().is_empty()).then_some(result))
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

fn append_rendered_section(
    result: &mut String,
    section: &str,
    cancel: &AtomicBool,
) -> Result<(), String> {
    check_cancel(cancel)?;
    let section = section.trim();
    if section.is_empty() {
        return Ok(());
    }
    let separator = usize::from(!result.is_empty()) * 2;
    if result
        .len()
        .saturating_add(separator)
        .saturating_add(section.len())
        > MAX_DOCUMENTATION_RENDERED_BYTES
    {
        return Err(format!(
            "documentation exceeds the {MAX_DOCUMENTATION_RENDERED_BYTES}-byte limit"
        ));
    }
    if separator != 0 {
        result.push_str("\n\n");
    }
    result.push_str(section);
    Ok(())
}

fn append_rich_text(
    target: &mut RichText,
    value: &str,
    code: bool,
    budget: &mut ParseBudget<'_>,
) -> Result<bool, String> {
    if value.is_empty() {
        return Ok(true);
    }
    let required_bytes = value
        .len()
        .saturating_add(usize::from(target.needs_fragment(code)) * DOCUMENTATION_FRAGMENT_OVERHEAD);
    if !budget.reserve_work(value.len().saturating_add(1))?
        || !budget.reserve_bytes(required_bytes)?
    {
        return Ok(false);
    }
    if code {
        target.append_code(value);
    } else {
        target.append_text(value);
    }
    Ok(true)
}

fn normalize_text(value: &str, cancel: &AtomicBool) -> Result<String, String> {
    let decoded = decode_entities(value, cancel)?;
    collapse_whitespace(&decoded, cancel)
}

fn normalize_code(value: &str, cancel: &AtomicBool) -> Result<String, String> {
    let decoded = decode_entities(value, cancel)?;
    sanitize_control(&decoded, cancel)
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("request cancelled".to_string())
    } else {
        Ok(())
    }
}

struct ParseBudget<'a> {
    cancel: &'a AtomicBool,
    remaining_bytes: usize,
    remaining_work: usize,
}

impl<'a> ParseBudget<'a> {
    fn new(cancel: &'a AtomicBool) -> Self {
        Self {
            cancel,
            remaining_bytes: MAX_DOCUMENTATION_PARSE_BYTES,
            remaining_work: MAX_DOCUMENTATION_PARSE_WORK,
        }
    }

    fn check_cancel(&self) -> Result<(), String> {
        check_cancel(self.cancel)
    }

    fn check_cancel_at(&self, index: usize) -> Result<(), String> {
        if index & 0x3ff == 0 {
            self.check_cancel()?;
        }
        Ok(())
    }

    fn reserve_bytes(&mut self, amount: usize) -> Result<bool, String> {
        self.check_cancel()?;
        if amount > self.remaining_bytes {
            self.remaining_bytes = 0;
            return Ok(false);
        }
        self.remaining_bytes -= amount;
        Ok(true)
    }

    fn reserve_work(&mut self, amount: usize) -> Result<bool, String> {
        self.check_cancel()?;
        if amount > self.remaining_work {
            #[cfg(test)]
            DOCUMENTATION_PARSE_WORK.with(|value| {
                value.set(value.get().saturating_add(self.remaining_work));
            });
            self.remaining_work = 0;
            return Ok(false);
        }
        self.remaining_work -= amount;
        #[cfg(test)]
        DOCUMENTATION_PARSE_WORK.with(|value| value.set(value.get().saturating_add(amount)));
        Ok(true)
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

    fn append_text(&mut self, value: &str) {
        if value.is_empty() {
            return;
        }
        match self.fragments.last_mut() {
            Some(Fragment::Text(current)) => current.push_str(value),
            _ => self.fragments.push(Fragment::Text(value.to_owned())),
        }
    }

    fn append_code(&mut self, value: &str) {
        if value.is_empty() {
            return;
        }
        match self.fragments.last_mut() {
            Some(Fragment::Code(current)) => current.push_str(value),
            _ => self.fragments.push(Fragment::Code(value.to_owned())),
        }
    }

    fn needs_fragment(&self, code: bool) -> bool {
        !matches!(
            (self.fragments.last(), code),
            (Some(Fragment::Code(_)), true) | (Some(Fragment::Text(_)), false)
        )
    }

    fn render(&self, format: MarkupKind, cancel: &AtomicBool) -> Result<Option<String>, String> {
        let mut result = String::new();
        for (index, fragment) in self.fragments.iter().enumerate() {
            check_cancel(cancel)?;
            let rendered = match fragment {
                Fragment::Text(value) => match format {
                    MarkupKind::Markdown => escape_markdown(value, cancel)?,
                    MarkupKind::PlainText => sanitize_control(value, cancel)?,
                },
                Fragment::Code(value) => match format {
                    MarkupKind::Markdown => markdown_code_span(value, cancel)?,
                    MarkupKind::PlainText => sanitize_control(value, cancel)?,
                },
            };
            check_cancel(cancel)?;
            if index & 0x3f == 0 {
                check_cancel(cancel)?;
            }
            if result.len().saturating_add(rendered.len()) > MAX_DOCUMENTATION_RENDERED_BYTES {
                return Err(format!(
                    "documentation exceeds the {MAX_DOCUMENTATION_RENDERED_BYTES}-byte limit"
                ));
            }
            result.push_str(&rendered);
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
    Parameter(Arc<[String]>),
    Returns,
}

struct MarkupParser<'a> {
    documentation: Documentation,
    target: Target,
    stack: Vec<OpenTag>,
    code_depth: usize,
    budget: ParseBudget<'a>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OpenTag {
    name: String,
    literal: String,
    previous_target: Target,
    kind: OpenTagKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenTagKind {
    Section,
    Code,
    Other,
}

impl<'a> MarkupParser<'a> {
    fn new(cancel: &'a AtomicBool) -> Self {
        Self {
            documentation: Documentation {
                summary: RichText::default(),
                remarks: RichText::default(),
                parameters: BTreeMap::new(),
                returns: RichText::default(),
            },
            target: Target::General,
            stack: Vec::new(),
            code_depth: 0,
            budget: ParseBudget::new(cancel),
        }
    }

    fn push_text(&mut self, value: &str) -> Result<bool, String> {
        let code = self.code_depth != 0;
        let value = if code {
            normalize_code(value, self.budget.cancel)?
        } else {
            normalize_text(value, self.budget.cancel)?
        };
        self.push_normalized(&value, code)
    }

    fn push_code(&mut self, value: &str) -> Result<bool, String> {
        let value = normalize_code(value, self.budget.cancel)?;
        self.push_normalized(&value, true)
    }

    fn push_normalized(&mut self, value: &str, code: bool) -> Result<bool, String> {
        let target = self.target.clone();
        match target {
            Target::General => append_rich_text(
                &mut self.documentation.summary,
                value,
                code,
                &mut self.budget,
            ),
            Target::Summary => append_rich_text(
                &mut self.documentation.summary,
                value,
                code,
                &mut self.budget,
            ),
            Target::Remarks => append_rich_text(
                &mut self.documentation.remarks,
                value,
                code,
                &mut self.budget,
            ),
            Target::Returns => append_rich_text(
                &mut self.documentation.returns,
                value,
                code,
                &mut self.budget,
            ),
            Target::Parameter(names) => self.append_parameters(&names, value, code),
        }
    }

    fn append_parameters(
        &mut self,
        names: &[String],
        value: &str,
        code: bool,
    ) -> Result<bool, String> {
        if value.is_empty() || names.is_empty() {
            return Ok(true);
        }
        let content_bytes = value.len().checked_mul(names.len()).unwrap_or(usize::MAX);
        let mut required_bytes = content_bytes;
        for (index, name) in names.iter().enumerate() {
            self.budget.check_cancel_at(index)?;
            if let Some(current) = self.documentation.parameters.get(name) {
                if current.needs_fragment(code) {
                    required_bytes = required_bytes.saturating_add(DOCUMENTATION_FRAGMENT_OVERHEAD);
                }
            } else {
                required_bytes = required_bytes
                    .saturating_add(name.len())
                    .saturating_add(DOCUMENTATION_PARAMETER_OVERHEAD)
                    .saturating_add(DOCUMENTATION_FRAGMENT_OVERHEAD);
            }
        }
        let work = content_bytes.saturating_add(names.len());
        if !self.budget.reserve_work(work)? || !self.budget.reserve_bytes(required_bytes)? {
            return Ok(false);
        }
        for (index, name) in names.iter().enumerate() {
            self.budget.check_cancel_at(index)?;
            let current = self
                .documentation
                .parameters
                .entry(name.clone())
                .or_default();
            if code {
                current.append_code(value);
            } else {
                current.append_text(value);
            }
        }
        Ok(true)
    }

    fn parameter_target(&mut self, value: &str) -> Result<Option<Target>, String> {
        let value = decode_entities(value, self.budget.cancel)?;
        let mut unique = std::collections::HashSet::new();
        let mut names = Vec::new();
        for (index, name) in value
            .split(|character: char| character == ',' || character.is_whitespace())
            .filter(|name| !name.is_empty())
            .enumerate()
        {
            self.budget.check_cancel_at(index)?;
            if !self.budget.reserve_work(1)? {
                return Ok(None);
            }
            let name = canonical_name(name);
            if unique.contains(&name) {
                continue;
            }
            let metadata_bytes = name.len().saturating_add(DOCUMENTATION_PARAMETER_OVERHEAD);
            if !self.budget.reserve_bytes(metadata_bytes)? {
                return Ok(None);
            }
            unique.insert(name.clone());
            names.push(name);
        }
        if names.is_empty() {
            return Ok(Some(Target::General));
        }
        if !self.budget.reserve_work(names.len())? {
            return Ok(None);
        }
        Ok(Some(Target::Parameter(Arc::from(names.into_boxed_slice()))))
    }

    fn handle_tag(&mut self, tag: Tag) -> Result<bool, String> {
        if tag.closing {
            let Some(open) = self.stack.last().filter(|open| open.name == tag.name) else {
                return self.push_text(&tag.literal);
            };
            let open = open.clone();
            self.stack.pop();
            if open.kind == OpenTagKind::Other && !self.push_text(&tag.literal)? {
                return Ok(false);
            }
            if open.kind == OpenTagKind::Code {
                self.code_depth = self.code_depth.saturating_sub(1);
            }
            self.target = open.previous_target;
            return Ok(true);
        }

        if tag.self_closing {
            let handled = match tag.name.as_str() {
                "paramref" => tag
                    .attrs
                    .get("name")
                    .map_or(Ok(true), |name| self.push_code(name))?,
                "see" => see_label(&tag.attrs).map_or(Ok(true), |label| self.push_code(&label))?,
                "br" => self.push_text("\n")?,
                _ => self.push_text(&tag.literal)?,
            };
            return Ok(handled);
        }

        if self.stack.len() >= MAX_DOCUMENTATION_NESTING {
            return self.push_text(&tag.literal);
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
                self.target = match tag.attrs.get("name") {
                    Some(name) => match self.parameter_target(name)? {
                        Some(target) => target,
                        None => return Ok(false),
                    },
                    None => Target::General,
                };
                OpenTagKind::Section
            }
            "c" | "code" => {
                self.code_depth = self.code_depth.saturating_add(1);
                OpenTagKind::Code
            }
            _ => {
                if !self.push_text(&tag.literal)? {
                    return Ok(false);
                }
                OpenTagKind::Other
            }
        };
        self.stack.push(OpenTag {
            name: tag.name,
            literal: tag.literal,
            previous_target,
            kind,
        });
        Ok(true)
    }

    fn finish(self) -> Documentation {
        self.documentation
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Tag {
    name: String,
    attrs: BTreeMap<String, String>,
    closing: bool,
    self_closing: bool,
    literal: String,
}

#[cfg(test)]
fn parse_markup(value: &str) -> Option<Documentation> {
    let cancel = AtomicBool::new(false);
    parse_markup_with_cancel(value, &cancel)
        .expect("test documentation parsing should not be cancelled")
}

fn parse_markup_with_cancel(
    value: &str,
    cancel: &AtomicBool,
) -> Result<Option<Documentation>, String> {
    let mut parser = MarkupParser::new(cancel);
    if !parser.budget.reserve_work(value.len())? {
        return Ok(None);
    }
    let mut cursor = 0usize;
    while cursor < value.len() {
        parser.budget.check_cancel_at(cursor)?;
        let Some(relative_start) = value[cursor..].find('<') else {
            if !parser.push_text(&value[cursor..])? {
                return Ok(None);
            }
            break;
        };
        let start = cursor.saturating_add(relative_start);
        if start > cursor && !parser.push_text(&value[cursor..start])? {
            return Ok(None);
        }
        let Some(end) = find_tag_end(value, start, &mut parser.budget)? else {
            if !parser.push_text(&value[start..])? {
                return Ok(None);
            }
            break;
        };
        if end.saturating_sub(start) > MAX_DOCUMENTATION_TAG_BYTES {
            if !parser.push_text(&value[start..=end])? {
                return Ok(None);
            }
            cursor = end.saturating_add(1);
            continue;
        }
        let Some(tag) = parse_tag(&value[start.saturating_add(1)..end], &parser.budget)? else {
            if !parser.push_text(&value[start..=end])? {
                return Ok(None);
            }
            cursor = end.saturating_add(1);
            continue;
        };
        if !parser.handle_tag(tag)? {
            return Ok(None);
        }
        cursor = end.saturating_add(1);
    }
    let documentation = parser.finish();
    Ok((!documentation.summary.is_empty()
        || !documentation.remarks.is_empty()
        || !documentation.returns.is_empty()
        || documentation
            .parameters
            .values()
            .any(|value| !value.is_empty()))
    .then_some(documentation))
}

fn find_tag_end(
    value: &str,
    start: usize,
    budget: &mut ParseBudget<'_>,
) -> Result<Option<usize>, String> {
    let mut quote = None;
    for (offset, character) in value[start.saturating_add(1)..].char_indices() {
        if !budget.reserve_work(1)? {
            return Ok(None);
        }
        budget.check_cancel_at(offset)?;
        match (quote, character) {
            (None, '\'' | '"') => quote = Some(character),
            (Some(current), character) if current == character => quote = None,
            (None, '>') => {
                return Ok(Some(start.saturating_add(1).saturating_add(offset)));
            }
            _ => {}
        }
    }
    Ok(None)
}

fn parse_tag(value: &str, budget: &ParseBudget<'_>) -> Result<Option<Tag>, String> {
    let literal = format!("<{value}>");
    let mut value = value.trim();
    if value.is_empty() || value.starts_with('!') || value.starts_with('?') {
        return Ok(None);
    }
    let closing = value.starts_with('/');
    if closing {
        value = value[1..].trim_start();
    }
    let self_closing = !closing && value.ends_with('/');
    if self_closing {
        value = value[..value.len().saturating_sub(1)].trim_end();
    }
    let mut name_end = value.len();
    for (index, character) in value.char_indices() {
        budget.check_cancel_at(index)?;
        if !character.is_ascii_alphanumeric()
            && character != '_'
            && character != ':'
            && character != '-'
        {
            name_end = index;
            break;
        }
    }
    if name_end == 0 {
        return Ok(None);
    }
    let name = value[..name_end].to_ascii_lowercase();
    let mut rest = value[name_end..].trim_start();
    let mut attrs = BTreeMap::new();
    let mut rest_offset = 0usize;
    while !rest.is_empty() {
        budget.check_cancel_at(rest_offset)?;
        let mut key_end = rest.len();
        for (index, character) in rest.char_indices() {
            budget.check_cancel_at(rest_offset.saturating_add(index))?;
            if character.is_whitespace() || character == '=' {
                key_end = index;
                break;
            }
        }
        if key_end == 0 {
            return Ok(None);
        }
        let key = rest[..key_end].to_ascii_lowercase();
        rest = rest[key_end..].trim_start();
        if !rest.starts_with('=') {
            return Ok(None);
        }
        rest = rest[1..].trim_start();
        let Some(quote) = rest.chars().next() else {
            return Ok(None);
        };
        if quote != '\'' && quote != '"' {
            return Ok(None);
        }
        let mut end = None;
        for (index, character) in rest[quote.len_utf8()..].char_indices() {
            budget.check_cancel_at(rest_offset.saturating_add(index))?;
            if character == quote {
                end = Some(index.saturating_add(quote.len_utf8()));
                break;
            }
        }
        let Some(end) = end else {
            return Ok(None);
        };
        attrs.insert(key, rest[quote.len_utf8()..end].to_owned());
        rest = rest[end.saturating_add(quote.len_utf8())..].trim_start();
        rest_offset = rest_offset.saturating_add(end.saturating_add(quote.len_utf8()));
    }
    Ok(Some(Tag {
        name,
        attrs,
        closing,
        self_closing,
        literal,
    }))
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

fn collapse_whitespace(value: &str, cancel: &AtomicBool) -> Result<String, String> {
    let mut result = String::with_capacity(value.len());
    let mut pending_space = false;
    for (index, character) in value.char_indices() {
        if index & 0x3ff == 0 {
            check_cancel(cancel)?;
        }
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
    check_cancel(cancel)?;
    Ok(result)
}

fn sanitize_control(value: &str, cancel: &AtomicBool) -> Result<String, String> {
    let mut result = String::with_capacity(value.len());
    for (index, character) in value.char_indices() {
        if index & 0x3ff == 0 {
            check_cancel(cancel)?;
        }
        result.push(
            if character.is_control() && !matches!(character, '\n' | '\r' | '\t') {
                '�'
            } else {
                character
            },
        );
    }
    check_cancel(cancel)?;
    Ok(result)
}

fn escape_markdown(value: &str, cancel: &AtomicBool) -> Result<String, String> {
    let sanitized = sanitize_control(value, cancel)?;
    let mut result = String::with_capacity(sanitized.len());
    for (index, character) in sanitized.char_indices() {
        if index & 0x3ff == 0 {
            check_cancel(cancel)?;
        }
        if character == '&' {
            result.push_str("&amp;");
        } else if matches!(
            character,
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '#' | '|' | '~'
        ) {
            result.push('\\');
            result.push(character);
        } else {
            result.push(character);
        }
    }
    check_cancel(cancel)?;
    Ok(result)
}

fn markdown_code_span(value: &str, cancel: &AtomicBool) -> Result<String, String> {
    let sanitized = sanitize_control(value, cancel)?;
    let value = sanitized.replace(['\n', '\r'], " ");
    let mut run = 0usize;
    let mut max_run = 0usize;
    for (index, character) in value.char_indices() {
        if index & 0x3ff == 0 {
            check_cancel(cancel)?;
        }
        if character == '`' {
            run = run.saturating_add(1);
            max_run = max_run.max(run);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat(max_run.saturating_add(1).max(1));
    let has_non_space = value.chars().any(|character| character != ' ');
    let pad = value.starts_with('`')
        || value.ends_with('`')
        || (value.starts_with(' ') && value.ends_with(' ') && has_non_space);
    check_cancel(cancel)?;
    Ok(if pad {
        format!("{fence} {value} {fence}")
    } else {
        format!("{fence}{value}{fence}")
    })
}

fn decode_entities(value: &str, cancel: &AtomicBool) -> Result<String, String> {
    let mut result = String::with_capacity(value.len());
    let mut cursor = 0usize;
    while cursor < value.len() {
        if cursor & 0x3ff == 0 {
            check_cancel(cancel)?;
        }
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
    check_cancel(cancel)?;
    Ok(result)
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
    mergeable_line: bool,
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
        || !is_adjacent(source, comment.end, declaration_start, cancel)?
        || comment.body.len() > MAX_DOCUMENTATION_COMMENT_BYTES
    {
        return Ok(None);
    }
    check_cancel(cancel)?;
    parse_markup_with_cancel(&comment.body, cancel)
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
            b'\'' => index = skip_string(bytes, index, limit, cancel)?,
            b'/' if bytes.get(index.saturating_add(1)) == Some(&b'/') => {
                let end = line_end(bytes, index.saturating_add(2), limit, cancel)?;
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
                let end = brace_end(bytes, index.saturating_add(1), limit, cancel)?;
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
                let end = paren_star_end(bytes, index.saturating_add(2), limit, cancel)?;
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

    let mut attachments: Vec<CommentAttachment> = Vec::with_capacity(raw.len());
    for (index, comment) in raw.into_iter().enumerate() {
        if index & 0x3f == 0 {
            check_cancel(cancel)?;
        }
        let body = source
            .get(comment.content_start..comment.content_end)
            .unwrap_or_default();
        if body.len() > MAX_DOCUMENTATION_COMMENT_BYTES {
            attachments.push(CommentAttachment {
                end: comment.end,
                body: String::new(),
                documented: false,
                mergeable_line: false,
            });
            continue;
        }
        let documented =
            comment.documented && is_standalone_comment(source, comment.start, cancel)?;
        if comment.kind == CommentKind::Line && documented {
            let body = body.strip_prefix([' ', '\t']).unwrap_or(body);
            let can_merge = attachments
                .last()
                .is_some_and(|previous| previous.mergeable_line && previous.end < comment.start)
                && is_single_line_gap(
                    source,
                    attachments
                        .last()
                        .map_or(comment.start, |previous| previous.end),
                    comment.start,
                    cancel,
                )?;
            if can_merge {
                let previous = attachments.last_mut().expect("merge candidate exists");
                if previous
                    .body
                    .len()
                    .saturating_add(1)
                    .saturating_add(body.len())
                    > MAX_DOCUMENTATION_COMMENT_BYTES
                {
                    previous.end = comment.end;
                    previous.body.clear();
                    previous.documented = false;
                } else {
                    previous.end = comment.end;
                    previous.body.push('\n');
                    previous.body.push_str(body);
                }
                continue;
            }
            attachments.push(CommentAttachment {
                end: comment.end,
                body: body.to_owned(),
                documented: true,
                mergeable_line: true,
            });
        } else {
            attachments.push(CommentAttachment {
                end: comment.end,
                body: normalize_block_body(body, cancel)?,
                documented,
                mergeable_line: false,
            });
        }
    }
    for (index, attachment) in attachments.iter_mut().enumerate() {
        if index & 0x3f == 0 {
            check_cancel(cancel)?;
        }
        if attachment.documented && is_license_comment(&attachment.body) {
            attachment.documented = false;
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

fn normalize_block_body(value: &str, cancel: &AtomicBool) -> Result<String, String> {
    let mut lines = Vec::new();
    for (index, line) in value.lines().enumerate() {
        if index & 0x3f == 0 {
            check_cancel(cancel)?;
        }
        let line = {
            let line = line.trim_start();
            line.strip_prefix('*').map_or(line, str::trim_start)
        };
        lines.push(line);
    }
    check_cancel(cancel)?;
    Ok(lines.join("\n"))
}

fn is_adjacent(
    source: &str,
    start: usize,
    end: usize,
    cancel: &AtomicBool,
) -> Result<bool, String> {
    let Some(gap) = source.get(start..end) else {
        return Ok(false);
    };
    let mut newlines = 0usize;
    for (index, character) in gap.char_indices() {
        if index & 0x3ff == 0 {
            check_cancel(cancel)?;
        }
        if !character.is_whitespace() {
            return Ok(false);
        }
        if character == '\n' {
            newlines = newlines.saturating_add(1);
        }
    }
    check_cancel(cancel)?;
    Ok(newlines <= 1)
}

fn is_standalone_comment(source: &str, start: usize, cancel: &AtomicBool) -> Result<bool, String> {
    let mut line_start = 0;
    for (index, character) in source[..start].char_indices().rev() {
        if index & 0x3ff == 0 {
            check_cancel(cancel)?;
        }
        if matches!(character, '\n' | '\r') {
            line_start = index.saturating_add(1);
            break;
        }
    }
    let Some(prefix) = source.get(line_start..start) else {
        return Ok(false);
    };
    for (index, character) in prefix.char_indices() {
        if index & 0x3ff == 0 {
            check_cancel(cancel)?;
        }
        if !character.is_whitespace() {
            return Ok(false);
        }
    }
    check_cancel(cancel)?;
    Ok(true)
}

fn is_single_line_gap(
    source: &str,
    start: usize,
    end: usize,
    cancel: &AtomicBool,
) -> Result<bool, String> {
    let Some(gap) = source.get(start..end) else {
        return Ok(false);
    };
    let mut newlines = 0usize;
    let mut previous_newline = false;
    for (index, character) in gap.char_indices() {
        if index & 0x3ff == 0 {
            check_cancel(cancel)?;
        }
        if !character.is_whitespace() {
            return Ok(false);
        }
        if character == '\n' {
            newlines = newlines.saturating_add(1);
            if previous_newline {
                return Ok(false);
            }
            previous_newline = true;
        } else if character != '\r' {
            previous_newline = false;
        }
    }
    check_cancel(cancel)?;
    Ok(newlines == 1)
}

fn line_end(
    bytes: &[u8],
    mut index: usize,
    limit: usize,
    cancel: &AtomicBool,
) -> Result<usize, String> {
    while index < limit && bytes[index] != b'\n' && bytes[index] != b'\r' {
        if index & 0x3ff == 0 {
            check_cancel(cancel)?;
        }
        index = index.saturating_add(1);
    }
    check_cancel(cancel)?;
    Ok(index)
}

fn skip_string(
    bytes: &[u8],
    mut index: usize,
    limit: usize,
    cancel: &AtomicBool,
) -> Result<usize, String> {
    index = index.saturating_add(1);
    while index < limit {
        if index & 0x3ff == 0 {
            check_cancel(cancel)?;
        }
        if bytes[index] == b'\'' {
            if bytes.get(index.saturating_add(1)) == Some(&b'\'') {
                index = index.saturating_add(2);
            } else {
                return Ok(index.saturating_add(1));
            }
        } else {
            index = index.saturating_add(1);
        }
    }
    check_cancel(cancel)?;
    Ok(limit)
}

fn brace_end(
    bytes: &[u8],
    mut index: usize,
    limit: usize,
    cancel: &AtomicBool,
) -> Result<usize, String> {
    while index < limit {
        if index & 0x3ff == 0 {
            check_cancel(cancel)?;
        }
        if bytes[index] == b'}' {
            return Ok(index.saturating_add(1));
        }
        index = index.saturating_add(1);
    }
    check_cancel(cancel)?;
    Ok(limit)
}

fn paren_star_end(
    bytes: &[u8],
    mut index: usize,
    limit: usize,
    cancel: &AtomicBool,
) -> Result<usize, String> {
    while index < limit {
        if index & 0x3ff == 0 {
            check_cancel(cancel)?;
        }
        if bytes[index] == b'*' && bytes.get(index.saturating_add(1)) == Some(&b')') {
            return Ok(index.saturating_add(2));
        }
        index = index.saturating_add(1);
    }
    check_cancel(cancel)?;
    Ok(limit)
}

#[cfg(test)]
mod tests {
    use super::{
        DOCUMENTATION_PARSE_WORK, Documentation, MAX_DOCUMENTATION_PARSE_WORK,
        MAX_DOCUMENTATION_RENDERED_BYTES, find_documentation, parse_markup,
        parse_markup_with_cancel, scan_comments,
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
            Some("Reads `A_B` &amp; `Name` and `Thing`.\n\n**Remarks**\n\nMore details.\n\n**Parameters**\n\n- `name` — An argument.\n- `other` — An argument.\n\n**Returns**\n\nThe result.".to_owned())
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
    fn parameter_expansion_is_bounded_before_appending_duplicate_names() {
        let names = std::iter::repeat_n("X", 1500).collect::<Vec<_>>().join(",");
        let value = "x".repeat(60_000);
        DOCUMENTATION_PARSE_WORK.with(|work| work.set(0));
        let documentation = parse_markup(&format!("<param name=\"{names}\">{value}</param>"))
            .expect("deduplicated parameter documentation");

        assert_eq!(
            documentation
                .parameter("x", MarkupKind::PlainText, &not_cancelled())
                .expect("deduplicated parameter value")
                .as_deref()
                .map(str::len),
            Some(value.len())
        );
        DOCUMENTATION_PARSE_WORK.with(|work| {
            assert!(
                work.get() <= MAX_DOCUMENTATION_PARSE_WORK,
                "parser work was amplified: {}",
                work.get()
            );
        });
    }

    #[test]
    fn parameter_metadata_is_bounded_before_appending_distinct_names() {
        let mut parameters = String::new();
        for index in 0..2000 {
            parameters.push_str(&format!("<param name=\"P{index}\">x</param>"));
        }

        DOCUMENTATION_PARSE_WORK.with(|work| work.set(0));
        assert!(parse_markup(&parameters).is_none());
        DOCUMENTATION_PARSE_WORK.with(|work| {
            assert!(
                work.get() <= MAX_DOCUMENTATION_PARSE_WORK,
                "parser work escaped its bound: {}",
                work.get()
            );
        });
    }

    #[test]
    fn malformed_angle_regions_consume_bounded_parser_work() {
        let malformed = "<".repeat(60 * 1024);
        let value = format!("<summary>{malformed}></summary>");

        DOCUMENTATION_PARSE_WORK.with(|work| work.set(0));
        assert!(parse_markup(&value).is_some());
        DOCUMENTATION_PARSE_WORK.with(|work| {
            assert!(
                work.get() <= MAX_DOCUMENTATION_PARSE_WORK,
                "malformed markup escaped its work bound: {}",
                work.get()
            );
        });
    }

    #[test]
    fn parsing_propagates_cancellation_before_markup_work() {
        let cancel = AtomicBool::new(true);
        assert_eq!(
            parse_markup_with_cancel("<summary>cancelled</summary>", &cancel)
                .expect_err("cancelled parser should fail"),
            "request cancelled"
        );
    }

    #[test]
    fn unsupported_markup_and_nested_code_are_preserved_as_safe_text() {
        let self_closing = parse_markup("<summary>Use <widget/> here</summary>")
            .expect("unsupported markup should remain documentation");
        assert_eq!(
            render(&self_closing, MarkupKind::PlainText),
            Some("Use <widget/> here".to_owned())
        );

        let only_self_closing =
            parse_markup("<widget/>").expect("unsupported-only markup should remain documentation");
        assert_eq!(
            render(&only_self_closing, MarkupKind::PlainText),
            Some("<widget/>".to_owned())
        );

        let nested_code = parse_markup("<summary><code>before<c>inner</c>after</code></summary>")
            .expect("nested code documentation");
        assert_eq!(
            render(&nested_code, MarkupKind::Markdown),
            Some("`beforeinnerafter`".to_owned())
        );

        let malformed = parse_markup("<summary>before <widget value></summary>")
            .expect("malformed markup should remain documentation");
        assert_eq!(
            render(&malformed, MarkupKind::PlainText),
            Some("before <widget value>".to_owned())
        );
    }

    #[test]
    fn markdown_code_spans_preserve_boundary_backticks_and_spaces() {
        let documentation =
            parse_markup("<summary><c>`foo`</c>|<c> foo </c>|<c> foo</c>|<c>foo </c></summary>")
                .expect("code span documentation");

        assert_eq!(
            render(&documentation, MarkupKind::Markdown),
            Some("`` `foo` ``\\|`  foo  `\\|` foo`\\|`foo `".to_owned())
        );

        let whitespace = parse_markup("<summary><c>  foo  bar\nbaz  </c></summary>")
            .expect("code whitespace documentation");
        assert_eq!(
            render(&whitespace, MarkupKind::Markdown),
            Some("`   foo  bar baz   `".to_owned())
        );
    }

    #[test]
    fn documentation_license_filters_apply_to_complete_line_and_block_groups() {
        let source = "/// <summary>Documented\n/// Copyright 2026; all rights reserved\n/// </summary>\nprocedure LineLicensed;\n(* <summary>Copyright 2026; all rights reserved</summary> *)\nprocedure BlockLicensed;\n";
        let comments = scan_comments(source, &not_cancelled()).expect("comments scan");

        for declaration in ["procedure LineLicensed", "procedure BlockLicensed"] {
            assert!(
                find_documentation(
                    source,
                    &comments,
                    source.find(declaration).expect("licensed declaration"),
                    &not_cancelled(),
                )
                .expect("license lookup")
                .is_none(),
                "license documentation attached to {declaration}"
            );
        }
    }

    #[test]
    fn xml_entities_are_decoded_once_in_text_and_attributes() {
        let documentation = parse_markup(
            "<summary>&amp;lt;tag&amp;gt; <paramref name=\"&amp;lt;Name&amp;gt;\"/> and <see cref=\"T:&amp;lt;Thing&amp;gt;\"/></summary><param name=\"X\">&amp;lt;tag&amp;gt;</param>",
        )
        .expect("entity documentation");

        assert_eq!(
            render(&documentation, MarkupKind::PlainText),
            Some(
                "&lt;tag&gt; &lt;Name&gt; and &lt;Thing&gt;\n\nParameters:\nx: &lt;tag&gt;"
                    .to_owned()
            )
        );
        assert_eq!(
            documentation
                .parameter("X", MarkupKind::PlainText, &not_cancelled())
                .expect("parameter documentation"),
            Some("&lt;tag&gt;".to_owned())
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

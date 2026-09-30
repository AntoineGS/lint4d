use crate::TargetPlatform;
use quick_xml::Reader;
use quick_xml::events::Event;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildCandidates {
    pub configs: Vec<String>,
    pub platforms: Vec<String>,
    pub default_config: Option<String>,
    pub default_platform: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum BuildSelectionMode {
    Session,
    Configured,
    #[default]
    ProjectDefault,
    Invalid,
}

impl BuildSelectionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Configured => "configured",
            Self::ProjectDefault => "projectDefault",
            Self::Invalid => "invalid",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildSelection {
    pub selected: Option<String>,
    pub candidates: Vec<String>,
    pub mode: BuildSelectionMode,
    pub project_default: Option<String>,
}

#[derive(Default)]
struct ElementFrame {
    name: String,
    attributes: std::collections::HashMap<String, String>,
    text: String,
    is_first_unconditioned_property_group: bool,
    platform_value: Option<String>,
}

/// Read build configurations, enabled platforms, and project defaults from a
/// `.dproj` document. Malformed trailing XML leaves the metadata parsed so far.
pub fn parse_build_candidates(xml: &str) -> BuildCandidates {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut stack = Vec::<ElementFrame>::new();
    let mut candidates = BuildCandidates::default();
    let mut has_platforms_block = false;
    let mut saw_unconditioned_property_group = false;
    let mut project_version = None;
    let mut compiler = None;
    let mut dcc_platform = None;
    let mut explicit_default_platform = None;

    while let Ok(event) = reader.read_event_into(&mut buffer) {
        match event {
            Event::Start(start) => {
                let name = super::local_name(start.name().as_ref());
                let attributes =
                    super::xml_attributes(&start, reader.decoder()).unwrap_or_default();
                let frame = open_element(
                    name,
                    attributes,
                    &stack,
                    &mut candidates.configs,
                    &mut has_platforms_block,
                    &mut saw_unconditioned_property_group,
                );
                stack.push(frame);
            }
            Event::Empty(empty) => {
                let name = super::local_name(empty.name().as_ref());
                let attributes =
                    super::xml_attributes(&empty, reader.decoder()).unwrap_or_default();
                let frame = open_element(
                    name,
                    attributes,
                    &stack,
                    &mut candidates.configs,
                    &mut has_platforms_block,
                    &mut saw_unconditioned_property_group,
                );
                close_element(
                    frame,
                    &stack,
                    &mut candidates,
                    &mut project_version,
                    &mut compiler,
                    &mut dcc_platform,
                    &mut explicit_default_platform,
                );
            }
            Event::Text(text) => {
                if let Some(frame) = stack.last_mut()
                    && let Ok(decoded) = text.unescape()
                {
                    frame.text.push_str(&decoded);
                }
            }
            Event::CData(text) => {
                if let Some(frame) = stack.last_mut() {
                    frame.text.push_str(&String::from_utf8_lossy(text.as_ref()));
                }
            }
            Event::End(_) => {
                if let Some(frame) = stack.pop() {
                    close_element(
                        frame,
                        &stack,
                        &mut candidates,
                        &mut project_version,
                        &mut compiler,
                        &mut dcc_platform,
                        &mut explicit_default_platform,
                    );
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }

    let derived_platform = if has_platforms_block {
        None
    } else {
        derive_platform(
            compiler.as_deref(),
            dcc_platform.as_deref(),
            project_version.as_deref(),
        )
    };
    if let Some(platform) = &derived_platform {
        push_unique_case_insensitive(&mut candidates.platforms, platform);
    }
    candidates.default_platform = explicit_default_platform.or(derived_platform);
    candidates
}

/// Match the resolved context value to the spelling reported by the project.
pub(crate) fn selection_for_value(
    value: Option<&str>,
    candidates: &[String],
    project_default: Option<&str>,
    configured: bool,
    is_platform: bool,
) -> BuildSelection {
    let selected = value.and_then(|value| {
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        let value = if is_platform {
            TargetPlatform::parse(value)
                .map(|platform| platform.name().to_owned())
                .unwrap_or_else(|| value.to_owned())
        } else {
            value.to_owned()
        };
        Some(
            candidates
                .iter()
                .find(|candidate| candidate.eq_ignore_ascii_case(&value))
                .cloned()
                .unwrap_or(value),
        )
    });

    BuildSelection {
        selected,
        candidates: candidates.to_vec(),
        mode: if configured {
            BuildSelectionMode::Configured
        } else {
            BuildSelectionMode::ProjectDefault
        },
        project_default: project_default.map(ToOwned::to_owned),
    }
}

fn open_element(
    name: String,
    attributes: std::collections::HashMap<String, String>,
    stack: &[ElementFrame],
    configs: &mut Vec<String>,
    has_platforms_block: &mut bool,
    saw_unconditioned_property_group: &mut bool,
) -> ElementFrame {
    let lower_name = name.to_ascii_lowercase();
    if lower_name == "buildconfiguration"
        && stack
            .last()
            .is_some_and(|parent| parent.name.eq_ignore_ascii_case("itemgroup"))
        && let Some(config) = attributes.get("include")
    {
        let config = config.trim();
        if !config.eq_ignore_ascii_case("base") {
            push_unique_case_insensitive(configs, config);
        }
    }

    if lower_name == "platforms" && path_ends_with(stack, &["projectextensions", "borlandproject"])
    {
        *has_platforms_block = true;
    }

    let is_first_unconditioned_property_group = lower_name == "propertygroup"
        && path_is(stack, &["project"])
        && attributes
            .get("condition")
            .is_none_or(|condition| condition.trim().is_empty())
        && !*saw_unconditioned_property_group;
    if is_first_unconditioned_property_group {
        *saw_unconditioned_property_group = true;
    }

    let platform_value = (lower_name == "platform"
        && path_ends_with(stack, &["projectextensions", "borlandproject", "platforms"]))
    .then(|| attributes.get("value").cloned())
    .flatten();

    ElementFrame {
        name,
        attributes,
        text: String::new(),
        is_first_unconditioned_property_group,
        platform_value,
    }
}

#[allow(clippy::too_many_arguments)]
fn close_element(
    frame: ElementFrame,
    ancestors: &[ElementFrame],
    candidates: &mut BuildCandidates,
    project_version: &mut Option<String>,
    compiler: &mut Option<String>,
    dcc_platform: &mut Option<String>,
    explicit_default_platform: &mut Option<String>,
) {
    let value = frame.text.trim();
    if let Some(platform_value) = frame.platform_value
        && value.eq_ignore_ascii_case("true")
        && let Some(platform) = normalized_platform(&platform_value)
    {
        push_unique_case_insensitive(&mut candidates.platforms, &platform);
    }

    if is_root_property(ancestors) {
        match frame.name.to_ascii_lowercase().as_str() {
            "projectversion" if project_version.is_none() && !value.is_empty() => {
                *project_version = Some(value.to_owned());
            }
            "dcc_dcccompiler" if compiler.is_none() && !value.is_empty() => {
                *compiler = Some(value.to_owned());
            }
            "dcc_platform" if dcc_platform.is_none() && !value.is_empty() => {
                *dcc_platform = Some(value.to_owned());
            }
            _ => {}
        }
    }

    if ancestors.len() == 2
        && path_is(ancestors, &["project", "propertygroup"])
        && ancestors[1].is_first_unconditioned_property_group
    {
        match frame.name.to_ascii_lowercase().as_str() {
            "config"
                if candidates.default_config.is_none()
                    && has_empty_property_condition(&frame.attributes, "config")
                    && !value.is_empty() =>
            {
                candidates.default_config = Some(value.to_owned());
            }
            "platform"
                if explicit_default_platform.is_none()
                    && has_empty_property_condition(&frame.attributes, "platform")
                    && !value.is_empty() =>
            {
                *explicit_default_platform = normalized_platform(value);
            }
            _ => {}
        }
    }
}

fn is_root_property(ancestors: &[ElementFrame]) -> bool {
    ancestors.len() == 2 && path_is(ancestors, &["project", "propertygroup"])
}

fn path_is(stack: &[ElementFrame], names: &[&str]) -> bool {
    stack.len() == names.len()
        && stack
            .iter()
            .zip(names)
            .all(|(frame, name)| frame.name.eq_ignore_ascii_case(name))
}

fn path_ends_with(stack: &[ElementFrame], names: &[&str]) -> bool {
    stack.len() >= names.len()
        && stack[stack.len() - names.len()..]
            .iter()
            .zip(names)
            .all(|(frame, name)| frame.name.eq_ignore_ascii_case(name))
}

fn has_empty_property_condition(
    attributes: &std::collections::HashMap<String, String>,
    property_name: &str,
) -> bool {
    let expected = format!("'$({property_name})'==''");
    attributes.get("condition").is_some_and(|condition| {
        condition
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>()
            .eq_ignore_ascii_case(&expected)
    })
}

fn push_unique_case_insensitive(values: &mut Vec<String>, value: &str) {
    let value = value.trim();
    if !value.is_empty()
        && !values
            .iter()
            .any(|current| current.eq_ignore_ascii_case(value))
    {
        values.push(value.to_owned());
    }
}

fn normalized_platform(value: &str) -> Option<String> {
    TargetPlatform::parse(value).map(|platform| platform.name().to_owned())
}

fn derive_platform(
    compiler: Option<&str>,
    dcc_platform: Option<&str>,
    project_version: Option<&str>,
) -> Option<String> {
    match compiler
        .map(str::trim)
        .map(str::to_ascii_uppercase)
        .as_deref()
    {
        Some("DCC32") => return Some("Win32".to_owned()),
        Some("DCC64") => return Some("Win64".to_owned()),
        _ => {}
    }
    if let Some(platform) = dcc_platform.and_then(normalized_platform) {
        return Some(platform);
    }
    let major_version = project_version?
        .trim()
        .split('.')
        .next()?
        .trim()
        .parse::<u32>()
        .ok()?;
    (major_version < 13).then(|| "Win32".to_owned())
}

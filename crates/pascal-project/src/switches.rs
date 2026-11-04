//! D45: compiler switch facts (`IFOPT`) for project and library code.
//!
//! A switch the evaluated metadata leaves unset takes dcc's default, as the
//! compiler does. The defaults were observed with each installation's own
//! compiler: a program testing `{$IFOPT X+}` for every switch, compiled with
//! `--no-config` (as CodeGear.Delphi.Targets does, `NoConfig="true"`) by
//! dcc32 21.0 (Delphi 2010), dcc32/dcc64 24.0 (XE3) and dcc32/dcc64 37.0;
//! all five agree, and agree with the `DefaultValue` of each option in the
//! DCC task's option table (Borland.Build.Tasks.Delphi.dll). Other compiler
//! versions and platforms were not observed and keep switches unknown.
//!
//! What can still change a switch after the project's groups, and how it is
//! handled:
//! - CodeGear.Delphi.Targets and CodeGear.Common.Targets assign none of these
//!   properties (7.0, 10.0 and 37.0).
//! - CodeGear.Common.Targets imports `$(GlobalOptionFile)` after the project
//!   ("common paths and compiler options"). That file is not evaluated, so
//!   every switch is unknown when the property is set, unknown, or could
//!   have been set by an import that was not evaluated.
//! - CodeGear.Common.Targets also imports the installation's
//!   `environment.proj` and `EnvOptions.proj` after the project. lint4d
//!   seeds `environment.proj` before the project, so a switch it sets is
//!   only known where the project's value equals it. `EnvOptions.proj` is
//!   read for library paths only; the installations' copies set no switch.

use crate::installations::InstallationSelection;
use crate::msbuild_eval::ProjectBuilder;
use crate::{CompilerVersion, ConditionalContext, ConditionalFact, TargetPlatform};
use std::collections::BTreeMap;

#[derive(Clone, Copy)]
enum Values {
    /// An MSBuild boolean.
    Boolean,
    /// `DCC_DebugInformation`: a boolean before 37.0, then `0`/`1`/`2`
    /// (`-$D0`, `-$D1`, `-$D2`); `$D1` and `$D2` answer `IFOPT D+`.
    DebugLevel,
    /// `DCC_SymbolReferenceInfo`: `0`/`1`/`2` (`-$Y-`, `-$YD`, `-$Y+`);
    /// `$YD` answers `IFOPT Y+`.
    SymbolLevel,
}

struct Switch {
    property: &'static str,
    option: &'static str,
    default: ConditionalFact,
    values: Values,
}

const fn switch(
    property: &'static str,
    option: &'static str,
    default: ConditionalFact,
    values: Values,
) -> Switch {
    Switch {
        property,
        option,
        default,
        values,
    }
}

/// The switches pascal-core's `IFOPT` evaluates, with the DCC task property
/// CodeGear.Delphi.Targets passes for each. J, M, P, V, W, L, G and U are not
/// evaluated by `IFOPT` yet.
const SWITCHES: &[Switch] = &[
    switch(
        "dcc_fullbooleanevaluations",
        "B",
        ConditionalFact::False,
        Values::Boolean,
    ),
    switch(
        "dcc_assertionsatruntime",
        "C",
        ConditionalFact::True,
        Values::Boolean,
    ),
    switch(
        "dcc_debuginformation",
        "D",
        ConditionalFact::True,
        Values::DebugLevel,
    ),
    switch(
        "dcc_longstrings",
        "H",
        ConditionalFact::True,
        Values::Boolean,
    ),
    switch(
        "dcc_iochecking",
        "I",
        ConditionalFact::True,
        Values::Boolean,
    ),
    switch("dcc_optimize", "O", ConditionalFact::True, Values::Boolean),
    switch(
        "dcc_integeroverflowcheck",
        "Q",
        ConditionalFact::False,
        Values::Boolean,
    ),
    switch(
        "dcc_rangechecking",
        "R",
        ConditionalFact::False,
        Values::Boolean,
    ),
    switch(
        "dcc_typedatparameter",
        "T",
        ConditionalFact::False,
        Values::Boolean,
    ),
    switch(
        "dcc_extendedsyntax",
        "X",
        ConditionalFact::True,
        Values::Boolean,
    ),
    switch(
        "dcc_symbolreferenceinfo",
        "Y",
        ConditionalFact::True,
        Values::SymbolLevel,
    ),
];

/// Whether the selected compiler's defaults are known: a selected (or
/// legacy) installation that loaded, and a compiler and platform whose dcc
/// was observed.
pub(crate) fn defaults_known(
    selection: Option<&InstallationSelection>,
    installation_failed: bool,
    platform: Option<&TargetPlatform>,
    platform_unknown: bool,
    version: Option<CompilerVersion>,
) -> bool {
    let observed: &[u32] = match platform {
        Some(TargetPlatform::Win32) => &[21, 24, 37],
        Some(TargetPlatform::Win64) => &[24, 37],
        _ => &[],
    };
    !installation_failed
        && !platform_unknown
        && matches!(
            selection,
            None | Some(InstallationSelection::Legacy | InstallationSelection::Selected { .. })
        )
        && version.is_some_and(|version| {
            observed
                .iter()
                .any(|major| version == CompilerVersion::new(*major, 0))
        })
}

/// The evaluated switch facts; `None` leaves a switch out (unknown).
pub(crate) struct SwitchFacts {
    project: Vec<(&'static str, Option<ConditionalFact>)>,
    library: Vec<(&'static str, Option<ConditionalFact>)>,
}

impl SwitchFacts {
    pub(crate) fn evaluate(
        builder: &ProjectBuilder,
        installation: Option<&BTreeMap<String, String>>,
        defaults_known: bool,
    ) -> Self {
        let global_option_file = global_option_file_possible(builder);
        let unset_is_default =
            defaults_known && !builder.incomplete && builder.undefined_is_empty();
        let mut facts = Self {
            project: Vec::new(),
            library: Vec::new(),
        };
        for switch in SWITCHES {
            let installed = installation.and_then(|properties| {
                properties
                    .iter()
                    .find(|(name, value)| {
                        name.eq_ignore_ascii_case(switch.property) && !value.trim().is_empty()
                    })
                    .map(|(_, value)| parse(switch.values, value))
            });
            let project = if global_option_file {
                None
            } else {
                project_value(builder, switch, installed, unset_is_default)
            };
            // Library code is compiled with the project's switches when its
            // source is on the search path, and with unknown ones when it
            // ships as a DCU; only the default (or installation value) that
            // the project also has is certain.
            let expected = installed.unwrap_or(switch.default);
            let library = (defaults_known
                && expected != ConditionalFact::Unknown
                && project == Some(expected))
            .then_some(expected);
            facts.project.push((switch.option, project));
            facts.library.push((switch.option, library));
        }
        facts
    }

    /// Caller-supplied options win.
    pub(crate) fn apply_project(&self, context: &mut ConditionalContext) {
        apply(context, &self.project);
    }

    /// Caller-supplied options win.
    pub(crate) fn apply_library(&self, context: &mut ConditionalContext) {
        apply(context, &self.library);
    }
}

fn apply(context: &mut ConditionalContext, facts: &[(&'static str, Option<ConditionalFact>)]) {
    for (option, fact) in facts {
        if let Some(fact) = fact
            && !context.options.contains_key(*option)
        {
            context.set_option(option, *fact);
        }
    }
}

fn project_value(
    builder: &ProjectBuilder,
    switch: &Switch,
    installed: Option<ConditionalFact>,
    unset_is_default: bool,
) -> Option<ConditionalFact> {
    if builder.unknown_properties.contains(switch.property) {
        return Some(ConditionalFact::Unknown);
    }
    let evaluated = builder
        .property(switch.property)
        .filter(|raw| !raw.trim().is_empty())
        .map(|raw| parse(switch.values, &raw));
    match (evaluated, installed) {
        // environment.proj is imported after the project: its value holds
        // only where the project's agrees.
        (Some(evaluated), Some(installed)) if evaluated == installed => Some(installed),
        (_, Some(_)) => Some(ConditionalFact::Unknown),
        (Some(evaluated), None) => Some(evaluated),
        (None, None) => unset_is_default.then_some(switch.default),
    }
}

fn global_option_file_possible(builder: &ProjectBuilder) -> bool {
    !builder.undefined_is_empty()
        || builder.unknown_properties.contains("globaloptionfile")
        || builder
            .property("globaloptionfile")
            .is_some_and(|value| !value.trim().is_empty())
}

fn parse(values: Values, raw: &str) -> ConditionalFact {
    let raw = raw.trim();
    match (values, raw) {
        (Values::DebugLevel | Values::SymbolLevel, "0") => ConditionalFact::False,
        (Values::DebugLevel | Values::SymbolLevel, "1" | "2") => ConditionalFact::True,
        (Values::SymbolLevel, _) => ConditionalFact::Unknown,
        _ => msbuild_boolean(raw),
    }
}

/// MSBuild's boolean spellings (`ConversionUtilities.ConvertStringToBool`);
/// the DCC task rejects anything else, so it says nothing about the switch.
fn msbuild_boolean(raw: &str) -> ConditionalFact {
    match raw.to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" | "!false" | "!off" | "!no" => ConditionalFact::True,
        "false" | "off" | "no" | "!true" | "!on" | "!yes" => ConditionalFact::False,
        _ => ConditionalFact::Unknown,
    }
}

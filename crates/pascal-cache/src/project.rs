use crate::{CanonicalEncode, Encoder};
use pascal_project::delphi_overrides::{EffectiveOverrides, PathMapping};
use pascal_project::installations::InstallationEvidence;
use pascal_project::rtl_constants::RtlConstantSource;
use pascal_project::{
    BuildSelection, BuildSelectionMode, CompilerVersion, ConditionalClosure, ConditionalContext,
    ConditionalFact, ConstantValue, InstallationOrigin, InstallationSelection, LayoutContext,
    LayoutPlatform, LayoutSettings, OpenReason, ProjectPathEntry, ProjectPathIssue,
    ProjectPathIssueKind, ProjectPathProvenance, ReadPolicy, ReadRootView, SourceOrigin,
};

fn strings(encoder: &mut Encoder, values: &[String]) {
    encoder.seq(values, |encoder, value| encoder.str(value));
}

impl CanonicalEncode for ConditionalFact {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.variant(match self {
            Self::True => 1,
            Self::False => 2,
            Self::Unknown => 3,
        });
    }
}

impl CanonicalEncode for CompilerVersion {
    fn encode(&self, encoder: &mut Encoder) {
        match self.numeric_parts() {
            Some((mantissa, scale)) => {
                encoder.variant(1);
                encoder.u64(mantissa);
                encoder.u8(scale);
            }
            None => {
                encoder.variant(2);
                encoder.u32(self.major);
                encoder.u32(self.minor);
                encoder.u32(self.patch);
            }
        }
    }
}

impl CanonicalEncode for ConstantValue {
    fn encode(&self, encoder: &mut Encoder) {
        match self {
            Self::Boolean(value) => {
                encoder.variant(1);
                encoder.bool(*value);
            }
            Self::Integer(value) => {
                encoder.variant(2);
                encoder.i64(*value);
            }
            Self::String(value) => {
                encoder.variant(3);
                encoder.str(value);
            }
            Self::Version(value) => {
                encoder.variant(4);
                encoder.encode(value);
            }
        }
    }
}

impl CanonicalEncode for LayoutPlatform {
    fn encode(&self, encoder: &mut Encoder) {
        match self {
            Self::Win32 => encoder.variant(1),
            Self::Win64 => encoder.variant(2),
            Self::Other(name) => {
                encoder.variant(3);
                encoder.str(name);
            }
        }
    }
}

impl CanonicalEncode for LayoutSettings {
    fn encode(&self, encoder: &mut Encoder) {
        let Self {
            record_alignment,
            minimum_enum_size,
            long_strings,
            old_type_layout,
            real_compatibility,
        } = self;
        encoder.field(1);
        encoder.option(record_alignment.as_ref(), |encoder, value| {
            encoder.u8(*value)
        });
        encoder.field(2);
        encoder.option(minimum_enum_size.as_ref(), |encoder, value| {
            encoder.u8(*value)
        });
        encoder.field(3);
        encoder.encode(long_strings);
        encoder.field(4);
        encoder.encode(old_type_layout);
        encoder.field(5);
        encoder.encode(real_compatibility);
    }
}

impl CanonicalEncode for LayoutContext {
    fn encode(&self, encoder: &mut Encoder) {
        let Self { platform, defaults } = self;
        encoder.field(1);
        encoder.option(platform.as_ref(), |encoder, platform| {
            encoder.encode(platform)
        });
        encoder.field(2);
        encoder.encode(defaults);
    }
}

impl CanonicalEncode for ConditionalContext {
    fn encode(&self, encoder: &mut Encoder) {
        let Self {
            compiler_version,
            layout,
            layout_explicit,
            defines,
            options,
            constants,
            absent_define,
            rtl_constants_known,
        } = self;
        encoder.field(1);
        encoder.option(compiler_version.as_ref(), |encoder, version| {
            encoder.encode(version)
        });
        encoder.field(2);
        encoder.encode(layout);
        encoder.field(3);
        encoder.bool(*layout_explicit);
        encoder.field(4);
        encoder.map(
            defines,
            |encoder, name| encoder.str(name),
            |encoder, fact| encoder.encode(fact),
        );
        encoder.field(5);
        encoder.map(
            options,
            |encoder, name| encoder.str(name),
            |encoder, fact| encoder.encode(fact),
        );
        encoder.field(6);
        encoder.map(
            constants,
            |encoder, name| encoder.str(name),
            |encoder, value| encoder.encode(value),
        );
        encoder.field(7);
        encoder.encode(absent_define);
        encoder.field(8);
        encoder.bool(*rtl_constants_known);
    }
}

impl CanonicalEncode for ProjectPathProvenance {
    fn encode(&self, encoder: &mut Encoder) {
        match self {
            Self::LegacyNative => encoder.variant(1),
            Self::Configured => encoder.variant(2),
            Self::Mapped { root } => {
                encoder.variant(3);
                encoder.encode(root.as_path());
            }
        }
    }
}

impl CanonicalEncode for ProjectPathEntry {
    fn encode(&self, encoder: &mut Encoder) {
        let Self { path, provenance } = self;
        encoder.field(1);
        encoder.encode(path.as_path());
        encoder.field(2);
        encoder.encode(provenance);
    }
}

impl CanonicalEncode for ProjectPathIssueKind {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.variant(match self {
            Self::MissingReference => 1,
            Self::MissingMainSource => 2,
            Self::MissingDirectory => 3,
            Self::UnresolvedProperty => 4,
            Self::Unreadable => 5,
        });
    }
}

impl CanonicalEncode for ProjectPathIssue {
    fn encode(&self, encoder: &mut Encoder) {
        let Self {
            kind,
            source_file,
            property,
            raw,
            path,
            unit_name,
            provenance,
        } = self;
        encoder.field(1);
        encoder.encode(kind);
        encoder.field(2);
        encoder.encode(source_file.as_path());
        encoder.field(3);
        encoder.str(property);
        encoder.field(4);
        encoder.str(raw);
        encoder.field(5);
        encoder.option(path.as_deref(), |encoder, path| encoder.encode(path));
        encoder.field(6);
        encoder.option(unit_name.as_deref(), |encoder, name| encoder.str(name));
        encoder.field(7);
        encoder.encode(provenance);
    }
}

impl CanonicalEncode for InstallationEvidence {
    fn encode(&self, encoder: &mut Encoder) {
        let Self {
            compiler_version,
            project_version,
            bds_root,
            conflicting,
        } = self;
        encoder.field(1);
        encoder.option(compiler_version.as_ref(), |encoder, version| {
            encoder.encode(version)
        });
        encoder.field(2);
        encoder.option(project_version.as_deref(), |encoder, version| {
            encoder.str(version)
        });
        encoder.field(3);
        encoder.option(bds_root.as_deref(), |encoder, root| encoder.str(root));
        encoder.field(4);
        encoder.bool(*conflicting);
    }
}

impl CanonicalEncode for InstallationOrigin {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.variant(match self {
            Self::Session => 1,
            Self::Configured => 2,
            Self::Metadata => 3,
        });
    }
}

impl CanonicalEncode for InstallationSelection {
    fn encode(&self, encoder: &mut Encoder) {
        match self {
            Self::Legacy => encoder.variant(1),
            Self::Selected { id, origin } => {
                encoder.variant(2);
                encoder.str(id);
                encoder.encode(origin);
            }
            Self::NeedsChoice { candidates } => {
                encoder.variant(3);
                strings(encoder, candidates);
            }
            Self::Invalid { id } => {
                encoder.variant(4);
                encoder.str(id);
            }
        }
    }
}

impl CanonicalEncode for OpenReason {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.variant(match self {
            Self::NoProject => 1,
            Self::DiscoveryIncomplete => 2,
            Self::CompilerVersionUnknown => 3,
            Self::ConfigUnresolved => 4,
            Self::ConfigInvalid => 5,
            Self::PlatformUnresolved => 6,
            Self::PlatformInvalid => 7,
        });
    }
}

impl CanonicalEncode for RtlConstantSource {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.variant(match self {
            Self::Override => 1,
            Self::SystemPas => 2,
            Self::Table => 3,
            Self::Unknown => 4,
        });
    }
}

impl CanonicalEncode for ConditionalClosure {
    fn encode(&self, encoder: &mut Encoder) {
        let Self {
            closed,
            open_reasons,
            rtl_source,
            rtl_constants,
        } = self;
        encoder.field(1);
        encoder.bool(*closed);
        encoder.field(2);
        encoder.seq(open_reasons, |encoder, reason| encoder.encode(reason));
        encoder.field(3);
        encoder.encode(rtl_source);
        encoder.field(4);
        encoder.option(rtl_constants.as_deref(), |encoder, constants| {
            strings(encoder, constants)
        });
    }
}

impl CanonicalEncode for BuildSelectionMode {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.variant(match self {
            Self::Session => 1,
            Self::Configured => 2,
            Self::ProjectDefault => 3,
            Self::Invalid => 4,
        });
    }
}

impl CanonicalEncode for BuildSelection {
    fn encode(&self, encoder: &mut Encoder) {
        let Self {
            selected,
            candidates,
            mode,
            project_default,
        } = self;
        encoder.field(1);
        encoder.option(selected.as_deref(), |encoder, selected| {
            encoder.str(selected)
        });
        encoder.field(2);
        strings(encoder, candidates);
        encoder.field(3);
        encoder.encode(mode);
        encoder.field(4);
        encoder.option(project_default.as_deref(), |encoder, default| {
            encoder.str(default)
        });
    }
}

impl CanonicalEncode for PathMapping {
    fn encode(&self, encoder: &mut Encoder) {
        let Self {
            from,
            to,
            config_file,
        } = self;
        encoder.field(1);
        encoder.str(from);
        encoder.field(2);
        encoder.encode(to.as_path());
        encoder.field(3);
        encoder.encode(config_file.as_path());
    }
}

impl CanonicalEncode for EffectiveOverrides {
    fn encode(&self, encoder: &mut Encoder) {
        let Self {
            properties,
            property_origins,
            path_mappings,
        } = self;
        encoder.field(1);
        encoder.map(
            properties,
            |encoder, name| encoder.str(name),
            |encoder, value| encoder.str(value),
        );
        encoder.field(2);
        encoder.map(
            property_origins,
            |encoder, name| encoder.str(name),
            |encoder, origin| encoder.encode(origin.as_path()),
        );
        encoder.field(3);
        encoder.seq(path_mappings, |encoder, mapping| encoder.encode(mapping));
    }
}

impl CanonicalEncode for ReadPolicy {
    fn encode(&self, encoder: &mut Encoder) {
        let view = self.view();
        let roots = |encoder: &mut Encoder, roots: &[ReadRootView<'_>]| {
            encoder.seq(roots, |encoder, root| {
                let ReadRootView {
                    path,
                    pattern_exclusion_bases,
                } = root;
                encoder.field(1);
                encoder.encode(*path);
                encoder.field(2);
                encoder.seq(*pattern_exclusion_bases, |encoder, base| {
                    encoder.encode(base.as_path())
                });
            });
        };
        encoder.field(1);
        roots(encoder, &view.configured_roots);
        encoder.field(2);
        roots(encoder, &view.mapped_roots);
        encoder.field(3);
        encoder.seq(view.authorized_files, |encoder, path| {
            encoder.encode(path.as_path())
        });
        encoder.field(4);
        encoder.seq(view.exclusions, |encoder, exclusion| encoder.str(exclusion));
        encoder.field(5);
        encoder.seq(view.exclusion_bases, |encoder, base| {
            encoder.encode(base.as_path())
        });
    }
}

impl CanonicalEncode for SourceOrigin {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.variant(match self {
            Self::ProjectCompiled => 1,
            Self::Library => 2,
        });
    }
}

#[cfg(test)]
mod tests {
    use crate::Encoder;
    use pascal_project::{ConditionalContext, ConditionalFact};

    const DEFAULT_CONDITIONAL_CONTEXT: &[u8] = &[
        1, 0, 0, // compiler_version: None
        2, 0, // layout
        1, 0, 0, //   platform: None
        2, 0, //   defaults
        1, 0, 0, //     record_alignment: None
        2, 0, 0, //     minimum_enum_size: None
        3, 0, 3, //     long_strings: Unknown
        4, 0, 3, //     old_type_layout: Unknown
        5, 0, 3, //     real_compatibility: Unknown
        3, 0, 0, // layout_explicit: false
        4, 0, 0, 0, 0, 0, 0, 0, 0, 0, // defines: 0 entries
        5, 0, 0, 0, 0, 0, 0, 0, 0, 0, // options: 0 entries
        6, 0, 0, 0, 0, 0, 0, 0, 0, 0, // constants: 0 entries
        7, 0, 3, // absent_define: Unknown
        8, 0, 0, // rtl_constants_known: false
    ];

    fn encoded(context: &ConditionalContext) -> Vec<u8> {
        let mut encoder = Encoder::new();
        encoder.encode(context);
        encoder.into_bytes()
    }

    #[test]
    fn default_conditional_context_has_its_canonical_bytes() {
        assert_eq!(
            encoded(&ConditionalContext::default()),
            DEFAULT_CONDITIONAL_CONTEXT
        );
    }

    #[test]
    fn a_define_is_one_map_entry() {
        let mut context = ConditionalContext::default();
        context.set_define("DEBUG", ConditionalFact::True);
        let mut expected = DEFAULT_CONDITIONAL_CONTEXT[..28].to_vec();
        expected.extend([4, 0, 1, 0, 0, 0, 0, 0, 0, 0]); // defines: 1 entry
        expected.extend([5, 0, 0, 0, 0, 0, 0, 0, b'D', b'E', b'B', b'U', b'G', 1]);
        expected.extend_from_slice(&DEFAULT_CONDITIONAL_CONTEXT[38..]);
        assert_eq!(encoded(&context), expected);
    }
}

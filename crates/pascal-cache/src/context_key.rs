use crate::manifest::manifest_encoder;
use crate::{CanonicalKey, Digest, KeyKind};
use pascal_project::ProjectContext;

/// The complete analysis inputs of one project/configuration context.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContextKey(CanonicalKey);

impl ContextKey {
    pub fn new(context: &ProjectContext) -> Self {
        let mut encoder = manifest_encoder(KeyKind::Context);
        encoder.field(1);
        encoder.bool(context.discovery_complete());
        encoder.field(2);
        encoder.bool(context.binding_metadata_complete());
        encoder.field(3);
        encoder.seq(context.path_issues(), |encoder, issue| {
            encoder.encode(issue)
        });
        encoder.field(4);
        encoder.option(context.project_file(), |encoder, path| encoder.encode(path));
        encoder.field(5);
        encoder.encode(context.installation_evidence());
        encoder.field(6);
        encoder.option(context.installation_selection(), |encoder, selection| {
            encoder.encode(selection)
        });
        encoder.field(7);
        encoder.seq(context.search_path_entries(), |encoder, entry| {
            encoder.encode(entry)
        });
        encoder.field(8);
        encoder.seq(context.browsing_path_entries(), |encoder, entry| {
            encoder.encode(entry)
        });
        encoder.field(9);
        encoder.seq(context.debug_dcu_path_entries(), |encoder, entry| {
            encoder.encode(entry)
        });
        encoder.field(10);
        encoder.option(context.main_source_entry(), |encoder, entry| {
            encoder.encode(entry)
        });
        encoder.field(11);
        encoder.map(
            context.explicit_unit_entries(),
            |encoder, name| encoder.str(name),
            |encoder, entries| encoder.seq(entries, |encoder, entry| encoder.encode(entry)),
        );
        encoder.field(12);
        encoder.seq(context.include_path_entries(), |encoder, entry| {
            encoder.encode(entry)
        });
        encoder.field(13);
        encoder.seq(context.unit_namespaces(), |encoder, namespace| {
            encoder.str(namespace)
        });
        encoder.field(14);
        encoder.map(
            context.unit_aliases(),
            |encoder, alias| encoder.str(alias),
            |encoder, unit| encoder.str(unit),
        );
        encoder.field(15);
        encoder.encode(context.conditional_context());
        encoder.field(16);
        encoder.encode(context.conditional_closure());
        encoder.field(17);
        encoder.encode(context.library_conditional_context());
        encoder.field(18);
        encoder.seq(context.project_source_roots(), |encoder, root| {
            encoder.encode(root.as_path())
        });
        encoder.field(19);
        encoder.option(context.config(), |encoder, config| encoder.str(config));
        encoder.field(20);
        encoder.option(context.platform(), |encoder, platform| {
            encoder.str(platform)
        });
        encoder.field(21);
        encoder.encode(context.config_selection());
        encoder.field(22);
        encoder.encode(context.platform_selection());
        encoder.field(23);
        encoder.encode(context.overrides());
        encoder.field(24);
        encoder.encode(context.read_policy());
        encoder.field(25);
        encoder.seq(context.packages(), |encoder, package| encoder.str(package));
        encoder.field(26);
        encoder.seq(context.installation_config_files(), |encoder, path| {
            encoder.encode(path.as_path())
        });
        encoder.field(27);
        encoder.option(context.override_error(), |encoder, error| {
            encoder.str(error)
        });
        Self(CanonicalKey::finish(KeyKind::Context, encoder))
    }

    pub fn key(&self) -> &CanonicalKey {
        &self.0
    }

    pub fn digest(&self) -> Digest {
        self.0.digest()
    }
}

use crate::manifest::manifest_encoder;
use crate::{
    CanonicalKey, ContextKey, Digest, ExpandedArtifactKey, KeyKind, LocalArtifactKey,
    SourceIdentity,
};
use pascal_project::{ProjectPathEntry, SourceOrigin};

#[derive(Debug, Clone, Copy)]
pub enum ArtifactRef<'a> {
    Local(&'a LocalArtifactKey),
    Expanded(&'a ExpandedArtifactKey),
}

/// Everything that binds an artifact to one source in one context.
#[derive(Debug, Clone, Copy)]
pub struct MemberInputs<'a> {
    pub context: &'a ContextKey,
    pub source: &'a SourceIdentity,
    pub entry: Option<&'a ProjectPathEntry>,
    pub origin: SourceOrigin,
    /// Unit name inferred from the path when the source declares none. It
    /// belongs to the member, never to the path-neutral artifact.
    pub fallback_unit_name: Option<&'a str>,
    /// `None` for a recognized but unindexed member.
    pub artifact: Option<ArtifactRef<'a>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MemberKey(CanonicalKey);

impl MemberKey {
    pub fn new(inputs: &MemberInputs<'_>) -> Self {
        let MemberInputs {
            context,
            source,
            entry,
            origin,
            fallback_unit_name,
            artifact,
        } = inputs;
        let mut encoder = manifest_encoder(KeyKind::Member);
        encoder.field(1);
        encoder.digest(&context.digest());
        encoder.field(2);
        encoder.encode(*source);
        encoder.field(3);
        encoder.option(*entry, |encoder, entry| encoder.encode(entry));
        encoder.field(4);
        encoder.encode(origin);
        encoder.field(5);
        encoder.option(*fallback_unit_name, |encoder, name| encoder.str(name));
        encoder.field(6);
        encoder.option(artifact.as_ref(), |encoder, artifact| match artifact {
            ArtifactRef::Local(key) => {
                encoder.variant(1);
                encoder.digest(&key.digest());
            }
            ArtifactRef::Expanded(key) => {
                encoder.variant(2);
                encoder.digest(&key.digest());
            }
        });
        Self(CanonicalKey::finish(KeyKind::Member, encoder))
    }

    pub fn key(&self) -> &CanonicalKey {
        &self.0
    }

    pub fn digest(&self) -> Digest {
        self.0.digest()
    }
}

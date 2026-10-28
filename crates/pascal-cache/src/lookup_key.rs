//! Subjects of binding and discovery evidence (spec §6.1 amendment
//! 2026-10-27, §9): the lookup a resolver performed and the directories a
//! discovery scanned. `l4d-zf1b.8` makes producers' keys match resolver
//! behaviour; these encodings are pinned.

use crate::manifest::manifest_encoder;
use crate::{CanonicalKey, Digest, KeyKind};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LookupKind {
    Unit,
    Include,
}

#[derive(Debug, Clone, Copy)]
pub struct LookupInputs<'a> {
    pub kind: LookupKind,
    /// The name under the extractor's Pascal normalization (spec §5).
    pub normalized_name: &'a str,
    /// The path expression as written (`uses X in 'X.pas'`, `{$I X.inc}`).
    pub path_expression: Option<&'a str>,
    /// The importer's directory, only when the resolver consults it.
    pub importer_directory: Option<&'a Path>,
}

/// One resolver lookup, whatever it found: binding evidence proves its
/// outcome, including a negative one, under one context.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LookupKey(CanonicalKey);

impl LookupKey {
    pub fn new(inputs: &LookupInputs<'_>) -> Self {
        let mut encoder = manifest_encoder(KeyKind::Lookup);
        encoder.field(1);
        encoder.variant(match inputs.kind {
            LookupKind::Unit => 1,
            LookupKind::Include => 2,
        });
        encoder.field(2);
        encoder.str(inputs.normalized_name);
        encoder.field(3);
        encoder.option(inputs.path_expression, |encoder, expression| {
            encoder.str(expression)
        });
        encoder.field(4);
        encoder.option(inputs.importer_directory, |encoder, directory| {
            encoder.encode(directory)
        });
        Self(CanonicalKey::finish(KeyKind::Lookup, encoder))
    }

    pub fn key(&self) -> &CanonicalKey {
        &self.0
    }

    pub fn digest(&self) -> Digest {
        self.0.digest()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScannedDirectory<'a> {
    pub path: &'a Path,
    pub recursive: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct DiscoveryInputs<'a> {
    /// In scan order, which decides which of two equal names is found.
    pub directories: &'a [ScannedDirectory<'a>],
    /// Exclusion patterns as configured: a set, so order and repeats do
    /// not change the scope.
    pub exclusions: &'a [&'a str],
}

/// The directories a membership discovery scanned and what it excluded.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DiscoveryScope(CanonicalKey);

impl DiscoveryScope {
    pub fn new(inputs: &DiscoveryInputs<'_>) -> Self {
        let mut exclusions = inputs.exclusions.to_vec();
        exclusions.sort_unstable();
        exclusions.dedup();
        let mut encoder = manifest_encoder(KeyKind::Discovery);
        encoder.field(1);
        encoder.seq(inputs.directories, |encoder, directory| {
            encoder.field(1);
            encoder.encode(directory.path);
            encoder.field(2);
            encoder.bool(directory.recursive);
        });
        encoder.field(2);
        // Sets are ordered by their encoded bytes (canonical encoding v1).
        encoder.map(
            exclusions.into_iter().map(|pattern| (pattern, ())),
            |encoder, pattern| encoder.str(pattern),
            |_, ()| {},
        );
        Self(CanonicalKey::finish(KeyKind::Discovery, encoder))
    }

    pub fn key(&self) -> &CanonicalKey {
        &self.0
    }

    pub fn digest(&self) -> Digest {
        self.0.digest()
    }
}

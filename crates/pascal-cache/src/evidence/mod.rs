//! Durable observations: the inputs an analysis read or looked for, in a
//! form another process checks against the current filesystem (spec §5, §7).

mod decode;

pub use decode::{ObservationError, decode_observation};

use crate::manifest::manifest_encoder;
use crate::{CanonicalEncode, Digest, Encoder, KeyKind, MANIFEST_DIGEST_CONTEXT, SourceContent};
use pascal_project::ProjectPathEntry;
use std::path::{Path, PathBuf};

/// Domain of directory-listing digests.
pub const LISTING_DIGEST_CONTEXT: &str =
    "lint4d persistent analysis cache 2026-10 directory listing v1";
/// Domain of observation-list digests, which dependent keys hold.
pub const OBSERVATIONS_DIGEST_CONTEXT: &str =
    "lint4d persistent analysis cache 2026-10 observations v1";

pub(crate) const PAYLOAD: u8 = 1;
pub(crate) const PRESENCE: u8 = 2;
pub(crate) const LISTING: u8 = 3;

/// One input an analysis consumed. Paths are exact native paths; an entry
/// is the authorization the access went through, so a validator applies
/// the current read policy to the same provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation {
    /// The bytes read at `path`.
    Payload {
        path: PathBuf,
        entry: ProjectPathEntry,
        content: SourceContent,
    },
    /// Whether `path` existed when it was looked up; nothing was read.
    Presence {
        path: PathBuf,
        entry: Option<ProjectPathEntry>,
        present: bool,
    },
    /// The complete listing of the directory at `path` (`listing_digest`).
    Listing {
        path: PathBuf,
        entry: ProjectPathEntry,
        listing: Digest,
    },
}

impl Observation {
    pub fn kind(&self) -> u8 {
        match self {
            Self::Payload { .. } => PAYLOAD,
            Self::Presence { .. } => PRESENCE,
            Self::Listing { .. } => LISTING,
        }
    }

    pub fn path(&self) -> &Path {
        match self {
            Self::Payload { path, .. }
            | Self::Presence { path, .. }
            | Self::Listing { path, .. } => path,
        }
    }

    pub fn encoded(&self) -> Vec<u8> {
        let mut encoder = Encoder::new();
        encoder.encode(self);
        encoder.into_bytes()
    }
}

impl CanonicalEncode for Observation {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.variant(self.kind());
        match self {
            Self::Payload {
                path,
                entry,
                content,
            } => {
                encoder.field(1);
                encoder.encode(path.as_path());
                encoder.field(2);
                encoder.encode(entry);
                encoder.field(3);
                encoder.encode(content);
            }
            Self::Presence {
                path,
                entry,
                present,
            } => {
                encoder.field(1);
                encoder.encode(path.as_path());
                encoder.field(2);
                encoder.option(entry.as_ref(), |encoder, entry| encoder.encode(entry));
                encoder.field(3);
                encoder.bool(*present);
            }
            Self::Listing {
                path,
                entry,
                listing,
            } => {
                encoder.field(1);
                encoder.encode(path.as_path());
                encoder.field(2);
                encoder.encode(entry);
                encoder.field(3);
                encoder.digest(listing);
            }
        }
    }
}

/// The canonical encoding of `path`: the stored observation index key.
pub fn path_key(path: &Path) -> Vec<u8> {
    let mut encoder = Encoder::new();
    encoder.encode(path);
    encoder.into_bytes()
}

/// The digest of a complete directory listing: its entries' final names,
/// files and directories kept apart, sorted and deduplicated. The parent
/// and the order the platform returned them in do not matter.
pub fn listing_digest(files: &[PathBuf], directories: &[PathBuf]) -> Digest {
    fn names(paths: &[PathBuf]) -> Vec<Vec<u8>> {
        let mut names: Vec<Vec<u8>> = paths
            .iter()
            .map(|path| path_key(path.file_name().map_or(path.as_path(), Path::new)))
            .collect();
        names.sort();
        names.dedup();
        names
    }
    let mut encoder = Encoder::new();
    encoder.field(1);
    encoder.seq(names(files), |encoder, name| encoder.bytes(&name));
    encoder.field(2);
    encoder.seq(names(directories), |encoder, name| encoder.bytes(&name));
    Digest::derive(LISTING_DIGEST_CONTEXT, encoder.as_bytes())
}

/// The digest of an ordered observation list, as a dependent key holds it.
pub fn observations_digest(observations: &[Observation]) -> Digest {
    let mut encoder = Encoder::new();
    encoder.seq(observations, |encoder, observation| {
        encoder.encode(observation)
    });
    Digest::derive(OBSERVATIONS_DIGEST_CONTEXT, encoder.as_bytes())
}

/// Whose inputs an evidence set proves: one root under one complete context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EvidenceSubject {
    pub context: Digest,
    pub root: Digest,
}

/// The evidence ID: the digest of the evidence manifest (subject,
/// conclusion, then each observation's encoding), streamed so the manifest
/// itself never exists in one buffer.
#[derive(Clone)]
pub(crate) struct EvidenceHasher(blake3::Hasher);

impl EvidenceHasher {
    pub(crate) fn new(subject: &EvidenceSubject, conclusion: Digest, count: usize) -> Self {
        let mut encoder = manifest_encoder(KeyKind::Evidence);
        encoder.field(1);
        encoder.digest(&subject.context);
        encoder.field(2);
        encoder.digest(&subject.root);
        encoder.field(3);
        encoder.digest(&conclusion);
        encoder.field(4);
        encoder.length(count);
        let mut hasher = blake3::Hasher::new_derive_key(MANIFEST_DIGEST_CONTEXT);
        hasher.update(encoder.as_bytes());
        Self(hasher)
    }

    /// Add the next observation's encoding (`Observation::encoded`).
    pub(crate) fn update(&mut self, encoded: &[u8]) {
        self.0.update(encoded);
    }

    pub(crate) fn finish(&self) -> Digest {
        Digest::from_bytes(*self.0.finalize().as_bytes())
    }
}

pub fn evidence_id(
    subject: &EvidenceSubject,
    conclusion: Digest,
    observations: &[Observation],
) -> Digest {
    let mut hasher = EvidenceHasher::new(subject, conclusion, observations.len());
    for observation in observations {
        hasher.update(&observation.encoded());
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_streamed_id_is_the_digest_of_the_whole_manifest() {
        let subject = EvidenceSubject {
            context: Digest::from_bytes([1; 32]),
            root: Digest::from_bytes([2; 32]),
        };
        let observations = [Observation::Presence {
            path: PathBuf::from("x"),
            entry: None,
            present: false,
        }];
        let mut manifest = manifest_encoder(KeyKind::Evidence);
        manifest.field(1);
        manifest.digest(&subject.context);
        manifest.field(2);
        manifest.digest(&subject.root);
        manifest.field(3);
        manifest.digest(&Digest::from_bytes([3; 32]));
        manifest.field(4);
        manifest.seq(&observations, |encoder, observation| {
            encoder.encode(observation)
        });
        assert_eq!(
            evidence_id(&subject, Digest::from_bytes([3; 32]), &observations),
            Digest::derive(MANIFEST_DIGEST_CONTEXT, manifest.as_bytes())
        );
    }
}

use super::Store;
use super::limits::MAX_MANIFEST_BYTES;
use super::namespaces::{Scope, ensure_live, epoch};
use super::outcome::{Bypass, Integrity, Lookup, Miss, Published, TooLarge};
use super::sqlite::{begin, classify, classify_read};
use crate::{CanonicalKey, Digest, ExpandedArtifactKey, KeyRecord, LocalArtifactKey, Namespace};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

/// An artifact key and, for an expanded artifact, the local key of its
/// root, whose manifest is retained and admitted with it (spec §4).
#[derive(Debug, Clone, Copy)]
pub struct ArtifactKeyRef<'a> {
    key: &'a CanonicalKey,
    root: Option<&'a CanonicalKey>,
}

/// The local key does not have the digest the expanded key embeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootMismatch;

impl<'a> ArtifactKeyRef<'a> {
    pub fn local(key: &'a LocalArtifactKey) -> Self {
        Self {
            key: key.key(),
            root: None,
        }
    }

    pub fn expanded(
        key: &'a ExpandedArtifactKey,
        root: &'a LocalArtifactKey,
    ) -> Result<Self, RootMismatch> {
        if key.root_digest() != root.digest() {
            return Err(RootMismatch);
        }
        Ok(Self {
            key: key.key(),
            root: Some(root.key()),
        })
    }

    pub fn digest(&self) -> Digest {
        self.key.digest()
    }

    pub(crate) fn key(&self) -> &'a CanonicalKey {
        self.key
    }

    pub(crate) fn root(&self) -> Option<&'a CanonicalKey> {
        self.root
    }
}

/// Insert `key`'s manifest unless the identical manifest is stored.
pub(crate) fn put_manifest(
    connection: &Connection,
    scope: Scope,
    key: &CanonicalKey,
) -> Result<Published, Bypass> {
    let fail = |error: rusqlite::Error| classify(&error);
    let bytes = key.manifest().len();
    if bytes > MAX_MANIFEST_BYTES {
        return Err(Bypass::TooLarge(TooLarge::Manifest { bytes }));
    }
    let stored: Option<Vec<u8>> = connection
        .query_row(
            "SELECT manifest FROM key_manifests \
             WHERE namespace_id = ?1 AND reset_epoch = ?2 AND digest = ?3",
            params![
                scope.namespace.as_bytes().as_slice(),
                epoch(scope),
                key.digest().as_bytes().as_slice()
            ],
            |row| row.get(0),
        )
        .optional()
        .map_err(fail)?;
    match stored {
        Some(manifest) if manifest == key.manifest() => Ok(Published::AlreadyPresent),
        Some(_) => Err(Bypass::Integrity(Integrity::ConflictingManifest)),
        None => {
            connection
                .execute(
                    "INSERT INTO key_manifests (namespace_id, reset_epoch, digest, kind, manifest) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        scope.namespace.as_bytes().as_slice(),
                        epoch(scope),
                        key.digest().as_bytes().as_slice(),
                        i64::from(key.kind().tag()),
                        key.manifest()
                    ],
                )
                .map_err(fail)?;
            Ok(Published::Inserted)
        }
    }
}

/// Compare the retained manifest of `key` with `key` exactly.
pub(crate) fn admit_manifest(
    connection: &Connection,
    namespace: &Namespace,
    scope: Scope,
    key: &CanonicalKey,
) -> Result<Lookup<()>, Bypass> {
    let stored = connection
        .query_row(
            "SELECT manifest FROM key_manifests \
             WHERE namespace_id = ?1 AND reset_epoch = ?2 AND digest = ?3",
            params![
                scope.namespace.as_bytes().as_slice(),
                epoch(scope),
                key.digest().as_bytes().as_slice()
            ],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional();
    let manifest = match stored {
        Ok(Some(manifest)) => manifest,
        Ok(None) => return Ok(Lookup::Miss(Miss::Absent)),
        Err(error) => return classify_read(&error).map(Lookup::Miss),
    };
    let record = KeyRecord {
        namespace: scope.namespace,
        digest: key.digest(),
        manifest: &manifest,
    };
    Ok(match key.admit(namespace, &record) {
        Ok(()) => Lookup::Hit(()),
        Err(mismatch) => Lookup::Miss(Miss::KeyMismatch(mismatch)),
    })
}

impl Store {
    /// Retain `key`'s manifest, for example a context key's.
    pub fn publish_key(&self, key: &CanonicalKey) -> Result<Published, Bypass> {
        let mut writer = self.writer()?;
        let transaction = begin(&mut writer, TransactionBehavior::Immediate)?;
        ensure_live(&transaction, self.scope)?;
        let published = put_manifest(&transaction, self.scope, key)?;
        self.before_commit();
        transaction.commit().map_err(|error| classify(&error))?;
        Ok(published)
    }

    pub fn admit_key(&self, key: &CanonicalKey) -> Result<Lookup<()>, Bypass> {
        let mut reader = self.reader()?;
        let transaction = begin(&mut reader, TransactionBehavior::Deferred)?;
        ensure_live(&transaction, self.scope)?;
        admit_manifest(&transaction, &self.namespace, self.scope, key)
    }
}

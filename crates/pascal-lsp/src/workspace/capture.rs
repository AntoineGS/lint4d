//! What one analysis read from disk, with durable digests, as the source
//! store saw it (spec §7). `workspace::evidence` builds evidence from it.
#![cfg_attr(not(test), allow(dead_code))]

use pascal_cache::{Digest, SourceContent, listing_digest};
use pascal_core::resolver::{DirectoryListing, LoadedSource, SourceRevision};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

/// Captures past this mark make the capture incomplete instead of growing.
pub(crate) const MAX_CAPTURED_READS: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapturedListing {
    pub(crate) directory: PathBuf,
    /// `listing_digest` of the disk listing, before editor buffers joined it.
    pub(crate) listing: Digest,
    pub(crate) complete: bool,
    /// Editor buffers added entries the disk does not have.
    pub(crate) overlay: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapturedPayload {
    pub(crate) path: PathBuf,
    /// The revision's session hash, which names this read in observations.
    pub(crate) content_hash: u64,
    /// The durable digest of the raw bytes read (spec §4).
    pub(crate) content: SourceContent,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CapturedReads {
    pub(crate) listings: Vec<CapturedListing>,
    pub(crate) payloads: Vec<CapturedPayload>,
    /// Reads past `MAX_CAPTURED_READS` were not recorded.
    pub(crate) overflowed: bool,
}

pub(crate) enum ListingCapture<'a> {
    Absent,
    One(&'a CapturedListing),
    /// The session listed the directory more than once and saw it change.
    Conflicting,
}

impl CapturedReads {
    pub(crate) fn listing_of(&self, directory: &Path) -> ListingCapture<'_> {
        let mut found = None;
        for listing in self
            .listings
            .iter()
            .filter(|listing| super::resolver::path_equivalent(&listing.directory, directory))
        {
            match found {
                None => found = Some(listing),
                Some(first) if first == listing => {}
                Some(_) => return ListingCapture::Conflicting,
            }
        }
        found.map_or(ListingCapture::Absent, ListingCapture::One)
    }

    pub(crate) fn payload_of(&self, path: &Path, content_hash: u64) -> Option<&SourceContent> {
        self.payloads
            .iter()
            .find(|payload| {
                payload.content_hash == content_hash
                    && super::resolver::path_equivalent(&payload.path, path)
            })
            .map(|payload| &payload.content)
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        let listings = self.listings.iter().fold(
            std::mem::size_of_val(self.listings.as_slice()),
            |total, listing| total.saturating_add(listing.directory.as_os_str().len()),
        );
        self.payloads.iter().fold(
            listings.saturating_add(std::mem::size_of_val(self.payloads.as_slice())),
            |total, payload| total.saturating_add(payload.path.as_os_str().len()),
        )
    }
}

/// Shared by every clone of the source store a session uses.
#[derive(Debug, Default)]
pub(crate) struct ReadCapture(Mutex<CapturedReads>);

impl ReadCapture {
    /// The store listed `directory`; `disk` is what the disk returned.
    pub(crate) fn listed(&self, directory: &Path, disk: &DirectoryListing, overlay: bool) {
        let listing = CapturedListing {
            directory: directory.to_path_buf(),
            listing: listing_digest(&disk.files, &disk.directories),
            complete: disk.complete,
            overlay,
        };
        self.push(|reads| reads.listings.push(listing));
    }

    /// The store read `source`. Buffers are not disk reads.
    pub(crate) fn loaded(&self, source: &LoadedSource) {
        let SourceRevision::Disk { content_hash, .. } = &source.revision else {
            return;
        };
        let payload = CapturedPayload {
            path: source.path.clone(),
            content_hash: *content_hash,
            content: SourceContent::from_bytes(&source.bytes),
        };
        self.push(|reads| reads.payloads.push(payload));
    }

    fn push(&self, record: impl FnOnce(&mut CapturedReads)) {
        let mut reads = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if reads.listings.len() + reads.payloads.len() >= MAX_CAPTURED_READS {
            reads.overflowed = true;
            return;
        }
        record(&mut reads);
    }

    pub(crate) fn snapshot(&self) -> CapturedReads {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::facts::tests::{index, index_with_buffers};
    use pascal_core::resolver::ResolutionObservation;

    const DPROJ: &[u8] = b"<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_IncludePath>include</DCC_IncludePath></PropertyGroup></Project>";
    const MAIN: &[u8] =
        b"unit Main;\ninterface\n{$I Shared.inc}\n{$I Missing.inc}\nimplementation\nend.\n";
    const SHARED: &[u8] = b"const Shared = 1;\n";

    fn disk_listing(directory: &Path) -> Digest {
        let (mut files, mut directories) = (Vec::new(), Vec::new());
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                directories.push(entry.path());
            } else {
                files.push(entry.path());
            }
        }
        listing_digest(&files, &directories)
    }

    #[test]
    fn a_session_captures_every_listing_and_payload_it_observed() {
        let indexed = index(
            &[
                ("App.dproj", DPROJ),
                ("Main.pas", MAIN),
                ("include/Shared.inc", SHARED),
            ],
            &["Main.pas"],
        );
        let expanded = &indexed.workspace.expansions[&indexed.uri("Main.pas")].expanded;
        let reads = expanded
            .reads
            .as_ref()
            .expect("a session captures its reads");
        let session = expanded.session.as_ref().unwrap();
        assert!(!reads.overflowed);
        assert!(
            reads
                .payloads
                .iter()
                .any(|payload| payload.path.ends_with("include/Shared.inc")
                    && payload.content == SourceContent::from_bytes(SHARED)),
            "{reads:?}"
        );
        let mut listed = 0;
        for observation in &session.observations {
            match observation {
                ResolutionObservation::Directory {
                    path,
                    complete: true,
                    ..
                } => {
                    let ListingCapture::One(captured) = reads.listing_of(path) else {
                        panic!("{path:?} was listed but not captured: {reads:?}")
                    };
                    assert!(captured.complete && !captured.overlay);
                    assert_eq!(captured.listing, disk_listing(path), "{path:?}");
                    listed += 1;
                }
                ResolutionObservation::Payload {
                    path,
                    revision: pascal_core::SourceRevision::Disk { content_hash, .. },
                    ..
                } => {
                    let content = reads
                        .payload_of(path, *content_hash)
                        .unwrap_or_else(|| panic!("{path:?} was read but not captured"));
                    assert_eq!(
                        *content,
                        SourceContent::from_bytes(&std::fs::read(path).unwrap())
                    );
                }
                _ => {}
            }
        }
        assert!(
            listed > 0,
            "the missing include was looked for in a listing"
        );
    }

    #[test]
    fn a_buffer_the_disk_lacks_marks_its_directory_listing() {
        let indexed = index_with_buffers(
            &[
                ("App.dproj", DPROJ),
                ("Main.pas", MAIN),
                ("include/Shared.inc", SHARED),
            ],
            &[("include/Extra.inc", "const Extra = 1;\n")],
            &["Main.pas"],
        );
        let reads = indexed.workspace.expansions[&indexed.uri("Main.pas")]
            .expanded
            .reads
            .clone()
            .unwrap();
        let include = indexed.root.join("include");
        assert!(
            reads
                .listings
                .iter()
                .any(|listing| listing.directory.ends_with("include") && listing.overlay),
            "{reads:?}"
        );
        // The digest is the disk's own listing, without the buffer.
        let captured = reads
            .listings
            .iter()
            .find(|listing| listing.directory.ends_with("include"))
            .unwrap();
        assert_eq!(captured.listing, disk_listing(&include));
    }

    #[test]
    fn captures_agree_or_conflict_and_stop_at_their_bound() {
        let capture = ReadCapture::default();
        let directory = Path::new("/w/include");
        let listing = |files: Vec<PathBuf>| DirectoryListing {
            files,
            directories: Vec::new(),
            stamp: None,
            complete: true,
        };
        assert!(matches!(
            capture.snapshot().listing_of(directory),
            ListingCapture::Absent
        ));
        capture.listed(directory, &listing(vec![directory.join("a.inc")]), false);
        capture.listed(directory, &listing(vec![directory.join("a.inc")]), false);
        assert!(matches!(
            capture.snapshot().listing_of(directory),
            ListingCapture::One(_)
        ));
        capture.listed(directory, &listing(vec![directory.join("b.inc")]), false);
        assert!(matches!(
            capture.snapshot().listing_of(directory),
            ListingCapture::Conflicting
        ));
        for _ in 0..MAX_CAPTURED_READS {
            capture.listed(Path::new("/w/other"), &listing(Vec::new()), false);
        }
        let reads = capture.snapshot();
        assert!(reads.overflowed);
        assert_eq!(
            reads.listings.len() + reads.payloads.len(),
            MAX_CAPTURED_READS
        );
    }
}

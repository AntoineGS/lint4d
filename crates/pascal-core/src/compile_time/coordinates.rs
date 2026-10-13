//! Coordinate validation boundary shared by header and source-order sessions.
use super::{AdmittedOwned, CompileTimeBudget};
use crate::{CancellationToken, LoadedSource, ResolverError};
use std::sync::Arc;

#[cfg(test)]
thread_local! {
    static VALIDATION_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn validate(bytes: &[u8]) -> Result<&str, std::str::Utf8Error> {
    #[cfg(test)]
    VALIDATION_BYTES.with(|count| count.set(count.get() + bytes.len()));
    std::str::from_utf8(bytes)
}

pub(crate) fn text(
    source: &LoadedSource,
    budget: &mut CompileTimeBudget<'_>,
    cancel: &dyn CancellationToken,
) -> Result<AdmittedOwned<Arc<str>>, ResolverError> {
    if cancel.is_cancelled() {
        return Err(ResolverError::Cancelled);
    }
    if let Some(text) = &source.decoded_text {
        // Arc<str> is already a UTF-8 proof. Clone the handle, not the backing,
        // and never send its body through a validator or byte-work admission.
        return budget
            .admit_owned(1, 0, 0, || text.clone())
            .map_err(|r| super::session::resolver_error(r, budget));
    }
    // A raw-only source has no coordinate proof. Validate once in bounded
    // chunks; Utf8Error (not a second UTF-8 parser) handles a split code point.
    // Every actual validation and copy is admitted before it occurs. Session
    // feeds reuse this owner; no unvalidated &[u8] becomes a &str.
    let bytes = source.bytes.as_ref();
    let mut text = budget
        .admit_owned(1, 0, bytes.len(), || String::with_capacity(bytes.len()))
        .map_err(|r| super::session::resolver_error(r, budget))?;
    let mut cursor = 0;
    while cursor < bytes.len() {
        let initial_end = cursor.saturating_add(4096).min(bytes.len());
        let mut end = initial_end;
        let chunk = loop {
            if cancel.is_cancelled() {
                return Err(ResolverError::Cancelled);
            }
            budget
                .charge(end - cursor, end - cursor, 0)
                .map_err(|r| super::session::resolver_error(r, budget))?;
            match validate(&bytes[cursor..end]) {
                Ok(chunk) => break chunk,
                Err(error)
                    if error.error_len().is_none()
                        && end < bytes.len()
                        && end - initial_end < 3 =>
                {
                    end += 1;
                }
                Err(_) => {
                    return Err(ResolverError::InvalidRequest(
                        "coordinate text is not UTF-8".into(),
                    ));
                }
            }
        };
        text.grow(budget, chunk.len(), chunk.len(), 0, |text| {
            text.push_str(chunk)
        })
        .map_err(|r| super::session::resolver_error(r, budget))?;
        cursor = end;
    }
    let arc_bytes = bytes
        .len()
        .checked_add(2 * std::mem::size_of::<usize>())
        .ok_or_else(|| ResolverError::InvalidRequest("coordinate backing overflow".into()))?;
    if cancel.is_cancelled() {
        return Err(ResolverError::Cancelled);
    }
    text.grow(budget, bytes.len(), bytes.len(), arc_bytes, |_| ())
        .map_err(|r| super::session::resolver_error(r, budget))?;
    let mut text = text
        .map_value(budget, Arc::<str>::from)
        .map_err(|r| super::session::resolver_error(r, budget))?;
    // Only the named, pre-admitted String backing died during conversion.
    text.retire_storage(bytes.len())
        .map_err(|r| super::session::resolver_error(r, budget))?;
    Ok(text)
}

#[cfg(test)]
pub(super) fn validation_bytes() -> usize {
    VALIDATION_BYTES.with(std::cell::Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile_time::*;
    use crate::{
        AdmittedCapabilityOutcome, AdmittedCapabilityReports, CancellationToken,
        IncludeResolveRequest, NoCancellation, Resolution, ResolutionOutcome, SourceId,
        SourceRevision, UnitResolveRequest,
    };
    use pascal_project::ConditionalContext;

    fn source(name: &str, text: &str) -> LoadedSource {
        LoadedSource {
            id: SourceId::new(name),
            path: name.into(),
            bytes: text.as_bytes().into(),
            decoded_text: Some(text.into()),
            compile_time_backing: None,
            revision: SourceRevision::Overlay {
                version: 1,
                content_hash: pascal_project::content_hash_bytes(text.as_bytes()),
            },
        }
    }

    struct Resources {
        child: LoadedSource,
    }
    impl CompileTimeResolver for Resources {
        fn include_capability(
            &mut self,
            _: IncludeResolveRequest<'_>,
            budget: &mut CompileTimeBudget<'_>,
            _: &dyn CancellationToken,
        ) -> Result<AdmittedCapabilityOutcome<LoadedSource>, ResolverError> {
            let metadata = crate::resolver::loaded_source_metadata_retained(&self.child);
            budget
                .charge(1, metadata, metadata)
                .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
            AdmittedCapabilityOutcome::copy_bounded_result(
                Resolution::Found(self.child.clone()),
                AdmittedCapabilityReports::copy(&[], &[], budget)?,
                budget,
            )
        }
        fn include(
            &mut self,
            _: IncludeResolveRequest<'_>,
            _: &mut CompileTimeBudget<'_>,
            _: &dyn CancellationToken,
        ) -> Result<ResolutionOutcome<LoadedSource>, ResolverError> {
            panic!("legacy include")
        }
        fn unit(
            &mut self,
            _: UnitResolveRequest<'_>,
            _: &mut CompileTimeBudget<'_>,
            _: &dyn CancellationToken,
        ) -> Result<ResolutionOutcome<ProviderSource>, ResolverError> {
            panic!("provider body")
        }
    }

    #[test]
    fn fixround1_decoded_coordinate_proof_is_reused_for_root_children_and_feeds() {
        let root = source(
            "root.pas",
            &format!(
                "{{$HPPEMIT 'one'}} {{$HPPEMIT 'two'}} {{$I header.inc}} {}",
                "body ".repeat(200_000)
            ),
        );
        let child = source(
            "header.inc",
            &format!(
                "{{$HPPEMIT 'three'}} {{$HPPEMIT 'four'}} unit Main; {}",
                "body ".repeat(200_000)
            ),
        );
        let before = validation_bytes();
        let mut resources = Resources { child };
        let h = verify_unit_header(
            &root,
            &ConditionalContext::default(),
            &mut resources,
            CompileTimeLimits::default(),
            &NoCancellation,
        )
        .unwrap()
        .known()
        .unwrap();
        assert_eq!(h.name, "Main");
        assert_eq!(h.declaration.source.as_str(), "header.inc");
        assert_eq!(
            validation_bytes(),
            before,
            "already validated Arc<str> must never enter UTF-8 validation, including each physical feed"
        );
    }

    #[test]
    fn fixround1_raw_coordinate_proof_is_validated_once_and_reused_across_directives() {
        let text = format!(
            "{{{}é}} {{$HPPEMIT 'one'}} {{$HPPEMIT 'two'}} unit Main;",
            "a".repeat(4094)
        );
        let mut root = source("root.pas", &text);
        root.decoded_text = None;
        let before = validation_bytes();
        let mut resources = Resources {
            child: source("unused.inc", ""),
        };
        let h = verify_unit_header(
            &root,
            &ConditionalContext::default(),
            &mut resources,
            CompileTimeLimits::default(),
            &NoCancellation,
        )
        .unwrap()
        .known()
        .unwrap();
        assert_eq!(&text[h.declaration.range.clone()], "unit Main;");
        assert_eq!(&text[h.name_parts[0].range.clone()], "Main");
        // The split é causes one admitted retry of the first 4096-byte chunk.
        // Plain probe, session initialization and every feed share this proof.
        assert_eq!(validation_bytes() - before, text.len() + 4096);
    }

    #[test]
    fn fixround1_raw_validation_admission_and_cancel_precede_traversal() {
        let mut root = source("raw.pas", &" ".repeat(50_000));
        root.decoded_text = None;
        let mut budget = CompileTimeBudget::new(
            CompileTimeLimits {
                max_byte_work: 128,
                ..CompileTimeLimits::default()
            },
            &NoCancellation,
        );
        let before = validation_bytes();
        assert!(matches!(
            text(&root, &mut budget, &NoCancellation),
            Err(ResolverError::LimitExceeded { .. })
        ));
        assert_eq!(
            validation_bytes(),
            before,
            "refuse before the first unadmitted validator call"
        );

        struct Stop(std::cell::Cell<usize>);
        impl CancellationToken for Stop {
            fn is_cancelled(&self) -> bool {
                let n = self.0.get() + 1;
                self.0.set(n);
                n >= 3
            }
        }
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let observer = budget
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut budget)
            .unwrap();
        let baseline = budget.snapshot().retained_bytes;
        let before = validation_bytes();
        assert!(matches!(
            text(&root, &mut budget, &Stop(0.into())),
            Err(ResolverError::Cancelled)
        ));
        assert_eq!(
            validation_bytes() - before,
            4096,
            "cancel between bounded validation calls"
        );
        assert!(budget.snapshot().work >= 8192 && budget.snapshot().byte_work >= 8192);
        assert_eq!(
            budget.snapshot().retained_bytes,
            baseline,
            "only dead raw-view storage refunds"
        );
        drop(observer);
        let mut invalid = source("invalid.pas", "unit Main;");
        invalid.decoded_text = None;
        invalid.bytes = b"unit \xff;".as_slice().into();
        assert!(matches!(
            text(&invalid, &mut budget, &NoCancellation),
            Err(ResolverError::InvalidRequest(_))
        ));
    }
}

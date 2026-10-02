//! Cache-only walk of a snapshot's interface closure.

use super::rename::{CANCELLATION_MESSAGE, OverlayInput, path_record_at};
use super::{Workspace, conditional_context_for_uri};
use crate::project_cache::InterfaceBinding;
use lsp_types::Url;
use pascal_project::ProjectContext;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};

/// The units a closure walk inserted into a snapshot index.
#[derive(Debug, Default)]
pub(super) struct ClosureWalk {
    pub(super) providers: HashSet<Url>,
    /// Whether part of the closure was not cached.
    pub(super) incomplete: bool,
    /// Units whose interface or unit entry was not cached.
    pub(super) missed: HashSet<Url>,
}

enum Insertion {
    Inserted,
    Missing,
    Skipped,
}

/// Inserts the cached interface closure of `dependencies` into the loader's
/// index as declaration providers and binds their interface imports, so
/// members inherited through a dependency's own imports resolve. Only verified
/// cache entries are used: nothing is resolved, read, or parsed. A missing
/// entry leaves its unit fenced and marks the walk incomplete.
///
/// `owned` are the snapshot's own indexed sources. The snapshot already bound
/// (or deliberately emptied) their imports, so the walk never roots at, binds,
/// or fences them: its interface-only bindings would replace the full ones.
pub(super) fn walk_interface_closure(
    loader: &mut Workspace,
    dependencies: &[Url],
    owned: &HashSet<Url>,
    cancel: &AtomicBool,
) -> Result<ClosureWalk, String> {
    let overlays = loader.overlay_inputs();
    let mut walk = ClosureWalk::default();
    let mut queue = VecDeque::new();
    let mut seen = HashSet::new();
    for uri in dependencies {
        if owned.contains(uri) {
            continue;
        }
        let (Some(hash), Some(context_key)) = (
            loader.indexed_content_hashes.get(uri).copied(),
            loader.document_contexts.get(uri).cloned(),
        ) else {
            continue;
        };
        if seen.insert(uri.clone()) {
            queue.push_back((uri.clone(), hash, context_key));
        }
    }
    let mut processed = HashSet::new();
    let mut work = 0usize;
    while let Some((uri, hash, context_key)) = queue.pop_front() {
        if cancel.load(Ordering::Relaxed) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        if work >= super::MAX_DEPENDENCY_WORK {
            break;
        }
        work += 1;
        processed.insert(uri.clone());
        let Some(context) = loader
            .contexts
            .get(&context_key)
            .map(|state| state.context.clone())
        else {
            walk.incomplete = true;
            walk.missed.insert(uri.clone());
            loader.index.clear_import_bindings(&uri);
            continue;
        };
        let Some(interface) = loader
            .project_cache
            .peek_interface_imports(&uri, &context, hash, &overlays)
        else {
            walk.incomplete = true;
            walk.missed.insert(uri.clone());
            loader.index.clear_import_bindings(&uri);
            continue;
        };
        let mut bindings = HashMap::new();
        for binding in &interface.bindings {
            if loader.index.contains(&binding.uri) {
                bindings.insert(binding.name.clone(), binding.uri.clone());
                continue;
            }
            match insert_provider(loader, binding, &context, &overlays) {
                Insertion::Inserted => {
                    walk.providers.insert(binding.uri.clone());
                    bindings.insert(binding.name.clone(), binding.uri.clone());
                    if interface.complete && seen.insert(binding.uri.clone()) {
                        queue.push_back((
                            binding.uri.clone(),
                            binding.content_hash(),
                            context_key.clone(),
                        ));
                    }
                }
                Insertion::Missing => {
                    walk.incomplete = true;
                    walk.missed.insert(binding.uri.clone());
                }
                Insertion::Skipped => {}
            }
        }
        loader.index.bind_imports(&uri, bindings);
    }
    // Providers that were not walked keep a fence instead of unbound imports.
    for provider in &walk.providers {
        if !processed.contains(provider) {
            loader.index.clear_import_bindings(provider);
        }
    }
    Ok(walk)
}

/// Inserts the cached parse of `binding`'s unit and records it for stale-result
/// revalidation. Only disk revisions are walked: an open document's text is
/// owned by its overlay.
fn insert_provider(
    loader: &mut Workspace,
    binding: &InterfaceBinding,
    context: &ProjectContext,
    overlays: &HashMap<Url, OverlayInput>,
) -> Insertion {
    let pascal_core::SourceRevision::Disk {
        stamp,
        content_hash,
        read_policy,
        path_entry,
    } = &binding.revision
    else {
        return Insertion::Skipped;
    };
    if overlays.contains_key(&binding.uri) {
        return Insertion::Skipped;
    }
    let Ok(path) = binding.uri.to_file_path() else {
        return Insertion::Skipped;
    };
    let Some(unit) = loader
        .project_cache
        .peek_unit(&binding.uri, context, *content_hash, overlays)
    else {
        return Insertion::Missing;
    };
    let conditional = conditional_context_for_uri(context, &binding.uri);
    if !loader
        .index
        .insert_parsed(binding.uri.clone(), unit.parsed.clone(), &conditional)
    {
        return Insertion::Skipped;
    }
    if let Some(record) = path_record_at(
        path,
        Some(stamp.clone()),
        Some(*content_hash),
        None,
        None,
        Some(read_policy.clone()),
        Some(path_entry.clone()),
        false,
    ) {
        let records = loader.analysis_records.get_or_insert_with(HashMap::new);
        super::resolver::merge_source_record(records, record);
    }
    Insertion::Inserted
}

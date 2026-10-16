use super::discovery_cache::{
    PathResolutionDirectoryEntries, PathResolutionDirectoryListing, path_resolution_cancelled,
    path_resolution_directory_listing,
};
#[cfg(test)]
use super::discovery_cache::{
    TEST_USE_LEGACY_PATH_RESOLUTION, read_dir_for_path_resolution,
    with_path_resolution_read_dir_count,
};
use super::selection::filesystem_identity_unverified;
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ProjectPathProvenance {
    LegacyNative,
    Configured,
    Mapped { root: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProjectPathEntry {
    pub path: PathBuf,
    pub provenance: ProjectPathProvenance,
}

#[derive(Debug, Clone)]
pub(super) struct ProvenanceRange {
    pub(super) range: Range<usize>,
    pub(super) provenance: ProjectPathProvenance,
}

impl ProjectPathEntry {
    pub fn visit_recovery_payload(
        &self,
        visit: &mut dyn FnMut(usize) -> Result<(), String>,
    ) -> Result<(), String> {
        visit(self.path.as_os_str().len())?;
        if let ProjectPathProvenance::Mapped { root } = &self.provenance {
            visit(root.as_os_str().len())?;
        }
        Ok(())
    }

    pub fn legacy(path: PathBuf) -> Self {
        Self {
            path,
            provenance: ProjectPathProvenance::LegacyNative,
        }
    }

    pub fn configured(path: PathBuf) -> Self {
        Self {
            path,
            provenance: ProjectPathProvenance::Configured,
        }
    }

    pub(super) fn resolved(path: PathBuf, resolved: &ResolvedPath, configured: bool) -> Self {
        let provenance = resolved.mapping.as_ref().map_or_else(
            || {
                if configured {
                    ProjectPathProvenance::Configured
                } else {
                    ProjectPathProvenance::LegacyNative
                }
            },
            |mapping| ProjectPathProvenance::Mapped {
                root: resolved_mapping_root(mapping),
            },
        );
        Self { path, provenance }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AuthorizedReadRoot {
    path: PathBuf,
    pattern_exclusion_bases: Vec<PathBuf>,
}

/// Immutable, requester-scoped authorization for project metadata and source
/// payloads.  The policy deliberately keeps configured native roots separate
/// from mapping provenance: a mapped entry may only use its selected mapping
/// root, while a configured native entry may use one of the requester's
/// workspace, source-path, or effective mapping destinations.
#[derive(Debug, Clone, Default)]
pub struct ReadPolicy {
    configured_roots: Vec<AuthorizedReadRoot>,
    mapped_roots: Vec<AuthorizedReadRoot>,
    authorized_files: Arc<Vec<PathBuf>>,
    pub(super) exclusions: Vec<String>,
    exclusion_bases: Vec<PathBuf>,
    compiled_exclusions: Arc<Option<CompiledExclusions>>,
}

/// Exclusion globs as written, plus a letter-case-folding variant compiled
/// the first time a path on a case-insensitive volume is checked.
#[derive(Debug)]
struct CompiledExclusions {
    patterns: Vec<String>,
    exact: globset::GlobSet,
    case_insensitive: std::sync::OnceLock<Option<globset::GlobSet>>,
}

impl CompiledExclusions {
    fn new(patterns: &[String]) -> Option<Self> {
        Some(Self {
            exact: compile_exclude_patterns(patterns, false)?,
            patterns: patterns.to_vec(),
            case_insensitive: std::sync::OnceLock::new(),
        })
    }

    fn for_case(&self, case_insensitive: bool) -> Option<&globset::GlobSet> {
        if case_insensitive {
            self.case_insensitive
                .get_or_init(|| compile_exclude_patterns(&self.patterns, true))
                .as_ref()
        } else {
            Some(&self.exact)
        }
    }
}

impl PartialEq for ReadPolicy {
    fn eq(&self, other: &Self) -> bool {
        self.configured_roots == other.configured_roots
            && self.mapped_roots == other.mapped_roots
            && self.authorized_files == other.authorized_files
            && self.exclusions == other.exclusions
            && self.exclusion_bases == other.exclusion_bases
    }
}

impl Eq for ReadPolicy {}

/// Everything [`ReadPolicy`] equality compares, borrowed for canonical
/// encodings. The compiled exclusion matcher is derived from `exclusions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadPolicyView<'a> {
    pub configured_roots: Vec<ReadRootView<'a>>,
    pub mapped_roots: Vec<ReadRootView<'a>>,
    pub authorized_files: &'a [PathBuf],
    pub exclusions: &'a [String],
    pub exclusion_bases: &'a [PathBuf],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadRootView<'a> {
    pub path: &'a Path,
    pub pattern_exclusion_bases: &'a [PathBuf],
}

fn root_views(roots: &[AuthorizedReadRoot]) -> Vec<ReadRootView<'_>> {
    roots
        .iter()
        .map(|root| {
            let AuthorizedReadRoot {
                path,
                pattern_exclusion_bases,
            } = root;
            ReadRootView {
                path,
                pattern_exclusion_bases,
            }
        })
        .collect()
}

impl ReadPolicy {
    pub fn view(&self) -> ReadPolicyView<'_> {
        let Self {
            configured_roots,
            mapped_roots,
            authorized_files,
            exclusions,
            exclusion_bases,
            compiled_exclusions: _,
        } = self;
        ReadPolicyView {
            configured_roots: root_views(configured_roots),
            mapped_roots: root_views(mapped_roots),
            authorized_files: authorized_files.as_slice(),
            exclusions,
            exclusion_bases,
        }
    }
}

impl ReadPolicy {
    /// Owned storage copied by `Clone`. The authorization-file table and the
    /// compiled matcher are shared Arc backing, not copied parser/cache payload.
    /// Recovery traversal remains separate and still refuses opaque matcher data.
    pub fn visit_owned_clone_payload(
        &self,
        visit: &mut dyn FnMut(usize) -> Result<(), String>,
    ) -> Result<(), String> {
        self.visit_owned_clone_payload_with_preflight(&mut |_, bytes| match bytes {
            Some(bytes) => visit(bytes),
            None => Ok(()),
        })
    }

    /// Read-only clone sizing with work events before each input-sized group.
    /// `None` requests admission of `work` visits; `Some(bytes)` is a payload
    /// sizing event already covered by that group's preflight. Shared matcher/
    /// authorization-file backing remains excluded exactly as in the plain visitor.
    pub fn visit_owned_clone_payload_with_preflight(
        &self,
        visit: &mut dyn FnMut(usize, Option<usize>) -> Result<(), String>,
    ) -> Result<(), String> {
        let overflow = || "read policy clone sizing overflow".to_string();
        let roots = self
            .configured_roots
            .len()
            .checked_add(self.mapped_roots.len())
            .ok_or_else(overflow)?;
        let work = roots
            .checked_mul(2)
            .and_then(|n| n.checked_add(self.exclusions.len()))
            .and_then(|n| n.checked_add(self.exclusion_bases.len()))
            .and_then(|n| n.checked_add(3))
            .ok_or_else(overflow)?;
        visit(work, None)?;
        visit(
            0,
            Some(
                roots
                    .checked_mul(std::mem::size_of::<AuthorizedReadRoot>())
                    .ok_or_else(overflow)?,
            ),
        )?;
        for root in self.configured_roots.iter().chain(&self.mapped_roots) {
            visit(root.pattern_exclusion_bases.len(), None)?;
            visit(0, Some(root.path.as_os_str().len()))?;
            visit(
                0,
                Some(
                    root.pattern_exclusion_bases
                        .len()
                        .checked_mul(std::mem::size_of::<PathBuf>())
                        .ok_or_else(overflow)?,
                ),
            )?;
            for base in &root.pattern_exclusion_bases {
                visit(0, Some(base.as_os_str().len()))?;
            }
        }
        visit(
            0,
            Some(
                self.exclusions
                    .len()
                    .checked_mul(std::mem::size_of::<String>())
                    .ok_or_else(overflow)?,
            ),
        )?;
        for pattern in &self.exclusions {
            visit(0, Some(pattern.len()))?;
        }
        visit(
            0,
            Some(
                self.exclusion_bases
                    .len()
                    .checked_mul(std::mem::size_of::<PathBuf>())
                    .ok_or_else(overflow)?,
            ),
        )?;
        for base in &self.exclusion_bases {
            visit(0, Some(base.as_os_str().len()))?;
        }
        Ok(())
    }

    pub fn visit_recovery_payload(
        &self,
        visit: &mut dyn FnMut(usize) -> Result<(), String>,
    ) -> Result<(), String> {
        for root in self.configured_roots.iter().chain(&self.mapped_roots) {
            visit(root.path.as_os_str().len())?;
            for base in &root.pattern_exclusion_bases {
                visit(base.as_os_str().len())?;
            }
        }
        for path in self.authorized_files.iter() {
            visit(path.as_os_str().len())?;
        }
        for pattern in &self.exclusions {
            visit(pattern.len())?;
        }
        for base in &self.exclusion_bases {
            visit(base.as_os_str().len())?;
        }
        // globset does not expose compiled trie/DFA storage. Refuse recovery
        // rather than pretend a single marker bounds an opaque compiled set.
        if self.compiled_exclusions.as_ref().is_some() {
            return Err("compiled project exclusions cannot be bounded for recovery".into());
        }
        Ok(())
    }
}

impl Hash for ReadPolicy {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.configured_roots.hash(state);
        self.mapped_roots.hash(state);
        self.authorized_files.hash(state);
        self.exclusions.hash(state);
        self.exclusion_bases.hash(state);
    }
}

impl ReadPolicy {
    pub fn new(
        roots: &[PathBuf],
        source_paths: &[String],
        exclusions: &[String],
        overrides: &EffectiveOverrides,
    ) -> Self {
        Self::new_with_installation_roots(roots, source_paths, exclusions, overrides, &[])
    }

    pub fn new_with_installation_roots(
        roots: &[PathBuf],
        source_paths: &[String],
        exclusions: &[String],
        overrides: &EffectiveOverrides,
        installation_roots: &[PathBuf],
    ) -> Self {
        let mut configured_roots = Vec::new();
        for root in roots {
            let root = absolute_lexical(root).unwrap_or_else(|_| root.to_path_buf());
            add_unique_path(&mut configured_roots, root.clone());
            for source in source_paths {
                let source = PathBuf::from(source);
                let source = if source.is_absolute() {
                    source
                } else if is_foreign_windows_path(&source) {
                    continue;
                } else {
                    root.join(source)
                };
                let source = absolute_lexical(&source).unwrap_or(source);
                add_unique_path(&mut configured_roots, source);
            }
        }
        for source in source_paths {
            let source = PathBuf::from(source);
            if source.is_absolute() {
                let source = absolute_lexical(&source).unwrap_or(source);
                add_unique_path(&mut configured_roots, source);
            }
        }
        for root in installation_roots {
            let root = absolute_lexical(root).unwrap_or_else(|_| root.to_path_buf());
            add_unique_path(&mut configured_roots, root);
        }

        let mut mapped_roots = Vec::new();
        for mapping in &overrides.path_mappings {
            add_unique_path(&mut mapped_roots, resolved_mapping_root(mapping));
        }

        let mut exclusion_bases = configured_roots.clone();
        exclusion_bases.extend(mapped_roots.iter().cloned());
        let mut unique_exclusion_bases = Vec::new();
        for base in exclusion_bases {
            add_unique_path(&mut unique_exclusion_bases, base);
        }
        let configured_roots = configured_roots
            .into_iter()
            .map(|path| AuthorizedReadRoot {
                path,
                pattern_exclusion_bases: unique_exclusion_bases.clone(),
            })
            .collect();
        let mapped_roots = mapped_roots
            .into_iter()
            .map(|path| AuthorizedReadRoot {
                path,
                pattern_exclusion_bases: unique_exclusion_bases.clone(),
            })
            .collect();

        Self {
            configured_roots,
            mapped_roots,
            authorized_files: Arc::new(Vec::new()),
            exclusions: exclusions.to_vec(),
            exclusion_bases: unique_exclusion_bases,
            compiled_exclusions: Arc::new(CompiledExclusions::new(exclusions)),
        }
    }

    /// Add exact metadata files explicitly named by the selected installation.
    /// Unlike a root, each path authorizes no siblings or descendants.
    pub(crate) fn with_authorized_files(mut self, files: &[PathBuf]) -> Self {
        for path in files {
            let path = absolute_lexical(path).unwrap_or_else(|_| path.clone());
            if !self.authorized_files.contains(&path) {
                Arc::make_mut(&mut self.authorized_files).push(path);
            }
        }
        self
    }

    pub fn allows_entry(&self, entry: &ProjectPathEntry) -> bool {
        match &entry.provenance {
            ProjectPathProvenance::LegacyNative => {
                safe_regular_file(&entry.path) && !self.is_excluded(&entry.path)
            }
            ProjectPathProvenance::Configured => {
                self.configured_roots
                    .iter()
                    .chain(self.mapped_roots.iter())
                    .any(|root| {
                        self.allows_regular_file_under_root(&entry.path, &root.path)
                            && !self.is_excluded_for_root(&entry.path, root)
                    })
                    || self.allows_authorized_file(&entry.path, true)
            }
            ProjectPathProvenance::Mapped { root } => {
                !self.is_excluded_for_mapped_root(&entry.path, root)
                    && self.allows_regular_file_under_root(&entry.path, root)
            }
        }
    }

    /// Return a conservative estimate of the heap-backed state retained by
    /// this authorization policy.
    ///
    /// Callers that retain policies in bounded request state can use this
    /// value without exposing the policy's internal authorization roots. The
    /// estimate intentionally overcounts compiled glob data so a bound may
    /// reject a large policy early rather than relying on an optimistic
    /// accounting approximation.
    pub fn retained_size_hint(&self) -> usize {
        fn path_bytes(path: &PathBuf) -> usize {
            std::mem::size_of::<PathBuf>().saturating_add(path.capacity())
        }

        fn root_bytes(root: &AuthorizedReadRoot) -> usize {
            std::mem::size_of::<AuthorizedReadRoot>()
                .saturating_add(path_bytes(&root.path))
                .saturating_add(
                    root.pattern_exclusion_bases
                        .iter()
                        .map(path_bytes)
                        .sum::<usize>(),
                )
        }

        let configured = self.configured_roots.iter().map(root_bytes).sum::<usize>();
        let mapped = self.mapped_roots.iter().map(root_bytes).sum::<usize>();
        let authorized_files = self.authorized_files.iter().map(path_bytes).sum::<usize>();
        let exclusions = self
            .exclusions
            .iter()
            .map(|value| std::mem::size_of::<String>() + value.capacity())
            .sum::<usize>();
        let exclusion_bases = self.exclusion_bases.iter().map(path_bytes).sum::<usize>();
        let compiled_patterns = self
            .exclusions
            .iter()
            .map(String::capacity)
            .sum::<usize>()
            .saturating_add(self.exclusions.len().saturating_mul(128))
            .saturating_mul(4);

        std::mem::size_of::<Self>()
            .saturating_add(configured)
            .saturating_add(mapped)
            .saturating_add(authorized_files)
            .saturating_add(exclusions)
            .saturating_add(exclusion_bases)
            .saturating_add(compiled_patterns)
    }

    pub fn allows_location(&self, entry: &ProjectPathEntry) -> bool {
        match &entry.provenance {
            ProjectPathProvenance::LegacyNative => {
                path_has_no_symlink_component(&entry.path) && !self.is_excluded(&entry.path)
            }
            ProjectPathProvenance::Configured => {
                self.configured_roots
                    .iter()
                    .chain(self.mapped_roots.iter())
                    .any(|root| {
                        self.allows_location_under_root(&entry.path, &root.path)
                            && !self.is_excluded_for_root(&entry.path, root)
                    })
                    || self.allows_authorized_file(&entry.path, false)
            }
            ProjectPathProvenance::Mapped { root } => {
                !self.is_excluded_for_mapped_root(&entry.path, root)
                    && self.allows_location_under_root(&entry.path, root)
            }
        }
    }

    pub fn entry_for_path(&self, path: &Path) -> Option<ProjectPathEntry> {
        self.mapped_roots
            .iter()
            .filter(|root| project_path_starts_with(path, &root.path))
            .max_by_key(|root| root.path.components().count())
            .map(|root| ProjectPathEntry {
                path: path.to_path_buf(),
                provenance: ProjectPathProvenance::Mapped {
                    root: root.path.clone(),
                },
            })
            .or_else(|| {
                if self.authorizes_exact_file(path) {
                    return Some(ProjectPathEntry {
                        path: path.to_path_buf(),
                        provenance: ProjectPathProvenance::Configured,
                    });
                }
                self.configured_roots
                    .iter()
                    .filter(|root| project_path_starts_with(path, &root.path))
                    .max_by_key(|root| root.path.components().count())
                    .map(|_| ProjectPathEntry {
                        path: path.to_path_buf(),
                        provenance: ProjectPathProvenance::Configured,
                    })
            })
    }

    #[allow(dead_code)]
    pub fn allows_path(&self, path: &Path, provenance: &ProjectPathProvenance) -> bool {
        self.allows_location(&ProjectPathEntry {
            path: path.to_path_buf(),
            provenance: provenance.clone(),
        })
    }

    /// Check the lexical roots and exclusions for a path without inspecting
    /// the filesystem. This is intentionally weaker than [`Self::allows_path`]
    /// because it does not prove that the path is a regular, symlink-free
    /// source. Callers using this method must treat a positive result as a
    /// cheap relevance check, not as permission to read the path.
    pub fn allows_path_without_filesystem(
        &self,
        path: &Path,
        provenance: &ProjectPathProvenance,
    ) -> bool {
        match provenance {
            ProjectPathProvenance::LegacyNative => !self.is_excluded(path),
            ProjectPathProvenance::Configured => {
                self.configured_roots
                    .iter()
                    .chain(self.mapped_roots.iter())
                    .any(|root| {
                        project_path_starts_with(path, &root.path)
                            && !self.is_excluded_for_root(path, root)
                    })
                    || self.authorizes_exact_file(path) && !self.is_excluded_authorized_file(path)
            }
            ProjectPathProvenance::Mapped { root } => {
                project_path_starts_with(path, root)
                    && !self.is_excluded_for_mapped_root(path, root)
            }
        }
    }

    #[allow(dead_code)]
    pub(crate) fn identity(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }
}
impl ReadPolicy {
    fn allows_regular_file_under_root(&self, path: &Path, root: &Path) -> bool {
        self.allows_location_under_root(path, root) && safe_regular_file(path)
    }

    fn authorizes_exact_file(&self, path: &Path) -> bool {
        let path = absolute_lexical(path).unwrap_or_else(|_| path.to_path_buf());
        self.authorized_files
            .iter()
            .any(|authorized| authorized == &path)
    }

    fn allows_authorized_file(&self, path: &Path, require_regular_file: bool) -> bool {
        self.authorizes_exact_file(path)
            && path_has_no_symlink_component(path)
            && (!require_regular_file || safe_regular_file(path))
            && !self.is_excluded_authorized_file(path)
    }

    fn is_excluded_authorized_file(&self, path: &Path) -> bool {
        let case_insensitive = path_identity::is_case_insensitive(path);
        if path
            .components()
            .any(|component| is_default_excluded_component(component, case_insensitive))
            || self.is_excluded(path)
        {
            return true;
        }

        // A locator may be outside every configured read root, so no
        // root-relative path is available for the ordinary exclusion check.
        // Try each component-relative suffix instead: this preserves patterns
        // such as `private/**` for `/external/private/settings.proj`, without
        // treating the absolute filesystem prefix as part of the pattern.
        let components = path
            .components()
            .filter_map(|component| match component {
                Component::Normal(component) => Some(component),
                _ => None,
            })
            .collect::<Vec<_>>();
        (0..components.len()).any(|start| {
            let relative = components[start..].iter().collect::<PathBuf>();
            self.matches_exclusion_patterns_for_relative(&relative, case_insensitive)
        })
    }

    fn allows_location_under_root(&self, path: &Path, root: &Path) -> bool {
        project_path_starts_with(path, root) && path_has_no_symlink_component(path)
    }

    pub(super) fn is_excluded(&self, path: &Path) -> bool {
        self.is_excluded_under_bases(path, &self.exclusion_bases)
    }

    fn is_excluded_for_root(&self, path: &Path, root: &AuthorizedReadRoot) -> bool {
        has_default_excluded_component_under(path, &root.path)
            || self.matches_exclusion_patterns(path, &root.pattern_exclusion_bases)
    }

    fn is_excluded_for_mapped_root(&self, path: &Path, root: &Path) -> bool {
        has_default_excluded_component_under(path, root)
            || self.matches_exclusion_patterns(path, &self.exclusion_bases)
    }

    fn is_excluded_under_bases(&self, path: &Path, bases: &[PathBuf]) -> bool {
        let case_insensitive = path_identity::is_case_insensitive(path);
        bases.iter().any(|base| {
            let Some(relative) = project_relative_path(path, base) else {
                return false;
            };
            if relative
                .components()
                .any(|component| is_default_excluded_component(component, case_insensitive))
            {
                return true;
            }
            self.matches_exclusion_patterns_for_relative(&relative, case_insensitive)
        })
    }

    fn matches_exclusion_patterns(&self, path: &Path, bases: &[PathBuf]) -> bool {
        let case_insensitive = path_identity::is_case_insensitive(path);
        bases.iter().any(|base| {
            project_relative_path(path, base).is_some_and(|relative| {
                self.matches_exclusion_patterns_for_relative(&relative, case_insensitive)
            })
        })
    }

    fn matches_exclusion_patterns_for_relative(
        &self,
        relative: &Path,
        case_insensitive: bool,
    ) -> bool {
        let Some(patterns) = self
            .compiled_exclusions
            .as_ref()
            .as_ref()
            .and_then(|compiled| compiled.for_case(case_insensitive))
        else {
            return false;
        };
        let mut prefix = PathBuf::new();
        relative.components().any(|component| {
            prefix.push(component.as_os_str());
            patterns.is_match(prefix.to_string_lossy().replace('\\', "/"))
        })
    }
}
fn compile_exclude_patterns(
    patterns: &[String],
    case_insensitive: bool,
) -> Option<globset::GlobSet> {
    let mut builder = globset::GlobSetBuilder::new();
    let mut valid_pattern_count = 0;
    for pattern in patterns {
        let normalized = pattern.replace('\\', "/");
        let glob = globset::GlobBuilder::new(&normalized)
            .case_insensitive(case_insensitive)
            .build();
        if let Ok(glob) = glob {
            builder.add(glob);
            valid_pattern_count += 1;
        }
    }
    (valid_pattern_count > 0)
        .then(|| builder.build().ok())
        .flatten()
}

fn project_relative_path(path: &Path, root: &Path) -> Option<PathBuf> {
    let path_components = path.components().collect::<Vec<_>>();
    let root_components = root.components().collect::<Vec<_>>();
    if path_components.len() < root_components.len() || !path_identity::path_starts_with(path, root)
    {
        return None;
    }
    let mut relative = PathBuf::new();
    for component in &path_components[root_components.len()..] {
        relative.push(component.as_os_str());
    }
    Some(relative)
}

fn has_default_excluded_component_under(path: &Path, root: &Path) -> bool {
    let case_insensitive = path_identity::is_case_insensitive(path);
    project_relative_path(path, root).is_some_and(|relative| {
        relative
            .components()
            .any(|component| is_default_excluded_component(component, case_insensitive))
    })
}

fn is_default_excluded_component(component: Component<'_>, case_insensitive: bool) -> bool {
    let Component::Normal(name) = component else {
        return false;
    };
    let Some(name) = name.to_str() else {
        return false;
    };
    const DEFAULT_EXCLUDED_COMPONENTS: &[&str] = &[
        ".git",
        ".worktrees",
        ".hg",
        ".svn",
        ".idea",
        ".vscode",
        "target",
        "node_modules",
        "dist",
        "build",
        "coverage",
    ];
    if case_insensitive {
        DEFAULT_EXCLUDED_COMPONENTS
            .iter()
            .any(|excluded| name.eq_ignore_ascii_case(excluded))
    } else {
        DEFAULT_EXCLUDED_COMPONENTS.contains(&name)
    }
}

#[cfg(target_os = "linux")]
pub(super) fn resolve_project_path_entry(
    raw: &str,
    base: &Path,
    overrides: &EffectiveOverrides,
    warnings: &mut Vec<String>,
    kind: &str,
    warn_missing: bool,
    configured: bool,
) -> Option<ProjectPathEntry> {
    let resolved = project_path_candidate(raw, base, overrides, warnings, kind)?;
    let candidate = lexical_normalize(&resolved.path);
    match resolve_existing_path_status_with_provenance(&candidate, &resolved, raw, warnings, kind) {
        ExistingPathStatus::Found(path) => {
            Some(ProjectPathEntry::resolved(path, &resolved, configured))
        }
        ExistingPathStatus::Missing if warn_missing => {
            warnings.push(missing_path_warning(kind, raw, &candidate, &resolved));
            None
        }
        ExistingPathStatus::Missing | ExistingPathStatus::Unresolvable => None,
    }
}

fn safe_regular_file(path: &Path) -> bool {
    if filesystem_identity_unverified(path) {
        return false;
    }
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    metadata.is_file() && !metadata.file_type().is_symlink()
}

fn path_has_no_symlink_component(path: &Path) -> bool {
    path.ancestors().all(|ancestor| {
        fs::symlink_metadata(ancestor).map_or(true, |metadata| !metadata.file_type().is_symlink())
    })
}

pub(super) fn project_path_candidate(
    raw: &str,
    base: &Path,
    overrides: &EffectiveOverrides,
    warnings: &mut Vec<String>,
    kind: &str,
) -> Option<ResolvedPath> {
    let raw = raw.trim();
    if raw.is_empty() || raw.contains(UNRESOLVED_MARKER) {
        return None;
    }
    let normalized = normalize_delphi_separators(raw);
    match overrides.resolve_path(&normalized, base) {
        Ok(resolved) => Some(resolved),
        Err(error) if error == format!("Windows path is unavailable on Linux: {normalized}") => {
            warnings.push(format!(
                "Windows path in {kind} is unavailable on Linux and was omitted: {raw}"
            ));
            None
        }
        Err(error) => {
            let error = restore_original_path_in_error(&error, &normalized, raw);
            let provenance = matching_path_mapping(raw, overrides)
                .map(|mapping| mapping_provenance_suffix(raw, mapping))
                .unwrap_or_default();
            warnings.push(format!("{kind}: {error}{provenance}"));
            None
        }
    }
}

fn normalize_delphi_separators(path: &str) -> String {
    path.replace('\\', "/")
}

fn restore_original_path_in_error(error: &str, normalized: &str, raw: &str) -> String {
    error
        .strip_suffix(normalized)
        .map_or_else(|| error.to_owned(), |prefix| format!("{prefix}{raw}"))
}

fn matching_path_mapping<'a>(
    raw: &str,
    overrides: &'a EffectiveOverrides,
) -> Option<&'a PathMapping> {
    let normalized = normalize_delphi_separators(raw).to_ascii_lowercase();
    overrides
        .path_mappings
        .iter()
        .filter(|mapping| {
            normalized == mapping.from || normalized.starts_with(&format!("{}/", mapping.from))
        })
        .max_by_key(|mapping| mapping.from.len())
}

fn mapping_provenance_suffix(raw: &str, mapping: &PathMapping) -> String {
    format!(
        " (from {raw}; mapping {} -> {})",
        mapping.config_file.display(),
        mapping.to.display(),
    )
}

pub(super) fn missing_path_warning(
    kind: &str,
    raw: &str,
    candidate: &Path,
    resolved: &ResolvedPath,
) -> String {
    match resolved.mapping.as_ref() {
        Some(mapping) => format!(
            "{kind} path does not exist and was omitted: {} (from {raw}; mapping {} -> {})",
            candidate.display(),
            mapping.config_file.display(),
            mapping.to.display(),
        ),
        None => format!(
            "{kind} path does not exist and was omitted: {}",
            candidate.display()
        ),
    }
}

pub(super) fn search_path_missing_warning(
    kind: &str,
    raw: &str,
    candidate: &Path,
    resolved: &ResolvedPath,
) -> String {
    match resolved.mapping.as_ref() {
        Some(mapping) => format!(
            "{kind} path does not exist yet; retaining it for lazy discovery: {} (from {raw}; mapping {} -> {})",
            candidate.display(),
            mapping.config_file.display(),
            mapping.to.display(),
        ),
        None => format!(
            "{kind} path does not exist yet; retaining it for lazy discovery: {}",
            candidate.display()
        ),
    }
}

pub(super) fn resolve_existing_path_status_with_provenance(
    path: &Path,
    resolved: &ResolvedPath,
    raw: &str,
    warnings: &mut Vec<String>,
    kind: &str,
) -> ExistingPathStatus {
    let warning_start = warnings.len();
    let status = resolve_existing_path_status(path, warnings, kind);
    if matches!(status, ExistingPathStatus::Unresolvable)
        && let Some(mapping) = resolved.mapping.as_ref()
    {
        let provenance = mapping_provenance_suffix(raw, mapping);
        for warning in warnings.iter_mut().skip(warning_start) {
            if !warning.ends_with(&provenance) {
                warning.push_str(&provenance);
            }
        }
    }
    status
}

#[derive(Debug)]
pub(super) enum ExistingPathStatus {
    Found(PathBuf),
    Missing,
    Unresolvable,
}

pub(super) fn absolute_lexical(path: &Path) -> Result<PathBuf, String> {
    if is_windows_absolute(path) || path.is_absolute() {
        Ok(lexical_normalize(path))
    } else {
        let current = std::env::current_dir()
            .map_err(|error| format!("could not determine current directory: {error}"))?;
        Ok(lexical_normalize(&current.join(path)))
    }
}

pub(super) fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(std::path::MAIN_SEPARATOR.to_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() && !path.is_absolute() {
                    normalized.push(component.as_os_str());
                }
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    normalized
}

fn resolved_mapping_root(mapping: &PathMapping) -> PathBuf {
    let candidate = lexical_normalize(&mapping.to);
    let mut warnings = Vec::new();
    match resolve_existing_path_status(&candidate, &mut warnings, "mapped path root") {
        ExistingPathStatus::Found(path) => path,
        ExistingPathStatus::Missing | ExistingPathStatus::Unresolvable => candidate,
    }
}

pub(super) fn resolve_existing_path(
    path: &Path,
    warnings: &mut Vec<String>,
    kind: &str,
) -> Option<PathBuf> {
    match resolve_existing_path_status(path, warnings, kind) {
        ExistingPathStatus::Found(path) => Some(path),
        ExistingPathStatus::Missing | ExistingPathStatus::Unresolvable => None,
    }
}

pub(super) fn resolve_existing_path_status(
    path: &Path,
    warnings: &mut Vec<String>,
    kind: &str,
) -> ExistingPathStatus {
    if is_foreign_windows_path(path) {
        warnings.push(format!(
            "Windows path in {kind} is unavailable on Linux and was omitted: {}",
            path.display()
        ));
        return ExistingPathStatus::Unresolvable;
    }
    let absolute = if path.is_absolute() {
        lexical_normalize(path)
    } else {
        let Ok(current) = std::env::current_dir() else {
            return ExistingPathStatus::Unresolvable;
        };
        lexical_normalize(&current.join(path))
    };
    #[cfg(test)]
    if TEST_USE_LEGACY_PATH_RESOLUTION.with(std::cell::Cell::get) {
        return resolve_existing_path_status_legacy(&absolute, warnings, kind);
    }
    if path_identity::is_case_insensitive(&absolute) {
        // Windows and macOS report the on-disk spelling (letter case, and
        // long names on Windows) of an existing path; when it is the
        // spelling asked for, no walk is needed. Symlinks, junctions and
        // other spellings take the walk.
        if fs::canonicalize(&absolute)
            .is_ok_and(|canonical| path_identity::without_verbatim_prefix(canonical) == absolute)
        {
            return ExistingPathStatus::Found(absolute);
        }
    } else if fs::symlink_metadata(&absolute).is_ok() {
        // An exact entry on a case-sensitive volume needs no walk. Like the
        // walk, this follows intermediate directory symlinks but keeps a
        // final symlink as an entry, and it lets paths through execute-only
        // directories resolve without requiring a listing.
        return ExistingPathStatus::Found(absolute);
    }
    let mut current = path_identity::walk_root(&absolute);
    for component in absolute.components() {
        let Component::Normal(component) = component else {
            continue;
        };
        if path_resolution_cancelled() {
            return ExistingPathStatus::Unresolvable;
        }
        let wanted = component.to_string_lossy();
        let entries = match path_resolution_directory_listing(&current) {
            PathResolutionDirectoryListing::Entries(entries) => entries,
            PathResolutionDirectoryListing::Missing => {
                return ExistingPathStatus::Missing;
            }
            PathResolutionDirectoryListing::Cancelled => {
                return ExistingPathStatus::Unresolvable;
            }
            PathResolutionDirectoryListing::Error(error) => {
                warnings.push(format!(
                    "could not inspect {kind} path under {}: {error}",
                    current.display()
                ));
                return ExistingPathStatus::Unresolvable;
            }
        };
        let folded_wanted = wanted.to_ascii_lowercase();
        let Some(matches) = entries.case_insensitive_names.get(&folded_wanted) else {
            if let Some(name) = short_name_alias_target(&current, component, &entries) {
                current.push(name);
                continue;
            }
            return ExistingPathStatus::Missing;
        };
        if let Some(index) = matches
            .iter()
            .find(|&&index| entries.names[index].as_os_str() == component)
        {
            current.push(&entries.names[*index]);
            continue;
        }
        if matches.len() > 1 {
            warnings.push(format!(
                "ambiguous case-insensitive {kind} path component {wanted:?} under {}",
                current.display()
            ));
            return ExistingPathStatus::Unresolvable;
        }
        current.push(&entries.names[matches[0]]);
    }
    ExistingPathStatus::Found(current)
}

/// The listed name that `component` aliases when it is not itself listed,
/// such as the long name behind a Windows 8.3 short name (`RUNNER~1`). A
/// symlinked component is kept as is, never replaced by its target's name.
#[cfg(windows)]
fn short_name_alias_target(
    directory: &Path,
    component: &std::ffi::OsStr,
    entries: &PathResolutionDirectoryEntries,
) -> Option<std::ffi::OsString> {
    let candidate = directory.join(component);
    if fs::symlink_metadata(&candidate)
        .ok()?
        .file_type()
        .is_symlink()
    {
        return None;
    }
    let name = fs::canonicalize(&candidate)
        .ok()?
        .file_name()?
        .to_os_string();
    entries
        .names
        .iter()
        .any(|listed| listed == &name)
        .then_some(name)
}

#[cfg(not(windows))]
fn short_name_alias_target(
    _directory: &Path,
    _component: &std::ffi::OsStr,
    _entries: &PathResolutionDirectoryEntries,
) -> Option<std::ffi::OsString> {
    None
}

#[cfg(test)]
// Keep the former walk available to the ignored benchmark for a same-process baseline.
fn resolve_existing_path_status_legacy(
    absolute: &Path,
    warnings: &mut Vec<String>,
    kind: &str,
) -> ExistingPathStatus {
    let mut current = path_identity::walk_root(absolute);
    for component in absolute.components() {
        let Component::Normal(component) = component else {
            continue;
        };
        let wanted = component.to_string_lossy();
        let mut matches = Vec::new();
        let entries = match read_dir_for_path_resolution(&current) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return ExistingPathStatus::Missing;
            }
            Err(error) => {
                warnings.push(format!(
                    "could not inspect {kind} path under {}: {error}",
                    current.display()
                ));
                return ExistingPathStatus::Unresolvable;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    warnings.push(format!(
                        "could not inspect {kind} path under {}: {error}",
                        current.display()
                    ));
                    return ExistingPathStatus::Unresolvable;
                }
            };
            let name = entry.file_name();
            if name.to_string_lossy().eq_ignore_ascii_case(&wanted) {
                matches.push(entry.path());
            }
        }
        if let Some(exact) = matches
            .iter()
            .find(|candidate| candidate.file_name().is_some_and(|name| name == component))
        {
            current = exact.clone();
            continue;
        }
        if matches.len() > 1 {
            warnings.push(format!(
                "ambiguous case-insensitive {kind} path component {wanted:?} under {}",
                current.display()
            ));
            return ExistingPathStatus::Unresolvable;
        }
        let Some(next) = matches.into_iter().next() else {
            return ExistingPathStatus::Missing;
        };
        current = next;
    }
    ExistingPathStatus::Found(current)
}
pub(super) fn is_windows_absolute(path: &Path) -> bool {
    is_windows_absolute_text(&path.to_string_lossy())
}

/// A Windows absolute path on a host that cannot open it. On Windows such a
/// path is native.
pub(super) fn is_foreign_windows_path(path: &Path) -> bool {
    cfg!(not(windows)) && is_windows_absolute(path)
}

pub(super) fn is_windows_absolute_text(path: &str) -> bool {
    let bytes = path.as_bytes();
    (bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic())
        || path.starts_with("\\\\")
}

pub(super) fn extension_is(path: &Path, extension: &str) -> bool {
    path.extension()
        .is_some_and(|value| value.to_string_lossy().eq_ignore_ascii_case(extension))
}

pub(super) fn is_compiled_reference(raw: &str) -> bool {
    raw.trim()
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.').map(|(_, extension)| extension))
        .is_some_and(|extension| extension.eq_ignore_ascii_case("dcp"))
}

#[cfg(all(test, windows))]
mod windows_path_tests {
    use super::{ExistingPathStatus, path_identity, resolve_existing_path_status};
    use std::fs;
    use std::path::{Path, PathBuf};

    #[test]
    fn path_resolution_keeps_the_drive_and_fixes_spelling() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let unit = temp.path().join("Unit1.pas");
        fs::write(&unit, b"").expect("unit file");
        let long = path_identity::without_verbatim_prefix(fs::canonicalize(&unit).unwrap());
        let requested = PathBuf::from(unit.to_string_lossy().to_ascii_uppercase());
        let mut warnings = Vec::new();
        let status = resolve_existing_path_status(&requested, &mut warnings, "unit");
        assert!(
            matches!(&status, ExistingPathStatus::Found(found) if found == &long),
            "{status:?} {warnings:?}"
        );
    }

    /// The 8.3 short name of `path`, when the volume generates one.
    fn windows_short_path(path: &Path) -> Option<PathBuf> {
        use std::os::windows::process::CommandExt;
        // cmd does not parse the quoting `Command::arg` applies.
        let output = std::process::Command::new("cmd")
            .raw_arg(format!(
                "/C for %I in (\"{}\") do @echo %~sI",
                path.display()
            ))
            .output()
            .ok()?;
        let short = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        (short.as_os_str() != path.as_os_str() && short.exists()).then_some(short)
    }

    #[test]
    fn path_resolution_reports_the_long_name_for_a_short_name() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let directory = temp.path().join("Long Directory Name");
        fs::create_dir(&directory).expect("directory");
        let unit = directory.join("LongUnitName.pas");
        fs::write(&unit, b"").expect("unit file");
        let long = path_identity::without_verbatim_prefix(fs::canonicalize(&unit).unwrap());
        let Some(short) = windows_short_path(&unit) else {
            eprintln!("skipped: the volume does not generate 8.3 names");
            return;
        };
        let mut warnings = Vec::new();
        let status = resolve_existing_path_status(&short, &mut warnings, "unit");
        assert!(
            matches!(&status, ExistingPathStatus::Found(found) if found == &long),
            "{} -> {status:?} {warnings:?}",
            short.display()
        );
    }

    // CI's temporary directory may sit on a volume without 8.3 names; the
    // system volume normally has them.
    #[test]
    fn path_resolution_reports_the_long_name_for_a_system_short_name() {
        let Some(program_files) = std::env::var_os("ProgramFiles").map(PathBuf::from) else {
            return;
        };
        let long =
            path_identity::without_verbatim_prefix(fs::canonicalize(&program_files).unwrap());
        let Some(short) = windows_short_path(&long) else {
            // GitHub's Windows runners have 8.3 names on the system volume,
            // so CI must not skip this silently.
            assert!(
                std::env::var_os("CI").is_none(),
                "no 8.3 name for {}",
                long.display()
            );
            return;
        };
        let mut warnings = Vec::new();
        let status = resolve_existing_path_status(&short, &mut warnings, "directory");
        assert!(
            matches!(&status, ExistingPathStatus::Found(found) if found == &long),
            "{} -> {status:?} {warnings:?}",
            short.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    // Needs case-distinct paths, which Windows volumes cannot hold.
    #[test]
    #[cfg(not(windows))]
    fn path_resolution_prefers_an_exact_component_over_case_variants() {
        let Some(base) = crate::path_identity::case_sensitive_test_dir() else {
            return;
        };
        let temp = tempfile::tempdir_in(base).expect("temporary directory");
        let exact = temp.path().join("Foo");
        fs::create_dir(&exact).expect("exact directory");
        fs::create_dir(temp.path().join("FOO")).expect("case variant directory");
        let mut warnings = Vec::new();

        let status = resolve_existing_path_status(&exact, &mut warnings, "unit path");

        match status {
            ExistingPathStatus::Found(path) => assert_eq!(path, exact),
            ExistingPathStatus::Missing => panic!("exact entry should be found"),
            ExistingPathStatus::Unresolvable => panic!("exact entry should be resolvable"),
        }
        assert!(warnings.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn read_policy_does_not_join_windows_source_paths_under_the_root() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let root = temp.path().to_path_buf();
        let policy = ReadPolicy::new_with_installation_roots(
            std::slice::from_ref(&root),
            &["C:/Delphi/lib".to_string(), "src".to_string()],
            &[],
            &EffectiveOverrides::default(),
            &[],
        );

        let roots: Vec<&Path> = policy
            .configured_roots
            .iter()
            .map(|root| root.path.as_path())
            .collect();
        assert_eq!(roots, [root.as_path(), root.join("src").as_path()]);
    }

    #[test]
    fn path_resolution_returns_on_disk_spelling_for_case_insensitive_matches() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let expected = temp.path().join("Foo").join("BaR.pas");
        fs::create_dir(temp.path().join("Foo")).expect("directory");
        fs::write(&expected, b"").expect("unit file");
        let requested = temp.path().join("foo").join("bar.PAS");
        let mut warnings = Vec::new();

        let status = resolve_existing_path_status(&requested, &mut warnings, "unit");

        match status {
            ExistingPathStatus::Found(path) => assert_eq!(path, expected),
            ExistingPathStatus::Missing => panic!("case-insensitive entry should be found"),
            ExistingPathStatus::Unresolvable => {
                panic!("a unique case-insensitive match should resolve")
            }
        }
        assert!(warnings.is_empty());
    }

    // Needs case-distinct paths, which Windows volumes cannot hold.
    #[test]
    #[cfg(not(windows))]
    fn path_resolution_rejects_ambiguous_case_insensitive_matches() {
        let Some(base) = crate::path_identity::case_sensitive_test_dir() else {
            return;
        };
        let temp = tempfile::tempdir_in(base).expect("temporary directory");
        fs::create_dir(temp.path().join("Foo")).expect("first case variant");
        fs::create_dir(temp.path().join("FOO")).expect("second case variant");
        let requested = temp.path().join("foo");
        let mut warnings = Vec::new();

        let status = resolve_existing_path_status(&requested, &mut warnings, "unit");

        assert!(matches!(status, ExistingPathStatus::Unresolvable));
        assert_eq!(
            warnings,
            vec![format!(
                "ambiguous case-insensitive unit path component \"foo\" under {}",
                temp.path().display()
            )]
        );
    }

    #[test]
    fn path_resolution_reports_missing_components_without_a_warning() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let requested = temp.path().join("Missing").join("Unit.pas");
        let mut warnings = Vec::new();

        let status = resolve_existing_path_status(&requested, &mut warnings, "unit");

        assert!(matches!(status, ExistingPathStatus::Missing));
        assert!(warnings.is_empty());
    }

    #[test]
    fn path_resolution_reports_file_as_directory_as_unresolvable() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let file = temp.path().join("NotADirectory");
        fs::write(&file, b"").expect("regular file");
        let requested = file.join("Unit.pas");
        let mut warnings = Vec::new();

        let status = resolve_existing_path_status(&requested, &mut warnings, "unit");

        assert!(matches!(status, ExistingPathStatus::Unresolvable));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].starts_with(&format!(
            "could not inspect unit path under {}:",
            file.display()
        )));
    }

    #[test]
    fn path_resolution_joins_relative_paths_to_current_directory() {
        let current = std::env::current_dir().expect("current directory");
        let temp = tempfile::Builder::new()
            .prefix("pascal-project-relative-")
            .tempdir_in(&current)
            .expect("temporary directory under current directory");
        let expected = temp.path().join("Unit.pas");
        fs::write(&expected, b"").expect("unit file");
        let requested = expected
            .strip_prefix(&current)
            .expect("temporary directory is under the current directory");
        let mut warnings = Vec::new();

        let status = resolve_existing_path_status(requested, &mut warnings, "unit path");

        match status {
            ExistingPathStatus::Found(path) => assert_eq!(path, expected),
            ExistingPathStatus::Missing => panic!("relative path should be found"),
            ExistingPathStatus::Unresolvable => panic!("relative path should be resolvable"),
        }
        assert!(warnings.is_empty());
    }

    #[test]
    fn path_resolution_lexically_normalizes_parent_components() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let expected = temp.path().join("Target").join("Unit.pas");
        fs::create_dir(temp.path().join("Target")).expect("target directory");
        fs::write(&expected, b"").expect("unit file");
        let requested = temp
            .path()
            .join("Unused")
            .join("..")
            .join("Target")
            .join("Unit.pas");
        let mut warnings = Vec::new();

        let status = resolve_existing_path_status(&requested, &mut warnings, "unit path");

        match status {
            ExistingPathStatus::Found(path) => assert_eq!(path, expected),
            ExistingPathStatus::Missing => panic!("normalized path should be found"),
            ExistingPathStatus::Unresolvable => panic!("normalized path should be resolvable"),
        }
        assert!(warnings.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn path_resolution_preserves_spelling_through_symlinked_directories() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let target = temp.path().join("Target");
        let link = temp.path().join("Link");
        fs::create_dir(&target).expect("target directory");
        fs::write(target.join("Unit.pas"), b"").expect("unit file");
        symlink(&target, &link).expect("directory symlink");
        let requested = link.join("Unit.pas");
        let mut warnings = Vec::new();

        let status = resolve_existing_path_status(&requested, &mut warnings, "unit");

        match status {
            ExistingPathStatus::Found(path) => assert_eq!(path, requested),
            ExistingPathStatus::Missing => panic!("symlinked target should be found"),
            ExistingPathStatus::Unresolvable => panic!("symlinked target should resolve"),
        }
        assert!(warnings.is_empty());
    }

    #[test]
    #[cfg(not(windows))]
    fn path_resolution_rejects_windows_absolute_paths_on_linux() {
        let requested = std::path::Path::new(r"C:\Delphi\Unit.pas");
        let mut warnings = Vec::new();

        let status = resolve_existing_path_status(requested, &mut warnings, "unit path");

        assert!(matches!(status, ExistingPathStatus::Unresolvable));
        assert_eq!(
            warnings,
            vec![format!(
                "Windows path in unit path is unavailable on Linux and was omitted: {}",
                requested.display()
            )]
        );
    }

    #[test]
    fn path_resolution_does_not_list_directories_for_exact_paths() {
        const PATHS: usize = 24;
        let temp = tempfile::tempdir().expect("temporary directory");
        let units = temp.path().join("Source").join("Units");
        fs::create_dir_all(&units).expect("unit directory");
        let paths = (0..PATHS)
            .map(|index| {
                let path = units.join(format!("Unit{index:02}.pas"));
                fs::write(&path, b"").expect("unit file");
                path
            })
            .collect::<Vec<_>>();
        let (statuses, read_dir_calls) = with_path_resolution_read_dir_count(|| {
            paths
                .iter()
                .map(|path| {
                    let mut warnings = Vec::new();
                    resolve_existing_path_status(path, &mut warnings, "unit")
                })
                .collect::<Vec<_>>()
        });

        assert_eq!(
            read_dir_calls, 0,
            "exact paths should avoid directory listings"
        );
        for (path, status) in paths.iter().zip(statuses) {
            match status {
                ExistingPathStatus::Found(resolved) => assert_eq!(&resolved, path),
                ExistingPathStatus::Missing => panic!("exact path should be found"),
                ExistingPathStatus::Unresolvable => panic!("exact path should be resolvable"),
            }
        }
    }

    #[test]
    fn configured_exclusions_are_not_weakened_by_overlapping_source_roots() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let workspace = temp.path().join("ws");
        let excluded = workspace.join("vendor/private/settings.optset");
        fs::create_dir_all(excluded.parent().expect("excluded parent")).expect("directories");
        fs::write(&excluded, b"settings").expect("excluded metadata");

        let policy = ReadPolicy::new(
            std::slice::from_ref(&workspace),
            &["vendor".to_string()],
            &["vendor/private".to_string()],
            &EffectiveOverrides::default(),
        );
        let entry = ProjectPathEntry {
            path: excluded,
            provenance: ProjectPathProvenance::Configured,
        };

        assert!(
            !policy.allows_entry(&entry),
            "an overlapping source root must not bypass the workspace-relative exclusion"
        );
    }

    #[test]
    fn legacy_payload_entries_respect_configured_exclusions() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        let path = root.join("private/Hidden.pas");
        fs::create_dir_all(path.parent().expect("private directory")).expect("directories");
        fs::write(&path, b"unit Hidden; interface implementation end.").expect("source");
        let policy = ReadPolicy::new(
            std::slice::from_ref(&root),
            &[],
            &["private".to_string()],
            &EffectiveOverrides::default(),
        );
        let entry = ProjectPathEntry {
            path,
            provenance: ProjectPathProvenance::LegacyNative,
        };

        assert!(
            !policy.allows_legacy_payload_entry(&entry),
            "legacy payload compatibility must not bypass configured exclusions"
        );
    }

    fn metrics_payload_fixture() -> (tempfile::TempDir, ReadPolicy, ProjectPathEntry) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("A.pas");
        fs::write(&path, b"unit A;").unwrap();
        let policy = ReadPolicy::new(
            &[root.path().to_path_buf()],
            &[],
            &[],
            &EffectiveOverrides::default(),
        );
        let entry = ProjectPathEntry {
            path,
            provenance: ProjectPathProvenance::Configured,
        };
        (root, policy, entry)
    }

    #[test]
    fn metrics_payload_counts_authorized_bytes_once() {
        use crate::analysis_metrics::{Metric, TestScope};
        let (_root, policy, entry) = metrics_payload_fixture();
        let scope = TestScope::new();
        assert_eq!(policy.read_payload_bytes(&entry, 100).unwrap(), b"unit A;");
        let s = scope.snapshot();
        assert_eq!(s.value(Metric::AuthorizedPayloadReadAttempts), 1);
        assert_eq!(s.value(Metric::AuthorizedPayloadBytes), 7);
        assert_eq!(s.value(Metric::AuthorizedPayloadReadFailures), 0);
    }

    #[test]
    fn metrics_overlimit_preserves_read_error_and_prefix_count() {
        use crate::analysis_metrics::{Metric, TestScope};
        let (_root, policy, entry) = metrics_payload_fixture();
        let scope = TestScope::new();
        assert_eq!(
            policy.read_payload_bytes(&entry, 3).unwrap_err(),
            "file exceeds the 3 byte safety limit"
        );
        let s = scope.snapshot();
        assert_eq!(s.value(Metric::AuthorizedPayloadReadAttempts), 1);
        assert_eq!(s.value(Metric::AuthorizedPayloadBytes), 4);
        assert_eq!(s.value(Metric::AuthorizedPayloadReadFailures), 1);
    }

    #[test]
    fn metrics_unauthorized_payload_does_not_claim_an_authorized_attempt() {
        use crate::analysis_metrics::{Metric, TestScope};
        let (_root, policy, _entry) = metrics_payload_fixture();
        let outside = tempfile::tempdir().unwrap();
        let path = outside.path().join("B.pas");
        fs::write(&path, b"unit B;").unwrap();
        let entry = ProjectPathEntry {
            path,
            provenance: ProjectPathProvenance::Configured,
        };
        let scope = TestScope::new();
        assert_eq!(
            policy.read_payload_bytes(&entry, 100).unwrap_err(),
            "payload path is not authorized"
        );
        let s = scope.snapshot();
        assert_eq!(s.value(Metric::AuthorizedPayloadReadAttempts), 0);
        assert_eq!(s.value(Metric::AuthorizedPayloadBytes), 0);
        assert_eq!(s.value(Metric::AuthorizedPayloadReadFailures), 0);
    }

    #[test]
    fn metrics_hash_and_stamp_probe_count_calls() {
        use crate::analysis_metrics::{Metric, TestScope};
        let scope = TestScope::new();
        content_hash_bytes(b"12345");
        let _ = path_stamp_result(std::path::Path::new("/nonexistent/metrics-probe"));
        let s = scope.snapshot();
        assert_eq!(s.value(Metric::LegacyHashBytes), 5);
        assert_eq!(s.value(Metric::ProjectStampProbes), 1);
    }
}

#[cfg(test)]
mod view_tests {
    use super::*;

    #[test]
    fn read_policy_view_follows_policy_equality() {
        let overrides = EffectiveOverrides::default();
        let first = ReadPolicy::new(
            &[PathBuf::from("/w")],
            &[],
            &["*.bak".to_string()],
            &overrides,
        );
        let same = ReadPolicy::new(
            &[PathBuf::from("/w")],
            &[],
            &["*.bak".to_string()],
            &overrides,
        );
        let other_root = ReadPolicy::new(
            &[PathBuf::from("/v")],
            &[],
            &["*.bak".to_string()],
            &overrides,
        );
        let other_exclusion = ReadPolicy::new(&[PathBuf::from("/w")], &[], &[], &overrides);
        assert_eq!(first, same);
        assert_eq!(first.view(), same.view());
        assert_ne!(first.view(), other_root.view());
        assert_ne!(first.view(), other_exclusion.view());
        assert_eq!(first.view().exclusions, ["*.bak".to_string()]);
    }
}

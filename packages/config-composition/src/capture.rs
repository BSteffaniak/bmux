use crate::{Codec, ConfigValue, Document, Error, ErrorKind, MergePolicy, Result, SourceId};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

#[derive(Clone)]
/// Explicit caller-captured working directory and environment; Debug is redacted.
pub struct LoadContext {
    cwd: PathBuf,
    environment: BTreeMap<String, String>,
}
impl std::fmt::Debug for LoadContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LoadContext(<redacted>)")
    }
}
impl LoadContext {
    /// Construct from explicit inputs; invalid context returns InvalidContext.
    pub fn new(cwd: PathBuf, environment: BTreeMap<String, String>) -> Result<Self> {
        if !cwd.is_absolute() {
            return Err(Error::new(ErrorKind::InvalidContext));
        }
        Ok(Self {
            cwd: normalize(&cwd),
            environment,
        })
    }
    /// Absolute working directory used to anchor relative paths.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
    /// Caller-supplied environment; never implicitly sampled from the process.
    pub fn environment(&self) -> &BTreeMap<String, String> {
        &self.environment
    }
    fn path(&self, path: &Path) -> PathBuf {
        normalize(&if path.is_absolute() {
            path.to_owned()
        } else {
            self.cwd.join(path)
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Missing-file authority; only Optional suppresses Missing.
pub enum Requirement {
    /// Fail when the requested object is missing.
    Required,
    /// Ignore only missing objects; propagate every other failure.
    Optional,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Explicit-file authority relative to configured discovery layers.
pub enum ExplicitMode {
    /// Skip other file layers, retaining defaults and inline layers.
    Exclusive,
    /// Apply the explicit file after all configured layers.
    Additive,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
/// Policy for configured file symlinks and their path components.
pub enum SymlinkPolicy {
    #[default]
    /// Follow configured symlinks, including immutable Nix-store targets.
    Follow,
    /// Reject matching objects instead of silently accepting them.
    Reject,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
/// Policy for repeated canonical file paths.
pub enum DuplicatePolicy {
    #[default]
    /// Capture each canonical path only once.
    Ignore,
    /// Reject matching objects instead of silently accepting them.
    Reject,
    /// Capture every occurrence in configured order.
    Allow,
}
#[derive(Clone, Default)]
/// Canonical filesystem containment policy.
pub enum BoundaryPolicy {
    #[default]
    /// No additional application containment boundary.
    Filesystem,
    /// Require canonical files and search directories to stay within this root.
    Within(PathBuf),
}
/// Which matching directories an ancestor search selects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SearchMatches {
    /// Select every matching directory within the configured bounds.
    #[default]
    All,
    /// Select only the first match walking from the start toward the root.
    Nearest,
}
/// Application order for all matching ancestor directories.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SearchOrder {
    /// Apply farthest directories first, so nearer values win replacement merges.
    #[default]
    OuterFirst,
    /// Apply nearest directories first, so farther values win replacement merges.
    InnerFirst,
}
/// Explicit inclusive stopping rule for an ancestor search.
#[derive(Clone, Default)]
pub enum SearchBoundary {
    /// Stop only at filesystem root, loader containment boundary, or depth bound.
    #[default]
    Filesystem,
    /// Stop at this inclusive canonical directory; it must contain the start.
    Directory(PathBuf),
    /// Stop at the first directory containing this single-component marker.
    /// A marker may be a file or directory (including a Git worktree .git file).
    /// Marker contents are never read and links are never followed for detection.
    Marker(String),
}
impl SearchBoundary {
    /// Explicit Git repository boundary, recognizing .git files and directories.
    /// This does not execute Git or enable any discovery unless selected by a layer.
    pub fn git_repository() -> Self {
        Self::Marker(".git".into())
    }
}
/// Typed bounded ancestor matching, ordering, and stopping policy.
#[derive(Clone, Default)]
pub struct SearchPolicy {
    /// Select nearest or all matching directories.
    pub matches: SearchMatches,
    /// Apply all matches in this order; Nearest is independent of this field.
    pub order: SearchOrder,
    /// Inclusive stopping rule. Absent markers fall back to depth/root bounds.
    pub boundary: SearchBoundary,
}
#[derive(Clone)]
/// Typed ordered source-discovery or inline-value policy.
pub enum Layer {
    /// Named typed defaults or overrides at this exact position; identical to Inline.
    Value(ConfigValue),
    /// Configurable ancestor search; existing Ancestors retains its original semantics.
    AncestorSearch {
        /// Starting directory; None uses the explicit context cwd.
        start: Option<PathBuf>,
        /// Single-component filename to match at each directory.
        file_name: String,
        /// Maximum parent edges from the starting directory, root depth zero.
        max_depth: usize,
        /// Typed matching, merge ordering, and inclusive stopping rule.
        policy: SearchPolicy,
    },
    /// An owned typed value at this exact position in the ordered layer sequence.
    Inline(ConfigValue),
    /// Owned document text parsed by the selected codec during pure resolution.
    Text(String),
    /// Optional path selected by the captured environment, never process state.
    EnvironmentFile {
        /// Application-selected environment variable name.
        variable: String,
        /// Missing-file policy when the variable is present.
        requirement: Requirement,
    },
    /// Optional document text selected by the captured environment.
    /// Empty text is admitted as an empty document, not treated as absence.
    EnvironmentText {
        /// Application-selected environment variable name.
        variable: String,
    },
    /// Capture this exact caller-selected path.
    File {
        /// Explicit path; file layers anchor relative paths to LoadContext cwd.
        path: PathBuf,
        /// Whether a missing filesystem object is an error.
        requirement: Requirement,
    },
    /// Optional XDG application user file with HOME fallback.
    UserXdg {
        /// Single-component application directory name.
        application: String,
        /// Single-component configuration filename.
        file_name: String,
    },
    /// Search a bounded ancestor chain; farthest configurations apply first.
    Ancestors {
        /// Starting directory; None selects the explicit context cwd.
        start: Option<PathBuf>,
        /// Single-component configuration filename.
        file_name: String,
        /// Inclusive ancestor stop directory or canonical containment policy.
        boundary: Option<PathBuf>,
        /// Maximum number of directory edges to traverse from the start.
        max_depth: usize,
    },
    /// Search bounded descendant directories in sorted lexical preorder.
    Descendants {
        /// Explicit root directory for descendant traversal.
        root: PathBuf,
        /// Single-component configuration filename.
        file_name: String,
        /// Maximum number of directory edges to traverse from the start.
        max_depth: usize,
    },
    /// Apply matching extension fragments in sorted filename order.
    Fragments {
        /// Directory whose direct children are considered as fragments.
        directory: PathBuf,
        /// Exact fragment filename extension, without a leading dot.
        extension: String,
        /// Whether a missing filesystem object is an error.
        requirement: Requirement,
    },
}
#[derive(Clone, Copy, Debug)]
/// Capture and resolution resource bounds; defaults are deliberately conservative.
pub struct Limits {
    /// Maximum bytes read from any one file.
    pub max_file_bytes: usize,
    /// Maximum aggregate bytes read across file layers.
    pub max_total_bytes: usize,
    /// Maximum captured sources, including inline layers but excluding defaults.
    pub max_files: usize,
    /// Maximum directory entries inspected across all searches.
    pub max_entries: usize,
    /// Maximum resolved value depth, root zero; supported maximum is 128.
    pub max_value_depth: usize,
    /// Maximum nodes in each input and composed value.
    pub max_value_nodes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_bytes: 1024 * 1024,
            max_total_bytes: 8 * 1024 * 1024,
            max_files: 128,
            max_entries: 4096,
            max_value_depth: 64,
            max_value_nodes: 100_000,
        }
    }
}
#[derive(Clone, Default)]
/// Consuming typed policy builder; build validates policy without filesystem access.
pub struct LoaderBuilder {
    defaults: Option<ConfigValue>,
    layers: Vec<(String, Layer)>,
    explicit: Option<(PathBuf, Requirement, ExplicitMode)>,
    policy: MergePolicy,
    limits: Limits,
    symlinks: SymlinkPolicy,
    duplicates: DuplicatePolicy,
    boundary: BoundaryPolicy,
}
impl LoaderBuilder {
    /// Set the memory-only defaults applied before every named layer.
    pub fn defaults(mut self, value: ConfigValue) -> Self {
        self.defaults = Some(value);
        self
    }
    /// Append a uniquely named layer at this exact ordering position.
    pub fn layer(mut self, name: impl Into<String>, layer: Layer) -> Self {
        self.layers.push((name.into(), layer));
        self
    }
    /// Append an ordered family of layers under one namespace. Names remain
    /// inspectable and collisions are validated by build, just like single layers.
    pub fn layers(
        mut self,
        namespace: &str,
        layers: impl IntoIterator<Item = (String, Layer)>,
    ) -> Self {
        self.layers.extend(
            layers
                .into_iter()
                .map(|(name, layer)| (format!("{namespace}/{name}"), layer)),
        );
        self
    }
    /// Append a selected file when present; None contributes no layer.
    pub fn file(
        self,
        name: impl Into<String>,
        path: Option<&Path>,
        requirement: Requirement,
    ) -> Self {
        match path {
            Some(path) => self.layer(
                name,
                Layer::File {
                    path: path.to_owned(),
                    requirement,
                },
            ),
            None => self,
        }
    }
    /// Append selected inline document text when present, without logging its contents.
    pub fn text(self, name: impl Into<String>, text: Option<&str>) -> Self {
        match text {
            Some(text) => self.layer(name, Layer::Text(text.to_owned())),
            None => self,
        }
    }
    /// Append a caller-selected ordered file family, with stable namespaced origins.
    pub fn files(
        self,
        namespace: &str,
        paths: impl IntoIterator<Item = PathBuf>,
        requirement: Requirement,
    ) -> Self {
        self.layers(
            namespace,
            paths
                .into_iter()
                .enumerate()
                .map(|(index, path)| (index.to_string(), Layer::File { path, requirement })),
        )
    }
    /// Add a conventional file-path and inline-text environment pair at this position.
    /// Names and prefix are application-owned. Exclusive mode excludes both sources.
    pub fn environment(self, prefix: &str) -> Self {
        self.layer(
            format!("{prefix}_CONFIG"),
            Layer::EnvironmentFile {
                variable: format!("{prefix}_CONFIG"),
                requirement: Requirement::Required,
            },
        )
        .layer(
            format!("{prefix}_CONFIG_TOML"),
            Layer::EnvironmentText {
                variable: format!("{prefix}_CONFIG_TOML"),
            },
        )
    }
    /// Set the last-applied explicit file and its missing-file/authority policy.
    pub fn explicit_file(
        mut self,
        path: PathBuf,
        requirement: Requirement,
        mode: ExplicitMode,
    ) -> Self {
        self.explicit = Some((path, requirement, mode));
        self
    }
    /// Set global merge behavior and exact-path overrides.
    pub fn merge_policy(mut self, policy: MergePolicy) -> Self {
        self.policy = policy;
        self
    }
    /// Set capture and pure-resolution resource bounds.
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }
    /// Set configured file symlink behavior.
    pub fn symlinks(mut self, policy: SymlinkPolicy) -> Self {
        self.symlinks = policy;
        self
    }
    /// Set repeated canonical file behavior.
    pub fn duplicates(mut self, policy: DuplicatePolicy) -> Self {
        self.duplicates = policy;
        self
    }
    /// Inclusive ancestor stop directory or canonical containment policy.
    pub fn boundary(mut self, policy: BoundaryPolicy) -> Self {
        self.boundary = policy;
        self
    }
    /// Validate names, policy and default-value bounds; returns InvalidPolicy or Limit.
    pub fn build(self) -> Result<Loader> {
        let limits = self.limits;
        if limits.max_files == 0
            || limits.max_entries == 0
            || limits.max_file_bytes == 0
            || limits.max_total_bytes == 0
            || limits.max_value_nodes == 0
            || limits.max_value_depth > 128
        {
            return Err(Error::new(ErrorKind::InvalidPolicy));
        }
        let mut names = BTreeSet::new();
        for (name, layer) in &self.layers {
            if name.is_empty() || !names.insert(name) {
                return Err(Error::new(ErrorKind::InvalidPolicy));
            }
            match layer {
                Layer::AncestorSearch {
                    file_name,
                    max_depth,
                    policy,
                    ..
                } => {
                    single_component(file_name)?;
                    if *max_depth > 128 {
                        return Err(Error::new(ErrorKind::InvalidPolicy));
                    }
                    if let SearchBoundary::Marker(marker) = &policy.boundary {
                        single_component(marker)?;
                    }
                }
                Layer::UserXdg {
                    application,
                    file_name,
                } => {
                    single_component(application)?;
                    single_component(file_name)?;
                }
                Layer::Ancestors {
                    file_name,
                    max_depth,
                    ..
                }
                | Layer::Descendants {
                    file_name,
                    max_depth,
                    ..
                } => {
                    single_component(file_name)?;
                    if *max_depth > 128 {
                        return Err(Error::new(ErrorKind::InvalidPolicy));
                    }
                }
                Layer::Fragments { extension, .. } => {
                    single_component(extension)?;
                }
                _ => {}
            }
        }
        if let Some(value) = &self.defaults {
            crate::value::validate_value(value, limits.max_value_depth, limits.max_value_nodes)?;
        }
        Ok(Loader(self))
    }
}
#[derive(Clone)]
/// Immutable typed loader; capture performs reads and resolve is pure.
pub struct Loader(LoaderBuilder);
#[derive(Clone)]
/// Explicit trusted-access provenance; Debug redacts layer names and paths.
pub struct Source {
    /// Stable source identity within this capture.
    pub id: SourceId,
    /// Caller-assigned ordered layer name (sensitive; Debug redacts it).
    pub layer: String,
    /// Explicit path; file layers anchor relative paths to LoadContext cwd.
    pub path: Option<PathBuf>,
}
impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Source")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}
#[derive(Clone)]
struct CapturedDocument {
    source: SourceId,
    content: CapturedContent,
}
#[derive(Clone)]
enum CapturedContent {
    Bytes(Vec<u8>),
    Inline(ConfigValue),
}
#[derive(Clone)]
/// Owned immutable documents and policies; Debug never reveals captured contents.
pub struct Captured {
    defaults: Option<ConfigValue>,
    documents: Vec<CapturedDocument>,
    sources: Vec<Source>,
    policy: MergePolicy,
    limits: Limits,
}
impl std::fmt::Debug for Captured {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Captured")
            .field("documents", &self.documents.len())
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Debug)]
/// Composed document plus source manifest; explicit fields are sensitive.
pub struct Resolved {
    /// Composed value and node-level origin map.
    pub document: Document,
    /// Ordered capture source manifest, beginning with defaults.
    pub sources: Vec<Source>,
}
impl Resolved {
    /// Decode without text roundtripping; returns Decode for serde failures or Limit for excessive depth.
    pub fn decode<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        self.document.decode()
    }
}
impl Captured {
    /// Ordered capture source manifest, beginning with defaults.
    pub fn sources(&self) -> &[Source] {
        &self.sources
    }
    /// Parse individual captured documents for application-owned transforms before
    /// composition. Sources retain their capture identities; no ambient reads occur.
    pub fn documents(&self, codec: &impl Codec) -> Result<Vec<Document>> {
        self.documents
            .iter()
            .map(|captured| {
                let value = match &captured.content {
                    CapturedContent::Inline(value) => value.clone(),
                    CapturedContent::Bytes(bytes) => {
                        let text = std::str::from_utf8(bytes)
                            .map_err(|_| Error::new(ErrorKind::Parse).at(captured.source))?;
                        codec
                            .parse(text)
                            .map_err(|error| error.at(captured.source))?
                    }
                };
                if let Err(error) = crate::value::validate_value(
                    &value,
                    self.limits.max_value_depth,
                    self.limits.max_value_nodes,
                ) {
                    crate::discard_value(value);
                    return Err(error.at(captured.source));
                }
                Ok(Document::new(value, captured.source))
            })
            .collect()
    }
    /// Apply a product-owned transform to each captured document without reopening
    /// files, rereading the environment, or losing source identities.
    pub fn try_map_documents<E>(
        &self,
        codec: &impl Codec,
        mut transform: impl FnMut(&Source, ConfigValue) -> std::result::Result<ConfigValue, E>,
    ) -> std::result::Result<Self, E>
    where
        E: From<Error>,
    {
        let mut mapped = self.clone();
        mapped.documents = self
            .documents(codec)?
            .into_iter()
            .map(|document| {
                let source = document.provenance[&Vec::new()];
                let value = transform(&self.sources[source.0], document.value)?;
                if let Err(error) = crate::value::validate_value(
                    &value,
                    self.limits.max_value_depth,
                    self.limits.max_value_nodes,
                ) {
                    crate::discard_value(value);
                    return Err(error.at(source).into());
                }
                Ok(CapturedDocument {
                    source,
                    content: CapturedContent::Inline(value),
                })
            })
            .collect::<std::result::Result<_, E>>()?;
        Ok(mapped)
    }
    /// No filesystem, environment, or working-directory access occurs here.
    pub fn resolve(&self, codec: &impl Codec) -> Result<Resolved> {
        let mut document = Document::new(
            self.defaults
                .clone()
                .unwrap_or_else(|| ConfigValue::Table(BTreeMap::new())),
            SourceId(0),
        );
        for incoming in self.documents(codec)? {
            let source = incoming.provenance[&Vec::new()];
            document
                .merge(incoming, &self.policy)
                .map_err(|error| error.at(source))?;
            crate::value::validate_value(
                &document.value,
                self.limits.max_value_depth,
                self.limits.max_value_nodes,
            )
            .map_err(|error| error.at(source))?;
        }
        Ok(Resolved {
            document,
            sources: self.sources.clone(),
        })
    }
}
impl Loader {
    /// Standard user convention only: optional XDG application/config.toml,
    /// with no ancestor, descendant, or project discovery. Defaults remain memory-only.
    /// Application names are validated by `build`, which returns InvalidPolicy.
    pub fn builder_for(application: impl Into<String>) -> LoaderBuilder {
        Self::builder().layer(
            "user",
            Layer::UserXdg {
                application: application.into(),
                file_name: "config.toml".into(),
            },
        )
    }
    /// Start with no implicit layers; use builder_for for the optional XDG convention.
    pub fn builder() -> LoaderBuilder {
        LoaderBuilder::default()
    }
    /// Capture explicit context once; filesystem, authority and limit failures are structured errors.
    pub fn capture(&self, context: &LoadContext) -> Result<Captured> {
        let boundary = match &self.0.boundary {
            BoundaryPolicy::Filesystem => None,
            BoundaryPolicy::Within(path) => {
                Some(fs::canonicalize(context.path(path)).map_err(io_error)?)
            }
        };
        let mut state = CaptureState {
            loader: &self.0,
            context,
            boundary,
            entries: 0,
            total: 0,
            seen: BTreeSet::new(),
            documents: Vec::new(),
            sources: vec![Source {
                id: SourceId(0),
                layer: "defaults".into(),
                path: None,
            }],
        };
        for (name, layer) in &self.0.layers {
            if matches!(layer, Layer::Inline(_) | Layer::Value(_) | Layer::Text(_))
                || !matches!(self.0.explicit, Some((_, _, ExplicitMode::Exclusive)))
            {
                state.layer(name, layer)?;
            }
        }
        if let Some((path, requirement, _)) = &self.0.explicit {
            state.file("explicit", path, *requirement)?;
        }
        Ok(Captured {
            defaults: self.0.defaults.clone(),
            documents: state.documents,
            sources: state.sources,
            policy: self.0.policy.clone(),
            limits: self.0.limits,
        })
    }
}
struct CaptureState<'a> {
    loader: &'a LoaderBuilder,
    context: &'a LoadContext,
    boundary: Option<PathBuf>,
    entries: usize,
    total: usize,
    seen: BTreeSet<PathBuf>,
    documents: Vec<CapturedDocument>,
    sources: Vec<Source>,
}
impl CaptureState<'_> {
    fn text(&mut self, name: &str, text: &str) -> Result<()> {
        let id = SourceId(self.sources.len());
        let limits = self.loader.limits;
        self.total = self
            .total
            .checked_add(text.len())
            .ok_or(Error::new(ErrorKind::Limit).at(id))?;
        if text.len() > limits.max_file_bytes
            || self.total > limits.max_total_bytes
            || self.documents.len() >= limits.max_files
        {
            return Err(Error::new(ErrorKind::Limit).at(id));
        }
        self.sources.push(Source {
            id,
            layer: name.to_owned(),
            path: None,
        });
        self.documents.push(CapturedDocument {
            source: id,
            content: CapturedContent::Bytes(text.as_bytes().to_vec()),
        });
        Ok(())
    }
    fn check_boundary(&self, path: &Path) -> Result<()> {
        if self
            .boundary
            .as_ref()
            .is_some_and(|root| !path.starts_with(root))
        {
            return Err(Error::new(ErrorKind::Boundary));
        }
        Ok(())
    }
    fn canonical(&self, path: &Path) -> Result<PathBuf> {
        if self.loader.symlinks == SymlinkPolicy::Reject {
            reject_symlinks(path)?;
        }
        let canonical = fs::canonicalize(path).map_err(io_error)?;
        self.check_boundary(&canonical)?;
        Ok(canonical)
    }
    fn file(&mut self, layer: &str, path: &Path, requirement: Requirement) -> Result<()> {
        let path = self.context.path(path);
        let id = SourceId(self.sources.len());
        let result = self.read_file(layer, &path, requirement, id);
        result.map_err(|error| error.at(id))
    }
    fn read_file(
        &mut self,
        layer: &str,
        path: &Path,
        requirement: Requirement,
        id: SourceId,
    ) -> Result<()> {
        let canonical = match self.canonical(path) {
            Err(error)
                if error.kind == ErrorKind::Missing && requirement == Requirement::Optional =>
            {
                return Ok(());
            }
            result => result?,
        };
        if self.seen.contains(&canonical) {
            match self.loader.duplicates {
                DuplicatePolicy::Ignore => return Ok(()),
                DuplicatePolicy::Reject => return Err(Error::new(ErrorKind::Duplicate)),
                DuplicatePolicy::Allow => {}
            }
        }
        if self.documents.len() >= self.loader.limits.max_files {
            return Err(Error::new(ErrorKind::Limit));
        }
        let remaining = self
            .loader
            .limits
            .max_total_bytes
            .saturating_sub(self.total);
        let max = self.loader.limits.max_file_bytes.min(remaining);
        let bytes = read_regular(path, &canonical, max, self.loader.symlinks)?;
        self.check_boundary(&fs::canonicalize(path).map_err(io_error)?)?;
        self.total += bytes.len();
        self.seen.insert(canonical);
        self.sources.push(Source {
            id,
            layer: layer.to_owned(),
            path: Some(path.to_owned()),
        });
        self.documents.push(CapturedDocument {
            source: id,
            content: CapturedContent::Bytes(bytes),
        });
        Ok(())
    }
    fn entries(&mut self, directory: &Path, requirement: Requirement) -> Result<Vec<fs::DirEntry>> {
        match self.canonical(directory) {
            Err(error)
                if error.kind == ErrorKind::Missing && requirement == Requirement::Optional =>
            {
                return Ok(Vec::new());
            }
            result => {
                result?;
            }
        }
        let mut entries = Vec::new();
        for entry in fs::read_dir(directory).map_err(io_error)? {
            self.entries = self
                .entries
                .checked_add(1)
                .ok_or(Error::new(ErrorKind::Limit))?;
            if self.entries > self.loader.limits.max_entries {
                return Err(Error::new(ErrorKind::Limit));
            }
            entries.push(entry.map_err(io_error)?);
        }
        entries.sort_by_key(|entry| entry.file_name());
        Ok(entries)
    }
    fn ancestor_search(
        &mut self,
        name: &str,
        start: Option<&Path>,
        file_name: &str,
        max_depth: usize,
        policy: &SearchPolicy,
    ) -> Result<()> {
        let start = self.canonical(&self.context.path(start.unwrap_or(&self.context.cwd)))?;
        let stop = match &policy.boundary {
            SearchBoundary::Directory(path) => Some(self.canonical(&self.context.path(path))?),
            _ => None,
        };
        if stop.as_ref().is_some_and(|stop| !start.starts_with(stop)) {
            return Err(Error::new(ErrorKind::Boundary));
        }
        let mut matches = Vec::new();
        let mut directory = Some(start.as_path());
        for _ in 0..=max_depth {
            let Some(path) = directory else { break };
            let canonical = self.canonical(path)?;
            let candidate = path.join(file_name);
            match self.canonical(&candidate) {
                Ok(_) => {
                    if policy.matches == SearchMatches::Nearest {
                        // Stop discovery immediately; a corrupt or nonregular nearest
                        // match fails rather than silently falling back to an ancestor.
                        return self.file(name, &candidate, Requirement::Required);
                    }
                    matches.push(candidate);
                }
                Err(error) if error.kind == ErrorKind::Missing => {}
                Err(error) => return Err(error),
            }
            let marker_stop = if let SearchBoundary::Marker(marker) = &policy.boundary {
                self.entries = self
                    .entries
                    .checked_add(1)
                    .ok_or(Error::new(ErrorKind::Limit))?;
                if self.entries > self.loader.limits.max_entries {
                    return Err(Error::new(ErrorKind::Limit));
                }
                match fs::symlink_metadata(path.join(marker)) {
                    Ok(_) => true,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                    Err(error) => return Err(io_error(error)),
                }
            } else {
                false
            };
            if marker_stop
                || stop.as_deref() == Some(path)
                || self.boundary.as_ref() == Some(&canonical)
            {
                break;
            }
            directory = path.parent();
        }
        if policy.order == SearchOrder::OuterFirst {
            matches.reverse();
        }
        for path in matches {
            self.file(name, &path, Requirement::Required)?;
        }
        Ok(())
    }
    fn layer(&mut self, name: &str, layer: &Layer) -> Result<()> {
        match layer {
            Layer::AncestorSearch {
                start,
                file_name,
                max_depth,
                policy,
            } => {
                self.ancestor_search(name, start.as_deref(), file_name, *max_depth, policy)?;
            }
            Layer::Inline(value) | Layer::Value(value) => {
                let id = SourceId(self.sources.len());
                crate::value::validate_value(
                    value,
                    self.loader.limits.max_value_depth,
                    self.loader.limits.max_value_nodes,
                )
                .map_err(|error| error.at(id))?;
                if self.documents.len() >= self.loader.limits.max_files {
                    return Err(Error::new(ErrorKind::Limit).at(id));
                }
                self.sources.push(Source {
                    id,
                    layer: name.to_owned(),
                    path: None,
                });
                self.documents.push(CapturedDocument {
                    source: id,
                    content: CapturedContent::Inline(value.clone()),
                });
            }
            Layer::Text(text) => self.text(name, text)?,
            Layer::EnvironmentText { variable } => {
                if let Some(text) = self.context.environment.get(variable) {
                    self.text(name, text)?;
                }
            }
            Layer::EnvironmentFile {
                variable,
                requirement,
            } => {
                if let Some(path) = self.context.environment.get(variable) {
                    self.file(name, Path::new(path), *requirement)?;
                }
            }
            Layer::File { path, requirement } => self.file(name, path, *requirement)?,
            Layer::UserXdg {
                application,
                file_name,
            } => {
                let base = self
                    .context
                    .environment
                    .get("XDG_CONFIG_HOME")
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from);
                let base = if let Some(base) = base.filter(|base| base.is_absolute()) {
                    base
                } else {
                    self.context
                        .environment
                        .get("HOME")
                        .map(PathBuf::from)
                        .filter(|base| base.is_absolute())
                        .ok_or(Error::new(ErrorKind::InvalidContext))?
                        .join(".config")
                };
                self.file(
                    name,
                    &base.join(application).join(file_name),
                    Requirement::Optional,
                )?;
            }
            Layer::Ancestors {
                start,
                file_name,
                boundary,
                max_depth,
            } => {
                let start = self.canonical(
                    &self
                        .context
                        .path(start.as_deref().unwrap_or(&self.context.cwd)),
                )?;
                let stop = boundary
                    .as_ref()
                    .map(|path| self.canonical(&self.context.path(path)))
                    .transpose()?;
                if stop.as_ref().is_some_and(|stop| !start.starts_with(stop)) {
                    return Err(Error::new(ErrorKind::Boundary));
                }
                let mut directories = Vec::new();
                let mut directory = Some(start.as_path());
                for _ in 0..=*max_depth {
                    let Some(path) = directory else { break };
                    self.canonical(path)?;
                    directories.push(path.to_owned());
                    if stop.as_deref() == Some(path)
                        || self.boundary.as_ref().is_some_and(|root| {
                            fs::canonicalize(path).is_ok_and(|path| path == *root)
                        })
                    {
                        break;
                    }
                    directory = path.parent();
                }
                for directory in directories.into_iter().rev() {
                    self.file(name, &directory.join(file_name), Requirement::Optional)?;
                }
            }
            Layer::Descendants {
                root,
                file_name,
                max_depth,
            } => {
                let mut stack = vec![(self.context.path(root), 0)];
                let mut visited = BTreeSet::new();
                while let Some((directory, depth)) = stack.pop() {
                    let canonical = self.canonical(&directory)?;
                    if !visited.insert(canonical) {
                        continue;
                    }
                    self.file(name, &directory.join(file_name), Requirement::Optional)?;
                    if depth < *max_depth {
                        let entries = self.entries(&directory, Requirement::Required)?;
                        for entry in entries.into_iter().rev() {
                            if entry.file_type().map_err(io_error)?.is_dir() {
                                stack.push((entry.path(), depth + 1));
                            }
                        }
                    }
                }
            }
            Layer::Fragments {
                directory,
                extension,
                requirement,
            } => {
                let directory = self.context.path(directory);
                for entry in self.entries(&directory, *requirement)? {
                    if entry
                        .path()
                        .extension()
                        .is_some_and(|ext| ext == extension.as_str())
                    {
                        // A matching directory is not a fragment; symlink files use file policy.
                        if !entry.file_type().map_err(io_error)?.is_dir() {
                            self.file(name, &entry.path(), Requirement::Required)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
/// Capture one absolute file without process context reads or lexical path folding.
/// Missing files are explicit errors; optional-file semantics belong to the caller.
/// Contents are returned intentionally and must not be logged as diagnostics.
pub fn capture_file(path: &Path, max_bytes: usize) -> Result<Vec<u8>> {
    if !path.is_absolute() {
        return Err(Error::new(ErrorKind::InvalidContext));
    }
    let canonical = fs::canonicalize(path).map_err(io_error)?;
    read_regular(path, &canonical, max_bytes, SymlinkPolicy::Follow)
}
fn read_regular(
    path: &Path,
    canonical: &Path,
    max: usize,
    policy: SymlinkPolicy,
) -> Result<Vec<u8>> {
    let expected = fs::metadata(canonical).map_err(io_error)?;
    if !expected.is_file() {
        return Err(Error::new(ErrorKind::NotRegular));
    }
    let file = safe_open(
        if policy == SymlinkPolicy::Follow {
            canonical
        } else {
            path
        },
        policy,
    )?;
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_file() {
        return Err(Error::new(ErrorKind::NotRegular));
    }
    let checked = fs::metadata(canonical).map_err(io_error)?;
    if !same_file(&metadata, &checked) || !same_file(&metadata, &expected) {
        return Err(Error::new(ErrorKind::Io));
    }
    if metadata.len() > max as u64 {
        return Err(Error::new(ErrorKind::Limit));
    }
    let mut bytes = Vec::new();
    file.take((max as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() > max {
        return Err(Error::new(ErrorKind::Limit));
    }
    Ok(bytes)
}
fn single_component(value: &str) -> Result<()> {
    let mut components = Path::new(value).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(Error::new(ErrorKind::InvalidPolicy));
    }
    Ok(())
}
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        // Preserve parent components until filesystem resolution: lexical folding
        // before following a symlink changes which file the user requested.
        if component != Component::CurDir {
            out.push(component);
        }
    }
    out
}
fn io_error(error: std::io::Error) -> Error {
    Error::new(if error.kind() == std::io::ErrorKind::NotFound {
        ErrorKind::Missing
    } else {
        ErrorKind::Io
    })
}
fn reject_symlinks(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for part in path.components() {
        current.push(part);
        if fs::symlink_metadata(&current)
            .map_err(io_error)?
            .file_type()
            .is_symlink()
        {
            return Err(Error::new(ErrorKind::Symlink));
        }
    }
    Ok(())
}
#[cfg(unix)]
fn safe_open(path: &Path, _: SymlinkPolicy) -> Result<File> {
    // Follow callers supply the canonical target. Walk it without following any
    // newly introduced links, closing the canonicalize/open symlink race.
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    let mut directory: OwnedFd = File::open("/").map_err(io_error)?.into();
    let parts: Vec<_> = path
        .components()
        .filter_map(|part| {
            if matches!(part, Component::Normal(_) | Component::ParentDir) {
                Some(part.as_os_str())
            } else {
                None
            }
        })
        .collect();
    if parts.is_empty() {
        return Err(Error::new(ErrorKind::NotRegular));
    }
    for (index, part) in parts.iter().enumerate() {
        let name =
            CString::new(part.as_bytes()).map_err(|_| Error::new(ErrorKind::InvalidPolicy))?;
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | libc::O_NOFOLLOW
            | if index + 1 < parts.len() {
                libc::O_DIRECTORY
            } else {
                0
            };
        // SAFETY: name is NUL terminated; directory owns a live descriptor;
        // successful openat returns a new descriptor owned exclusively below.
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(io_error(std::io::Error::last_os_error()));
        }
        // SAFETY: fd was just returned by openat and is not owned elsewhere.
        directory = unsafe { OwnedFd::from_raw_fd(fd) };
    }
    Ok(File::from(directory))
}
#[cfg(not(unix))]
fn safe_open(path: &Path, _: SymlinkPolicy) -> Result<File> {
    // No portable nonblocking open is available. Do not risk blocking on special files.
    if !fs::metadata(path).map_err(io_error)?.is_file() {
        return Err(Error::new(ErrorKind::NotRegular));
    }
    std::fs::OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(io_error)
}
#[cfg(unix)]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}
#[cfg(not(unix))]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.len() == right.len() && left.is_file() == right.is_file()
}

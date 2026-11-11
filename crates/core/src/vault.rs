//! The [`Vault`] model: a directory tree of notes plus the derived link and
//! tag indexes, with the desktop app's exact link-resolution algorithm.

use crate::CONFIG_DIR;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::link::parse_linktext;
use crate::note::Note;
use crate::path;
use crate::rewrite;
use crate::Wikilink;

/// Errors that can occur while opening or resolving a vault.
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("failed to read directory {0}: {1}")]
    Io(String, #[source] std::io::Error),
    #[error("not a directory: {0}")]
    NotADir(String),
    #[error("destination file already exists: {0}")]
    Exists(String),
    #[error("note not found: {0}")]
    NotFound(String),
}

/// How new links are written when a note is renamed/moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinkFormat {
    /// Use the note's basename when unique, otherwise the full path.
    #[default]
    Shortest,
    /// Always use a path relative to the referencing note.
    Relative,
    /// Always use the vault-relative path.
    Absolute,
}

/// Reference/property edits produced by a rename, keyed by the resulting path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceEdits {
    pub path: String,
    pub edits: Vec<crate::rewrite::TextEdit>,
    /// Saved text read at application time, before these edits.
    pub before: String,
}

/// Captured link changes after a successful physical rename.
/// Discarding this plan keeps the renamed file and leaves references unchanged.
#[derive(Debug)]
pub struct RenamePlan {
    hidden_moved_path: Option<String>,
    rewrites: Vec<Rewrite>,
}
impl RenamePlan {
    pub fn affected_references(&self) -> usize {
        self.rewrites.len()
    }
}

/// A Markdown vault.
///
/// Notes are keyed by their path relative to the vault root, using `/`
/// separators. The `backlinks` and `tags` maps are derived on open.
#[derive(Debug, Clone)]
pub struct Vault {
    pub root: PathBuf,
    pub notes: BTreeMap<String, Note>,
    /// `target path -> [source path, ...]`.
    pub backlinks: BTreeMap<String, Vec<String>>,
    /// `tag name -> [note path, ...]` (tags include the leading `#`).
    pub tags: BTreeMap<String, Vec<String>>,
    /// The link format used when rewriting links on rename.
    pub new_link_format: LinkFormat,
    /// Lowercased basename -> note keys sharing that basename.
    unique: BTreeMap<String, Vec<String>>,
    /// Lowercased frontmatter alias -> note path.
    aliases: BTreeMap<String, String>,
    /// All non-hidden file paths in the vault (markdown and attachments).
    files: BTreeSet<String>,
}

impl Vault {
    /// Construct the unloaded inventory used by the application startup loader.
    pub fn empty(root: impl AsRef<Path>) -> Result<Self, VaultError> {
        let root = root.as_ref().to_path_buf();
        if !root.is_dir() {
            return Err(VaultError::NotADir(root.display().to_string()));
        }
        Ok(Self {
            root,
            notes: BTreeMap::new(),
            backlinks: BTreeMap::new(),
            tags: BTreeMap::new(),
            new_link_format: LinkFormat::default(),
            unique: BTreeMap::new(),
            aliases: BTreeMap::new(),
            files: BTreeSet::new(),
        })
    }
    /// Opens a vault by walking the directory tree and parsing every `.md`
    /// file. Hidden files and directories (the configuration directory, `.git`, etc.) are
    /// skipped.
    pub fn open(root: impl AsRef<Path>) -> Result<Vault, VaultError> {
        let root = root.as_ref().to_path_buf();
        if !root.is_dir() {
            return Err(VaultError::NotADir(root.display().to_string()));
        }

        let mut notes = BTreeMap::new();
        let mut files = BTreeSet::new();
        walk(&root, &root, &mut notes, &mut files)?;

        let Indexes {
            unique,
            backlinks,
            tags,
            aliases,
        } = build_indexes(&notes, &files);

        let new_link_format = std::fs::read_to_string(root.join(CONFIG_DIR).join("app.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|settings| {
                settings
                    .get("newLinkFormat")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
            })
            .map(|format| match format.as_str() {
                "absolute" => LinkFormat::Absolute,
                "relative" => LinkFormat::Relative,
                _ => LinkFormat::Shortest,
            })
            .unwrap_or_default();

        Ok(Vault {
            root,
            notes,
            backlinks,
            tags,
            new_link_format,
            unique,
            aliases,
            files,
        })
    }

    /// Returns the note at the given vault-relative path.
    pub fn get(&self, path: &str) -> Option<&Note> {
        self.notes.get(path)
    }

    /// Finds a note by title (file name without extension).
    pub fn get_by_title(&self, title: &str) -> Option<&Note> {
        self.notes
            .values()
            .find(|n| n.title.eq_ignore_ascii_case(title))
    }

    /// Whether the vault contains a file (markdown or attachment) at the given
    /// relative path.
    pub fn has_file(&self, path: &str) -> bool {
        self.files.contains(path)
    }

    /// All non-hidden file paths in the vault.
    pub fn files(&self) -> &BTreeSet<String> {
        &self.files
    }

    /// Reconcile one actual filesystem notification without writing bytes or
    /// rescanning unrelated paths. Reads complete before the live index changes.
    pub fn reconcile_path_from_disk(&mut self, path: &str) -> Result<(), VaultError> {
        self.reconcile_path_from_disk_inner(path, true)
    }
    /// Incremental initial inventory publication, finalized once after loading.
    pub fn load_startup_path_from_disk(&mut self, path: &str) -> Result<(), VaultError> {
        self.reconcile_path_from_disk_inner(path, false)
    }
    pub fn finish_startup_indexes(&mut self) {
        self.rebuild_indexes();
    }
    fn reconcile_path_from_disk_inner(
        &mut self,
        path: &str,
        rebuild: bool,
    ) -> Result<(), VaultError> {
        if std::path::Path::new(path).components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        }) {
            return Err(VaultError::NotFound(path.into()));
        }
        if path::has_hidden_ancestor(path) {
            return Ok(());
        }
        let absolute = self.root.join(path);
        let stat = match std::fs::symlink_metadata(&absolute) {
            Ok(stat) => Some(stat),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(VaultError::Io(absolute.display().to_string(), error)),
        };
        let note =
            if stat.as_ref().is_some_and(|stat| stat.is_file()) && path::extension(path) == "md" {
                let bytes = std::fs::read(&absolute)
                    .map_err(|error| VaultError::Io(absolute.display().to_string(), error))?;
                Some(Note::parse(
                    PathBuf::from(path),
                    String::from_utf8_lossy(&bytes).into_owned(),
                ))
            } else {
                None
            };
        if stat.as_ref().is_some_and(|stat| stat.is_file()) {
            self.files.insert(path.to_owned());
        } else {
            self.files.remove(path);
        }
        self.notes.remove(path);
        if let Some(note) = note {
            self.notes.insert(path.to_owned(), note);
        }
        if rebuild {
            self.rebuild_indexes();
        }
        Ok(())
    }

    /// Resolves a wikilink target to the best note path
    /// (`getFirstLinkpathDest`).
    pub fn resolve(&self, from: &Note, link: &Wikilink) -> Option<String> {
        get_linkpath_dest(&self.unique, link.path_part(), &note_key(from))
            .into_iter()
            .next()
    }

    /// Resolves a bare link target string to the best note path.
    pub fn resolve_path(&self, from: &Note, target: &str) -> Option<String> {
        get_linkpath_dest(&self.unique, target, &note_key(from))
            .into_iter()
            .next()
    }

    /// Resolve a cached reference whose source key may outlive the indexed
    /// filesystem file, including raw dot-path keys retained after a rename.
    pub fn resolve_from_path(&self, source: &str, target: &str) -> Option<String> {
        get_linkpath_dest(&self.unique, target, source)
            .into_iter()
            .next()
    }
    /// Preserve all ordered destination identities for retained raw-source
    /// reference updates, including ambiguous filename resolutions.
    pub fn resolve_all_from_path(&self, source: &str, target: &str) -> Vec<String> {
        get_linkpath_dest(&self.unique, target, source)
    }

    /// All resolved note paths for a link (`getLinkpathDest`).
    pub fn resolve_all(&self, from: &Note, target: &str) -> Vec<String> {
        get_linkpath_dest(&self.unique, target, &note_key(from))
    }

    /// Resolves a link target to a single note path, falling back to
    /// frontmatter aliases (the desktop app's `LinkResolver`).
    pub fn resolve_linkpath_alias(&self, from: &Note, linktext: &str) -> Option<String> {
        let n = get_linkpath_dest(&self.unique, linktext, &note_key(from));
        if let Some(first) = n.first() {
            return Some(first.clone());
        }
        let lower = linktext.to_lowercase();
        if let Some(p) = self.aliases.get(&lower) {
            return Some(p.clone());
        }
        let base = path::basename(linktext);
        self.aliases.get(base).cloned()
    }

    /// Notes that link to the given path.
    pub fn backlinks_to(&self, path: &str) -> &[String] {
        self.backlinks.get(path).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Notes that link to the given path, with the number of link instances
    /// from each source (`(source path, occurrences)`).
    pub fn backlink_counts(&self, path: &str) -> Vec<(String, usize)> {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for (src, note) in &self.notes {
            let mut n = 0;
            for target in crate::graph_refs::link_targets(note) {
                if get_linkpath_dest(&self.unique, &target, src)
                    .first()
                    .map(String::as_str)
                    == Some(path)
                {
                    n += 1;
                }
            }
            if n > 0 {
                counts.push((src.clone(), n));
            }
        }
        counts
    }

    /// Notes carrying the given tag.
    pub fn notes_with_tag(&self, tag: &str) -> &[String] {
        self.tags.get(tag).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Files with no incoming links, over all files (markdown and attachments).
    pub fn orphans(&self) -> Vec<String> {
        self.files
            .iter()
            .filter(|f| !self.backlinks.contains_key(*f))
            .cloned()
            .collect()
    }

    /// Files with no outgoing links, over all files. Non-markdown files always
    /// count (they cannot link out).
    pub fn deadends(&self) -> Vec<String> {
        let mut out = Vec::new();
        for f in &self.files {
            match self.notes.get(f) {
                Some(note) => {
                    let has_out = crate::graph_refs::link_targets(note)
                        .iter()
                        .any(|t| self.resolve_path(note, t).is_some());
                    if !has_out {
                        out.push(f.clone());
                    }
                }
                None => out.push(f.clone()),
            }
        }
        out
    }

    /// The plugin API's `getTags()`: the global tag index. Each raw tag has a single
    /// trailing `/` stripped, must pass [`crate::link::is_valid_tag`], and
    /// implies its parent tags (hierarchy expansion).
    pub fn get_tags(&self) -> BTreeMap<String, usize> {
        let mut map = BTreeMap::new();
        for note in self.notes.values() {
            for tag in note.all_tags() {
                add_tag(&mut map, &tag);
            }
        }
        map
    }

    /// Returns an available path for a new note, appending ` 1`, ` 2`, ...
    /// when a file with the same (case-insensitive) path already exists.
    pub fn get_available_path(&self, path: &str, extension: &str) -> String {
        let join = |p: &str| {
            if extension.is_empty() {
                p.to_string()
            } else {
                format!("{p}.{extension}")
            }
        };
        let mut candidate = join(path);
        let mut n = 1;
        while self
            .files
            .iter()
            .any(|k| k.eq_ignore_ascii_case(&candidate))
            || self.root.join(&candidate).is_dir()
        {
            candidate = join(&format!("{path} {n}"));
            n += 1;
        }
        candidate
    }

    /// Creates a new note at `path` (normalized), returning the actual path
    /// used. A missing `.md` extension is appended.
    pub fn create(&mut self, path: &str, content: &str) -> Result<String, VaultError> {
        let normalized = path::normalize_path(path);
        let (stem, ext) = match normalized.rfind('.') {
            Some(i) if i != 0 => (&normalized[..i], normalized[i + 1..].to_string()),
            _ => (normalized.as_str(), "md".to_string()),
        };
        // normalizePath represents an empty path as "/". Appending an
        // extension must pass through normalization again before filesystem
        // joining, just as the adapter normalizes its final file path.
        let candidate = path::normalize_path(&format!("{stem}.{ext}"));
        let (stem, extension) = candidate.rsplit_once('.').unwrap_or((&candidate, ""));
        let path = self.get_available_path(stem, extension);
        let abs = self.root.join(&path);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| VaultError::Io(parent.display().to_string(), e))?;
        }
        std::fs::write(&abs, content).map_err(|e| VaultError::Io(abs.display().to_string(), e))?;
        self.files.insert(path.clone());
        if path::extension(&path) == "md" {
            self.notes.insert(
                path.clone(),
                Note::parse(PathBuf::from(&path), content.to_string()),
            );
        }
        self.rebuild_indexes();
        Ok(path)
    }

    /// Overwrites a note's contents on disk and in memory.
    pub fn modify(&mut self, path: &str, content: &str) -> Result<(), VaultError> {
        if !self.notes.contains_key(path) {
            return Err(VaultError::NotFound(path.to_string()));
        }
        let abs = self.root.join(path);
        std::fs::write(&abs, content).map_err(|e| VaultError::Io(abs.display().to_string(), e))?;
        self.notes.insert(
            path.to_string(),
            Note::parse(PathBuf::from(path), content.to_string()),
        );
        self.rebuild_indexes();
        Ok(())
    }

    /// Renames (or moves) a file, updating all inbound links to the new path.
    pub fn rename(&mut self, old_path: &str, new_path: &str) -> Result<(), VaultError> {
        self.rename_with_edits(old_path, new_path).map(|_| ())
    }

    /// Returns exact reference/property source edits for retained editors.
    pub fn rename_with_edits(
        &mut self,
        old_path: &str,
        new_path: &str,
    ) -> Result<Vec<SourceEdits>, VaultError> {
        let plan = self.rename_and_plan(old_path, new_path)?;
        let mut loaded = self.files.clone();
        if let Some(path) = &plan.hidden_moved_path {
            loaded.insert(path.clone());
        }
        self.apply_rename_plan_with_loaded_files(plan, &loaded)
    }

    /// Capture references, perform the physical rename, then plan link changes.
    /// Applying those changes is a separate FileManager decision boundary.
    pub fn rename_and_plan(
        &mut self,
        old_path: &str,
        new_path: &str,
    ) -> Result<RenamePlan, VaultError> {
        if !self.files.contains(old_path) {
            return Err(VaultError::NotFound(old_path.to_string()));
        }
        let new_path = path::normalize_path(new_path);
        if old_path == new_path {
            return Ok(RenamePlan {
                hidden_moved_path: None,
                rewrites: Vec::new(),
            });
        }
        let new_lower = new_path.to_lowercase();
        if (self.files.iter().any(|k| k.to_lowercase() == new_lower)
            || self.root.join(new_path.trim_start_matches('/')).exists())
            && old_path.to_lowercase() != new_lower
        {
            return Err(VaultError::Exists(new_path));
        }

        // Capture every resolved reference before changing either endpoint.
        // Moving a source can change its outgoing links, and renaming a file
        // can change the resolution of otherwise unrelated ambiguous links.
        let mut refs = Vec::new();
        for (source_path, note) in &self.notes {
            // FileManager iterates successful metadata jobs. A worker failure
            // leaves the file searchable but supplies no rewrite references.
            if crate::cache::CachedMetadata::try_from_note(note).is_err() {
                continue;
            }
            let references = note
                .references()
                .into_iter()
                .map(|r| {
                    (
                        r.original,
                        r.link,
                        RewriteLocation::Body {
                            start: note.source[..note.body_offset + r.start]
                                .encode_utf16()
                                .count()
                                - usize::from(note.source.starts_with('\u{feff}')),
                            end: note.source[..note.body_offset + r.end]
                                .encode_utf16()
                                .count()
                                - usize::from(note.source.starts_with('\u{feff}')),
                        },
                    )
                })
                .chain(note.frontmatter_links.iter().map(|r| {
                    (
                        r.original.clone(),
                        r.link.clone(),
                        RewriteLocation::Property(
                            r.key_units
                                .clone()
                                .unwrap_or_else(|| r.key.encode_utf16().collect()),
                        ),
                    )
                }));
            for (original, link, location) in references {
                if !crate::link::is_internal_link(&link) {
                    continue;
                }
                let lt = parse_linktext(&link);
                if lt.path.is_empty() {
                    continue;
                }
                let target = get_linkpath_dest(&self.unique, &lt.path, source_path);
                if !target.is_empty() {
                    refs.push((source_path.clone(), original, link, location, target));
                }
            }
        }

        let old_abs = self.root.join(old_path);
        let new_abs = self.root.join(new_path.trim_start_matches('/'));
        std::fs::rename(&old_abs, &new_abs)
            .map_err(|e| VaultError::Io(old_abs.display().to_string(), e))?;

        if let Some(mut note) = self.notes.remove(old_path) {
            note.path = PathBuf::from(&new_path);
            note.title = path::basename_no_md(&new_path);
            self.notes.insert(new_path.clone(), note);
        }
        if path::extension(&new_path) == "md" && !self.notes.contains_key(&new_path) {
            let bytes = std::fs::read(&new_abs)
                .map_err(|e| VaultError::Io(new_abs.display().to_string(), e))?;
            let source = String::from_utf8_lossy(&bytes).into_owned();
            self.notes.insert(
                new_path.clone(),
                Note::parse(PathBuf::from(&new_path), source),
            );
        }
        self.files.remove(old_path);
        self.files.insert(new_path.clone());
        self.rebuild_indexes();

        // Phase 2: compute new link text against the updated index.
        let mut rewrites = Vec::new();
        for (old_source_path, original, link, location, old_targets) in refs {
            let source_path = if old_source_path == old_path {
                &new_path
            } else {
                &old_source_path
            };
            let lt = parse_linktext(&link);
            let current_targets = get_linkpath_dest(&self.unique, &lt.path, source_path);
            if old_targets == current_targets {
                continue;
            }
            let target_path = if old_targets[0] == old_path {
                &new_path
            } else {
                &old_targets[0]
            };
            let is_wikilink = original.starts_with("[[") || original.starts_with("![[");
            let base = self.file_to_linktext(target_path, source_path, is_wikilink);
            let new_linktext = format!("{base}{}", lt.subpath);
            if new_linktext == link {
                continue;
            }
            let new_text = rewrite::rewrite_link(&original, &link, &new_linktext);
            rewrites.push(Rewrite {
                source: source_path.clone(),
                location,
                new_text,
            });
        }
        if path::extension(&new_path) != "md" || path::has_hidden_ancestor(&new_path) {
            self.notes.remove(&new_path);
        }
        if path::has_hidden_ancestor(&new_path) {
            self.files.remove(&new_path);
        }
        self.rebuild_indexes();
        Ok(RenamePlan {
            hidden_moved_path: path::has_hidden_ancestor(&new_path).then_some(new_path),
            rewrites,
        })
    }

    /// Apply captured native coordinates to the current saved bytes.
    pub fn apply_rename_plan(&mut self, plan: RenamePlan) -> Result<Vec<SourceEdits>, VaultError> {
        self.apply_rename_plan_with_loaded_files(plan, &self.files.clone())
    }

    /// The retained owner supplies its current loaded TFile paths, including
    /// adapter raw identities not present in the canonical parsed inventory.
    pub fn apply_rename_plan_with_loaded_files(
        &mut self,
        plan: RenamePlan,
        loaded: &BTreeSet<String>,
    ) -> Result<Vec<SourceEdits>, VaultError> {
        let result = self.apply_rewrites(plan.rewrites, loaded);
        self.rebuild_indexes();
        result
    }

    /// Deletes a file permanently, preserving its parent folders.
    pub fn delete(&mut self, path: &str) -> Result<(), VaultError> {
        if !self.files.contains(path) {
            return Err(VaultError::NotFound(path.to_string()));
        }
        let abs = self.root.join(path);
        std::fs::remove_file(&abs).map_err(|e| VaultError::Io(abs.display().to_string(), e))?;
        self.notes.remove(path);
        self.files.remove(path);
        self.rebuild_indexes();
        Ok(())
    }

    /// Moves a file into the vault's `.trash` folder. Inbound links are left
    /// untouched (they become unresolved), matching the desktop app.
    pub fn trash(&mut self, path: &str) -> Result<(), VaultError> {
        if !self.files.contains(path) {
            return Err(VaultError::NotFound(path.to_string()));
        }
        let name = path::basename(path);
        let (stem, ext) = match name.rfind('.') {
            Some(i) if i != 0 => (&name[..i], &name[i..]),
            _ => (name, ""),
        };
        let trash_dir = self.root.join(".trash");
        std::fs::create_dir_all(&trash_dir)
            .map_err(|e| VaultError::Io(trash_dir.display().to_string(), e))?;
        let mut new_abs = trash_dir.join(name);
        let mut n = 1;
        // Trash is hidden from the vault index. The desktop adapter
        // checks the filesystem and starts collision suffixes at 2.
        while new_abs.exists() {
            n += 1;
            new_abs = trash_dir.join(format!("{stem} {n}{ext}"));
        }

        let old_abs = self.root.join(path);
        std::fs::rename(&old_abs, &new_abs)
            .map_err(|e| VaultError::Io(old_abs.display().to_string(), e))?;
        self.notes.remove(path);
        self.files.remove(path);
        self.rebuild_indexes();
        Ok(())
    }

    /// The native CLI requests system trash independently of the configured
    /// UI deletion preference. A false adapter result falls back to local trash.
    /// The adapter must only return true after completing the filesystem move.
    pub fn trash_with_system(
        &mut self,
        path: &str,
        system_trash: impl FnOnce(&Path) -> bool,
    ) -> Result<(), VaultError> {
        if !self.files.contains(path) {
            return Err(VaultError::NotFound(path.to_owned()));
        }
        if system_trash(&self.root.join(path)) {
            self.notes.remove(path);
            self.files.remove(path);
            self.rebuild_indexes();
            Ok(())
        } else {
            self.trash(path)
        }
    }

    /// Rebuilds the derived indexes after a mutation.
    fn rebuild_indexes(&mut self) {
        let Indexes {
            unique,
            backlinks,
            tags,
            aliases,
        } = build_indexes(&self.notes, &self.files);
        self.unique = unique;
        self.backlinks = backlinks;
        self.tags = tags;
        self.aliases = aliases;
    }

    /// The plugin API's `fileToLinktext`: how `new_path` is linked from `source_path`.
    pub fn file_to_linktext(&self, new_path: &str, source_path: &str, strip_md: bool) -> String {
        let r = if strip_md {
            path::strip_md_extension(new_path)
        } else {
            new_path.to_string()
        };
        match self.new_link_format {
            LinkFormat::Absolute => r,
            LinkFormat::Relative => {
                let mut o = String::new();
                let mut a = path::parent(source_path).to_string();
                while !a.is_empty() && a != "/" && !r.starts_with(&format!("{a}/")) {
                    o.push_str("../");
                    a = path::parent(&a).to_string();
                }
                if r.starts_with(&format!("{a}/")) {
                    format!("{o}{}", &r[a.len() + 1..])
                } else {
                    format!("{o}{r}")
                }
            }
            LinkFormat::Shortest => {
                let s = if strip_md {
                    path::basename_no_md(new_path)
                } else {
                    path::basename(new_path).to_string()
                };
                let dest = get_linkpath_dest(&self.unique, &s, source_path);
                if dest.len() == 1 && dest[0] == new_path {
                    s
                } else {
                    r
                }
            }
        }
    }

    fn apply_rewrites(
        &mut self,
        rewrites: Vec<Rewrite>,
        loaded: &BTreeSet<String>,
    ) -> Result<Vec<SourceEdits>, VaultError> {
        // Native metadata parses a BOM-stripped string, whereas zR patches the
        // raw adapter string at those UTF-16 offsets. Expand partial surrogate
        // boundaries to valid UTF-8 edits, using Node's replacement encoding.
        fn raw_utf16_edit(
            source: &str,
            start: usize,
            end: usize,
            text: &str,
        ) -> crate::rewrite::TextEdit {
            let mut start_byte = source.len();
            let mut end_byte = source.len();
            let mut before = Vec::new();
            let mut after = Vec::new();
            let mut offset = 0;
            for (byte, ch) in source.char_indices() {
                let mut storage = [0; 2];
                let units = ch.encode_utf16(&mut storage);
                if start >= offset && start < offset + units.len() {
                    start_byte = byte;
                    before.extend_from_slice(&units[..start - offset]);
                }
                if end >= offset && end < offset + units.len() {
                    if end == offset {
                        end_byte = byte;
                    } else {
                        end_byte = byte + ch.len_utf8();
                        after.extend_from_slice(&units[end - offset..]);
                    }
                }
                offset += units.len();
            }
            before.extend(text.encode_utf16());
            before.extend(after);
            crate::rewrite::TextEdit {
                start: start_byte,
                end: end_byte,
                text: String::from_utf16_lossy(&before),
            }
        }
        let mut by_source: BTreeMap<String, Vec<(RewriteLocation, String)>> = BTreeMap::new();
        for r in rewrites {
            by_source
                .entry(r.source)
                .or_default()
                .push((r.location, r.new_text));
        }
        let mut source_edits = Vec::new();
        for (source, mut edits) in by_source {
            edits.sort_by_key(|e| {
                std::cmp::Reverse(match e.0 {
                    RewriteLocation::Body { start, .. } => start,
                    RewriteLocation::Property(_) => 0,
                })
            });
            if !loaded.contains(&source) {
                continue;
            }
            let abs = self.root.join(&source);
            let bytes =
                std::fs::read(&abs).map_err(|e| VaultError::Io(abs.display().to_string(), e))?;
            let before = String::from_utf8_lossy(&bytes).into_owned();
            let mut updated = before.clone();
            let mut record = SourceEdits {
                path: source.clone(),
                edits: Vec::new(),
                before: before.clone(),
            };
            let mut properties = Vec::new();
            for (location, new_text) in edits {
                match location {
                    RewriteLocation::Body { start, end } => {
                        let edit = raw_utf16_edit(&updated, start, end, &new_text);
                        let trimmed = if before.starts_with('\u{feff}') {
                            Some(edit.clone())
                        } else {
                            crate::rewrite::TextEdit::within_span(
                                edit.start,
                                &updated[edit.start..edit.end],
                                &edit.text,
                            )
                        };
                        updated.replace_range(edit.start..edit.end, &edit.text);
                        if let Some(edit) = trimmed {
                            record.edits.push(edit);
                        }
                    }
                    RewriteLocation::Property(key) => properties.push((key, new_text)),
                }
            }
            // Native frontmatter processing sees current text after body splices;
            // a leading BOM prevents this separate property pass.
            if !properties.is_empty() && !updated.starts_with('\u{feff}') {
                if let Some(bounds) = crate::frontmatter::processing_bounds(&updated) {
                    if let Some(yaml) = crate::frontmatter::rewrite_links_units(
                        &updated[bounds.from..bounds.to],
                        &properties,
                    ) {
                        if let Some(edit) = crate::rewrite::TextEdit::within_span(
                            bounds.from,
                            &updated[bounds.from..bounds.to],
                            &yaml,
                        ) {
                            record.edits.push(edit);
                        }
                        updated.replace_range(bounds.from..bounds.to, &yaml);
                    }
                }
            }
            record.edits.sort_by_key(|edit| edit.start);
            // Verify editor edits against the actual current saved source.
            // Stale overlapping/header spans may need a single valid change.
            let mut replay = before.clone();
            let mut valid = true;
            let mut next_start = before.len();
            for edit in record.edits.iter().rev() {
                if edit.end > next_start
                    || edit.start > edit.end
                    || !replay.is_char_boundary(edit.start)
                    || !replay.is_char_boundary(edit.end)
                {
                    valid = false;
                    break;
                }
                replay.replace_range(edit.start..edit.end, &edit.text);
                next_start = edit.start;
            }
            if !valid || replay != updated {
                record.edits = crate::rewrite::TextEdit::within_span(0, &before, &updated)
                    .into_iter()
                    .collect();
            }
            source_edits.push(record);
            std::fs::write(&abs, &updated)
                .map_err(|e| VaultError::Io(abs.display().to_string(), e))?;
            if self.files.contains(&source) && path::extension(&source) == "md" {
                self.notes
                    .insert(source.clone(), Note::parse(PathBuf::from(&source), updated));
            }
        }
        Ok(source_edits)
    }
}

#[derive(Debug)]
struct Rewrite {
    source: String,
    location: RewriteLocation,
    new_text: String,
}

#[derive(Debug)]
enum RewriteLocation {
    Body { start: usize, end: usize },
    Property(Vec<u16>),
}

struct Indexes {
    unique: BTreeMap<String, Vec<String>>,
    backlinks: BTreeMap<String, Vec<String>>,
    tags: BTreeMap<String, Vec<String>>,
    aliases: BTreeMap<String, String>,
}

fn build_indexes(notes: &BTreeMap<String, Note>, files: &BTreeSet<String>) -> Indexes {
    let mut unique: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for key in files {
        unique
            .entry(path::basename(key).to_lowercase())
            .or_default()
            .push(key.clone());
    }

    let mut aliases: BTreeMap<String, String> = BTreeMap::new();
    for (key, note) in notes {
        if let Some(fm) = &note.frontmatter {
            for a in fm.aliases() {
                aliases.entry(a.to_lowercase()).or_insert(key.clone());
            }
        }
    }

    let mut backlinks: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut tags: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for (key, note) in notes {
        for tag in note.all_tags() {
            tags.entry(tag).or_default().push(key.clone());
        }
    }

    for (key, note) in notes {
        for target in crate::graph_refs::link_targets(note) {
            if let Some(dest) = get_linkpath_dest(&unique, &target, key).first() {
                let entry = backlinks.entry(dest.clone()).or_default();
                if !entry.contains(key) {
                    entry.push(key.clone());
                }
            }
        }
    }

    Indexes {
        unique,
        backlinks,
        tags,
        aliases,
    }
}

fn add_tag(map: &mut BTreeMap<String, usize>, tag: &str) {
    let tag = tag.strip_suffix('/').unwrap_or(tag);
    if !crate::link::is_valid_tag(tag) {
        return;
    }
    *map.entry(tag.to_string()).or_default() += 1;
    if let Some(i) = tag.rfind('/') {
        add_tag(map, &tag[..i]);
    }
}

fn note_key(note: &Note) -> String {
    note.path.to_string_lossy().replace('\\', "/")
}

fn walk(
    root: &Path,
    dir: &Path,
    notes: &mut BTreeMap<String, Note>,
    files: &mut BTreeSet<String>,
) -> Result<(), VaultError> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| VaultError::Io(dir.display().to_string(), e))?;

    for entry in entries {
        let entry = entry.map_err(|e| VaultError::Io(dir.display().to_string(), e))?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let ft = entry
            .file_type()
            .map_err(|e| VaultError::Io(path.display().to_string(), e))?;
        if ft.is_dir() {
            walk(root, &path, notes, files)?;
        } else if ft.is_file() {
            let rel = path
                .strip_prefix(root)
                .map_err(|_| VaultError::NotADir(root.display().to_string()))?;
            let key = rel.to_string_lossy().replace('\\', "/");
            files.insert(key.clone());
            if path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
            {
                let bytes = std::fs::read(&path)
                    .map_err(|e| VaultError::Io(path.display().to_string(), e))?;
                let source = String::from_utf8_lossy(&bytes).into_owned();
                notes.insert(key, Note::parse(rel.to_path_buf(), source));
            }
        }
    }

    Ok(())
}

/// The plugin API's `getLinkpathDest`: resolves `linktext` relative to `source_path`
/// against the basename lookup.
fn get_linkpath_dest(
    unique: &BTreeMap<String, Vec<String>>,
    linktext: &str,
    source_path: &str,
) -> Vec<String> {
    if linktext.is_empty() {
        return vec![source_path.to_string()];
    }

    let mut n = linktext.to_lowercase();
    let mut i = path::basename(&n).to_string();
    let mut r: Option<&Vec<String>> = None;
    if i.contains('.') {
        r = unique.get(&i);
    }
    if r.is_none_or(|v| v.is_empty()) {
        n = format!("{linktext}.md").to_lowercase();
        i = path::basename(&n).to_string();
        r = unique.get(&i);
    }
    let r = match r {
        Some(v) if !v.is_empty() => v,
        _ => return Vec::new(),
    };

    if i == n && r.len() == 1 {
        return r.clone();
    }

    let mut o = path::parent(source_path).to_lowercase();

    if n.starts_with("./") || n.starts_with("../") {
        if n.starts_with("./../") {
            n = n[2..].to_string();
        }
        if n.starts_with("./") {
            if !o.is_empty() {
                o.push('/');
            }
            n = format!("{o}{}", &n[2..]);
        } else {
            while n.starts_with("../") {
                n = n[3..].to_string();
                o = path::parent(&o).to_string();
            }
            if !o.is_empty() {
                o.push('/');
            }
            n = format!("{o}{n}");
        }
        for key in r {
            if key.to_lowercase() == n {
                return vec![key.clone()];
            }
        }
    }

    if n.starts_with('/') {
        n = n[1..].to_string();
    }
    for key in r {
        if key.to_lowercase() == n {
            return vec![key.clone()];
        }
    }

    if linktext.starts_with('/') {
        return Vec::new();
    }

    let mut same = Vec::new();
    let mut other = Vec::new();
    for key in r {
        let m = key.to_lowercase();
        if m.ends_with(&n) {
            if m.starts_with(&o) {
                same.push(key.clone());
            } else {
                other.push(key.clone());
            }
        }
    }
    same.sort_by_key(|k| k.len());
    other.sort_by_key(|k| k.len());
    same.extend(other);
    same
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            p.push(format!("mnemosyne-test-{}-{}", tag, std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }

        fn write(&self, rel: &str, contents: &str) {
            let p = self.0.join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(p, contents).unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn opens_vault_and_indexes() {
        let t = TempDir::new("open");
        t.write("Home.md", "# Home\n\n[[Alpha]] [[Missing]]\n");
        t.write(
            "notes/Alpha.md",
            "---\ntags: [guide]\n---\n#tag\n\nsee [[Home]]\n",
        );
        t.write(&format!("{CONFIG_DIR}/app.json"), "{}");

        let vault = Vault::open(&t.0).unwrap();
        assert_eq!(vault.notes.len(), 2);
        assert!(!vault.notes.contains_key(&format!("{CONFIG_DIR}/app.json")));

        assert_eq!(vault.backlinks_to("notes/Alpha.md"), &["Home.md"]);
        assert_eq!(vault.backlinks_to("Home.md"), &["notes/Alpha.md"]);
        assert!(vault.backlinks_to("Missing").is_empty());

        let alpha_tags = vault.notes["notes/Alpha.md"].all_tags();
        assert!(alpha_tags.contains(&"#guide".to_string()));
        assert!(alpha_tags.contains(&"#tag".to_string()));
        assert_eq!(vault.notes_with_tag("#guide"), &["notes/Alpha.md"]);
    }

    #[test]
    fn resolves_links_by_shortest_path() {
        let t = TempDir::new("resolve");
        t.write("Home.md", "[[Alpha]] [[unique]]\n");
        t.write("notes/Alpha.md", "[[Beta]]\n");
        t.write("notes/Beta.md", "[[Home]]\n");
        t.write("deep/nested/Unique.md", "[[Home]]\n");

        let vault = Vault::open(&t.0).unwrap();
        let home = vault.get("Home.md").unwrap();

        let alpha = vault.get("notes/Alpha.md").unwrap();
        assert_eq!(
            vault.resolve_path(alpha, "Beta").as_deref(),
            Some("notes/Beta.md")
        );
        assert_eq!(
            vault.resolve_path(alpha, "Home").as_deref(),
            Some("Home.md")
        );
        // Unique case-insensitive basename match across the vault.
        assert_eq!(
            vault.resolve_path(home, "unique").as_deref(),
            Some("deep/nested/Unique.md")
        );
        assert!(vault.resolve_path(home, "does-not-exist").is_none());
    }

    #[test]
    fn ambiguous_basename_resolves_to_shortest_path() {
        // An ambiguous basename resolves to the shortest matching
        // path (then first in path order for ties).
        let t = TempDir::new("ambiguous");
        t.write("a/Note.md", "");
        t.write("b/Note.md", "");
        t.write("c/Link.md", "[[Note]]");
        let vault = Vault::open(&t.0).unwrap();
        let link = vault.get("c/Link.md").unwrap();
        assert_eq!(
            vault.resolve_path(link, "Note").as_deref(),
            Some("a/Note.md")
        );
    }

    #[test]
    fn relative_and_absolute_paths() {
        let t = TempDir::new("rel");
        t.write("Home.md", "");
        t.write("a/Child.md", "[[./Sibling]] [[../Home]] [[/a/Child]]");
        t.write("a/Sibling.md", "");
        let vault = Vault::open(&t.0).unwrap();
        let child = vault.get("a/Child.md").unwrap();
        assert_eq!(
            vault.resolve_path(child, "./Sibling").as_deref(),
            Some("a/Sibling.md")
        );
        assert_eq!(
            vault.resolve_path(child, "../Home").as_deref(),
            Some("Home.md")
        );
        assert_eq!(
            vault.resolve_path(child, "/a/Child").as_deref(),
            Some("a/Child.md")
        );
    }

    #[test]
    fn self_link_resolves() {
        let t = TempDir::new("selflink");
        t.write("a/Note.md", "[[Note]]");
        let vault = Vault::open(&t.0).unwrap();
        let note = vault.get("a/Note.md").unwrap();
        assert_eq!(
            vault.resolve_path(note, "Note").as_deref(),
            Some("a/Note.md")
        );
    }

    #[test]
    fn rename_missing_parent_preserves_source_and_creates_no_folder() {
        let t = TempDir::new("rename-missing-parent");
        t.write("Old.md", "[[Old]]\n");
        let mut vault = Vault::open(&t.0).unwrap();
        assert!(vault.rename("Old.md", "Missing/New.md").is_err());
        assert!(!t.0.join("Missing").exists());
        assert_eq!(fs::read_to_string(t.0.join("Old.md")).unwrap(), "[[Old]]\n");
        assert!(vault.has_file("Old.md"));
    }

    #[test]
    fn rename_extension_rewrites_before_changing_note_classification() {
        let t = TempDir::new("rename-extension-classification");
        t.write("Old.md", "[[Old]]\n");
        t.write("Source.md", "[[Old]]\n");
        let mut vault = Vault::open(&t.0).unwrap();
        vault.rename("Old.md", "New.txt").unwrap();
        assert_eq!(
            fs::read_to_string(t.0.join("New.txt")).unwrap(),
            "[[New.txt]]\n"
        );
        assert_eq!(vault.get("Source.md").unwrap().source, "[[New.txt]]\n");
        assert!(vault.get("New.txt").is_none());
        assert!(vault.has_file("New.txt"));
        vault.rename("New.txt", "Again.md").unwrap();
        assert_eq!(vault.get("Again.md").unwrap().source, "[[New.txt]]\n");
        assert_eq!(vault.get("Source.md").unwrap().source, "[[Again]]\n");
    }

    #[test]
    fn rename_hidden_target_rewrites_then_removes_index_entry() {
        let t = TempDir::new("rename-hidden-target");
        t.write("Old.md", "[[Old]]\n");
        t.write("Source.md", "[[Old]]\n");
        let mut vault = Vault::open(&t.0).unwrap();
        vault.rename("Old.md", ".hidden").unwrap();
        assert_eq!(
            fs::read_to_string(t.0.join(".hidden")).unwrap(),
            "[[.hidden]]\n"
        );
        assert_eq!(vault.get("Source.md").unwrap().source, "[[.hidden]]\n");
        assert!(vault.get(".hidden").is_none());
        assert!(!vault.has_file(".hidden"));
    }

    #[test]
    fn rename_source_edits_reconstruct_files_and_preserve_wiki_delimiters() {
        let t = TempDir::new("rename-editor-source-edits");
        t.write("Old.md", "[[Old]]\n");
        t.write(
            "Source.md",
            "---\n# discarded\nrelated: '[[Old]]'\n---\n[[Old]] and [Old](Old.md)\n",
        );
        let mut vault = Vault::open(&t.0).unwrap();
        let original: BTreeMap<_, _> = vault
            .notes
            .iter()
            .map(|(path, note)| (path.clone(), note.source.clone()))
            .collect();
        let records = vault.rename_with_edits("Old.md", "New.md").unwrap();
        assert_eq!(
            records
                .iter()
                .find(|record| record.path == "New.md")
                .unwrap()
                .edits,
            vec![crate::rewrite::TextEdit {
                start: 2,
                end: 5,
                text: "New".into()
            }]
        );
        for record in records {
            let old = if record.path == "New.md" {
                "Old.md"
            } else {
                &record.path
            };
            let mut text = original[old].clone();
            for edit in record.edits.iter().rev() {
                text.replace_range(edit.start..edit.end, &edit.text);
            }
            assert_eq!(text, fs::read_to_string(t.0.join(record.path)).unwrap());
        }
    }

    #[test]
    fn rename_updates_links() {
        let t = TempDir::new("rename");
        t.write("Home.md", "[[Alpha]]\n");
        t.write("notes/Alpha.md", "[[Home]]\n");
        t.write("notes/Beta.md", "see [[Alpha|the alpha]] here\n");
        let mut vault = Vault::open(&t.0).unwrap();

        vault.rename("notes/Alpha.md", "notes/Gamma.md").unwrap();

        assert!(vault.get("notes/Gamma.md").is_some());
        assert!(vault.get("notes/Alpha.md").is_none());
        assert_eq!(vault.get("Home.md").unwrap().source, "[[Gamma]]\n");
        assert_eq!(
            vault.get("notes/Beta.md").unwrap().source,
            "see [[Gamma|the alpha]] here\n"
        );
    }

    #[test]
    fn rename_updates_self_and_outgoing_references_at_moved_source() {
        let t = TempDir::new("rename-source-references");
        t.write(
            "a/Old.md",
            "[[Old]] [[#Heading]] [[./Sibling]] [Sibling](./Sibling.md) [[../Root]] [[Missing]]\n",
        );
        t.write("a/Sibling.md", "");
        t.write("Root.md", "");
        t.write("Source.md", "[[Old]] [Old](Old.md)\n");
        let mut vault = Vault::open(&t.0).unwrap();
        fs::create_dir_all(t.0.join("b")).unwrap();
        vault.rename("a/Old.md", "b/New.md").unwrap();

        assert_eq!(
            vault.get("b/New.md").unwrap().source,
            "[[New]] [[#Heading]] [[Sibling]] [Sibling](Sibling.md) [[../Root]] [[Missing]]\n"
        );
        assert_eq!(
            vault.get("Source.md").unwrap().source,
            "[[New]] [New](New.md)\n"
        );
        assert_eq!(
            fs::read_to_string(t.0.join("b/New.md")).unwrap(),
            vault.get("b/New.md").unwrap().source
        );
        assert_eq!(vault.backlinks_to("b/New.md"), &["Source.md", "b/New.md"]);
        assert_eq!(vault.backlinks_to("a/Sibling.md"), &["b/New.md"]);
        assert!(vault.backlinks_to("a/Old.md").is_empty());
    }

    #[test]
    fn moving_preserves_links_whose_resolution_does_not_change() {
        let t = TempDir::new("rename-unchanged-resolution");
        t.write("a/Old.md", "[[Old]]\n");
        t.write("Source.md", "[[Old]] [Old](Old.md) [[a/Old]]\n");
        let mut vault = Vault::open(&t.0).unwrap();

        fs::create_dir_all(t.0.join("b")).unwrap();
        vault.rename("a/Old.md", "b/Old.md").unwrap();

        assert_eq!(vault.get("b/Old.md").unwrap().source, "[[Old]]\n");
        assert_eq!(
            vault.get("Source.md").unwrap().source,
            "[[Old]] [Old](Old.md) [[Old]]\n"
        );
    }

    #[test]
    fn rename_updates_property_links_and_reads_link_format_setting() {
        let t = TempDir::new("rename-property-links");
        t.write("Old.md", "---\nself: \"[[Old]]\"\n---\n[[Old]]\n");
        t.write("Source.md", "---\n# discarded comment\nrelated: '[[Old#Heading|Old]]'\nlist: [\"[[Old]]\", other]\nnested:\n  key: \"[[Old]]\"\nmd: \"[Old](Old.md)\"\ntext: \"prefix [[Old]]\"\n---\n[[Old]]\n");
        t.write(&format!("{CONFIG_DIR}/app.json"), "{\"newLinkFormat\":\"absolute\"}");
        let mut vault = Vault::open(&t.0).unwrap();
        assert_eq!(vault.new_link_format, LinkFormat::Absolute);

        fs::create_dir_all(t.0.join("folder")).unwrap();
        vault.rename("Old.md", "folder/New.md").unwrap();

        assert_eq!(vault.get("Source.md").unwrap().source, "---\nrelated: \"[[folder/New#Heading|Old]]\"\nlist:\n  - \"[[folder/New]]\"\n  - other\nnested:\n  key: \"[[folder/New]]\"\nmd: \"[New](folder/New.md)\"\ntext: prefix [[Old]]\n---\n[[folder/New]]\n");
        assert_eq!(
            vault.get("folder/New.md").unwrap().source,
            "---\nself: \"[[folder/New]]\"\n---\n[[folder/New]]\n"
        );
        assert_eq!(
            vault.backlink_counts("folder/New.md"),
            [
                ("Source.md".to_string(), 5),
                ("folder/New.md".to_string(), 2)
            ]
        );
    }

    #[test]
    fn renamed_markdown_destinations_keep_resolving_after_url_encoding() {
        let t = TempDir::new("rename-encoded-markdown");
        t.write("Old Name.md", "");
        t.write(
            "Source.md",
            "[Old Name](Old%20Name.md#Heading) ![custom](Old%20Name.md)\n",
        );
        let mut vault = Vault::open(&t.0).unwrap();
        assert_eq!(
            vault.backlink_counts("Old Name.md"),
            [("Source.md".to_string(), 2)]
        );

        vault.rename("Old Name.md", "New Name.md").unwrap();

        assert_eq!(
            vault.get("Source.md").unwrap().source,
            "[New Name](New%20Name.md#Heading) ![custom](New%20Name.md)\n"
        );
        assert_eq!(
            vault.backlink_counts("New Name.md"),
            [("Source.md".to_string(), 2)]
        );
        assert_eq!(
            Vault::open(&t.0).unwrap().backlink_counts("New Name.md"),
            [("Source.md".to_string(), 2)]
        );
    }

    #[test]
    fn create_uses_available_path() {
        let t = TempDir::new("create");
        t.write("Note.md", "");
        let mut vault = Vault::open(&t.0).unwrap();
        assert_eq!(vault.create("Note", "# New").unwrap(), "Note 1.md");
        assert_eq!(vault.create("Note", "").unwrap(), "Note 2.md");
        assert_eq!(vault.create("Other", "x").unwrap(), "Other.md");
    }

    #[test]
    fn create_normalizes_final_empty_name_and_preserves_file_classification() {
        let t = TempDir::new("create-adapter-normalization");
        fs::create_dir(t.0.join("Folder.md")).unwrap();
        let mut vault = Vault::open(&t.0).unwrap();
        assert_eq!(vault.create("/", "hidden").unwrap(), ".md");
        assert_eq!(fs::read_to_string(t.0.join(".md")).unwrap(), "hidden");
        assert_eq!(vault.create("Folder", "new").unwrap(), "Folder 1.md");
        assert_eq!(vault.create("Image.png", "bytes").unwrap(), "Image.png");
        assert!(vault.has_file("Image.png"));
        assert!(vault.get("Image.png").is_none());
        vault.create("Upper.MD", "markdown").unwrap();
        assert!(Vault::open(&t.0).unwrap().get("Upper.MD").is_some());
    }

    #[test]
    fn app_settings_reject_yaml_that_is_not_json() {
        let t = TempDir::new("strict-settings-json");
        t.write(&format!("{CONFIG_DIR}/app.json"), "newLinkFormat: absolute\n");
        assert_eq!(
            Vault::open(&t.0).unwrap().new_link_format,
            LinkFormat::Shortest
        );
        t.write(
            &format!("{CONFIG_DIR}/app.json"),
            "{\"newLinkFormat\":\"relative\",\"extra\":true}",
        );
        assert_eq!(
            Vault::open(&t.0).unwrap().new_link_format,
            LinkFormat::Relative
        );
    }

    #[test]
    fn delete_removes_note() {
        let t = TempDir::new("delete");
        t.write("a/Note.md", "");
        let mut vault = Vault::open(&t.0).unwrap();
        vault.delete("a/Note.md").unwrap();
        assert!(vault.get("a/Note.md").is_none());
    }

    #[test]
    fn delete_preserves_empty_folders_vault_root_and_ancestors() {
        let t = TempDir::new("delete-preserves-folders");
        t.write("parent/vault/nested/deep/Note.md", "");
        let root = t.0.join("parent/vault");
        let mut vault = Vault::open(&root).unwrap();

        vault.delete("nested/deep/Note.md").unwrap();

        for folder in [
            &t.0,
            &t.0.join("parent"),
            &root,
            &root.join("nested"),
            &root.join("nested/deep"),
        ] {
            assert!(
                folder.is_dir(),
                "deleting a file removed {}",
                folder.display()
            );
        }
        assert!(Vault::open(&root).unwrap().files().is_empty());
    }

    #[test]
    fn delete_last_root_file_preserves_vault() {
        let t = TempDir::new("delete-last-root-file");
        t.write("vault/Note.md", "");
        let root = t.0.join("vault");
        let mut vault = Vault::open(&root).unwrap();

        vault.delete("Note.md").unwrap();

        assert!(root.is_dir());
        assert!(t.0.is_dir());
        assert!(Vault::open(&root).unwrap().files().is_empty());
    }

    #[test]
    fn trash_preserves_existing_files_and_uses_suffix_two() {
        let t = TempDir::new("trash-collision");
        t.write("nested/Note.md", "new");
        t.write(".trash/Note.md", "original");
        t.write(".trash/Note 2.md", "second");
        let mut vault = Vault::open(&t.0).unwrap();
        assert!(!vault.has_file(".trash/Note.md"));

        vault.trash("nested/Note.md").unwrap();

        assert_eq!(
            fs::read_to_string(t.0.join(".trash/Note.md")).unwrap(),
            "original"
        );
        assert_eq!(
            fs::read_to_string(t.0.join(".trash/Note 2.md")).unwrap(),
            "second"
        );
        assert_eq!(
            fs::read_to_string(t.0.join(".trash/Note 3.md")).unwrap(),
            "new"
        );
        assert!(!t.0.join(".trash/Note 1.md").exists());
        assert!(t.0.join("nested").is_dir());
    }

    #[test]
    fn trash_skips_existing_directories() {
        let t = TempDir::new("trash-directory-collision");
        t.write("Note.md", "new");
        t.write(".trash/Note.md/keep.txt", "directory contents");
        let mut vault = Vault::open(&t.0).unwrap();

        vault.trash("Note.md").unwrap();

        assert_eq!(
            fs::read_to_string(t.0.join(".trash/Note.md/keep.txt")).unwrap(),
            "directory contents"
        );
        assert_eq!(
            fs::read_to_string(t.0.join(".trash/Note 2.md")).unwrap(),
            "new"
        );
    }

    #[test]
    fn trash_collision_survives_reopening_vault() {
        let t = TempDir::new("trash-reopen");
        t.write("a/Note.md", "first");
        t.write("b/Note.md", "second");
        t.write("c/Note.md", "third");
        let mut vault = Vault::open(&t.0).unwrap();
        vault.trash("a/Note.md").unwrap();
        vault.trash("b/Note.md").unwrap();
        let mut reopened = Vault::open(&t.0).unwrap();
        reopened.trash("c/Note.md").unwrap();

        for (name, content) in [
            ("Note.md", "first"),
            ("Note 2.md", "second"),
            ("Note 3.md", "third"),
        ] {
            assert_eq!(
                fs::read_to_string(t.0.join(".trash").join(name)).unwrap(),
                content
            );
        }
        assert!(reopened.files().is_empty());
        assert!(Vault::open(&t.0).unwrap().files().is_empty());
    }

    #[test]
    fn trash_preserves_attachment_and_extensionless_names() {
        let t = TempDir::new("trash-attachment-names");
        for name in ["image.png", "archive.tar.gz", "LICENSE", "trailing."] {
            t.write(&format!("nested/{name}"), "new");
            t.write(&format!(".trash/{name}"), "original");
        }
        let mut vault = Vault::open(&t.0).unwrap();

        for (name, target) in [
            ("image.png", "image 2.png"),
            ("archive.tar.gz", "archive.tar 2.gz"),
            ("LICENSE", "LICENSE 2"),
            ("trailing.", "trailing 2."),
        ] {
            vault.trash(&format!("nested/{name}")).unwrap();
            assert_eq!(
                fs::read_to_string(t.0.join(".trash").join(name)).unwrap(),
                "original"
            );
            assert_eq!(
                fs::read_to_string(t.0.join(".trash").join(target)).unwrap(),
                "new"
            );
            assert!(!vault.has_file(&format!("nested/{name}")));
        }
        assert!(vault.files().is_empty());
    }

    #[test]
    fn delete_and_trash_rebuild_indexes_without_rewriting_inbound_links() {
        for trash in [false, true] {
            let t = TempDir::new(if trash {
                "trash-indexes"
            } else {
                "delete-indexes"
            });
            let source = "[[Target]] ![[image.png]]\n";
            t.write("Source.md", source);
            t.write(
                "nested/Target.md",
                "---\naliases: [Nickname]\n---\n#removed [[Source]]",
            );
            t.write("nested/image.png", "attachment");
            let mut vault = Vault::open(&t.0).unwrap();
            assert_eq!(vault.backlinks_to("nested/Target.md"), &["Source.md"]);
            assert_eq!(vault.backlinks_to("nested/image.png"), &["Source.md"]);
            assert_eq!(vault.backlinks_to("Source.md"), &["nested/Target.md"]);

            for file in ["nested/Target.md", "nested/image.png"] {
                if trash {
                    vault.trash(file).unwrap();
                } else {
                    vault.delete(file).unwrap();
                }
                assert!(!vault.has_file(file));
                assert!(vault.get(file).is_none());
                assert!(vault.backlinks_to(file).is_empty());
            }

            let source_note = vault.get("Source.md").unwrap();
            assert_eq!(source_note.source, source);
            assert_eq!(fs::read_to_string(t.0.join("Source.md")).unwrap(), source);
            assert!(vault.resolve_path(source_note, "Target").is_none());
            assert!(vault.resolve_path(source_note, "image.png").is_none());
            assert!(vault
                .resolve_linkpath_alias(source_note, "Nickname")
                .is_none());
            assert!(vault.backlinks_to("Source.md").is_empty());
            assert!(vault.notes_with_tag("#removed").is_empty());
            assert!(!vault.get_tags().contains_key("#removed"));
            assert_eq!(
                vault.files().iter().map(String::as_str).collect::<Vec<_>>(),
                ["Source.md"]
            );
        }
    }

    #[test]
    fn legacy_alias_helper_is_separate_from_graph_destination_lookup() {
        let t = TempDir::new("alias");
        t.write("Long Note Title.md", "---\naliases: [LNT]\n---\n");
        t.write("Home.md", "[[LNT]]\n");
        let vault = Vault::open(&t.0).unwrap();
        let home = vault.get("Home.md").unwrap();
        assert_eq!(
            vault.resolve_linkpath_alias(home, "LNT").as_deref(),
            Some("Long Note Title.md")
        );
        // The legacy helper remains separate; native cache graph lookup uses
        // filename destinations and therefore leaves this alias unresolved.
        let graph = vault.graph();
        assert!(!graph
            .edges
            .iter()
            .any(|e| e.from == "Home.md" && e.to == "Long Note Title.md"));
        assert_eq!(graph.unresolved.get("LNT"), Some(&1));
    }

    #[test]
    fn frontmatter_links_create_backlinks() {
        let t = TempDir::new("fmlinks");
        t.write("Target.md", "");
        t.write(
            "Source.md",
            "---\nrelated: \"[[Target]]\"\nseeAlso: [\"[[Target]]\", \"[[Other]]\"]\n---\n",
        );
        let vault = Vault::open(&t.0).unwrap();
        assert_eq!(vault.backlinks_to("Target.md"), &["Source.md"]);
        assert_eq!(vault.notes["Source.md"].frontmatter_links.len(), 3);
    }
}

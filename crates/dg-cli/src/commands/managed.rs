use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Args, ValueEnum};
use md_db::config::Config;
use md_db::document::Document;
use md_db::schema::{Schema, TypeDef};

#[derive(Args)]
pub struct ManagedArgs {
    /// Managed mode action
    #[arg(value_enum, default_value = "status")]
    pub action: ManagedAction,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ManagedAction {
    /// Make schema-managed files writable only through dg.
    On,
    /// Restore owner write access to schema-managed files.
    Off,
    /// Show the current managed mode state.
    Status,
}

/// Run the managed mode command.
pub fn run(root: &Path, schema: &Schema, args: &ManagedArgs) -> Result<()> {
    match args.action {
        ManagedAction::On => enable(root, schema),
        ManagedAction::Off => disable(root, schema),
        ManagedAction::Status => {
            let config = Config::load(&root.join(".dg"));
            println!(
                "managed mode: {}",
                if config.managed {
                    "enabled"
                } else {
                    "disabled"
                }
            );
            Ok(())
        }
    }
}

/// Enable managed mode and enforce permissions immediately.
pub(crate) fn enable(root: &Path, schema: &Schema) -> Result<()> {
    let dg_root = root.join(".dg");
    let mut config = Config::load(&dg_root);
    config.managed = true;
    config
        .save(&dg_root)
        .context("failed to save managed mode configuration")?;
    set_schema_paths_readonly(root, schema, true)
        .context("failed to make schema-managed paths read-only")?;
    println!("managed mode: enabled");
    Ok(())
}

/// Disable managed mode and restore owner write access immediately.
pub(crate) fn disable(root: &Path, schema: &Schema) -> Result<()> {
    let dg_root = root.join(".dg");
    let mut config = Config::load(&dg_root);
    config.managed = false;
    config
        .save(&dg_root)
        .context("failed to save managed mode configuration")?;
    set_schema_paths_readonly(root, schema, false)
        .context("failed to restore schema-managed path permissions")?;
    println!("managed mode: disabled");
    Ok(())
}

/// Load the active schema for commands that run before normal CLI setup.
pub(crate) fn load_schema(root: &Path) -> Result<Schema> {
    let schema_path = root.join(".dg").join("schema.kdl");
    if schema_path.is_file() {
        Schema::from_file(&schema_path)
            .with_context(|| format!("failed to load schema: {}", schema_path.display()))
    } else {
        Schema::from_str(dg_schemas::SCHEMA).context("failed to parse built-in schema")
    }
}

/// Non-singleton schema folders are managed recursively, including their
/// existing parent directories down to (but not including) the project root.
/// Singleton documents are selected by their schema folder and filename
/// pattern. Markdown files with a known non-singleton frontmatter type are also
/// managed even when they are outside the configured folder.
pub(crate) fn set_schema_paths_readonly(
    root: &Path,
    schema: &Schema,
    readonly: bool,
) -> Result<()> {
    let paths = managed_paths(root, schema)?;
    let mut paths: Vec<PathBuf> = paths.into_iter().collect();
    if readonly {
        paths.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    }

    for path in paths {
        set_path_readonly(&path, readonly)
            .with_context(|| format!("failed to update permissions for {}", path.display()))?;
    }
    Ok(())
}

/// Temporarily make managed paths writable while an early command runs.
pub(crate) struct ManagedWriteGuard<'a> {
    root: &'a Path,
    schema: &'a Schema,
}

impl<'a> ManagedWriteGuard<'a> {
    pub(crate) fn new(root: &'a Path, schema: &'a Schema) -> Result<Self> {
        set_schema_paths_readonly(root, schema, false)
            .context("failed to make schema-managed paths writable")?;
        Ok(Self { root, schema })
    }
}

impl Drop for ManagedWriteGuard<'_> {
    fn drop(&mut self) {
        if let Err(error) = set_schema_paths_readonly(self.root, self.schema, true) {
            eprintln!("warning: failed to restore managed permissions: {error:#}");
        }
    }
}

fn managed_paths(root: &Path, schema: &Schema) -> Result<BTreeSet<PathBuf>> {
    let mut paths = BTreeSet::new();

    for type_def in &schema.types {
        if type_def.singleton {
            continue;
        }
        let Some(folder) = type_def.folder.as_deref() else {
            continue;
        };
        let folder_path = schema_folder(root, folder)?;
        if is_root_folder(folder)
            || !is_real_path(&folder_path)?
            || !folder_path.is_dir()
            || !is_within_root(root, &folder_path)?
        {
            continue;
        }
        collect_tree(&folder_path, &mut paths)?;
        add_folder_ancestors(root, &folder_path, &mut paths)?;
    }

    let singleton_patterns: Vec<&str> = schema
        .types
        .iter()
        .filter(|type_def| type_def.singleton)
        .filter_map(|type_def| type_def.match_pattern.as_deref())
        .collect();
    if !singleton_patterns.is_empty() {
        for path in md_db::discovery::discover_singleton_files(root, &singleton_patterns)? {
            if !is_real_path(&path)? {
                continue;
            }
            if schema
                .types
                .iter()
                .filter(|type_def| type_def.singleton)
                .any(|type_def| singleton_path_matches(root, &path, type_def))
            {
                paths.insert(path);
            }
        }
    }

    // A valid document moved outside its configured folder is still a
    // schema-managed document and must not become an unmanaged write escape.
    for path in md_db::discovery::discover_files(root, None, &[], false)? {
        if is_real_path(&path)? && is_within_root(root, &path)? && is_schema_document(&path, schema)
        {
            paths.insert(path);
        }
    }

    Ok(paths)
}

fn add_folder_ancestors(
    root: &Path,
    folder: &Path,
    paths: &mut BTreeSet<PathBuf>,
) -> std::io::Result<()> {
    let mut ancestor = folder.parent();
    while let Some(path) = ancestor {
        if path == root {
            break;
        }
        if path.starts_with(root) && is_real_path(path)? && path.is_dir() {
            paths.insert(path.to_path_buf());
        }
        ancestor = path.parent();
    }
    Ok(())
}

fn schema_folder(root: &Path, folder: &str) -> Result<PathBuf> {
    let relative = Path::new(folder);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        bail!("schema folder escapes project root: {folder}");
    }
    Ok(root.join(relative))
}

fn is_root_folder(folder: &str) -> bool {
    Path::new(folder)
        .components()
        .all(|component| matches!(component, Component::CurDir))
}

fn is_within_root(root: &Path, path: &Path) -> std::io::Result<bool> {
    let root = fs::canonicalize(root)?;
    let path = match fs::canonicalize(path) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    Ok(path.starts_with(root))
}

fn singleton_path_matches(root: &Path, path: &Path, type_def: &TypeDef) -> bool {
    let Some(pattern) = type_def.match_pattern.as_deref() else {
        return false;
    };
    if path.file_name().and_then(|name| name.to_str()) != Some(pattern) {
        return false;
    }

    let Some(folder) = type_def.folder.as_deref() else {
        return false;
    };
    let Ok(root) = fs::canonicalize(root) else {
        return false;
    };
    let Ok(path) = fs::canonicalize(path) else {
        return false;
    };
    if !path.starts_with(&root) {
        return false;
    }
    let Ok(folder_path) = schema_folder(&root, folder) else {
        return false;
    };
    let Ok(folder) = fs::canonicalize(folder_path) else {
        return false;
    };
    if !folder.starts_with(&root) {
        return false;
    }

    if folder == root {
        return path.parent() == Some(root.as_path());
    }
    path.starts_with(folder)
}

fn is_schema_document(path: &Path, schema: &Schema) -> bool {
    let Ok(document) = Document::from_file(path) else {
        return false;
    };
    let Some(frontmatter) = document.frontmatter else {
        return false;
    };
    let Some(type_name) = frontmatter.get_display("type") else {
        return false;
    };
    schema
        .get_type(&type_name)
        .is_some_and(|type_def| !type_def.singleton)
}

fn collect_tree(path: &Path, paths: &mut BTreeSet<PathBuf>) -> std::io::Result<()> {
    if !is_real_path(path)? || md_db::discovery::is_ignored_dir(path) {
        return Ok(());
    }

    let metadata = fs::symlink_metadata(path)?;
    paths.insert(path.to_path_buf());
    if !metadata.is_dir() {
        return Ok(());
    }

    for entry in fs::read_dir(path)? {
        collect_tree(&entry?.path(), paths)?;
    }
    Ok(())
}

fn is_real_path(path: &Path) -> std::io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(!metadata.file_type().is_symlink()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn set_path_readonly(path: &Path, readonly: bool) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }

    let mut permissions = metadata.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = permissions.mode();
        let new_mode = if readonly {
            mode & !0o222
        } else {
            mode | 0o200
        };
        if mode != new_mode {
            permissions.set_mode(new_mode);
            fs::set_permissions(path, permissions)?;
        }
    }
    #[cfg(not(unix))]
    {
        permissions.set_readonly(readonly);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn schema() -> Schema {
        Schema::from_str(dg_schemas::SCHEMA).expect("built-in schema must parse")
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode()
    }

    #[test]
    fn managed_paths_include_document_tree_and_root_readme() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("docs/architecture")).unwrap();
        fs::write(root.join("docs/architecture/adr-001.md"), "# ADR\n").unwrap();
        fs::write(root.join("README.md"), "# Project\n").unwrap();

        let paths = managed_paths(root, &schema()).unwrap();
        assert!(paths.contains(&root.join("docs")));
        assert!(paths.contains(&root.join("docs/architecture/adr-001.md")));
        assert!(paths.contains(&root.join("README.md")));
    }

    #[cfg(unix)]
    #[test]
    fn managed_permissions_can_be_toggled() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("docs/architecture")).unwrap();
        fs::write(root.join("docs/architecture/adr-001.md"), "# ADR\n").unwrap();
        fs::write(root.join("README.md"), "# Project\n").unwrap();
        let schema = schema();

        set_schema_paths_readonly(root, &schema, true).unwrap();
        assert_eq!(mode(&root.join("docs")) & 0o222, 0);
        assert_eq!(mode(&root.join("docs/architecture/adr-001.md")) & 0o222, 0);
        assert_eq!(mode(&root.join("README.md")) & 0o222, 0);

        set_schema_paths_readonly(root, &schema, false).unwrap();
        assert_ne!(mode(&root.join("docs")) & 0o200, 0);
        assert_ne!(mode(&root.join("docs/architecture/adr-001.md")) & 0o200, 0);
        assert_ne!(mode(&root.join("README.md")) & 0o200, 0);
    }
}

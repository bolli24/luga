use std::collections::HashMap;
use std::io;
use std::path::{Component, Path, PathBuf};

/// Provides the source of imported modules.
pub trait SourceLoader {
    /// Resolve a module path to a unique key, or `None` if the module doesn't exist.
    /// The key is used for caching, cycle detection and in `ModuleErrors.path`.
    fn resolve(&self, path: &Path) -> Option<PathBuf>;
    /// Load the source of a module previously returned by `resolve`.
    fn load(&self, path: &Path) -> io::Result<String>;
}

/// Loads modules from the file system.
pub struct FsLoader;

impl SourceLoader for FsLoader {
    fn resolve(&self, path: &Path) -> Option<PathBuf> {
        path.canonicalize().ok()
    }

    fn load(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }
}

/// Loads modules from an in-memory set of files, e.g. when running in a browser.
#[derive(Debug, Default, Clone)]
pub struct MemoryLoader {
    files: HashMap<PathBuf, String>,
}

impl MemoryLoader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, path: impl AsRef<Path>, src: impl Into<String>) {
        self.files.insert(normalize(path.as_ref()), src.into());
    }
}

impl SourceLoader for MemoryLoader {
    fn resolve(&self, path: &Path) -> Option<PathBuf> {
        let path = normalize(path);
        self.files.contains_key(&path).then_some(path)
    }

    fn load(&self, path: &Path) -> io::Result<String> {
        self.files
            .get(&normalize(path))
            .cloned()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }
}

/// Lexically normalize a path by removing `.` and resolving `..` components.
fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match normalized.components().next_back() {
                Some(Component::Normal(_)) => {
                    normalized.pop();
                }
                // `..` at the root stays at the root
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => normalized.push(".."),
            },
            other => normalized.push(other),
        }
    }
    normalized
}

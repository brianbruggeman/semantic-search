use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use proxima::pipe::Pipe;

use crate::error::IndexError;

/// source: `() -> Vec<PathBuf>`; gitignore-aware listing of regular files under `root`.
#[derive(Debug, Clone)]
pub struct Walk {
    root: PathBuf,
}

impl Walk {
    #[must_use]
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }
}

impl Pipe for Walk {
    type In = ();
    type Out = Vec<PathBuf>;
    type Err = IndexError;

    fn call(&self, _input: Self::In) -> impl Future<Output = Result<Self::Out, Self::Err>> {
        let root = self.root.clone();
        async move {
            tokio::task::spawn_blocking(move || Ok(list_files(&root)))
                .await
                .map_err(|error| IndexError::Join(error.to_string()))?
        }
    }
}

/// Files honoring `.gitignore`, `.ignore` and global excludes even outside a git work tree; hidden entries skipped.
#[must_use]
pub fn list_files(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = WalkBuilder::new(root)
        .require_git(false)
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .map(ignore::DirEntry::into_path)
        .collect();
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[proxima::test(runtime = "tokio")]
    async fn walk_skips_gitignored_hidden_and_keeps_regular_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        std::fs::write(dir.path().join(".gitignore"), "target/\n*.log\n").expect("gitignore");
        std::fs::create_dir_all(dir.path().join("target")).expect("mkdir");
        std::fs::write(dir.path().join("target/out.rs"), "fn x() {}").expect("write");
        std::fs::write(dir.path().join("run.log"), "noise").expect("write");
        std::fs::write(dir.path().join(".hidden"), "secret").expect("write");
        std::fs::write(dir.path().join("src/lib.rs"), "fn y() {}").expect("write");

        let found = Walk::new(dir.path()).call(()).await.expect("walk");

        let names: Vec<String> = found
            .iter()
            .map(|path| {
                path.strip_prefix(dir.path())
                    .expect("under root")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, vec!["src/lib.rs".to_owned()]);
    }
}

use std::path::{Path, PathBuf};
use tempfile::TempDir;

use crate::embed::{EmbedPool, EmbedderKind, hash_embed};
use crate::index::Shard;
use crate::store::{Store, StoreConfig};

pub struct Corpus {
    pub dir: TempDir,
    pub shard: Shard,
}

impl Corpus {
    pub fn root(&self) -> PathBuf {
        self.dir.path().join("tree")
    }

    pub fn chunk_count(&self) -> u64 {
        Store::open(&self.shard.config)
            .expect("open store")
            .chunk_count()
            .expect("count chunks")
    }
}

pub fn hash_pool() -> EmbedPool {
    EmbedPool::spawn(2, || Ok(hash_embed))
}

pub fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create destination");
    for entry in std::fs::read_dir(from).expect("read source dir") {
        let entry = entry.expect("dir entry");
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).expect("copy file");
        }
    }
}

/// The repo's own `src` copied into a temp tree, with a shard wired to `pool`.
pub fn own_src_corpus(pool: &EmbedPool, kind: EmbedderKind) -> Corpus {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("tree");
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &root.join("src"),
    );
    let config = StoreConfig {
        dir: dir.path().join("shard"),
        model_id: kind.model_id().to_owned(),
    };
    let shard = Shard::new(&root, config, pool);
    Corpus { dir, shard }
}

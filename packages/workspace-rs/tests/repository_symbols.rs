use std::time::Duration;

use maestro_workspace::symbols::{RepositorySymbolIndex, SymbolIndexConfig};

fn fixture(root: &std::path::Path) {
    let source = root.join("packages/runtime/src");
    std::fs::create_dir_all(&source).expect("create source tree");
    std::fs::write(
        source.join("alpha.rs"),
        "pub struct Needle;\npub fn construct() -> Needle { Needle }\n",
    )
    .expect("write alpha");
    std::fs::write(
        source.join("beta.rs"),
        "use crate::alpha::Needle;\nfn caller() { let _ = Needle; }\n",
    )
    .expect("write beta");
    let ignored = root.join("vendor/copied");
    std::fs::create_dir_all(&ignored).expect("create ignored tree");
    std::fs::write(ignored.join("wrong.rs"), "pub struct Needle;\n").expect("write ignored source");
}

fn test_config() -> SymbolIndexConfig {
    SymbolIndexConfig {
        max_files: 1_000,
        max_file_bytes: 1_000_000,
        background_refresh_interval: Duration::from_secs(300),
    }
}

#[test]
fn indexes_definitions_and_referencing_files_without_ignored_trees() {
    let directory = tempfile::tempdir().expect("tempdir");
    fixture(directory.path());
    let index = RepositorySymbolIndex::new(directory.path(), test_config());

    let refresh = index.refresh().expect("initial refresh");
    let result = index.query("Needle", 20).expect("symbol query");

    assert_eq!(refresh.indexed_files, 2);
    assert_eq!(refresh.parsed_files, 2);
    assert_eq!(refresh.reused_files, 0);
    assert_eq!(result.revision.len(), 64);
    assert_eq!(result.definitions.len(), 1);
    assert_eq!(result.definitions[0].path, "packages/runtime/src/alpha.rs");
    assert_eq!(result.definitions[0].line, 1);
    assert_eq!(result.definitions[0].kind, "struct");
    assert_eq!(
        result.referencing_files,
        [
            "packages/runtime/src/alpha.rs".to_string(),
            "packages/runtime/src/beta.rs".to_string(),
        ]
    );
    assert!(!result.truncated);
}

#[test]
fn refresh_reuses_unchanged_files_and_reparses_only_changes() {
    let directory = tempfile::tempdir().expect("tempdir");
    fixture(directory.path());
    let source = directory.path().join("packages/runtime/src");
    let index = RepositorySymbolIndex::new(directory.path(), test_config());
    let initial = index.refresh().expect("initial refresh");

    let unchanged = index.refresh().expect("unchanged refresh");
    assert_eq!(unchanged.parsed_files, 0);
    assert_eq!(unchanged.reused_files, 2);
    assert_eq!(unchanged.revision, initial.revision);

    std::fs::write(source.join("alpha.rs"), "pub enum Replacement { Value }\n")
        .expect("replace alpha");
    std::fs::remove_file(source.join("beta.rs")).expect("remove beta");
    let changed = index.refresh().expect("changed refresh");

    assert_eq!(changed.parsed_files, 1);
    assert_eq!(changed.removed_files, 1);
    assert_ne!(changed.revision, initial.revision);
    assert!(
        index
            .query("Needle", 20)
            .expect("old query")
            .definitions
            .is_empty()
    );
    assert_eq!(
        index
            .query("Replacement", 20)
            .expect("new query")
            .definitions[0]
            .kind,
        "enum"
    );
}

#[test]
fn query_rejects_non_identifiers_and_bounds_referencing_files() {
    let directory = tempfile::tempdir().expect("tempdir");
    fixture(directory.path());
    let source = directory.path().join("packages/runtime/src");
    for index in 0..5 {
        std::fs::write(
            source.join(format!("reference_{index}.rs")),
            "fn use_it() { let _ = Needle; }\n",
        )
        .expect("write reference");
    }
    let index = RepositorySymbolIndex::new(directory.path(), test_config());
    index.refresh().expect("refresh");

    let result = index.query("Needle", 2).expect("bounded query");
    assert_eq!(result.referencing_files.len(), 2);
    assert!(result.truncated);
    for invalid in ["", "Needle.*", "crate::Needle", "../Needle"] {
        assert!(index.query(invalid, 20).is_err(), "accepted {invalid:?}");
    }
    assert!(index.query("Needle", 0).is_err());
}

#[test]
fn reports_when_source_limits_make_the_index_incomplete() {
    let directory = tempfile::tempdir().expect("tempdir");
    let source = directory.path().join("src");
    std::fs::create_dir_all(&source).expect("create source");
    std::fs::write(source.join("alpha.rs"), "pub struct Alpha;\n").expect("write alpha");
    std::fs::write(source.join("beta.rs"), "pub struct Beta;\n").expect("write beta");

    let mut file_limited = test_config();
    file_limited.max_files = 1;
    let index = RepositorySymbolIndex::new(directory.path(), file_limited);
    let refresh = index.refresh().expect("file-limited refresh");
    let result = index.query("Alpha", 20).expect("file-limited query");
    assert!(refresh.index_truncated);
    assert!(result.index_truncated);

    let mut size_limited = test_config();
    size_limited.max_file_bytes = 8;
    let index = RepositorySymbolIndex::new(directory.path(), size_limited);
    let refresh = index.refresh().expect("size-limited refresh");
    let result = index.query("Alpha", 20).expect("size-limited query");
    assert_eq!(refresh.skipped_files, 2);
    assert_eq!(result.skipped_files, 2);
    assert!(result.index_truncated);
}

#[test]
fn invalidation_requires_a_blocking_refresh_and_updates_the_revision() {
    let directory = tempfile::tempdir().expect("tempdir");
    fixture(directory.path());
    let index = RepositorySymbolIndex::new(directory.path(), test_config());
    assert!(index.needs_blocking_refresh());
    let initial = index.refresh().expect("initial refresh");
    assert!(!index.needs_blocking_refresh());

    std::fs::write(
        directory.path().join("packages/runtime/src/gamma.rs"),
        "pub trait Added {}\n",
    )
    .expect("write added source");
    index.invalidate();
    assert!(index.needs_blocking_refresh());
    let changed = index.refresh().expect("invalidated refresh");

    assert_ne!(changed.revision, initial.revision);
    assert!(!index.needs_blocking_refresh());
    assert_eq!(
        index.query("Added", 20).expect("added query").definitions[0].kind,
        "trait"
    );
}

#[test]
fn invalidated_same_size_same_mtime_content_is_reparsed() {
    let directory = tempfile::tempdir().expect("tempdir");
    fixture(directory.path());
    let file = directory.path().join("packages/runtime/src/alpha.rs");
    let index = RepositorySymbolIndex::new(directory.path(), test_config());
    let initial = index.refresh().expect("initial refresh");
    let modified = std::fs::metadata(&file)
        .expect("source metadata")
        .modified()
        .expect("modified time");
    let before = std::fs::read_to_string(&file).expect("read original");
    let after = before.replace("Needle", "Thread");
    assert_eq!(before.len(), after.len());
    std::fs::write(&file, after).expect("rewrite same-size source");
    std::fs::File::options()
        .write(true)
        .open(&file)
        .expect("open rewritten source")
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .expect("restore modified time");

    index.invalidate();
    let changed = index.refresh().expect("refresh rewritten source");

    assert_ne!(changed.revision, initial.revision);
    assert!(
        index
            .query("Needle", 20)
            .expect("old query")
            .definitions
            .is_empty()
    );
    assert_eq!(
        index.query("Thread", 20).expect("new query").definitions[0].kind,
        "struct"
    );
}

#[test]
fn background_refresh_admission_is_single_flight() {
    let directory = tempfile::tempdir().expect("tempdir");
    fixture(directory.path());
    let mut config = test_config();
    config.background_refresh_interval = Duration::ZERO;
    let index = RepositorySymbolIndex::new(directory.path(), config);
    index.refresh().expect("initial refresh");

    assert!(index.needs_background_refresh());
    assert!(index.begin_background_refresh());
    assert!(!index.begin_background_refresh());
    index.end_background_refresh();
    assert!(index.begin_background_refresh());
    index.end_background_refresh();
}

#[cfg(unix)]
#[test]
fn skips_external_symlink_sources() {
    use std::os::unix::fs::symlink;

    let parent = tempfile::tempdir().expect("tempdir");
    let root = parent.path().join("repo");
    fixture(&root);
    let outside = parent.path().join("outside.rs");
    std::fs::write(&outside, "pub struct Escaped;\n").expect("write outside source");
    symlink(&outside, root.join("packages/runtime/src/external.rs")).expect("create symlink");
    let index = RepositorySymbolIndex::new(&root, test_config());
    index.refresh().expect("refresh");

    assert!(
        index
            .query("Escaped", 20)
            .expect("query")
            .definitions
            .is_empty()
    );
}

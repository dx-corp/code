//! Benchmarks for parallel file indexer
//!
//! Run with: cargo bench --bench indexer_bench

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use maestro_tui::files::{FileIndexer, IndexerConfig};
use std::fs::File;
use std::hint::black_box;
use tempfile::TempDir;

/// Create a test directory structure for benchmarking
fn create_test_structure(num_files: usize, depth: usize) -> TempDir {
    let dir = TempDir::new().unwrap();

    // Create nested directories
    let mut current = dir.path().to_path_buf();
    for d in 0..depth {
        current = current.join(format!("dir{}", d));
        std::fs::create_dir_all(&current).unwrap();

        // Create files at each level
        let files_per_level = num_files / depth.max(1);
        for f in 0..files_per_level {
            let ext = match f % 4 {
                0 => "rs",
                1 => "ts",
                2 => "json",
                _ => "md",
            };
            File::create(current.join(format!("file{}.{}", f, ext))).unwrap();
        }
    }

    dir
}

/// Benchmark indexer config creation
fn bench_indexer_config(c: &mut Criterion) {
    c.bench_function("indexer_config_default", |b| {
        b.iter(|| {
            let config = IndexerConfig::default();
            black_box(config)
        });
    });

    c.bench_function("indexer_config_builder", |b| {
        b.iter(|| {
            let config = IndexerConfig::default()
                .with_max_files(black_box(10000))
                .skip_dir(black_box("vendor"))
                .include_only(black_box(&["rs", "ts"]));
            black_box(config)
        });
    });
}

/// Benchmark indexer creation
fn bench_indexer_creation(c: &mut Criterion) {
    c.bench_function("indexer_new_default", |b| {
        b.iter(|| {
            let indexer = FileIndexer::default();
            black_box(indexer)
        });
    });

    c.bench_function("indexer_new_custom", |b| {
        b.iter(|| {
            let config = IndexerConfig::default().with_max_files(1000);
            let indexer = FileIndexer::new(config);
            black_box(indexer)
        });
    });
}

/// Benchmark file indexing with varying directory sizes
fn bench_index_sync(c: &mut Criterion) {
    let mut group = c.benchmark_group("index_sync");
    group.sample_size(20); // Fewer samples for I/O-bound benchmarks

    for (num_files, depth) in &[(10, 2), (50, 3), (100, 4), (500, 5)] {
        let dir = create_test_structure(*num_files, *depth);
        let indexer = FileIndexer::default();

        group.bench_with_input(
            BenchmarkId::new("files", format!("{}x{}", num_files, depth)),
            &dir,
            |b, dir| {
                b.iter(|| {
                    indexer.clear_cache();
                    let files = indexer.get_files(black_box(dir.path()));
                    black_box(files)
                });
            },
        );
    }

    group.finish();
}

/// Benchmark cache hits
fn bench_cache_hit(c: &mut Criterion) {
    let dir = create_test_structure(100, 3);
    let indexer = FileIndexer::default();

    // Prime the cache
    let _ = indexer.get_files(dir.path());

    c.bench_function("index_cache_hit", |b| {
        b.iter(|| {
            let files = indexer.get_files(black_box(dir.path()));
            black_box(files)
        });
    });
}

/// Benchmark has_valid_cache check
fn bench_cache_check(c: &mut Criterion) {
    let dir = create_test_structure(50, 2);
    let indexer = FileIndexer::default();

    // Prime the cache
    let _ = indexer.get_files(dir.path());

    c.bench_function("has_valid_cache", |b| {
        b.iter(|| {
            let valid = indexer.has_valid_cache(black_box(dir.path()));
            black_box(valid)
        });
    });
}

/// Benchmark clear_cache
fn bench_clear_cache(c: &mut Criterion) {
    let dir = create_test_structure(50, 2);
    let indexer = FileIndexer::default();

    c.bench_function("clear_cache", |b| {
        b.iter(|| {
            // Prime then clear
            let _ = indexer.get_files(dir.path());
            indexer.clear_cache();
            black_box(());
        });
    });
}

/// Benchmark status retrieval
fn bench_status(c: &mut Criterion) {
    let indexer = FileIndexer::default();

    c.bench_function("indexer_status", |b| {
        b.iter(|| {
            let status = indexer.status();
            black_box(status)
        });
    });
}

/// Benchmark extension filtering
fn bench_extension_filter(c: &mut Criterion) {
    let dir = create_test_structure(200, 4);

    let mut group = c.benchmark_group("extension_filter");
    group.sample_size(20);

    // No filter
    let indexer_all = FileIndexer::default();
    group.bench_function("no_filter", |b| {
        b.iter(|| {
            indexer_all.clear_cache();
            let files = indexer_all.get_files(black_box(dir.path()));
            black_box(files)
        });
    });

    // Filter to only .rs files
    let config_rs = IndexerConfig::default().include_only(&["rs"]);
    let indexer_rs = FileIndexer::new(config_rs);
    group.bench_function("rs_only", |b| {
        b.iter(|| {
            indexer_rs.clear_cache();
            let files = indexer_rs.get_files(black_box(dir.path()));
            black_box(files)
        });
    });

    group.finish();
}

/// Benchmark async indexing
fn bench_index_async(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = create_test_structure(100, 3);
    let indexer = FileIndexer::default();

    c.bench_function("index_async", |b| {
        b.iter(|| {
            rt.block_on(async {
                indexer.clear_cache();
                let files = indexer
                    .index_async(black_box(dir.path().to_path_buf()), None)
                    .await;
                black_box(files)
            })
        });
    });
}

criterion_group!(
    benches,
    bench_indexer_config,
    bench_indexer_creation,
    bench_index_sync,
    bench_cache_hit,
    bench_cache_check,
    bench_clear_cache,
    bench_status,
    bench_extension_filter,
    bench_index_async,
);

criterion_main!(benches);

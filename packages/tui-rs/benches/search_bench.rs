use std::path::PathBuf;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use maestro_tui::files::{FileSearch, WorkspaceFile};
use std::hint::black_box;

fn workspace_files(count: usize) -> Vec<WorkspaceFile> {
    const NAMES: [&str; 8] = [
        "src/components/user_profile.rs",
        "src/commands/registry.rs",
        "src/files/workspace.rs",
        "tests/integration/session_test.rs",
        "docs/architecture.md",
        "config/providers.json",
        "assets/icon.png",
        "scripts/release.mjs",
    ];

    (0..count)
        .map(|index| {
            let suffix = NAMES[index % NAMES.len()];
            let relative_path = format!("package_{index}/{suffix}");
            let path = PathBuf::from("/workspace").join(&relative_path);
            let name = suffix.rsplit('/').next().unwrap().to_owned();
            let extension = name
                .rsplit_once('.')
                .map(|(_, extension)| extension.to_owned());
            WorkspaceFile {
                path,
                relative_path,
                name,
                extension,
                is_dir: false,
            }
        })
        .collect()
}

fn bench_workspace_search(c: &mut Criterion) {
    let mut group = c.benchmark_group("workspace_search");

    for count in [1_000, 10_000, 50_000] {
        let search = FileSearch::new(workspace_files(count)).max_results(20);
        group.bench_with_input(BenchmarkId::from_parameter(count), &search, |b, search| {
            b.iter(|| {
                let result = search.search(black_box("usrprof"));
                black_box(result)
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_workspace_search);
criterion_main!(benches);

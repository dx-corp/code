use anyhow::Result;

/// The interactive key-dispatch future can exceed Tokio's 2 MiB default
/// worker stack in debug builds. Keep both development and production entry
/// points on the same bounded stack size so the first modal key cannot abort
/// the process with a stack overflow.
const TOKIO_WORKER_STACK_BYTES: usize = 8 * 1024 * 1024;

fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(TOKIO_WORKER_STACK_BYTES)
        .build()?
        .block_on(maestro_tui::run_cli(std::env::args_os().collect()))
}

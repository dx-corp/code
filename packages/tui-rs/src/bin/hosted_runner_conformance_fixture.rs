#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    maestro_tui::hosted_runner_conformance::run().await
}

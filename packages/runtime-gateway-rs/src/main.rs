use maestro_runtime_gateway::{
    CliAction, RuntimeGatewayConfig, parse_cli_action, print_cli_help, print_cli_version, serve,
};
use std::{env, process};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    match parse_cli_action(env::args().skip(1)) {
        Ok(CliAction::Serve) => serve(RuntimeGatewayConfig::from_env()).await,
        Ok(CliAction::Help) => {
            print_cli_help();
            Ok(())
        }
        Ok(CliAction::Version) => {
            print_cli_version();
            Ok(())
        }
        Err(error) => {
            eprintln!("{error}\nRun `maestro-runtime-gateway --help` for usage.");
            process::exit(2);
        }
    }
}

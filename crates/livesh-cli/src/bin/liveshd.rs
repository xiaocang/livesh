#[tokio::main]
async fn main() {
    if let Err(err) = livesh_cli::daemon::run().await {
        eprintln!("liveshd: {err:#}");
        std::process::exit(livesh_cli::exit_code_for_error(&err));
    }
}

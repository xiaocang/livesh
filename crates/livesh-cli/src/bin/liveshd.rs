const STRIP_PREFIX_ENV: &str = "LIVESH_STRIP_PREFIX_ENV";

#[tokio::main]
async fn main() {
    let strip_prefix_env = std::env::var(STRIP_PREFIX_ENV)
        .ok()
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if let Err(err) = livesh_cli::daemon::run(strip_prefix_env).await {
        eprintln!("liveshd: {err:#}");
        std::process::exit(livesh_cli::exit_code_for_error(&err));
    }
}

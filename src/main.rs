use lambda_http::{Error, run, service_fn};
use telegram_wikimedia_commons_uploader_bot::app::{handle_lambda_request, run_polling};

/// Starts the bot in AWS Lambda (webhook) mode, or long-polling server mode.
///
/// Lambda is auto-detected via `AWS_LAMBDA_RUNTIME_API`; set `BOT_MODE=polling` to force
/// the long-living server mode used on Toolforge / Cloud VPS.
#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .without_time()
        .init();

    if use_polling_mode() {
        return run_polling()
            .await
            .map_err(|error| Error::from(format!("{error:#}")));
    }

    run(service_fn(|request| async move {
        handle_lambda_request(request)
            .await
            .map_err(|error| error.to_string())
    }))
    .await
}

/// Chooses long-polling (server) mode unless running inside AWS Lambda.
fn use_polling_mode() -> bool {
    match std::env::var("BOT_MODE") {
        Ok(mode) => mode.eq_ignore_ascii_case("polling"),
        Err(_) => std::env::var("AWS_LAMBDA_RUNTIME_API").is_err(),
    }
}

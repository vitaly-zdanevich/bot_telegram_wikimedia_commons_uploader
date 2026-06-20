use lambda_http::{Error, run, service_fn};
use telegram_wikimedia_commons_uploader_bot::app::handle_lambda_request;

/// Starts the AWS Lambda HTTP runtime for the Telegram webhook.
#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .without_time()
        .init();
    run(service_fn(|request| async move {
        handle_lambda_request(request)
            .await
            .map_err(|error| error.to_string())
    }))
    .await
}

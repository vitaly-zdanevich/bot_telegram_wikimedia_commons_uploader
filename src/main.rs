use lambda_http::{Error, run, service_fn};
use telegram_wikimedia_commons_uploader_bot::app::{
    handle_lambda_request, run_polling, run_webhook_server,
};

/// Starts the bot in AWS Lambda, standalone webhook server, or long-polling mode.
///
/// Lambda is auto-detected via `AWS_LAMBDA_RUNTIME_API`; set `BOT_MODE=webhook` to run
/// a standalone HTTP server, or `BOT_MODE=polling` for long polling.
#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .without_time()
        .init();

    match runtime_mode() {
        RuntimeMode::Polling => {
            return run_polling()
                .await
                .map_err(|error| Error::from(format!("{error:#}")));
        }
        RuntimeMode::WebhookServer => {
            return run_webhook_server()
                .await
                .map_err(|error| Error::from(format!("{error:#}")));
        }
        RuntimeMode::Lambda => {}
    }

    run(service_fn(|request| async move {
        handle_lambda_request(request)
            .await
            .map_err(|error| error.to_string())
    }))
    .await
}

enum RuntimeMode {
    Lambda,
    WebhookServer,
    Polling,
}

/// Chooses long-polling by default outside AWS Lambda unless `BOT_MODE` is explicit.
fn runtime_mode() -> RuntimeMode {
    if let Ok(mode) = std::env::var("BOT_MODE") {
        return match mode.to_ascii_lowercase().as_str() {
            "lambda" => RuntimeMode::Lambda,
            "webhook" | "server" => RuntimeMode::WebhookServer,
            "polling" => RuntimeMode::Polling,
            _ => RuntimeMode::Polling,
        };
    }

    if std::env::var("AWS_LAMBDA_RUNTIME_API").is_ok() {
        RuntimeMode::Lambda
    } else {
        RuntimeMode::Polling
    }
}

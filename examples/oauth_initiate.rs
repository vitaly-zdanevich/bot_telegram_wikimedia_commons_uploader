//! Live smoke test for the OAuth 1.0a signing — no browser or Telegram needed.
//!
//! Calls `Special:OAuth/initiate` on meta.wikimedia.org and prints the authorize URL.
//! If it succeeds, MediaWiki accepted our HMAC-SHA1 signature (the part not covered by
//! the offline unit tests). Your consumer secret stays in your shell env, never in code.
//!
//! Run:
//!   OAUTH_CONSUMER_KEY=... OAUTH_CONSUMER_SECRET=... cargo run --example oauth_initiate
//!
//! Then open the printed URL, authorize, and you'll be shown the verification code —
//! that confirms the whole request-token step works before you wire up the bot.

use telegram_wikimedia_commons_uploader_bot::oauth::{Consumer, OAuthClient, OAuthEndpoints};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let key = std::env::var("OAUTH_CONSUMER_KEY")
        .map_err(|_| anyhow::anyhow!("set OAUTH_CONSUMER_KEY in the environment"))?;
    let secret = std::env::var("OAUTH_CONSUMER_SECRET")
        .map_err(|_| anyhow::anyhow!("set OAUTH_CONSUMER_SECRET in the environment"))?;

    let client = OAuthClient::new(
        Consumer { key, secret },
        OAuthEndpoints::wikimedia(),
        "commons-uploader-oauth-smoke/0.1 (https://github.com/vitaly-zdanevich/bot_telegram_wikimedia_commons_uploader)",
    )?;

    println!("Requesting a temporary token from meta.wikimedia.org …");
    let (request_token, _request_secret) = client.initiate().await?;
    println!("✅ initiate OK — MediaWiki accepted the signature.");
    println!("   request token: {request_token}");
    println!(
        "\nOpen this URL, authorize, and you'll get a verification code:\n{}",
        client.authorize_url(&request_token)
    );
    Ok(())
}

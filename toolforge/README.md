# Toolforge deployment (long-living server)

This is the **Toolforge** counterpart to the AWS Lambda deployment in `infra/`. Toolforge is
a managed container platform (not a VM and **not** Terraform-managed), so "infra as code"
here means these manifests + the `toolforge` CLI. The AWS `infra/` Terraform is unaffected.

> Status: **starting scaffold.** Toolforge's build/runtime details evolve — confirm command
> names against the current [Toolforge docs](https://wikitech.wikimedia.org/wiki/Help:Toolforge)
> as you go. The bot runs as a **continuous job** doing long polling (`BOT_MODE=polling`),
> with **SQLite** storage on the tool's data dir.

## 1. Get a tool account
Request/create a tool at <https://toolsadmin.wikimedia.org/>, then from a bastion:

```bash
ssh login.toolforge.org
become YOURTOOL
```

## 2. Set configuration as envvars (secrets stay out of the image)

```bash
toolforge envvars create BOT_MODE                 # value: polling
toolforge envvars create TELEGRAM_BOT_TOKEN        # from @BotFather
toolforge envvars create CREDENTIAL_ENC_KEY        # openssl rand -base64 32
toolforge envvars create SQLITE_PATH               # e.g. /data/project/YOURTOOL/bot.sqlite
# OAuth 1.0a consumer (from Special:OAuthConsumerRegistration, Upload grant):
toolforge envvars create OAUTH_CONSUMER_KEY
toolforge envvars create OAUTH_CONSUMER_SECRET
# Optional self-hosted Bot API server for ~2 GB files (see step 5):
# toolforge envvars create TELEGRAM_API_BASE       # e.g. http://localhost:8081
```

## 3. Build the image
The Build Service installs packages from `toolforge/Aptfile` (e.g. `unar` for RAR) and
compiles the bot:

```bash
toolforge build start https://github.com/vitaly-zdanevich/bot_telegram_wikimedia_commons_uploader
toolforge build show     # wait until it succeeds
```

If a Rust buildpack isn't available on your Toolforge, build the binary in CI instead
(a portable build with `--features sqlite,archive,rar` and **without** `heic` needs only
glibc + `unar` at runtime) and run it under a base image.

## 4. Run the continuous job

```bash
toolforge jobs load toolforge/jobs.yaml   # edit image: tool-YOURTOOL/... first
toolforge jobs list
toolforge jobs logs commons-uploader-bot
```

`scripts/toolforge-deploy.sh` wraps steps 3–4.

## 5. (Optional) self-hosted Bot API server for ~2 GB files
Without it the public Bot API caps downloads at 20 MB. To lift that, run
[`telegram-bot-api`](https://github.com/tdlib/telegram-bot-api) (needs `api_id`/`api_hash`
from <https://my.telegram.org>) and point the bot at it via `TELEGRAM_API_BASE`. Because
two Toolforge jobs don't share `localhost`, the simplest layout is to bundle the API server
and the bot in **one** job (a wrapper that starts the server in the background, then the bot).

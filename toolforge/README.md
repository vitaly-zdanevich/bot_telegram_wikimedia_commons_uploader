# Toolforge deployment (long-living server)

This is the **Toolforge** counterpart to the AWS Lambda deployment in `infra/`. Toolforge is
a managed container platform (not a VM and **not** Terraform-managed), so "infra as code"
here means these manifests + the `toolforge` CLI. The AWS `infra/` Terraform is unaffected.

> Status: **starting scaffold.** Toolforge's build/runtime details evolve — confirm command
> names against the current [Toolforge docs](https://wikitech.wikimedia.org/wiki/Help:Toolforge)
> as you go. The bot runs as a **continuous job** doing long polling (`BOT_MODE=polling`),
> with **SQLite** storage on the tool's data dir.

## TL;DR — first-deploy checklist

**Prerequisites:** a Wikimedia developer (LDAP) account with an **SSH key** uploaded, approved
**Toolforge membership**, and (optional) an **OAuth 1.0a consumer** — the bot works on a bot
password without it. Nothing to install locally; the `toolforge` CLI lives on the bastion.

1. **SSH in:** `ssh login.toolforge.org` (§1).
2. **Create/enter the tool:** make it at [toolsadmin](https://toolsadmin.wikimedia.org/), then `become YOURTOOL`.
3. **Set secrets** as envvars: `BOT_MODE=polling`, `TELEGRAM_BOT_TOKEN`, `CREDENTIAL_ENC_KEY`, `SQLITE_PATH`, optional `OAUTH_CONSUMER_KEY`/`OAUTH_CONSUMER_SECRET` (§2).
4. **Build** from Git: `toolforge build start <repo>` → `toolforge build show` (§3).
5. **Run:** edit the image name in `toolforge/jobs.yaml`, then `toolforge jobs load toolforge/jobs.yaml` (§4).
6. **Verify:** `toolforge jobs list`, `toolforge jobs logs commons-uploader-bot`, then message the bot `/start`.

OAuth is **additive** — deploy on a bot password now; add the consumer envvars later and restart
the job (`toolforge jobs restart commons-uploader-bot`) to pick them up.

## 1. Get a tool account & SSH in
You need a Wikimedia developer (LDAP) account with an **SSH key** registered
([toolsadmin](https://toolsadmin.wikimedia.org/) → Striker, or Wikitech preferences) and approved
**Toolforge membership**. The `toolforge` CLI is **preinstalled on the bastion** — there is
nothing to install on your machine (and don't try: it authenticates with a Kubernetes cert that
lives in the tool's home). Create/own a tool, then:

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
# OAuth 1.0a consumer (Special:OAuthConsumerRegistration). When registering, you MUST:
#   - choose OAuth 1.0a (not 2.0),
#   - tick "Allow consumer to specify a callback in requests" (enables the 'oob' flow),
#   - grant BOTH "Upload new files" AND "Create, edit, and move pages".
# These are fixed at registration — a wrong choice means re-registering. You may use a
# proposed consumer as its owner before approval (good for self-testing).
toolforge envvars create OAUTH_CONSUMER_KEY
toolforge envvars create OAUTH_CONSUMER_SECRET
# Optional self-hosted Bot API server for ~2 GB files (see step 5):
# toolforge envvars create TELEGRAM_API_BASE       # e.g. http://localhost:8081
```

## 3. Build the image
The Build Service installs packages from the repo-root `Aptfile` (e.g. `unar` for RAR) and
compiles the bot. The repo-root `project.toml` sets `RUSTFLAGS=-C target-cpu=native` so the
binary is tuned for the build node's CPU:

```bash
toolforge build start https://github.com/vitaly-zdanevich/bot_telegram_wikimedia_commons_uploader
toolforge build show     # wait until it succeeds
```

> **`target-cpu=native` caveat:** it targets the *build* node's CPU, and Toolforge's run node
> may differ. If the bot ever crashes with **illegal instruction (SIGILL)**, change
> `project.toml` to `-C target-cpu=x86-64-v3` (portable across modern servers) and rebuild.

If a Rust buildpack isn't available on your Toolforge, build the binary in CI instead (a
portable build with `--features sqlite,archive,rar` and **without** `heic` needs only glibc +
`unar` at runtime; pass `RUSTFLAGS=-C target-cpu=native` to that build) and run it under a
base image.

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

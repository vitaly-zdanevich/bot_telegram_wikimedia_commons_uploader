# Toolforge deployment

This is the **Toolforge** counterpart to the AWS Lambda deployment in `infra/`. Toolforge is
a managed container platform (not a VM and **not** Terraform-managed), so "infra as code"
here means these manifests + the `toolforge` CLI. The AWS `infra/` Terraform is unaffected.

The preferred Toolforge deployment is a **webservice** receiving Telegram webhooks
(`BOT_MODE=webhook`) with **SQLite** storage on the tool's data dir. The older continuous
long-polling job (`BOT_MODE=polling`) remains available as a fallback.

## TL;DR — first-deploy checklist

**Prerequisites:** a Wikimedia developer (LDAP) account with an **SSH key** uploaded, approved
**Toolforge membership**, and (optional) an **OAuth 1.0a consumer** — the bot works on a bot
password without it. Nothing to install locally; the `toolforge` CLI lives on the bastion.

1. **SSH in:** `ssh vitaly-zdanevich@login.toolforge.org` (§1).
2. **Create/enter the tool:** make it at [toolsadmin](https://toolsadmin.wikimedia.org/), then `become YOURTOOL`.
3. **Set secrets** as envvars: `BOT_MODE=webhook`, `TELEGRAM_BOT_TOKEN`, `TELEGRAM_WEBHOOK_SECRET`, `CREDENTIAL_ENC_KEY`, `SQLITE_PATH`, optional `OAUTH_CONSUMER_KEY`/`OAUTH_CONSUMER_SECRET` and local Bot API credentials (§2, §5).
4. **Build** from Git: `toolforge build start <repo>` → `toolforge build show` (§3).
5. **Run:** `cp toolforge/service.template ~/service.template`, then `toolforge webservice buildservice start --mount=all --health-check-path=/healthz` (§4).
6. **Register webhook:** locally run `TOOLFORGE_TOOL=YOURTOOL ./scripts/set-webhook.sh`.
7. **Verify:** `toolforge webservice status`, `toolforge webservice buildservice logs -f`
   or locally `./scripts/toolforge-logs.sh -f`, then message the bot `/start`.

OAuth is **additive** — deploy on a bot password now; add the consumer envvars later and restart
the job (`toolforge jobs restart commons-uploader-bot`) to pick them up.

## 1. Get a tool account & SSH in
You need a Wikimedia developer (LDAP) account with an **SSH key** registered
([toolsadmin](https://toolsadmin.wikimedia.org/) → Striker, or Wikitech preferences) and approved
**Toolforge membership**. The `toolforge` CLI is **preinstalled on the bastion** — there is
nothing to install on your machine (and don't try: it authenticates with a Kubernetes cert that
lives in the tool's home). Create/own a tool, then:

```bash
ssh vitaly-zdanevich@login.toolforge.org
become YOURTOOL
```

## 2. Set configuration as envvars (secrets stay out of the image)

```bash
toolforge envvars create BOT_MODE                 # value: webhook
toolforge envvars create TELEGRAM_BOT_TOKEN        # from @BotFather
toolforge envvars create TELEGRAM_WEBHOOK_SECRET   # random secret, also used by set-webhook.sh
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
# Optional local Bot API server in the same webservice pod (see step 5):
# toolforge envvars create TELEGRAM_API_ID         # from https://my.telegram.org/apps
# toolforge envvars create TELEGRAM_API_HASH       # from https://my.telegram.org/apps
# toolforge envvars create TELEGRAM_BOT_API_CLOUD_LOGOUT 1  # first local switch only
# toolforge envvars create MAX_FILE_MB             # optional; keep conservative until streaming
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

## 4. Run the webhook webservice

```bash
cp toolforge/service.template ~/service.template
toolforge webservice buildservice start --mount=all --health-check-path=/healthz
toolforge webservice status
toolforge webservice buildservice logs -f
```

`scripts/toolforge-webhook-deploy.sh` wraps steps 3–4 when run from a checkout on the bastion
after `become YOURTOOL`.

From a local checkout, `scripts/toolforge-logs.sh` SSHes to the bastion and reads the
webservice logs with `kubectl logs`. Common forms:

```bash
./scripts/toolforge-logs.sh
./scripts/toolforge-logs.sh --since 2h --errors
./scripts/toolforge-logs.sh --follow
```

To keep using long polling instead, set `BOT_MODE=polling`, edit the image in
`toolforge/jobs.yaml`, and run:

```bash
toolforge jobs load toolforge/jobs.yaml
toolforge jobs logs commons-uploader-bot -f
```

## 5. (Optional) local Bot API server
Without it the public Bot API caps downloads at 20 MB. To lift that, place the official
[`telegram-bot-api`](https://github.com/tdlib/telegram-bot-api) executable at:

```bash
/data/project/YOURTOOL/bin/telegram-bot-api
```

Then set `TELEGRAM_API_ID` and `TELEGRAM_API_HASH` from <https://my.telegram.org/apps>.
`Procfile` starts `scripts/run-toolforge-webhook.sh`, which launches `telegram-bot-api`
on `127.0.0.1:8081` in `--local` mode, exports `TELEGRAM_API_BASE` for the Rust bot, and
registers a loopback webhook to `http://127.0.0.1:$PORT/telegram`.

For the first switch from Telegram's cloud Bot API, set:

```bash
toolforge envvars create TELEGRAM_BOT_API_CLOUD_LOGOUT 1
```

Remove it after a successful local start. The server can handle much larger files, but the
Rust bot still buffers downloads in memory, so raise `MAX_FILE_MB` deliberately.

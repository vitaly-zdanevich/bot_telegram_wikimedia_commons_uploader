# Telegram → Wikimedia Commons uploader bot

<p align="center">
  <img src="logo.png" alt="Telegram to Wikimedia Commons uploader bot logo" width="180">
</p>

[![Quality Gate Status](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_bot_telegram_wikimedia_commons_uploader&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_bot_telegram_wikimedia_commons_uploader)
[![Maintainability Rating](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_bot_telegram_wikimedia_commons_uploader&metric=sqale_rating)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_bot_telegram_wikimedia_commons_uploader)
[![Coverage](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_bot_telegram_wikimedia_commons_uploader&metric=coverage)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_bot_telegram_wikimedia_commons_uploader)
[![Lines of Code](https://sloc.xyz/github/vitaly-zdanevich/bot_telegram_wikimedia_commons_uploader)](https://github.com/vitaly-zdanevich/bot_telegram_wikimedia_commons_uploader)

Public Telegram bot — **[@wikimedia_commons_uploader_bot](https://t.me/wikimedia_commons_uploader_bot)** —
that uploads images and media you send straight to **Wikimedia Commons, under your own
account**. Written in Rust, it runs on AWS Lambda (arm64) behind a Telegram webhook,
with DynamoDB for per-user settings. Designed to stay within the AWS free tier.

## How it works (for users)

1. Send `/start`. The bot asks you to create a **scoped Bot Password** at
   <https://commons.wikimedia.org/wiki/Special:BotPasswords> — tick **Upload new files** and
   **Create, edit, and move pages** (the second is needed to write each file's description
   page), never your real password. You get a username like `YourName@telegram` and a password.
2. Send the bot-password **username**, then the **bot password** (the bot deletes your
   message immediately and stores it **AES-256-GCM encrypted**).
3. Pick a **license** (default **CC BY 4.0**; also CC BY-SA 4.0, CC0,
   PD-Russia-expired, PD-Russia, or PD-RusEmpire) and an optional **filename prefix**.
4. Send a photo or file. It is uploaded to Commons with a generated `{{Information}}`
   page, license, categories, geotag, and provenance.

### Captions

A caption becomes the file description. Extra directive lines (any order,
case-insensitive) are recognised — and for an **album** the single caption applies to
every photo:

```
A view of the old town
Categories: Minsk, Architecture of Belarus
Source: https://example.com/photo/
Author: Jane Doe
Date: 2009-12-03
```

`Categories:` accepts many comma-separated names. `Source:`/`Author:`/`Date:` override the
defaults (own work / your account / EXIF date). EXIF `DateTimeOriginal` and GPS are read
automatically (date → `{{Information}}`, GPS → `{{Location dec}}`).

Each file is named **`<your prefix> <caption text> <original name>`**, with emoji and line
breaks stripped — e.g. caption "Minsk trip" + `IMG_5638.DNG` → `Minsk trip IMG_5638.webp`.
The caption becomes a descriptive prefix on every photo of an album, and the original name
keeps them unique (bare generic names like `IMG_5638` are otherwise blocked by Commons).

### Directives

**In a caption** (per file; shared across an album), as `Keyword: value` lines:

| Directive | Purpose |
| --------- | ------- |
| `Categories:` / `Category:` / `c:` | categories (comma-separated; multiple lines allowed) |
| `Source:` | source (defaults to `{{own}}`) |
| `Author:` / `a:` | author |
| `Date:` | date, e.g. `2009-12-03` |
| `Coord:` / `Coordinates:` / `Location:` / `GPS:` | a Google / Yandex / 2GIS / OpenStreetMap link, or plain `lat, lon` |

**As a plain message** to set your defaults for future uploads (colon optional; short aliases):

| Directive | Alias | Sets |
| --------- | ----- | ---- |
| `category` | `c` | default categories (added to every upload) |
| `author` | `a` | default author |
| `prefix` | `p` | filename prefix |
| `description` / `caption` | `d` / `cap` | default description |
| `language` | `lang` | description language (e.g. `ru` → `{{ru\|…}}`) |
| `license` | `l` | custom license: a key (`cc-by-4.0`), a template (`{{PD-RU-exempt}}` or `PD-RU-exempt`), or free text |

Examples: `c Minsk, Belarus` · `author Jane Doe` · `license {{PD-RU-exempt}}` ·
`Coord: https://www.openstreetmap.org/?mlat=55.75&mlon=37.61`

### Accepted formats

Uploaded as-is when Commons accepts them: **JPEG, PNG, GIF, SVG, TIFF, WebP, XCF, PDF,
DjVu, STL**, audio **WAV, MP3, OGA, OGG, Opus, FLAC, MID/MIDI** (voice messages too), and
video **WebM, OGV, MPG/MPEG** — see [Commons: File types](https://commons.wikimedia.org/wiki/Commons:File_types).

Converted first, because Commons does **not** accept them:

| Input | Converted to | Notes |
| ----- | ------------ | ----- |
| DNG (raw) | WebP (lossy) by default, or embedded JPEG | default is raw development to WebP with embedded-JPEG fallback; `/settings dng extract` uploads the DNG's embedded JPEG preview directly; original SHA-1/MD5 + filename recorded |
| HEIC/HEIF | WebP (lossy) | decoded with libheif; needs the libheif build (see below) |
| BMP | WebP (lossless) | |

> AVIF is **not** accepted by Commons (still a wishlist item), which is why DNG/HEIC/BMP
> are converted to WebP rather than AVIF.

**File size:** the Telegram cloud Bot API only lets bots
[**download files up to 20 MB**](https://core.telegram.org/bots/api#getfile), so larger
files are rejected with a clear message (this is a Telegram limit, not a Commons one). Send
originals as a **file/document** for full quality.

**Wikimedia IP blocks:** Wikimedia globally blocks many data-centre IP ranges (including
AWS) as "open proxy/webhost", so uploads from Lambda can be refused with a `blocked` error.
Affected uploads need the account to hold a global IP-block exemption, or the bot to route
Commons traffic through a non-blocked IP (e.g. an outbound proxy). The bot replies with a
clear message when this happens.

Set the `commons_proxy` Terraform variable (env `COMMONS_PROXY`) to route Commons
login/upload traffic through a clean IP — for example a small proxy on **Wikimedia Cloud
VPS / Toolforge**, whose IPs are not blocked. Then `terraform apply` to update the Lambda.

### Archives (zip / rar) — server build only

On the long-living server build (not Lambda), send a **`.zip`** and the bot uploads the
images inside it, sharing one caption/categories/license across them. Two `/settings`
toggles control the flow:

- **Show file list** (off by default) — reply with the names of the images found.
- **Confirm before upload** (on by default) — send thumbnails of all previewable images and
  a **Start upload** / **Cancel** button; nothing is uploaded until you confirm. If an
  archive contains a generic `IMG_...` filename, the bot asks for a filename prefix before
  upload. Archive previews try the original extracted image by default and fall back to a small
  JPEG thumbnail if Telegram rejects the original preview; set `ARCHIVE_THUMBNAIL_RESIZE=true`
  (Terraform: `archive_thumbnail_resize = true`) to resize previews first.

ZIP works out of the box (pure Rust). **RAR** shells out to a system extractor at runtime —
`unar` (free, in Debian main) preferred, `unrar` as fallback — behind the extra `rar` Cargo
feature, so there's no native build dependency. Install `unar` on the host (on Toolforge,
add it to the build `Aptfile`). Archives are disabled on the Lambda build because Telegram's
20 MB download cap makes multi-image archives impractical there.

### Commands & settings

- `/start` — connect your account / resume setup
- `/help` — usage, your uploads link, related projects, contact
- `/settings` — license, filename prefix, default categories, DNG handling, and toggles:
  return upload links (**on** by default), return category links (**off**), return
  non-existing category links (**off**).
  On the server build, two more: show an archive's file list (**off**), and require a
  thumbnail + **Confirm** step before uploading an archive (**on**)
- `/forget` — delete your stored credentials and settings
- `/stat` — admins only: total users and uploads

## Deploy

Prerequisites: AWS credentials, Terraform ≥ 1.6, Rust (the build script installs a
project-local toolchain + `cargo-lambda` if missing), and a Telegram bot token from
[@BotFather](https://t.me/BotFather).

1. **Put your secrets in `infra/terraform.tfvars`** (gitignored) — copy the example:

   ```bash
   cp infra/terraform.tfvars.example infra/terraform.tfvars
   ```

   ```hcl
   telegram_bot_token      = "123456:your-bot-token"      # from @BotFather
   telegram_webhook_secret = "some-random-string"
   credential_enc_key      = "base64-32-bytes"            # openssl rand -base64 32
   admin_telegram_user_ids = "123456789"                  # optional, for /stat
   ```

   (You can instead export `TF_VAR_telegram_bot_token`, etc.)

2. Deploy (builds the Lambda zip, applies Terraform, sets the webhook):

   ```bash
   ./scripts/deploy.sh
   ```

3. Update only the code later:

   ```bash
   ./scripts/update-code.sh
   ```

### HEIC support (libheif)

HEIC decoding needs the C/C++ `libheif` library, which does not cross-compile cleanly,
so it is behind the Cargo `heic` feature. `./scripts/build-lambda.sh` builds **with HEIC
via Docker** (`scripts/build-lambda-docker.sh`, an arm64 Amazon Linux 2023 image that
compiles libheif + libde265) when Docker is available, and otherwise falls back to a fast
`cargo-lambda`/zig cross-build **without** HEIC (DNG and BMP still convert). Force the
fast build with `HEIC=0 ./scripts/build-lambda.sh`.

### Long-living server (Toolforge / Cloud VPS)

Besides Lambda, the same binary runs on **Wikimedia Toolforge / Cloud VPS**, whose IPs
Commons does not block. On Toolforge, set `BOT_MODE=webhook` and run it as a build-service
webservice at `https://<tool>.toolforge.org/telegram`; for a private VM/systemd service,
set `BOT_MODE=polling` to call `getUpdates` in a loop instead. Pair either mode with a self-hosted
[Telegram Bot API server](https://github.com/tdlib/telegram-bot-api) to raise the file
limit from 20 MB to larger files; the Toolforge wrapper starts a local server automatically
when `/data/project/<tool>/bin/telegram-bot-api` and `TELEGRAM_API_ID/HASH` are present.

Storage uses **SQLite** instead of DynamoDB — build with the `sqlite` feature and set
`SQLITE_PATH`. Add archive support (and optionally RAR):

```bash
# ZIP archives + SQLite storage
cargo build --release --features sqlite,archive
# add RAR (shells out to unar/unrar at runtime, e.g. apt install unar)
cargo build --release --features sqlite,archive,rar
```

Operate it with `scripts/server-logs.sh` (journald logs) and `scripts/server-status.sh`
(service state, memory, recent errors, Telegram `getMe`); both take the systemd unit name
via the `SERVICE` env var (default `commons-uploader-bot`).

### Scripts

| Script | Purpose |
| ------ | ------- |
| `scripts/deploy.sh` | Build, `terraform apply`, set webhook |
| `scripts/build-lambda.sh` | Build the arm64 Lambda zip (HEIC via Docker, or `HEIC=0`) |
| `scripts/build-lambda-docker.sh` | Build the zip with HEIC inside Docker |
| `scripts/update-code.sh` | Rebuild and push only the Lambda code |
| `scripts/set-webhook.sh` | Point Telegram at `WEBHOOK_URL`, `TOOLFORGE_TOOL`, or the Lambda Function URL |
| `scripts/toolforge-webhook-deploy.sh` | Build and start the Toolforge webhook webservice |
| `scripts/toolforge-deploy.sh` | Build and load the older Toolforge long-polling job |
| `scripts/show-logs.sh` | Read CloudWatch logs (`--since 2h`, `--errors`, `--follow`) — Lambda |
| `scripts/toolforge-logs.sh` | Read Toolforge webservice logs (`--tail 200`, `--since 2h`, `--errors`, `--follow`) |
| `scripts/server-logs.sh` | Read journald logs (`--since 2h`, `--errors`, `--follow`) — server |
| `scripts/server-status.sh` | Server health: service state, memory, recent errors, `getMe` |

## Cost — AWS free tier

A personal bot stays at **$0/month**: Lambda (1M req + 400k GB-s free, arm64), DynamoDB
(25 GB + 25 RCU/WCU always-free; provisioned 5/5 here), CloudWatch Logs (5 GB free,
14-day retention), and the Lambda Function URL (no extra charge). There is **no KMS**
(a customer key would cost $1/month) — credentials are encrypted in-app with AES-256-GCM
using a key kept in a Lambda environment variable. See [AWS Free Tier](https://aws.amazon.com/free/),
[Lambda pricing](https://aws.amazon.com/lambda/pricing/), and [DynamoDB pricing](https://aws.amazon.com/dynamodb/pricing/).

The default region is `us-east-1`, closest to Wikimedia's eqiad (Ashburn, VA) write
datacenter. Lambda defaults to 3008 MB and the maximum 900 s (15 min) timeout.

## Security

- **Two ways to connect** (the bot offers both at `/start`):
  - **OAuth** (recommended) — authorize on-wiki and paste back a short verification code; no
    password is shared. Uses MediaWiki OAuth 1.0a out-of-band, so there's **no callback
    endpoint** to host (works on Lambda and Toolforge alike). Set `OAUTH_CONSUMER_KEY` /
    `OAUTH_CONSUMER_SECRET` from
    [Special:OAuthConsumerRegistration](https://meta.wikimedia.org/wiki/Special:OAuthConsumerRegistration)
    (Upload grant) to enable it.
  - **Bot password** — a **scoped** [Bot Password](https://commons.wikimedia.org/wiki/Special:BotPasswords)
    (grants: “Upload new files” + “Create, edit, and move pages”), revocable any time.
- Stored credentials (the bot-password token, or the OAuth token+secret) are **AES-256-GCM
  encrypted** before storage and decrypted only in memory per upload; the bot deletes the
  Telegram message containing a bot password.
- The webhook is protected by a secret header; IAM is scoped to the one DynamoDB table.
- Each upload is your own work under your own account, with the attribution category
  `Uploaded with Telegram bot @wikimedia_commons_uploader_bot by Vitaly Zdanevich`.

## Need help uploading to Commons?

Message the author **[@vitaly_zdanevich](https://t.me/vitaly_zdanevich)** — happy to help
with this bot or with uploading to Wikimedia Commons in general.

## Related projects

- [bot_telegram_wikimedia_commons](https://github.com/vitaly-zdanevich/bot_telegram_wikimedia_commons) ([@wikimedia_commons_bot](https://t.me/wikimedia_commons_bot)) — Telegram bot to **search/get** Commons media.
- [Browser extension to upload to Commons](https://gitlab.com/vitaly-zdanevich-extensions/uploading-to-wikimedia-commons).
- [pwb_wrapper_for_simpler_uploading_to_commons](https://gitlab.com/vitaly_zdanevich_wikimedia/pwb_wrapper_for_simpler_uploading_to_commons) — CLI (Pywikibot wrapper) for simpler Commons uploads.
- [gThumb Wikimedia Commons extension](https://gitlab.com/vitaly_zdanevich_wikimedia/gthumb-wikimedia-commons-extension).
- [wikipedia-userstyle-dark-minimum](https://github.com/vitaly-zdanevich/wikipedia-userstyle-dark-minimum) — dark Wikipedia theme.
- [wiki2man_on_rust](https://gitlab.com/vitaly_zdanevich_wikimedia/wiki2man_on_rust) — convert Wikipedia articles to man pages.

## License

MIT — see [LICENSE](LICENSE).

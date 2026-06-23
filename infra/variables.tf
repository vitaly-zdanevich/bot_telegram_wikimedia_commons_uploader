variable "aws_region" {
  type        = string
  description = "AWS region. us-east-1 is closest to Wikimedia's eqiad (Ashburn, VA) write datacenter."
  default     = "us-east-1"
}

variable "project_name" {
  type        = string
  description = "Name prefix for AWS resources."
  default     = "telegram-wikimedia-commons-uploader-bot"
}

variable "lambda_zip_path" {
  type        = string
  description = "Path to the Lambda zip produced by scripts/build-lambda.sh."
  default     = "../build/lambda.zip"
}

variable "telegram_bot_token" {
  type        = string
  description = "Telegram bot token from BotFather. Stored in Terraform state."
  sensitive   = true
}

variable "telegram_webhook_secret" {
  type        = string
  description = "Secret sent by Telegram in the X-Telegram-Bot-Api-Secret-Token header."
  sensitive   = true
}

variable "credential_enc_key" {
  type        = string
  description = "Base64-encoded 32-byte key for AES-256-GCM encryption of user bot-password tokens. Generate with: openssl rand -base64 32"
  sensitive   = true
}

variable "admin_telegram_user_ids" {
  type        = string
  description = "Comma-separated Telegram user IDs allowed to use /stat."
  default     = ""
}

variable "github_url" {
  type        = string
  description = "Project URL shown in /help and the Commons User-Agent."
  default     = "https://github.com/vitaly-zdanevich/bot_telegram_wikimedia_commons_uploader"
}

variable "commons_proxy" {
  type        = string
  description = "Optional HTTP(S) proxy URL for Commons traffic, to upload from a non-blocked IP (Wikimedia blocks AWS data-centre IPs). E.g. a proxy on Wikimedia Cloud VPS."
  default     = ""
}

variable "oauth_consumer_key" {
  type        = string
  description = "Optional OAuth 1.0a consumer key (Special:OAuthConsumerRegistration) so users can connect via OAuth instead of a bot password."
  default     = ""
}

variable "oauth_consumer_secret" {
  type        = string
  description = "OAuth 1.0a consumer secret paired with oauth_consumer_key."
  default     = ""
  sensitive   = true
}

variable "default_license" {
  type        = string
  description = "Default license key: cc-by-4.0, cc-by-sa-4.0, or cc-zero."
  default     = "cc-by-4.0"
}

variable "webp_quality" {
  type        = number
  description = "Lossy WebP quality (1-100) used when converting DNG/HEIC."
  default     = 90
}

variable "max_file_mb" {
  type        = number
  description = "Maximum file size the bot downloads from Telegram. The cloud Bot API cap is 20 MB."
  default     = 20
}

variable "lambda_memory_size" {
  type        = number
  description = "Lambda memory in MB. 3008 (~3 GB) is the most an account gets without a limit increase; raise toward 10240 after one."
  default     = 3008
}

variable "lambda_timeout_seconds" {
  type        = number
  description = "Lambda timeout. 900 seconds is the maximum (15 minutes)."
  default     = 900
}

variable "dynamodb_read_capacity" {
  type        = number
  description = "Provisioned DynamoDB RCUs. Keep low to stay in the always-free 25 RCU tier."
  default     = 5
}

variable "dynamodb_write_capacity" {
  type        = number
  description = "Provisioned DynamoDB WCUs. Keep low to stay in the always-free 25 WCU tier."
  default     = 5
}

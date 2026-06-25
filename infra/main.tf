data "aws_iam_policy_document" "lambda_assume_role" {
  statement {
    actions = ["sts:AssumeRole"]

    principals {
      type        = "Service"
      identifiers = ["lambda.amazonaws.com"]
    }
  }
}

locals {
  dynamodb_table_name = "${var.project_name}-data"

  environment_variables = {
    PROJECT_NAME             = var.project_name
    TELEGRAM_BOT_TOKEN       = var.telegram_bot_token
    TELEGRAM_WEBHOOK_SECRET  = var.telegram_webhook_secret
    CREDENTIAL_ENC_KEY       = var.credential_enc_key
    ADMIN_TELEGRAM_USER_IDS  = var.admin_telegram_user_ids
    GITHUB_URL               = var.github_url
    DYNAMODB_TABLE           = aws_dynamodb_table.data.name
    DEFAULT_LICENSE          = var.default_license
    WEBP_QUALITY             = tostring(var.webp_quality)
    MAX_FILE_MB              = tostring(var.max_file_mb)
    ARCHIVE_THUMBNAIL_RESIZE = tostring(var.archive_thumbnail_resize)
    COMMONS_USER_AGENT       = "${var.project_name}/0.1 (${var.github_url})"
    COMMONS_PROXY            = var.commons_proxy
    OAUTH_CONSUMER_KEY       = var.oauth_consumer_key
    OAUTH_CONSUMER_SECRET    = var.oauth_consumer_secret
    RUST_LOG                 = "info"
  }
}

resource "aws_iam_role" "lambda" {
  name               = var.project_name
  assume_role_policy = data.aws_iam_policy_document.lambda_assume_role.json
}

resource "aws_iam_role_policy_attachment" "lambda_basic" {
  role       = aws_iam_role.lambda.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole"
}

data "aws_iam_policy_document" "lambda_app" {
  statement {
    actions = [
      "dynamodb:GetItem",
      "dynamodb:PutItem",
      "dynamodb:DeleteItem",
      "dynamodb:Scan",
      "dynamodb:DescribeTable"
    ]
    resources = [aws_dynamodb_table.data.arn]
  }
}

resource "aws_iam_role_policy" "lambda_app" {
  name   = "${var.project_name}-app"
  role   = aws_iam_role.lambda.id
  policy = data.aws_iam_policy_document.lambda_app.json
}

resource "aws_cloudwatch_log_group" "lambda" {
  name              = "/aws/lambda/${var.project_name}"
  retention_in_days = 14
}

resource "aws_dynamodb_table" "data" {
  name           = local.dynamodb_table_name
  billing_mode   = "PROVISIONED"
  read_capacity  = var.dynamodb_read_capacity
  write_capacity = var.dynamodb_write_capacity
  hash_key       = "pk"
  range_key      = "sk"
  table_class    = "STANDARD"

  attribute {
    name = "pk"
    type = "S"
  }

  attribute {
    name = "sk"
    type = "S"
  }

  ttl {
    attribute_name = "expires_at"
    enabled        = true
  }
}

resource "aws_lambda_function" "bot" {
  function_name = var.project_name
  role          = aws_iam_role.lambda.arn
  filename      = var.lambda_zip_path

  package_type  = "Zip"
  architectures = ["arm64"]
  runtime       = "provided.al2023"
  handler       = "bootstrap"

  memory_size = var.lambda_memory_size
  timeout     = var.lambda_timeout_seconds

  ephemeral_storage {
    size = 10240
  }

  source_code_hash = filebase64sha256(var.lambda_zip_path)

  environment {
    variables = local.environment_variables
  }

  depends_on = [
    aws_cloudwatch_log_group.lambda,
    aws_iam_role_policy.lambda_app
  ]
}

resource "aws_lambda_function_url" "bot" {
  function_name      = aws_lambda_function.bot.function_name
  authorization_type = "NONE"
}

resource "aws_lambda_permission" "function_url" {
  statement_id           = "AllowFunctionUrlInvoke"
  action                 = "lambda:InvokeFunctionUrl"
  function_name          = aws_lambda_function.bot.function_name
  principal              = "*"
  function_url_auth_type = "NONE"
}

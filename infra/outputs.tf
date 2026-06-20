output "function_name" {
  value = aws_lambda_function.bot.function_name
}

output "function_url" {
  value = aws_lambda_function_url.bot.function_url
}

output "dynamodb_table" {
  value = aws_dynamodb_table.data.name
}

#!/bin/bash
set -e

# Debug: test Postgres connectivity before starting the gateway
if [ -n "$DATABASE_URL" ] && echo "$DATABASE_URL" | grep -q "^postgres"; then
  # Extract host and port from DATABASE_URL
  DB_HOST=$(echo "$DATABASE_URL" | sed -n 's|.*@\([^:/]*\).*|\1|p')
  DB_PORT=$(echo "$DATABASE_URL" | sed -n 's|.*:\([0-9]*\)/.*|\1|p')
  DB_PORT=${DB_PORT:-5432}

  echo "Testing Postgres connectivity to $DB_HOST:$DB_PORT..."

  # DNS resolution
  echo "DNS resolution:"
  getent hosts "$DB_HOST" || echo "DNS resolution failed"

  # TCP connectivity
  echo "TCP connectivity test:"
  timeout 10 bash -c "echo > /dev/tcp/$DB_HOST/$DB_PORT" 2>&1 && echo "TCP OK" || echo "TCP FAILED"

  # pg_isready
  echo "pg_isready test:"
  pg_isready -h "$DB_HOST" -p "$DB_PORT" -t 10 2>&1 || echo "pg_isready failed"
fi

exec pensyve-mcp-gateway

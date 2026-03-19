# Track 5: Infrastructure, Deployment & Public Presence — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Production AWS deployment via OpenTofu, pensyve.com website, CI/CD — all build-in-public safe.

**Architecture:** Split across two repos: pensyve (public — Dockerfile, CI workflow, secrets hardening, website) and pensyve-infra (private — OpenTofu modules, deploy workflows, billing). AWS stack: ECS Fargate + Aurora Serverless v2 + ElastiCache Redis + S3 + CloudFront + Route53.

**Tech Stack:** OpenTofu, AWS (ECS, Aurora, ElastiCache, S3, CloudFront, Route53, ECR, Secrets Manager), GitHub Actions, Docker, Astro (website), gitleaks, Stripe

---

## Sprint 1 — Task 5.3: Secrets & Security Hardening (pensyve repo)

> **Priority: MUST be done before anything goes public.** This is the prerequisite for all other Track 5 work and for the repo going open-source.

### Task 5.3.1: Harden `.gitignore`

- [ ] Add comprehensive secret/infrastructure patterns to `pensyve/.gitignore`

**File: `pensyve/.gitignore`** — append these entries after the existing content:

```gitignore
# --- Existing entries (do not remove) ---
target/
__pycache__/
*.egg-info/
dist/
.env
*.db
*.db-journal
models/
*.onnx
.venv/
*.so
*.dylib
*.dSYM/
.fastembed_cache/
benchmarks/results/*.json
.cargo/
node_modules/
pensyve-ts/dist/

# --- Secrets & credentials ---
.env*
!.env.example
*.pem
*.key
*.crt
*.p12
*.pfx
credentials*
*_credentials*
*.keystore
service-account*.json
gcloud-*.json

# --- Infrastructure state (should never be in public repo) ---
terraform.tfstate*
*.tfstate*
*.tfvars
*.tfvars.json
.terraform/
.terraform.lock.hcl
*.tfplan

# --- AWS ---
.aws/
aws-exports.js

# --- Docker ---
docker-compose.override.yml

# --- IDE & OS ---
.idea/
.vscode/settings.json
.vscode/launch.json
*.swp
*.swo
*~
.DS_Store
Thumbs.db

# --- Build artifacts ---
*.whl
*.tar.gz
```

**Verification:**

```bash
cd /home/wshobson/workspace/major7apps/pensyve
# Confirm no secrets currently exist in working tree
git ls-files | xargs grep -l -E "(AKIA[0-9A-Z]{16}|sk-[a-zA-Z0-9]{48}|ghp_[a-zA-Z0-9]{36}|-----BEGIN (RSA |EC )?PRIVATE KEY-----)" 2>/dev/null || echo "No secrets found in tracked files"

# Confirm .gitignore covers .env variants
echo "test" > .env.local && git check-ignore .env.local && rm .env.local
echo "test" > .env.production && git check-ignore .env.production && rm .env.production
echo "test" > secret.pem && git check-ignore secret.pem && rm secret.pem
echo "test" > terraform.tfstate && git check-ignore terraform.tfstate && rm terraform.tfstate
```

- [ ] Git commit: `chore: harden .gitignore for secrets, credentials, and infra state`

---

### Task 5.3.2: Install and configure gitleaks pre-commit hook

- [ ] Create `.pre-commit-config.yaml` in `pensyve/` root

**File: `pensyve/.pre-commit-config.yaml`**

```yaml
# Pre-commit hooks for secret scanning and code quality.
# Install: pip install pre-commit && pre-commit install
# Run manually: pre-commit run --all-files

repos:
  - repo: https://github.com/gitleaks/gitleaks
    rev: v8.22.1
    hooks:
      - id: gitleaks

  - repo: https://github.com/pre-commit/pre-commit-hooks
    rev: v5.0.0
    hooks:
      - id: check-added-large-files
        args: ['--maxkb=1024']
      - id: check-merge-conflict
      - id: detect-private-key
      - id: end-of-file-fixer
      - id: trailing-whitespace
        args: [--markdown-linebreak-ext=md]
```

- [ ] Create `.gitleaks.toml` for custom rules

**File: `pensyve/.gitleaks.toml`**

```toml
# Gitleaks configuration for Pensyve.
# See: https://github.com/gitleaks/gitleaks

title = "pensyve gitleaks config"

[allowlist]
  description = "Global allowlist"
  paths = [
    '''\.lock$''',
    '''\.sum$''',
    '''go\.sum$''',
    '''Cargo\.lock$''',
    '''package-lock\.json$''',
    '''\.fastembed_cache/''',
    '''benchmarks/results/''',
    '''\.onnx$''',
  ]

# Additional rule: flag any PENSYVE_ env var assignment with a literal value
# (these should use env vars or secrets manager, never hardcoded)
[[rules]]
  id = "pensyve-hardcoded-config"
  description = "Hardcoded PENSYVE_ configuration value"
  regex = '''PENSYVE_(API_KEY|SECRET|DB_PASSWORD|STRIPE_KEY)\s*=\s*['"][^'"]+['"]'''
  tags = ["pensyve", "config"]
```

**Verification:**

```bash
cd /home/wshobson/workspace/major7apps/pensyve

# Install pre-commit (into .venv)
.venv/bin/pip install pre-commit

# Install the hooks
.venv/bin/pre-commit install

# Run against all existing files to verify no secrets exist
.venv/bin/pre-commit run --all-files

# Test that gitleaks catches a fake secret
echo 'AWS_SECRET_ACCESS_KEY="wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"' > /tmp/test_secret.txt
gitleaks detect --source /tmp/test_secret.txt && echo "FAIL: gitleaks did not catch secret" || echo "PASS: gitleaks caught the secret"
rm /tmp/test_secret.txt
```

- [ ] Git commit: `chore: add gitleaks pre-commit hook and secret scanning config`

---

### Task 5.3.3: Audit repo history for leaked secrets

- [ ] Run gitleaks against full repo history

```bash
cd /home/wshobson/workspace/major7apps/pensyve

# Scan entire git history
gitleaks detect --source . --verbose --report-path /tmp/gitleaks-audit-report.json

# Review report
cat /tmp/gitleaks-audit-report.json
```

- [ ] If secrets found: document them, rotate credentials, and if necessary use `git filter-repo` to remove from history (confirm with user before rewriting history)
- [ ] Create `.env.example` showing required env vars without values

**File: `pensyve/.env.example`**

```bash
# Pensyve environment configuration
# Copy to .env and fill in values. Never commit .env files.

# Core
PENSYVE_PATH=              # SQLite database path (default: ~/.pensyve/pensyve.db)
PENSYVE_NAMESPACE=default  # Memory namespace

# Server
PENSYVE_HOST=0.0.0.0
PENSYVE_PORT=8000

# Tier 2 Extraction (optional)
PENSYVE_TIER2_ENABLED=false
PENSYVE_TIER2_MODEL_PATH=  # Path to GGUF model file

# Auth (optional, for managed service)
PENSYVE_API_KEY=
PENSYVE_AUTH_ENABLED=false

# Database (managed service — Postgres)
PENSYVE_DATABASE_URL=      # postgres://user:pass@host:5432/pensyve

# Redis (managed service — episode state)
PENSYVE_REDIS_URL=         # redis://host:6379/0

# Stripe (billing, managed service only)
PENSYVE_STRIPE_SECRET_KEY=
PENSYVE_STRIPE_WEBHOOK_SECRET=

# Observability
PENSYVE_LOG_LEVEL=info
PENSYVE_OTEL_ENDPOINT=     # OpenTelemetry collector URL
```

- [ ] Git commit: `chore: add .env.example and audit repo history for secrets`

---

### Task 5.3.4: Add gitleaks to CI (GitHub Actions)

- [ ] Create secrets scanning CI workflow

**File: `pensyve/.github/workflows/secrets-scan.yml`**

```yaml
name: Secret Scanning

on:
  pull_request:
  push:
    branches: [main]

permissions:
  contents: read

jobs:
  gitleaks:
    name: Gitleaks
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0

      - uses: gitleaks/gitleaks-action@v2
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

**Verification:**

```bash
# Validate workflow syntax
cd /home/wshobson/workspace/major7apps/pensyve
cat .github/workflows/secrets-scan.yml | python3 -c "import sys, yaml; yaml.safe_load(sys.stdin); print('YAML valid')"
```

- [ ] Git commit: `ci: add gitleaks secret scanning workflow`

---

## Sprint 2 — Task 5.1: OpenTofu Infrastructure (pensyve-infra repo) + Task 5.4: Website

### Task 5.1.0: Create pensyve-infra repository

- [ ] Initialize the private repository

```bash
cd /home/wshobson/workspace/major7apps

# Create the repo directory
mkdir -p pensyve-infra
cd pensyve-infra

# Initialize git
git init
git branch -M main

# Create initial structure
mkdir -p infra/modules/{networking,compute,data,storage,cdn,dns,monitoring,secrets}
mkdir -p infra/environments/{dev,staging,prod}
mkdir -p .github/workflows
```

- [ ] Create `.gitignore` for pensyve-infra

**File: `pensyve-infra/.gitignore`**

```gitignore
# OpenTofu / Terraform
.terraform/
.terraform.lock.hcl
*.tfstate
*.tfstate.*
*.tfplan
crash.log
crash.*.log
override.tf
override.tf.json
*_override.tf
*_override.tf.json

# Environment-specific secrets (tfvars with actual values)
# NOTE: checked-in .tfvars contain only non-secret defaults.
# Secret values come from AWS Secrets Manager or CI env vars.
*.auto.tfvars
*.auto.tfvars.json
secrets.tfvars

# OS / IDE
.DS_Store
.idea/
.vscode/
*.swp
*.swo

# Credentials
.env*
*.pem
*.key
credentials*
```

- [ ] Create `README.md` for pensyve-infra

**File: `pensyve-infra/README.md`**

```markdown
# pensyve-infra

Private infrastructure repository for Pensyve. Contains OpenTofu modules, deploy workflows, and environment configurations.

## Prerequisites

- OpenTofu >= 1.9
- AWS CLI configured with appropriate credentials
- S3 bucket for state backend (created manually once)

## Usage

```bash
cd infra
tofu init
tofu plan -var-file=environments/dev/terraform.tfvars
tofu apply -var-file=environments/dev/terraform.tfvars
```

## Module Structure

| Module | Purpose |
|--------|---------|
| networking | VPC, subnets, security groups, NAT gateway |
| compute | ECS Fargate cluster, task definitions, ALB |
| data | Aurora Serverless v2 (Postgres), ElastiCache Redis |
| storage | S3 buckets (blobs, static site) |
| cdn | CloudFront distributions |
| dns | Route53 zones and records |
| monitoring | CloudWatch alarms, dashboards, log groups |
| secrets | AWS Secrets Manager, SSM Parameter Store |
```

- [ ] Git commit in pensyve-infra: `chore: initialize pensyve-infra repository structure`

---

### Task 5.1.1: OpenTofu root configuration

- [ ] Create root module with S3 backend and provider configuration

**File: `pensyve-infra/infra/versions.tf`**

```hcl
terraform {
  required_version = ">= 1.9.0"

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
    random = {
      source  = "hashicorp/random"
      version = "~> 3.6"
    }
  }

  backend "s3" {
    bucket         = "pensyve-tofu-state"
    key            = "infra/terraform.tfstate"
    region         = "us-east-1"
    encrypt        = true
    dynamodb_table = "pensyve-tofu-locks"
  }
}

provider "aws" {
  region = var.aws_region

  default_tags {
    tags = {
      Project     = "pensyve"
      Environment = var.environment
      ManagedBy   = "opentofu"
    }
  }
}

# CloudFront requires ACM certificates in us-east-1
provider "aws" {
  alias  = "us_east_1"
  region = "us-east-1"

  default_tags {
    tags = {
      Project     = "pensyve"
      Environment = var.environment
      ManagedBy   = "opentofu"
    }
  }
}
```

**File: `pensyve-infra/infra/variables.tf`**

```hcl
variable "aws_region" {
  description = "AWS region for all resources"
  type        = string
  default     = "us-east-1"
}

variable "environment" {
  description = "Deployment environment (dev, staging, prod)"
  type        = string

  validation {
    condition     = contains(["dev", "staging", "prod"], var.environment)
    error_message = "Environment must be dev, staging, or prod."
  }
}

variable "project_name" {
  description = "Project name used for resource naming"
  type        = string
  default     = "pensyve"
}

variable "domain_name" {
  description = "Root domain name"
  type        = string
  default     = "pensyve.com"
}

# Networking
variable "vpc_cidr" {
  description = "CIDR block for the VPC"
  type        = string
  default     = "10.0.0.0/16"
}

variable "availability_zones" {
  description = "List of availability zones"
  type        = list(string)
  default     = ["us-east-1a", "us-east-1b"]
}

# Compute
variable "api_cpu" {
  description = "CPU units for API task (1024 = 1 vCPU)"
  type        = number
  default     = 512
}

variable "api_memory" {
  description = "Memory (MB) for API task"
  type        = number
  default     = 1024
}

variable "api_desired_count" {
  description = "Desired number of API task instances"
  type        = number
  default     = 1
}

variable "api_container_port" {
  description = "Port the API container listens on"
  type        = number
  default     = 8000
}

# Data
variable "aurora_min_capacity" {
  description = "Aurora Serverless v2 minimum ACU"
  type        = number
  default     = 0.5
}

variable "aurora_max_capacity" {
  description = "Aurora Serverless v2 maximum ACU"
  type        = number
  default     = 4
}

variable "redis_node_type" {
  description = "ElastiCache Redis node type"
  type        = string
  default     = "cache.t4g.micro"
}

# Feature flags
variable "enable_cdn" {
  description = "Enable CloudFront distribution"
  type        = bool
  default     = false
}

variable "enable_monitoring" {
  description = "Enable CloudWatch alarms and dashboards"
  type        = bool
  default     = true
}
```

**File: `pensyve-infra/infra/outputs.tf`**

```hcl
output "vpc_id" {
  description = "VPC ID"
  value       = module.networking.vpc_id
}

output "alb_dns_name" {
  description = "ALB DNS name for API access"
  value       = module.compute.alb_dns_name
}

output "api_url" {
  description = "Full API URL"
  value       = var.enable_cdn ? "https://api.${var.domain_name}" : "http://${module.compute.alb_dns_name}"
}

output "ecr_repository_url" {
  description = "ECR repository URL for Docker images"
  value       = module.compute.ecr_repository_url
}

output "aurora_endpoint" {
  description = "Aurora cluster endpoint"
  value       = module.data.aurora_endpoint
  sensitive   = true
}

output "redis_endpoint" {
  description = "ElastiCache Redis endpoint"
  value       = module.data.redis_endpoint
}

output "website_bucket" {
  description = "S3 bucket for static website"
  value       = module.storage.website_bucket_name
}

output "cloudfront_distribution_id" {
  description = "CloudFront distribution ID"
  value       = var.enable_cdn ? module.cdn[0].distribution_id : null
}
```

**File: `pensyve-infra/infra/main.tf`**

```hcl
# --- Networking ---
module "networking" {
  source = "./modules/networking"

  project_name       = var.project_name
  environment        = var.environment
  vpc_cidr           = var.vpc_cidr
  availability_zones = var.availability_zones
}

# --- Secrets (must come before compute and data) ---
module "secrets" {
  source = "./modules/secrets"

  project_name = var.project_name
  environment  = var.environment
}

# --- Data (Aurora + Redis) ---
module "data" {
  source = "./modules/data"

  project_name       = var.project_name
  environment        = var.environment
  vpc_id             = module.networking.vpc_id
  private_subnet_ids = module.networking.private_subnet_ids
  aurora_min_capacity = var.aurora_min_capacity
  aurora_max_capacity = var.aurora_max_capacity
  redis_node_type     = var.redis_node_type
  db_credentials_arn  = module.secrets.db_credentials_arn

  depends_on = [module.networking, module.secrets]
}

# --- Storage (S3) ---
module "storage" {
  source = "./modules/storage"

  project_name = var.project_name
  environment  = var.environment
}

# --- Compute (ECS Fargate + ALB + ECR) ---
module "compute" {
  source = "./modules/compute"

  project_name        = var.project_name
  environment         = var.environment
  vpc_id              = module.networking.vpc_id
  public_subnet_ids   = module.networking.public_subnet_ids
  private_subnet_ids  = module.networking.private_subnet_ids
  api_cpu             = var.api_cpu
  api_memory          = var.api_memory
  api_desired_count   = var.api_desired_count
  api_container_port  = var.api_container_port
  aurora_endpoint     = module.data.aurora_endpoint
  aurora_port         = module.data.aurora_port
  redis_endpoint      = module.data.redis_endpoint
  db_credentials_arn  = module.secrets.db_credentials_arn
  api_key_arn         = module.secrets.api_key_arn
  ecs_security_group_id = module.networking.ecs_security_group_id
  alb_security_group_id = module.networking.alb_security_group_id

  depends_on = [module.networking, module.data, module.secrets]
}

# --- CDN (CloudFront) ---
module "cdn" {
  count  = var.enable_cdn ? 1 : 0
  source = "./modules/cdn"

  project_name        = var.project_name
  environment         = var.environment
  domain_name         = var.domain_name
  alb_dns_name        = module.compute.alb_dns_name
  website_bucket_domain = module.storage.website_bucket_regional_domain
  website_bucket_id   = module.storage.website_bucket_id
  acm_certificate_arn = module.dns.acm_certificate_arn

  providers = {
    aws = aws.us_east_1
  }

  depends_on = [module.compute, module.storage, module.dns]
}

# --- DNS (Route53) ---
module "dns" {
  source = "./modules/dns"

  domain_name  = var.domain_name
  environment  = var.environment
  alb_dns_name = module.compute.alb_dns_name
  alb_zone_id  = module.compute.alb_zone_id

  providers = {
    aws.us_east_1 = aws.us_east_1
  }

  depends_on = [module.compute]
}

# --- Monitoring ---
module "monitoring" {
  count  = var.enable_monitoring ? 1 : 0
  source = "./modules/monitoring"

  project_name       = var.project_name
  environment        = var.environment
  ecs_cluster_name   = module.compute.ecs_cluster_name
  ecs_service_name   = module.compute.ecs_service_name
  alb_arn_suffix     = module.compute.alb_arn_suffix
  aurora_cluster_id  = module.data.aurora_cluster_id
  redis_cluster_id   = module.data.redis_cluster_id

  depends_on = [module.compute, module.data]
}
```

- [ ] Git commit in pensyve-infra: `infra: add root OpenTofu configuration with module wiring`

---

### Task 5.1.2: Networking module

- [ ] Create VPC, subnets, NAT gateway, security groups

**File: `pensyve-infra/infra/modules/networking/variables.tf`**

```hcl
variable "project_name" {
  type = string
}

variable "environment" {
  type = string
}

variable "vpc_cidr" {
  type = string
}

variable "availability_zones" {
  type = list(string)
}
```

**File: `pensyve-infra/infra/modules/networking/main.tf`**

```hcl
resource "aws_vpc" "main" {
  cidr_block           = var.vpc_cidr
  enable_dns_hostnames = true
  enable_dns_support   = true

  tags = {
    Name = "${var.project_name}-${var.environment}-vpc"
  }
}

# --- Public subnets (ALB) ---
resource "aws_subnet" "public" {
  count = length(var.availability_zones)

  vpc_id                  = aws_vpc.main.id
  cidr_block              = cidrsubnet(var.vpc_cidr, 8, count.index)
  availability_zone       = var.availability_zones[count.index]
  map_public_ip_on_launch = true

  tags = {
    Name = "${var.project_name}-${var.environment}-public-${var.availability_zones[count.index]}"
    Tier = "public"
  }
}

# --- Private subnets (ECS, Aurora, Redis) ---
resource "aws_subnet" "private" {
  count = length(var.availability_zones)

  vpc_id            = aws_vpc.main.id
  cidr_block        = cidrsubnet(var.vpc_cidr, 8, count.index + 100)
  availability_zone = var.availability_zones[count.index]

  tags = {
    Name = "${var.project_name}-${var.environment}-private-${var.availability_zones[count.index]}"
    Tier = "private"
  }
}

# --- Internet Gateway ---
resource "aws_internet_gateway" "main" {
  vpc_id = aws_vpc.main.id

  tags = {
    Name = "${var.project_name}-${var.environment}-igw"
  }
}

# --- Elastic IP for NAT Gateway ---
resource "aws_eip" "nat" {
  domain = "vpc"

  tags = {
    Name = "${var.project_name}-${var.environment}-nat-eip"
  }
}

# --- NAT Gateway (single AZ for cost in dev/staging) ---
resource "aws_nat_gateway" "main" {
  allocation_id = aws_eip.nat.id
  subnet_id     = aws_subnet.public[0].id

  tags = {
    Name = "${var.project_name}-${var.environment}-nat"
  }

  depends_on = [aws_internet_gateway.main]
}

# --- Route tables ---
resource "aws_route_table" "public" {
  vpc_id = aws_vpc.main.id

  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.main.id
  }

  tags = {
    Name = "${var.project_name}-${var.environment}-public-rt"
  }
}

resource "aws_route_table" "private" {
  vpc_id = aws_vpc.main.id

  route {
    cidr_block     = "0.0.0.0/0"
    nat_gateway_id = aws_nat_gateway.main.id
  }

  tags = {
    Name = "${var.project_name}-${var.environment}-private-rt"
  }
}

resource "aws_route_table_association" "public" {
  count          = length(var.availability_zones)
  subnet_id      = aws_subnet.public[count.index].id
  route_table_id = aws_route_table.public.id
}

resource "aws_route_table_association" "private" {
  count          = length(var.availability_zones)
  subnet_id      = aws_subnet.private[count.index].id
  route_table_id = aws_route_table.private.id
}

# --- Security Groups ---

# ALB: allow HTTP/HTTPS from internet
resource "aws_security_group" "alb" {
  name_prefix = "${var.project_name}-${var.environment}-alb-"
  vpc_id      = aws_vpc.main.id
  description = "ALB security group"

  ingress {
    description = "HTTP"
    from_port   = 80
    to_port     = 80
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }

  ingress {
    description = "HTTPS"
    from_port   = 443
    to_port     = 443
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }

  tags = {
    Name = "${var.project_name}-${var.environment}-alb-sg"
  }

  lifecycle {
    create_before_destroy = true
  }
}

# ECS tasks: allow traffic from ALB only
resource "aws_security_group" "ecs" {
  name_prefix = "${var.project_name}-${var.environment}-ecs-"
  vpc_id      = aws_vpc.main.id
  description = "ECS tasks security group"

  ingress {
    description     = "From ALB"
    from_port       = 8000
    to_port         = 8000
    protocol        = "tcp"
    security_groups = [aws_security_group.alb.id]
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }

  tags = {
    Name = "${var.project_name}-${var.environment}-ecs-sg"
  }

  lifecycle {
    create_before_destroy = true
  }
}

# Aurora: allow traffic from ECS only
resource "aws_security_group" "aurora" {
  name_prefix = "${var.project_name}-${var.environment}-aurora-"
  vpc_id      = aws_vpc.main.id
  description = "Aurora security group"

  ingress {
    description     = "Postgres from ECS"
    from_port       = 5432
    to_port         = 5432
    protocol        = "tcp"
    security_groups = [aws_security_group.ecs.id]
  }

  tags = {
    Name = "${var.project_name}-${var.environment}-aurora-sg"
  }

  lifecycle {
    create_before_destroy = true
  }
}

# Redis: allow traffic from ECS only
resource "aws_security_group" "redis" {
  name_prefix = "${var.project_name}-${var.environment}-redis-"
  vpc_id      = aws_vpc.main.id
  description = "Redis security group"

  ingress {
    description     = "Redis from ECS"
    from_port       = 6379
    to_port         = 6379
    protocol        = "tcp"
    security_groups = [aws_security_group.ecs.id]
  }

  tags = {
    Name = "${var.project_name}-${var.environment}-redis-sg"
  }

  lifecycle {
    create_before_destroy = true
  }
}
```

**File: `pensyve-infra/infra/modules/networking/outputs.tf`**

```hcl
output "vpc_id" {
  description = "VPC ID"
  value       = aws_vpc.main.id
}

output "public_subnet_ids" {
  description = "Public subnet IDs"
  value       = aws_subnet.public[*].id
}

output "private_subnet_ids" {
  description = "Private subnet IDs"
  value       = aws_subnet.private[*].id
}

output "alb_security_group_id" {
  description = "ALB security group ID"
  value       = aws_security_group.alb.id
}

output "ecs_security_group_id" {
  description = "ECS security group ID"
  value       = aws_security_group.ecs.id
}

output "aurora_security_group_id" {
  description = "Aurora security group ID"
  value       = aws_security_group.aurora.id
}

output "redis_security_group_id" {
  description = "Redis security group ID"
  value       = aws_security_group.redis.id
}
```

- [ ] Git commit in pensyve-infra: `infra: add networking module — VPC, subnets, NAT, security groups`

---

### Task 5.1.3: Secrets module

- [ ] Create Secrets Manager resources

**File: `pensyve-infra/infra/modules/secrets/variables.tf`**

```hcl
variable "project_name" {
  type = string
}

variable "environment" {
  type = string
}
```

**File: `pensyve-infra/infra/modules/secrets/main.tf`**

```hcl
# Database credentials — value set manually or via CI, not in code
resource "aws_secretsmanager_secret" "db_credentials" {
  name                    = "${var.project_name}/${var.environment}/db-credentials"
  description             = "Aurora Postgres credentials for Pensyve"
  recovery_window_in_days = 7

  tags = {
    Name = "${var.project_name}-${var.environment}-db-credentials"
  }
}

# Initial secret value (will be rotated)
resource "aws_secretsmanager_secret_version" "db_credentials" {
  secret_id = aws_secretsmanager_secret.db_credentials.id
  secret_string = jsonencode({
    username = "pensyve"
    password = random_password.db_password.result
    engine   = "postgres"
    port     = 5432
  })
}

resource "random_password" "db_password" {
  length           = 32
  special          = true
  override_special = "!#$%&*()-_=+[]{}<>:?"
}

# API key for the Pensyve REST API
resource "aws_secretsmanager_secret" "api_key" {
  name                    = "${var.project_name}/${var.environment}/api-key"
  description             = "API key for Pensyve REST API authentication"
  recovery_window_in_days = 7

  tags = {
    Name = "${var.project_name}-${var.environment}-api-key"
  }
}

resource "aws_secretsmanager_secret_version" "api_key" {
  secret_id     = aws_secretsmanager_secret.api_key.id
  secret_string = random_password.api_key.result
}

resource "random_password" "api_key" {
  length  = 48
  special = false
}

# Stripe keys (managed service billing)
resource "aws_secretsmanager_secret" "stripe" {
  name                    = "${var.project_name}/${var.environment}/stripe"
  description             = "Stripe API keys for billing"
  recovery_window_in_days = 7

  tags = {
    Name = "${var.project_name}-${var.environment}-stripe"
  }
}

# SSM Parameters for non-secret configuration
resource "aws_ssm_parameter" "log_level" {
  name  = "/${var.project_name}/${var.environment}/log-level"
  type  = "String"
  value = var.environment == "prod" ? "info" : "debug"

  tags = {
    Name = "${var.project_name}-${var.environment}-log-level"
  }
}

resource "aws_ssm_parameter" "tier2_enabled" {
  name  = "/${var.project_name}/${var.environment}/tier2-enabled"
  type  = "String"
  value = var.environment == "prod" ? "true" : "false"

  tags = {
    Name = "${var.project_name}-${var.environment}-tier2-enabled"
  }
}
```

**File: `pensyve-infra/infra/modules/secrets/outputs.tf`**

```hcl
output "db_credentials_arn" {
  description = "ARN of the DB credentials secret"
  value       = aws_secretsmanager_secret.db_credentials.arn
}

output "api_key_arn" {
  description = "ARN of the API key secret"
  value       = aws_secretsmanager_secret.api_key.arn
}

output "stripe_secret_arn" {
  description = "ARN of the Stripe secret"
  value       = aws_secretsmanager_secret.stripe.arn
}
```

- [ ] Git commit in pensyve-infra: `infra: add secrets module — Secrets Manager and SSM parameters`

---

### Task 5.1.4: Data module (Aurora Serverless v2 + ElastiCache Redis)

- [ ] Create Aurora and Redis resources

**File: `pensyve-infra/infra/modules/data/variables.tf`**

```hcl
variable "project_name" {
  type = string
}

variable "environment" {
  type = string
}

variable "vpc_id" {
  type = string
}

variable "private_subnet_ids" {
  type = list(string)
}

variable "aurora_min_capacity" {
  type    = number
  default = 0.5
}

variable "aurora_max_capacity" {
  type    = number
  default = 4
}

variable "redis_node_type" {
  type    = string
  default = "cache.t4g.micro"
}

variable "db_credentials_arn" {
  description = "ARN of the Secrets Manager secret for DB credentials"
  type        = string
}
```

**File: `pensyve-infra/infra/modules/data/main.tf`**

```hcl
# --- Aurora Serverless v2 (Postgres) ---

resource "aws_db_subnet_group" "aurora" {
  name       = "${var.project_name}-${var.environment}-aurora"
  subnet_ids = var.private_subnet_ids

  tags = {
    Name = "${var.project_name}-${var.environment}-aurora-subnet-group"
  }
}

# Look up the security group by tag (created by networking module)
data "aws_security_groups" "aurora" {
  filter {
    name   = "vpc-id"
    values = [var.vpc_id]
  }

  filter {
    name   = "tag:Name"
    values = ["${var.project_name}-${var.environment}-aurora-sg"]
  }
}

resource "aws_rds_cluster" "aurora" {
  cluster_identifier = "${var.project_name}-${var.environment}"
  engine             = "aurora-postgresql"
  engine_mode        = "provisioned"
  engine_version     = "16.4"
  database_name      = "pensyve"

  master_username                     = "pensyve"
  manage_master_user_password         = false
  master_password                     = jsondecode(data.aws_secretsmanager_secret_version.db_creds.secret_string)["password"]

  db_subnet_group_name   = aws_db_subnet_group.aurora.name
  vpc_security_group_ids = data.aws_security_groups.aurora.ids

  storage_encrypted = true
  deletion_protection = var.environment == "prod"

  # Serverless v2 scaling
  serverlessv2_scaling_configuration {
    min_capacity = var.aurora_min_capacity
    max_capacity = var.aurora_max_capacity
  }

  # Enable pgvector extension
  allow_major_version_upgrade = false
  apply_immediately           = var.environment != "prod"

  skip_final_snapshot       = var.environment != "prod"
  final_snapshot_identifier = var.environment == "prod" ? "${var.project_name}-${var.environment}-final" : null

  tags = {
    Name = "${var.project_name}-${var.environment}-aurora"
  }
}

data "aws_secretsmanager_secret_version" "db_creds" {
  secret_id = var.db_credentials_arn
}

resource "aws_rds_cluster_instance" "aurora" {
  count = var.environment == "prod" ? 2 : 1

  identifier         = "${var.project_name}-${var.environment}-${count.index}"
  cluster_identifier = aws_rds_cluster.aurora.id
  instance_class     = "db.serverless"
  engine             = aws_rds_cluster.aurora.engine
  engine_version     = aws_rds_cluster.aurora.engine_version

  publicly_accessible = false

  tags = {
    Name = "${var.project_name}-${var.environment}-aurora-instance-${count.index}"
  }
}

# --- ElastiCache Redis ---

resource "aws_elasticache_subnet_group" "redis" {
  name       = "${var.project_name}-${var.environment}-redis"
  subnet_ids = var.private_subnet_ids

  tags = {
    Name = "${var.project_name}-${var.environment}-redis-subnet-group"
  }
}

data "aws_security_groups" "redis" {
  filter {
    name   = "vpc-id"
    values = [var.vpc_id]
  }

  filter {
    name   = "tag:Name"
    values = ["${var.project_name}-${var.environment}-redis-sg"]
  }
}

resource "aws_elasticache_replication_group" "redis" {
  replication_group_id = "${var.project_name}-${var.environment}"
  description          = "Pensyve Redis — episode state and rate limiting"

  engine               = "redis"
  engine_version       = "7.1"
  node_type            = var.redis_node_type
  num_cache_clusters   = var.environment == "prod" ? 2 : 1
  port                 = 6379

  subnet_group_name    = aws_elasticache_subnet_group.redis.name
  security_group_ids   = data.aws_security_groups.redis.ids

  at_rest_encryption_enabled = true
  transit_encryption_enabled = true

  automatic_failover_enabled = var.environment == "prod"

  apply_immediately = var.environment != "prod"

  tags = {
    Name = "${var.project_name}-${var.environment}-redis"
  }
}
```

**File: `pensyve-infra/infra/modules/data/outputs.tf`**

```hcl
output "aurora_endpoint" {
  description = "Aurora cluster writer endpoint"
  value       = aws_rds_cluster.aurora.endpoint
}

output "aurora_reader_endpoint" {
  description = "Aurora cluster reader endpoint"
  value       = aws_rds_cluster.aurora.reader_endpoint
}

output "aurora_port" {
  description = "Aurora cluster port"
  value       = aws_rds_cluster.aurora.port
}

output "aurora_cluster_id" {
  description = "Aurora cluster identifier"
  value       = aws_rds_cluster.aurora.cluster_identifier
}

output "redis_endpoint" {
  description = "Redis primary endpoint"
  value       = aws_elasticache_replication_group.redis.primary_endpoint_address
}

output "redis_cluster_id" {
  description = "Redis replication group ID"
  value       = aws_elasticache_replication_group.redis.id
}
```

- [ ] Git commit in pensyve-infra: `infra: add data module — Aurora Serverless v2 and ElastiCache Redis`

---

### Task 5.1.5: Storage module (S3)

- [ ] Create S3 buckets for blobs and website hosting

**File: `pensyve-infra/infra/modules/storage/variables.tf`**

```hcl
variable "project_name" {
  type = string
}

variable "environment" {
  type = string
}
```

**File: `pensyve-infra/infra/modules/storage/main.tf`**

```hcl
# --- Blob storage (multimodal memory content) ---
resource "aws_s3_bucket" "blobs" {
  bucket = "${var.project_name}-${var.environment}-blobs"

  tags = {
    Name = "${var.project_name}-${var.environment}-blobs"
  }
}

resource "aws_s3_bucket_versioning" "blobs" {
  bucket = aws_s3_bucket.blobs.id
  versioning_configuration {
    status = "Enabled"
  }
}

resource "aws_s3_bucket_server_side_encryption_configuration" "blobs" {
  bucket = aws_s3_bucket.blobs.id

  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm = "aws:kms"
    }
    bucket_key_enabled = true
  }
}

resource "aws_s3_bucket_public_access_block" "blobs" {
  bucket = aws_s3_bucket.blobs.id

  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_lifecycle_configuration" "blobs" {
  bucket = aws_s3_bucket.blobs.id

  rule {
    id     = "transition-to-ia"
    status = "Enabled"

    transition {
      days          = 90
      storage_class = "STANDARD_IA"
    }
  }
}

# --- Static website bucket ---
resource "aws_s3_bucket" "website" {
  bucket = "${var.project_name}-${var.environment}-website"

  tags = {
    Name = "${var.project_name}-${var.environment}-website"
  }
}

resource "aws_s3_bucket_website_configuration" "website" {
  bucket = aws_s3_bucket.website.id

  index_document {
    suffix = "index.html"
  }

  error_document {
    key = "404.html"
  }
}

resource "aws_s3_bucket_public_access_block" "website" {
  bucket = aws_s3_bucket.website.id

  # CloudFront OAC handles access — block direct public access
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}
```

**File: `pensyve-infra/infra/modules/storage/outputs.tf`**

```hcl
output "blobs_bucket_name" {
  description = "Blob storage bucket name"
  value       = aws_s3_bucket.blobs.id
}

output "blobs_bucket_arn" {
  description = "Blob storage bucket ARN"
  value       = aws_s3_bucket.blobs.arn
}

output "website_bucket_name" {
  description = "Website bucket name"
  value       = aws_s3_bucket.website.id
}

output "website_bucket_arn" {
  description = "Website bucket ARN"
  value       = aws_s3_bucket.website.arn
}

output "website_bucket_regional_domain" {
  description = "Website bucket regional domain name (for CloudFront)"
  value       = aws_s3_bucket.website.bucket_regional_domain_name
}

output "website_bucket_id" {
  description = "Website bucket ID"
  value       = aws_s3_bucket.website.id
}
```

- [ ] Git commit in pensyve-infra: `infra: add storage module — S3 buckets for blobs and website`

---

### Task 5.1.6: Compute module (ECS Fargate + ALB + ECR)

- [ ] Create ECS cluster, task definition, service, ALB, and ECR repository

**File: `pensyve-infra/infra/modules/compute/variables.tf`**

```hcl
variable "project_name" {
  type = string
}

variable "environment" {
  type = string
}

variable "vpc_id" {
  type = string
}

variable "public_subnet_ids" {
  type = list(string)
}

variable "private_subnet_ids" {
  type = list(string)
}

variable "api_cpu" {
  type    = number
  default = 512
}

variable "api_memory" {
  type    = number
  default = 1024
}

variable "api_desired_count" {
  type    = number
  default = 1
}

variable "api_container_port" {
  type    = number
  default = 8000
}

variable "aurora_endpoint" {
  type = string
}

variable "aurora_port" {
  type = number
}

variable "redis_endpoint" {
  type = string
}

variable "db_credentials_arn" {
  type = string
}

variable "api_key_arn" {
  type = string
}

variable "ecs_security_group_id" {
  type = string
}

variable "alb_security_group_id" {
  type = string
}
```

**File: `pensyve-infra/infra/modules/compute/main.tf`**

```hcl
# --- ECR Repository ---
resource "aws_ecr_repository" "api" {
  name                 = "${var.project_name}-api"
  image_tag_mutability = "IMMUTABLE"
  force_delete         = var.environment != "prod"

  image_scanning_configuration {
    scan_on_push = true
  }

  encryption_configuration {
    encryption_type = "AES256"
  }

  tags = {
    Name = "${var.project_name}-api"
  }
}

resource "aws_ecr_lifecycle_policy" "api" {
  repository = aws_ecr_repository.api.name

  policy = jsonencode({
    rules = [
      {
        rulePriority = 1
        description  = "Keep last 20 images"
        selection = {
          tagStatus   = "any"
          countType   = "imageCountMoreThan"
          countNumber = 20
        }
        action = {
          type = "expire"
        }
      }
    ]
  })
}

# --- ECS Cluster ---
resource "aws_ecs_cluster" "main" {
  name = "${var.project_name}-${var.environment}"

  setting {
    name  = "containerInsights"
    value = var.environment == "prod" ? "enabled" : "disabled"
  }

  tags = {
    Name = "${var.project_name}-${var.environment}-cluster"
  }
}

# --- CloudWatch Log Group ---
resource "aws_cloudwatch_log_group" "api" {
  name              = "/ecs/${var.project_name}-${var.environment}/api"
  retention_in_days = var.environment == "prod" ? 90 : 14

  tags = {
    Name = "${var.project_name}-${var.environment}-api-logs"
  }
}

# --- IAM Roles ---
data "aws_region" "current" {}
data "aws_caller_identity" "current" {}

# Task execution role (ECR pull, log writing, secrets access)
resource "aws_iam_role" "ecs_execution" {
  name = "${var.project_name}-${var.environment}-ecs-execution"

  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Action = "sts:AssumeRole"
        Effect = "Allow"
        Principal = {
          Service = "ecs-tasks.amazonaws.com"
        }
      }
    ]
  })
}

resource "aws_iam_role_policy_attachment" "ecs_execution_base" {
  role       = aws_iam_role.ecs_execution.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AmazonECSTaskExecutionRolePolicy"
}

resource "aws_iam_role_policy" "ecs_execution_secrets" {
  name = "${var.project_name}-${var.environment}-secrets-access"
  role = aws_iam_role.ecs_execution.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect = "Allow"
        Action = [
          "secretsmanager:GetSecretValue"
        ]
        Resource = [
          var.db_credentials_arn,
          var.api_key_arn,
        ]
      }
    ]
  })
}

# Task role (what the running container can do)
resource "aws_iam_role" "ecs_task" {
  name = "${var.project_name}-${var.environment}-ecs-task"

  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Action = "sts:AssumeRole"
        Effect = "Allow"
        Principal = {
          Service = "ecs-tasks.amazonaws.com"
        }
      }
    ]
  })
}

resource "aws_iam_role_policy" "ecs_task_s3" {
  name = "${var.project_name}-${var.environment}-s3-access"
  role = aws_iam_role.ecs_task.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect = "Allow"
        Action = [
          "s3:GetObject",
          "s3:PutObject",
          "s3:DeleteObject",
          "s3:ListBucket"
        ]
        Resource = [
          "arn:aws:s3:::${var.project_name}-*-blobs",
          "arn:aws:s3:::${var.project_name}-*-blobs/*"
        ]
      }
    ]
  })
}

# --- Task Definition ---
resource "aws_ecs_task_definition" "api" {
  family                   = "${var.project_name}-${var.environment}-api"
  network_mode             = "awsvpc"
  requires_compatibilities = ["FARGATE"]
  cpu                      = var.api_cpu
  memory                   = var.api_memory
  execution_role_arn       = aws_iam_role.ecs_execution.arn
  task_role_arn            = aws_iam_role.ecs_task.arn

  container_definitions = jsonencode([
    {
      name  = "api"
      image = "${aws_ecr_repository.api.repository_url}:latest"
      essential = true

      portMappings = [
        {
          containerPort = var.api_container_port
          protocol      = "tcp"
        }
      ]

      environment = [
        { name = "PENSYVE_HOST", value = "0.0.0.0" },
        { name = "PENSYVE_PORT", value = tostring(var.api_container_port) },
        { name = "PENSYVE_NAMESPACE", value = "default" },
        { name = "PENSYVE_LOG_LEVEL", value = var.environment == "prod" ? "info" : "debug" },
        { name = "PENSYVE_REDIS_URL", value = "redis://${var.redis_endpoint}:6379/0" },
        { name = "PENSYVE_AUTH_ENABLED", value = "true" },
        { name = "PENSYVE_TIER2_ENABLED", value = var.environment == "prod" ? "true" : "false" },
      ]

      secrets = [
        {
          name      = "PENSYVE_DATABASE_URL"
          valueFrom = "${var.db_credentials_arn}:connection_string::"
        },
        {
          name      = "PENSYVE_API_KEY"
          valueFrom = var.api_key_arn
        },
      ]

      logConfiguration = {
        logDriver = "awslogs"
        options = {
          "awslogs-group"         = aws_cloudwatch_log_group.api.name
          "awslogs-region"        = data.aws_region.current.name
          "awslogs-stream-prefix" = "api"
        }
      }

      healthCheck = {
        command     = ["CMD-SHELL", "curl -f http://localhost:${var.api_container_port}/v1/health || exit 1"]
        interval    = 30
        timeout     = 5
        retries     = 3
        startPeriod = 60
      }
    }
  ])

  tags = {
    Name = "${var.project_name}-${var.environment}-api"
  }
}

# --- Application Load Balancer ---
resource "aws_lb" "api" {
  name               = "${var.project_name}-${var.environment}-alb"
  internal           = false
  load_balancer_type = "application"
  security_groups    = [var.alb_security_group_id]
  subnets            = var.public_subnet_ids

  enable_deletion_protection = var.environment == "prod"

  tags = {
    Name = "${var.project_name}-${var.environment}-alb"
  }
}

resource "aws_lb_target_group" "api" {
  name        = "${var.project_name}-${var.environment}-api"
  port        = var.api_container_port
  protocol    = "HTTP"
  vpc_id      = var.vpc_id
  target_type = "ip"

  health_check {
    enabled             = true
    healthy_threshold   = 2
    unhealthy_threshold = 3
    interval            = 30
    path                = "/v1/health"
    port                = "traffic-port"
    protocol            = "HTTP"
    timeout             = 5
    matcher             = "200"
  }

  deregistration_delay = 30

  tags = {
    Name = "${var.project_name}-${var.environment}-api-tg"
  }
}

resource "aws_lb_listener" "http" {
  load_balancer_arn = aws_lb.api.arn
  port              = 80
  protocol          = "HTTP"

  default_action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.api.arn
  }
}

# --- ECS Service ---
resource "aws_ecs_service" "api" {
  name            = "${var.project_name}-${var.environment}-api"
  cluster         = aws_ecs_cluster.main.id
  task_definition = aws_ecs_task_definition.api.arn
  desired_count   = var.api_desired_count
  launch_type     = "FARGATE"

  network_configuration {
    subnets          = var.private_subnet_ids
    security_groups  = [var.ecs_security_group_id]
    assign_public_ip = false
  }

  load_balancer {
    target_group_arn = aws_lb_target_group.api.arn
    container_name   = "api"
    container_port   = var.api_container_port
  }

  deployment_circuit_breaker {
    enable   = true
    rollback = true
  }

  deployment_maximum_percent         = 200
  deployment_minimum_healthy_percent = 100

  depends_on = [aws_lb_listener.http]

  tags = {
    Name = "${var.project_name}-${var.environment}-api-service"
  }
}
```

**File: `pensyve-infra/infra/modules/compute/outputs.tf`**

```hcl
output "ecr_repository_url" {
  description = "ECR repository URL"
  value       = aws_ecr_repository.api.repository_url
}

output "ecs_cluster_name" {
  description = "ECS cluster name"
  value       = aws_ecs_cluster.main.name
}

output "ecs_service_name" {
  description = "ECS service name"
  value       = aws_ecs_service.api.name
}

output "alb_dns_name" {
  description = "ALB DNS name"
  value       = aws_lb.api.dns_name
}

output "alb_zone_id" {
  description = "ALB hosted zone ID"
  value       = aws_lb.api.zone_id
}

output "alb_arn_suffix" {
  description = "ALB ARN suffix (for CloudWatch metrics)"
  value       = aws_lb.api.arn_suffix
}

output "alb_listener_arn" {
  description = "ALB HTTP listener ARN"
  value       = aws_lb_listener.http.arn
}
```

- [ ] Git commit in pensyve-infra: `infra: add compute module — ECS Fargate, ALB, ECR, IAM roles`

---

### Task 5.1.7: CDN module (CloudFront)

- [ ] Create CloudFront distributions for website and API

**File: `pensyve-infra/infra/modules/cdn/variables.tf`**

```hcl
variable "project_name" {
  type = string
}

variable "environment" {
  type = string
}

variable "domain_name" {
  type = string
}

variable "alb_dns_name" {
  type = string
}

variable "website_bucket_domain" {
  type = string
}

variable "website_bucket_id" {
  type = string
}

variable "acm_certificate_arn" {
  type = string
}
```

**File: `pensyve-infra/infra/modules/cdn/main.tf`**

```hcl
# OAC for S3 website access
resource "aws_cloudfront_origin_access_control" "website" {
  name                              = "${var.project_name}-${var.environment}-website-oac"
  description                       = "OAC for website S3 bucket"
  origin_access_control_origin_type = "s3"
  signing_behavior                  = "always"
  signing_protocol                  = "sigv4"
}

# Website CloudFront distribution
resource "aws_cloudfront_distribution" "website" {
  enabled             = true
  is_ipv6_enabled     = true
  default_root_object = "index.html"
  comment             = "${var.project_name} ${var.environment} website"
  price_class         = var.environment == "prod" ? "PriceClass_All" : "PriceClass_100"

  aliases = var.environment == "prod" ? [var.domain_name, "www.${var.domain_name}"] : ["${var.environment}.${var.domain_name}"]

  origin {
    domain_name              = var.website_bucket_domain
    origin_access_control_id = aws_cloudfront_origin_access_control.website.id
    origin_id                = "s3-website"
  }

  default_cache_behavior {
    allowed_methods  = ["GET", "HEAD", "OPTIONS"]
    cached_methods   = ["GET", "HEAD"]
    target_origin_id = "s3-website"

    forwarded_values {
      query_string = false
      cookies {
        forward = "none"
      }
    }

    viewer_protocol_policy = "redirect-to-https"
    min_ttl                = 0
    default_ttl            = 3600
    max_ttl                = 86400
    compress               = true
  }

  # SPA fallback: return index.html for 404s
  custom_error_response {
    error_code         = 404
    response_code      = 200
    response_page_path = "/index.html"
  }

  restrictions {
    geo_restriction {
      restriction_type = "none"
    }
  }

  viewer_certificate {
    acm_certificate_arn      = var.acm_certificate_arn
    ssl_support_method       = "sni-only"
    minimum_protocol_version = "TLSv1.2_2021"
  }

  tags = {
    Name = "${var.project_name}-${var.environment}-website-cf"
  }
}

# API CloudFront distribution
resource "aws_cloudfront_distribution" "api" {
  enabled         = true
  is_ipv6_enabled = true
  comment         = "${var.project_name} ${var.environment} API"
  price_class     = var.environment == "prod" ? "PriceClass_All" : "PriceClass_100"

  aliases = ["api.${var.domain_name}"]

  origin {
    domain_name = var.alb_dns_name
    origin_id   = "alb-api"

    custom_origin_config {
      http_port              = 80
      https_port             = 443
      origin_protocol_policy = "http-only"
      origin_ssl_protocols   = ["TLSv1.2"]
    }
  }

  default_cache_behavior {
    allowed_methods  = ["DELETE", "GET", "HEAD", "OPTIONS", "PATCH", "POST", "PUT"]
    cached_methods   = ["GET", "HEAD"]
    target_origin_id = "alb-api"

    forwarded_values {
      query_string = true
      headers      = ["Authorization", "X-Pensyve-Key", "Content-Type", "Origin"]
      cookies {
        forward = "none"
      }
    }

    viewer_protocol_policy = "redirect-to-https"
    min_ttl                = 0
    default_ttl            = 0
    max_ttl                = 0
  }

  restrictions {
    geo_restriction {
      restriction_type = "none"
    }
  }

  viewer_certificate {
    acm_certificate_arn      = var.acm_certificate_arn
    ssl_support_method       = "sni-only"
    minimum_protocol_version = "TLSv1.2_2021"
  }

  tags = {
    Name = "${var.project_name}-${var.environment}-api-cf"
  }
}

# S3 bucket policy to allow CloudFront OAC access
data "aws_iam_policy_document" "website_bucket_policy" {
  statement {
    sid    = "AllowCloudFrontOAC"
    effect = "Allow"
    principals {
      type        = "Service"
      identifiers = ["cloudfront.amazonaws.com"]
    }
    actions   = ["s3:GetObject"]
    resources = ["arn:aws:s3:::${var.website_bucket_id}/*"]
    condition {
      test     = "StringEquals"
      variable = "AWS:SourceArn"
      values   = [aws_cloudfront_distribution.website.arn]
    }
  }
}

resource "aws_s3_bucket_policy" "website" {
  bucket = var.website_bucket_id
  policy = data.aws_iam_policy_document.website_bucket_policy.json
}
```

**File: `pensyve-infra/infra/modules/cdn/outputs.tf`**

```hcl
output "distribution_id" {
  description = "Website CloudFront distribution ID"
  value       = aws_cloudfront_distribution.website.id
}

output "website_domain_name" {
  description = "Website CloudFront domain name"
  value       = aws_cloudfront_distribution.website.domain_name
}

output "api_distribution_id" {
  description = "API CloudFront distribution ID"
  value       = aws_cloudfront_distribution.api.id
}

output "api_domain_name" {
  description = "API CloudFront domain name"
  value       = aws_cloudfront_distribution.api.domain_name
}
```

- [ ] Git commit in pensyve-infra: `infra: add CDN module — CloudFront for website and API`

---

### Task 5.1.8: DNS module (Route53)

- [ ] Create hosted zone, records, and ACM certificate

**File: `pensyve-infra/infra/modules/dns/variables.tf`**

```hcl
variable "domain_name" {
  type = string
}

variable "environment" {
  type = string
}

variable "alb_dns_name" {
  type = string
}

variable "alb_zone_id" {
  type = string
}
```

**File: `pensyve-infra/infra/modules/dns/main.tf`**

```hcl
terraform {
  required_providers {
    aws = {
      source                = "hashicorp/aws"
      configuration_aliases = [aws.us_east_1]
    }
  }
}

# Hosted zone (assumes domain is already registered)
resource "aws_route53_zone" "main" {
  name = var.domain_name

  tags = {
    Name = var.domain_name
  }
}

# ACM certificate (must be in us-east-1 for CloudFront)
resource "aws_acm_certificate" "main" {
  provider = aws.us_east_1

  domain_name               = var.domain_name
  subject_alternative_names = ["*.${var.domain_name}"]
  validation_method         = "DNS"

  lifecycle {
    create_before_destroy = true
  }

  tags = {
    Name = "${var.domain_name}-cert"
  }
}

# DNS validation records
resource "aws_route53_record" "cert_validation" {
  for_each = {
    for dvo in aws_acm_certificate.main.domain_validation_options : dvo.domain_name => {
      name   = dvo.resource_record_name
      record = dvo.resource_record_value
      type   = dvo.resource_record_type
    }
  }

  allow_overwrite = true
  name            = each.value.name
  records         = [each.value.record]
  ttl             = 60
  type            = each.value.type
  zone_id         = aws_route53_zone.main.zone_id
}

resource "aws_acm_certificate_validation" "main" {
  provider = aws.us_east_1

  certificate_arn         = aws_acm_certificate.main.arn
  validation_record_fqdns = [for record in aws_route53_record.cert_validation : record.fqdn]
}

# API subdomain -> ALB
resource "aws_route53_record" "api" {
  zone_id = aws_route53_zone.main.zone_id
  name    = "api.${var.domain_name}"
  type    = "A"

  alias {
    name                   = var.alb_dns_name
    zone_id                = var.alb_zone_id
    evaluate_target_health = true
  }
}
```

**File: `pensyve-infra/infra/modules/dns/outputs.tf`**

```hcl
output "zone_id" {
  description = "Route53 zone ID"
  value       = aws_route53_zone.main.zone_id
}

output "name_servers" {
  description = "Route53 name servers"
  value       = aws_route53_zone.main.name_servers
}

output "acm_certificate_arn" {
  description = "ACM certificate ARN"
  value       = aws_acm_certificate.main.arn
}
```

- [ ] Git commit in pensyve-infra: `infra: add DNS module — Route53 zone, ACM certificate, API record`

---

### Task 5.1.9: Monitoring module

- [ ] Create CloudWatch alarms and dashboard

**File: `pensyve-infra/infra/modules/monitoring/variables.tf`**

```hcl
variable "project_name" {
  type = string
}

variable "environment" {
  type = string
}

variable "ecs_cluster_name" {
  type = string
}

variable "ecs_service_name" {
  type = string
}

variable "alb_arn_suffix" {
  type = string
}

variable "aurora_cluster_id" {
  type = string
}

variable "redis_cluster_id" {
  type = string
}
```

**File: `pensyve-infra/infra/modules/monitoring/main.tf`**

```hcl
# --- SNS Topic for alerts ---
resource "aws_sns_topic" "alerts" {
  name = "${var.project_name}-${var.environment}-alerts"

  tags = {
    Name = "${var.project_name}-${var.environment}-alerts"
  }
}

# --- ECS Alarms ---
resource "aws_cloudwatch_metric_alarm" "ecs_cpu_high" {
  alarm_name          = "${var.project_name}-${var.environment}-ecs-cpu-high"
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 3
  metric_name         = "CPUUtilization"
  namespace           = "AWS/ECS"
  period              = 60
  statistic           = "Average"
  threshold           = 80

  dimensions = {
    ClusterName = var.ecs_cluster_name
    ServiceName = var.ecs_service_name
  }

  alarm_actions = [aws_sns_topic.alerts.arn]
  ok_actions    = [aws_sns_topic.alerts.arn]

  tags = {
    Name = "${var.project_name}-${var.environment}-ecs-cpu-high"
  }
}

resource "aws_cloudwatch_metric_alarm" "ecs_memory_high" {
  alarm_name          = "${var.project_name}-${var.environment}-ecs-memory-high"
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 3
  metric_name         = "MemoryUtilization"
  namespace           = "AWS/ECS"
  period              = 60
  statistic           = "Average"
  threshold           = 80

  dimensions = {
    ClusterName = var.ecs_cluster_name
    ServiceName = var.ecs_service_name
  }

  alarm_actions = [aws_sns_topic.alerts.arn]
  ok_actions    = [aws_sns_topic.alerts.arn]

  tags = {
    Name = "${var.project_name}-${var.environment}-ecs-memory-high"
  }
}

# --- ALB Alarms ---
resource "aws_cloudwatch_metric_alarm" "alb_5xx" {
  alarm_name          = "${var.project_name}-${var.environment}-alb-5xx"
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 2
  metric_name         = "HTTPCode_Target_5XX_Count"
  namespace           = "AWS/ApplicationELB"
  period              = 300
  statistic           = "Sum"
  threshold           = 10

  dimensions = {
    LoadBalancer = var.alb_arn_suffix
  }

  alarm_actions = [aws_sns_topic.alerts.arn]
  treat_missing_data = "notBreaching"

  tags = {
    Name = "${var.project_name}-${var.environment}-alb-5xx"
  }
}

resource "aws_cloudwatch_metric_alarm" "alb_latency" {
  alarm_name          = "${var.project_name}-${var.environment}-alb-latency-p99"
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 3
  metric_name         = "TargetResponseTime"
  namespace           = "AWS/ApplicationELB"
  period              = 60
  extended_statistic  = "p99"
  threshold           = 5

  dimensions = {
    LoadBalancer = var.alb_arn_suffix
  }

  alarm_actions = [aws_sns_topic.alerts.arn]
  treat_missing_data = "notBreaching"

  tags = {
    Name = "${var.project_name}-${var.environment}-alb-latency-p99"
  }
}

# --- Aurora Alarms ---
resource "aws_cloudwatch_metric_alarm" "aurora_cpu" {
  alarm_name          = "${var.project_name}-${var.environment}-aurora-cpu-high"
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 3
  metric_name         = "CPUUtilization"
  namespace           = "AWS/RDS"
  period              = 60
  statistic           = "Average"
  threshold           = 80

  dimensions = {
    DBClusterIdentifier = var.aurora_cluster_id
  }

  alarm_actions = [aws_sns_topic.alerts.arn]
  ok_actions    = [aws_sns_topic.alerts.arn]

  tags = {
    Name = "${var.project_name}-${var.environment}-aurora-cpu-high"
  }
}

resource "aws_cloudwatch_metric_alarm" "aurora_connections" {
  alarm_name          = "${var.project_name}-${var.environment}-aurora-connections-high"
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 2
  metric_name         = "DatabaseConnections"
  namespace           = "AWS/RDS"
  period              = 300
  statistic           = "Average"
  threshold           = 50

  dimensions = {
    DBClusterIdentifier = var.aurora_cluster_id
  }

  alarm_actions = [aws_sns_topic.alerts.arn]

  tags = {
    Name = "${var.project_name}-${var.environment}-aurora-connections-high"
  }
}

# --- Dashboard ---
resource "aws_cloudwatch_dashboard" "main" {
  dashboard_name = "${var.project_name}-${var.environment}"

  dashboard_body = jsonencode({
    widgets = [
      {
        type   = "metric"
        x      = 0
        y      = 0
        width  = 12
        height = 6
        properties = {
          title   = "ECS CPU & Memory"
          metrics = [
            ["AWS/ECS", "CPUUtilization", "ClusterName", var.ecs_cluster_name, "ServiceName", var.ecs_service_name],
            ["AWS/ECS", "MemoryUtilization", "ClusterName", var.ecs_cluster_name, "ServiceName", var.ecs_service_name],
          ]
          period = 60
          stat   = "Average"
          region = "us-east-1"
        }
      },
      {
        type   = "metric"
        x      = 12
        y      = 0
        width  = 12
        height = 6
        properties = {
          title   = "ALB Request Count & Latency"
          metrics = [
            ["AWS/ApplicationELB", "RequestCount", "LoadBalancer", var.alb_arn_suffix, { stat = "Sum" }],
            ["AWS/ApplicationELB", "TargetResponseTime", "LoadBalancer", var.alb_arn_suffix, { stat = "p99" }],
          ]
          period = 60
          region = "us-east-1"
        }
      },
      {
        type   = "metric"
        x      = 0
        y      = 6
        width  = 12
        height = 6
        properties = {
          title   = "Aurora CPU & Connections"
          metrics = [
            ["AWS/RDS", "CPUUtilization", "DBClusterIdentifier", var.aurora_cluster_id],
            ["AWS/RDS", "DatabaseConnections", "DBClusterIdentifier", var.aurora_cluster_id],
          ]
          period = 60
          stat   = "Average"
          region = "us-east-1"
        }
      },
      {
        type   = "metric"
        x      = 12
        y      = 6
        width  = 12
        height = 6
        properties = {
          title   = "Redis CPU & Memory"
          metrics = [
            ["AWS/ElastiCache", "CPUUtilization", "ReplicationGroupId", var.redis_cluster_id],
            ["AWS/ElastiCache", "DatabaseMemoryUsagePercentage", "ReplicationGroupId", var.redis_cluster_id],
          ]
          period = 60
          stat   = "Average"
          region = "us-east-1"
        }
      },
    ]
  })
}
```

**File: `pensyve-infra/infra/modules/monitoring/outputs.tf`**

```hcl
output "alerts_topic_arn" {
  description = "SNS topic ARN for alerts"
  value       = aws_sns_topic.alerts.arn
}

output "dashboard_name" {
  description = "CloudWatch dashboard name"
  value       = aws_cloudwatch_dashboard.main.dashboard_name
}
```

- [ ] Git commit in pensyve-infra: `infra: add monitoring module — CloudWatch alarms and dashboard`

---

### Task 5.1.10: Environment configurations

- [ ] Create tfvars for dev, staging, prod

**File: `pensyve-infra/infra/environments/dev/terraform.tfvars`**

```hcl
environment        = "dev"
aws_region         = "us-east-1"
project_name       = "pensyve"
domain_name        = "pensyve.com"
vpc_cidr           = "10.0.0.0/16"
availability_zones = ["us-east-1a", "us-east-1b"]

# Minimal resources for dev
api_cpu           = 256
api_memory        = 512
api_desired_count = 1

aurora_min_capacity = 0.5
aurora_max_capacity = 2
redis_node_type     = "cache.t4g.micro"

enable_cdn        = false
enable_monitoring = false
```

**File: `pensyve-infra/infra/environments/staging/terraform.tfvars`**

```hcl
environment        = "staging"
aws_region         = "us-east-1"
project_name       = "pensyve"
domain_name        = "pensyve.com"
vpc_cidr           = "10.1.0.0/16"
availability_zones = ["us-east-1a", "us-east-1b"]

# Moderate resources for staging
api_cpu           = 512
api_memory        = 1024
api_desired_count = 1

aurora_min_capacity = 0.5
aurora_max_capacity = 4
redis_node_type     = "cache.t4g.micro"

enable_cdn        = true
enable_monitoring = true
```

**File: `pensyve-infra/infra/environments/prod/terraform.tfvars`**

```hcl
environment        = "prod"
aws_region         = "us-east-1"
project_name       = "pensyve"
domain_name        = "pensyve.com"
vpc_cidr           = "10.2.0.0/16"
availability_zones = ["us-east-1a", "us-east-1b", "us-east-1c"]

# Production resources
api_cpu           = 1024
api_memory        = 2048
api_desired_count = 2

aurora_min_capacity = 1
aurora_max_capacity = 16
redis_node_type     = "cache.t4g.small"

enable_cdn        = true
enable_monitoring = true
```

**Verification:**

```bash
cd /home/wshobson/workspace/major7apps/pensyve-infra/infra

# Validate OpenTofu configuration (syntax only, no AWS creds needed)
tofu init -backend=false
tofu validate

# Check formatting
tofu fmt -check -recursive
```

- [ ] Git commit in pensyve-infra: `infra: add environment configurations for dev, staging, prod`

---

### Task 5.4.1: Website scaffold (Astro)

- [ ] Initialize Astro project in `pensyve/website/`

```bash
cd /home/wshobson/workspace/major7apps/pensyve

# Create Astro project (static output for S3)
npm create astro@latest website -- --template minimal --no-install --no-git
cd website
npm install
```

- [ ] Configure Astro for static output

**File: `pensyve/website/astro.config.mjs`**

```javascript
import { defineConfig } from 'astro/config';

export default defineConfig({
  output: 'static',
  site: 'https://pensyve.com',
  build: {
    assets: '_assets',
  },
  vite: {
    build: {
      cssMinify: true,
    },
  },
});
```

**File: `pensyve/website/tsconfig.json`**

```json
{
  "extends": "astro/tsconfigs/strict",
  "compilerOptions": {
    "baseUrl": ".",
    "paths": {
      "@/*": ["src/*"]
    }
  }
}
```

- [ ] Create base layout

**File: `pensyve/website/src/layouts/BaseLayout.astro`**

```astro
---
interface Props {
  title: string;
  description?: string;
}

const { title, description = 'Universal memory runtime for AI agents' } = Astro.props;
---

<!doctype html>
<html lang="en">
  <head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <meta name="description" content={description} />
    <meta name="generator" content={Astro.generator} />

    <!-- Open Graph -->
    <meta property="og:title" content={title} />
    <meta property="og:description" content={description} />
    <meta property="og:type" content="website" />
    <meta property="og:url" content={Astro.url} />

    <link rel="icon" type="image/svg+xml" href="/favicon.svg" />
    <title>{title}</title>
  </head>
  <body>
    <nav class="nav">
      <div class="nav-container">
        <a href="/" class="nav-brand">Pensyve</a>
        <div class="nav-links">
          <a href="/docs">Docs</a>
          <a href="/api">API Reference</a>
          <a href="/blog">Blog</a>
          <a href="/changelog">Changelog</a>
          <a href="https://github.com/major7apps/pensyve" target="_blank" rel="noopener">GitHub</a>
        </div>
      </div>
    </nav>

    <main>
      <slot />
    </main>

    <footer class="footer">
      <div class="footer-container">
        <p>&copy; {new Date().getFullYear()} Major7 Apps. Open source under Apache-2.0.</p>
      </div>
    </footer>
  </body>
</html>

<style is:global>
  :root {
    --color-bg: #0a0a0a;
    --color-surface: #141414;
    --color-border: #262626;
    --color-text: #e5e5e5;
    --color-text-muted: #a3a3a3;
    --color-accent: #3b82f6;
    --color-accent-hover: #60a5fa;
    --font-sans: 'Inter', system-ui, sans-serif;
    --font-mono: 'JetBrains Mono', 'Fira Code', monospace;
    --max-width: 1200px;
  }

  * {
    margin: 0;
    padding: 0;
    box-sizing: border-box;
  }

  body {
    font-family: var(--font-sans);
    background: var(--color-bg);
    color: var(--color-text);
    line-height: 1.6;
    min-height: 100vh;
    display: flex;
    flex-direction: column;
  }

  main {
    flex: 1;
  }

  a {
    color: var(--color-accent);
    text-decoration: none;
    transition: color 0.2s;
  }

  a:hover {
    color: var(--color-accent-hover);
  }

  .nav {
    border-bottom: 1px solid var(--color-border);
    padding: 1rem 0;
  }

  .nav-container {
    max-width: var(--max-width);
    margin: 0 auto;
    padding: 0 2rem;
    display: flex;
    justify-content: space-between;
    align-items: center;
  }

  .nav-brand {
    font-size: 1.25rem;
    font-weight: 700;
    color: var(--color-text);
  }

  .nav-links {
    display: flex;
    gap: 2rem;
  }

  .nav-links a {
    color: var(--color-text-muted);
    font-size: 0.9rem;
  }

  .nav-links a:hover {
    color: var(--color-text);
  }

  .footer {
    border-top: 1px solid var(--color-border);
    padding: 2rem 0;
    margin-top: 4rem;
  }

  .footer-container {
    max-width: var(--max-width);
    margin: 0 auto;
    padding: 0 2rem;
    text-align: center;
    color: var(--color-text-muted);
    font-size: 0.85rem;
  }

  @media (max-width: 768px) {
    .nav-container {
      flex-direction: column;
      gap: 1rem;
    }

    .nav-links {
      gap: 1rem;
      flex-wrap: wrap;
      justify-content: center;
    }
  }
</style>
```

- [ ] Create landing page

**File: `pensyve/website/src/pages/index.astro`**

```astro
---
import BaseLayout from '../layouts/BaseLayout.astro';
---

<BaseLayout title="Pensyve — Universal Memory Runtime for AI Agents">
  <section class="hero">
    <div class="hero-container">
      <h1>Universal memory runtime<br />for AI agents</h1>
      <p class="hero-subtitle">
        Give your AI agents persistent, cross-session memory. Rust core engine with Python, TypeScript,
        MCP, and REST interfaces. Vector + BM25 + graph retrieval with FSRS memory decay.
      </p>
      <div class="hero-actions">
        <a href="/docs" class="btn btn-primary">Get Started</a>
        <a href="https://github.com/major7apps/pensyve" class="btn btn-secondary" target="_blank" rel="noopener">
          View on GitHub
        </a>
      </div>
      <div class="hero-install">
        <code>pip install pensyve</code>
      </div>
    </div>
  </section>

  <section class="features">
    <div class="features-container">
      <div class="feature">
        <h3>Multi-Signal Retrieval</h3>
        <p>8-signal fusion: vector similarity, BM25, entity graph proximity, recency, frequency,
           confidence, type boost, and cross-encoder reranking.</p>
      </div>
      <div class="feature">
        <h3>Memory Lifecycle</h3>
        <p>FSRS forgetting curve for natural memory decay. Consolidation promotes repeated
           episodic memories to semantic knowledge. Bayesian procedural reliability.</p>
      </div>
      <div class="feature">
        <h3>Multiple Interfaces</h3>
        <p>Rust core, Python SDK (PyO3), TypeScript SDK, MCP server (Claude Code plugin),
           REST API (FastAPI), CLI. Use from anywhere.</p>
      </div>
      <div class="feature">
        <h3>Structured Knowledge</h3>
        <p>Episodic, semantic (SPO triples), and procedural memory types.
           Entity graph with relationship tracking. Namespace isolation.</p>
      </div>
      <div class="feature">
        <h3>Rust Performance</h3>
        <p>Core engine in Rust with SQLite + FTS5. ONNX embeddings (gte-modernbert-base).
           In-memory vector index for sub-millisecond cosine similarity.</p>
      </div>
      <div class="feature">
        <h3>Cloud Ready</h3>
        <p>Local SQLite for development, Aurora Serverless v2 (Postgres + pgvector) for
           production. Scales to zero. Redis-backed session state.</p>
      </div>
    </div>
  </section>

  <section class="quickstart">
    <div class="quickstart-container">
      <h2>Quick Start</h2>
      <pre><code>{`import pensyve

# Initialize
p = pensyve.Pensyve(namespace="my-project")
agent = p.entity("assistant", kind="agent")

# Remember facts
p.remember(entity=agent, fact="User prefers dark mode")

# Start an episode (conversation)
with p.episode(agent) as ep:
    ep.message("user", "How do I configure the database?")
    ep.message("assistant", "Use DATABASE_URL env var...")

# Recall relevant memories
results = p.recall("user preferences", entity=agent)
for mem in results:
    print(f"[{mem.memory_type}] {mem.content}")`}</code></pre>
    </div>
  </section>
</BaseLayout>

<style>
  .hero {
    text-align: center;
    padding: 6rem 2rem 4rem;
  }

  .hero-container {
    max-width: 800px;
    margin: 0 auto;
  }

  h1 {
    font-size: 3rem;
    font-weight: 800;
    line-height: 1.1;
    margin-bottom: 1.5rem;
  }

  .hero-subtitle {
    font-size: 1.2rem;
    color: var(--color-text-muted);
    margin-bottom: 2rem;
    line-height: 1.7;
  }

  .hero-actions {
    display: flex;
    gap: 1rem;
    justify-content: center;
    margin-bottom: 2rem;
  }

  .btn {
    padding: 0.75rem 2rem;
    border-radius: 8px;
    font-weight: 600;
    font-size: 1rem;
    transition: all 0.2s;
  }

  .btn-primary {
    background: var(--color-accent);
    color: white;
  }

  .btn-primary:hover {
    background: var(--color-accent-hover);
    color: white;
  }

  .btn-secondary {
    border: 1px solid var(--color-border);
    color: var(--color-text);
  }

  .btn-secondary:hover {
    border-color: var(--color-text-muted);
    color: var(--color-text);
  }

  .hero-install code {
    background: var(--color-surface);
    border: 1px solid var(--color-border);
    padding: 0.5rem 1.5rem;
    border-radius: 6px;
    font-family: var(--font-mono);
    font-size: 0.9rem;
    color: var(--color-text-muted);
  }

  .features {
    padding: 4rem 2rem;
  }

  .features-container {
    max-width: var(--max-width);
    margin: 0 auto;
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(320px, 1fr));
    gap: 2rem;
  }

  .feature {
    background: var(--color-surface);
    border: 1px solid var(--color-border);
    border-radius: 12px;
    padding: 2rem;
  }

  .feature h3 {
    font-size: 1.1rem;
    margin-bottom: 0.75rem;
  }

  .feature p {
    color: var(--color-text-muted);
    font-size: 0.95rem;
  }

  .quickstart {
    padding: 4rem 2rem;
  }

  .quickstart-container {
    max-width: 700px;
    margin: 0 auto;
  }

  .quickstart h2 {
    font-size: 2rem;
    margin-bottom: 1.5rem;
    text-align: center;
  }

  .quickstart pre {
    background: var(--color-surface);
    border: 1px solid var(--color-border);
    border-radius: 12px;
    padding: 2rem;
    overflow-x: auto;
  }

  .quickstart code {
    font-family: var(--font-mono);
    font-size: 0.85rem;
    line-height: 1.7;
    color: var(--color-text-muted);
  }

  @media (max-width: 768px) {
    h1 {
      font-size: 2rem;
    }

    .hero-actions {
      flex-direction: column;
      align-items: center;
    }
  }
</style>
```

- [ ] Create stub pages for docs, blog, changelog, API reference

**File: `pensyve/website/src/pages/docs.astro`**

```astro
---
import BaseLayout from '../layouts/BaseLayout.astro';
---

<BaseLayout title="Documentation — Pensyve">
  <div class="page-container">
    <h1>Documentation</h1>
    <p>Full documentation is coming soon. In the meantime, check out the
       <a href="https://github.com/major7apps/pensyve">GitHub repository</a> for
       setup instructions and API reference.</p>

    <h2>Quick Links</h2>
    <ul>
      <li><a href="https://github.com/major7apps/pensyve#readme">Getting Started</a></li>
      <li><a href="/api">REST API Reference</a></li>
      <li><a href="https://github.com/major7apps/pensyve/tree/main/pensyve-ts">TypeScript SDK</a></li>
      <li><a href="https://github.com/major7apps/pensyve/tree/main/pensyve-plugin">Claude Code Plugin</a></li>
    </ul>
  </div>
</BaseLayout>

<style>
  .page-container {
    max-width: 800px;
    margin: 0 auto;
    padding: 4rem 2rem;
  }

  h1 {
    font-size: 2.5rem;
    margin-bottom: 1rem;
  }

  h2 {
    font-size: 1.5rem;
    margin-top: 2rem;
    margin-bottom: 1rem;
  }

  p {
    color: var(--color-text-muted);
    margin-bottom: 1rem;
  }

  ul {
    list-style: none;
    padding: 0;
  }

  li {
    padding: 0.5rem 0;
    border-bottom: 1px solid var(--color-border);
  }
</style>
```

**File: `pensyve/website/src/pages/api.astro`**

```astro
---
import BaseLayout from '../layouts/BaseLayout.astro';
---

<BaseLayout title="API Reference — Pensyve">
  <div class="page-container">
    <h1>API Reference</h1>
    <p>The Pensyve REST API runs on FastAPI with automatic OpenAPI documentation.</p>

    <h2>Base URL</h2>
    <code class="url">https://api.pensyve.com/v1</code>

    <h2>Authentication</h2>
    <p>All requests require an API key via the <code>X-Pensyve-Key</code> header.</p>

    <h2>Endpoints</h2>
    <div class="endpoint">
      <span class="method post">POST</span>
      <code>/v1/recall</code>
      <p>Search memories with multi-signal retrieval</p>
    </div>
    <div class="endpoint">
      <span class="method post">POST</span>
      <code>/v1/remember</code>
      <p>Store a semantic memory (fact)</p>
    </div>
    <div class="endpoint">
      <span class="method post">POST</span>
      <code>/v1/entities</code>
      <p>Create or get an entity</p>
    </div>
    <div class="endpoint">
      <span class="method post">POST</span>
      <code>/v1/episodes/start</code>
      <p>Start an episode (bounded interaction sequence)</p>
    </div>
    <div class="endpoint">
      <span class="method post">POST</span>
      <code>/v1/episodes/message</code>
      <p>Add a message to an active episode</p>
    </div>
    <div class="endpoint">
      <span class="method post">POST</span>
      <code>/v1/episodes/end</code>
      <p>End an episode and extract memories</p>
    </div>
    <div class="endpoint">
      <span class="method delete">DELETE</span>
      <code>/v1/entities/{'{entity_name}'}</code>
      <p>Forget (soft or hard delete) memories for an entity</p>
    </div>
    <div class="endpoint">
      <span class="method post">POST</span>
      <code>/v1/consolidate</code>
      <p>Trigger memory consolidation (dreaming cycle)</p>
    </div>
    <div class="endpoint">
      <span class="method get">GET</span>
      <code>/v1/health</code>
      <p>Health check</p>
    </div>

    <p class="note">Interactive OpenAPI docs available at <code>/docs</code> when running the server locally.</p>
  </div>
</BaseLayout>

<style>
  .page-container {
    max-width: 800px;
    margin: 0 auto;
    padding: 4rem 2rem;
  }

  h1 { font-size: 2.5rem; margin-bottom: 1rem; }
  h2 { font-size: 1.5rem; margin-top: 2.5rem; margin-bottom: 1rem; }
  p { color: var(--color-text-muted); margin-bottom: 1rem; }

  .url {
    display: block;
    background: var(--color-surface);
    border: 1px solid var(--color-border);
    padding: 0.75rem 1.25rem;
    border-radius: 6px;
    font-family: var(--font-mono);
    font-size: 0.9rem;
  }

  .endpoint {
    display: flex;
    align-items: flex-start;
    gap: 0.75rem;
    padding: 1rem 0;
    border-bottom: 1px solid var(--color-border);
    flex-wrap: wrap;
  }

  .endpoint code {
    font-family: var(--font-mono);
    font-size: 0.9rem;
  }

  .endpoint p {
    width: 100%;
    font-size: 0.9rem;
    margin: 0.25rem 0 0 0;
  }

  .method {
    font-size: 0.75rem;
    font-weight: 700;
    padding: 0.2rem 0.5rem;
    border-radius: 4px;
    font-family: var(--font-mono);
  }

  .method.post { background: #1d4ed8; color: white; }
  .method.get { background: #15803d; color: white; }
  .method.delete { background: #b91c1c; color: white; }

  .note {
    margin-top: 2rem;
    font-style: italic;
    font-size: 0.9rem;
  }
</style>
```

**File: `pensyve/website/src/pages/blog.astro`**

```astro
---
import BaseLayout from '../layouts/BaseLayout.astro';
---

<BaseLayout title="Blog — Pensyve">
  <div class="page-container">
    <h1>Blog</h1>
    <p>Build-in-public updates, technical deep dives, and announcements.</p>
    <p class="coming-soon">Coming soon.</p>
  </div>
</BaseLayout>

<style>
  .page-container {
    max-width: 800px;
    margin: 0 auto;
    padding: 4rem 2rem;
  }

  h1 { font-size: 2.5rem; margin-bottom: 1rem; }
  p { color: var(--color-text-muted); }
  .coming-soon { font-style: italic; margin-top: 2rem; }
</style>
```

**File: `pensyve/website/src/pages/changelog.astro`**

```astro
---
import BaseLayout from '../layouts/BaseLayout.astro';
---

<BaseLayout title="Changelog — Pensyve">
  <div class="page-container">
    <h1>Changelog</h1>

    <article class="entry">
      <h2>v0.1.0 — Initial Release</h2>
      <time>2026</time>
      <ul>
        <li>Core engine: SQLite + FTS5, ONNX embeddings, 8-signal fusion retrieval</li>
        <li>FSRS memory decay and consolidation</li>
        <li>Python SDK via PyO3</li>
        <li>TypeScript SDK (HTTP client)</li>
        <li>MCP server with 6 tools</li>
        <li>REST API with 9 endpoints</li>
        <li>CLI: recall, stats, inspect</li>
        <li>Tier 2 LLM-based extraction</li>
      </ul>
    </article>
  </div>
</BaseLayout>

<style>
  .page-container {
    max-width: 800px;
    margin: 0 auto;
    padding: 4rem 2rem;
  }

  h1 { font-size: 2.5rem; margin-bottom: 2rem; }

  .entry {
    border-left: 2px solid var(--color-accent);
    padding-left: 1.5rem;
    margin-bottom: 3rem;
  }

  .entry h2 { font-size: 1.3rem; margin-bottom: 0.25rem; }
  .entry time { color: var(--color-text-muted); font-size: 0.85rem; }

  .entry ul {
    margin-top: 1rem;
    padding-left: 1.25rem;
  }

  .entry li {
    color: var(--color-text-muted);
    padding: 0.25rem 0;
  }
</style>
```

- [ ] Create favicon

**File: `pensyve/website/public/favicon.svg`**

```svg
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32" fill="none">
  <rect width="32" height="32" rx="6" fill="#3b82f6"/>
  <text x="16" y="23" text-anchor="middle" font-family="system-ui" font-weight="800" font-size="20" fill="white">P</text>
</svg>
```

**Verification:**

```bash
cd /home/wshobson/workspace/major7apps/pensyve/website
npm run build  # should output to dist/
ls dist/index.html  # verify landing page built
```

- [ ] Git commit in pensyve: `feat: add pensyve.com website scaffold (Astro static site)`

---

## Sprint 3 — Task 5.2: Container & CI/CD

### Task 5.2.1: Dockerfile (pensyve repo)

- [ ] Create multi-stage Dockerfile

**File: `pensyve/Dockerfile`**

```dockerfile
# =============================================================================
# Pensyve API Server — Multi-stage Docker Build
# Stage 1: Rust compilation (pensyve-mcp, pensyve-cli, PyO3 wheel)
# Stage 2: Python runtime with FastAPI + compiled native module
# =============================================================================

# --- Stage 1: Rust builder ---
FROM rust:1.85-bookworm AS rust-builder

# Install maturin for PyO3 wheel building
RUN pip3 install --break-system-packages maturin

WORKDIR /build

# Copy workspace Cargo files first for dependency caching
COPY Cargo.toml Cargo.lock ./
COPY pensyve-core/Cargo.toml pensyve-core/Cargo.toml
COPY pensyve-python/Cargo.toml pensyve-python/Cargo.toml
COPY pensyve-mcp/Cargo.toml pensyve-mcp/Cargo.toml
COPY pensyve-cli/Cargo.toml pensyve-cli/Cargo.toml

# Create dummy source files so cargo can download/compile dependencies
RUN mkdir -p pensyve-core/src pensyve-python/src pensyve-mcp/src pensyve-cli/src && \
    echo "pub fn placeholder() {}" > pensyve-core/src/lib.rs && \
    echo "use pyo3::prelude::*; #[pymodule] fn pensyve_python(_py: Python, _m: &Bound<'_, PyModule>) -> PyResult<()> { Ok(()) }" > pensyve-python/src/lib.rs && \
    echo "fn main() {}" > pensyve-mcp/src/main.rs && \
    echo "fn main() {}" > pensyve-cli/src/main.rs

# Build dependencies (cached layer)
RUN cargo build --release 2>/dev/null || true

# Copy actual source code
COPY pensyve-core/ pensyve-core/
COPY pensyve-python/ pensyve-python/
COPY pensyve-mcp/ pensyve-mcp/
COPY pensyve-cli/ pensyve-cli/

# Touch source files to invalidate the dummy build
RUN find pensyve-core/src pensyve-python/src pensyve-mcp/src pensyve-cli/src -name "*.rs" -exec touch {} +

# Build release binaries
RUN cargo build --release -p pensyve-mcp -p pensyve-cli

# Build PyO3 wheel
RUN maturin build --release \
    --manifest-path pensyve-python/Cargo.toml \
    --out /build/wheels/

# --- Stage 2: Python runtime ---
FROM python:3.12-slim-bookworm AS runtime

# Install runtime dependencies
RUN apt-get update && \
    apt-get install -y --no-install-recommends curl && \
    rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd --create-home --shell /bin/bash pensyve

WORKDIR /app

# Copy compiled binaries
COPY --from=rust-builder /build/target/release/pensyve-mcp /usr/local/bin/
COPY --from=rust-builder /build/target/release/pensyve-cli /usr/local/bin/

# Install PyO3 wheel
COPY --from=rust-builder /build/wheels/*.whl /tmp/
RUN pip install --no-cache-dir /tmp/*.whl && rm /tmp/*.whl

# Install Python dependencies
COPY pensyve_server/requirements.txt /app/requirements.txt
RUN pip install --no-cache-dir -r requirements.txt

# Copy application code
COPY pensyve_server/ /app/pensyve_server/
COPY pensyve-python/python/pensyve/ /app/pensyve/

# Set ownership
RUN chown -R pensyve:pensyve /app

# Switch to non-root user
USER pensyve

# Environment defaults
ENV PENSYVE_HOST=0.0.0.0
ENV PENSYVE_PORT=8000
ENV PENSYVE_LOG_LEVEL=info

EXPOSE 8000

HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD curl -f http://localhost:8000/v1/health || exit 1

CMD ["uvicorn", "pensyve_server.main:app", \
     "--host", "0.0.0.0", \
     "--port", "8000", \
     "--workers", "2", \
     "--log-level", "info"]
```

- [ ] Create `.dockerignore`

**File: `pensyve/.dockerignore`**

```
# Git
.git
.gitignore

# Build artifacts
target/debug/
target/release/deps/
target/release/build/
target/release/.fingerprint/
target/release/examples/
target/release/incremental/

# Python
__pycache__
*.pyc
.venv/
*.egg-info/
dist/

# Node
node_modules/
pensyve-ts/

# IDE
.idea/
.vscode/
*.swp

# Data
*.db
*.db-journal
models/
*.onnx
.fastembed_cache/

# Docs
docs/
*.md
!pensyve_server/requirements.txt

# Tests
tests/
benchmarks/

# Website
website/

# Infrastructure
.terraform/
*.tfstate*
*.tfvars

# CI
.github/
.pre-commit-config.yaml
```

**Verification:**

```bash
cd /home/wshobson/workspace/major7apps/pensyve

# Build the Docker image (requires Docker daemon)
docker build -t pensyve-api:local .

# Test the container starts and serves health endpoint
docker run --rm -d --name pensyve-test -p 8000:8000 pensyve-api:local
sleep 5
curl -f http://localhost:8000/v1/health
docker stop pensyve-test
```

- [ ] Git commit in pensyve: `feat: add multi-stage Dockerfile for API server`

---

### Task 5.2.2: CI workflow (pensyve repo)

- [ ] Create comprehensive CI workflow

**File: `pensyve/.github/workflows/ci.yml`**

```yaml
name: CI

on:
  push:
    branches: [main]
  pull_request:
    branches: [main]

permissions:
  contents: read

env:
  CARGO_TERM_COLOR: always
  RUSTFLAGS: "-D warnings"

jobs:
  # --- Lint ---
  lint-rust:
    name: Lint (Rust)
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy, rustfmt

      - uses: Swatinem/rust-cache@v2

      - name: Check formatting
        run: cargo fmt --all -- --check

      - name: Clippy
        run: cargo clippy --workspace -- -D warnings

  lint-python:
    name: Lint (Python)
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - uses: astral-sh/setup-uv@v5

      - name: Set up Python
        run: uv python install 3.12

      - name: Install tools
        run: uv pip install --system ruff pyright

      - name: Ruff check
        run: ruff check .

      - name: Ruff format check
        run: ruff format --check .

  # --- Test ---
  test-rust:
    name: Test (Rust)
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - uses: dtolnay/rust-toolchain@stable

      - uses: Swatinem/rust-cache@v2

      - name: Run tests
        run: cargo test --workspace

  test-python:
    name: Test (Python)
    runs-on: ubuntu-latest
    needs: [lint-rust]
    steps:
      - uses: actions/checkout@v4

      - uses: dtolnay/rust-toolchain@stable

      - uses: Swatinem/rust-cache@v2

      - uses: astral-sh/setup-uv@v5

      - name: Set up Python
        run: uv python install 3.12

      - name: Create venv and install deps
        run: |
          uv venv .venv
          source .venv/bin/activate
          uv pip install -r pensyve_server/requirements.txt
          uv pip install maturin pytest

      - name: Build PyO3 module
        run: |
          source .venv/bin/activate
          maturin develop --manifest-path pensyve-python/Cargo.toml

      - name: Run Python tests
        run: |
          source .venv/bin/activate
          pytest tests/python/ -v

  test-typescript:
    name: Test (TypeScript)
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - uses: oven-sh/setup-bun@v2

      - name: Install dependencies
        working-directory: pensyve-ts
        run: bun install

      - name: Lint
        working-directory: pensyve-ts
        run: bun run lint

      - name: Test
        working-directory: pensyve-ts
        run: bun test

  # --- Build container (on main branch only) ---
  build-container:
    name: Build & Push Container
    runs-on: ubuntu-latest
    needs: [test-rust, test-python, lint-rust, lint-python]
    if: github.ref == 'refs/heads/main' && github.event_name == 'push'
    permissions:
      id-token: write
      contents: read
    steps:
      - uses: actions/checkout@v4

      - name: Configure AWS credentials
        uses: aws-actions/configure-aws-credentials@v4
        with:
          role-to-arn: ${{ secrets.AWS_DEPLOY_ROLE_ARN }}
          aws-region: us-east-1

      - name: Login to Amazon ECR
        id: login-ecr
        uses: aws-actions/amazon-ecr-login@v2

      - name: Build, tag, and push image
        env:
          ECR_REGISTRY: ${{ steps.login-ecr.outputs.registry }}
          ECR_REPOSITORY: pensyve-api
          IMAGE_TAG: ${{ github.sha }}
        run: |
          docker build -t $ECR_REGISTRY/$ECR_REPOSITORY:$IMAGE_TAG .
          docker tag $ECR_REGISTRY/$ECR_REPOSITORY:$IMAGE_TAG $ECR_REGISTRY/$ECR_REPOSITORY:latest
          docker push $ECR_REGISTRY/$ECR_REPOSITORY:$IMAGE_TAG
          docker push $ECR_REGISTRY/$ECR_REPOSITORY:latest

  # --- Build website ---
  build-website:
    name: Build Website
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - uses: actions/setup-node@v4
        with:
          node-version: 20

      - name: Install dependencies
        working-directory: website
        run: npm ci

      - name: Build
        working-directory: website
        run: npm run build

      - name: Upload artifact
        if: github.ref == 'refs/heads/main' && github.event_name == 'push'
        uses: actions/upload-artifact@v4
        with:
          name: website
          path: website/dist/
```

- [ ] Git commit in pensyve: `ci: add comprehensive CI workflow — lint, test, build, push`

---

### Task 5.2.3: Deploy workflow (pensyve-infra repo)

- [ ] Create deploy workflow triggered by ECR push or manual dispatch

**File: `pensyve-infra/.github/workflows/deploy.yml`**

```yaml
name: Deploy

on:
  workflow_dispatch:
    inputs:
      environment:
        description: 'Target environment'
        required: true
        default: 'dev'
        type: choice
        options:
          - dev
          - staging
          - prod
      image_tag:
        description: 'Docker image tag (default: latest)'
        required: false
        default: 'latest'

  repository_dispatch:
    types: [ecr-push]

permissions:
  id-token: write
  contents: read

env:
  AWS_REGION: us-east-1

jobs:
  deploy:
    name: Deploy to ${{ github.event.inputs.environment || 'dev' }}
    runs-on: ubuntu-latest
    environment: ${{ github.event.inputs.environment || 'dev' }}

    steps:
      - uses: actions/checkout@v4

      - name: Configure AWS credentials
        uses: aws-actions/configure-aws-credentials@v4
        with:
          role-to-arn: ${{ secrets.AWS_DEPLOY_ROLE_ARN }}
          aws-region: ${{ env.AWS_REGION }}

      - name: Login to Amazon ECR
        id: login-ecr
        uses: aws-actions/amazon-ecr-login@v2

      - name: Set variables
        id: vars
        run: |
          ENV="${{ github.event.inputs.environment || 'dev' }}"
          TAG="${{ github.event.inputs.image_tag || 'latest' }}"
          echo "environment=$ENV" >> $GITHUB_OUTPUT
          echo "image_tag=$TAG" >> $GITHUB_OUTPUT
          echo "cluster=pensyve-$ENV" >> $GITHUB_OUTPUT
          echo "service=pensyve-$ENV-api" >> $GITHUB_OUTPUT

      - name: Update ECS service
        run: |
          # Force new deployment with the specified image
          aws ecs update-service \
            --cluster ${{ steps.vars.outputs.cluster }} \
            --service ${{ steps.vars.outputs.service }} \
            --force-new-deployment \
            --region ${{ env.AWS_REGION }}

      - name: Wait for deployment
        run: |
          aws ecs wait services-stable \
            --cluster ${{ steps.vars.outputs.cluster }} \
            --services ${{ steps.vars.outputs.service }} \
            --region ${{ env.AWS_REGION }}

      - name: Health check
        run: |
          # Get ALB DNS name from ECS service
          ALB_DNS=$(aws elbv2 describe-load-balancers \
            --names pensyve-${{ steps.vars.outputs.environment }}-alb \
            --query 'LoadBalancers[0].DNSName' \
            --output text \
            --region ${{ env.AWS_REGION }})

          # Wait for health endpoint
          for i in $(seq 1 10); do
            if curl -sf "http://$ALB_DNS/v1/health" > /dev/null 2>&1; then
              echo "Health check passed"
              exit 0
            fi
            echo "Attempt $i/10 — waiting..."
            sleep 10
          done
          echo "Health check failed after 10 attempts"
          exit 1

  deploy-website:
    name: Deploy Website
    runs-on: ubuntu-latest
    if: github.event.inputs.environment == 'prod' || github.event.inputs.environment == 'staging'
    needs: [deploy]

    steps:
      - name: Configure AWS credentials
        uses: aws-actions/configure-aws-credentials@v4
        with:
          role-to-arn: ${{ secrets.AWS_DEPLOY_ROLE_ARN }}
          aws-region: ${{ env.AWS_REGION }}

      - name: Download website artifact
        uses: dawidd6/action-download-artifact@v6
        with:
          repo: major7apps/pensyve
          workflow: ci.yml
          name: website
          path: website-dist/

      - name: Sync to S3
        run: |
          ENV="${{ github.event.inputs.environment || 'dev' }}"
          aws s3 sync website-dist/ s3://pensyve-$ENV-website/ \
            --delete \
            --cache-control "public, max-age=3600"

      - name: Invalidate CloudFront
        if: github.event.inputs.environment == 'prod'
        run: |
          DIST_ID=$(aws cloudfront list-distributions \
            --query "DistributionList.Items[?Comment=='pensyve prod website'].Id" \
            --output text)
          aws cloudfront create-invalidation \
            --distribution-id $DIST_ID \
            --paths "/*"
```

- [ ] Git commit in pensyve-infra: `ci: add deploy workflow — ECS update and website sync`

---

### Task 5.2.4: Release workflow (pensyve-infra repo)

- [ ] Create release workflow for publishing packages

**File: `pensyve-infra/.github/workflows/release.yml`**

```yaml
name: Release

on:
  workflow_dispatch:
    inputs:
      version:
        description: 'Release version (e.g., 0.2.0)'
        required: true
      publish_pypi:
        description: 'Publish to PyPI'
        type: boolean
        default: true
      publish_crates:
        description: 'Publish to crates.io'
        type: boolean
        default: true
      publish_npm:
        description: 'Publish to npm'
        type: boolean
        default: true

permissions:
  contents: write
  id-token: write

jobs:
  validate:
    name: Validate version
    runs-on: ubuntu-latest
    steps:
      - name: Check version format
        run: |
          if ! echo "${{ inputs.version }}" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+(-[a-z]+\.[0-9]+)?$'; then
            echo "Invalid version format: ${{ inputs.version }}"
            exit 1
          fi

  publish-pypi:
    name: Publish to PyPI
    runs-on: ubuntu-latest
    needs: [validate]
    if: inputs.publish_pypi
    steps:
      - uses: actions/checkout@v4
        with:
          repository: major7apps/pensyve
          ref: main

      - uses: dtolnay/rust-toolchain@stable

      - uses: astral-sh/setup-uv@v5

      - name: Set up Python
        run: uv python install 3.12

      - name: Install maturin
        run: uv pip install --system maturin[patchelf]

      - name: Build wheels
        run: |
          maturin build --release \
            --manifest-path pensyve-python/Cargo.toml \
            --out dist/

      - name: Publish to PyPI
        uses: pypa/gh-action-pypi-publish@release/v1
        with:
          packages-dir: dist/

  publish-crates:
    name: Publish to crates.io
    runs-on: ubuntu-latest
    needs: [validate]
    if: inputs.publish_crates
    steps:
      - uses: actions/checkout@v4
        with:
          repository: major7apps/pensyve
          ref: main

      - uses: dtolnay/rust-toolchain@stable

      - name: Publish pensyve-core
        run: cargo publish -p pensyve-core
        env:
          CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}

  publish-npm:
    name: Publish to npm
    runs-on: ubuntu-latest
    needs: [validate]
    if: inputs.publish_npm
    steps:
      - uses: actions/checkout@v4
        with:
          repository: major7apps/pensyve
          ref: main

      - uses: oven-sh/setup-bun@v2

      - name: Install and build
        working-directory: pensyve-ts
        run: |
          bun install
          bun run build

      - name: Publish
        working-directory: pensyve-ts
        run: npm publish
        env:
          NODE_AUTH_TOKEN: ${{ secrets.NPM_TOKEN }}

  create-github-release:
    name: Create GitHub Release
    runs-on: ubuntu-latest
    needs: [publish-pypi, publish-crates, publish-npm]
    if: always() && !contains(needs.*.result, 'failure')
    steps:
      - uses: actions/checkout@v4
        with:
          repository: major7apps/pensyve
          ref: main

      - name: Create tag and release
        uses: softprops/action-gh-release@v2
        with:
          tag_name: v${{ inputs.version }}
          name: v${{ inputs.version }}
          generate_release_notes: true
          draft: false
          repository: major7apps/pensyve
        env:
          GITHUB_TOKEN: ${{ secrets.PENSYVE_REPO_TOKEN }}
```

- [ ] Git commit in pensyve-infra: `ci: add release workflow — PyPI, crates.io, npm publishing`

---

### Task 5.2.5: Infrastructure plan/apply workflow (pensyve-infra repo)

- [ ] Create OpenTofu plan/apply workflow

**File: `pensyve-infra/.github/workflows/infra.yml`**

```yaml
name: Infrastructure

on:
  pull_request:
    paths:
      - 'infra/**'
  push:
    branches: [main]
    paths:
      - 'infra/**'
  workflow_dispatch:
    inputs:
      environment:
        description: 'Target environment'
        required: true
        default: 'dev'
        type: choice
        options:
          - dev
          - staging
          - prod
      action:
        description: 'Action to perform'
        required: true
        default: 'plan'
        type: choice
        options:
          - plan
          - apply

permissions:
  id-token: write
  contents: read
  pull-requests: write

env:
  TOFU_VERSION: 1.9.0
  AWS_REGION: us-east-1

jobs:
  plan:
    name: Plan (${{ github.event.inputs.environment || 'dev' }})
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Setup OpenTofu
        uses: opentofu/setup-opentofu@v1
        with:
          tofu_version: ${{ env.TOFU_VERSION }}

      - name: Configure AWS credentials
        uses: aws-actions/configure-aws-credentials@v4
        with:
          role-to-arn: ${{ secrets.AWS_INFRA_ROLE_ARN }}
          aws-region: ${{ env.AWS_REGION }}

      - name: OpenTofu Init
        working-directory: infra
        run: tofu init

      - name: OpenTofu Plan
        working-directory: infra
        run: |
          ENV="${{ github.event.inputs.environment || 'dev' }}"
          tofu plan \
            -var-file=environments/$ENV/terraform.tfvars \
            -out=tfplan \
            -no-color 2>&1 | tee plan-output.txt

      - name: Comment PR with plan
        if: github.event_name == 'pull_request'
        uses: actions/github-script@v7
        with:
          script: |
            const fs = require('fs');
            const plan = fs.readFileSync('infra/plan-output.txt', 'utf8');
            const truncated = plan.length > 60000 ? plan.substring(0, 60000) + '\n\n... (truncated)' : plan;

            github.rest.issues.createComment({
              issue_number: context.issue.number,
              owner: context.repo.owner,
              repo: context.repo.repo,
              body: `## OpenTofu Plan\n\n\`\`\`\n${truncated}\n\`\`\``
            });

      - name: Upload plan
        uses: actions/upload-artifact@v4
        with:
          name: tfplan
          path: infra/tfplan

  apply:
    name: Apply (${{ github.event.inputs.environment || 'dev' }})
    runs-on: ubuntu-latest
    needs: [plan]
    if: >-
      (github.event_name == 'workflow_dispatch' && github.event.inputs.action == 'apply') ||
      (github.event_name == 'push' && github.ref == 'refs/heads/main')
    environment: ${{ github.event.inputs.environment || 'dev' }}

    steps:
      - uses: actions/checkout@v4

      - name: Setup OpenTofu
        uses: opentofu/setup-opentofu@v1
        with:
          tofu_version: ${{ env.TOFU_VERSION }}

      - name: Configure AWS credentials
        uses: aws-actions/configure-aws-credentials@v4
        with:
          role-to-arn: ${{ secrets.AWS_INFRA_ROLE_ARN }}
          aws-region: ${{ env.AWS_REGION }}

      - name: Download plan
        uses: actions/download-artifact@v4
        with:
          name: tfplan
          path: infra/

      - name: OpenTofu Init
        working-directory: infra
        run: tofu init

      - name: OpenTofu Apply
        working-directory: infra
        run: tofu apply -auto-approve tfplan
```

- [ ] Git commit in pensyve-infra: `ci: add infrastructure plan/apply workflow`

---

## Sprint 4 — Task 5.5: Billing & Multi-tenancy

### Task 5.5.1: Billing infrastructure module (pensyve-infra repo)

- [ ] Create billing OpenTofu module for DynamoDB usage metering table

**File: `pensyve-infra/infra/modules/billing/variables.tf`**

```hcl
variable "project_name" {
  type = string
}

variable "environment" {
  type = string
}
```

**File: `pensyve-infra/infra/modules/billing/main.tf`**

```hcl
# DynamoDB table for usage metering (per-namespace counters)
resource "aws_dynamodb_table" "usage" {
  name         = "${var.project_name}-${var.environment}-usage"
  billing_mode = "PAY_PER_REQUEST"
  hash_key     = "namespace_id"
  range_key    = "period"

  attribute {
    name = "namespace_id"
    type = "S"
  }

  attribute {
    name = "period"
    type = "S"
  }

  ttl {
    attribute_name = "expires_at"
    enabled        = true
  }

  point_in_time_recovery {
    enabled = var.environment == "prod"
  }

  tags = {
    Name = "${var.project_name}-${var.environment}-usage"
  }
}

# DynamoDB table for subscription state
resource "aws_dynamodb_table" "subscriptions" {
  name         = "${var.project_name}-${var.environment}-subscriptions"
  billing_mode = "PAY_PER_REQUEST"
  hash_key     = "namespace_id"

  attribute {
    name = "namespace_id"
    type = "S"
  }

  point_in_time_recovery {
    enabled = var.environment == "prod"
  }

  tags = {
    Name = "${var.project_name}-${var.environment}-subscriptions"
  }
}

# Stripe webhook secret
resource "aws_secretsmanager_secret" "stripe_webhook" {
  name                    = "${var.project_name}/${var.environment}/stripe-webhook-secret"
  description             = "Stripe webhook signing secret"
  recovery_window_in_days = 7
}
```

**File: `pensyve-infra/infra/modules/billing/outputs.tf`**

```hcl
output "usage_table_name" {
  description = "DynamoDB usage metering table name"
  value       = aws_dynamodb_table.usage.name
}

output "usage_table_arn" {
  description = "DynamoDB usage metering table ARN"
  value       = aws_dynamodb_table.usage.arn
}

output "subscriptions_table_name" {
  description = "DynamoDB subscriptions table name"
  value       = aws_dynamodb_table.subscriptions.name
}

output "subscriptions_table_arn" {
  description = "DynamoDB subscriptions table ARN"
  value       = aws_dynamodb_table.subscriptions.arn
}
```

- [ ] Git commit in pensyve-infra: `infra: add billing module — DynamoDB usage metering and subscriptions`

---

### Task 5.5.2: Billing server module (pensyve repo)

- [ ] Create billing Python module for the REST API

**File: `pensyve/pensyve_server/billing.py`**

```python
"""
Billing and usage metering for Pensyve managed service.

Tiers:
  - Free:       1 namespace, 10K memories, 1K recalls/month
  - Pro:        5 namespaces, 100K memories, 50K recalls/month
  - Team:       25 namespaces, 500K memories, 250K recalls/month
  - Enterprise: unlimited (custom pricing)

Usage tracked per namespace per calendar month.
Stripe handles checkout and subscription management.
"""

from __future__ import annotations

import os
from dataclasses import dataclass
from enum import Enum
from typing import Any


class Tier(str, Enum):
    FREE = "free"
    PRO = "pro"
    TEAM = "team"
    ENTERPRISE = "enterprise"


@dataclass(frozen=True)
class TierLimits:
    max_namespaces: int
    max_memories: int
    max_recalls_per_month: int


TIER_LIMITS: dict[Tier, TierLimits] = {
    Tier.FREE: TierLimits(
        max_namespaces=1,
        max_memories=10_000,
        max_recalls_per_month=1_000,
    ),
    Tier.PRO: TierLimits(
        max_namespaces=5,
        max_memories=100_000,
        max_recalls_per_month=50_000,
    ),
    Tier.TEAM: TierLimits(
        max_namespaces=25,
        max_memories=500_000,
        max_recalls_per_month=250_000,
    ),
    Tier.ENTERPRISE: TierLimits(
        max_namespaces=999_999,
        max_memories=999_999_999,
        max_recalls_per_month=999_999_999,
    ),
}


@dataclass
class UsageRecord:
    namespace_id: str
    period: str  # YYYY-MM
    api_calls: int = 0
    recalls: int = 0
    memories_stored: int = 0
    storage_bytes: int = 0
    embedding_ops: int = 0


class BillingService:
    """Usage metering and limit enforcement.

    In production, this talks to DynamoDB for usage counters
    and Stripe for subscription management. In local/dev mode,
    it operates without external dependencies (no limits enforced).
    """

    def __init__(self) -> None:
        self._enabled = os.environ.get("PENSYVE_BILLING_ENABLED", "false").lower() == "true"
        self._stripe_key = os.environ.get("PENSYVE_STRIPE_SECRET_KEY", "")

    @property
    def enabled(self) -> bool:
        return self._enabled

    def get_tier(self, namespace_id: str) -> Tier:
        """Look up the subscription tier for a namespace."""
        if not self._enabled:
            return Tier.ENTERPRISE  # no limits in local mode
        # TODO: look up in DynamoDB subscriptions table
        return Tier.FREE

    def get_limits(self, namespace_id: str) -> TierLimits:
        """Get the usage limits for a namespace based on its tier."""
        return TIER_LIMITS[self.get_tier(namespace_id)]

    def check_limit(self, namespace_id: str, operation: str) -> bool:
        """Check if a namespace has exceeded its limits for the given operation.

        Returns True if the operation is allowed, False if limit exceeded.
        """
        if not self._enabled:
            return True
        # TODO: check current usage against tier limits
        return True

    def record_usage(self, namespace_id: str, operation: str, count: int = 1) -> None:
        """Record a usage event for billing."""
        if not self._enabled:
            return
        # TODO: increment DynamoDB counter
        pass

    def create_checkout_session(self, namespace_id: str, tier: Tier) -> dict[str, Any]:
        """Create a Stripe checkout session for upgrading."""
        if not self._enabled:
            return {"error": "Billing not enabled"}
        # TODO: create Stripe checkout session
        return {"url": "https://checkout.stripe.com/..."}

    def handle_webhook(self, payload: bytes, signature: str) -> dict[str, Any]:
        """Handle Stripe webhook events."""
        if not self._enabled:
            return {"error": "Billing not enabled"}
        # TODO: verify signature, process event
        return {"status": "processed"}
```

- [ ] Git commit in pensyve: `feat: add billing module scaffold — tiers, usage metering, Stripe integration`

---

## Verification Checklist

After completing all tasks, verify the full Track 5 implementation:

- [ ] **Secrets (5.3):** Run `pre-commit run --all-files` — no secrets detected
- [ ] **Secrets (5.3):** Run `gitleaks detect --source . --verbose` — clean history
- [ ] **Secrets (5.3):** `.gitignore` covers `.env*`, `*.pem`, `*.tfstate*`, `*.tfvars`
- [ ] **Infra (5.1):** Run `cd pensyve-infra/infra && tofu init -backend=false && tofu validate` — passes
- [ ] **Infra (5.1):** Run `tofu fmt -check -recursive` — no formatting issues
- [ ] **Infra (5.1):** All 8 modules have `variables.tf`, `main.tf`, `outputs.tf`
- [ ] **Infra (5.1):** Three environment tfvars (dev, staging, prod) with appropriate sizing
- [ ] **Container (5.2):** Run `docker build -t pensyve-api:test .` — builds successfully
- [ ] **Container (5.2):** Run container and verify `/v1/health` responds 200
- [ ] **CI (5.2):** `ci.yml` runs lint, test, build for Rust/Python/TypeScript
- [ ] **CI (5.2):** `deploy.yml` deploys to ECS with health check
- [ ] **CI (5.2):** `infra.yml` runs tofu plan on PR, apply on merge
- [ ] **Website (5.4):** Run `cd website && npm run build` — builds to `dist/`
- [ ] **Website (5.4):** Landing page, docs, API reference, blog, changelog pages exist
- [ ] **Billing (5.5):** `billing.py` defines tiers, limits, and Stripe scaffold
- [ ] **Billing (5.5):** `billing` OpenTofu module creates DynamoDB tables
- [ ] **Cross-repo:** pensyve-infra is initialized with git, has .gitignore, has README

---

## File Inventory

### pensyve repo (public) — new/modified files

| File | Task | Action |
|------|------|--------|
| `.gitignore` | 5.3.1 | Modified (append) |
| `.pre-commit-config.yaml` | 5.3.2 | New |
| `.gitleaks.toml` | 5.3.2 | New |
| `.env.example` | 5.3.3 | New |
| `.github/workflows/secrets-scan.yml` | 5.3.4 | New |
| `.github/workflows/ci.yml` | 5.2.2 | New |
| `Dockerfile` | 5.2.1 | New |
| `.dockerignore` | 5.2.1 | New |
| `website/astro.config.mjs` | 5.4.1 | New |
| `website/tsconfig.json` | 5.4.1 | New |
| `website/src/layouts/BaseLayout.astro` | 5.4.1 | New |
| `website/src/pages/index.astro` | 5.4.1 | New |
| `website/src/pages/docs.astro` | 5.4.1 | New |
| `website/src/pages/api.astro` | 5.4.1 | New |
| `website/src/pages/blog.astro` | 5.4.1 | New |
| `website/src/pages/changelog.astro` | 5.4.1 | New |
| `website/public/favicon.svg` | 5.4.1 | New |
| `pensyve_server/billing.py` | 5.5.2 | New |

### pensyve-infra repo (private) — all new files

| File | Task |
|------|------|
| `.gitignore` | 5.1.0 |
| `README.md` | 5.1.0 |
| `infra/versions.tf` | 5.1.1 |
| `infra/variables.tf` | 5.1.1 |
| `infra/outputs.tf` | 5.1.1 |
| `infra/main.tf` | 5.1.1 |
| `infra/modules/networking/main.tf` | 5.1.2 |
| `infra/modules/networking/variables.tf` | 5.1.2 |
| `infra/modules/networking/outputs.tf` | 5.1.2 |
| `infra/modules/secrets/main.tf` | 5.1.3 |
| `infra/modules/secrets/variables.tf` | 5.1.3 |
| `infra/modules/secrets/outputs.tf` | 5.1.3 |
| `infra/modules/data/main.tf` | 5.1.4 |
| `infra/modules/data/variables.tf` | 5.1.4 |
| `infra/modules/data/outputs.tf` | 5.1.4 |
| `infra/modules/storage/main.tf` | 5.1.5 |
| `infra/modules/storage/variables.tf` | 5.1.5 |
| `infra/modules/storage/outputs.tf` | 5.1.5 |
| `infra/modules/compute/main.tf` | 5.1.6 |
| `infra/modules/compute/variables.tf` | 5.1.6 |
| `infra/modules/compute/outputs.tf` | 5.1.6 |
| `infra/modules/cdn/main.tf` | 5.1.7 |
| `infra/modules/cdn/variables.tf` | 5.1.7 |
| `infra/modules/cdn/outputs.tf` | 5.1.7 |
| `infra/modules/dns/main.tf` | 5.1.8 |
| `infra/modules/dns/variables.tf` | 5.1.8 |
| `infra/modules/dns/outputs.tf` | 5.1.8 |
| `infra/modules/monitoring/main.tf` | 5.1.9 |
| `infra/modules/monitoring/variables.tf` | 5.1.9 |
| `infra/modules/monitoring/outputs.tf` | 5.1.9 |
| `infra/modules/billing/main.tf` | 5.5.1 |
| `infra/modules/billing/variables.tf` | 5.5.1 |
| `infra/modules/billing/outputs.tf` | 5.5.1 |
| `infra/environments/dev/terraform.tfvars` | 5.1.10 |
| `infra/environments/staging/terraform.tfvars` | 5.1.10 |
| `infra/environments/prod/terraform.tfvars` | 5.1.10 |
| `.github/workflows/deploy.yml` | 5.2.3 |
| `.github/workflows/release.yml` | 5.2.4 |
| `.github/workflows/infra.yml` | 5.2.5 |

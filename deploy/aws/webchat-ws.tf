###############################################################################
# Greentic WebChat Operator — AWS deployment module (WebSocket transport)
#
# Implements the AWS reference deployment described in spec section 12.1 of
# docs/superpowers/specs/2026-04-30-webchat-directline-websocket-design.md.
#
# Provisions:
#   - ALB with HTTPS (443) + HTTP→HTTPS redirect, idle_timeout sized for WS.
#   - IP-target group with cookie stickiness and /healthz health check.
#   - Fargate task + service running the operator image.
#   - ElastiCache Serverless (Valkey) Redis pub/sub backplane (spec §8).
#   - Application Auto Scaling on a custom CloudWatch connection-count metric.
#
# This module assumes shared networking (VPC + subnets) is created out-of-band
# and passed in as variables. It does not create VPCs, NAT gateways, route
# tables, IAM roles, or KMS keys.
###############################################################################

terraform {
  required_version = ">= 1.5.0"

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
  }
}

provider "aws" {
  region = var.region

  default_tags {
    tags = local.common_tags
  }
}

###############################################################################
# Variables
###############################################################################

variable "region" {
  description = "AWS region to deploy the webchat operator into."
  type        = string

  validation {
    condition     = can(regex("^[a-z]{2}-[a-z]+-[0-9]+$", var.region))
    error_message = "region must be a valid AWS region identifier (e.g. eu-west-1)."
  }
}

variable "environment" {
  description = "Deployment environment name. Drives tag values and resource naming."
  type        = string

  validation {
    condition     = can(regex("^(dev|staging|prod)$", var.environment))
    error_message = "environment must be one of: dev, staging, prod."
  }
}

variable "operator_image" {
  description = "Container image reference for the greentic-runner operator (e.g. ghcr.io/greenticai/greentic-runner:0.5.x)."
  type        = string

  validation {
    condition     = length(var.operator_image) > 0
    error_message = "operator_image must not be empty."
  }
}

variable "public_dns_name" {
  description = "Optional public DNS name the ALB will serve. Informational only; this module does not create Route53 records."
  type        = optional(string)
  default     = null
}

variable "acm_certificate_arn" {
  description = "ARN of an ACM certificate (in the same region) covering the public DNS name. Used by the HTTPS listener."
  type        = string

  validation {
    condition     = can(regex("^arn:aws:acm:", var.acm_certificate_arn))
    error_message = "acm_certificate_arn must be a valid ACM certificate ARN."
  }
}

variable "vpc_id" {
  description = "VPC the ALB, ECS service, and ElastiCache cache will live in."
  type        = string

  validation {
    condition     = can(regex("^vpc-", var.vpc_id))
    error_message = "vpc_id must be a valid VPC identifier (vpc-...)."
  }
}

variable "private_subnet_ids" {
  description = "Private subnets used by the Fargate tasks and the ElastiCache cache. Must span at least two AZs."
  type        = list(string)

  validation {
    condition     = length(var.private_subnet_ids) >= 2
    error_message = "private_subnet_ids must contain at least two subnets across distinct AZs."
  }
}

variable "public_subnet_ids" {
  description = "Public subnets the internet-facing ALB attaches to. Must span at least two AZs."
  type        = list(string)

  validation {
    condition     = length(var.public_subnet_ids) >= 2
    error_message = "public_subnet_ids must contain at least two subnets across distinct AZs."
  }
}

variable "redis_url" {
  description = "Optional override for REDIS_URL passed to the operator. When empty/null the operator is wired to the provisioned ElastiCache Serverless endpoint (spec §8 Redis backplane)."
  type        = optional(string)
  default     = null
}

variable "log_group_name" {
  description = "CloudWatch Logs group name for operator stdout/stderr. The group is created and managed by this module."
  type        = string
  default     = "/greentic/webchat-ws"
}

variable "task_cpu" {
  description = "Fargate task CPU units (e.g. 512, 1024). 1024 = 1 vCPU."
  type        = number
  default     = 1024
}

variable "task_memory" {
  description = "Fargate task memory (MiB). Must be compatible with task_cpu per Fargate sizing matrix."
  type        = number
  default     = 2048
}

variable "desired_count" {
  description = "Initial desired task count. Auto-scaling adjusts between min/max_capacity at runtime."
  type        = number
  default     = 2

  validation {
    condition     = var.desired_count >= 1
    error_message = "desired_count must be at least 1."
  }
}

variable "min_capacity" {
  description = "Minimum task count for the auto-scaling target. Spec recommends >= 2 to avoid cold-start gaps."
  type        = number
  default     = 2
}

variable "max_capacity" {
  description = "Maximum task count for the auto-scaling target."
  type        = number
  default     = 20
}

variable "rust_log" {
  description = "RUST_LOG directive forwarded to the operator process."
  type        = string
  default     = "info,greentic_runner_host=info"
}

###############################################################################
# Locals
###############################################################################

locals {
  name_prefix = "greentic-webchat-${var.environment}"

  common_tags = {
    Project     = "Greentic"
    Component   = "webchat-ws"
    Environment = var.environment
  }

  # When no override is supplied, derive REDIS_URL from the ElastiCache Serverless
  # endpoint. Serverless caches expose TLS on 6379, so the rediss:// scheme is used.
  effective_redis_url = coalesce(
    var.redis_url,
    "rediss://${aws_elasticache_serverless_cache.webchat.endpoint[0].address}:${aws_elasticache_serverless_cache.webchat.endpoint[0].port}",
  )
}

###############################################################################
# Security groups
###############################################################################

resource "aws_security_group" "alb" {
  name        = "${local.name_prefix}-alb"
  description = "Ingress for the Greentic webchat ALB (HTTP redirect + HTTPS)."
  vpc_id      = var.vpc_id

  ingress {
    description      = "HTTP (redirected to HTTPS)"
    from_port        = 80
    to_port          = 80
    protocol         = "tcp"
    cidr_blocks      = ["0.0.0.0/0"]
    ipv6_cidr_blocks = ["::/0"]
  }

  ingress {
    description      = "HTTPS / WSS"
    from_port        = 443
    to_port          = 443
    protocol         = "tcp"
    cidr_blocks      = ["0.0.0.0/0"]
    ipv6_cidr_blocks = ["::/0"]
  }

  egress {
    description = "Allow ALB → tasks (and outbound for health checks)."
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
}

resource "aws_security_group" "ecs_tasks" {
  name        = "${local.name_prefix}-tasks"
  description = "Greentic webchat operator task ENIs (Fargate awsvpc)."
  vpc_id      = var.vpc_id

  ingress {
    description     = "App traffic from the ALB."
    from_port       = 8080
    to_port         = 8080
    protocol        = "tcp"
    security_groups = [aws_security_group.alb.id]
  }

  egress {
    description = "Outbound (registry pulls, Redis, telemetry)."
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
}

# Spec §12.1: ElastiCache security group permits :6379 from the ECS service SG only.
resource "aws_security_group" "redis" {
  name        = "${local.name_prefix}-redis"
  description = "ElastiCache Serverless ingress restricted to webchat operator tasks."
  vpc_id      = var.vpc_id

  ingress {
    description     = "Redis/Valkey from operator tasks only."
    from_port       = 6379
    to_port         = 6379
    protocol        = "tcp"
    security_groups = [aws_security_group.ecs_tasks.id]
  }

  egress {
    description = "Allow Redis service traffic outbound (replication, etc.)."
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
}

###############################################################################
# Application Load Balancer
###############################################################################

resource "aws_lb" "webchat" {
  name               = "${local.name_prefix}-alb"
  load_balancer_type = "application"
  internal           = false
  security_groups    = [aws_security_group.alb.id]
  subnets            = var.public_subnet_ids

  # Spec §12.1: 10 min idle timeout. The 25 s server keepalive ping (spec §6.4)
  # sits comfortably under this floor on every cloud LB.
  idle_timeout = 600

  drop_invalid_header_fields = true
  enable_http2               = true
}

resource "aws_lb_target_group" "webchat" {
  name        = "${local.name_prefix}-tg"
  port        = 8080
  protocol    = "HTTP"
  target_type = "ip" # Fargate awsvpc (spec §12.1)
  vpc_id      = var.vpc_id

  # Spec §12.1: deregistration_delay must be < Fargate stopTimeout cap (120 s).
  # 110 s mirrors the task stopTimeout with a 10 s safety margin so the LB stops
  # forwarding before the container is force-killed.
  deregistration_delay = 110

  # Cookie-based stickiness keeps a given conversation pinned to a replica when
  # possible (spec §5.1). Stickiness is an optimization — the Redis backplane
  # (spec §8) covers the case where it cannot be honored.
  stickiness {
    type            = "app_cookie"
    cookie_name     = "GTC_AFFINITY"
    cookie_duration = 86400 # 24 h, per spec §12.1
    enabled         = true
  }

  health_check {
    enabled             = true
    path                = "/healthz"
    port                = "traffic-port"
    protocol            = "HTTP"
    matcher             = "200"
    interval            = 15
    timeout             = 5
    healthy_threshold   = 2
    unhealthy_threshold = 3
  }
}

resource "aws_lb_listener" "https" {
  load_balancer_arn = aws_lb.webchat.arn
  port              = 443
  protocol          = "HTTPS"
  ssl_policy        = "ELBSecurityPolicy-TLS13-1-2-2021-06"
  certificate_arn   = var.acm_certificate_arn

  default_action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.webchat.arn
  }
}

resource "aws_lb_listener" "http_redirect" {
  load_balancer_arn = aws_lb.webchat.arn
  port              = 80
  protocol          = "HTTP"

  default_action {
    type = "redirect"

    redirect {
      port        = "443"
      protocol    = "HTTPS"
      status_code = "HTTP_301"
    }
  }
}

###############################################################################
# CloudWatch log group + IAM
###############################################################################

resource "aws_cloudwatch_log_group" "webchat" {
  name              = var.log_group_name
  retention_in_days = 30
}

data "aws_iam_policy_document" "ecs_task_assume" {
  statement {
    effect  = "Allow"
    actions = ["sts:AssumeRole"]

    principals {
      type        = "Service"
      identifiers = ["ecs-tasks.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "task_execution" {
  name               = "${local.name_prefix}-task-exec"
  assume_role_policy = data.aws_iam_policy_document.ecs_task_assume.json
}

resource "aws_iam_role_policy_attachment" "task_execution_managed" {
  role       = aws_iam_role.task_execution.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AmazonECSTaskExecutionRolePolicy"
}

resource "aws_iam_role" "task" {
  name               = "${local.name_prefix}-task"
  assume_role_policy = data.aws_iam_policy_document.ecs_task_assume.json
}

# Permits the operator to publish the custom WebsocketConnectionsPerTask metric
# that drives auto-scaling (spec §12.1). Scoped to the Greentic/Webchat namespace
# via condition; broader IAM hardening (KMS, secrets) is left to the caller.
data "aws_iam_policy_document" "task_inline" {
  statement {
    sid       = "PublishWebchatMetrics"
    effect    = "Allow"
    actions   = ["cloudwatch:PutMetricData"]
    resources = ["*"]

    condition {
      test     = "StringEquals"
      variable = "cloudwatch:namespace"
      values   = ["Greentic/Webchat"]
    }
  }
}

resource "aws_iam_role_policy" "task_inline" {
  name   = "${local.name_prefix}-task-inline"
  role   = aws_iam_role.task.id
  policy = data.aws_iam_policy_document.task_inline.json
}

###############################################################################
# ECS cluster + service
###############################################################################

resource "aws_ecs_cluster" "webchat" {
  name = "${local.name_prefix}-cluster"

  setting {
    name  = "containerInsights"
    value = "enabled"
  }
}

resource "aws_ecs_cluster_capacity_providers" "webchat" {
  cluster_name       = aws_ecs_cluster.webchat.name
  capacity_providers = ["FARGATE", "FARGATE_SPOT"]

  default_capacity_provider_strategy {
    capacity_provider = "FARGATE"
    weight            = 1
    base              = 1
  }
}

resource "aws_ecs_task_definition" "webchat" {
  family                   = "${local.name_prefix}-operator"
  network_mode             = "awsvpc"
  requires_compatibilities = ["FARGATE"]
  cpu                      = tostring(var.task_cpu)
  memory                   = tostring(var.task_memory)
  execution_role_arn       = aws_iam_role.task_execution.arn
  task_role_arn            = aws_iam_role.task.arn

  container_definitions = jsonencode([
    {
      name      = "webchat"
      image     = var.operator_image
      essential = true

      # Spec §11/§12.1: stopTimeout ≤ Fargate cap (120 s) with a 10 s margin
      # so the in-process WS drain (default 30 s) plus pre-drain grace fits.
      stopTimeout = 110

      portMappings = [
        {
          name          = "http"
          containerPort = 8080
          hostPort      = 8080
          protocol      = "tcp"
          appProtocol   = "http"
        },
      ]

      environment = [
        { name = "PORT", value = "8080" },
        { name = "REDIS_URL", value = local.effective_redis_url },
        { name = "RUST_LOG", value = var.rust_log },
        # Spec §13.1: WS handler is gated behind this flag during rollout.
        { name = "WEBCHAT_WS_ENABLED", value = "true" },
        { name = "GREENTIC_ENV", value = var.environment },
      ]

      healthCheck = {
        # Container-level liveness ping; ALB health check is the source of
        # truth for traffic decisions, this catches in-process hangs.
        command     = ["CMD-SHELL", "curl -fsS http://127.0.0.1:8080/healthz || exit 1"]
        interval    = 30
        timeout     = 5
        retries     = 3
        startPeriod = 30
      }

      logConfiguration = {
        logDriver = "awslogs"
        options = {
          "awslogs-group"         = aws_cloudwatch_log_group.webchat.name
          "awslogs-region"        = var.region
          "awslogs-stream-prefix" = "webchat"
        }
      }
    },
  ])
}

resource "aws_ecs_service" "webchat" {
  name            = "${local.name_prefix}-svc"
  cluster         = aws_ecs_cluster.webchat.id
  task_definition = aws_ecs_task_definition.webchat.arn
  desired_count   = var.desired_count
  launch_type     = "FARGATE"

  # Avoid restarting the whole fleet on a deploy: rolling 50%/100% replacement
  # gives the WS drain logic (spec §11) headroom to evict gracefully.
  deployment_minimum_healthy_percent = 100
  deployment_maximum_percent         = 200

  # Spec §11: tasks need the full stopTimeout window to drain WS connections
  # before traffic is shifted away.
  health_check_grace_period_seconds = 60

  enable_execute_command = false
  propagate_tags         = "TASK_DEFINITION"

  network_configuration {
    subnets          = var.private_subnet_ids
    security_groups  = [aws_security_group.ecs_tasks.id]
    assign_public_ip = false
  }

  load_balancer {
    target_group_arn = aws_lb_target_group.webchat.arn
    container_name   = "webchat"
    container_port   = 8080
  }

  # Auto-scaling owns desired_count after the initial create; without ignoring
  # this, terraform would clobber scaling decisions on every plan.
  lifecycle {
    ignore_changes = [desired_count]
  }

  depends_on = [
    aws_lb_listener.https,
    aws_lb_listener.http_redirect,
  ]
}

###############################################################################
# ElastiCache Serverless (Valkey) — Redis pub/sub backplane (spec §8)
###############################################################################

resource "aws_elasticache_serverless_cache" "webchat" {
  name        = "${local.name_prefix}-redis"
  description = "Greentic webchat WS backplane (pub/sub)."
  engine      = "valkey"

  cache_usage_limits {
    # Spec §8.6: webchat traffic is sparse; cap storage to keep cost predictable.
    data_storage {
      maximum = 5
      unit    = "GB"
    }

    # Conservative ceiling; backplane traffic is bounded by bot reply volume
    # rather than connection count. Caller can raise via override if needed.
    ecpu_per_second {
      maximum = 5000
    }
  }

  # Spec §12.1: Daily snapshot retention 1.
  daily_snapshot_time      = "03:00"
  snapshot_retention_limit = 1

  security_group_ids = [aws_security_group.redis.id]
  subnet_ids         = var.private_subnet_ids
}

###############################################################################
# Application Auto Scaling — target tracking on custom CloudWatch metric
###############################################################################

resource "aws_appautoscaling_target" "webchat" {
  max_capacity       = var.max_capacity
  min_capacity       = var.min_capacity
  resource_id        = "service/${aws_ecs_cluster.webchat.name}/${aws_ecs_service.webchat.name}"
  scalable_dimension = "ecs:service:DesiredCount"
  service_namespace  = "ecs"
}

# Spec §12.1: target tracking on Greentic/Webchat WebsocketConnectionsPerTask,
# target value 500, scale-out 60 s, scale-in 300 s. Each task publishes the
# metric every 60 s (operator side, not provisioned here).
resource "aws_appautoscaling_policy" "ws_connections" {
  name               = "${local.name_prefix}-ws-conn-tracking"
  policy_type        = "TargetTrackingScaling"
  resource_id        = aws_appautoscaling_target.webchat.resource_id
  scalable_dimension = aws_appautoscaling_target.webchat.scalable_dimension
  service_namespace  = aws_appautoscaling_target.webchat.service_namespace

  target_tracking_scaling_policy_configuration {
    target_value       = 500
    scale_out_cooldown = 60
    scale_in_cooldown  = 300

    customized_metric_specification {
      metric_name = "WebsocketConnectionsPerTask"
      namespace   = "Greentic/Webchat"
      statistic   = "Average"
      unit        = "Count"

      dimensions {
        name  = "ClusterName"
        value = aws_ecs_cluster.webchat.name
      }

      dimensions {
        name  = "ServiceName"
        value = "${local.name_prefix}-svc"
      }
    }
  }
}

###############################################################################
# Outputs
###############################################################################

output "alb_dns_name" {
  description = "Public DNS name of the webchat ALB (use as the CNAME target for the public_dns_name)."
  value       = aws_lb.webchat.dns_name
}

output "redis_endpoint" {
  description = "ElastiCache Serverless endpoint (host:port) for the Redis backplane."
  value       = "${aws_elasticache_serverless_cache.webchat.endpoint[0].address}:${aws_elasticache_serverless_cache.webchat.endpoint[0].port}"
}

output "ecs_service_name" {
  description = "Name of the Fargate service running the webchat operator."
  value       = aws_ecs_service.webchat.name
}

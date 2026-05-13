#!/usr/bin/env bash
# =============================================================================
#  Greentic WebChat operator — GCP Cloud Run deployment (WebSocket-tuned)
# -----------------------------------------------------------------------------
#  Spec source of truth:
#    docs/superpowers/specs/2026-04-30-webchat-directline-websocket-design.md
#  This script implements the GCP variant from spec section 12.2, with
#  cross-references to sections 5 (architecture), 8 (Redis backplane),
#  9 (resource limits) and 11 (graceful shutdown).
#
#  Required environment variables (no defaults):
#    PROJECT_ID        — target GCP project id
#    OPERATOR_IMAGE    — Artifact Registry image (e.g.
#                        europe-west1-docker.pkg.dev/PROJECT/greentic/webchat:0.5.x)
#    VPC_NETWORK       — VPC self-link or short name (Direct VPC egress)
#    VPC_SUBNET        — subnet name in REGION (Direct VPC egress)
#    SERVICE_ACCOUNT   — runtime SA, e.g. webchat@PROJECT.iam.gserviceaccount.com
#
#  Optional (with defaults):
#    REGION                — default europe-west1
#    SERVICE_NAME          — default greentic-webchat
#    REDIS_INSTANCE_NAME   — default greentic-webchat-redis
#    REDIS_TIER            — default standard (HA, per spec 18.6 recommendation)
#    REDIS_SIZE_GB         — default 1
#    REDIS_VERSION         — default redis_7_2
#    REDIS_AUTH_SECRET     — default greentic-webchat-redis-auth (Secret Manager id)
#    RUST_LOG              — default info,greentic_runner=debug
#
#  Idempotency: every create step checks for existing resources and skips.
# =============================================================================

set -euo pipefail

# -----------------------------------------------------------------------------
# Config (with the bash ${VAR:?missing} guard for required inputs)
# -----------------------------------------------------------------------------
PROJECT_ID="${PROJECT_ID:?PROJECT_ID is required}"
OPERATOR_IMAGE="${OPERATOR_IMAGE:?OPERATOR_IMAGE is required (e.g. europe-west1-docker.pkg.dev/PROJECT/greentic/webchat:0.5.x)}"
VPC_NETWORK="${VPC_NETWORK:?VPC_NETWORK is required (Direct VPC egress per spec 12.2)}"
VPC_SUBNET="${VPC_SUBNET:?VPC_SUBNET is required (must be in REGION)}"
SERVICE_ACCOUNT="${SERVICE_ACCOUNT:?SERVICE_ACCOUNT is required (runtime identity)}"

REGION="${REGION:-europe-west1}"
SERVICE_NAME="${SERVICE_NAME:-greentic-webchat}"
REDIS_INSTANCE_NAME="${REDIS_INSTANCE_NAME:-greentic-webchat-redis}"
REDIS_TIER="${REDIS_TIER:-standard}"             # spec 12.2: HA tier for prod
REDIS_SIZE_GB="${REDIS_SIZE_GB:-1}"              # spec 8.6: 1 GiB oversized for 1k users
REDIS_VERSION="${REDIS_VERSION:-redis_7_2}"
REDIS_AUTH_SECRET="${REDIS_AUTH_SECRET:-greentic-webchat-redis-auth}"
RUST_LOG="${RUST_LOG:-info,greentic_runner=debug}"

# -----------------------------------------------------------------------------
# Logging helper
# -----------------------------------------------------------------------------
log() {
  echo "==> $*"
}

# -----------------------------------------------------------------------------
# Pre-flight: ensure required gcloud APIs are enabled
# -----------------------------------------------------------------------------
enable_apis() {
  log "Enabling required GCP APIs in project ${PROJECT_ID}"
  gcloud services enable \
    run.googleapis.com \
    redis.googleapis.com \
    secretmanager.googleapis.com \
    servicenetworking.googleapis.com \
    compute.googleapis.com \
    --project="${PROJECT_ID}"
}

# -----------------------------------------------------------------------------
# Memorystore Redis (spec section 8 — Redis backplane)
#   - tier=standard (HA) per spec 12.2 + 18.6
#   - auth + transit encryption per spec 8.1 (rediss:// in prod)
#   - PRIVATE_SERVICE_ACCESS connect mode for VPC-native Cloud Run egress
# -----------------------------------------------------------------------------
ensure_redis() {
  log "Checking Memorystore Redis instance '${REDIS_INSTANCE_NAME}' in ${REGION}"
  if gcloud redis instances describe "${REDIS_INSTANCE_NAME}" \
      --region="${REGION}" \
      --project="${PROJECT_ID}" >/dev/null 2>&1; then
    log "Redis instance '${REDIS_INSTANCE_NAME}' already exists — skipping create (idempotent)"
  else
    log "Creating Memorystore Redis instance '${REDIS_INSTANCE_NAME}' (tier=${REDIS_TIER}, size=${REDIS_SIZE_GB}GiB)"
    gcloud redis instances create "${REDIS_INSTANCE_NAME}" \
      --project="${PROJECT_ID}" \
      --region="${REGION}" \
      --tier="${REDIS_TIER}" \
      --size="${REDIS_SIZE_GB}" \
      --redis-version="${REDIS_VERSION}" \
      --network="${VPC_NETWORK}" \
      --connect-mode=PRIVATE_SERVICE_ACCESS \
      --enable-auth \
      --transit-encryption-mode=SERVER_AUTHENTICATION
  fi
}

# -----------------------------------------------------------------------------
# Mirror the Redis AUTH string into Secret Manager so Cloud Run can mount it
# as an env var. The auth string is generated by Memorystore on create.
# -----------------------------------------------------------------------------
sync_redis_auth_secret() {
  log "Syncing Memorystore AUTH string into Secret Manager id '${REDIS_AUTH_SECRET}'"
  local auth
  auth="$(gcloud redis instances get-auth-string "${REDIS_INSTANCE_NAME}" \
            --region="${REGION}" \
            --project="${PROJECT_ID}" \
            --format='value(authString)')"

  if gcloud secrets describe "${REDIS_AUTH_SECRET}" \
      --project="${PROJECT_ID}" >/dev/null 2>&1; then
    log "Secret '${REDIS_AUTH_SECRET}' exists — adding new version"
  else
    log "Creating Secret Manager secret '${REDIS_AUTH_SECRET}'"
    gcloud secrets create "${REDIS_AUTH_SECRET}" \
      --project="${PROJECT_ID}" \
      --replication-policy=automatic
  fi

  printf '%s' "${auth}" | gcloud secrets versions add "${REDIS_AUTH_SECRET}" \
    --project="${PROJECT_ID}" \
    --data-file=-
}

# -----------------------------------------------------------------------------
# Resolve Redis host (private IP) + port for env injection.
# -----------------------------------------------------------------------------
resolve_redis_endpoint() {
  REDIS_HOST="$(gcloud redis instances describe "${REDIS_INSTANCE_NAME}" \
                  --region="${REGION}" \
                  --project="${PROJECT_ID}" \
                  --format='value(host)')"
  REDIS_PORT="$(gcloud redis instances describe "${REDIS_INSTANCE_NAME}" \
                  --region="${REGION}" \
                  --project="${PROJECT_ID}" \
                  --format='value(port)')"
  log "Resolved Redis endpoint: ${REDIS_HOST}:${REDIS_PORT}"
}

# -----------------------------------------------------------------------------
# Cloud Run service deployment (spec section 12.2).
#
# Flag-by-flag rationale:
#   --execution-environment=gen2 .... full Linux + reliable SIGTERM trap
#                                     (spec 11.1 graceful shutdown sequence)
#   --concurrency=250 ............... 1 WS = 1 concurrent request; default 80
#                                     too tight for the per-replica conn pool
#                                     (spec 9 resource limits + 12.2)
#   --cpu=1 / --memory=512Mi ........ baseline, scale via max-instances
#   --min-instances=2 ............... avoid cold-start drops (spec 12.2)
#   --max-instances=20 .............. caps blast radius; tune per tenant
#   --timeout=3600 .................. Cloud Run hard cap (60 min). Token TTL
#                                     (~30 min, spec 7.2) forces reconnect well
#                                     before this ceiling is reached
#   --no-cpu-throttling ............. instance-based billing for steady WS load
#                                     (spec 12.2)
#   --session-affinity .............. best-effort sticky cookie (spec 5.1).
#                                     Redis backplane covers gaps when sticky
#                                     fails (spec 16 risk register)
#   --network/--subnet/--vpc-egress=private-ranges-only ..
#                                     Direct VPC egress (modern default,
#                                     replaces legacy Serverless VPC Connector)
#   NO --use-http2 .................. end-to-end HTTP/2 breaks the WS upgrade
#                                     in the Microsoft Web Chat library
#                                     (spec 12.2 — explicit warning)
# -----------------------------------------------------------------------------
deploy_service() {
  log "Deploying Cloud Run service '${SERVICE_NAME}' to ${REGION}"

  gcloud run deploy "${SERVICE_NAME}" \
    --project="${PROJECT_ID}" \
    --region="${REGION}" \
    --image="${OPERATOR_IMAGE}" \
    --platform=managed \
    --execution-environment=gen2 \
    --concurrency=250 \
    --cpu=1 \
    --memory=512Mi \
    --min-instances=2 \
    --max-instances=20 \
    --timeout=3600 \
    --no-cpu-throttling \
    --session-affinity \
    --network="${VPC_NETWORK}" \
    --subnet="${VPC_SUBNET}" \
    --vpc-egress=private-ranges-only \
    --service-account="${SERVICE_ACCOUNT}" \
    --port=8080 \
    --allow-unauthenticated \
    --set-env-vars="REDIS_HOST=${REDIS_HOST},REDIS_PORT=${REDIS_PORT},RUST_LOG=${RUST_LOG},WEBCHAT_WS_ENABLED=true" \
    --set-secrets="REDIS_AUTH=${REDIS_AUTH_SECRET}:latest"
  # NOTE: --use-http2 is intentionally OMITTED. See spec 12.2.
}

# -----------------------------------------------------------------------------
# Show post-deploy summary so the operator can sanity-check the URL + revision.
# -----------------------------------------------------------------------------
print_summary() {
  log "Deployment complete — fetching service URL and current revision"
  gcloud run services describe "${SERVICE_NAME}" \
    --project="${PROJECT_ID}" \
    --region="${REGION}" \
    --format='value(status.url,status.latestReadyRevisionName)'
}

# -----------------------------------------------------------------------------
# Entrypoint
# -----------------------------------------------------------------------------
main() {
  log "Greentic webchat operator — GCP deploy (project=${PROJECT_ID}, region=${REGION})"
  enable_apis
  ensure_redis
  sync_redis_auth_secret
  resolve_redis_endpoint
  deploy_service
  print_summary
  log "Done."
}

main "$@"

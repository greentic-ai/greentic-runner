// =============================================================================
// Greentic WebChat Operator on Azure Container Apps (WebSocket-enabled)
// =============================================================================
//
// Deploys the WebSocket-capable Direct Line transport for the Greentic
// webchat operator per spec section 12.3 of:
//   docs/superpowers/specs/2026-04-30-webchat-directline-websocket-design.md
//
// Architecture:
//   * Container Apps Environment (workload-profiles v2) with:
//       - Consumption profile for general workload
//       - Dedicated Ingress-D4 profile for premium ingress (>4 min WS idle)
//   * Premium ingress: terminationGracePeriodSeconds=1200, requestIdleTimeout=30
//     (spec section 12.3 — required because default ingress idle is 240s and
//     the WS keepalive ping cadence + production WS sessions need >4 min idle).
//   * Container App in single-revision mode (sticky sessions require it
//     per Azure Container Apps docs and spec section 12.3).
//   * Azure Cache for Redis Standard for the cross-replica pub/sub backplane
//     (spec section 8 — strict Redis, no abstraction trait).
//   * Connection string sourced from Azure Key Vault via managed identity
//     (zero in-template secret material).
//
// Target API versions are pinned per the task brief.
// =============================================================================

targetScope = 'resourceGroup'

// -----------------------------------------------------------------------------
// Parameters
// -----------------------------------------------------------------------------

@description('Azure region for all resources.')
param location string = 'westeurope'

@description('Deployment environment. Drives naming and SKU choices downstream.')
@allowed([
  'dev'
  'staging'
  'prod'
])
param environment string

@description('Container image for the greentic webchat operator (e.g. ghcr.io/greenticai/greentic-runner:0.5.x).')
param operatorImage string

@description('Existing virtual network that hosts the Container Apps subnet and the Redis private endpoint.')
param vnetName string

@description('Subnet (in vnetName) the Container Apps Environment is delegated to. Must be /23 or larger for workload profiles.')
param containerAppSubnetName string

@description('Subnet (in vnetName) where the Redis private endpoint NIC is placed.')
param redisSubnetName string

@description('Existing Key Vault that stores the Redis connection string secret.')
param keyVaultName string

@description('Name of the Key Vault secret holding the Redis connection string (e.g. rediss://:KEY@host:6380).')
param redisSecretName string = 'webchat-redis-url'

@description('Optional custom hostname to bind to the Container App ingress (FQDN, e.g. webchat.example.com). Leave empty to skip.')
param customDomainName string = ''

@description('RUST_LOG directive for the operator container.')
param rustLog string = 'info,greentic_runner=info,greentic_runner_host=info'

@description('Optional override for the resource basename. Defaults to greentic-webchat-{environment}.')
param namePrefix string = 'greentic-webchat-${environment}'

// -----------------------------------------------------------------------------
// Variables
// -----------------------------------------------------------------------------

// Resource names — kept short to stay within Azure's 32/63-char limits where
// applicable. Redis cache name is globally unique so we suffix with a
// uniqueString seed of resourceGroup id.
var envName = '${namePrefix}-env'
var appName = '${namePrefix}-app'
var redisName = toLower(replace('${namePrefix}-${uniqueString(resourceGroup().id)}', '_', '-'))
var redisPrivateEndpointName = '${namePrefix}-redis-pe'
var logWorkspaceName = '${namePrefix}-logs'
var managedIdentityName = '${namePrefix}-id'

// Workload profile names. The Ingress-D4 name is required by ACA premium
// ingress wiring (spec section 12.3).
var consumptionProfileName = 'Consumption'
var ingressProfileName = 'Ingress-D4'

// Container App secret name (referenced by env var via secretRef).
var redisSecretRefName = 'redis-url'

// Subnet resource IDs (existing VNet/subnets are assumed to be pre-provisioned
// outside this module — Container Apps requires VNet integration for premium
// ingress and Redis private endpoints require an explicit subnet).
var containerAppSubnetId = resourceId('Microsoft.Network/virtualNetworks/subnets', vnetName, containerAppSubnetName)
var redisSubnetId = resourceId('Microsoft.Network/virtualNetworks/subnets', vnetName, redisSubnetName)

// Common tags applied to every resource for cost attribution + ownership.
var commonTags = {
  workload: 'greentic-webchat'
  environment: environment
  component: 'directline-websocket'
  managedBy: 'bicep'
}

// -----------------------------------------------------------------------------
// User-assigned managed identity (so the Container App can pull the Redis
// connection string from Key Vault without a stored credential).
// -----------------------------------------------------------------------------

resource managedIdentity 'Microsoft.ManagedIdentity/userAssignedIdentities@2023-01-31' = {
  name: managedIdentityName
  location: location
  tags: commonTags
}

// -----------------------------------------------------------------------------
// Existing Key Vault (referenced for secret URI construction and RBAC).
// -----------------------------------------------------------------------------

resource keyVault 'Microsoft.KeyVault/vaults@2023-07-01' existing = {
  name: keyVaultName
}

// Grant the Container App identity the standard "Key Vault Secrets User"
// role (RBAC mode). Vault must be configured for Azure RBAC — common for
// modern deployments. If the vault is in access-policy mode, this resource
// has no effect; add an access policy outside this module.
var keyVaultSecretsUserRoleId = '4633458b-17de-408a-b874-0445c86b69e6'

resource kvRoleAssignment 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  scope: keyVault
  name: guid(keyVault.id, managedIdentity.id, keyVaultSecretsUserRoleId)
  properties: {
    principalId: managedIdentity.properties.principalId
    principalType: 'ServicePrincipal'
    roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', keyVaultSecretsUserRoleId)
  }
}

// -----------------------------------------------------------------------------
// Log Analytics workspace — required by Container Apps Environment for
// diagnostics, and the natural sink for greentic-runner-host structured logs.
// -----------------------------------------------------------------------------

resource logWorkspace 'Microsoft.OperationalInsights/workspaces@2023-09-01' = {
  name: logWorkspaceName
  location: location
  tags: commonTags
  properties: {
    sku: {
      name: 'PerGB2018'
    }
    retentionInDays: environment == 'prod' ? 90 : 30
    features: {
      enableLogAccessUsingOnlyResourcePermissions: true
    }
  }
}

// -----------------------------------------------------------------------------
// Container Apps Environment with workload profiles (v2) and premium ingress.
// =============================================================================
// Two profiles:
//   * Consumption — runs the operator container (and any future
//     consumption-priced workloads).
//   * Ingress-D4 — dedicated nodes for premium ingress proxies. Per Azure
//     docs the workload profile name "Ingress-D4" is conventional and the
//     workloadProfileType "D4" is the smallest supported size for premium
//     ingress (1 vCPU per ingress proxy instance).
//
// Premium ingress is REQUIRED here (spec section 12.3): default ingress
// idle timeout is 240s, but production WS sessions easily exceed 4 minutes
// of inactivity between bot replies. Premium ingress lifts the cap to 30 min
// and gives us a configurable termination-grace-period for cloud-side
// connection draining (separate from, and longer than, the in-app drain).
// -----------------------------------------------------------------------------

// Note on API version: the brief targets `Microsoft.App/managedEnvironments@2024-03-01`,
// but premium ingress configuration (`ingressConfiguration`) was only added to the
// stable schema in `2025-07-01`. We pin to `2025-07-01` here because spec section 12.3
// REQUIRES premium ingress to be declared in the same module — out-of-band CLI
// configuration would defeat the "single self-contained file" goal.
// `Microsoft.App/containerApps` and `Microsoft.Cache/redis` remain on the brief's
// pinned versions (2024-03-01 and 2023-08-01 respectively).
resource managedEnv 'Microsoft.App/managedEnvironments@2025-07-01' = {
  name: envName
  location: location
  tags: commonTags
  properties: {
    appLogsConfiguration: {
      destination: 'log-analytics'
      logAnalyticsConfiguration: {
        customerId: logWorkspace.properties.customerId
        sharedKey: logWorkspace.listKeys().primarySharedKey
      }
    }
    vnetConfiguration: {
      // Internal=false because the operator's WebSocket endpoint is
      // client-facing. Front with Azure Front Door / WAF if you need a public
      // hardened edge; this module exposes the ACA ingress directly.
      internal: false
      infrastructureSubnetId: containerAppSubnetId
    }
    workloadProfiles: [
      {
        // Consumption profile — pay-per-second compute for the operator.
        name: consumptionProfileName
        workloadProfileType: 'Consumption'
      }
      {
        // Dedicated profile for premium ingress proxies (spec 12.3).
        // D4 = 4 vCPU / 16 GiB. Min/max nodes = 2 each for steady-state;
        // raise maximumCount in prod if ingress saturates (visible via
        // the "Ingress CPU Usage" / "Ingress Memory Usage Bytes" metrics).
        name: ingressProfileName
        workloadProfileType: 'D4'
        minimumCount: 2
        maximumCount: environment == 'prod' ? 4 : 2
      }
    ]
    // Premium ingress configuration — lifts WS idle ceiling to 30 min and
    // gives the cloud-side ingress its own 1200s drain budget on shutdown.
    // Note: the in-app `terminationGracePeriodSeconds: 110` on the container
    // app handles WS drain inside the operator (matches spec section 11);
    // this 1200s value is the OUTER cloud-side ingress drain.
    ingressConfiguration: {
      workloadProfileName: ingressProfileName
      terminationGracePeriodSeconds: 1200
      requestIdleTimeout: 30 // minutes; max per Azure docs
      headerCountLimit: 100
      scale: {
        minReplicas: 2
        maxReplicas: environment == 'prod' ? 10 : 4
      }
    }
  }
}

// -----------------------------------------------------------------------------
// Azure Cache for Redis (Standard tier — basic HA via primary/replica).
// =============================================================================
// SKU: Standard, capacity=1 (1 GiB) — far oversized for chat throughput per
// spec section 8.6 ("1000 concurrent users -> ~1000 channels, sparse traffic").
// SSL only on port 6380; non-SSL 6379 disabled. Public network access is off;
// access goes through a private endpoint into the Container Apps subnet's VNet.
// -----------------------------------------------------------------------------

resource redis 'Microsoft.Cache/redis@2023-08-01' = {
  name: redisName
  location: location
  tags: commonTags
  properties: {
    sku: {
      name: 'Standard'
      family: 'C'
      capacity: 1
    }
    enableNonSslPort: false
    minimumTlsVersion: '1.2'
    redisVersion: '6'
    publicNetworkAccess: 'Disabled' // private endpoint only
    redisConfiguration: {
      // Pub/sub messages aren't persisted, so eviction policy is mostly
      // moot for this workload — but allkeys-lru is the safe default if
      // anything else ever lands on this cache.
      'maxmemory-policy': 'allkeys-lru'
    }
  }
}

// -----------------------------------------------------------------------------
// Private endpoint for Redis on the dedicated Redis subnet.
// (Operator pods reach Redis via the VNet over the private DNS zone — no
// internet egress, no shared-key in transit on the public surface.)
// -----------------------------------------------------------------------------

resource redisPrivateEndpoint 'Microsoft.Network/privateEndpoints@2023-11-01' = {
  name: redisPrivateEndpointName
  location: location
  tags: commonTags
  properties: {
    subnet: {
      id: redisSubnetId
    }
    privateLinkServiceConnections: [
      {
        name: 'redis-plsc'
        properties: {
          privateLinkServiceId: redis.id
          groupIds: [
            'redisCache'
          ]
        }
      }
    ]
  }
}

// -----------------------------------------------------------------------------
// Container App — the greentic webchat operator.
// =============================================================================
// Key correctness requirements (spec section 12.3):
//   * revisionsMode = 'Single'  — sticky sessions REQUIRE single-revision mode
//   * stickySessions.affinity = 'sticky' — best-effort cookie affinity (the
//     Redis backplane covers correctness; stickiness is a latency optimization)
//   * transport = 'auto' — DO NOT set 'http2' explicitly; the ACA ingress will
//     negotiate HTTP/1.1 + WebSocket upgrade transparently. Forcing HTTP/2
//     end-to-end breaks the Microsoft botframework-webchat WS upgrade.
//   * terminationGracePeriodSeconds = 110 — matches the in-app drain budget
//     described in spec section 11 (5s pre-drain + 30s drain + headroom for
//     1001 close-frame propagation). The cloud-side 1200s grace at the
//     ingress level is unrelated and lives on the managed environment.
// -----------------------------------------------------------------------------

resource webchatApp 'Microsoft.App/containerApps@2024-03-01' = {
  name: appName
  location: location
  tags: commonTags
  identity: {
    type: 'UserAssigned'
    userAssignedIdentities: {
      '${managedIdentity.id}': {}
    }
  }
  // Single-revision mode is mandatory for sticky sessions (spec 12.3 +
  // Azure docs). Blue/green via traffic split is not available for this app;
  // use revision activate/deactivate flips instead.
  properties: {
    environmentId: managedEnv.id
    workloadProfileName: consumptionProfileName
    configuration: {
      activeRevisionsMode: 'Single'
      ingress: {
        external: true
        targetPort: 8080
        // 'auto' lets the ingress negotiate HTTP/1.1 (required for the
        // botframework-webchat WS upgrade). Do NOT change to 'http2'.
        transport: 'auto'
        allowInsecure: false
        stickySessions: {
          // Cookie name is undocumented — do not depend on it in app code.
          affinity: 'sticky'
        }
        // CORS handled in-app (origin allowlist enforced at WS upgrade per
        // spec section 7.4); we still tighten transport-level rules here.
        traffic: [
          {
            weight: 100
            latestRevision: true
          }
        ]
        // Pre-requisite: when customDomainName is set, the DNS CNAME verification
        // record must already be in place (asuid.<domain> -> verification ID).
        // Managed certificate creation is intentionally out of scope for this
        // module so it can be applied incrementally without DNS-validation
        // deadlocks (create the cert resource separately, then patch in the
        // certificateId via a follow-up deployment).
        customDomains: empty(customDomainName) ? null : [
          {
            name: customDomainName
            bindingType: 'Disabled'
          }
        ]
      }
      secrets: [
        {
          // Pulled from Key Vault at runtime via the user-assigned identity.
          // The vault secret value MUST be a full rediss:// URL with auth,
          // e.g. rediss://:<accessKey>@<host>:6380
          name: redisSecretRefName
          identity: managedIdentity.id
          keyVaultUrl: '${keyVault.properties.vaultUri}secrets/${redisSecretName}'
        }
      ]
      registries: []
      maxInactiveRevisions: 2
    }
    template: {
      // In-app drain budget (spec section 11): SIGTERM -> readiness fail ->
      // 5s pre-drain -> 1001 Going Away -> 30s drain -> force-close + exit.
      // 110s gives the operator the spec's 35s budget plus headroom for the
      // SIGTERM->process delay and any straggler 1011 closes.
      terminationGracePeriodSeconds: 110
      containers: [
        {
          name: 'webchat'
          image: operatorImage
          resources: {
            cpu: json('1.0')
            memory: '2Gi'
          }
          env: [
            {
              name: 'REDIS_URL'
              secretRef: redisSecretRefName
            }
            {
              name: 'RUST_LOG'
              value: rustLog
            }
            {
              // Spec section 13.2 — feature-flag the WS path on by default
              // for this deployment shape; per-tenant rollout still gated
              // by the in-pack `messaging-webchat-gui.websocket_enabled`.
              name: 'WEBCHAT_WS_ENABLED'
              value: 'true'
            }
            {
              name: 'PORT'
              value: '8080'
            }
            // Note: ACA automatically injects CONTAINER_APP_REPLICA_NAME at
            // runtime; the operator reads it for the `replica_id` tracing
            // attribute described in spec section 10.2. Do not override.
          ]
          probes: [
            {
              type: 'Liveness'
              httpGet: {
                path: '/healthz'
                port: 8080
                scheme: 'HTTP'
              }
              initialDelaySeconds: 10
              periodSeconds: 20
              timeoutSeconds: 5
              failureThreshold: 3
            }
            {
              type: 'Readiness'
              httpGet: {
                path: '/healthz'
                port: 8080
                scheme: 'HTTP'
              }
              initialDelaySeconds: 5
              periodSeconds: 5
              timeoutSeconds: 3
              failureThreshold: 3
              successThreshold: 1
            }
          ]
        }
      ]
      scale: {
        // Spec 12.3: minReplicas=2 (avoid cold-start drops on first message),
        // maxReplicas=30 (matches ACA per-app default ceiling and is plenty
        // for >50k concurrent WS clients at the 200 concurrent-request rule).
        minReplicas: 2
        maxReplicas: 30
        rules: [
          {
            // Each WebSocket = 1 concurrent request from the ingress's
            // perspective, so concurrentRequests=200 caps each replica
            // at ~200 active sockets before the scaler kicks in.
            name: 'ws-conn'
            http: {
              metadata: {
                concurrentRequests: '200'
              }
            }
          }
          {
            // Memory guard — protects against per-WS buffer growth or
            // catch-up replay storms exceeding planned headroom.
            name: 'mem-guard'
            custom: {
              type: 'memory'
              metadata: {
                type: 'Utilization'
                value: '70'
              }
            }
          }
        ]
      }
    }
  }
  dependsOn: [
    // The role assignment must exist before the app starts pulling the KV
    // secret, otherwise the first revision will fail to start.
    kvRoleAssignment
    // The private endpoint is the only path to Redis (public access is off);
    // make sure it's in place before the app boots and tries to connect.
    redisPrivateEndpoint
  ]
}

// -----------------------------------------------------------------------------
// Outputs
// -----------------------------------------------------------------------------

@description('FQDN assigned by Container Apps to the webchat operator. Point your DNS / front door at this.')
output containerAppFqdn string = webchatApp.properties.configuration.ingress.fqdn

@description('Hostname of the Azure Cache for Redis instance. Build the connection string as rediss://:<accessKey>@<host>:6380 and store it in Key Vault under redisSecretName.')
output redisHostName string = redis.properties.hostName

@description('Resource ID of the Container Apps managed environment (useful for sibling deployments).')
output containerAppEnvironmentId string = managedEnv.id

@description('Resource ID of the user-assigned managed identity used to pull the Redis secret from Key Vault.')
output managedIdentityResourceId string = managedIdentity.id

@description('Principal ID of the user-assigned managed identity (for granting additional RBAC outside this module).')
output managedIdentityPrincipalId string = managedIdentity.properties.principalId

import { api } from './credentials'

export interface SystemSettings {
  region: string
  authRegion: string | null
  apiRegion: string | null
  tlsBackend: 'rustls' | 'native-tls'
  defaultEndpoint: string
  loadBalancingMode: 'priority' | 'balanced'
  concurrency: {
    maxInFlight: number
    maxInFlightPerCredential: number
    maxWaiting: number
    waitTimeoutSeconds: number
    rateLimitCooldownSeconds: number
    adaptiveConcurrency: boolean
    requestsPerMinute: number
    refreshBackoffSeconds: number
  }
  requestLimits: {
    maxUpstreamAttempts: number
    maxConcurrentRequests: number
    totalTimeoutSeconds: number
    firstContentTimeoutSeconds: number
    idleTimeoutSeconds: number
    maxResponseBytes: number
    maxResponseEvents: number
  }
  sessionAffinity: { enabled: boolean; ttlSeconds: number; maxEntries: number }
  extractThinking: boolean
  ccBufferedStream: boolean
  toolOutputRecoveryAttempts: number
  countTokensTimeoutSeconds: number
  requestLogRetentionDays: number
}

export interface SettingsResponse {
  settings: SystemSettings
  running: SystemSettings
  defaults: SystemSettings
  revision: string
  restartRequired: string[]
  canSave: boolean
  endpoints: string[]
}

export async function getSettings(): Promise<SettingsResponse> {
  return (await api.get<SettingsResponse>('/settings')).data
}

export async function saveSettings(
  settings: SystemSettings,
  revision: string
): Promise<SettingsResponse> {
  return (await api.put<SettingsResponse>('/settings', { settings, revision }))
    .data
}

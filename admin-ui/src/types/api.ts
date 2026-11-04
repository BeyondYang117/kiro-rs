// 凭据状态响应
export interface CredentialsStatusResponse {
  concurrency?: ConcurrencySnapshot
  total: number
  available: number
  currentId: number
  credentials: CredentialStatusItem[]
}

// 单个凭据状态
export interface CredentialStatusItem {
  id: number
  balance?: BalanceResponse | null
  priority: number
  disabled: boolean
  failureCount: number
  isCurrent: boolean
  expiresAt: string | null
  authMethod: string | null
  hasProfileArn: boolean
  email?: string
  refreshTokenHash?: string
  apiKeyHash?: string
  maskedApiKey?: string
  successCount: number
  inFlight?: number
  cooldownRemainingMs?: number
  effectiveConcurrencyLimit?: number
  adaptiveProbeTarget?: number
  lastUsedAt: string | null
  hasProxy: boolean
  proxyUrl?: string
  refreshFailureCount: number
  disabledReason?: string
  endpoint: string
}

export interface ConcurrencySnapshot {
  adaptiveConcurrency?: boolean
  recoveringCredentials?: number
  probingCredentials?: number
  maxInFlight: number
  maxInFlightPerCredential: number
  maxWaiting: number
  waitTimeoutSeconds: number
  inFlight: number
  waiting: number
  waitingOpus?: number
  coolingCredentials: number
  saturatedCredentials: number
  oldestWaitMs: number
  admittedTotal: number
  queuedTotal: number
  queueFullTotal: number
  waitTimeoutTotal: number
  cancelledWaitTotal: number
  rateLimitedTotal: number
  adaptiveReductionTotal?: number
  finishedWaitTotal: number
  totalWaitMs: number
  maxWaitMs: number
}

// 余额响应
export interface BalanceResponse {
  id: number
  subscriptionTitle: string | null
  currentUsage: number
  usageLimit: number
  remaining: number
  usagePercentage: number
  nextResetAt: number | null
  updatedAt: number | null
}

// 成功响应
export interface SuccessResponse {
  success: boolean
  message: string
}

// 错误响应
export interface AdminErrorResponse {
  error: {
    type: string
    message: string
  }
}

// 请求类型
export interface SetDisabledRequest {
  disabled: boolean
}

export interface SetPriorityRequest {
  priority: number
}

// 添加凭据请求
export interface AddCredentialRequest {
  refreshToken?: string
  authMethod?: 'social' | 'idc' | 'api_key'
  clientId?: string
  clientSecret?: string
  priority?: number
  authRegion?: string
  apiRegion?: string
  machineId?: string
  proxyUrl?: string
  proxyUsername?: string
  proxyPassword?: string
  kiroApiKey?: string
  endpoint?: string
}

// 添加凭据响应
export interface AddCredentialResponse {
  success: boolean
  message: string
  credentialId: number
  email?: string
}

import { api } from './credentials'

export interface DebugJson {
  originalBytes: number
  truncated: boolean
  json: unknown | null
  omittedReason?: string
}

export interface UpstreamResponseDiagnostics {
  outcome: string
  contentType: string | null
  contentLength: number | null
  bodyBytes: number
  chunks: number
  frames: number
  eventTypes: Record<string, number>
  eventFields: Record<string, string[]>
  unknownEvents: number
  textBytes: number
  nonWhitespaceTextBytes: number
  reasoningBytes: number
  redactedReasoningBytes: number
  toolInputBytes: number
  incompleteFrame: boolean | null
  bufferedBytes?: number | null
  pendingToolCalls: number
  thinkingEnabled: boolean
  content?: string
  contentTruncated?: boolean
  stopReason?: string | null
  refusalCategory?: string | null
  refusalExplanation?: string | null
}

export interface UpstreamRequestDiagnostics {
  thinkingTransport: 'native' | 'legacy'
  requestDebugEnabled: boolean
  bodyBytes: number
  modelControls: Record<string, string | number | null>
  legacyControls: Record<string, string | number | null>
  historyMessages: number
  historyUserMessages: number
  historyAssistantMessages: number
  historyReasoningMessages: number
  historySignedReasoningMessages: number
  historyRedactedReasoningMessages: number
  historyTextBytes: number
  recentHistory: unknown[]
  historySummaryTruncated: boolean
  currentMessage: unknown
}

export interface LogRecord {
  requestDebug?: DebugJson
  id: string
  timestamp: number
  method: string
  path: string
  model: string | null
  stream: boolean
  status: 'running' | 'success' | 'error' | 'interrupted'
  httpStatus: number
  durationMs: number
  firstByteMs: number | null
  affinity?: 'hit' | 'miss' | 'fallback'
  inputTokens: number | null
  outputTokens: number | null
  kiroCredits?: number | null
  response?: {
    content: unknown[]
    stopReason: string | null
    truncated: boolean
  }
  credentialId: number | null
  attempts: number
  error: string | null
  upstreamAttempts?: {
    timestamp: number
    credentialId: number
    endpoint: string
    status: number | null
    error: string | null
    upstreamRequestId: string | null
    responseHeadersMs?: number
    firstByteMs?: number
    durationMs?: number
    bodyBytes?: number
    bodyComplete?: boolean
    kiroCredits?: number | null
    responseDiagnostics?: UpstreamResponseDiagnostics
    requestDebug?: DebugJson
    requestDiagnostics?: UpstreamRequestDiagnostics
  }[]
  diagnostics?: { timestamp: number; level: string; target: string; message: string }[]
}

export interface LogFilters {
  search?: string
  model?: string
  status?: string
  credentialId?: string
  httpStatus?: string
  from?: number
  to?: number
}

export interface LogResult {
  items: LogRecord[]
  page: number
  pageSize: number
  retentionDays: number
  maxRows: number
  summary: {
    total: number
    success: number
    error: number
    interrupted: number
    running: number
    inputTokens: number
    outputTokens: number
    averageDurationMs: number
    kiroCredits?: number | null
    meteredRequests?: number
  }
}

export async function getLogs(filters: LogFilters, page: number): Promise<LogResult> {
  return (await api.get<LogResult>('/logs', { params: { ...filters, page, pageSize: 20 } })).data
}

export async function getLog(id: string): Promise<LogRecord> {
  return (await api.get<LogRecord>(`/logs/${encodeURIComponent(id)}`)).data
}

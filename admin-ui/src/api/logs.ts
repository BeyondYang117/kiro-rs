import { api } from './credentials'

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
  incompleteFrame: boolean
  pendingToolCalls: number
  thinkingEnabled: boolean
}

export interface LogRecord {
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
  inputTokens: number | null
  outputTokens: number | null
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
    responseDiagnostics?: UpstreamResponseDiagnostics
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
  }
}

export async function getLogs(filters: LogFilters, page: number): Promise<LogResult> {
  return (await api.get<LogResult>('/logs', { params: { ...filters, page, pageSize: 20 } })).data
}

export async function getLog(id: string): Promise<LogRecord> {
  return (await api.get<LogRecord>(`/logs/${encodeURIComponent(id)}`)).data
}

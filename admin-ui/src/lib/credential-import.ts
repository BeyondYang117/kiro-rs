import type { AddCredentialRequest, AddCredentialResponse, BalanceResponse, CredentialsStatusResponse } from '../types/api'

export interface ImportEntry {
  index: number
  request?: AddCredentialRequest
  email?: string
  error?: string
}

export type ImportStatus = 'pending' | 'checking' | 'verifying' | 'verified' | 'imported' | 'duplicate' | 'warning' | 'failed' | 'cancelled'

export interface ImportResult {
  index: number
  status: ImportStatus
  email?: string
  credentialId?: number
  usage?: string
  error?: string
  detail?: string
}

function isObject(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

/** Parse exported credentials without allowing one malformed row to abort the batch. */
export function parseCredentialImport(input: string): ImportEntry[] {
  let parsed: unknown = JSON.parse(input.replace(/^\uFEFF/, '').trim())
  if (isObject(parsed)) {
    if (Array.isArray(parsed.credentials)) parsed = parsed.credentials
    else if (Array.isArray(parsed.accounts)) parsed = parsed.accounts
  }
  const rows = Array.isArray(parsed) ? parsed : [parsed]
  if (!rows.length) throw new Error('没有可导入的凭据')

  return rows.map((row, index) => {
    let email: string | undefined
    try {
      if (!isObject(row)) throw new Error('凭据必须是 JSON 对象')
      // Also accept the nested credentials used by older account-manager exports.
      const data = isObject(row.credentials) ? { ...row, ...row.credentials } : row
      const string = (key: string): string | undefined => {
        const snakeKey = key.replace(/[A-Z]/g, letter => `_${letter.toLowerCase()}`)
        const value = data[key] ?? data[snakeKey]
        if (value == null) return undefined
        if (typeof value !== 'string') throw new Error(`${key} 必须是字符串`)
        return value.trim() || undefined
      }
      email = string('email')
      const kiroApiKey = string('kiroApiKey')
      const refreshToken = string('refreshToken')
      const clientId = string('clientId')
      const clientSecret = string('clientSecret')
      const rawMethod = string('authMethod')?.toLowerCase()
      const method = rawMethod === 'builder-id' || rawMethod === 'iam' ? 'idc'
        : rawMethod === 'apikey' ? 'api_key' : rawMethod
      const authMethod = method || (kiroApiKey ? 'api_key' : clientId && clientSecret ? 'idc' : 'social')
      if (authMethod !== 'social' && authMethod !== 'idc' && authMethod !== 'api_key') {
        throw new Error('authMethod 仅支持 social、idc、api_key（兼容 builder-id / iam / apikey）')
      }
      if (authMethod === 'api_key') {
        if (!kiroApiKey) throw new Error('缺少 kiroApiKey')
      } else {
        if (kiroApiKey) throw new Error('kiroApiKey 与 OAuth 认证方式冲突')
        if (!refreshToken) throw new Error('缺少 refreshToken')
        if (authMethod === 'idc' && (!clientId || !clientSecret)) {
          throw new Error('idc 模式需要同时提供 clientId 和 clientSecret')
        }
        if (Boolean(clientId) !== Boolean(clientSecret)) {
          throw new Error('需要同时提供 clientId 和 clientSecret')
        }
      }
      const priority = data.priority ?? 0
      if (typeof priority !== 'number' || !Number.isInteger(priority) || priority < 0 || priority > 0xffffffff) {
        throw new Error('priority 必须是 0 至 4294967295 的整数')
      }
      if (data.disabled != null && typeof data.disabled !== 'boolean') throw new Error('disabled 必须是布尔值')
      const request: AddCredentialRequest = {
        authMethod, priority, disabled: data.disabled === true, email,
        ...(authMethod === 'api_key' ? { kiroApiKey } : { refreshToken, clientId, clientSecret }),
        authRegion: string('authRegion') || string('region'),
        apiRegion: string('apiRegion'),
        profileArn: string('profileArn'),
        machineId: string('machineId'),
        subscriptionTitle: string('subscriptionTitle'),
        endpoint: string('endpoint'),
        proxyUrl: string('proxyUrl'),
        proxyUsername: string('proxyUsername'),
        proxyPassword: string('proxyPassword'),
      }
      return { index: index + 1, request, email }
    } catch (error) {
      return { index: index + 1, email, error: error instanceof Error ? error.message : '凭据格式错误' }
    }
  })
}

interface ImportDependencies {
  getCredentials: () => Promise<CredentialsStatusResponse>
  addCredential: (request: AddCredentialRequest) => Promise<AddCredentialResponse>
  getBalance: (id: number, force: boolean) => Promise<BalanceResponse>
  hash: (value: string) => Promise<string>
  describeError: (error: unknown) => { title: string; detail?: string }
}

/** An added credential remains saved even when its balance endpoint rejects a request. */
export async function runCredentialImport(
  entries: ImportEntry[],
  previous: ImportResult[],
  verify: boolean,
  dependencies: ImportDependencies,
  onResult: (result: ImportResult) => void,
  isCancelled: () => boolean,
): Promise<void> {
  const existing = await dependencies.getCredentials()
  const oauthHashes = new Set(existing.credentials.flatMap(c => c.refreshTokenHash ? [c.refreshTokenHash] : []))
  const apiHashes = new Set(existing.credentials.flatMap(c => c.apiKeyHash ? [c.apiKeyHash] : []))
  for (const entry of entries) {
    const old = previous.find(result => result.index === entry.index)
    const result: ImportResult = { index: entry.index, email: entry.email, credentialId: old?.credentialId, status: 'checking' }
    if (isCancelled()) {
      // Preserve saved-but-unverified accounts when stopping a retry.
      onResult(old?.credentialId ? { ...old, status: 'warning' } : { ...result, status: 'cancelled' })
      continue
    }
    onResult(result)
    let saved = old?.credentialId !== undefined
    try {
      if (!entry.request) throw new Error(entry.error || '凭据格式错误')
      if (!saved) {
        const isApiKey = entry.request.authMethod === 'api_key'
        const hash = await dependencies.hash((isApiKey ? entry.request.kiroApiKey : entry.request.refreshToken)!)
        const hashes = isApiKey ? apiHashes : oauthHashes
        if (hashes.has(hash)) {
          onResult({ ...result, status: 'duplicate' })
          continue
        }
        if (isCancelled()) {
          onResult({ ...result, status: 'cancelled' })
          continue
        }
        onResult({ ...result, status: 'verifying' })
        const added = await dependencies.addCredential({ ...entry.request, checkBalance: verify })
        result.credentialId = added.credentialId
        result.email = added.email || entry.email
        saved = true
        // Include warnings in deduplication: they have already been persisted.
        hashes.add(hash)
        if (!verify) {
          onResult({ ...result, status: 'imported' })
          continue
        }
        if (added.balanceError) {
          const error = dependencies.describeError({ response: { data: { error: { message: added.balanceError } } } })
          onResult({ ...result, status: 'warning', error: error.title, detail: error.detail })
          continue
        }
        if (added.balance) {
          onResult({ ...result, status: 'verified', usage: `${added.balance.currentUsage}/${added.balance.usageLimit}` })
          continue
        }
        // Compatibility with older servers that do not return balance in POST responses.
      }
      onResult({ ...result, status: 'verifying' })
      const balance = await dependencies.getBalance(result.credentialId!, true)
      onResult({ ...result, status: 'verified', usage: `${balance.currentUsage}/${balance.usageLimit}` })
    } catch (error) {
      const parsed = dependencies.describeError(error)
      onResult({ ...result, status: saved ? 'warning' : 'failed', error: parsed.title, detail: parsed.detail })
    }
  }
}

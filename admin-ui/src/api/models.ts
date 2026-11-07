import { api } from './credentials'

export interface AvailableModel {
  modelId: string
  modelName: string
  provider: string | null
  rateMultiplier: number | null
  rateUnit: string | null
  maxInputTokens: number | null
  nativeMaxTokens: boolean | null
  minOutputTokens: number | null
  maxOutputTokens: number | null
  effortLevels: string[]
  effortSchemaPath: string | null
  defaultEffort: string | null
  thinkingTypes: string[]
  defaultThinking: string | null
  thinkingDisplays: string[]
  proxySupported: boolean
}

export interface ModelCatalog {
  credentialId: number
  models: AvailableModel[]
  defaultModelId: string | null
  region: string
  updatedAt: string
  cached: boolean
  stale: boolean
  refreshError: string | null
}

export interface ModelTestResult {
  credentialId: number
  modelId: string
  success: boolean
  durationMs: number
  testedAt: string
  response: string | null
  error: string | null
}

export async function testModel(id: number, modelId: string): Promise<ModelTestResult> {
  return (
    await api.post<ModelTestResult>(`/credentials/${id}/models/test`, { modelId }, {
      timeout: 70000,
    })
  ).data
}

export async function getAvailableModels(
  id: number,
  force = false
): Promise<ModelCatalog> {
  return (
    await api.get<ModelCatalog>(`/credentials/${id}/models`, {
      params: { force },
      timeout: 120000,
    })
  ).data
}

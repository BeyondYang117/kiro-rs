import { api } from './credentials'

export interface ClientKey {
  id: string
  maskedKey: string
  legacy: boolean
  configured: boolean
  active: boolean
}

export interface ClientKeysResponse {
  keys: ClientKey[]
  revision: string
  canSave: boolean
  restartRequired: boolean
}

export async function getClientKeys(): Promise<ClientKeysResponse> {
  return (await api.get<ClientKeysResponse>('/client-keys')).data
}

export async function addClientKey(
  revision: string,
  key?: string
): Promise<{
  key: string
  state: ClientKeysResponse
}> {
  return (await api.post('/client-keys', { revision, key })).data
}

export async function deleteClientKey(
  id: string,
  revision: string
): Promise<ClientKeysResponse> {
  return (
    await api.delete<ClientKeysResponse>(`/client-keys/${id}`, {
      data: { revision },
    })
  ).data
}

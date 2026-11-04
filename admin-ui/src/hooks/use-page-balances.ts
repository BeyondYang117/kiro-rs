import { useCallback, useEffect, useRef, useState } from 'react'
import { useQueryClient } from '@tanstack/react-query'
import { getCredentialBalance } from '@/api/credentials'
import { extractErrorMessage } from '@/lib/utils'
import type { BalanceResponse, CredentialStatusItem } from '@/types/api'

const BALANCE_STALE_MS = 5 * 60 * 1000

function credentialKey(credential: CredentialStatusItem) {
  return `${credential.id}:${credential.apiKeyHash || credential.refreshTokenHash || ''}`
}

export function usePageBalances(credentials: CredentialStatusItem[], paused: boolean) {
  const queryClient = useQueryClient()
  const [loadingIds, setLoadingIds] = useState<Set<number>>(new Set())
  const [errors, setErrors] = useState<Map<number, string>>(new Map())
  const current = useRef({ credentials, paused })
  const mounted = useRef(false)
  const running = useRef(false)
  const requests = useRef(new Map<number, Promise<BalanceResponse>>())
  const lastAttempt = useRef(new Map<string, number>())
  current.current = { credentials, paused }

  const refreshBalance = useCallback((id: number, force = false): Promise<BalanceResponse> => {
    const pending = requests.current.get(id)
    if (pending) return pending
    const credential = current.current.credentials.find((item) => item.id === id)
    if (credential) lastAttempt.current.set(credentialKey(credential), Date.now())

    const request = (async () => {
      setLoadingIds((previous) => new Set(previous).add(id))
      setErrors((previous) => {
        const next = new Map(previous)
        next.delete(id)
        return next
      })
      try {
        const balance = await getCredentialBalance(id, force)
        if (mounted.current) {
          // 列表始终使用服务端校验过账号身份的缓存，避免按 ID 复用旧账号数据。
          await queryClient.invalidateQueries({ queryKey: ['credentials'] })
        }
        return balance
      } catch (error) {
        if (mounted.current) {
          setErrors((previous) => new Map(previous).set(id, extractErrorMessage(error)))
        }
        throw error
      } finally {
        requests.current.delete(id)
        if (mounted.current) {
          setLoadingIds((previous) => {
            const next = new Set(previous)
            next.delete(id)
            return next
          })
        }
      }
    })()
    requests.current.set(id, request)
    return request
  }, [queryClient])

  const refreshStale = useCallback(async () => {
    if (running.current || !mounted.current) return
    running.current = true
    try {
      // 每次取最新页面，翻页或切换筛选后不再继续查询旧页。
      while (mounted.current && !current.current.paused && document.visibilityState === 'visible') {
        const now = Date.now()
        const credential = current.current.credentials.find((item) => {
          return !item.disabled && !requests.current.has(item.id)
            && (!item.balance?.updatedAt || now - item.balance.updatedAt * 1000 >= BALANCE_STALE_MS)
            && now - (lastAttempt.current.get(credentialKey(item)) || 0) >= BALANCE_STALE_MS
        })
        if (!credential) break
        try {
          await refreshBalance(credential.id)
        } catch {
          // 每行显示错误，保留上次成功查询的数据。
        }
      }
    } finally {
      running.current = false
    }
  }, [refreshBalance])

  useEffect(() => {
    mounted.current = true
    const interval = window.setInterval(() => void refreshStale(), 30_000)
    const onVisible = () => void refreshStale()
    document.addEventListener('visibilitychange', onVisible)
    return () => {
      mounted.current = false
      window.clearInterval(interval)
      document.removeEventListener('visibilitychange', onVisible)
    }
  }, [refreshStale])

  useEffect(() => {
    void refreshStale()
  }, [credentials, paused, refreshStale])

  return { loadingIds, errors, refreshBalance }
}

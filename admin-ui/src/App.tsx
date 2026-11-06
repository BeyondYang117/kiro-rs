import { useState, useEffect, useRef, useCallback } from 'react'
import { storage } from '@/lib/storage'
import { LoginPage } from '@/components/login-page'
import { Dashboard } from '@/components/dashboard'
import { Toaster } from '@/components/ui/sonner'
import { RequestLogs } from '@/components/request-logs'
import { SystemSettingsPage } from '@/components/system-settings'
import { useQueryClient } from '@tanstack/react-query'

type Page = 'credentials' | 'logs' | 'settings'
const pageFromHash = (): Page =>
  window.location.hash === '#settings'
    ? 'settings'
    : window.location.hash === '#logs'
      ? 'logs'
      : 'credentials'

function App() {
  const [isLoggedIn, setIsLoggedIn] = useState(false)
  const [page, setPage] = useState<Page>(pageFromHash)
  const pageRef = useRef(page)
  const navigationGuard = useRef<(() => boolean) | null>(null)
  const registerNavigationGuard = useCallback(
    (guard: (() => boolean) | null) => {
      navigationGuard.current = guard
    },
    []
  )
  const queryClient = useQueryClient()

  useEffect(() => {
    const syncPage = () => {
      const next = pageFromHash()
      if (next === pageRef.current) return
      if (navigationGuard.current && !navigationGuard.current()) {
        window.history.replaceState(
          null,
          '',
          `${window.location.pathname}${window.location.search}#settings`
        )
        return
      }
      pageRef.current = next
      setPage(next)
    }
    window.addEventListener('hashchange', syncPage)
    return () => window.removeEventListener('hashchange', syncPage)
  }, [])

  const navigate = (next: Page) => {
    if (
      next !== pageRef.current &&
      navigationGuard.current &&
      !navigationGuard.current()
    )
      return false
    pageRef.current = next
    setPage(next)
    window.location.hash = next === 'credentials' ? '' : next
    return true
  }

  useEffect(() => {
    // 检查是否已经有保存的 API Key
    if (storage.getApiKey()) {
      setIsLoggedIn(true)
    }
  }, [])

  const handleLogin = () => {
    setIsLoggedIn(true)
  }

  const handleLogout = () => {
    if (!navigate('credentials')) return
    storage.removeApiKey()
    queryClient.clear()
    setIsLoggedIn(false)
  }

  return (
    <>
      {isLoggedIn ? (
        page === 'settings' ? (
          <SystemSettingsPage
            onBack={() => navigate('credentials')}
            onLogout={handleLogout}
            registerNavigationGuard={registerNavigationGuard}
          />
        ) : page === 'logs' ? (
          <RequestLogs
            onBack={() => navigate('credentials')}
            onLogout={handleLogout}
          />
        ) : (
          <Dashboard
            onLogout={handleLogout}
            onShowLogs={() => navigate('logs')}
            onShowSettings={() => navigate('settings')}
          />
        )
      ) : (
        <LoginPage onLogin={handleLogin} />
      )}
      <Toaster position="top-right" />
    </>
  )
}

export default App

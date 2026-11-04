import { useState, useEffect } from 'react'
import { storage } from '@/lib/storage'
import { LoginPage } from '@/components/login-page'
import { Dashboard } from '@/components/dashboard'
import { Toaster } from '@/components/ui/sonner'
import { RequestLogs } from '@/components/request-logs'
import { useQueryClient } from '@tanstack/react-query'

function App() {
  const [isLoggedIn, setIsLoggedIn] = useState(false)
  const [page, setPage] = useState<'credentials' | 'logs'>(() => window.location.hash === '#logs' ? 'logs' : 'credentials')
  const queryClient = useQueryClient()

  useEffect(() => {
    const syncPage = () => setPage(window.location.hash === '#logs' ? 'logs' : 'credentials')
    window.addEventListener('hashchange', syncPage)
    return () => window.removeEventListener('hashchange', syncPage)
  }, [])

  const navigate = (next: 'credentials' | 'logs') => {
    setPage(next)
    window.location.hash = next === 'logs' ? 'logs' : ''
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
    storage.removeApiKey()
    queryClient.clear()
    navigate('credentials')
    setIsLoggedIn(false)
  }

  return (
    <>
      {isLoggedIn ? (
        page === 'logs'
          ? <RequestLogs onBack={() => navigate('credentials')} onLogout={handleLogout} />
          : <Dashboard onLogout={handleLogout} onShowLogs={() => navigate('logs')} />
      ) : (
        <LoginPage onLogin={handleLogin} />
      )}
      <Toaster position="top-right" />
    </>
  )
}

export default App

import { useState, useEffect, useRef, useMemo, useCallback } from 'react'
import {
  RefreshCw,
  LogOut,
  Moon,
  Sun,
  Server,
  Plus,
  Upload,
  FileUp,
  Trash2,
  RotateCcw,
  CheckCircle2,
  Search,
  X,
  MoreHorizontal,
  ChevronLeft,
  ChevronRight,
  ChevronDown,
} from 'lucide-react'
import { useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import { storage } from '@/lib/storage'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Input } from '@/components/ui/input'
import { Checkbox } from '@/components/ui/checkbox'
import {
  DropdownMenu,
  DropdownMenuTrigger,
  DropdownMenuContent,
  DropdownMenuItem,
} from '@/components/ui/dropdown-menu'
import { CredentialCard } from '@/components/credential-card'
import { ConcurrencyPanel } from '@/components/concurrency-panel'
import { BalanceDialog } from '@/components/balance-dialog'
import { AddCredentialDialog } from '@/components/add-credential-dialog'
import { BatchImportDialog } from '@/components/batch-import-dialog'
import { KamImportDialog } from '@/components/kam-import-dialog'
import { BatchVerifyDialog, type VerifyResult } from '@/components/batch-verify-dialog'
import {
  useCredentials,
  useDeleteCredential,
  useResetFailure,
  useLoadBalancingMode,
  useSetLoadBalancingMode,
} from '@/hooks/use-credentials'
import { getCredentialBalance, forceRefreshToken } from '@/api/credentials'
import { extractErrorMessage } from '@/lib/utils'
import type { BalanceResponse } from '@/types/api'

interface DashboardProps {
  onLogout: () => void
  onShowLogs: () => void
}

export function Dashboard({ onLogout, onShowLogs }: DashboardProps) {
  const [selectedCredentialId, setSelectedCredentialId] = useState<number | null>(null)
  const [balanceDialogOpen, setBalanceDialogOpen] = useState(false)
  const [addDialogOpen, setAddDialogOpen] = useState(false)
  const [batchImportDialogOpen, setBatchImportDialogOpen] = useState(false)
  const [kamImportDialogOpen, setKamImportDialogOpen] = useState(false)
  const [selectedIds, setSelectedIds] = useState<Set<number>>(new Set())
  const [verifyDialogOpen, setVerifyDialogOpen] = useState(false)
  const [verifying, setVerifying] = useState(false)
  const [verifyProgress, setVerifyProgress] = useState({ current: 0, total: 0 })
  const [verifyResults, setVerifyResults] = useState<Map<number, VerifyResult>>(new Map())
  const [balanceMap, setBalanceMap] = useState<Map<number, BalanceResponse>>(new Map())
  const [loadingBalanceIds, setLoadingBalanceIds] = useState<Set<number>>(new Set())
  const [queryingInfo, setQueryingInfo] = useState(false)
  const [queryInfoProgress, setQueryInfoProgress] = useState({ current: 0, total: 0 })
  const [batchRefreshing, setBatchRefreshing] = useState(false)
  const [batchRefreshProgress, setBatchRefreshProgress] = useState({ current: 0, total: 0 })
  const cancelVerifyRef = useRef(false)
  const [currentPage, setCurrentPage] = useState(1)
  const [itemsPerPage, setItemsPerPage] = useState(20)
  const [search, setSearch] = useState('')
  const [statusFilter, setStatusFilter] = useState('all')
  const [authFilter, setAuthFilter] = useState('all')
  const [sortBy, setSortBy] = useState('default')
  const [batchMutating, setBatchMutating] = useState(false)
  const [darkMode, setDarkMode] = useState(() => {
    if (typeof window !== 'undefined') {
      return document.documentElement.classList.contains('dark')
    }
    return false
  })

  const queryClient = useQueryClient()
  const { data, isLoading, isFetching, error, refetch } = useCredentials()
  const { mutate: deleteCredential } = useDeleteCredential()
  const { mutate: resetFailure } = useResetFailure()
  const { data: loadBalancingData, isLoading: isLoadingMode } = useLoadBalancingMode()
  const { mutate: setLoadBalancingMode, isPending: isSettingMode } = useSetLoadBalancingMode()

  const filteredCredentials = useMemo(() => {
    const keyword = search.trim().toLowerCase()
    const credentials = (data?.credentials || []).filter((credential) => {
      const hasFailures = credential.failureCount > 0 || credential.refreshFailureCount > 0
      if (statusFilter === 'enabled' && credential.disabled) return false
      if (statusFilter === 'disabled' && !credential.disabled) return false
      if (statusFilter === 'failed' && !hasFailures) return false
      if (authFilter !== 'all' && credential.authMethod !== authFilter) return false
      return (
        !keyword ||
        [
          String(credential.id),
          `#${credential.id}`,
          credential.email,
          credential.endpoint,
          credential.maskedApiKey,
        ].some((value) => value?.toLowerCase().includes(keyword))
      )
    })
    return credentials.sort((a, b) => {
      if (sortBy === 'priority') return a.priority - b.priority || a.id - b.id
      if (sortBy === 'recent')
        return (
          (Date.parse(b.lastUsedAt || '') || 0) - (Date.parse(a.lastUsedAt || '') || 0) ||
          a.id - b.id
        )
      if (sortBy === 'success') return b.successCount - a.successCount || a.id - b.id
      if (sortBy === 'failures')
        return (
          b.failureCount + b.refreshFailureCount - (a.failureCount + a.refreshFailureCount) ||
          a.id - b.id
        )
      return (
        Number(b.isCurrent) - Number(a.isCurrent) ||
        Number(a.disabled) - Number(b.disabled) ||
        a.priority - b.priority ||
        a.id - b.id
      )
    })
  }, [data?.credentials, search, statusFilter, authFilter, sortBy])
  const totalPages = Math.max(1, Math.ceil(filteredCredentials.length / itemsPerPage))
  const page = Math.min(currentPage, totalPages)
  const startIndex = (page - 1) * itemsPerPage
  const currentCredentials = filteredCredentials.slice(startIndex, startIndex + itemsPerPage)
  const disabledCredentialCount =
    data?.credentials.filter((credential) => credential.disabled).length || 0
  const selectedCredentials =
    data?.credentials.filter((credential) => selectedIds.has(credential.id)) || []
  const selectedDisabledCount = selectedCredentials.filter(
    (credential) => credential.disabled
  ).length
  const selectedFailedCount = selectedCredentials.filter(
    (credential) => credential.failureCount > 0 || credential.refreshFailureCount > 0
  ).length
  const selectedRefreshableCount = selectedCredentials.filter(
    (credential) => !credential.disabled && credential.authMethod !== 'api_key'
  ).length
  const selectedOnPage = currentCredentials.filter((credential) =>
    selectedIds.has(credential.id)
  ).length
  const batchBusy = batchMutating || verifying || batchRefreshing
  const hasFilters = Boolean(search || statusFilter !== 'all' || authFilter !== 'all')

  useEffect(() => {
    setCurrentPage(1)
  }, [search, statusFilter, authFilter, sortBy, itemsPerPage])

  useEffect(() => {
    setCurrentPage((previous) => Math.min(previous, totalPages))
  }, [totalPages])

  const resetFilters = () => {
    setSearch('')
    setStatusFilter('all')
    setAuthFilter('all')
    setCurrentPage(1)
  }

  const toggleCurrentPage = () => {
    setSelectedIds((previous) => {
      const next = new Set(previous)
      const allSelected = currentCredentials.every((credential) => next.has(credential.id))
      currentCredentials.forEach((credential) =>
        allSelected ? next.delete(credential.id) : next.add(credential.id)
      )
      return next
    })
  }

  // 只保留当前仍存在的凭据缓存，避免删除后残留旧数据
  useEffect(() => {
    if (!data?.credentials) {
      setBalanceMap(new Map())
      setLoadingBalanceIds(new Set())
      return
    }

    const validIds = new Set(data.credentials.map((credential) => credential.id))

    setSelectedIds((previous) => {
      const next = new Set([...previous].filter((id) => validIds.has(id)))
      return next.size === previous.size ? previous : next
    })

    setBalanceMap((prev) => {
      const next = new Map<number, BalanceResponse>()
      prev.forEach((value, id) => {
        if (validIds.has(id)) {
          next.set(id, value)
        }
      })
      return next.size === prev.size ? prev : next
    })

    setLoadingBalanceIds((prev) => {
      if (prev.size === 0) {
        return prev
      }
      const next = new Set<number>()
      prev.forEach((id) => {
        if (validIds.has(id)) {
          next.add(id)
        }
      })
      return next.size === prev.size ? prev : next
    })
  }, [data?.credentials])

  const toggleDarkMode = () => {
    setDarkMode(!darkMode)
    document.documentElement.classList.toggle('dark')
  }

  const handleViewBalance = (id: number) => {
    setSelectedCredentialId(id)
    setBalanceDialogOpen(true)
  }

  const handleBalanceLoaded = useCallback((balance: BalanceResponse) => {
    setBalanceMap((previous) => new Map(previous).set(balance.id, balance))
  }, [])

  const handleRefresh = async () => {
    const result = await refetch()
    if (result.error) toast.error(`刷新失败: ${extractErrorMessage(result.error)}`)
    else toast.success('已刷新凭据列表')
  }

  const handleLogout = () => {
    storage.removeApiKey()
    queryClient.clear()
    onLogout()
  }

  // 选择管理
  const toggleSelect = (id: number) => {
    setSelectedIds((previous) => {
      const next = new Set(previous)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })
  }

  const deselectAll = () => {
    setSelectedIds(new Set())
  }

  // 批量删除（仅删除已禁用项）
  const handleBatchDelete = async () => {
    if (selectedIds.size === 0) {
      toast.error('请先选择要删除的凭据')
      return
    }

    const disabledIds = Array.from(selectedIds).filter((id) => {
      const credential = data?.credentials.find((c) => c.id === id)
      return Boolean(credential?.disabled)
    })

    if (disabledIds.length === 0) {
      toast.error('选中的凭据中没有已禁用项')
      return
    }

    const skippedCount = selectedIds.size - disabledIds.length
    const skippedText = skippedCount > 0 ? `（将跳过 ${skippedCount} 个未禁用凭据）` : ''

    if (
      !confirm(`确定要删除 ${disabledIds.length} 个已禁用凭据吗？此操作无法撤销。${skippedText}`)
    ) {
      return
    }

    setBatchMutating(true)
    let successCount = 0
    let failCount = 0

    for (const id of disabledIds) {
      try {
        await new Promise<void>((resolve, reject) => {
          deleteCredential(id, {
            onSuccess: () => {
              successCount++
              resolve()
            },
            onError: (err) => {
              failCount++
              reject(err)
            },
          })
        })
      } catch (error) {
        // 错误已在 onError 中处理
      }
    }

    const skippedResultText = skippedCount > 0 ? `，已跳过 ${skippedCount} 个未禁用凭据` : ''

    if (failCount === 0) {
      toast.success(`成功删除 ${successCount} 个已禁用凭据${skippedResultText}`)
    } else {
      toast.warning(
        `删除已禁用凭据：成功 ${successCount} 个，失败 ${failCount} 个${skippedResultText}`
      )
    }

    setBatchMutating(false)
    deselectAll()
  }

  // 批量恢复异常
  const handleBatchResetFailure = async () => {
    if (selectedIds.size === 0) {
      toast.error('请先选择要恢复的凭据')
      return
    }

    const failedIds = Array.from(selectedIds).filter((id) => {
      const cred = data?.credentials.find((c) => c.id === id)
      return cred && (cred.failureCount > 0 || cred.refreshFailureCount > 0)
    })

    if (failedIds.length === 0) {
      toast.error('选中的凭据中没有失败的凭据')
      return
    }

    setBatchMutating(true)
    let successCount = 0
    let failCount = 0

    for (const id of failedIds) {
      try {
        await new Promise<void>((resolve, reject) => {
          resetFailure(id, {
            onSuccess: () => {
              successCount++
              resolve()
            },
            onError: (err) => {
              failCount++
              reject(err)
            },
          })
        })
      } catch (error) {
        // 错误已在 onError 中处理
      }
    }

    if (failCount === 0) {
      toast.success(`成功恢复 ${successCount} 个凭据`)
    } else {
      toast.warning(`成功 ${successCount} 个，失败 ${failCount} 个`)
    }

    setBatchMutating(false)
    deselectAll()
  }

  // 批量刷新 Token
  const handleBatchForceRefresh = async () => {
    if (selectedIds.size === 0) {
      toast.error('请先选择要刷新的凭据')
      return
    }

    const enabledIds = Array.from(selectedIds).filter((id) => {
      const cred = data?.credentials.find((c) => c.id === id)
      return cred && !cred.disabled && cred.authMethod !== 'api_key'
    })

    if (enabledIds.length === 0) {
      toast.error('选中的凭据中没有可刷新 Token 的凭据')
      return
    }

    setBatchRefreshing(true)
    setBatchRefreshProgress({ current: 0, total: enabledIds.length })

    let successCount = 0
    let failCount = 0

    for (let i = 0; i < enabledIds.length; i++) {
      try {
        await forceRefreshToken(enabledIds[i])
        successCount++
      } catch {
        failCount++
      }
      setBatchRefreshProgress({ current: i + 1, total: enabledIds.length })
    }

    setBatchRefreshing(false)
    queryClient.invalidateQueries({ queryKey: ['credentials'] })

    if (failCount === 0) {
      toast.success(`成功刷新 ${successCount} 个凭据的 Token`)
    } else {
      toast.warning(`刷新 Token：成功 ${successCount} 个，失败 ${failCount} 个`)
    }

    deselectAll()
  }

  // 一键清除所有已禁用凭据
  const handleClearAll = async () => {
    if (!data?.credentials || data.credentials.length === 0) {
      toast.error('没有可清除的凭据')
      return
    }

    const disabledCredentials = data.credentials.filter((credential) => credential.disabled)

    if (disabledCredentials.length === 0) {
      toast.error('没有可清除的已禁用凭据')
      return
    }

    if (!confirm(`确定要清除所有 ${disabledCredentials.length} 个已禁用凭据吗？此操作无法撤销。`)) {
      return
    }

    setBatchMutating(true)
    let successCount = 0
    let failCount = 0

    for (const credential of disabledCredentials) {
      try {
        await new Promise<void>((resolve, reject) => {
          deleteCredential(credential.id, {
            onSuccess: () => {
              successCount++
              resolve()
            },
            onError: (err) => {
              failCount++
              reject(err)
            },
          })
        })
      } catch (error) {
        // 错误已在 onError 中处理
      }
    }

    if (failCount === 0) {
      toast.success(`成功清除所有 ${successCount} 个已禁用凭据`)
    } else {
      toast.warning(`清除已禁用凭据：成功 ${successCount} 个，失败 ${failCount} 个`)
    }

    setBatchMutating(false)
    deselectAll()
  }

  // 查询当前页凭据信息（逐个查询，避免瞬时并发）
  const handleQueryCurrentPageInfo = async () => {
    if (currentCredentials.length === 0) {
      toast.error('当前页没有可查询的凭据')
      return
    }

    const ids = currentCredentials
      .filter((credential) => !credential.disabled)
      .map((credential) => credential.id)

    if (ids.length === 0) {
      toast.error('当前页没有可查询的启用凭据')
      return
    }

    setQueryingInfo(true)
    setQueryInfoProgress({ current: 0, total: ids.length })

    let successCount = 0
    let failCount = 0

    for (let i = 0; i < ids.length; i++) {
      const id = ids[i]

      setLoadingBalanceIds((prev) => {
        const next = new Set(prev)
        next.add(id)
        return next
      })

      try {
        const balance = await getCredentialBalance(id)
        successCount++

        setBalanceMap((prev) => {
          const next = new Map(prev)
          next.set(id, balance)
          return next
        })
      } catch (error) {
        failCount++
      } finally {
        setLoadingBalanceIds((prev) => {
          const next = new Set(prev)
          next.delete(id)
          return next
        })
      }

      setQueryInfoProgress({ current: i + 1, total: ids.length })
    }

    setQueryingInfo(false)

    if (failCount === 0) {
      toast.success(`查询完成：成功 ${successCount}/${ids.length}`)
    } else {
      toast.warning(`查询完成：成功 ${successCount} 个，失败 ${failCount} 个`)
    }
  }

  // 批量验活
  const handleBatchVerify = async () => {
    if (selectedIds.size === 0) {
      toast.error('请先选择要验活的凭据')
      return
    }

    // 初始化状态
    setVerifying(true)
    cancelVerifyRef.current = false
    const ids = Array.from(selectedIds)
    setVerifyProgress({ current: 0, total: ids.length })

    let successCount = 0

    // 初始化结果，所有凭据状态为 pending
    const initialResults = new Map<number, VerifyResult>()
    ids.forEach((id) => {
      initialResults.set(id, { id, status: 'pending' })
    })
    setVerifyResults(initialResults)
    setVerifyDialogOpen(true)

    // 开始验活
    for (let i = 0; i < ids.length; i++) {
      // 检查是否取消
      if (cancelVerifyRef.current) {
        toast.info('已取消验活')
        break
      }

      const id = ids[i]

      // 更新当前凭据状态为 verifying
      setVerifyResults((prev) => {
        const newResults = new Map(prev)
        newResults.set(id, { id, status: 'verifying' })
        return newResults
      })

      try {
        const balance = await getCredentialBalance(id)
        successCount++
        setBalanceMap((previous) => new Map(previous).set(id, balance))

        // 更新为成功状态
        setVerifyResults((prev) => {
          const newResults = new Map(prev)
          newResults.set(id, {
            id,
            status: 'success',
            usage: `${balance.currentUsage}/${balance.usageLimit}`,
          })
          return newResults
        })
      } catch (error) {
        // 更新为失败状态
        setVerifyResults((prev) => {
          const newResults = new Map(prev)
          newResults.set(id, {
            id,
            status: 'failed',
            error: extractErrorMessage(error),
          })
          return newResults
        })
      }

      // 更新进度
      setVerifyProgress({ current: i + 1, total: ids.length })

      // 添加延迟防止封号（最后一个不需要延迟）
      if (i < ids.length - 1 && !cancelVerifyRef.current) {
        await new Promise((resolve) => setTimeout(resolve, 2000))
      }
    }

    setVerifying(false)

    if (!cancelVerifyRef.current) {
      toast.success(`验活完成：成功 ${successCount}/${ids.length}`)
    }
  }

  // 取消验活
  const handleCancelVerify = () => {
    cancelVerifyRef.current = true
    setVerifying(false)
  }

  // 切换负载均衡模式
  const handleToggleLoadBalancing = () => {
    const currentMode = loadBalancingData?.mode || 'priority'
    const newMode = currentMode === 'priority' ? 'balanced' : 'priority'

    setLoadBalancingMode(newMode, {
      onSuccess: () => {
        const modeName = newMode === 'priority' ? '优先级模式' : '均衡负载模式'
        toast.success(`已切换到${modeName}`)
      },
      onError: (error) => {
        toast.error(`切换失败: ${extractErrorMessage(error)}`)
      },
    })
  }

  if (isLoading) {
    return (
      <div className="min-h-screen flex items-center justify-center bg-background">
        <div className="text-center">
          <div className="animate-spin rounded-full h-12 w-12 border-b-2 border-primary mx-auto mb-4"></div>
          <p className="text-muted-foreground">加载中...</p>
        </div>
      </div>
    )
  }

  if (error) {
    return (
      <div className="min-h-screen flex items-center justify-center bg-background p-4">
        <Card className="w-full max-w-md">
          <CardContent className="pt-6 text-center">
            <div className="text-red-500 mb-4">加载失败</div>
            <p className="text-muted-foreground mb-4">{(error as Error).message}</p>
            <div className="space-x-2">
              <Button onClick={() => refetch()}>重试</Button>
              <Button variant="outline" onClick={onShowLogs}>
                查看调用日志
              </Button>
              <Button variant="outline" onClick={handleLogout}>
                重新登录
              </Button>
            </div>
          </CardContent>
        </Card>
      </div>
    )
  }

  return (
    <div className="min-h-screen bg-background">
      {/* 顶部导航 */}
      <header className="sticky top-0 z-50 w-full border-b bg-background/95 backdrop-blur supports-[backdrop-filter]:bg-background/60">
        <div className="container flex min-h-14 flex-wrap items-center justify-between gap-2 px-4 py-2 md:px-8">
          <div className="flex items-center gap-2">
            <Server className="h-5 w-5" />
            <span className="font-semibold">Kiro Admin</span>
          </div>
          <div className="flex items-center gap-2">
            <Button variant="outline" size="sm" onClick={onShowLogs}>
              调用日志
            </Button>
            <Button
              variant="outline"
              size="sm"
              onClick={handleToggleLoadBalancing}
              disabled={isLoadingMode || isSettingMode}
              title="切换负载均衡模式"
            >
              {isLoadingMode
                ? '加载中...'
                : loadBalancingData?.mode === 'priority'
                  ? '优先级模式'
                  : '均衡负载'}
            </Button>
            <Button variant="ghost" size="icon" aria-label="切换主题" onClick={toggleDarkMode}>
              {darkMode ? <Sun className="h-5 w-5" /> : <Moon className="h-5 w-5" />}
            </Button>
            <Button
              variant="ghost"
              size="icon"
              aria-label="刷新凭据列表"
              onClick={handleRefresh}
              disabled={isFetching}
            >
              <RefreshCw className={`h-5 w-5 ${isFetching ? 'animate-spin' : ''}`} />
            </Button>
            <Button variant="ghost" size="icon" aria-label="退出登录" onClick={handleLogout}>
              <LogOut className="h-5 w-5" />
            </Button>
          </div>
        </div>
      </header>

      {/* 主内容 */}
      <main className="container mx-auto px-4 md:px-8 py-6">
        {/* 统计卡片 */}
        <div className="grid grid-cols-3 gap-2 sm:gap-4 mb-6">
          <Card>
            <CardHeader className="px-3 pt-4 pb-2 sm:px-6">
              <CardTitle className="text-sm font-medium text-muted-foreground">凭据总数</CardTitle>
            </CardHeader>
            <CardContent className="px-3 pb-4 sm:px-6">
              <div className="text-2xl font-bold">{data?.total || 0}</div>
            </CardContent>
          </Card>
          <Card>
            <CardHeader className="px-3 pt-4 pb-2 sm:px-6">
              <CardTitle className="text-sm font-medium text-muted-foreground">已启用凭据</CardTitle>
            </CardHeader>
            <CardContent className="px-3 pb-4 sm:px-6">
              <div className="text-2xl font-bold text-green-600">{data?.available || 0}</div>
            </CardContent>
          </Card>
          <Card>
            <CardHeader className="px-3 pt-4 pb-2 sm:px-6">
              <CardTitle className="text-sm font-medium text-muted-foreground">当前活跃</CardTitle>
            </CardHeader>
            <CardContent className="px-3 pb-4 sm:px-6">
              <div className="text-2xl font-bold flex flex-wrap items-center gap-2">
                {data?.credentials.find((credential) => credential.isCurrent)
                  ? `#${data.currentId}`
                  : '—'}
                {data?.credentials.some((credential) => credential.isCurrent) && (
                  <Badge variant="success">活跃</Badge>
                )}
              </div>
            </CardContent>
          </Card>
        </div>

        {data?.concurrency && <ConcurrencyPanel metrics={data.concurrency} />}

        <section className="space-y-4" aria-label="凭据管理">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div>
              <h1 className="text-xl font-semibold">
                凭据管理{' '}
                <span className="ml-1 text-sm font-normal text-muted-foreground">
                  {data?.total || 0} 个
                </span>
              </h1>
              <p className="mt-1 text-xs text-muted-foreground">集中管理账号、查看用量与运行状态</p>
            </div>
            <div className="flex flex-wrap items-center gap-2">
              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <Button size="sm" variant="outline">
                    <Upload />
                    导入凭据
                    <ChevronDown className="!h-3 !w-3" />
                  </Button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end">
                  <DropdownMenuItem onSelect={() => setBatchImportDialogOpen(true)}>
                    <Upload />
                    批量导入
                  </DropdownMenuItem>
                  <DropdownMenuItem onSelect={() => setKamImportDialogOpen(true)}>
                    <FileUp />
                    Kiro Account Manager 导入
                  </DropdownMenuItem>
                </DropdownMenuContent>
              </DropdownMenu>
              <Button onClick={() => setAddDialogOpen(true)} size="sm">
                <Plus />
                添加凭据
              </Button>
            </div>
          </div>

          <div className="overflow-hidden rounded-lg border bg-card">
            <div className="flex flex-wrap items-center gap-2 border-b p-4">
              <div className="relative w-full sm:w-64 sm:flex-1">
                <Search className="pointer-events-none absolute left-3 top-3 h-4 w-4 text-muted-foreground" />
                <Input
                  aria-label="搜索凭据"
                  placeholder="搜索邮箱、ID、端点或密钥片段"
                  value={search}
                  onChange={(event) => setSearch(event.target.value)}
                  className="pl-9 pr-9"
                />
                {search && (
                  <Button
                    size="icon"
                    variant="ghost"
                    className="absolute right-1 top-1 h-8 w-8"
                    aria-label="清空搜索"
                    onClick={() => setSearch('')}
                  >
                    <X />
                  </Button>
                )}
              </div>
              <select
                aria-label="凭据状态"
                className="h-10 min-w-0 flex-1 rounded-md border bg-background px-3 text-sm sm:flex-none"
                value={statusFilter}
                onChange={(event) => setStatusFilter(event.target.value)}
              >
                <option value="all">全部状态</option>
                <option value="enabled">已启用</option>
                <option value="disabled">已禁用</option>
                <option value="failed">有失败记录</option>
              </select>
              <select
                aria-label="认证类型"
                className="h-10 min-w-0 flex-1 rounded-md border bg-background px-3 text-sm sm:flex-none"
                value={authFilter}
                onChange={(event) => setAuthFilter(event.target.value)}
              >
                <option value="all">全部类型</option>
                <option value="social">Social</option>
                <option value="idc">IdC</option>
                <option value="api_key">API Key</option>
              </select>
              <select
                aria-label="凭据排序"
                className="h-10 min-w-0 flex-1 rounded-md border bg-background px-3 text-sm sm:flex-none"
                value={sortBy}
                onChange={(event) => setSortBy(event.target.value)}
              >
                <option value="default">默认排序</option>
                <option value="priority">优先级从高到低</option>
                <option value="recent">最近调用</option>
                <option value="success">成功次数最多</option>
                <option value="failures">失败次数最多</option>
              </select>
            </div>

            <div className="flex min-h-14 flex-wrap items-center justify-between gap-2 border-b bg-muted/20 px-4 py-2">
              <div className="flex flex-wrap items-center gap-3 text-sm">
                <label className="flex cursor-pointer items-center gap-2">
                  <Checkbox
                    aria-label="全选当前页"
                    checked={
                      selectedOnPage === 0
                        ? false
                        : selectedOnPage === currentCredentials.length
                          ? true
                          : 'indeterminate'
                    }
                    onCheckedChange={toggleCurrentPage}
                    disabled={currentCredentials.length === 0}
                  />
                  <span>本页全选</span>
                </label>
                {selectedIds.size > 0 ? (
                  <>
                    <span className="text-muted-foreground">
                      已选 {selectedIds.size} 个
                      {selectedIds.size > selectedOnPage
                        ? `（含其他页或筛选外 ${selectedIds.size - selectedOnPage} 个）`
                        : ''}
                    </span>
                    <Button onClick={deselectAll} size="sm" variant="ghost" className="h-7 px-2">
                      取消选择
                    </Button>
                  </>
                ) : (
                  <span className="text-xs text-muted-foreground">
                    共 {filteredCredentials.length} 个结果
                  </span>
                )}
              </div>
              <div className="flex flex-wrap items-center gap-2">
                {verifying && !verifyDialogOpen && (
                  <Button onClick={() => setVerifyDialogOpen(true)} size="sm" variant="secondary">
                    <CheckCircle2 className="animate-spin" />
                    验活 {verifyProgress.current}/{verifyProgress.total}
                  </Button>
                )}
                <Button
                  onClick={handleQueryCurrentPageInfo}
                  size="sm"
                  variant="ghost"
                  disabled={
                    queryingInfo || !currentCredentials.some((credential) => !credential.disabled)
                  }
                >
                  <RefreshCw className={queryingInfo ? 'animate-spin' : ''} />
                  {queryingInfo
                    ? `查询中 ${queryInfoProgress.current}/${queryInfoProgress.total}`
                    : '查询本页用量'}
                </Button>
                <DropdownMenu>
                  <DropdownMenuTrigger asChild>
                    <Button size="sm" variant="ghost" aria-label="更多列表操作">
                      <MoreHorizontal />
                    </Button>
                  </DropdownMenuTrigger>
                  <DropdownMenuContent align="end">
                    <DropdownMenuItem
                      onSelect={handleClearAll}
                      disabled={disabledCredentialCount === 0 || batchBusy}
                      className="text-destructive focus:text-destructive"
                    >
                      <Trash2 />
                      清除全部已禁用（{disabledCredentialCount}）
                    </DropdownMenuItem>
                  </DropdownMenuContent>
                </DropdownMenu>
              </div>
            </div>

            {selectedIds.size > 0 && (
              <div className="flex flex-wrap items-center gap-2 border-b bg-muted/40 px-4 py-3">
                <span className="mr-1 text-xs text-muted-foreground">批量操作</span>
                <Button
                  onClick={handleBatchVerify}
                  size="sm"
                  variant="outline"
                  disabled={batchBusy}
                >
                  <CheckCircle2 />
                  验活
                </Button>
                <Button
                  onClick={handleBatchForceRefresh}
                  size="sm"
                  variant="outline"
                  disabled={batchBusy || selectedRefreshableCount === 0}
                >
                  <RefreshCw className={batchRefreshing ? 'animate-spin' : ''} />
                  {batchRefreshing
                    ? `刷新中 ${batchRefreshProgress.current}/${batchRefreshProgress.total}`
                    : '刷新 Token'}
                </Button>
                <Button
                  onClick={handleBatchResetFailure}
                  size="sm"
                  variant="outline"
                  disabled={batchBusy || selectedFailedCount === 0}
                >
                  <RotateCcw />
                  恢复异常
                </Button>
                <Button
                  onClick={handleBatchDelete}
                  size="sm"
                  variant="outline"
                  className="text-destructive hover:text-destructive"
                  disabled={batchBusy || selectedDisabledCount === 0}
                >
                  <Trash2 />
                  删除已禁用（{selectedDisabledCount}）
                </Button>
              </div>
            )}

            <div
              className="credential-grid hidden items-center gap-4 border-b bg-muted/30 px-4 py-3 text-xs font-medium text-muted-foreground lg:grid"
              aria-hidden="true"
            >
              <span>账号 / 认证方式</span>
              <span>启用状态</span>
              <span title="数值越小越优先">优先级</span>
              <span>调用情况</span>
              <span>剩余用量</span>
              <span className="text-right">操作</span>
            </div>
            {currentCredentials.length === 0 ? (
              <div className="px-4 py-16 text-center">
                <Search className="mx-auto mb-3 h-8 w-8 text-muted-foreground/50" />
                <p className="text-sm font-medium">
                  {data?.credentials.length === 0 ? '暂无凭据' : '没有匹配的凭据'}
                </p>
                <p className="mt-2 text-xs text-muted-foreground">
                  {data?.credentials.length === 0
                    ? '添加或导入凭据后，即可在这里管理。'
                    : '尝试其他关键词，或调整筛选条件。'}
                </p>
                {hasFilters && (
                  <Button variant="outline" size="sm" className="mt-4" onClick={resetFilters}>
                    清除筛选
                  </Button>
                )}
              </div>
            ) : (
              <div className="divide-y">
                {currentCredentials.map((credential) => (
                  <CredentialCard
                    key={credential.id}
                    credential={credential}
                    onViewBalance={handleViewBalance}
                    selected={selectedIds.has(credential.id)}
                    onToggleSelect={() => toggleSelect(credential.id)}
                    balance={balanceMap.get(credential.id) || null}
                    loadingBalance={loadingBalanceIds.has(credential.id)}
                  />
                ))}
              </div>
            )}

            <div className="flex flex-wrap items-center justify-between gap-3 border-t px-4 py-3 text-xs text-muted-foreground">
              <div className="flex flex-wrap items-center gap-3">
                <span aria-live="polite">
                  {filteredCredentials.length
                    ? `${startIndex + 1}–${Math.min(startIndex + itemsPerPage, filteredCredentials.length)} / ${filteredCredentials.length} 个`
                    : '0 个结果'}
                </span>
                {hasFilters && (
                  <Button
                    variant="link"
                    size="sm"
                    className="h-auto p-0 text-xs"
                    onClick={resetFilters}
                  >
                    清除筛选
                  </Button>
                )}
              </div>
              <div className="flex items-center gap-3">
                <label className="flex items-center gap-2">
                  每页
                  <select
                    aria-label="每页数量"
                    className="h-8 rounded-md border bg-background px-2 text-foreground"
                    value={itemsPerPage}
                    onChange={(event) => setItemsPerPage(Number(event.target.value))}
                  >
                    <option value={10}>10</option>
                    <option value={20}>20</option>
                    <option value={50}>50</option>
                  </select>
                  条
                </label>
                <div className="flex items-center gap-1">
                  <Button
                    variant="outline"
                    size="icon"
                    className="h-8 w-8"
                    aria-label="上一页"
                    disabled={page === 1}
                    onClick={() => setCurrentPage(page - 1)}
                  >
                    <ChevronLeft />
                  </Button>
                  <span className="min-w-12 text-center tabular-nums">
                    {page} / {totalPages}
                  </span>
                  <Button
                    variant="outline"
                    size="icon"
                    className="h-8 w-8"
                    aria-label="下一页"
                    disabled={page === totalPages}
                    onClick={() => setCurrentPage(page + 1)}
                  >
                    <ChevronRight />
                  </Button>
                </div>
              </div>
            </div>
          </div>
        </section>
      </main>

      {/* 余额对话框 */}
      <BalanceDialog
        credentialId={selectedCredentialId}
        open={balanceDialogOpen}
        onOpenChange={setBalanceDialogOpen}
        onBalanceLoaded={handleBalanceLoaded}
      />

      {/* 添加凭据对话框 */}
      <AddCredentialDialog open={addDialogOpen} onOpenChange={setAddDialogOpen} />

      {/* 批量导入对话框 */}
      <BatchImportDialog open={batchImportDialogOpen} onOpenChange={setBatchImportDialogOpen} />

      {/* KAM 账号导入对话框 */}
      <KamImportDialog open={kamImportDialogOpen} onOpenChange={setKamImportDialogOpen} />

      {/* 批量验活对话框 */}
      <BatchVerifyDialog
        open={verifyDialogOpen}
        onOpenChange={setVerifyDialogOpen}
        verifying={verifying}
        progress={verifyProgress}
        results={verifyResults}
        onCancel={handleCancelVerify}
      />
    </div>
  )
}

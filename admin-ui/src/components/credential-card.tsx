import { useState } from 'react'
import { toast } from 'sonner'
import {
  RefreshCw,
  ChevronDown,
  Wallet,
  Trash2,
  Loader2,
  MoreHorizontal,
  RotateCcw,
  Pencil,
  Boxes,
} from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Switch } from '@/components/ui/switch'
import { Input } from '@/components/ui/input'
import { Checkbox } from '@/components/ui/checkbox'
import { cn } from '@/lib/utils'
import {
  DropdownMenu,
  DropdownMenuTrigger,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
} from '@/components/ui/dropdown-menu'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import type { CredentialStatusItem, BalanceResponse } from '@/types/api'
import { ModelsDialog } from '@/components/models-dialog'
import {
  useSetDisabled,
  useSetPriority,
  useResetFailure,
  useDeleteCredential,
  useForceRefreshToken,
} from '@/hooks/use-credentials'

interface CredentialCardProps {
  credential: CredentialStatusItem
  onViewBalance: (id: number) => void
  selected: boolean
  onToggleSelect: () => void
  balance: BalanceResponse | null
  loadingBalance: boolean
  balanceError?: string
}

const disabledReasonLabels: Record<string, string> = {
  Manual: '手动禁用',
  PermissionDenied: '上游拒绝权限，请检查后启用',
  TooManyFailures: '连续调用失败',
  TooManyRefreshFailures: '连续刷新失败',
  QuotaExceeded: '额度已用尽',
  InvalidRefreshToken: '刷新凭据已失效',
  InvalidConfig: '配置无效',
}

function formatLastUsed(lastUsedAt: string | null): string {
  if (!lastUsedAt) return '从未使用'
  const date = new Date(lastUsedAt)
  if (Number.isNaN(date.getTime())) return '未知'
  const now = new Date()
  const diff = now.getTime() - date.getTime()
  if (diff < 0) return '刚刚'
  const seconds = Math.floor(diff / 1000)
  if (seconds < 60) return `${seconds} 秒前`
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return `${minutes} 分钟前`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours} 小时前`
  const days = Math.floor(hours / 24)
  return `${days} 天前`
}

export function CredentialCard({
  credential,
  onViewBalance,
  selected,
  onToggleSelect,
  balance,
  loadingBalance,
  balanceError,
}: CredentialCardProps) {
  const [expanded, setExpanded] = useState(false)
  const [editingPriority, setEditingPriority] = useState(false)
  const [priorityValue, setPriorityValue] = useState(String(credential.priority))
  const [showDeleteDialog, setShowDeleteDialog] = useState(false)
  const [showModels, setShowModels] = useState(false)

  const setDisabled = useSetDisabled()
  const setPriority = useSetPriority()
  const resetFailure = useResetFailure()
  const deleteCredential = useDeleteCredential()
  const forceRefresh = useForceRefreshToken()

  const handleToggleDisabled = () => {
    setDisabled.mutate(
      { id: credential.id, disabled: !credential.disabled },
      {
        onSuccess: (res) => {
          toast.success(res.message)
        },
        onError: (err) => {
          toast.error('操作失败: ' + (err as Error).message)
        },
      }
    )
  }

  const handlePriorityChange = () => {
    const newPriority = Number(priorityValue)
    if (
      !priorityValue.trim() ||
      !Number.isInteger(newPriority) ||
      newPriority < 0 ||
      newPriority > 4294967295
    ) {
      toast.error('优先级必须是非负整数')
      return
    }
    setPriority.mutate(
      { id: credential.id, priority: newPriority },
      {
        onSuccess: (res) => {
          toast.success(res.message)
          setEditingPriority(false)
        },
        onError: (err) => {
          toast.error('操作失败: ' + (err as Error).message)
        },
      }
    )
  }

  const handleReset = () => {
    resetFailure.mutate(credential.id, {
      onSuccess: (res) => {
        toast.success(res.message)
      },
      onError: (err) => {
        toast.error('操作失败: ' + (err as Error).message)
      },
    })
  }

  const handleForceRefresh = () => {
    forceRefresh.mutate(credential.id, {
      onSuccess: (res) => {
        toast.success(res.message)
      },
      onError: (err) => {
        toast.error('刷新失败: ' + (err as Error).message)
      },
    })
  }

  const handleDelete = () => {
    if (!credential.disabled) {
      toast.error('请先禁用凭据再删除')
      setShowDeleteDialog(false)
      return
    }

    deleteCredential.mutate(credential.id, {
      onSuccess: (res) => {
        toast.success(res.message)
        setShowDeleteDialog(false)
      },
      onError: (err) => {
        toast.error('删除失败: ' + (err as Error).message)
      },
    })
  }

  const hasFailures = credential.failureCount > 0 || credential.refreshFailureCount > 0
  const disabledReason = credential.disabledReason
    ? disabledReasonLabels[credential.disabledReason] || credential.disabledReason
    : null
  const name = credential.email || `凭据 #${credential.id}`
  const authLabel =
    credential.authMethod === 'api_key'
      ? 'API Key'
      : credential.authMethod === 'idc'
        ? 'IdC'
        : credential.authMethod === 'social'
          ? 'Social'
          : credential.authMethod || '未知类型'
  const remainingPercentage = balance
    ? Math.min(100, Math.max(0, 100 - balance.usagePercentage))
    : 0

  return (
    <>
      <div className={cn('transition-colors hover:bg-muted/30', selected && 'bg-muted/50')}>
        <div className="credential-grid items-center gap-4 px-4 py-4">
          <div className="col-span-2 flex min-w-0 items-center gap-3 lg:col-span-1">
            <Checkbox
              checked={selected}
              onCheckedChange={onToggleSelect}
              aria-label={`选择${name}`}
            />
            <div className="min-w-0 flex-1">
              <div className="flex min-w-0 items-center gap-2">
                <span className="truncate text-sm font-medium" title={name}>
                  {name}
                </span>
                {credential.isCurrent && (
                  <Badge variant="success" className="shrink-0 px-2">
                    当前
                  </Badge>
                )}
              </div>
              <div className="mt-1 flex min-w-0 items-center gap-2 text-xs text-muted-foreground">
                <span className="shrink-0 font-mono">#{credential.id}</span>
                <span className="shrink-0">{authLabel}</span>
                {credential.endpoint && (
                  <span className="truncate" title={credential.endpoint}>
                    {credential.endpoint}
                  </span>
                )}
              </div>
            </div>
          </div>

          <div>
            <div className="mb-1 text-xs text-muted-foreground lg:hidden">状态</div>
            <label className="inline-flex cursor-pointer items-center gap-2">
              <Switch
                checked={!credential.disabled}
                onCheckedChange={handleToggleDisabled}
                disabled={setDisabled.isPending}
                aria-label={`启用${name}`}
              />
              <span
                className={cn(
                  'text-xs',
                  credential.disabled
                    ? 'text-muted-foreground'
                    : 'text-green-600 dark:text-green-400'
                )}
              >
                {credential.disabled ? '禁用' : '启用'}
              </span>
            </label>
            {credential.disabled && disabledReason && (
              <p className="mt-1 truncate text-xs text-destructive" title={disabledReason}>
                {disabledReason}
              </p>
            )}
            {!credential.disabled && (credential.cooldownRemainingMs ?? 0) > 0 && (
              <p className="mt-1 text-xs text-amber-600 dark:text-amber-400">
                冷却剩余约 {Math.ceil(credential.cooldownRemainingMs! / 1000)} 秒
              </p>
            )}
          </div>

          <div>
            <div className="mb-1 text-xs text-muted-foreground lg:hidden">优先级</div>
            <Button
              variant="ghost"
              size="sm"
              className="h-8 gap-1 px-2 font-mono"
              title="编辑优先级，数值越小越优先"
              aria-label={`编辑${name}的优先级，当前为 ${credential.priority}`}
              onClick={() => {
                setPriorityValue(String(credential.priority))
                setEditingPriority(true)
              }}
            >
              {credential.priority}
              <Pencil className="!h-3 !w-3 text-muted-foreground" />
            </Button>
          </div>

          <div className="min-w-0 text-sm tabular-nums">
            <div className="mb-1 text-xs text-muted-foreground lg:hidden">调用情况</div>
            <div>
              {credential.successCount.toLocaleString()}{' '}
              <span className="text-xs text-muted-foreground">次成功</span>
            </div>
            <div
              className={cn(
                'mt-1 text-xs',
                hasFailures ? 'text-destructive' : 'text-muted-foreground'
              )}
            >
              失败 {credential.failureCount} · 刷新失败 {credential.refreshFailureCount}
            </div>
            {credential.inFlight !== undefined && (
              <div className="mt-1 text-xs text-muted-foreground">
                在途 {credential.inFlight}
                {credential.effectiveConcurrencyLimit !== undefined && ` / 当前上限 ${credential.effectiveConcurrencyLimit}`}
              </div>
            )}
            {credential.adaptiveProbeTarget !== undefined && (
              <div className="mt-1 text-xs text-muted-foreground">
                单请求探测 · 成功后上限 {credential.adaptiveProbeTarget}
              </div>
            )}
            <div
              className="mt-1 text-xs text-muted-foreground"
              title={
                credential.lastUsedAt ? new Date(credential.lastUsedAt).toLocaleString() : undefined
              }
            >
              {formatLastUsed(credential.lastUsedAt)}
            </div>
          </div>

          <div className="min-w-0 text-sm tabular-nums">
            <div className="mb-1 text-xs text-muted-foreground lg:hidden">剩余用量</div>
            {loadingBalance && !balance ? (
              <span className="inline-flex items-center gap-1 text-xs text-muted-foreground">
                <Loader2 className="h-3 w-3 animate-spin" />
                查询中…
              </span>
            ) : balance ? (
              <>
                <div
                  className="truncate"
                  title={`${balance.remaining.toFixed(2)} / ${balance.usageLimit.toFixed(2)}`}
                >
                  {balance.remaining.toFixed(2)}{' '}
                  <span className="text-xs text-muted-foreground">
                    / {balance.usageLimit.toFixed(2)}
                  </span>
                </div>
                <div
                  className="my-1.5 h-1 w-full max-w-32 overflow-hidden rounded-full bg-muted"
                  role="meter"
                  aria-label={`${name}剩余用量百分比`}
                  aria-valuemin={0}
                  aria-valuemax={100}
                  aria-valuenow={remainingPercentage}
                >
                  <div
                    className={cn(
                      'h-full rounded-full',
                      remainingPercentage < 20 ? 'bg-destructive' : 'bg-green-500'
                    )}
                    style={{ width: `${remainingPercentage}%` }}
                  />
                </div>
                <p
                  className="truncate text-xs text-muted-foreground"
                  title={balance.subscriptionTitle || undefined}
                >
                  {balance.subscriptionTitle || '未知订阅'} · {remainingPercentage.toFixed(0)}%
                </p>
                <p className="mt-1 text-xs text-muted-foreground" title={balance.updatedAt ? new Date(balance.updatedAt * 1000).toLocaleString('zh-CN') : undefined}>
                  {loadingBalance ? '更新中…' : balance.updatedAt
                    ? `更新于 ${formatLastUsed(new Date(balance.updatedAt * 1000).toISOString())}`
                    : '更新时间未知'}
                </p>
              </>
            ) : (
              <span className="text-xs text-muted-foreground">尚未查询</span>
            )}
            {balanceError && !loadingBalance && (
              <p className="mt-1 text-xs text-destructive" title={balanceError}>
                {balance ? '更新失败，显示上次记录' : '查询失败，请手动重试'}
              </p>
            )}
          </div>

          <div className="col-span-2 flex items-center justify-end gap-1 border-t pt-3 lg:col-span-1 lg:border-0 lg:pt-0">
            <Button
              size="sm"
              variant="outline"
              className="h-8 px-2"
              onClick={() => onViewBalance(credential.id)}
            >
              <Wallet />
              余额
            </Button>
            <Button
              size="icon"
              variant="ghost"
              className="h-8 w-8"
              onClick={() => setExpanded(!expanded)}
              aria-label={`${expanded ? '收起' : '展开'}${name}的详情`}
              aria-expanded={expanded}
              aria-controls={`credential-details-${credential.id}`}
            >
              <ChevronDown className={cn('transition-transform', expanded && 'rotate-180')} />
            </Button>
            <DropdownMenu>
              <DropdownMenuTrigger asChild>
                <Button
                  size="icon"
                  variant="ghost"
                  className="h-8 w-8"
                  aria-label={`${name}的更多操作`}
                >
                  <MoreHorizontal />
                </Button>
              </DropdownMenuTrigger>
              <DropdownMenuContent align="end">
                <DropdownMenuItem onSelect={() => setShowModels(true)}>
                  <Boxes />
                  查看可用模型
                </DropdownMenuItem>
                <DropdownMenuItem
                  onSelect={handleForceRefresh}
                  disabled={
                    forceRefresh.isPending ||
                    credential.disabled ||
                    credential.authMethod === 'api_key'
                  }
                >
                  <RefreshCw className={forceRefresh.isPending ? 'animate-spin' : ''} />
                  刷新 Token
                </DropdownMenuItem>
                <DropdownMenuItem
                  onSelect={handleReset}
                  disabled={resetFailure.isPending || !hasFailures}
                >
                  <RotateCcw />
                  重置失败
                </DropdownMenuItem>
                <DropdownMenuSeparator />
                <DropdownMenuItem
                  className="text-destructive focus:text-destructive"
                  onSelect={() => setShowDeleteDialog(true)}
                  disabled={!credential.disabled || deleteCredential.isPending}
                >
                  <Trash2 />
                  删除凭据（需先禁用）
                </DropdownMenuItem>
              </DropdownMenuContent>
            </DropdownMenu>
          </div>
        </div>

        {expanded && (
          <dl
            id={`credential-details-${credential.id}`}
            className="grid gap-4 border-t bg-muted/20 px-4 py-4 text-xs sm:grid-cols-2 lg:grid-cols-3"
          >
            <div>
              <dt className="text-muted-foreground">Token 到期时间</dt>
              <dd className="mt-1 break-words">
                {credential.expiresAt
                  ? new Date(credential.expiresAt).toLocaleString('zh-CN', { hour12: false })
                  : '未提供'}
              </dd>
            </div>
            <div>
              <dt className="text-muted-foreground">代理</dt>
              <dd className="mt-1 break-all">
                {credential.hasProxy ? credential.proxyUrl || '已配置' : '未配置'}
              </dd>
            </div>
            <div>
              <dt className="text-muted-foreground">Profile ARN</dt>
              <dd className="mt-1">{credential.hasProfileArn ? '已配置' : '未配置'}</dd>
            </div>
            {credential.maskedApiKey && (
              <div>
                <dt className="text-muted-foreground">API Key</dt>
                <dd className="mt-1 break-all font-mono">{credential.maskedApiKey}</dd>
              </div>
            )}
            {disabledReason && (
              <div className="sm:col-span-2">
                <dt className="text-muted-foreground">禁用原因</dt>
                <dd className="mt-1 break-words text-destructive">{disabledReason}</dd>
              </div>
            )}
          </dl>
        )}
      </div>

      <Dialog open={editingPriority} onOpenChange={setEditingPriority}>
        <DialogContent className="sm:max-w-sm">
          <DialogHeader>
            <DialogTitle>编辑优先级</DialogTitle>
            <DialogDescription>{name}，数值越小越优先。</DialogDescription>
          </DialogHeader>
          <form
            onSubmit={(event) => {
              event.preventDefault()
              handlePriorityChange()
            }}
            className="space-y-4"
          >
            <label className="block space-y-2 text-sm">
              <span>优先级</span>
              <Input
                type="number"
                min="0"
                max="4294967295"
                step="1"
                autoFocus
                required
                value={priorityValue}
                onChange={(event) => setPriorityValue(event.target.value)}
              />
            </label>
            <DialogFooter>
              <Button type="button" variant="outline" onClick={() => setEditingPriority(false)}>
                取消
              </Button>
              <Button type="submit" disabled={setPriority.isPending}>
                {setPriority.isPending ? '保存中…' : '保存'}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>

      {showModels && <ModelsDialog credentialId={credential.id} open={showModels} onOpenChange={setShowModels} />}

      <Dialog open={showDeleteDialog} onOpenChange={setShowDeleteDialog}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>确认删除凭据</DialogTitle>
            <DialogDescription>
              您确定要删除 {name}（#{credential.id}）吗？此操作无法撤销。
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              variant="outline"
              onClick={() => setShowDeleteDialog(false)}
              disabled={deleteCredential.isPending}
            >
              取消
            </Button>
            <Button
              variant="destructive"
              onClick={handleDelete}
              disabled={deleteCredential.isPending || !credential.disabled}
            >
              {deleteCredential.isPending ? '删除中…' : '确认删除'}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  )
}

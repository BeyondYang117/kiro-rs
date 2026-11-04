import { RefreshCw } from 'lucide-react'
import type { BalanceResponse } from '@/types/api'
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '@/components/ui/dialog'
import { Progress } from '@/components/ui/progress'
import { Button } from '@/components/ui/button'

interface BalanceDialogProps {
  credentialId: number | null
  open: boolean
  onOpenChange: (open: boolean) => void
  balance: BalanceResponse | null
  loading: boolean
  error?: string
  onRefresh: () => void
}

export function BalanceDialog({
  credentialId,
  open,
  onOpenChange,
  balance,
  loading,
  error,
  onRefresh,
}: BalanceDialogProps) {
  const formatDate = (timestamp: number | null) => {
    if (!timestamp) return '未知'
    return new Date(timestamp * 1000).toLocaleString('zh-CN')
  }

  const formatNumber = (num: number) => {
    return num.toLocaleString('zh-CN', { minimumFractionDigits: 2, maximumFractionDigits: 2 })
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>凭据 #{credentialId} 余额信息</DialogTitle>
        </DialogHeader>

        <div className="flex items-center justify-between gap-2 text-xs text-muted-foreground">
          <span>{balance?.updatedAt ? `更新于 ${formatDate(balance.updatedAt)}` : '尚无查询记录'}</span>
          <Button size="sm" variant="outline" disabled={loading || credentialId === null} onClick={onRefresh}>
            <RefreshCw className={loading ? 'animate-spin' : ''} />
            刷新余额
          </Button>
        </div>

        {loading && !balance && (
          <div className="flex items-center justify-center py-8">
            <div className="animate-spin rounded-full h-8 w-8 border-b-2 border-primary"></div>
          </div>
        )}

        {error && !loading && (
          <div className="space-y-1 text-sm text-destructive" role="alert">
            <p>{error}</p>
            {balance && <p>更新失败，当前显示上次成功查询的记录。</p>}
          </div>
        )}

        {balance && (
          <div className="space-y-4">
            {/* 订阅类型 */}
            <div className="text-center">
              <span className="text-lg font-semibold">
                {balance.subscriptionTitle || '未知订阅类型'}
              </span>
            </div>

            {/* 使用进度 */}
            <div className="space-y-2">
              <div className="flex justify-between text-sm">
                <span>已使用: ${formatNumber(balance.currentUsage)}</span>
                <span>限额: ${formatNumber(balance.usageLimit)}</span>
              </div>
              <Progress value={balance.usagePercentage} />
              <div className="text-center text-sm text-muted-foreground">
                {balance.usagePercentage.toFixed(1)}% 已使用
              </div>
            </div>

            {/* 详细信息 */}
            <div className="grid grid-cols-2 gap-4 pt-4 border-t text-sm">
              <div>
                <span className="text-muted-foreground">剩余额度：</span>
                <span className="font-medium text-green-600">
                  ${formatNumber(balance.remaining)}
                </span>
              </div>
              <div>
                <span className="text-muted-foreground">下次重置：</span>
                <span className="font-medium">{formatDate(balance.nextResetAt)}</span>
              </div>
            </div>
          </div>
        )}
      </DialogContent>
    </Dialog>
  )
}

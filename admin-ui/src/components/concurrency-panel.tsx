import { Card, CardContent } from '@/components/ui/card'
import { Progress } from '@/components/ui/progress'
import type { ConcurrencySnapshot } from '@/types/api'

export function ConcurrencyPanel({ metrics }: { metrics: ConcurrencySnapshot }) {
  const averageWait = metrics.finishedWaitTotal > 0
    ? metrics.totalWaitMs / metrics.finishedWaitTotal / 1000
    : 0

  return (
    <Card className="mb-6" aria-label="上游并发状态">
      <CardContent className="p-4 sm:p-6">
        <div className="mb-4 flex flex-wrap items-center justify-between gap-2">
          <h2 className="text-sm font-semibold">上游并发</h2>
          <p className="text-xs text-muted-foreground">每 5 秒刷新 · 累计值自本次启动起</p>
        </div>
        <dl className="grid grid-cols-2 gap-5 lg:grid-cols-4">
          <div>
            <dt className="text-xs text-muted-foreground">在途调用</dt>
            <dd className="mt-1 text-xl font-semibold tabular-nums">
              {metrics.inFlight} <span className="text-sm font-normal text-muted-foreground">/ {metrics.maxInFlight}</span>
            </dd>
            <Progress value={metrics.inFlight} max={metrics.maxInFlight} className="mt-2 h-1.5" aria-label="在途调用占比" />
            <p className="mt-2 text-xs text-muted-foreground">每账号上限 {metrics.maxInFlightPerCredential}</p>
          </div>
          <div>
            <dt className="text-xs text-muted-foreground">等待调用</dt>
            <dd className="mt-1 text-xl font-semibold tabular-nums">
              {metrics.waiting} <span className="text-sm font-normal text-muted-foreground">/ {metrics.maxWaiting}</span>
            </dd>
            <Progress value={metrics.waiting} max={Math.max(metrics.maxWaiting, 1)} className="mt-2 h-1.5" aria-label="等待队列占比" />
            <p className="mt-2 text-xs text-muted-foreground">
              {metrics.maxWaiting === 0 ? '满载立即拒绝' : `最长已等 ${(metrics.oldestWaitMs / 1000).toFixed(1)} 秒 · 超时 ${metrics.waitTimeoutSeconds} 秒`}
            </p>
          </div>
          <div>
            <dt className="text-xs text-muted-foreground">冷却 / 满载账号</dt>
            <dd className="mt-1 text-xl font-semibold tabular-nums">{metrics.coolingCredentials} / {metrics.saturatedCredentials}</dd>
            <p className="mt-2 text-xs text-muted-foreground">已启用账号中统计，可重叠</p>
          </div>
          <div>
            <dt className="text-xs text-muted-foreground">排队拒绝 / 等待超时</dt>
            <dd className="mt-1 text-xl font-semibold tabular-nums">{metrics.queueFullTotal.toLocaleString()} / {metrics.waitTimeoutTotal.toLocaleString()}</dd>
            <p className="mt-2 text-xs text-muted-foreground">429 共 {metrics.rateLimitedTotal.toLocaleString()} 次</p>
          </div>
        </dl>
        <p className="mt-4 border-t pt-3 text-xs leading-relaxed text-muted-foreground">
          累计获准 {metrics.admittedTotal.toLocaleString()} 次 · 入队 {metrics.queuedTotal.toLocaleString()} 次 · 取消等待 {metrics.cancelledWaitTotal.toLocaleString()} 次
          {' · '}已结束排队平均 {averageWait.toFixed(2)} 秒 / 最长 {(metrics.maxWaitMs / 1000).toFixed(2)} 秒
        </p>
      </CardContent>
    </Card>
  )
}

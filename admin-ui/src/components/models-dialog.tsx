import { useState } from 'react'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import { RefreshCw, Search } from 'lucide-react'
import { getAvailableModels } from '@/api/models'
import { extractErrorMessage } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Input } from '@/components/ui/input'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'

export function ModelsDialog({
  credentialId,
  open,
  onOpenChange,
}: {
  credentialId: number
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  const [search, setSearch] = useState('')
  const queryClient = useQueryClient()
  const query = useQuery({
    queryKey: ['credential-models', credentialId],
    queryFn: () => getAvailableModels(credentialId),
    enabled: open,
    staleTime: 0,
    retry: false,
    refetchOnWindowFocus: false,
  })
  const data = query.data
  const [refreshing, setRefreshing] = useState(false)
  const [refreshError, setRefreshError] = useState<string | null>(null)
  // Explicit refresh goes through the same query cache so reopening preserves the result.
  const refresh = async () => {
    setRefreshing(true)
    setRefreshError(null)
    try {
      const result = await getAvailableModels(credentialId, true)
      queryClient.setQueryData(['credential-models', credentialId], result)
    } catch (error) {
      setRefreshError(extractErrorMessage(error))
    } finally {
      setRefreshing(false)
    }
  }
  const models =
    data?.models.filter((model) =>
      [model.modelId, model.modelName, model.provider].some((value) =>
        value?.toLowerCase().includes(search.toLowerCase())
      )
    ) ?? []
  const busy = query.isFetching || refreshing
  const error =
    refreshError ?? (query.error ? extractErrorMessage(query.error) : null)

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[90dvh] w-[calc(100%-2rem)] overflow-y-auto sm:max-w-3xl">
        <DialogHeader>
          <DialogTitle>凭据 #{credentialId} 可用模型</DialogTitle>
          <DialogDescription>
            从该账号的 Kiro 服务查询授权模型。代理支持状态用于区分已适配的模型。
            成功查询的目录在 5 分钟内用于账号选路，过期或刷新失败后恢复订阅规则。
          </DialogDescription>
        </DialogHeader>
        <div className="flex flex-wrap items-center justify-between gap-3 text-xs text-muted-foreground">
          <span>
            {data
              ? `${data.region} · ${data.models.length} 个模型 · 更新于 ${new Date(data.updatedAt).toLocaleString()}`
              : '尚无查询记录'}
          </span>
          <Button
            type="button"
            size="sm"
            variant="outline"
            disabled={busy}
            onClick={refresh}
          >
            <RefreshCw className={busy ? 'animate-spin' : ''} />
            {busy ? '正在查询…' : '刷新模型'}
          </Button>
        </div>
        {(error || data?.stale) && (
          <p
            role="alert"
            className="rounded-md border border-amber-500/40 bg-amber-500/5 p-3 text-sm"
          >
            {error ?? data?.refreshError}
            {error && data && '。当前显示上次记录，请刷新确认当前授权。'}
          </p>
        )}
        <label className="flex items-center gap-2">
          <Search className="h-4 w-4 text-muted-foreground" />
          <Input
            aria-label="搜索模型"
            placeholder="搜索名称、模型 ID 或提供商"
            value={search}
            onChange={(event) => setSearch(event.target.value)}
          />
        </label>
        {query.isPending && busy && (
          <p
            role="status"
            className="py-6 text-center text-sm text-muted-foreground"
          >
            正在查询账号可用模型…
          </p>
        )}
        {data && models.length === 0 && (
          <p className="py-6 text-center text-sm text-muted-foreground">
            {data.models.length
              ? '没有匹配的模型'
              : '该账号在当前区域未返回可用模型'}
          </p>
        )}
        <div className="space-y-3">
          {models.map((model) => (
            <article
              key={model.modelId}
              className="space-y-3 rounded-lg border p-4"
            >
              <div className="flex flex-wrap items-start justify-between gap-2">
                <div className="min-w-0">
                  <h3 className="font-medium">{model.modelName}</h3>
                  <p className="break-all font-mono text-xs text-muted-foreground">
                    {model.modelId}
                  </p>
                </div>
                <div className="flex flex-wrap gap-2">
                  {data?.defaultModelId === model.modelId && (
                    <Badge variant="outline">账号默认</Badge>
                  )}
                  <Badge
                    variant={model.proxySupported ? 'secondary' : 'outline'}
                  >
                    {model.proxySupported ? '代理已适配' : '代理待适配'}
                  </Badge>
                </div>
              </div>
              <dl className="grid gap-3 text-xs sm:grid-cols-2 lg:grid-cols-3">
                <div>
                  <dt className="text-muted-foreground">提供商</dt>
                  <dd>{model.provider ?? '未上报'}</dd>
                </div>
                <div>
                  <dt className="text-muted-foreground">
                    输入 / 输出 Token 上限
                  </dt>
                  <dd>
                    {model.maxInputTokens?.toLocaleString() ?? '未上报'} /{' '}
                    {model.maxOutputTokens?.toLocaleString() ?? '未上报'}
                    {model.nativeMaxTokens === false && (
                      <span className="block text-muted-foreground">
                        上游不接受原生 max_tokens，使用模型默认输出配置
                      </span>
                    )}
                    {model.minOutputTokens != null && (
                      <span className="block text-muted-foreground">
                        输出下限 {model.minOutputTokens.toLocaleString()}
                      </span>
                    )}
                  </dd>
                </div>
                <div>
                  <dt className="text-muted-foreground">计费倍率</dt>
                  <dd>
                    {model.rateMultiplier !== null
                      ? `${model.rateMultiplier}×${model.rateUnit ? ` · ${model.rateUnit}` : ''}`
                      : '未上报'}
                  </dd>
                </div>
                <div className="sm:col-span-2">
                  <dt className="text-muted-foreground">Effort</dt>
                  <dd className="break-words">
                    {model.effortLevels.length
                      ? `${model.effortLevels.join(' / ')}${model.defaultEffort ? `（默认 ${model.defaultEffort}）` : ''}`
                      : '未上报'}
                    {model.effortSchemaPath && (
                      <span className="ml-2 font-mono text-muted-foreground">
                        {model.effortSchemaPath}
                      </span>
                    )}
                  </dd>
                </div>
                <div>
                  <dt className="text-muted-foreground">Thinking</dt>
                  <dd className="break-words">
                    {model.thinkingTypes.length
                      ? `${model.thinkingTypes.join(' / ')}${model.defaultThinking ? `（默认 ${model.defaultThinking}）` : ''}`
                      : '未上报'}
                    {!!model.thinkingDisplays?.length && (
                      <span className="block text-muted-foreground">
                        显示：{model.thinkingDisplays.join(' / ')}
                      </span>
                    )}
                  </dd>
                </div>
              </dl>
            </article>
          ))}
        </div>
        <p className="text-xs leading-relaxed text-muted-foreground">
          模型列表缓存 5 分钟。“代理待适配”的模型暂不能通过 messages
          接口调用；Claude 原生参数需启用 nativeThinking，GPT/Kimi 使用各自的原生参数。
          这里只查询模型目录，不会发送生成请求或自动切换模型。
        </p>
      </DialogContent>
    </Dialog>
  )
}

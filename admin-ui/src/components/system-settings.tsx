import { useEffect, useState, type FormEvent } from 'react'
import { flushSync } from 'react-dom'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import {
  ArrowLeft,
  Check,
  LogOut,
  RefreshCw,
  RotateCcw,
  Save,
  Settings,
} from 'lucide-react'
import { toast } from 'sonner'
import {
  getSettings,
  saveSettings,
  type SettingsResponse,
  type SystemSettings,
} from '@/api/settings'
import { extractErrorMessage } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Input } from '@/components/ui/input'
import { Badge } from '@/components/ui/badge'
import { Switch } from '@/components/ui/switch'

type Field = {
  path: string
  label: string
  help: string
  min?: number
  max?: number
  type?: 'text' | 'switch' | 'select'
  options?: { value: string; label: string }[]
}

const groups: {
  id: string
  title: string
  description: string
  fields: Field[]
}[] = [
  {
    id: 'scheduling',
    title: '调度与并发',
    description: '控制账号选择、上游容量与排队。负载均衡模式保存后立即生效。',
    fields: [
      {
        path: 'loadBalancingMode',
        label: '负载均衡模式',
        help: '均衡模式分散请求；优先级模式优先使用高优先级账号。',
        type: 'select',
        options: [
          { value: 'balanced', label: '均衡负载' },
          { value: 'priority', label: '优先级模式' },
        ],
      },
      {
        path: 'concurrency.maxInFlight',
        label: '全局上游并发',
        help: '所有账号共享的上游在途调用上限。',
        min: 1,
        max: 1000000,
      },
      {
        path: 'concurrency.maxInFlightPerCredential',
        label: '单账号并发',
        help: '每个账号允许的最大在途调用数。',
        min: 1,
        max: 1000000,
      },
      {
        path: 'concurrency.maxWaiting',
        label: '最大排队数',
        help: '等待上游名额的请求数，0 表示不排队。',
        min: 0,
        max: 1000000,
      },
      {
        path: 'concurrency.waitTimeoutSeconds',
        label: '排队超时（秒）',
        help: '超过等待时长后返回容量不足。',
        min: 1,
        max: 86400,
      },
      {
        path: 'concurrency.requestsPerMinute',
        label: '单账号每分钟调用数',
        help: '滚动 60 秒的调用预留上限，0 表示关闭主动限速。',
        min: 0,
        max: 1000000,
      },
      {
        path: 'concurrency.rateLimitCooldownSeconds',
        label: '限流冷却（秒）',
        help: '上游限流后账号的基础冷却时长。',
        min: 1,
        max: 86400,
      },
      {
        path: 'concurrency.refreshBackoffSeconds',
        label: '认证刷新退避（秒）',
        help: '认证服务临时故障后的初始退避时长。',
        min: 1,
        max: 86400,
      },
      {
        path: 'concurrency.adaptiveConcurrency',
        label: '自适应并发',
        help: '限流后降低单账号并发，成功读取响应后逐步恢复。',
        type: 'switch',
      },
    ],
  },
  {
    id: 'requests',
    title: '请求与响应限制',
    description: '约束单个客户端请求的总耗时、上游尝试次数和响应规模。',
    fields: [
      {
        path: 'requestLimits.maxConcurrentRequests',
        label: '客户端请求并发',
        help: '同时处理的客户端请求上限，与上游并发分别控制。',
        min: 1,
        max: 1000000,
      },
      {
        path: 'requestLimits.maxUpstreamAttempts',
        label: '上游尝试次数',
        help: '包含重试、压缩、搜索与工具恢复的总尝试上限。',
        min: 1,
        max: 100,
      },
      {
        path: 'requestLimits.totalTimeoutSeconds',
        label: '总超时（秒）',
        help: '单个请求从接入到响应消费完成的总时间预算。',
        min: 1,
        max: 86400,
      },
      {
        path: 'requestLimits.firstContentTimeoutSeconds',
        label: '首内容超时（秒）',
        help: '等待上游首个有效内容的最长时间。',
        min: 1,
        max: 86400,
      },
      {
        path: 'requestLimits.idleTimeoutSeconds',
        label: '响应空闲超时（秒）',
        help: '响应流连续没有内容时的等待上限。',
        min: 1,
        max: 86400,
      },
      {
        path: 'requestLimits.maxResponseBytes',
        label: '响应大小上限（字节）',
        help: '默认 33554432 字节，即 32 MiB。',
        min: 1024,
        max: 1073741824,
      },
      {
        path: 'requestLimits.maxResponseEvents',
        label: '响应事件上限',
        help: '单个响应允许消费的最大事件数。',
        min: 1,
        max: 1000000,
      },
    ],
  },
  {
    id: 'affinity',
    title: '会话亲和',
    description: '尽量将同一会话分配到最近成功响应的账号。',
    fields: [
      {
        path: 'sessionAffinity.enabled',
        label: '启用会话亲和',
        help: '仅在请求成功后建立或刷新账号绑定。',
        type: 'switch',
      },
      {
        path: 'sessionAffinity.ttlSeconds',
        label: '绑定有效期（秒）',
        help: '超过有效期的会话重新选择账号。',
        min: 1,
        max: 86400,
      },
      {
        path: 'sessionAffinity.maxEntries',
        label: '最大绑定数',
        help: '限制内存中保存的会话绑定数量。',
        min: 1,
        max: 1000000,
      },
    ],
  },
  {
    id: 'behavior',
    title: '响应处理与日志',
    description: '设置流式响应行为、工具恢复策略和调用日志保留时间。',
    fields: [
      {
        path: 'ccBufferedStream',
        label: 'Claude Code 响应缓冲',
        help: '缓冲完整响应以修正初始用量；关闭后直接流式输出。',
        type: 'switch',
      },
      {
        path: 'extractThinking',
        label: '提取思考内容',
        help: '将非流式响应的 thinking 标签解析为独立内容块。',
        type: 'switch',
      },
      {
        path: 'toolOutputRecoveryAttempts',
        label: '工具输出恢复次数',
        help: '未输出内容时，对截断工具调用进行缩块恢复，0 表示关闭。',
        min: 0,
        max: 2,
      },
      {
        path: 'countTokensTimeoutSeconds',
        label: '远程 Token 计数超时（秒）',
        help: '远程服务失败或超时后回退到本地估算。',
        min: 1,
        max: 300,
      },
      {
        path: 'requestLogRetentionDays',
        label: '调用日志保留（天）',
        help: '重启后按新期限清理，同时最多保留 100000 条记录。',
        min: 1,
        max: 3650,
      },
    ],
  },
  {
    id: 'upstream',
    title: '上游基础配置',
    description: '区域覆盖项留空时使用默认区域。账号自己的区域设置仍然优先。',
    fields: [
      {
        path: 'region',
        label: '默认区域',
        help: '例如 us-east-1。',
        type: 'text',
      },
      {
        path: 'authRegion',
        label: '认证区域',
        help: '用于 Token 刷新，可留空。',
        type: 'text',
      },
      {
        path: 'apiRegion',
        label: 'API 区域',
        help: '用于上游 API 请求，可留空。',
        type: 'text',
      },
      {
        path: 'defaultEndpoint',
        label: '默认端点',
        help: '凭据未指定端点时使用。',
        type: 'select',
      },
      {
        path: 'tlsBackend',
        label: 'TLS 后端',
        help: 'native-tls 需要服务构建时启用对应支持。',
        type: 'select',
        options: [
          { value: 'rustls', label: 'Rustls' },
          { value: 'native-tls', label: 'Native TLS' },
        ],
      },
    ],
  },
]

function getValue(
  settings: SystemSettings,
  path: string
): string | number | boolean | null {
  return path
    .split('.')
    .reduce<unknown>(
      (value, key) => (value as Record<string, unknown>)[key],
      settings
    ) as string | number | boolean | null
}
function setValue(
  settings: SystemSettings,
  path: string,
  value: unknown
): SystemSettings {
  const next = structuredClone(settings)
  const keys = path.split('.')
  const parent = keys
    .slice(0, -1)
    .reduce<
      Record<string, unknown>
    >((object, key) => object[key] as Record<string, unknown>, next as unknown as Record<string, unknown>)
  parent[keys[keys.length - 1]] = value
  return next
}
const display = (value: unknown) =>
  value === null
    ? '跟随默认区域'
    : typeof value === 'boolean'
      ? value
        ? '开启'
        : '关闭'
      : String(value)

export function SystemSettingsPage({
  onBack,
  onLogout,
  registerNavigationGuard,
}: {
  onBack: () => void
  onLogout: () => void
  registerNavigationGuard: (guard: (() => boolean) | null) => void
}) {
  const client = useQueryClient()
  const query = useQuery({
    queryKey: ['system-settings'],
    queryFn: getSettings,
    refetchOnWindowFocus: false,
    staleTime: 0,
  })
  const [snapshot, setSnapshot] = useState<SettingsResponse | null>(null)
  const [draft, setDraft] = useState<SystemSettings | null>(null)
  const [saving, setSaving] = useState(false)
  const [conflict, setConflict] = useState(false)
  const [activeGroup, setActiveGroup] = useState(groups[0].id)
  const dirty = Boolean(
    snapshot &&
      draft &&
      JSON.stringify(snapshot.settings) !== JSON.stringify(draft)
  )

  useEffect(() => {
    // 查询缓存更新时，不覆盖正在编辑的表单。
    if (query.data && !dirty) {
      setSnapshot(query.data)
      setDraft(query.data.settings)
    }
  }, [query.data, dirty])

  useEffect(() => {
    const beforeUnload = (event: BeforeUnloadEvent) => {
      if (dirty) event.preventDefault()
    }
    window.addEventListener('beforeunload', beforeUnload)
    return () => window.removeEventListener('beforeunload', beforeUnload)
  }, [dirty])

  useEffect(() => {
    registerNavigationGuard(
      () =>
        !saving &&
        (!dirty || window.confirm('设置尚未保存，确定离开并放弃修改吗？'))
    )
    return () => registerNavigationGuard(null)
  }, [dirty, saving, registerNavigationGuard])

  const reload = async () => {
    if (dirty && !window.confirm('重新加载会放弃当前未保存修改，确定继续吗？'))
      return
    const result = await query.refetch()
    if (result.data && !result.error) {
      setSnapshot(result.data)
      setDraft(result.data.settings)
      setConflict(false)
    } else toast.error(`读取设置失败：${extractErrorMessage(result.error)}`)
  }
  const submit = async (event: FormEvent) => {
    event.preventDefault()
    if (
      !draft ||
      !snapshot ||
      saving ||
      !dirty ||
      !snapshot.canSave ||
      conflict
    )
      return
    setSaving(true)
    try {
      const result = await saveSettings(draft, snapshot.revision)
      setSnapshot(result)
      setDraft(result.settings)
      client.setQueryData(['system-settings'], result)
      client.invalidateQueries({ queryKey: ['loadBalancingMode'] })
      client.invalidateQueries({ queryKey: ['credentials'] })
      toast.success(
        result.restartRequired.length
          ? '设置已保存，部分参数将在服务重启后生效'
          : '设置已保存并生效'
      )
    } catch (error) {
      if ((error as { response?: { status: number } }).response?.status === 409)
        setConflict(true)
      toast.error(`保存失败：${extractErrorMessage(error)}`)
    } finally {
      setSaving(false)
    }
  }
  const group = groups.find((item) => item.id === activeGroup)!
  const modifiedCount =
    draft && snapshot
      ? groups
          .flatMap((item) => item.fields)
          .filter(
            (field) =>
              getValue(draft, field.path) !==
              getValue(snapshot.settings, field.path)
          ).length
      : 0

  return (
    <div className="min-h-screen bg-background">
      <header className="sticky top-0 z-50 border-b bg-background/95 backdrop-blur">
        <div className="container flex min-h-14 flex-wrap items-center justify-between gap-2 px-4 py-2 md:px-8">
          <div className="flex items-center gap-2">
            <Button
              variant="ghost"
              size="sm"
              disabled={saving}
              onClick={onBack}
            >
              <ArrowLeft className="mr-2 h-4 w-4" />
              凭据管理
            </Button>
            <span className="font-semibold">系统设置</span>
          </div>
          <div className="flex items-center gap-2">
            <Button
              variant="outline"
              size="sm"
              onClick={reload}
              disabled={query.isFetching || saving}
            >
              <RefreshCw
                className={`mr-2 h-4 w-4 ${query.isFetching ? 'animate-spin' : ''}`}
              />
              重新加载
            </Button>
            <Button
              variant="ghost"
              size="icon"
              aria-label="退出登录"
              disabled={saving}
              onClick={onLogout}
            >
              <LogOut className="h-4 w-4" />
            </Button>
          </div>
        </div>
      </header>
      <main className="container mx-auto px-4 py-6 md:px-8">
        <div className="mb-6">
          <h1 className="flex items-center gap-2 text-2xl font-semibold">
            <Settings className="h-6 w-6" />
            系统设置
          </h1>
          <p className="mt-2 text-sm text-muted-foreground">
            集中管理运行参数。负载均衡模式立即生效，其余参数保存后需重启服务。
          </p>
        </div>
        {!snapshot || !draft ? (
          <Card>
            <CardContent className="space-y-4 pt-6">
              <p role="status">
                {query.isError
                  ? `读取设置失败：${extractErrorMessage(query.error)}`
                  : '正在读取系统设置…'}
              </p>
              {query.isError && <Button onClick={reload}>重试</Button>}
            </CardContent>
          </Card>
        ) : (
          <>
            <div className="mb-6 space-y-3" aria-live="polite">
              {!snapshot.canSave && (
                <p className="rounded-lg border p-4 text-sm">
                  当前服务未提供配置文件路径，只能查看设置。
                </p>
              )}
              {conflict && (
                <p
                  role="alert"
                  className="rounded-lg border border-destructive p-4 text-sm text-destructive"
                >
                  设置已被其他操作修改。请重新加载最新设置后再编辑保存。
                </p>
              )}
              {snapshot.restartRequired.length > 0 && (
                <div className="rounded-lg border border-amber-500/40 bg-amber-500/5 p-4">
                  <p className="text-sm font-medium">
                    {snapshot.restartRequired.length} 项已保存，等待服务重启生效
                  </p>
                  <p className="mt-1 text-sm text-muted-foreground">
                    各参数下方可查看当前运行值。恢复为当前运行值并保存可取消待生效修改。
                  </p>
                </div>
              )}
            </div>
            <form
              onSubmit={submit}
              onInvalidCapture={(event) => {
                const id = (event.target as HTMLInputElement).id
                const invalidGroup = groups.find((item) =>
                  item.fields.some((field) => field.path === id)
                )
                if (invalidGroup)
                  flushSync(() => setActiveGroup(invalidGroup.id))
              }}
            >
              <div className="grid items-start gap-6 md:grid-cols-[200px_minmax(0,1fr)]">
                <nav
                  aria-label="设置分类"
                  className="flex flex-wrap gap-2 md:sticky md:top-20 md:flex-col"
                >
                  {groups.map((item) => (
                    <Button
                      key={item.id}
                      type="button"
                      variant={activeGroup === item.id ? 'secondary' : 'ghost'}
                      className="justify-start"
                      aria-current={
                        activeGroup === item.id ? 'page' : undefined
                      }
                      onClick={() => setActiveGroup(item.id)}
                    >
                      {item.title}
                    </Button>
                  ))}
                </nav>
                <div className="space-y-6">
                  {groups.map((item) => (
                    <Card key={item.id} hidden={item.id !== activeGroup}>
                      <CardHeader>
                        <CardTitle>{item.title}</CardTitle>
                        <p className="text-sm text-muted-foreground">
                          {item.description}
                        </p>
                      </CardHeader>
                      <CardContent className="grid gap-x-6 gap-y-6 lg:grid-cols-2">
                        {item.fields.map((field) => {
                          const value = getValue(draft, field.path)
                          const pending = snapshot.restartRequired.includes(
                            field.path
                          )
                          const changed =
                            value !== getValue(snapshot.settings, field.path)
                          const options =
                            field.options ||
                            snapshot.endpoints.map((endpoint) => ({
                              value: endpoint,
                              label: endpoint,
                            }))
                          return (
                            <div key={field.path} className="space-y-2">
                              <div className="flex flex-wrap items-center gap-2">
                                <label
                                  htmlFor={field.path}
                                  className="text-sm font-medium"
                                >
                                  {field.label}
                                </label>
                                {field.path === 'loadBalancingMode' && (
                                  <Badge variant="secondary">立即生效</Badge>
                                )}
                                {changed && (
                                  <span className="text-xs text-primary">
                                    未保存
                                  </span>
                                )}
                              </div>
                              {field.type === 'switch' ? (
                                <Switch
                                  id={field.path}
                                  checked={Boolean(value)}
                                  disabled={saving || !snapshot.canSave}
                                  aria-describedby={`${field.path}-help`}
                                  onCheckedChange={(next) =>
                                    setDraft(setValue(draft, field.path, next))
                                  }
                                />
                              ) : field.type === 'select' ? (
                                <select
                                  id={field.path}
                                  value={String(value)}
                                  disabled={saving || !snapshot.canSave}
                                  aria-describedby={`${field.path}-help`}
                                  className="flex h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm"
                                  onChange={(event) =>
                                    setDraft(
                                      setValue(
                                        draft,
                                        field.path,
                                        event.target.value
                                      )
                                    )
                                  }
                                >
                                  {options.map((option) => (
                                    <option
                                      key={option.value}
                                      value={option.value}
                                    >
                                      {option.label}
                                    </option>
                                  ))}
                                </select>
                              ) : (
                                <Input
                                  id={field.path}
                                  type={
                                    field.type === 'text' ? 'text' : 'number'
                                  }
                                  value={value === null ? '' : String(value)}
                                  min={field.min}
                                  max={field.max}
                                  step={field.type === 'text' ? undefined : 1}
                                  required={
                                    field.type !== 'text' ||
                                    field.path === 'region'
                                  }
                                  disabled={saving || !snapshot.canSave}
                                  aria-describedby={`${field.path}-help`}
                                  onChange={(event) =>
                                    setDraft(
                                      setValue(
                                        draft,
                                        field.path,
                                        field.type === 'text'
                                          ? event.target.value || null
                                          : event.target.value === ''
                                            ? ''
                                            : Number(event.target.value)
                                      )
                                    )
                                  }
                                />
                              )}
                              <p
                                id={`${field.path}-help`}
                                className="text-xs leading-relaxed text-muted-foreground"
                              >
                                {field.help}
                                {field.min !== undefined &&
                                  ` 范围 ${field.min.toLocaleString()}–${field.max!.toLocaleString()}。`}
                              </p>
                              {pending && (
                                <p className="text-xs text-amber-700 dark:text-amber-400">
                                  待重启生效 · 当前运行：
                                  {display(
                                    getValue(snapshot.running, field.path)
                                  )}
                                </p>
                              )}
                            </div>
                          )
                        })}
                      </CardContent>
                    </Card>
                  ))}
                  <div className="flex flex-wrap items-center justify-between gap-3 rounded-lg border bg-card p-4">
                    <Button
                      type="button"
                      variant="ghost"
                      disabled={saving || !snapshot.canSave}
                      onClick={() => {
                        if (
                          window.confirm(
                            `将“${group.title}”恢复为推荐默认值？保存后才会应用。`
                          )
                        ) {
                          let next = draft
                          group.fields.forEach((field) => {
                            next = setValue(
                              next,
                              field.path,
                              getValue(snapshot.defaults, field.path)
                            )
                          })
                          setDraft(next)
                        }
                      }}
                    >
                      <RotateCcw className="mr-2 h-4 w-4" />
                      恢复本组默认值
                    </Button>
                    <div className="flex flex-wrap items-center gap-3">
                      <span className="text-sm text-muted-foreground">
                        {dirty ? (
                          `${modifiedCount} 项未保存`
                        ) : (
                          <span className="flex items-center gap-1">
                            <Check className="h-4 w-4" />
                            已与保存配置同步
                          </span>
                        )}
                      </span>
                      <Button
                        type="button"
                        variant="outline"
                        disabled={!dirty || saving}
                        onClick={() => {
                          setDraft(snapshot.settings)
                        }}
                      >
                        撤销修改
                      </Button>
                      <Button
                        type="submit"
                        disabled={
                          !dirty || saving || !snapshot.canSave || conflict
                        }
                      >
                        <Save className="mr-2 h-4 w-4" />
                        {saving ? '正在保存…' : '保存设置'}
                      </Button>
                    </div>
                  </div>
                </div>
              </div>
            </form>
          </>
        )}
      </main>
    </div>
  )
}

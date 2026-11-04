import { useState, type FormEvent } from 'react'
import { useQuery } from '@tanstack/react-query'
import { ArrowLeft, RefreshCw, LogOut, Download, Moon, Sun } from 'lucide-react'
import { toast } from 'sonner'
import { getLogs, getLog, type LogFilters, type LogRecord, type UpstreamResponseDiagnostics } from '@/api/logs'
import { extractErrorMessage } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Badge } from '@/components/ui/badge'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription } from '@/components/ui/dialog'

const labels = { success: '成功', error: '失败', interrupted: '中断', running: '进行中' }
const formatNumber = (n: number | null | undefined) => n == null ? '—' : n.toLocaleString('zh-CN')
const formatTime = (n: number) => new Date(n).toLocaleString('zh-CN', { hour12: false })
const formatDuration = (n: number | null | undefined) => n == null ? '—' : n < 1000 ? `${n} ms` : `${(n / 1000).toFixed(2)} s`

function Status({ record }: { record: LogRecord }) {
  return <Badge variant={record.status === 'success' ? 'success' : record.status === 'error' ? 'destructive' : record.status === 'interrupted' ? 'warning' : 'secondary'}>{labels[record.status]}</Badge>
}

function ResponseDiagnostics({ data }: { data: UpstreamResponseDiagnostics }) {
  const outcomes: Record<string, string> = { empty: '无有效内容', decode_error: '解码失败', read_error: '读取失败', incomplete: '响应不完整', prefix_limit: '空内容超出限制', limited: '达到上游限制', success: '成功' }
  return <div className="mt-3 space-y-2 rounded-md bg-muted/30 p-3 text-xs">
    <p className="font-medium">响应诊断 · {outcomes[data.outcome] ?? data.outcome}</p>
    <dl className="grid gap-2 sm:grid-cols-2">
      {[
        ['响应类型 / 声明长度', `${data.contentType ?? '未提供'} / ${data.contentLength == null ? '未提供' : `${formatNumber(data.contentLength)} B`}`],
        ['实际接收 / 数据块 / 事件帧', `${formatNumber(data.bodyBytes)} B / ${formatNumber(data.chunks)} / ${formatNumber(data.frames)}`],
        ['文本 / 非空白文本', `${formatNumber(data.textBytes)} B / ${formatNumber(data.nonWhitespaceTextBytes)} B`],
        ['推理 / 脱敏推理 / 工具参数', `${formatNumber(data.reasoningBytes)} B / ${formatNumber(data.redactedReasoningBytes)} B / ${formatNumber(data.toolInputBytes)} B`],
        ['未知事件 / 未完成工具调用', `${formatNumber(data.unknownEvents)} / ${formatNumber(data.pendingToolCalls)}`],
        ['残留不完整帧 / 启用思考', `${data.incompleteFrame ? '是' : '否'} / ${data.thinkingEnabled ? '是' : '否'}`],
      ].map(([label, value]) => <div key={label}><dt className="text-muted-foreground">{label}</dt><dd className="mt-1 break-all">{value}</dd></div>)}
    </dl>
    <p className="break-all">事件计数：{Object.entries(data.eventTypes).map(([kind, count]) => `${kind} × ${count}`).join(' · ') || '未解析到事件'}</p>
    {!!Object.keys(data.eventFields).length && <details><summary className="cursor-pointer text-muted-foreground">查看各类事件首帧字段</summary><pre className="mt-2 max-h-48 overflow-auto whitespace-pre-wrap break-words">{Object.entries(data.eventFields).map(([kind, fields]) => `${kind}: ${fields.join(', ') || '未识别到对象字段'}`).join('\n')}</pre></details>}
  </div>
}

export function RequestLogs({ onBack, onLogout }: { onBack: () => void; onLogout: () => void }) {
  const [filters, setFilters] = useState<LogFilters>({})
  const [draft, setDraft] = useState({ search: '', model: '', status: '', credentialId: '', httpStatus: '', from: '', to: '' })
  const [page, setPage] = useState(1)
  const [autoRefresh, setAutoRefresh] = useState(true)
  const [selectedId, setSelectedId] = useState<string | null>(null)
  const [dark, setDark] = useState(document.documentElement.classList.contains('dark'))
  const { data, error, isLoading, isFetching, refetch } = useQuery({
    queryKey: ['request-logs', filters, page],
    queryFn: () => getLogs(filters, page),
    refetchInterval: autoRefresh ? 10000 : false,
  })
  const detail = useQuery({
    queryKey: ['request-log', selectedId],
    queryFn: () => getLog(selectedId!),
    enabled: selectedId !== null,
    refetchInterval: query => query.state.data?.status === 'running' ? 3000 : false,
  })
  const summary = data?.summary
  const totalPages = Math.max(1, Math.ceil((summary?.total ?? 0) / 20))

  function search(event: FormEvent) {
    event.preventDefault()
    const from = draft.from ? new Date(draft.from).getTime() : undefined
    const to = draft.to ? new Date(draft.to).getTime() : undefined
    if (from != null && to != null && from > to) { toast.error('开始时间不能晚于结束时间'); return }
    setFilters({ search: draft.search.trim() || undefined, model: draft.model.trim() || undefined,
      status: draft.status || undefined, credentialId: draft.credentialId || undefined,
      httpStatus: draft.httpStatus || undefined, from, to })
    setPage(1)
  }

  function exportDetail(record: LogRecord) {
    const url = URL.createObjectURL(new Blob([JSON.stringify(record, null, 2)], { type: 'application/json' }))
    const link = document.createElement('a')
    link.href = url; link.download = `kiro-request-${record.id}.json`; link.click()
    URL.revokeObjectURL(url)
  }

  return <div className="min-h-screen bg-background text-foreground">
    <header className="sticky top-0 z-40 border-b bg-background/95 backdrop-blur">
      <div className="container flex min-h-14 flex-wrap items-center justify-between gap-2 px-4 py-2 md:px-8">
        <div className="flex items-center gap-3">
          <Button variant="ghost" size="sm" onClick={onBack}><ArrowLeft className="mr-2 h-4 w-4" />凭据管理</Button>
          <span className="font-semibold">调用日志</span>
        </div>
        <div className="flex items-center gap-2">
          <label className="flex items-center gap-2 text-sm"><input type="checkbox" checked={autoRefresh} onChange={e => setAutoRefresh(e.target.checked)} />每 10 秒刷新</label>
          <Button variant="ghost" size="icon" aria-label="刷新日志" onClick={() => refetch()} disabled={isFetching}><RefreshCw className={`h-4 w-4 ${isFetching ? 'animate-spin' : ''}`} /></Button>
          <Button variant="ghost" size="icon" aria-label="切换主题" onClick={() => { document.documentElement.classList.toggle('dark'); setDark(!dark) }}>{dark ? <Sun className="h-4 w-4" /> : <Moon className="h-4 w-4" />}</Button>
          <Button variant="ghost" size="icon" aria-label="退出登录" onClick={onLogout}><LogOut className="h-4 w-4" /></Button>
        </div>
      </div>
    </header>
    <main className="container mx-auto space-y-6 px-4 py-6 md:px-8">
      <div><h1 className="text-2xl font-semibold">调用与错误记录</h1>
        <p className="mt-2 text-sm text-muted-foreground">查看请求用量、响应耗时与错误原因。统计随查询条件更新，Token 为服务返回的计数或估算值。</p></div>
      <form onSubmit={search} className="rounded-lg border bg-card p-4">
        <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
          <label className="space-y-1 text-sm">关键字<Input placeholder="请求 ID、路径、错误原因…" value={draft.search} maxLength={512} onChange={e => setDraft({ ...draft, search: e.target.value })} /></label>
          <label className="space-y-1 text-sm">模型<Input placeholder="完整模型名称" value={draft.model} onChange={e => setDraft({ ...draft, model: e.target.value })} /></label>
          <label className="space-y-1 text-sm">调用状态<select className="flex h-10 w-full rounded-md border bg-background px-3" value={draft.status} onChange={e => setDraft({ ...draft, status: e.target.value })}><option value="">全部状态</option>{Object.entries(labels).map(([value, label]) => <option key={value} value={value}>{label}</option>)}</select></label>
          <label className="space-y-1 text-sm">凭据 ID<Input type="number" min="0" step="1" placeholder="全部凭据" value={draft.credentialId} onChange={e => setDraft({ ...draft, credentialId: e.target.value })} /></label>
          <label className="space-y-1 text-sm">开始时间（本地）<Input type="datetime-local" value={draft.from} onChange={e => setDraft({ ...draft, from: e.target.value })} /></label>
          <label className="space-y-1 text-sm">结束时间（本地）<Input type="datetime-local" value={draft.to} onChange={e => setDraft({ ...draft, to: e.target.value })} /></label>
          <label className="space-y-1 text-sm">HTTP 状态码<Input type="number" min="100" max="599" placeholder="例如 400、502" value={draft.httpStatus} onChange={e => setDraft({ ...draft, httpStatus: e.target.value })} /></label>
          <div className="flex items-end gap-2"><Button type="submit">查询</Button><Button type="button" variant="outline" onClick={() => { setDraft({ search: '', model: '', status: '', credentialId: '', httpStatus: '', from: '', to: '' }); setFilters({}); setPage(1) }}>重置</Button></div>
        </div>
      </form>
      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
        {[
          ['调用次数', formatNumber(summary?.total), `成功 ${summary?.success ?? 0} · 失败 ${summary?.error ?? 0} · 中断 ${summary?.interrupted ?? 0} · 进行中 ${summary?.running ?? 0}`],
          ['输入 Token', formatNumber(summary?.inputTokens), '包含 count_tokens 的计数结果'],
          ['输出 Token', formatNumber(summary?.outputTokens), '压缩请求累计各轮用量'],
          ['平均耗时', summary ? formatDuration(Math.round(summary.averageDurationMs)) : '—', '从接收请求到响应结束'],
        ].map(([title, value, caption]) => <Card key={title}><CardHeader className="pb-2"><CardTitle className="text-sm text-muted-foreground">{title}</CardTitle></CardHeader><CardContent><div className="text-2xl font-semibold">{value}</div><p className="mt-1 text-xs text-muted-foreground">{caption}</p></CardContent></Card>)}
      </div>
      {error ? <Card><CardContent className="py-8 text-center"><p className="mb-4 text-destructive">{extractErrorMessage(error)}</p><Button onClick={() => refetch()}>重试</Button></CardContent></Card> :
        <div className="overflow-hidden rounded-lg border">
          <div className="overflow-x-auto"><table className="w-full whitespace-nowrap text-left text-sm">
            <thead className="bg-muted/50 text-muted-foreground"><tr>{['时间 / 请求', '模型 / 接口', '状态', '耗时 / 首包', 'Token 输入 / 输出', '最后凭据 / 尝试', '操作'].map(title => <th className="px-4 py-3 font-medium" key={title}>{title}</th>)}</tr></thead>
            <tbody>{isLoading ? <tr><td colSpan={7} className="p-12 text-center text-muted-foreground">正在加载日志…</td></tr> : !data?.items.length ? <tr><td colSpan={7} className="p-12 text-center text-muted-foreground">暂无符合条件的调用记录。更新后的服务收到 API 请求后会开始记录。</td></tr> : data.items.map(record => <tr key={record.id} className="border-t hover:bg-muted/30">
              <td className="px-4 py-3"><div>{formatTime(record.timestamp)}</div><button className="mt-1 font-mono text-xs text-muted-foreground hover:underline" onClick={() => setSelectedId(record.id)} title={record.id}>{record.id.slice(0, 8)}</button></td>
              <td className="max-w-64 px-4 py-3"><div className="truncate" title={record.model ?? ''}>{record.model ?? '—'} {record.stream && <Badge variant="outline">流式</Badge>}</div><div className="mt-1 font-mono text-xs text-muted-foreground">{record.method} {record.path}</div></td>
              <td className="px-4 py-3"><Status record={record} /><div className="mt-1 text-xs text-muted-foreground">HTTP {record.httpStatus || '—'}</div></td>
              <td className="px-4 py-3"><div>{record.status === 'running' ? '进行中' : formatDuration(record.durationMs)}</div><div className="mt-1 text-xs text-muted-foreground">首包 {formatDuration(record.firstByteMs)}</div></td>
              <td className="px-4 py-3 tabular-nums">{formatNumber(record.inputTokens)} / {formatNumber(record.outputTokens)}</td>
              <td className="px-4 py-3">{record.credentialId == null ? '—' : `#${record.credentialId}`} / {record.attempts} 次</td>
              <td className="px-4 py-3"><Button size="sm" variant="outline" onClick={() => setSelectedId(record.id)}>详情</Button></td>
            </tr>)}</tbody>
          </table></div>
          <div className="flex flex-wrap items-center justify-between gap-3 border-t px-4 py-3 text-sm"><span className="text-muted-foreground">共 {summary?.total ?? 0} 条 · 第 {page} / {totalPages} 页</span><div className="flex gap-2"><Button size="sm" variant="outline" disabled={page <= 1 || isFetching} onClick={() => setPage(page - 1)}>上一页</Button><Button size="sm" variant="outline" disabled={page >= totalPages || isFetching} onClick={() => setPage(page + 1)}>下一页</Button></div></div>
        </div>}
      <p className="text-xs text-muted-foreground">日志保留 {data?.retentionDays ?? 30} 天，定期清理至最近 {formatNumber(data?.maxRows ?? 100000)} 条。只记录调用元数据与诊断信息，不保存对话正文和认证请求头；错误信息中的常见密钥格式会脱敏。历史调用无法补录。</p>
    </main>
    <Dialog open={selectedId !== null} onOpenChange={open => { if (!open) setSelectedId(null) }}>
      <DialogContent className="max-h-[90vh] max-w-4xl overflow-y-auto">
        <DialogHeader><DialogTitle>调用详情</DialogTitle><DialogDescription className="break-all font-mono">{selectedId}</DialogDescription></DialogHeader>
        {detail.isLoading ? <p>正在加载详情…</p> : detail.error ? <div className="space-y-3"><p className="text-destructive">{extractErrorMessage(detail.error)}</p><Button onClick={() => detail.refetch()}>重试</Button></div> : detail.data && <>
          <div className="flex flex-wrap items-center justify-between gap-2"><Status record={detail.data} /><Button size="sm" variant="outline" onClick={() => exportDetail(detail.data!)}><Download className="mr-2 h-4 w-4" />导出详情 JSON</Button></div>
          <dl className="grid gap-3 rounded-lg bg-muted/30 p-4 text-sm sm:grid-cols-2">
            {[
              ['时间', formatTime(detail.data.timestamp)], ['接口', `${detail.data.method} ${detail.data.path}`],
              ['模型', detail.data.model ?? '—'], ['HTTP / 模式', `${detail.data.httpStatus || '—'} / ${detail.data.stream ? '流式' : '非流式'}`],
              ['总耗时 / 首包', `${formatDuration(detail.data.durationMs)} / ${formatDuration(detail.data.firstByteMs)}`],
              ['输入 / 输出 Token', `${formatNumber(detail.data.inputTokens)} / ${formatNumber(detail.data.outputTokens)}`],
            ].map(([key, value]) => <div key={key}><dt className="text-muted-foreground">{key}</dt><dd className="mt-1 break-all">{value}</dd></div>)}
          </dl>
          {detail.data.error && <section><h3 className="mb-2 font-medium text-destructive">错误原因</h3><pre className="max-h-80 overflow-auto whitespace-pre-wrap break-words rounded-md border border-destructive/30 bg-destructive/5 p-3 text-xs">{detail.data.error}</pre></section>}
          <section><h3 className="mb-2 font-medium">上游调用（{detail.data.attempts} 次）</h3><div className="space-y-2">{detail.data.upstreamAttempts?.length ? detail.data.upstreamAttempts.map((attempt, index) => <div key={index} className="rounded-md border p-3 text-sm"><div className="flex flex-wrap justify-between gap-2"><span>#{index + 1} · 凭据 #{attempt.credentialId} · {attempt.endpoint}</span><span className="text-muted-foreground">{formatTime(attempt.timestamp)} · {attempt.status == null ? '未取得 HTTP 响应' : `HTTP ${attempt.status}`}</span></div>{attempt.upstreamRequestId && <p className="mt-2 break-all font-mono text-xs">上游请求 ID：{attempt.upstreamRequestId}</p>}{attempt.responseDiagnostics && <ResponseDiagnostics data={attempt.responseDiagnostics} />}{attempt.error && <pre className="mt-2 max-h-64 overflow-auto whitespace-pre-wrap break-words text-xs text-destructive">{attempt.error}</pre>}</div>) : <p className="text-sm text-muted-foreground">未向上游发起请求</p>}</div></section>
          <section><h3 className="mb-2 font-medium">诊断记录</h3>{detail.data.diagnostics?.length ? <div className="space-y-2">{detail.data.diagnostics.map((diagnostic, index) => <div key={index} className="rounded-md border p-3"><p className="mb-2 break-all text-xs text-muted-foreground">{formatTime(diagnostic.timestamp)} · {diagnostic.level} · {diagnostic.target}</p><pre className="max-h-64 overflow-auto whitespace-pre-wrap break-words text-xs">{diagnostic.message}</pre></div>)}</div> : <p className="text-sm text-muted-foreground">本次调用没有警告或错误</p>}</section>
        </>}
      </DialogContent>
    </Dialog>
  </div>
}

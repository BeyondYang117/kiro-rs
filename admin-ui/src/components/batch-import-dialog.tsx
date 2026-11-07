import { useMemo, useRef, useState } from 'react'
import { useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import { CheckCircle2, XCircle, AlertCircle, Loader2, Upload } from 'lucide-react'
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription, DialogFooter } from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { addCredential, getCredentials, getCredentialBalance } from '@/api/credentials'
import { extractErrorMessage, parseError, sha256Hex } from '@/lib/utils'
import { parseCredentialImport, runCredentialImport, type ImportResult, type ImportStatus } from '@/lib/credential-import'

interface BatchImportDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

const statusText: Record<ImportStatus, string> = {
  pending: '等待中', checking: '检查重复…', verifying: '导入 / 验证中…',
  verified: '导入成功，额度已验证', imported: '导入成功，未查询额度', duplicate: '重复凭据，已跳过',
  warning: '已导入，额度验证失败（已保留）', failed: '添加失败（未创建）', cancelled: '未处理，已停止',
}

function StatusIcon({ status }: { status: ImportStatus }) {
  if (status === 'checking' || status === 'verifying') return <Loader2 className="w-5 h-5 shrink-0 animate-spin text-blue-500" />
  if (status === 'verified' || status === 'imported') return <CheckCircle2 className="w-5 h-5 shrink-0 text-green-500" />
  if (status === 'failed') return <XCircle className="w-5 h-5 shrink-0 text-red-500" />
  if (status === 'warning' || status === 'duplicate') return <AlertCircle className="w-5 h-5 shrink-0 text-amber-500" />
  return <div className="w-5 h-5 shrink-0 rounded-full border-2 border-muted-foreground/40" />
}

export function BatchImportDialog({ open, onOpenChange }: BatchImportDialogProps) {
  const [jsonInput, setJsonInput] = useState('')
  const [verify, setVerify] = useState(true)
  const [importing, setImporting] = useState(false)
  const [stopping, setStopping] = useState(false)
  const [results, setResults] = useState<ImportResult[]>([])
  const [filter, setFilter] = useState<'all' | 'issues'>('all')
  const cancel = useRef(false)
  const running = useRef(false)
  const fileInput = useRef<HTMLInputElement>(null)
  const queryClient = useQueryClient()

  const { entries, parseError: inputError } = useMemo(() => {
    if (!jsonInput.trim()) return { entries: [], parseError: '' }
    try {
      return { entries: parseCredentialImport(jsonInput), parseError: '' }
    } catch (error) {
      return { entries: [], parseError: extractErrorMessage(error) }
    }
  }, [jsonInput])

  const resetForm = () => {
    setJsonInput('')
    setResults([])
    setFilter('all')
    setStopping(false)
  }

  const handleImport = async (retry = false) => {
    if (running.current || inputError || !entries.length) return
    const selected = retry
      ? entries.filter(entry => results.some(result => result.index === entry.index && ['warning', 'failed', 'cancelled'].includes(result.status)))
      : entries
    if (!selected.length) return
    running.current = true
    cancel.current = false
    setStopping(false)
    setImporting(true)
    const next: ImportResult[] = retry ? [...results] : entries.map(entry => ({ index: entry.index, email: entry.email, status: 'pending' }))
    const previous = retry ? [...results] : []
    setResults([...next])
    try {
      await runCredentialImport(selected, previous, verify, {
        getCredentials, addCredential, getBalance: getCredentialBalance, hash: sha256Hex,
        describeError: parseError,
      }, result => {
        const position = next.findIndex(item => item.index === result.index)
        next[position] = result
        setResults([...next])
      }, () => cancel.current)
      const count = (statuses: ImportStatus[]) => next.filter(item => statuses.includes(item.status)).length
      const message = `导入完成：成功 ${count(['verified', 'imported'])}，待验证 ${count(['warning'])}，重复 ${count(['duplicate'])}，失败 ${count(['failed'])}，未处理 ${count(['cancelled'])}`
      if (next.every(item => item.status === 'verified' || item.status === 'imported')) toast.success(message)
      else toast.info(message)
    } catch (error) {
      // A failed list request prevents any mutation and can be retried safely.
      setResults(retry ? previous : [])
      toast.error('读取凭据列表失败，请重试：' + extractErrorMessage(error))
    } finally {
      running.current = false
      setImporting(false)
      setStopping(false)
      void queryClient.invalidateQueries({ queryKey: ['credentials'] })
    }
  }

  const loadFile = async (file?: File) => {
    if (!file) return
    try {
      setJsonInput(await file.text())
    } catch (error) {
      toast.error('读取文件失败：' + extractErrorMessage(error))
    }
  }

  const completed = results.filter(result => !['pending', 'checking', 'verifying'].includes(result.status)).length
  const retryCount = results.filter(result => ['warning', 'failed', 'cancelled'].includes(result.status)).length
  const visibleResults = filter === 'all' ? results : results.filter(result => ['warning', 'failed', 'cancelled'].includes(result.status))
  const invalidEntries = entries.filter(entry => entry.error)

  return (
    <Dialog open={open} onOpenChange={nextOpen => {
      if (!nextOpen && running.current) return
      if (!nextOpen) resetForm()
      onOpenChange(nextOpen)
    }}>
      <DialogContent className="sm:max-w-3xl max-h-[90vh] flex flex-col">
        <DialogHeader>
          <DialogTitle>批量导入凭据</DialogTitle>
          <DialogDescription>支持 OAuth 和 API Key，自动检查重复，逐条显示导入结果。</DialogDescription>
        </DialogHeader>

        <div className="flex-1 min-h-0 overflow-y-auto space-y-4 py-2">
          <div className="space-y-2">
            <div className="flex items-center justify-between gap-2">
              <label htmlFor="batch-credentials" className="text-sm font-medium">JSON 格式凭据</label>
              <input ref={fileInput} type="file" accept=".json,application/json" className="hidden" aria-label="选择凭据 JSON 文件" disabled={importing || results.length > 0} onChange={event => {
                void loadFile(event.target.files?.[0])
                event.target.value = ''
              }} />
              <Button type="button" variant="outline" size="sm" disabled={importing || results.length > 0} onClick={() => fileInput.current?.click()}>
                <Upload className="w-4 h-4 mr-2" />选择 JSON 文件
              </Button>
            </div>
            <textarea id="batch-credentials"
              placeholder={'粘贴单个凭据、凭据数组或 {"credentials": [...]}\n\nOAuth: [{"refreshToken":"...","profileArn":"..."}]\nIdC: [{"refreshToken":"...","clientId":"...","clientSecret":"..."}]\nAPI Key: [{"kiroApiKey":"ksk_xxx"}]'}
              value={jsonInput} onChange={event => setJsonInput(event.target.value)} disabled={importing || results.length > 0}
              className="flex min-h-[160px] w-full rounded-md border border-input bg-background px-3 py-2 text-sm font-mono focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring disabled:opacity-60"
            />
            <p className="text-xs text-muted-foreground">兼容 camelCase / snake_case 字段及 accounts 导出格式，保留区域、Profile ARN、邮箱、代理和禁用状态。</p>
            {inputError && <p role="alert" className="text-sm text-red-600">JSON 解析失败：{inputError}</p>}
            {entries.length > 0 && results.length === 0 && (
              <div className="space-y-1 text-sm">
                <p>识别到 {entries.length} 个凭据，可导入 {entries.length - invalidEntries.length} 个，格式错误 {invalidEntries.length} 个。</p>
                {invalidEntries.length > 0 && <div className="max-h-24 overflow-y-auto text-xs text-red-600">
                  {invalidEntries.map(entry => <p key={entry.index}>凭据 #{entry.index}：{entry.error}</p>)}
                </div>}
              </div>
            )}
          </div>

          <div className="rounded-md border p-3 space-y-2">
            <label className="flex items-center gap-2 text-sm font-medium">
              <input type="checkbox" checked={verify} disabled={importing || results.length > 0} onChange={event => setVerify(event.target.checked)} />
              导入后验证额度
            </label>
            <p className="text-xs text-muted-foreground">OAuth 添加时会验证 Token。额度查询失败的凭据仍会保留，可重试验证；关闭此项可跳过额度查询。API Key 的有效性通过额度查询确认。</p>
          </div>

          {results.length > 0 && <>
            <div className="space-y-2" aria-live="polite">
              <div className="flex justify-between text-sm">
                <span>{importing ? stopping ? '正在停止，等待当前凭据处理完成…' : '导入中…' : results.some(result => result.status === 'cancelled') ? '导入已停止' : '导入完成'}</span>
                <span>{completed} / {results.length}</span>
              </div>
              <div role="progressbar" aria-label="导入进度" aria-valuenow={completed} aria-valuemin={0} aria-valuemax={results.length} className="w-full bg-secondary rounded-full h-2">
                <div className="bg-primary h-2 rounded-full transition-all" style={{ width: `${completed / results.length * 100}%` }} />
              </div>
            </div>
            <div className="flex flex-wrap gap-x-4 gap-y-1 text-sm">
              <span className="text-green-600">成功：{results.filter(result => ['verified', 'imported'].includes(result.status)).length}</span>
              <span className="text-amber-600">待验证：{results.filter(result => result.status === 'warning').length}</span>
              <span className="text-muted-foreground">重复：{results.filter(result => result.status === 'duplicate').length}</span>
              <span className="text-red-600">失败：{results.filter(result => result.status === 'failed').length}</span>
              <span className="text-muted-foreground">未处理：{results.filter(result => result.status === 'cancelled').length}</span>
            </div>
            <div className="flex gap-2">
              <Button size="sm" variant={filter === 'all' ? 'secondary' : 'ghost'} onClick={() => setFilter('all')}>全部</Button>
              <Button size="sm" variant={filter === 'issues' ? 'secondary' : 'ghost'} onClick={() => setFilter('issues')}>待处理（{retryCount}）</Button>
            </div>
            <div className="border rounded-md divide-y max-h-[300px] overflow-y-auto">
              {visibleResults.length === 0 && <p className="p-3 text-sm text-muted-foreground">没有待处理的凭据</p>}
              {visibleResults.map(result => <div key={result.index} className="p-3 flex items-start gap-3">
                <StatusIcon status={result.status} />
                <div className="flex-1 min-w-0 space-y-1">
                  <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
                    <span className="text-sm font-medium break-all">{result.email || `凭据 #${result.index}`}</span>
                    {result.credentialId !== undefined && <span className="text-xs text-muted-foreground">ID: {result.credentialId}</span>}
                    <span className="text-xs text-muted-foreground">{statusText[result.status]}</span>
                  </div>
                  {result.usage && <p className="text-xs text-muted-foreground">用量：{result.usage}</p>}
                  {result.error && <p className={`text-xs break-words whitespace-pre-wrap ${result.status === 'warning' ? 'text-amber-600' : 'text-red-600'}`}>{result.error}</p>}
                  {result.detail && <details className="text-xs text-muted-foreground"><summary className="cursor-pointer">错误详情</summary><p className="mt-1 whitespace-pre-wrap break-words">{result.detail}</p></details>}
                  {result.status === 'warning' && <p className="text-xs text-muted-foreground">凭据已保存。请检查区域、Profile ARN 或上游权限后重试额度验证。</p>}
                </div>
              </div>)}
            </div>
          </>}
        </div>

        <DialogFooter className="gap-2">
          {importing ? <Button variant="outline" disabled={stopping} onClick={() => { cancel.current = true; setStopping(true) }}>停止后续导入</Button> : <>
            <Button variant="outline" onClick={() => { onOpenChange(false); resetForm() }}>{results.length ? '关闭' : '取消'}</Button>
            {results.length > 0 ? <>
              <Button variant="outline" onClick={resetForm}>导入下一批</Button>
              {retryCount > 0 && <Button onClick={() => void handleImport(true)}>重试待处理（{retryCount}）</Button>}
            </> : <Button onClick={() => void handleImport()} disabled={!entries.some(entry => entry.request) || !!inputError}>{verify ? '开始导入并验证' : '开始导入'}</Button>}
          </>}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

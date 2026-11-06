import { useEffect, useState } from 'react'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import { Copy, KeyRound, Plus, RefreshCw, Trash2 } from 'lucide-react'
import { toast } from 'sonner'
import {
  addClientKey,
  deleteClientKey,
  getClientKeys,
  type ClientKey,
} from '@/api/client-keys'
import { extractErrorMessage } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Badge } from '@/components/ui/badge'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'

export function ClientKeysPanel({
  onBusyChange,
}: {
  onBusyChange: (busy: boolean) => void
}) {
  const client = useQueryClient()
  const query = useQuery({
    queryKey: ['client-keys'],
    queryFn: getClientKeys,
    refetchOnWindowFocus: false,
  })
  const [input, setInput] = useState('')
  const [busy, setBusy] = useState(false)
  const [conflict, setConflict] = useState(false)
  // 完整密钥只保存在当前弹窗状态，不放进查询缓存或本地存储。
  const [created, setCreated] = useState<string | null>(null)
  const [deleting, setDeleting] = useState<ClientKey | null>(null)
  useEffect(() => {
    onBusyChange(busy)
    return () => onBusyChange(false)
  }, [busy, onBusyChange])

  const data = query.data
  const disabled = busy || query.isFetching || !data?.canSave || conflict
  const configuredCount = data?.keys.filter((key) => key.configured).length ?? 0
  const failed = (error: unknown) => {
    if ((error as { response?: { status: number } }).response?.status === 409)
      setConflict(true)
    toast.error(`操作失败：${extractErrorMessage(error)}`)
  }
  const reload = async () => {
    const result = await query.refetch()
    if (!result.error) setConflict(false)
    else toast.error(`读取失败：${extractErrorMessage(result.error)}`)
  }
  const add = async (key?: string) => {
    if (!data || disabled) return
    setBusy(true)
    try {
      const result = await addClientKey(data.revision, key)
      client.setQueryData(['client-keys'], result.state)
      setInput('')
      setCreated(result.key)
      toast.success('调用密钥已保存，重启服务后可用')
    } catch (error) {
      failed(error)
    } finally {
      setBusy(false)
    }
  }
  const remove = async () => {
    if (!data || !deleting || disabled) return
    setBusy(true)
    try {
      const result = await deleteClientKey(deleting.id, data.revision)
      client.setQueryData(['client-keys'], result)
      setDeleting(null)
      toast.success('已从配置移除，重启服务后停止接受此密钥')
    } catch (error) {
      failed(error)
      setDeleting(null)
    } finally {
      setBusy(false)
    }
  }

  return (
    <Card>
      <CardHeader>
        <div className="flex flex-wrap items-center justify-between gap-3">
          <CardTitle className="flex items-center gap-2">
            <KeyRound className="h-5 w-5" />
            调用密钥
          </CardTitle>
          <Button
            type="button"
            variant="outline"
            size="sm"
            onClick={reload}
            disabled={busy || query.isFetching}
          >
            <RefreshCw
              className={`mr-2 h-4 w-4 ${query.isFetching ? 'animate-spin' : ''}`}
            />
            刷新密钥
          </Button>
        </div>
        <p className="text-sm text-muted-foreground">
          为不同的人分配独立调用密钥，支持 /v1 和
          /cc/v1。所有密钥共享上游账号池，管理后台使用独立的管理密钥。
        </p>
      </CardHeader>
      <CardContent className="space-y-5">
        {query.isError && (
          <p role="alert" className="text-sm text-destructive">
            读取密钥失败：{extractErrorMessage(query.error)}。请刷新重试。
          </p>
        )}
        {!data ? (
          <p role="status">
            {query.isError ? '暂时无法显示调用密钥。' : '正在读取调用密钥…'}
          </p>
        ) : (
          <>
            {!data.canSave && (
              <p className="rounded-lg border p-3 text-sm">
                当前服务未提供配置文件路径，只能查看密钥。
              </p>
            )}
            {conflict && (
              <p
                role="alert"
                className="rounded-lg border border-destructive p-3 text-sm text-destructive"
              >
                密钥配置已被其他操作修改，请刷新密钥后重试。
              </p>
            )}
            <p
              className={`rounded-lg border p-3 text-sm ${data.restartRequired ? 'border-amber-500/40 bg-amber-500/5' : ''}`}
              role="status"
            >
              {data.restartRequired
                ? '密钥变更已保存，等待服务重启生效。标记为待删除的密钥在重启前仍然有效。'
                : '当前调用密钥与运行配置一致。新增和删除均在服务重启后生效。'}
            </p>
            <div className="space-y-3 rounded-lg border p-4">
              <div className="flex flex-wrap items-center justify-between gap-3">
                <h3 className="text-sm font-medium">添加调用密钥</h3>
                <Button type="button" onClick={() => add()} disabled={disabled}>
                  <Plus className="mr-2 h-4 w-4" />
                  生成新密钥
                </Button>
              </div>
              <div className="space-y-2">
                <label htmlFor="client-key-input" className="text-sm">
                  或添加已有密钥
                </label>
                <div className="flex flex-col gap-2 sm:flex-row">
                  <Input
                    id="client-key-input"
                    type="password"
                    autoComplete="off"
                    spellCheck={false}
                    placeholder="输入已有的调用密钥"
                    value={input}
                    disabled={disabled}
                    aria-describedby="client-key-help"
                    onChange={(event) => setInput(event.target.value)}
                    onKeyDown={(event) => {
                      if (event.key === 'Enter') {
                        event.preventDefault()
                        if (input) add(input)
                      }
                    }}
                  />
                  <Button
                    type="button"
                    variant="outline"
                    disabled={disabled || !input}
                    onClick={() => add(input)}
                  >
                    添加密钥
                  </Button>
                </div>
                <p
                  id="client-key-help"
                  className="text-xs text-muted-foreground"
                >
                  仅支持非空的可打印 ASCII
                  字符，不能含空白或逗号。操作会直接保存，无需点击“保存设置”。
                </p>
              </div>
            </div>
            <div className="space-y-3">
              <h3 className="text-sm font-medium">
                已保存 {configuredCount} 个调用密钥
              </h3>
              {data.keys.length === 0 && (
                <p className="text-sm text-muted-foreground">
                  暂无调用密钥，请生成或添加一个。
                </p>
              )}
              {data.keys.map((key) => (
                <div
                  key={key.id}
                  className="flex flex-wrap items-center justify-between gap-3 rounded-lg border p-4"
                >
                  <div className="min-w-0 space-y-2">
                    <div className="flex flex-wrap items-center gap-2">
                      <code className="break-all text-sm">{key.maskedKey}</code>
                      {key.legacy && <Badge variant="outline">原 apiKey</Badge>}
                      <Badge
                        variant={
                          key.configured && key.active ? 'secondary' : 'outline'
                        }
                      >
                        {!key.configured
                          ? '待删除 · 重启生效'
                          : key.active
                            ? '使用中'
                            : '待启用 · 重启生效'}
                      </Badge>
                    </div>
                    <p className="font-mono text-xs text-muted-foreground">
                      指纹 {key.id.slice(0, 12)}
                    </p>
                  </div>
                  {key.configured && (
                    <Button
                      type="button"
                      variant="outline"
                      size="sm"
                      disabled={disabled || configuredCount <= 1}
                      onClick={() => setDeleting(key)}
                      aria-label={`删除密钥 ${key.id.slice(0, 12)}`}
                    >
                      <Trash2 className="mr-2 h-4 w-4" />
                      删除
                    </Button>
                  )}
                </div>
              ))}
              <p className="text-xs text-muted-foreground">
                至少保留一个调用密钥。列表仅展示脱敏值；请自行记录密钥指纹对应的使用者。
              </p>
            </div>
          </>
        )}
      </CardContent>
      <Dialog
        open={created !== null}
        onOpenChange={(open) => {
          if (!open) setCreated(null)
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>调用密钥已保存</DialogTitle>
            <DialogDescription>
              完整密钥仅在此处展示一次，请复制后分发给使用者。重启服务后可用。
            </DialogDescription>
          </DialogHeader>
          <Input
            aria-label="新调用密钥"
            readOnly
            value={created ?? ''}
            className="font-mono"
            onFocus={(event) => event.target.select()}
          />
          {created && (
            <p className="text-xs text-muted-foreground">
              请同时记录列表中的指纹，方便以后识别和撤销。
            </p>
          )}
          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={() => setCreated(null)}
            >
              完成
            </Button>
            <Button
              type="button"
              onClick={async () => {
                try {
                  await navigator.clipboard.writeText(created!)
                  toast.success('已复制密钥')
                } catch {
                  toast.error('无法访问剪贴板，请选中密钥手动复制')
                }
              }}
            >
              <Copy className="mr-2 h-4 w-4" />
              复制密钥
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
      <Dialog
        open={deleting !== null}
        onOpenChange={(open) => {
          if (!open && !busy) setDeleting(null)
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>删除调用密钥？</DialogTitle>
            <DialogDescription>
              将移除指纹 {deleting?.id.slice(0, 12)}{' '}
              对应的密钥，重启服务后该密钥将无法调用 API。
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              disabled={busy}
              onClick={() => setDeleting(null)}
            >
              取消
            </Button>
            <Button
              type="button"
              variant="destructive"
              disabled={disabled}
              onClick={remove}
            >
              {busy ? '正在删除…' : '确认删除'}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </Card>
  )
}

import { useQuery } from '@tanstack/react-query'
import { useState } from 'react'
import { getUsageRequest, getUsageRequests, type UsageRequest, type UsageRequestDetail } from '@/api/usage'
import { PageHeader } from '@/components/page-header'
import { Button } from '@/components/ui/button'
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '@/components/ui/dialog'
import { extractErrorMessage } from '@/lib/utils'

function money(value: number | null | undefined) {
  if (value == null) return '未定价'
  return `$${value.toFixed(4)}`
}

function duration(ms: number) {
  if (ms < 1000) return `${ms} ms`
  return `${(ms / 1000).toFixed(1)} s`
}

function pretty(text: string) {
  const trimmed = text.trim()
  if (!trimmed) return ''
  try {
    return JSON.stringify(JSON.parse(trimmed), null, 2)
  } catch {
    return text
  }
}

function textOf(value: unknown): string {
  if (typeof value === 'string') return value
  if (Array.isArray(value)) {
    return value.map((item) => {
      if (typeof item === 'string') return item
      if (!item || typeof item !== 'object') return String(item ?? '')
      const block = item as { type?: string; text?: string; thinking?: string; name?: string; input?: unknown; content?: unknown }
      if (block.type === 'thinking' || block.thinking) return `思考\n${block.thinking || block.text || ''}`
      if (block.type === 'tool_use') return `工具 ${block.name || ''}\n${pretty(JSON.stringify(block.input ?? ''))}`
      if (block.type === 'tool_result') return `工具结果\n${textOf(block.content ?? block.text ?? '')}`
      if (block.text) return block.text
      return pretty(JSON.stringify(item))
    }).filter(Boolean).join('\n\n')
  }
  if (value && typeof value === 'object') return pretty(JSON.stringify(value))
  return ''
}

function conversation(detail: UsageRequestDetail) {
  const turns: { role: string; label: string; text: string }[] = []
  let inbound: { system?: unknown; messages?: { role?: string; content?: unknown }[] } | null = null
  try {
    inbound = JSON.parse(detail.inboundBody)
  } catch {
    inbound = null
  }
  if (inbound?.system) turns.push({ role: 'system', label: '系统', text: textOf(inbound.system) })
  for (const message of inbound?.messages ?? []) {
    const role = message.role || 'user'
    turns.push({ role, label: role === 'assistant' ? '助手' : role === 'user' ? '用户' : role, text: textOf(message.content) })
  }
  const response = detail.responseBody?.trim() ?? ''
  if (response) {
    let parsed: { content?: unknown } | null = null
    try {
      parsed = JSON.parse(response)
    } catch {
      parsed = null
    }
    turns.push({
      role: 'assistant',
      label: '最终响应',
      text: parsed?.content ? textOf(parsed.content) : response,
    })
  }
  return turns
}

export function RequestsPage() {
  const [selectedId, setSelectedId] = useState<number | null>(null)
  const { data, isLoading, error, refetch, isFetching } = useQuery({
    queryKey: ['usage-requests'],
    queryFn: getUsageRequests,
  })

  return (
    <div className="space-y-5">
      <PageHeader title="请求" description="点一行打开详情。对话、入站和出站都在窗口里。">
        <Button variant="outline" size="sm" onClick={() => refetch()} disabled={isFetching}>刷新</Button>
      </PageHeader>
      {isLoading ? <p className="text-sm text-muted-foreground">加载中</p> : null}
      {error ? <p className="text-sm text-destructive">{extractErrorMessage(error)}</p> : null}
      <div className="overflow-x-auto rounded-md border">
        <table className="w-full min-w-[920px] text-left text-sm">
          <thead className="border-b bg-card text-muted-foreground">
            <tr>
              {['时间', '账号', '模型', '方式', '状态', '耗时', '输入', '输出', '费用', ''].map((label) => (
                <th key={label || 'action'} className="px-3 py-2 font-medium">{label}</th>
              ))}
            </tr>
          </thead>
          <tbody>
            {(data ?? []).map((row) => (
              <tr key={row.id} className="cursor-pointer border-b last:border-0 hover:bg-secondary/40" onClick={() => setSelectedId(row.id)}>
                <td className="whitespace-nowrap px-3 py-2">{new Date(row.time).toLocaleString()}</td>
                <td className="px-3 py-2">{row.account || '—'}</td>
                <td className="px-3 py-2">{row.model}</td>
                <td className="px-3 py-2">{row.stream ? '流式' : '非流式'}</td>
                <td className="px-3 py-2">{row.status}{row.stopReason ? <span className="ml-2 text-muted-foreground">{row.stopReason}</span> : null}</td>
                <td className="px-3 py-2">{duration(row.durationMs)}</td>
                <td className="px-3 py-2">{row.inputTokens}</td>
                <td className="px-3 py-2">{row.outputTokens}</td>
                <td className="px-3 py-2">{money(row.costUsd)}</td>
                <td className="px-3 py-2 text-right">
                  <Button size="sm" variant="outline" onClick={(event) => { event.stopPropagation(); setSelectedId(row.id) }}>详情</Button>
                </td>
              </tr>
            ))}
            {data && data.length === 0 ? (
              <tr><td className="px-3 py-8 text-muted-foreground" colSpan={10}>还没有请求记录。</td></tr>
            ) : null}
          </tbody>
        </table>
      </div>
      <RequestDialog id={selectedId} row={data?.find((item) => item.id === selectedId)} onClose={() => setSelectedId(null)} />
    </div>
  )
}

function RequestDialog({ id, row, onClose }: { id: number | null; row?: UsageRequest; onClose: () => void }) {
  const [tab, setTab] = useState<'chat' | 'inbound' | 'outbound'>('chat')
  const detail = useQuery({
    queryKey: ['usage-request', id],
    queryFn: () => getUsageRequest(id as number),
    enabled: id != null,
  })
  const turns = detail.data ? conversation(detail.data) : []

  return (
    <Dialog open={id != null} onOpenChange={(open) => { if (!open) { onClose(); setTab('chat') } }}>
      <DialogContent className="flex max-h-[86vh] w-[min(1100px,calc(100vw-2rem))] max-w-none flex-col gap-4 overflow-hidden">
        <DialogHeader>
          <DialogTitle>请求详情</DialogTitle>
          {row ? (
            <p className="text-sm text-muted-foreground">
              {row.status} · {row.model} · {row.account || '未记录账号'} · {duration(row.durationMs)} · {row.stream ? '流式' : '非流式'}
            </p>
          ) : null}
        </DialogHeader>
        {detail.isLoading ? <p className="text-sm text-muted-foreground">正在读取</p> : null}
        {detail.error ? <p className="text-sm text-destructive">{extractErrorMessage(detail.error)}</p> : null}
        {detail.data ? (
          <>
            <div className="flex gap-2">
              {([
                ['chat', '对话'],
                ['inbound', '入站'],
                ['outbound', '出站'],
              ] as const).map(([key, label]) => (
                <button key={key} type="button" className={`rounded-md px-3 py-1.5 text-sm ${tab === key ? 'bg-secondary' : 'text-muted-foreground hover:bg-secondary/60'}`} onClick={() => setTab(key)}>
                  {label}
                </button>
              ))}
            </div>
            <div className="min-h-0 flex-1 overflow-auto pr-1">
              {tab === 'chat' ? (
                <div className="space-y-3">
                  {turns.length === 0 ? <p className="text-sm text-muted-foreground">这条记录解析不出对话。可以看入站原文。</p> : null}
                  {turns.map((turn, index) => (
                    <article key={index} className="rounded-md border bg-background p-3">
                      <div className="mb-1 text-xs text-muted-foreground">{turn.label}</div>
                      <pre className="whitespace-pre-wrap font-sans text-sm leading-6">{turn.text || '空'}</pre>
                    </article>
                  ))}
                  {!detail.data.responseBody?.trim() ? (
                    <p className="text-sm text-muted-foreground">这条记录没有保存最终响应。部署这版之后的新请求才会有。</p>
                  ) : null}
                </div>
              ) : null}
              {tab === 'inbound' ? <Raw title="入站请求头" text={detail.data.inboundHeaders} bodyTitle="入站请求体" body={detail.data.inboundBody} /> : null}
              {tab === 'outbound' ? <Raw title="出站请求头" text={detail.data.outboundHeaders} bodyTitle="出站请求体" body={detail.data.outboundBody} /> : null}
            </div>
          </>
        ) : null}
      </DialogContent>
    </Dialog>
  )
}

function Raw({ title, text, bodyTitle, body }: { title: string; text: string; bodyTitle: string; body: string }) {
  return (
    <div className="space-y-4">
      <section>
        <h3 className="mb-2 text-sm font-medium">{title}</h3>
        <pre className="overflow-auto rounded-md border bg-background p-3 font-mono text-xs leading-5">{text || '没有记录'}</pre>
      </section>
      <section>
        <h3 className="mb-2 text-sm font-medium">{bodyTitle}</h3>
        <pre className="overflow-auto rounded-md border bg-background p-3 font-mono text-xs leading-5">{pretty(body) || '没有记录'}</pre>
      </section>
    </div>
  )
}

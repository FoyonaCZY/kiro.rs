import { useQuery } from '@tanstack/react-query'
import { useState } from 'react'
import { getUsageRequest, getUsageRequests } from '@/api/usage'
import { Button } from '@/components/ui/button'
import { extractErrorMessage } from '@/lib/utils'

function money(value: number | null | undefined) {
  if (value == null) return '未定价'
  return `$${value.toFixed(4)}`
}

function duration(ms: number) {
  if (ms < 1000) return `${ms} ms`
  return `${(ms / 1000).toFixed(1)} s`
}

function size(bytes: number | null | undefined) {
  if (bytes == null) return '—'
  if (bytes < 1024) return `${bytes} B`
  return `${(bytes / 1024).toFixed(1)} KiB`
}

function Block({ title, text }: { title: string; text: string }) {
  return (
    <div className="space-y-1">
      <div className="text-xs text-muted-foreground">{title}</div>
      <pre className="max-h-72 overflow-auto whitespace-pre-wrap break-all rounded-md border bg-card p-3 text-xs">{text || '没有记录'}</pre>
    </div>
  )
}

export function RequestsPage() {
  const [selectedId, setSelectedId] = useState<number | null>(null)
  const { data, isLoading, error, refetch, isFetching } = useQuery({
    queryKey: ['usage-requests'],
    queryFn: getUsageRequests,
  })
  const detail = useQuery({
    queryKey: ['usage-request', selectedId],
    queryFn: () => getUsageRequest(selectedId as number),
    enabled: selectedId != null,
  })

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between gap-3">
        <div>
          <h1 className="text-lg font-medium">请求</h1>
          <p className="text-sm text-muted-foreground">最近 200 条。点一行看请求头和请求体。密钥头已打码，单份原文最多保留 256 KiB。</p>
        </div>
        <Button variant="outline" size="sm" onClick={() => refetch()} disabled={isFetching}>
          刷新
        </Button>
      </div>

      {isLoading ? <p className="text-sm text-muted-foreground">加载中</p> : null}
      {error ? <p className="text-sm text-destructive">{extractErrorMessage(error)}</p> : null}

      <div className="overflow-x-auto rounded-md border">
        <table className="w-full min-w-[980px] text-left text-sm">
          <thead className="border-b bg-card text-muted-foreground">
            <tr>
              <th className="px-3 py-2 font-medium">时间</th>
              <th className="px-3 py-2 font-medium">账号</th>
              <th className="px-3 py-2 font-medium">模型</th>
              <th className="px-3 py-2 font-medium">方式</th>
              <th className="px-3 py-2 font-medium">状态</th>
              <th className="px-3 py-2 font-medium">耗时</th>
              <th className="px-3 py-2 font-medium">体积</th>
              <th className="px-3 py-2 font-medium">输入</th>
              <th className="px-3 py-2 font-medium">输出</th>
              <th className="px-3 py-2 font-medium">费用</th>
            </tr>
          </thead>
          <tbody>
            {(data ?? []).map((row) => (
              <tr
                key={row.id}
                className={`cursor-pointer border-b last:border-0 ${selectedId === row.id ? 'bg-card' : ''}`}
                onClick={() => setSelectedId(row.id)}
              >
                <td className="px-3 py-2 whitespace-nowrap">{new Date(row.time).toLocaleString()}</td>
                <td className="px-3 py-2" title={row.endpoint || undefined}>{row.account || '—'}</td>
                <td className="px-3 py-2">{row.model}</td>
                <td className="px-3 py-2">{row.stream ? '流式' : '非流式'}</td>
                <td className="px-3 py-2" title={row.error ?? undefined}>
                  {row.status}
                  {row.stopReason ? <span className="ml-2 text-muted-foreground">{row.stopReason}</span> : null}
                </td>
                <td className="px-3 py-2">{duration(row.durationMs)}</td>
                <td className="px-3 py-2">{size(row.requestBytes)}</td>
                <td className="px-3 py-2">{row.inputTokens}</td>
                <td className="px-3 py-2">{row.outputTokens}</td>
                <td className="px-3 py-2">{money(row.costUsd)}</td>
              </tr>
            ))}
            {data && data.length === 0 ? (
              <tr>
                <td className="px-3 py-6 text-muted-foreground" colSpan={10}>还没有请求记录。部署后，新的对话会出现在这里。</td>
              </tr>
            ) : null}
          </tbody>
        </table>
      </div>

      {selectedId != null ? (
        <div className="space-y-3">
          {detail.isLoading ? <p className="text-sm text-muted-foreground">正在读取请求原文</p> : null}
          {detail.error ? <p className="text-sm text-destructive">{extractErrorMessage(detail.error)}</p> : null}
          {detail.data ? <Detail detail={detail.data} /> : null}
        </div>
      ) : null}
    </div>
  )
}

function Detail({ detail }: { detail: Awaited<ReturnType<typeof getUsageRequest>> }) {
  return (
    <div className="space-y-3">
      <p className="text-sm text-muted-foreground">
        {detail.account || '未记录账号'} · {detail.endpoint || '未记录端点'} · 出站 {size(detail.requestBytes)}
      </p>
      <div className="grid gap-3 lg:grid-cols-2">
        <Block title="入站请求头" text={detail.inboundHeaders} />
        <Block title="出站请求头" text={detail.outboundHeaders} />
        <Block title="入站请求体" text={detail.inboundBody} />
        <Block title="出站请求体" text={detail.outboundBody} />
      </div>
    </div>
  )
}

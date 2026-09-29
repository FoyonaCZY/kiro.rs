import { useQuery } from '@tanstack/react-query'
import { useState } from 'react'
import { getUsageRequest, getUsageRequests, type UsageRequestDetail } from '@/api/usage'
import { PageHeader } from '@/components/page-header'
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

function pretty(text: string) {
  const trimmed = text.trim()
  if (!trimmed) return ''
  try {
    return JSON.stringify(JSON.parse(trimmed), null, 2)
  } catch {
    return text
  }
}

function Block({ title, text }: { title: string; text: string }) {
  const formatted = pretty(text)
  return (
    <section className="min-w-0">
      <h3 className="mb-2 text-sm font-medium">{title}</h3>
      {formatted ? (
        <pre className="max-h-[28rem] overflow-auto rounded-md border bg-background p-4 font-mono text-xs leading-5">{formatted}</pre>
      ) : (
        <p className="rounded-md border border-dashed px-4 py-6 text-sm text-muted-foreground">没有记录</p>
      )}
    </section>
  )
}

export function RequestsPage() {
  const [selectedId, setSelectedId] = useState<number | null>(null)
  const [tab, setTab] = useState<'response' | 'inbound' | 'outbound'>('response')
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
    <div className="space-y-5">
      <PageHeader title="请求" description="最近 200 条。点一行看最终响应；入站和出站原文分开。密钥头已打码，单份最多 256 KiB。">
        <Button variant="outline" size="sm" onClick={() => refetch()} disabled={isFetching}>刷新</Button>
      </PageHeader>
      {isLoading ? <p className="text-sm text-muted-foreground">加载中</p> : null}
      {error ? <p className="text-sm text-destructive">{extractErrorMessage(error)}</p> : null}
      <div className="overflow-x-auto rounded-md border">
        <table className="w-full min-w-[920px] text-left text-sm">
          <thead className="border-b bg-card text-muted-foreground">
            <tr>
              {['时间', '账号', '模型', '方式', '状态', '耗时', '输入', '输出', '费用'].map((label) => (
                <th key={label} className="px-3 py-2 font-medium">{label}</th>
              ))}
            </tr>
          </thead>
          <tbody>
            {(data ?? []).map((row) => (
              <tr
                key={row.id}
                className={`cursor-pointer border-b last:border-0 ${selectedId === row.id ? 'bg-secondary/70' : 'hover:bg-secondary/40'}`}
                onClick={() => {
                  setSelectedId(row.id)
                  setTab('response')
                }}
              >
                <td className="whitespace-nowrap px-3 py-2">{new Date(row.time).toLocaleString()}</td>
                <td className="px-3 py-2">{row.account || '—'}</td>
                <td className="px-3 py-2">{row.model}</td>
                <td className="px-3 py-2">{row.stream ? '流式' : '非流式'}</td>
                <td className="px-3 py-2" title={row.error ?? undefined}>
                  {row.status}
                  {row.stopReason ? <span className="ml-2 text-muted-foreground">{row.stopReason}</span> : null}
                </td>
                <td className="px-3 py-2">{duration(row.durationMs)}</td>
                <td className="px-3 py-2">{row.inputTokens}</td>
                <td className="px-3 py-2">{row.outputTokens}</td>
                <td className="px-3 py-2">{money(row.costUsd)}</td>
              </tr>
            ))}
            {data && data.length === 0 ? (
              <tr>
                <td className="px-3 py-8 text-muted-foreground" colSpan={9}>还没有请求记录。</td>
              </tr>
            ) : null}
          </tbody>
        </table>
      </div>
      {selectedId != null ? (
        <Detail
          loading={detail.isLoading}
          error={detail.error}
          detail={detail.data}
          tab={tab}
          onTab={setTab}
          onClose={() => setSelectedId(null)}
        />
      ) : null}
    </div>
  )
}

function Detail({
  loading,
  error,
  detail,
  tab,
  onTab,
  onClose,
}: {
  loading: boolean
  error: unknown
  detail?: UsageRequestDetail
  tab: 'response' | 'inbound' | 'outbound'
  onTab: (tab: 'response' | 'inbound' | 'outbound') => void
  onClose: () => void
}) {
  return (
    <div className="space-y-4 rounded-md border bg-card p-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h2 className="text-base font-medium">请求详情</h2>
          {detail ? (
            <p className="mt-1 text-sm text-muted-foreground">
              {detail.account || '未记录账号'} · {detail.endpoint || '未记录端点'} · {detail.stream ? '流式正文' : '完整响应'}
            </p>
          ) : null}
        </div>
        <Button variant="ghost" size="sm" onClick={onClose}>关闭</Button>
      </div>
      {loading ? <p className="text-sm text-muted-foreground">正在读取</p> : null}
      {error ? <p className="text-sm text-destructive">{extractErrorMessage(error)}</p> : null}
      {detail ? (
        <>
          <div className="flex gap-2">
            {([
              ['response', '最终响应'],
              ['inbound', '入站'],
              ['outbound', '出站'],
            ] as const).map(([id, label]) => (
              <button
                key={id}
                type="button"
                className={`rounded-md px-3 py-1.5 text-sm ${tab === id ? 'bg-secondary' : 'text-muted-foreground hover:bg-secondary/60'}`}
                onClick={() => onTab(id)}
              >
                {label}
              </button>
            ))}
          </div>
          {tab === 'response' ? (
            detail.responseBody?.trim() ? (
              <Block title="返回给客户端的内容" text={detail.responseBody} />
            ) : (
              <p className="rounded-md border border-dashed px-4 py-6 text-sm text-muted-foreground">这条记录没有保存最终响应。部署这版之后的新请求才会有。</p>
            )
          ) : null}
          {tab === 'inbound' ? (
            <div className="grid gap-4">
              <Block title="入站请求头" text={detail.inboundHeaders} />
              <Block title="入站请求体" text={detail.inboundBody} />
            </div>
          ) : null}
          {tab === 'outbound' ? (
            <div className="grid gap-4">
              <Block title="出站请求头" text={detail.outboundHeaders} />
              <Block title="出站请求体" text={detail.outboundBody} />
            </div>
          ) : null}
        </>
      ) : null}
    </div>
  )
}

import { useQuery } from '@tanstack/react-query'
import { getUsageRequests } from '@/api/usage'
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

export function RequestsPage() {
  const { data, isLoading, error, refetch, isFetching } = useQuery({
    queryKey: ['usage-requests'],
    queryFn: getUsageRequests,
  })

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between gap-3">
        <div>
          <h1 className="text-lg font-medium">请求</h1>
          <p className="text-sm text-muted-foreground">最近 200 条。token 按返回文本估算，费用按已配置单价计算。</p>
        </div>
        <Button variant="outline" size="sm" onClick={() => refetch()} disabled={isFetching}>
          刷新
        </Button>
      </div>

      {isLoading ? <p className="text-sm text-muted-foreground">加载中</p> : null}
      {error ? <p className="text-sm text-destructive">{extractErrorMessage(error)}</p> : null}

      <div className="overflow-x-auto rounded-md border">
        <table className="w-full min-w-[860px] text-left text-sm">
          <thead className="border-b bg-card text-muted-foreground">
            <tr>
              <th className="px-3 py-2 font-medium">时间</th>
              <th className="px-3 py-2 font-medium">模型</th>
              <th className="px-3 py-2 font-medium">方式</th>
              <th className="px-3 py-2 font-medium">状态</th>
              <th className="px-3 py-2 font-medium">耗时</th>
              <th className="px-3 py-2 font-medium">输入</th>
              <th className="px-3 py-2 font-medium">输出</th>
              <th className="px-3 py-2 font-medium">费用</th>
            </tr>
          </thead>
          <tbody>
            {(data ?? []).map((row) => (
              <tr key={row.id} className="border-b last:border-0">
                <td className="px-3 py-2 whitespace-nowrap">{new Date(row.time).toLocaleString()}</td>
                <td className="px-3 py-2">{row.model}</td>
                <td className="px-3 py-2">{row.stream ? '流式' : '非流式'}</td>
                <td className="px-3 py-2" title={row.error ?? undefined}>{row.status}</td>
                <td className="px-3 py-2">{duration(row.durationMs)}</td>
                <td className="px-3 py-2">{row.inputTokens}</td>
                <td className="px-3 py-2">{row.outputTokens}</td>
                <td className="px-3 py-2">{money(row.costUsd)}</td>
              </tr>
            ))}
            {data && data.length === 0 ? (
              <tr>
                <td className="px-3 py-6 text-muted-foreground" colSpan={8}>还没有请求记录。部署后，新的对话会出现在这里。</td>
              </tr>
            ) : null}
          </tbody>
        </table>
      </div>
    </div>
  )
}

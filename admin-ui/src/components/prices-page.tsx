import { useState } from 'react'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import { deletePrice, getPrices, getUsageSummary, savePrice, type ModelPrice } from '@/api/usage'
import { PageHeader } from '@/components/page-header'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Input } from '@/components/ui/input'
import { extractErrorMessage } from '@/lib/utils'

function money(value: number) {
  return `$${value.toFixed(4)}`
}

export function PricesPage() {
  const queryClient = useQueryClient()
  const summary = useQuery({ queryKey: ['usage-summary'], queryFn: getUsageSummary })
  const prices = useQuery({ queryKey: ['usage-prices'], queryFn: getPrices })
  const [name, setName] = useState('')
  const [aliases, setAliases] = useState('')
  const [inputPerM, setInputPerM] = useState('')
  const [outputPerM, setOutputPerM] = useState('')
  const [saving, setSaving] = useState(false)

  const refresh = () => {
    queryClient.invalidateQueries({ queryKey: ['usage-summary'] })
    queryClient.invalidateQueries({ queryKey: ['usage-prices'] })
    queryClient.invalidateQueries({ queryKey: ['usage-requests'] })
  }

  const submit = async () => {
    const price: ModelPrice = {
      id: '',
      name: name.trim(),
      aliases: aliases.split(',').map((item) => item.trim()).filter(Boolean),
      inputPerM: Number(inputPerM),
      outputPerM: Number(outputPerM),
    }
    if (!price.name || price.aliases.length === 0 || Number.isNaN(price.inputPerM) || Number.isNaN(price.outputPerM)) {
      toast.error('名称、别名和单价都要填')
      return
    }
    setSaving(true)
    try {
      await savePrice(price)
      setName('')
      setAliases('')
      setInputPerM('')
      setOutputPerM('')
      refresh()
      toast.success('已保存定价')
    } catch (error) {
      toast.error(extractErrorMessage(error))
    } finally {
      setSaving(false)
    }
  }

  const remove = async (price: ModelPrice) => {
    try {
      await deletePrice(price.id)
      refresh()
    } catch (error) {
      toast.error(extractErrorMessage(error))
    }
  }

  const totals = summary.data

  return (
    <div className="space-y-6">
      <PageHeader title="价格" description="单价是每百万 token 的美元价格。缓存不计入。未匹配到别名的请求费用显示为未定价。" />

      <div className="grid gap-4 md:grid-cols-4">
        <Card>
          <CardHeader className="pb-2"><CardTitle className="text-sm font-medium text-muted-foreground">请求</CardTitle></CardHeader>
          <CardContent className="text-2xl font-medium">{totals?.requests ?? '—'}</CardContent>
        </Card>
        <Card>
          <CardHeader className="pb-2"><CardTitle className="text-sm font-medium text-muted-foreground">失败</CardTitle></CardHeader>
          <CardContent className="text-2xl font-medium">{totals?.errors ?? '—'}</CardContent>
        </Card>
        <Card>
          <CardHeader className="pb-2"><CardTitle className="text-sm font-medium text-muted-foreground">输入 / 输出</CardTitle></CardHeader>
          <CardContent className="text-2xl font-medium">{totals ? `${totals.inputTokens} / ${totals.outputTokens}` : '—'}</CardContent>
        </Card>
        <Card>
          <CardHeader className="pb-2"><CardTitle className="text-sm font-medium text-muted-foreground">估算费用</CardTitle></CardHeader>
          <CardContent className="text-2xl font-medium">{totals ? money(totals.costUsd) : '—'}</CardContent>
        </Card>
      </div>

      {totals && totals.unpricedRequests > 0 ? (
        <p className="text-sm text-muted-foreground">{totals.unpricedRequests} 条请求没有匹配到单价，不计入费用。</p>
      ) : null}

      <div className="grid gap-3 md:grid-cols-4">
        <Input placeholder="名称，例如 Opus" value={name} onChange={(event) => setName(event.target.value)} />
        <Input placeholder="别名，逗号分隔" value={aliases} onChange={(event) => setAliases(event.target.value)} />
        <Input placeholder="输入 $/1M" value={inputPerM} onChange={(event) => setInputPerM(event.target.value)} />
        <div className="flex gap-2">
          <Input placeholder="输出 $/1M" value={outputPerM} onChange={(event) => setOutputPerM(event.target.value)} />
          <Button onClick={submit} disabled={saving}>保存</Button>
        </div>
      </div>

      <div className="overflow-x-auto rounded-md border">
        <table className="w-full text-left text-sm">
          <thead className="border-b bg-card text-muted-foreground">
            <tr>
              <th className="px-3 py-2 font-medium">名称</th>
              <th className="px-3 py-2 font-medium">别名</th>
              <th className="px-3 py-2 font-medium">输入</th>
              <th className="px-3 py-2 font-medium">输出</th>
              <th className="px-3 py-2 font-medium"></th>
            </tr>
          </thead>
          <tbody>
            {(prices.data ?? []).map((price) => (
              <tr key={price.id} className="border-b last:border-0">
                <td className="px-3 py-2">{price.name}</td>
                <td className="px-3 py-2">{price.aliases.join(', ')}</td>
                <td className="px-3 py-2">{price.inputPerM}</td>
                <td className="px-3 py-2">{price.outputPerM}</td>
                <td className="px-3 py-2 text-right">
                  <Button variant="ghost" size="sm" onClick={() => remove(price)}>删除</Button>
                </td>
              </tr>
            ))}
            {prices.data && prices.data.length === 0 ? (
              <tr><td className="px-3 py-6 text-muted-foreground" colSpan={5}>还没有定价。</td></tr>
            ) : null}
          </tbody>
        </table>
      </div>

      {totals && totals.byModel.length > 0 ? (
        <div className="overflow-x-auto rounded-md border">
          <table className="w-full text-left text-sm">
            <thead className="border-b bg-card text-muted-foreground">
              <tr>
                <th className="px-3 py-2 font-medium">模型</th>
                <th className="px-3 py-2 font-medium">请求</th>
                <th className="px-3 py-2 font-medium">失败</th>
                <th className="px-3 py-2 font-medium">输入</th>
                <th className="px-3 py-2 font-medium">输出</th>
                <th className="px-3 py-2 font-medium">费用</th>
              </tr>
            </thead>
            <tbody>
              {totals.byModel.map((row) => (
                <tr key={row.model} className="border-b last:border-0">
                  <td className="px-3 py-2">{row.model}</td>
                  <td className="px-3 py-2">{row.requests}</td>
                  <td className="px-3 py-2">{row.errors}</td>
                  <td className="px-3 py-2">{row.inputTokens}</td>
                  <td className="px-3 py-2">{row.outputTokens}</td>
                  <td className="px-3 py-2">{money(row.costUsd)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : null}
    </div>
  )
}

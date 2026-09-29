import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useState } from 'react'
import {
  createAccessGroup,
  createAccessKey,
  deleteAccessGroup,
  deleteAccessKey,
  getAccessGroups,
  getAccessKeys,
  renameAccessGroup,
  setGroupMembers,
  updateAccessKey,
} from '@/api/access'
import { getCredentials } from '@/api/credentials'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { extractErrorMessage } from '@/lib/utils'

export function AccessPage() {
  const queryClient = useQueryClient()
  const keys = useQuery({ queryKey: ['access-keys'], queryFn: getAccessKeys })
  const groups = useQuery({ queryKey: ['access-groups'], queryFn: getAccessGroups })
  const credentials = useQuery({ queryKey: ['credentials'], queryFn: getCredentials })
  const [name, setName] = useState('')
  const [groupId, setGroupId] = useState('default')
  const [groupName, setGroupName] = useState('')
  const [selectedGroup, setSelectedGroup] = useState('default')
  const [revealed, setRevealed] = useState<number | null>(null)
  const [error, setError] = useState('')

  function refresh() {
    queryClient.invalidateQueries({ queryKey: ['access-keys'] })
    queryClient.invalidateQueries({ queryKey: ['access-groups'] })
  }

  const addKey = useMutation({
    mutationFn: () => createAccessKey({ name, groupId }),
    onSuccess: () => {
      setName('')
      setError('')
      refresh()
    },
    onError: (err) => setError(extractErrorMessage(err)),
  })
  const addGroup = useMutation({
    mutationFn: () => createAccessGroup(groupName),
    onSuccess: () => {
      setGroupName('')
      setError('')
      refresh()
    },
    onError: (err) => setError(extractErrorMessage(err)),
  })

  const activeGroup = groups.data?.find((group) => group.id === selectedGroup) ?? groups.data?.[0]
  const memberSet = new Set(activeGroup?.members ?? [])

  return (
    <div className="space-y-8">
      <div>
        <h1 className="text-lg font-medium">接入</h1>
        <p className="text-sm text-muted-foreground">每把 Key 绑定一个调度分组。默认分组包含还没单独分配的凭据。初始 apiKey 会成为第一把默认 Key。</p>
      </div>
      {error ? <p className="text-sm text-destructive">{error}</p> : null}

      <section className="space-y-3">
        <h2 className="text-sm font-medium">接入 Key</h2>
        <form
          className="flex flex-wrap gap-2"
          onSubmit={(event) => {
            event.preventDefault()
            addKey.mutate()
          }}
        >
          <Input value={name} onChange={(event) => setName(event.target.value)} placeholder="名称" className="max-w-48" />
          <select className="rounded-md border bg-background px-2 text-sm" value={groupId} onChange={(event) => setGroupId(event.target.value)}>
            {(groups.data ?? []).map((group) => (
              <option key={group.id} value={group.id}>{group.name}</option>
            ))}
          </select>
          <Button type="submit" size="sm" disabled={!name.trim() || addKey.isPending}>添加</Button>
        </form>
        <div className="overflow-x-auto rounded-md border">
          <table className="w-full min-w-[720px] text-left text-sm">
            <thead className="border-b bg-card text-muted-foreground">
              <tr>
                <th className="px-3 py-2 font-medium">名称</th>
                <th className="px-3 py-2 font-medium">Key</th>
                <th className="px-3 py-2 font-medium">分组</th>
                <th className="px-3 py-2 font-medium">状态</th>
                <th className="px-3 py-2 font-medium"></th>
              </tr>
            </thead>
            <tbody>
              {(keys.data ?? []).map((key) => (
                <tr key={key.id} className="border-b last:border-0">
                  <td className="px-3 py-2">{key.name}</td>
                  <td className="px-3 py-2 font-mono text-xs">
                    {revealed === key.id ? key.secret : key.prefix}
                    <button type="button" className="ml-2 text-muted-foreground" onClick={() => setRevealed(revealed === key.id ? null : key.id)}>
                      {revealed === key.id ? '隐藏' : '显示'}
                    </button>
                  </td>
                  <td className="px-3 py-2">
                    <select
                      className="rounded-md border bg-background px-2 py-1"
                      value={key.groupId}
                      onChange={(event) => updateAccessKey(key.id, { groupId: event.target.value }).then(refresh).catch((err) => setError(extractErrorMessage(err)))}
                    >
                      {(groups.data ?? []).map((group) => (
                        <option key={group.id} value={group.id}>{group.name}</option>
                      ))}
                    </select>
                  </td>
                  <td className="px-3 py-2">{key.disabled ? '已停用' : '启用'}</td>
                  <td className="px-3 py-2 text-right">
                    <Button
                      variant="ghost"
                      size="sm"
                      onClick={() => updateAccessKey(key.id, { disabled: !key.disabled }).then(refresh).catch((err) => setError(extractErrorMessage(err)))}
                    >
                      {key.disabled ? '启用' : '停用'}
                    </Button>
                    <Button
                      variant="ghost"
                      size="sm"
                      onClick={() => deleteAccessKey(key.id).then(refresh).catch((err) => setError(extractErrorMessage(err)))}
                    >
                      删除
                    </Button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </section>

      <section className="space-y-3">
        <h2 className="text-sm font-medium">调度分组</h2>
        <form
          className="flex gap-2"
          onSubmit={(event) => {
            event.preventDefault()
            addGroup.mutate()
          }}
        >
          <Input value={groupName} onChange={(event) => setGroupName(event.target.value)} placeholder="新分组名称" className="max-w-48" />
          <Button type="submit" size="sm" disabled={!groupName.trim() || addGroup.isPending}>添加分组</Button>
        </form>
        <div className="flex flex-wrap gap-2">
          {(groups.data ?? []).map((group) => (
            <button
              key={group.id}
              type="button"
              className={`rounded-md border px-3 py-1 text-sm ${activeGroup?.id === group.id ? 'bg-secondary' : ''}`}
              onClick={() => setSelectedGroup(group.id)}
            >
              {group.name} · {group.keyCount} 把 Key
            </button>
          ))}
        </div>
        {activeGroup ? (
          <div className="space-y-3 rounded-md border p-3">
            <div className="flex flex-wrap items-center gap-2">
              <Input
                key={activeGroup.id}
                defaultValue={activeGroup.name}
                className="max-w-48"
                onBlur={(event) => {
                  const next = event.target.value.trim()
                  if (next && next !== activeGroup.name) {
                    renameAccessGroup(activeGroup.id, next).then(refresh).catch((err) => setError(extractErrorMessage(err)))
                  }
                }}
              />
              {activeGroup.isDefault ? <span className="text-xs text-muted-foreground">默认分组不能删除</span> : (
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => deleteAccessGroup(activeGroup.id).then(() => {
                    setSelectedGroup('default')
                    refresh()
                  }).catch((err) => setError(extractErrorMessage(err)))}
                >
                  删除分组
                </Button>
              )}
            </div>
            <div className="grid gap-2 sm:grid-cols-2">
              {(credentials.data?.credentials ?? []).map((credential) => {
                const checked = memberSet.has(credential.id)
                const label = credential.email || `#${credential.id}`
                return (
                  <label key={credential.id} className="flex items-center gap-2 text-sm">
                    <input
                      type="checkbox"
                      checked={checked}
                      onChange={(event) => {
                        const next = new Set(memberSet)
                        if (event.target.checked) next.add(credential.id)
                        else next.delete(credential.id)
                        setGroupMembers(activeGroup.id, [...next]).then(refresh).catch((err) => setError(extractErrorMessage(err)))
                      }}
                    />
                    <span>{label}</span>
                  </label>
                )
              })}
            </div>
          </div>
        ) : null}
      </section>
    </div>
  )
}

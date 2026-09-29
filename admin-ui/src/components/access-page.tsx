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
  type AccessGroup,
} from '@/api/access'
import { getCredentials } from '@/api/credentials'
import { PageHeader } from '@/components/page-header'
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
  const [selectedGroup, setSelectedGroup] = useState<string | null>(null)
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
    onSuccess: (group) => {
      setGroupName('')
      setSelectedGroup(group.id)
      setError('')
      refresh()
    },
    onError: (err) => setError(extractErrorMessage(err)),
  })

  const active = groups.data?.find((group) => group.id === selectedGroup) ?? null

  return (
    <div className="space-y-8">
      <PageHeader title="接入" description="一把 Key 只从它的调度分组里拿号。没进任何分组的账号属于默认组；移出最后一个组后也会回到默认组。" />
      {error ? <p className="text-sm text-destructive">{error}</p> : null}

      <section className="space-y-3">
        <h2 className="text-sm font-medium">调度分组</h2>
        <form className="flex flex-wrap gap-2" onSubmit={(event) => { event.preventDefault(); addGroup.mutate() }}>
          <Input value={groupName} onChange={(event) => setGroupName(event.target.value)} placeholder="新分组名称" className="max-w-56" />
          <Button type="submit" size="sm" disabled={!groupName.trim() || addGroup.isPending}>新建</Button>
        </form>
        <div className="overflow-x-auto rounded-md border">
          <table className="w-full text-left text-sm">
            <thead className="border-b bg-card text-muted-foreground">
              <tr>
                <th className="px-3 py-2 font-medium">名称</th>
                <th className="px-3 py-2 font-medium">账号</th>
                <th className="px-3 py-2 font-medium">Key</th>
                <th className="px-3 py-2 font-medium"></th>
              </tr>
            </thead>
            <tbody>
              {(groups.data ?? []).map((group) => (
                <tr key={group.id} className="cursor-pointer border-b last:border-0 hover:bg-secondary/40" onClick={() => setSelectedGroup(group.id)}>
                  <td className="px-3 py-2">{group.name}{group.isDefault ? <span className="ml-2 text-xs text-muted-foreground">默认</span> : null}</td>
                  <td className="px-3 py-2">{group.members.length}</td>
                  <td className="px-3 py-2">{group.keyCount}</td>
                  <td className="px-3 py-2 text-right text-muted-foreground">管理</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
        {active ? <GroupEditor group={active} credentials={credentials.data?.credentials ?? []} onClose={() => setSelectedGroup(null)} onChanged={refresh} onError={setError} /> : null}
      </section>

      <section className="space-y-3">
        <h2 className="text-sm font-medium">接入 Key</h2>
        <form className="flex flex-wrap gap-2" onSubmit={(event) => { event.preventDefault(); addKey.mutate() }}>
          <Input value={name} onChange={(event) => setName(event.target.value)} placeholder="名称" className="max-w-48" />
          <select className="h-9 rounded-md border bg-background px-2 text-sm" value={groupId} onChange={(event) => setGroupId(event.target.value)}>
            {(groups.data ?? []).map((group) => (
              <option key={group.id} value={group.id}>{group.name}</option>
            ))}
          </select>
          <Button type="submit" size="sm" disabled={!name.trim() || addKey.isPending}>添加</Button>
        </form>
        <div className="overflow-x-auto rounded-md border">
          <table className="w-full min-w-[680px] text-left text-sm">
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
                      className="h-8 rounded-md border bg-background px-2"
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
                    <Button variant="ghost" size="sm" onClick={() => updateAccessKey(key.id, { disabled: !key.disabled }).then(refresh).catch((err) => setError(extractErrorMessage(err)))}>
                      {key.disabled ? '启用' : '停用'}
                    </Button>
                    <Button variant="ghost" size="sm" onClick={() => deleteAccessKey(key.id).then(refresh).catch((err) => setError(extractErrorMessage(err)))}>删除</Button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </section>
    </div>
  )
}

function GroupEditor({
  group,
  credentials,
  onClose,
  onChanged,
  onError,
}: {
  group: AccessGroup
  credentials: Array<{ id: number; email?: string | null }>
  onClose: () => void
  onChanged: () => void
  onError: (message: string) => void
}) {
  const members = new Set(group.members)
  return (
    <div className="space-y-3 rounded-md border bg-card p-4">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <Input
          key={group.id}
          defaultValue={group.name}
          className="max-w-56"
          onBlur={(event) => {
            const next = event.target.value.trim()
            if (next && next !== group.name) {
              renameAccessGroup(group.id, next).then(onChanged).catch((err) => onError(extractErrorMessage(err)))
            }
          }}
        />
        <div className="flex gap-2">
          {group.isDefault ? <span className="self-center text-xs text-muted-foreground">默认分组不能删除</span> : (
            <Button variant="outline" size="sm" onClick={() => deleteAccessGroup(group.id).then(() => { onClose(); onChanged() }).catch((err) => onError(extractErrorMessage(err)))}>删除分组</Button>
          )}
          <Button variant="ghost" size="sm" onClick={onClose}>关闭</Button>
        </div>
      </div>
      <p className="text-sm text-muted-foreground">勾选的账号才会被这个分组调度。取消最后一个分组后，账号回到默认组。</p>
      <div className="grid gap-2 sm:grid-cols-2">
        {credentials.map((credential) => (
          <label key={credential.id} className="flex items-center gap-2 rounded-md border px-3 py-2 text-sm">
            <input
              type="checkbox"
              checked={members.has(credential.id)}
              onChange={(event) => {
                const next = new Set(members)
                if (event.target.checked) next.add(credential.id)
                else next.delete(credential.id)
                setGroupMembers(group.id, [...next]).then(onChanged).catch((err) => onError(extractErrorMessage(err)))
              }}
            />
            <span>{credential.email || `#${credential.id}`}</span>
          </label>
        ))}
        {credentials.length === 0 ? <p className="text-sm text-muted-foreground">还没有账号。</p> : null}
      </div>
    </div>
  )
}

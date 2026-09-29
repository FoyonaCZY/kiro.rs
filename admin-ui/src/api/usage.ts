import axios from 'axios'
import { storage } from '@/lib/storage'

const api = axios.create({
  baseURL: '/api/admin',
  headers: { 'Content-Type': 'application/json' },
})

api.interceptors.request.use((config) => {
  const apiKey = storage.getApiKey()
  if (apiKey) {
    config.headers['x-api-key'] = apiKey
  }
  return config
})

export interface UsageRequest {
  id: number
  time: string
  model: string
  stream: boolean
  status: number
  durationMs: number
  inputTokens: number
  outputTokens: number
  error?: string | null
  costUsd?: number | null
  account?: string
  endpoint?: string
  requestBytes?: number
  stopReason?: string
}

export interface UsageRequestDetail extends UsageRequest {
  inboundHeaders: string
  inboundBody: string
  outboundHeaders: string
  outboundBody: string
  responseBody?: string
}

export interface ModelUsage {
  model: string
  requests: number
  errors: number
  inputTokens: number
  outputTokens: number
  costUsd: number
}

export interface UsageSummary {
  requests: number
  errors: number
  inputTokens: number
  outputTokens: number
  costUsd: number
  unpricedRequests: number
  byModel: ModelUsage[]
}

export interface ModelPrice {
  id: string
  name: string
  aliases: string[]
  inputPerM: number
  outputPerM: number
}

export async function getUsageRequests(): Promise<UsageRequest[]> {
  const { data } = await api.get<UsageRequest[]>('/usage/requests')
  return data
}

export async function getUsageRequest(id: number): Promise<UsageRequestDetail> {
  const { data } = await api.get<UsageRequestDetail>(`/usage/requests/${id}`)
  return data
}

export async function getUsageSummary(): Promise<UsageSummary> {
  const { data } = await api.get<UsageSummary>('/usage/summary')
  return data
}

export async function getPrices(): Promise<ModelPrice[]> {
  const { data } = await api.get<ModelPrice[]>('/usage/prices')
  return data
}

export async function savePrice(price: ModelPrice): Promise<ModelPrice> {
  const { data } = await api.post<ModelPrice>('/usage/prices', price)
  return data
}

export async function deletePrice(id: string): Promise<void> {
  await api.delete(`/usage/prices/${encodeURIComponent(id)}`)
}

// Explicit administrator actions. These intentionally use the shared API
// caller so failures are the node's own bounded, authenticated explanation.

import { call } from './api'
import type { NodeInfo } from './types'

export interface AdminAccess {
  authenticated: boolean
  token_configured: boolean
  local: boolean
}

export interface AdminCompatibility {
  runtime: 'compatible' | 'mismatch' | 'not_found' | 'unknown'
  pinned: string | null
  found: string | null
  checked_at_ms: number | null
  models: { state: 'offered' | 'none_offered' | 'unknown'; offered: number | null }
  application: CompatibilityFact<string>
  wire: CompatibilityFact<number>
  policy: PolicyCompatibility
}

export interface CompatibilityFact<T> {
  state: 'local' | 'compatible' | 'mismatch' | 'unknown'
  local: T | null
  peer: T | null
  upgrade_needed?: boolean
}

export interface PolicyCompatibility {
  identity: CompatibilityFact<string>
  bundle: CompatibilityFact<string>
  receipt: {
    state: 'supported' | 'unsupported' | 'unknown_or_legacy'
    advertised: boolean | null
    upgrade_needed: boolean
  }
}

export interface AdminMember {
  id: string
  name: string
  self: boolean
  reachable: boolean
  last_seen_ms: number | null
  channels: string[]
  compatibility: AdminCompatibility
}

export interface AdminChannel {
  name: string
  nodes: string[]
}

export interface MeshAdministration {
  local: boolean
  inventory_source: string
  mesh: unknown | null
  members: AdminMember[]
  channels: AdminChannel[]
  capabilities: {
    invite: boolean
    remove_member: boolean
    share_existing_channel_with_hub: boolean
    edit_member_channels: { available: false; reason: string }
  }
}

export interface MeshInvitation {
  code: string
  display_code: string
  url: string
  qr_svg: string | null
  channels: string[]
  expires_at: number
  state: 'waiting' | 'received' | 'admitted'
  received: { node_id: string; name: string } | null
  received_fingerprint: string | null
  own_fingerprint: string | null
}

export interface LifecycleCapability {
  available: boolean
  reason: string
  recovery: string
}

export interface Maintenance {
  local: boolean
  active_sessions: number
  service: {
    platform: 'linux' | 'macos'
    supervisor: string
    container: string | null
    unit_path: string | null
    unit_installed: boolean
    state: { state: 'running' | 'stopped' | 'unknown'; detail: string }
    install: LifecycleCapability
    uninstall: LifecycleCapability
    restart: LifecycleCapability
  }
  boundary: NodeInfo
  mesh: unknown | null
}

const invitationPath = (code: string) => `/api/admin/mesh/invitations/${encodeURIComponent(code)}`

export const admin = {
  access: () => call<AdminAccess>('GET', '/api/admin/access'),
  login: (token: string) => call<{ ok: boolean }>('POST', '/api/login', { token }),
  mesh: () => call<MeshAdministration>('GET', '/api/admin/mesh'),
  createInvitation: (channels: string[], ttlSecs?: number) =>
    call<MeshInvitation>('POST', '/api/admin/mesh/invitations', {
      channels,
      ...(ttlSecs === undefined ? {} : { ttl_secs: ttlSecs }),
    }),
  pollInvitation: (code: string) => call<MeshInvitation>('GET', invitationPath(code)),
  admitInvitation: (code: string) =>
    call<{ invitation: MeshInvitation; effect: string; directory_refreshed: boolean; directory_refresh_error: string | null }>(
      'POST',
      `${invitationPath(code)}/admit`,
      { confirm: true },
    ),
  cancelInvitation: (code: string) => call<{ cancelled: string }>('DELETE', invitationPath(code)),
  removeMember: (id: string) =>
    call<{
      removed: string
      effect: string
      data_not_retracted: true
      directory_refreshed: boolean
      directory_refresh_error: string | null
    }>('DELETE', `/api/admin/mesh/members/${encodeURIComponent(id)}`, { confirm: true }),
  shareWithHub: (channels: string[]) =>
    call<{ shared: string[]; effect: string }>('POST', '/api/admin/mesh/hub-share', {
      channels,
      confirm: true,
    }),
  maintenance: () => call<Maintenance>('GET', '/api/admin/maintenance'),
  boundaryCheck: () => call<unknown>('POST', '/api/admin/maintenance/boundary-check'),
  install: () =>
    call<{ scheduled: true; operation: 'install'; target: string; effect: string }>('POST', '/api/admin/maintenance/install', {
      confirm: true,
    }),
  uninstall: () =>
    call<{ scheduled: true; operation: 'uninstall'; target: string; effect: string }>('POST', '/api/admin/maintenance/uninstall', {
      confirm: true,
    }),
  restart: () =>
    call<{ scheduled: true; operation: 'restart'; target: string; effect: string }>('POST', '/api/admin/maintenance/restart', {
      confirm: true,
    }),
}

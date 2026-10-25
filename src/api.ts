import { invoke } from '@tauri-apps/api/core';
import type {
  AccountSummary,
  AuthState,
  CreditStore,
  ProbeResult,
  CheckinRecord,
  CheckinStore,
  LimitSignal,
  Ownership,
  Probe,
  ProfileStore,
  SwitchPlan,
  TaskRow,
  Transcript,
  UnifyPlan,
  UnifyReport,
} from './types';

/** 在浏览器里跑 `vite dev` 时没有 Tauri IPC，退回示例数据以便调样式。 */
export const isTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;

const MOCK_TASKS: TaskRow[] = [
  { id: 'abffe1ce', uid: 'db0427fd-b175-497c-85a0-c01d8f35cb13', short_uid: 'db0427fd', cwd: 'C:/Users/Administrator/WorkBuddy/2026-10-21-16-11-34', title: '切换账号保留任务', status: 'working', created_at: 1792570294, updated_at: 1792572368 },
  { id: '9ad10b58', uid: '8bc5fd09-3730-4a8a-9182-6dadb2a1b058', short_uid: '8bc5fd09', cwd: 'C:/Users/Administrator/WorkBuddy/2026-10-19-19-49-13', title: '无法识别的会话内容', status: 'completed', created_at: 1792400000, updated_at: 1792410585 },
  { id: '95ab5ada', uid: '8bc5fd09-3730-4a8a-9182-6dadb2a1b058', short_uid: '8bc5fd09', cwd: 'D:\\workbuddy_p\\yuyin', title: '了解鸿蒙全命令流程', status: 'completed', created_at: 1792398000, updated_at: 1792398645 },
  { id: 'dcea4602', uid: '697af213-6a63-4edb-ad9c-1ff588ee434d', short_uid: '697af213', cwd: 'C:/Users/Administrator/WorkBuddy/2026-10-18-15-46-15', title: '讨论数字11的含义', status: 'completed', created_at: 1792319175, updated_at: 1792319194 },
  { id: '9bb9bbc1', uid: '4dba68a4-797f-4a97-a7e0-a23b261a3d48', short_uid: '4dba68a4', cwd: 'C:/Users/Administrator/WorkBuddy/2026-10-18-13-07-08', title: '用户输入数字11', status: 'error', created_at: 1792309628, updated_at: 1792309657 },
  { id: '8515eb83', uid: '8bc5fd09-3730-4a8a-9182-6dadb2a1b058', short_uid: '8bc5fd09', cwd: 'D:\\workbuddy_p\\apple', title: '整理 /api/apple/start 记录到新 txt', status: 'completed', created_at: 1791946740, updated_at: 1791946777 },
];

const MOCK_ACCOUNTS: AccountSummary[] = [
  ['128a8f9c-9167-46e5-86e7-a61fa5e766ce', 1794734516, 1794907316],
  ['438b979e-2c4f-4b21-983c-d2e0081ce5b5', 1794896888, 1795069688],
  ['4dba68a4-797f-4a97-a7e0-a23b261a3d48', 1794735570, 1794908370],
  ['697af213-6a63-4edb-ad9c-1ff588ee434d', 1794728770, 1794901570],
  ['70859058-c2c9-4c42-a463-2b773aa953bb', 1794833026, 1795005826],
  ['8bc5fd09-3730-4a8a-9182-6dadb2a1b058', 1794887477, 1795060277],
  ['d0925912-43cb-466c-98a6-d06c7d86e8ad', 1794894698, 1795067498],
  ['db0427fd-b175-497c-85a0-c01d8f35cb13', 1794897148, 1795069948],
  ['edef9155-d7fc-473d-9684-525b5987a911', 1794833328, 1795006128],
].map(([uid, exp, rexp]) => ({
  uid: uid as string,
  short_uid: (uid as string).slice(0, 8),
  file: `${uid}.json`,
  domain: 'www.workbuddy.cn',
  expires_at: exp as number,
  refresh_expires_at: rexp as number,
  access_encrypted: false,
  refresh_encrypted: false,
}));

export async function listTasks(): Promise<TaskRow[]> {
  if (!isTauri) return MOCK_TASKS;
  return invoke<TaskRow[]>('list_tasks');
}

export async function listAccounts(): Promise<AccountSummary[]> {
  if (!isTauri) return MOCK_ACCOUNTS;
  return invoke<AccountSummary[]>('list_accounts');
}

export async function probe(): Promise<Probe> {
  if (!isTauri) {
    return {
      exe: 'D:\\WorkBuddy\\WorkBuddy.exe',
      auth_file: 'workbuddy-desktop.info',
      data_root: 'C:\\Users\\Administrator\\.workbuddy',
      db_ok: true,
      accounts_dir: 'C:\\Users\\Administrator\\.dsh\\workbuddy-accounts',
      at_rest_key_id: '9127dea1b44020a7',
    };
  }
  return invoke<Probe>('probe');
}

export async function currentUid(): Promise<string> {
  if (!isTauri) return 'db0427fd-b175-497c-85a0-c01d8f35cb13';
  return invoke<string>('current_uid');
}

export async function openClient(): Promise<void> {
  if (!isTauri) return;
  return invoke<void>('open_client');
}

export async function openFolder(path: string): Promise<void> {
  if (!isTauri) return;
  return invoke<void>('open_folder', { path });
}

export async function switchPlan(target: string): Promise<SwitchPlan> {
  if (!isTauri) {
    return {
      target_uid: target,
      target_short: target.slice(0, 8),
      current_uid: 'db0427fd-b175-497c-85a0-c01d8f35cb13',
      current_short: 'db0427fd',
      client_running: true,
      working_tasks: [['db0427fd', '切换账号保留任务']],
      target_task_count: 1,
      warnings: ['客户端有 1 个任务处于 running 状态，切号需要重启客户端，会中断它们'],
    };
  }
  return invoke<SwitchPlan>('switch_plan', { target });
}

export async function switchApply(target: string): Promise<string> {
  if (!isTauri) return `（预览模式）已模拟切换到 ${target.slice(0, 8)}`;
  return invoke<string>('switch_apply', { target });
}

export async function unifyPlan(target?: string): Promise<UnifyPlan> {
  if (!isTauri) {
    return {
      target_uid: target ?? 'db0427fd-b175-497c-85a0-c01d8f35cb13',
      target_short: (target ?? 'db0427fd').slice(0, 8),
      total_sessions: 18,
      to_update: 17,
      already_target: 1,
      new_records: 17,
      known_records: 0,
      by_account: [['4dba68a4', 1], ['697af213', 1], ['8bc5fd09', 15], ['db0427fd', 1]],
      warnings: [],
    };
  }
  return invoke<UnifyPlan>('unify_plan', { target: target ?? null });
}

export async function unifyApply(target?: string): Promise<UnifyReport> {
  if (!isTauri) {
    return { dry_run: false, backup_path: null, updated_rows: 17, recorded: 17, total_sessions: 18 };
  }
  return invoke<UnifyReport>('unify_apply', { target: target ?? null });
}

export async function unifyRollback(): Promise<number> {
  if (!isTauri) return 0;
  return invoke<number>('unify_rollback');
}

export async function unifyStatus(): Promise<Ownership> {
  if (!isTauri) return { version: 1, unified_to: null, sessions: {}, updated_at: 0 };
  return invoke<Ownership>('unify_status');
}

export async function exportCredentials(path: string, password: string): Promise<number> {
  if (!isTauri) throw new Error('浏览器预览不支持导出凭据');
  return invoke<number>('export_credentials', { path, password });
}

export async function importCredentials(
  path: string,
  password: string,
): Promise<{ imported: number; skipped: number }> {
  if (!isTauri) throw new Error('浏览器预览不支持导入凭据');
  return invoke<{ imported: number; skipped: number }>('import_credentials', { path, password });
}

export async function probeModel(uid: string, model: string): Promise<ProbeResult> {
  if (!isTauri) {
    return {
      model,
      limited: true,
      http_status: 429,
      message: '（预览模式）模拟：使用量已超出频率限制',
      reset_at: Math.floor(Date.now() / 1000) + 3600,
    };
  }
  return invoke<ProbeResult>('probe_model', { uid, model });
}

export async function scanLimits(): Promise<LimitSignal[]> {
  if (!isTauri) {
    return [
      {
        session_id: '9bb9bbc1',
        uid: '4dba68a4-797f-4a97-a7e0-a23b261a3d48',
        message: '429 您的使用量已超出频率限制，将在 2026-10-19 09:56:23 UTC+8 重置，您也可以切换其他模型继续使用。',
        kind: 'model',
        reset_at: Math.floor(Date.now() / 1000) + 1800,
        detected_at: Math.floor(Date.now() / 1000) - 120,
        model: 'glm-5.2',
      },
    ];
  }
  return invoke<LimitSignal[]>('scan_limits');
}

const MOCK_TRANSCRIPT: Transcript = {
  session_id: 'abffe1ce',
  title: '切换账号保留任务',
  cwd: 'C:/Users/Administrator/WorkBuddy/2026-10-21-16-11-34',
  model: 'Hy4 preview',
  started_at: 1792570295,
  ended_at: 1792572368,
  path: 'C:/Users/Administrator/.workbuddy/projects/c-Users-Administrator-WorkBuddy-2026-10-21-16-11-34/abffe1ce.jsonl',
  size_bytes: 1_842_311,
  stats: {
    user_turns: 6,
    assistant_turns: 11,
    tool_calls: 24,
    reasoning_chars: 18420,
    total_chars: 31280,
    dropped_entries: 0,
    tools: [
      ['Bash', 9],
      ['Read', 6],
      ['Edit', 4],
      ['Grep', 3],
      ['TaskUpdate', 2],
    ],
  },
  entries: [
    { ts: 1792570295, kind: 'user', name: null, status: null, text: '怎么切换账号 保留任务', chars: 11, truncated: false },
    { ts: 1792570310, kind: 'assistant', name: null, status: null, text: '这个需求本质上是「多账号任务数据互通」。我先确认客户端的数据结构，再谈怎么实现。', chars: 42, truncated: false },
    { ts: 1792570330, kind: 'tool', name: 'Read', status: null, text: 'D:/workbuddy_p/workbuddy-hub/src/App.tsx', chars: 58, truncated: false },
    { ts: 1792570331, kind: 'tool_result', name: 'Read', status: 'completed', text: '1\timport { useEffect, useMemo, useRef, useState } from "react";\n2\timport { listen } from "@tauri-apps/api/event";\n…', chars: 18240, truncated: true },
    { ts: 1792570340, kind: 'assistant', name: null, status: null, text: '数据库里只有 `status=\'error\'`，真正的错误文本在会话正文的最后一条 assistant 消息里。', chars: 48, truncated: false },
  ],
};

export async function sessionDetail(id: string): Promise<Transcript> {
  if (!isTauri) return { ...MOCK_TRANSCRIPT, session_id: id };
  return invoke<Transcript>('session_detail', { id });
}

export async function autostartGet(): Promise<boolean> {
  if (!isTauri) return false;
  return invoke<boolean>('autostart_get');
}

export async function autostartSet(enabled: boolean): Promise<boolean> {
  if (!isTauri) return enabled;
  return invoke<boolean>('autostart_set', { enabled });
}

const MOCK_CHECKIN: CheckinStore = (() => {
  const today = new Date();
  const pad = (n: number) => String(n).padStart(2, '0');
  const date = `${today.getFullYear()}-${pad(today.getMonth() + 1)}-${pad(today.getDate())}`;
  const rows: [string, string, number, number, string, number][] = [
    ['128a8f9c-9167-46e5-86e7-a61fa5e766ce', 'already', 5, 100, '今天已签到 · 连签 5 天', 1240],
    ['438b979e-2c4f-4b21-983c-d2e0081ce5b5', 'already', 3, 100, '今天已签到 · 连签 3 天', 860],
    ['4dba68a4-797f-4a97-a7e0-a23b261a3d48', 'error', 0, 0, 'HTTP 401 · 登录已失效', -1],
    ['697af213-6a63-4edb-ad9c-1ff588ee434d', 'already', 5, 100, '今天已签到 · 连签 5 天', 1240],
    ['70859058-c2c9-4c42-a463-2b773aa953bb', 'already', 7, 100, '今天已签到 · 连签 7 天', 640],
    ['8bc5fd09-3730-4a8a-9182-6dadb2a1b058', 'claimed', 16, 100, '签到成功 · +100 积分', 2380],
    ['d0925912-43cb-466c-98a6-d06c7d86e8ad', 'claimed', 3, 100, '签到成功 · +100 积分', 2380],
    ['db0427fd-b175-497c-85a0-c01d8f35cb13', 'inactive', 0, 0, '签到活动未开启', 0],
    ['edef9155-d7fc-473d-9684-525b5987a911', 'already', 4, 100, '今天已签到 · 连签 4 天', 1520],
  ];
  const records: Record<string, CheckinRecord> = {};
  for (const [uid, state, streak, credit, message, total] of rows) {
    records[uid] = {
      uid,
      date,
      state: state as CheckinRecord['state'],
      credit,
      streak_days: streak,
      total_credits: total,
      message,
      updated_at: Math.floor(Date.now() / 1000) - 60,
    };
  }
  return {
    version: 1,
    auto: true,
    last_run: Math.floor(Date.now() / 1000) - 60,
    last_summary: '新签 2 · 已签 5 · 失败 1',
    records,
  };
})();

export async function checkinOverview(): Promise<CheckinStore> {
  if (!isTauri) return MOCK_CHECKIN;
  return invoke<CheckinStore>('checkin_overview');
}

export async function checkinRun(uid?: string): Promise<CheckinStore> {
  if (!isTauri) return MOCK_CHECKIN;
  return invoke<CheckinStore>('checkin_run', { uid: uid ?? null });
}

const MOCK_AUTH: AuthState = {
  logged_in: true,
  current_uid: 'db0427fd-b175-497c-85a0-c01d8f35cb13',
  auth_file: 'mock://workbuddy-desktop.info',
  reason: null,
};

export interface SyncAccountsResult {
  current: string | null;
  imported: number;
}

export async function syncAccounts(): Promise<SyncAccountsResult> {
  if (!isTauri) return { current: null, imported: 0 };
  return invoke<SyncAccountsResult>('sync_accounts');
}

export async function authState(): Promise<AuthState> {
  if (!isTauri) return MOCK_AUTH;
  return invoke<AuthState>('auth_state');
}

export const ERR_NO_LOGIN = 'NO_LOGIN:';
export const ERR_EXPIRED = 'EXPIRED:';
export const ERR_TOKEN_EXPIRED = 'TOKEN_EXPIRED:';

/** 后端用前缀区分「未登录 / 凭据过期」，这里统一拆成可判断的结构。 */
export function describeError(e: unknown): {
  code: 'no_login' | 'expired' | 'token_expired' | null;
  text: string;
} {
  const raw = e instanceof Error ? e.message : String(e);
  if (raw.startsWith(ERR_NO_LOGIN)) {
    return { code: 'no_login', text: raw.slice(ERR_NO_LOGIN.length) };
  }
  if (raw.startsWith(ERR_EXPIRED)) {
    return { code: 'expired', text: raw.slice(ERR_EXPIRED.length) };
  }
  if (raw.startsWith(ERR_TOKEN_EXPIRED)) {
    return { code: 'token_expired', text: raw.slice(ERR_TOKEN_EXPIRED.length) };
  }
  return { code: null, text: raw };
}

const MOCK_CREDITS: CreditStore = {
  version: 1,
  credits: {
    '128a8f9c-9167-46e5-86e7-a61fa5e766ce': { remaining: 2910.9, total: 3400, packages: 3, updated_at: 0, valid: true },
    '438b979e-2c4f-4b21-983c-d2e0081ce5b5': { remaining: 640, total: 1000, packages: 2, updated_at: 0, valid: true },
    '4dba68a4-797f-4a97-a7e0-a23b261a3d48': { remaining: 0, total: 500, packages: 1, updated_at: 0, valid: false },
    '697af213-6a63-4edb-ad9c-1ff588ee434d': { remaining: 1520, total: 2000, packages: 2, updated_at: 0, valid: true },
    '70859058-c2c9-4c42-a463-2b773aa953bb': { remaining: 860, total: 1500, packages: 2, updated_at: 0, valid: true },
    '8bc5fd09-3730-4a8a-9182-6dadb2a1b058': { remaining: 3240, total: 4000, packages: 3, updated_at: 0, valid: true },
    'd0925912-43cb-466c-98a6-d06c7d86e8ad': { remaining: 1180, total: 1500, packages: 1, updated_at: 0, valid: true },
    'db0427fd-b175-497c-85a0-c01d8f35cb13': { remaining: 45.6, total: 500, packages: 1, updated_at: 0, valid: true },
    'edef9155-d7fc-473d-9684-525b5987a911': { remaining: 2100, total: 3000, packages: 2, updated_at: 0, valid: true },
  },
};

export async function creditsOverview(): Promise<CreditStore> {
  if (!isTauri) return MOCK_CREDITS;
  return invoke<CreditStore>('credits_overview');
}

export async function creditsRefresh(uid?: string): Promise<CreditStore> {
  if (!isTauri) return MOCK_CREDITS;
  return invoke<CreditStore>('credits_refresh', { uid: uid ?? null });
}

export async function checkinSetAuto(enabled: boolean): Promise<CheckinStore> {
  if (!isTauri) return { ...MOCK_CHECKIN, auto: enabled };
  return invoke<CheckinStore>('checkin_set_auto', { enabled });
}

// 示例数据用假号码，真实号码只存在本机的 profiles.json 里
const MOCK_PROFILES: ProfileStore = (() => {
  const rows: [string, string, string][] = [
    ['128a8f9c-9167-46e5-86e7-a61fa5e766ce', '138****0001', '主号'],
    ['438b979e-2c4f-4b21-983c-d2e0081ce5b5', '138****0002', '工作号'],
    ['4dba68a4-797f-4a97-a7e0-a23b261a3d48', '138****0003', ''],
    ['697af213-6a63-4edb-ad9c-1ff588ee434d', '138****0004', ''],
    ['70859058-c2c9-4c42-a463-2b773aa953bb', '138****0005', '· Y ·'],
    ['8bc5fd09-3730-4a8a-9182-6dadb2a1b058', '138****0006', 'G'],
    ['d0925912-43cb-466c-98a6-d06c7d86e8ad', '138****0007', ''],
    ['db0427fd-b175-497c-85a0-c01d8f35cb13', '138****0008', ''],
    ['edef9155-d7fc-473d-9684-525b5987a911', '138****0009', ''],
  ];
  const profiles: Record<string, ProfileStore['profiles'][string]> = {};
  for (const [uid, phone, nickname] of rows) {
    profiles[uid] = {
      uid,
      nickname,
      phone,
      email: '',
      uin: 0,
      account_type: 'personal',
      fetched_at: Math.floor(Date.now() / 1000),
    };
  }
  return { version: 1, profiles, last_run: Math.floor(Date.now() / 1000) };
})();

export async function profilesOverview(): Promise<ProfileStore> {
  if (!isTauri) return MOCK_PROFILES;
  return invoke<ProfileStore>('profiles_overview');
}

export async function profilesRefresh(force = false): Promise<ProfileStore> {
  if (!isTauri) return MOCK_PROFILES;
  return invoke<ProfileStore>('profiles_refresh', { force });
}

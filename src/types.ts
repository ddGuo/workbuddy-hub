export interface TaskRow {
  id: string;
  uid: string;
  short_uid: string;
  cwd: string;
  title: string;
  status: string;
  created_at: number;
  updated_at: number;
}

export interface AccountSummary {
  uid: string;
  short_uid: string;
  file: string;
  domain: string;
  expires_at: number;
  refresh_expires_at: number;
  access_encrypted: boolean;
  refresh_encrypted: boolean;
}

export interface Probe {
  exe: string | null;
  auth_file: string | null;
  data_root: string;
  db_ok: boolean;
  accounts_dir: string;
  at_rest_key_id: string | null;
}

export interface SwitchPlan {
  target_uid: string;
  target_short: string;
  current_uid: string;
  current_short: string;
  client_running: boolean;
  working_tasks: [string, string][];
  target_task_count: number;
  warnings: string[];
}

export interface Account extends AccountSummary {
  taskCount: number;
  isCurrent: boolean;
}

export interface UnifyPlan {
  target_uid: string;
  target_short: string;
  total_sessions: number;
  to_update: number;
  already_target: number;
  new_records: number;
  known_records: number;
  by_account: [string, number][];
  warnings: string[];
}

export interface UnifyReport {
  dry_run: boolean;
  backup_path: string | null;
  updated_rows: number;
  recorded: number;
  total_sessions: number;
}

export interface Ownership {
  version: number;
  unified_to: string | null;
  sessions: Record<string, string>;
  updated_at: number;
}

export interface LimitSignal {
  session_id: string;
  uid: string;
  message: string;
  kind: 'account' | 'model';
  reset_at: number | null;
  detected_at: number;
  /** 触发限流的模型 */
  model?: string | null;
}

export interface TranscriptEntry {
  ts: number;
  kind: 'user' | 'assistant' | 'tool' | 'tool_result';
  name: string | null;
  status: string | null;
  text: string;
  chars: number;
  truncated: boolean;
}

export interface TranscriptStats {
  user_turns: number;
  assistant_turns: number;
  tool_calls: number;
  reasoning_chars: number;
  total_chars: number;
  dropped_entries: number;
  tools: [string, number][];
}

export interface Transcript {
  session_id: string;
  title: string | null;
  cwd: string | null;
  model: string | null;
  started_at: number;
  ended_at: number;
  entries: TranscriptEntry[];
  stats: TranscriptStats;
  path: string;
  size_bytes: number;
}

export type CheckinState = 'claimed' | 'already' | 'inactive' | 'error';

export interface CheckinRecord {
  uid: string;
  date: string;
  state: CheckinState;
  credit: number;
  streak_days: number;
  /** 剩余积分；-1 表示还没拉到 */
  total_credits: number;
  message: string;
  updated_at: number;
}

export interface CheckinStore {
  version: number;
  auto: boolean;
  records: Record<string, CheckinRecord>;
  last_run: number;
  last_summary: string;
}

export interface Profile {
  uid: string;
  nickname: string;
  /** 已脱敏，形如 199****9283；没有手机号时为空 */
  phone: string;
  email: string;
  uin: number;
  account_type: string;
  fetched_at: number;
}

export interface ProfileStore {
  version: number;
  profiles: Record<string, Profile>;
  last_run: number;
}

export type AuthReason = 'no_auth_file' | 'empty_uid' | 'unreadable';

export interface AuthState {
  logged_in: boolean;
  current_uid: string | null;
  auth_file: string | null;
  reason: AuthReason | null;
}

export interface CreditSummary {
  /** 剩余积分，可能带小数 */
  remaining: number;
  total: number;
  packages: number;
  updated_at: number;
}

export interface CreditStore {
  version: number;
  credits: Record<string, CreditSummary>;
}

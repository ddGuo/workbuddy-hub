import { useEffect, useMemo, useRef, useState, type MouseEvent as ReactMouseEvent } from 'react';
import { listen } from '@tauri-apps/api/event';
import { getCurrentWindow } from '@tauri-apps/api/window';
import TaskDetail from './TaskDetail';
import {
  autostartGet,
  autostartSet,
  checkinOverview,
  checkinRun,
  checkinSetAuto,
  currentUid,
  isTauri,
  listAccounts,
  listTasks,
  openClient,
  openFolder,
  probe,
  profilesOverview,
  scanLimits,
  switchApply,
  switchPlan,
  unifyApply,
  unifyPlan,
  unifyRollback,
  unifyStatus,
} from './api';
import { version as appVersion } from '../package.json';
import appIcon from './assets/app-icon.png';
import { applyTheme, loadThemeMode, saveThemeMode, type ThemeMode } from './theme';
import type {
  Account,
  AccountSummary,
  CheckinStore,
  LimitSignal,
  Ownership,
  Probe,
  ProfileStore,
  SwitchPlan,
  TaskRow,
  UnifyPlan,
} from './types';

const STATUS_LABEL: Record<string, string> = {
  working: '进行中',
  completed: '已完成',
  error: '失败',
  pending: '待处理',
  planning: '规划中',
  archived: '已归档',
};

const THEMES: { key: ThemeMode; icon: string; label: string }[] = [
  { key: 'system', icon: '◐', label: '跟随系统' },
  { key: 'light', icon: '☀', label: '浅色' },
  { key: 'dark', icon: '☾', label: '深色' },
];

interface DialogPayload {
  kind: 'switch' | 'unify';
  target: string;
  targetShort: string;
  switchPlan?: SwitchPlan;
  unifyPlan?: UnifyPlan;
}

function fmtTime(sec: number): string {
  if (!sec) return '—';
  const d = new Date(sec * 1000);
  const now = new Date();
  const pad = (n: number) => String(n).padStart(2, '0');
  if (d.toDateString() === now.toDateString()) {
    return `今天 ${pad(d.getHours())}:${pad(d.getMinutes())}`;
  }
  return `${d.getMonth() + 1}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function fmtDate(sec: number): string {
  if (!sec) return '—';
  const d = new Date(sec * 1000);
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

function shortPath(p: string): string {
  const parts = p.split(/[\\/]/).filter(Boolean);
  return parts.length > 2 ? `…/${parts.slice(-2).join('/')}` : p;
}

/** 本地日期，和 hub 侧 `format_date` 对齐（都是 UTC+8 机器上的本地日）。 */
function todayStr(): string {
  const d = new Date();
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

/** 由 uid 派生一个稳定的头像色相，同一账号每次都是同一个颜色。 */
function avatarStyle(uid: string) {
  let h = 0;
  for (let i = 0; i < uid.length; i++) h = (h * 31 + uid.charCodeAt(i)) % 360;
  return {
    background: `linear-gradient(135deg, hsl(${h} 68% 58%), hsl(${(h + 46) % 360} 72% 46%))`,
  };
}

export default function App() {
  const [tasks, setTasks] = useState<TaskRow[]>([]);
  const [accounts, setAccounts] = useState<AccountSummary[]>([]);
  const [current, setCurrent] = useState('');
  const [info, setInfo] = useState<Probe | null>(null);
  const [ownership, setOwnership] = useState<Ownership | null>(null);
  const [query, setQuery] = useState('');
  const [statusFilter, setStatusFilter] = useState<string>('all');
  const [groupBy, setGroupBy] = useState<'account' | 'time'>('account');
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [dialog, setDialog] = useState<DialogPayload | null>(null);
  const [busy, setBusy] = useState(false);
  const [progress, setProgress] = useState('');
  const [toast, setToast] = useState<{ kind: 'ok' | 'err'; text: string } | null>(null);
  const [detail, setDetail] = useState<TaskRow | null>(null);
  const [limits, setLimits] = useState<LimitSignal[]>([]);
  const [limitMuted, setLimitMuted] = useState(false);
  const [autostart, setAutostart] = useState(false);
  const [themeMode, setThemeMode] = useState<ThemeMode>(() => loadThemeMode());
  const [checkin, setCheckin] = useState<CheckinStore | null>(null);
  const [profiles, setProfiles] = useState<ProfileStore | null>(null);
  const [maximized, setMaximized] = useState(false);
  const [checkinBusy, setCheckinBusy] = useState(false);
  const [checkinStep, setCheckinStep] = useState('');
  const progressRef = useRef<(() => void) | null>(null);
  const searchRef = useRef<HTMLInputElement>(null);

  // 主题：写 data-theme，选「跟随系统」时监听系统变化
  useEffect(() => {
    applyTheme(themeMode);
  }, [themeMode]);

  useEffect(() => {
    if (themeMode !== 'system' || !window.matchMedia) return;
    const mq = window.matchMedia('(prefers-color-scheme: dark)');
    const onChange = () => applyTheme('system');
    mq.addEventListener('change', onChange);
    return () => mq.removeEventListener('change', onChange);
  }, [themeMode]);

  function chooseTheme(mode: ThemeMode) {
    setThemeMode(mode);
    saveThemeMode(mode);
  }

  // 切换过程中后端会逐步推进度，这里订阅下来显示，
  // 避免用户对着没有反馈的按钮干等十几秒。
  useEffect(() => {
    if (!isTauri) return;
    const un1 = listen<string>('switch-progress', (e) => setProgress(e.payload));
    const un2 = listen<string>('checkin-progress', (e) => setCheckinStep(e.payload));
    const un3 = listen<ProfileStore>('profiles-updated', (e) => setProfiles(e.payload));
    progressRef.current = () => {
      void un1.then((f) => f());
      void un2.then((f) => f());
      void un3.then((f) => f());
    };
    return () => {
      progressRef.current?.();
      progressRef.current = null;
    };
  }, []);

  async function load() {
    setLoading(true);
    setError(null);
    try {
      const [t, a, c, p, o, lim, auto, ck, pf] = await Promise.all([
        listTasks(),
        listAccounts(),
        currentUid(),
        probe(),
        unifyStatus().catch(() => null),
        scanLimits().catch(() => [] as LimitSignal[]),
        autostartGet().catch(() => false),
        checkinOverview().catch(() => null),
        profilesOverview().catch(() => null),
      ]);
      setTasks(t);
      setAccounts(a);
      setCurrent(c);
      setInfo(p);
      setOwnership(o);
      setLimits(lim);
      setAutostart(auto);
      setCheckin(ck);
      setProfiles(pf);
      // 调试 / 截图用：?task=<id 前缀> 直接打开某条任务的详情
      const openId = new URLSearchParams(window.location.search).get('task');
      if (openId) {
        const hit = t.find((x) => x.id.startsWith(openId));
        if (hit) setDetail(hit);
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    void load();
  }, []);

  useEffect(() => {
    if (!toast) return;
    const id = window.setTimeout(() => setToast(null), 6000);
    return () => window.clearTimeout(id);
  }, [toast]);

  // 自定义标题栏：跟随窗口的还原/最大化状态换图标
  useEffect(() => {
    if (!isTauri) return;
    const win = getCurrentWindow();
    let unlisten: (() => void) | undefined;
    const sync = () => {
      win.isMaximized().then(setMaximized).catch(() => {});
    };
    sync();
    win
      .onResized(sync)
      .then((f) => {
        unlisten = f;
      })
      .catch(() => {});
    return () => unlisten?.();
  }, []);

  // 按 / 直接聚焦搜索，和大多数列表类工具的习惯一致
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const el = e.target as HTMLElement | null;
      const typing =
        !!el && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.isContentEditable);
      if (e.key === '/' && !typing) {
        e.preventDefault();
        searchRef.current?.focus();
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);

  // 限流守护目前只做「报告」：每分钟扫一遍失败会话，读到 429 就在列表上方提示。
  useEffect(() => {
    if (!isTauri) return;
    const id = window.setInterval(() => {
      scanLimits()
        .then(setLimits)
        .catch(() => {});
    }, 60_000);
    return () => window.clearInterval(id);
  }, []);

  async function winAction(action: 'min' | 'max' | 'close') {
    if (!isTauri) return;
    const win = getCurrentWindow();
    try {
      if (action === 'min') await win.minimize();
      else if (action === 'max') await win.toggleMaximize();
      else await win.close(); // 关窗走后端逻辑：隐藏到托盘
    } catch (e) {
      setToast({ kind: 'err', text: `窗口操作失败：${String(e)}` });
    }
  }

  // 双击标题栏空白处 = 最大化/还原；点在按钮或输入框上不触发
  function onTitlebarDoubleClick(e: ReactMouseEvent<HTMLElement>) {
    const el = e.target as HTMLElement;
    if (el.closest('button, input, label, .theme-switch, .window-controls')) return;
    void winAction('max');
  }

  async function toggleAutostart(next: boolean) {
    try {
      const v = await autostartSet(next);
      setAutostart(v);
      setToast({ kind: 'ok', text: v ? '已加入开机自启' : '已取消开机自启' });
    } catch (e) {
      setToast({ kind: 'err', text: e instanceof Error ? e.message : String(e) });
    }
  }

  async function doCheckin() {
    setCheckinBusy(true);
    setCheckinStep('正在签到…');
    try {
      const store = await checkinRun();
      setCheckin(store);
      setToast({ kind: 'ok', text: store.last_summary || '签到完成' });
    } catch (e) {
      setToast({ kind: 'err', text: e instanceof Error ? e.message : String(e) });
    } finally {
      setCheckinBusy(false);
      setCheckinStep('');
    }
  }

  async function toggleAutoCheckin(next: boolean) {
    try {
      setCheckin(await checkinSetAuto(next));
    } catch (e) {
      setToast({ kind: 'err', text: e instanceof Error ? e.message : String(e) });
    }
  }

  // 列表里优先显示手机号（脱敏），没有就昵称，再没有才回退 uid 前缀
  const labelOf = (uid: string, shortUid?: string) => {
    const p = profiles?.profiles?.[uid];
    if (p?.phone) return p.phone;
    if (p?.nickname) return p.nickname;
    return shortUid ?? uid.slice(0, 8);
  };

  const accountMap = useMemo(() => {
    const m = new Map<string, Account>();
    for (const a of accounts) {
      m.set(a.uid, {
        ...a,
        taskCount: tasks.filter((t) => t.uid === a.uid).length,
        isCurrent: a.uid === current,
      });
    }
    return m;
  }, [accounts, tasks, current]);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    return tasks.filter((t) => {
      if (statusFilter !== 'all' && t.status !== statusFilter) return false;
      if (!q) return true;
      return (
        t.title.toLowerCase().includes(q) ||
        t.cwd.toLowerCase().includes(q) ||
        t.short_uid.includes(q)
      );
    });
  }, [tasks, query, statusFilter]);

  const grouped = useMemo(() => {
    if (groupBy === 'time') {
      const now = Date.now() / 1000;
      const buckets: [string, (t: TaskRow) => boolean][] = [
        ['今天', (t) => now - t.updated_at < 86400],
        ['最近 7 天', (t) => now - t.updated_at >= 86400 && now - t.updated_at < 604800],
        ['更早', (t) => now - t.updated_at >= 604800],
      ];
      return buckets
        .map(([label, fn]) => ({ key: label, rows: filtered.filter(fn) }))
        .filter((g) => g.rows.length > 0);
    }
    const byUid = new Map<string, TaskRow[]>();
    for (const t of filtered) {
      const list = byUid.get(t.uid) ?? [];
      list.push(t);
      byUid.set(t.uid, list);
    }
    return [...byUid.entries()]
      .map(([uid, rows]) => ({ key: uid, rows }))
      .sort((a, b) => b.rows.length - a.rows.length);
  }, [filtered, groupBy]);

  const statusCounts = useMemo(() => {
    const m: Record<string, number> = {};
    for (const t of tasks) m[t.status] = (m[t.status] ?? 0) + 1;
    return m;
  }, [tasks]);

  // 只认 24 小时内的信号：数据库里几个月前的失败任务会一直被扫出来，
  // 那不是「现在被限流」，拿来报警只会让人关掉这个功能。
  const activeLimit = useMemo(() => {
    const now = Date.now() / 1000;
    return (
      limits
        .filter((l) => l.detected_at > 0 && now - l.detected_at < 24 * 3600)
        .sort((a, b) => b.detected_at - a.detected_at)[0] ?? null
    );
  }, [limits]);

  const limitAlt = useMemo(() => {
    if (!activeLimit) return null;
    const now = Date.now() / 1000;
    return (
      accounts.find(
        (a) => a.uid !== activeLimit.uid && a.uid !== current && a.refresh_expires_at > now,
      ) ?? null
    );
  }, [accounts, activeLimit, current]);

  const today = todayStr();
  const checkinStats = useMemo(() => {
    let done = 0;
    let failed = 0;
    for (const r of Object.values(checkin?.records ?? {})) {
      if (r.date !== today) continue;
      if (r.state === 'claimed' || r.state === 'already') done++;
      if (r.state === 'error') failed++;
    }
    return { done, failed, total: accounts.length };
  }, [checkin, accounts.length, today]);

  async function openSwitchDialog(uid: string) {
    setBusy(true);
    try {
      const [sp, up] = await Promise.all([switchPlan(uid), unifyPlan(uid)]);
      setDialog({
        kind: 'switch',
        target: uid,
        targetShort: labelOf(uid, sp.target_short),
        switchPlan: sp,
        unifyPlan: up,
      });
    } catch (e) {
      setToast({ kind: 'err', text: e instanceof Error ? e.message : String(e) });
    } finally {
      setBusy(false);
    }
  }

  async function openUnifyDialog() {
    setBusy(true);
    try {
      const up = await unifyPlan();
      setDialog({
        kind: 'unify',
        target: current,
        targetShort: labelOf(current),
        unifyPlan: up,
      });
    } catch (e) {
      setToast({ kind: 'err', text: e instanceof Error ? e.message : String(e) });
    } finally {
      setBusy(false);
    }
  }

  async function confirmDialog() {
    if (!dialog) return;
    setBusy(true);
    if (dialog.kind === 'switch') setProgress('准备中…');
    try {
      if (dialog.kind === 'switch') {
        const msg = await switchApply(dialog.target);
        setToast({ kind: 'ok', text: msg });
      } else {
        const r = await unifyApply(dialog.target);
        setToast({
          kind: 'ok',
          text: `已归集 ${r.updated_rows} 条任务到 ${dialog.targetShort}，重启客户端后生效`,
        });
      }
      setDialog(null);
      await load();
    } catch (e) {
      setToast({ kind: 'err', text: e instanceof Error ? e.message : String(e) });
    } finally {
      setBusy(false);
      setProgress('');
    }
  }

  async function doRollback() {
    setBusy(true);
    try {
      const n = await unifyRollback();
      setToast({ kind: 'ok', text: `已恢复 ${n} 条任务的原始归属` });
      await load();
    } catch (e) {
      setToast({ kind: 'err', text: e instanceof Error ? e.message : String(e) });
    } finally {
      setBusy(false);
    }
  }

  const sp = dialog?.switchPlan;
  const up = dialog?.unifyPlan;
  const checkinPct = checkinStats.total
    ? Math.round((checkinStats.done / checkinStats.total) * 100)
    : 0;

  return (
    <div className="app">
      <header
        className="topbar"
        data-tauri-drag-region
        onDoubleClick={onTitlebarDoubleClick}
      >
        <div className="brand" data-tauri-drag-region>
          <img className="mark" src={appIcon} width={28} height={28} alt="" draggable={false} />
          <h1 data-tauri-drag-region>WorkBuddy Hub</h1>
          <span className="version" data-tauri-drag-region>
            v{appVersion}
          </span>
          {!isTauri && <span className="mock">示例数据</span>}
        </div>

        <div className="actions">
          <label className="search">
            <svg viewBox="0 0 24 24" fill="none" aria-hidden="true">
              <circle cx="11" cy="11" r="6.5" stroke="currentColor" strokeWidth="1.8" />
              <path d="m16 16 4 4" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" />
            </svg>
            <input
              ref={searchRef}
              placeholder="搜索任务、路径或账号"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
            />
            <kbd className="kbd">/</kbd>
          </label>

          <button
            className="icon-btn round"
            onClick={() => void load()}
            disabled={loading}
            title={loading ? '刷新中' : '刷新'}
            aria-label="刷新"
          >
            <svg viewBox="0 0 24 24" fill="none" className={loading ? 'spin' : ''}>
              <path
                d="M20 12a8 8 0 1 1-2.34-5.66M20 4v4h-4"
                stroke="currentColor"
                strokeWidth="1.8"
                strokeLinecap="round"
                strokeLinejoin="round"
              />
            </svg>
          </button>

          <button className="btn" onClick={() => void openUnifyDialog()} disabled={busy || !current}>
            同步任务
          </button>

          <span className="actions-sep" />

          <button className="btn primary" onClick={() => void openClient()}>
            打开客户端
          </button>

          <div className="theme-switch" title="主题">
            {THEMES.map((t) => (
              <button
                key={t.key}
                className={themeMode === t.key ? 'on' : ''}
                onClick={() => chooseTheme(t.key)}
                title={t.label}
                aria-label={t.label}
              >
                {t.icon}
              </button>
            ))}
          </div>

          <div className="window-controls">
            <button
              className="win-btn"
              title="最小化"
              aria-label="最小化"
              onClick={() => void winAction('min')}
            >
              <svg viewBox="0 0 24 24" fill="none" aria-hidden="true">
                <path d="M5 12h14" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" />
              </svg>
            </button>
            <button
              className="win-btn"
              title={maximized ? '向下还原' : '最大化'}
              aria-label={maximized ? '向下还原' : '最大化'}
              onClick={() => void winAction('max')}
            >
              <svg viewBox="0 0 24 24" fill="none" aria-hidden="true">
                {maximized ? (
                  <>
                    <rect
                      x="5.5"
                      y="8"
                      width="10.5"
                      height="10.5"
                      rx="1.6"
                      stroke="currentColor"
                      strokeWidth="1.6"
                    />
                    <path
                      d="M8.5 8V6.2c0-.66.54-1.2 1.2-1.2h8.1c.66 0 1.2.54 1.2 1.2v8.1c0 .66-.54 1.2-1.2 1.2H16"
                      stroke="currentColor"
                      strokeWidth="1.6"
                      strokeLinecap="round"
                    />
                  </>
                ) : (
                  <rect
                    x="5.5"
                    y="5.5"
                    width="13"
                    height="13"
                    rx="1.8"
                    stroke="currentColor"
                    strokeWidth="1.6"
                  />
                )}
              </svg>
            </button>
            <button
              className="win-btn close"
              title="关闭到托盘"
              aria-label="关闭"
              onClick={() => void winAction('close')}
            >
              <svg viewBox="0 0 24 24" fill="none" aria-hidden="true">
                <path
                  d="M6 6l12 12M18 6 6 18"
                  stroke="currentColor"
                  strokeWidth="1.8"
                  strokeLinecap="round"
                />
              </svg>
            </button>
          </div>
        </div>
      </header>

      <div className="body">
        <aside className="rail">
          <div className="rail-head">
            <span>账号</span>
            <span className="rail-count">{accounts.length}</span>
          </div>

          <div className="rail-list">
            {[...accountMap.values()]
              .sort((a, b) => b.taskCount - a.taskCount)
              .map((a) => {
                const ck = checkin?.records?.[a.uid];
                const fresh = ck && ck.date === today;
                return (
                  <div
                    key={a.uid}
                    className={`account ${a.isCurrent ? 'current' : ''} ${
                      a.taskCount === 0 ? 'empty' : ''
                    }`}
                    title={`${labelOf(a.uid, a.short_uid)}${
                      profiles?.profiles?.[a.uid]?.nickname
                        ? `（${profiles.profiles[a.uid].nickname}）`
                        : ''
                    }\n${a.uid}\n凭据到期 ${fmtDate(a.refresh_expires_at)}${
                      fresh ? `\n签到：${ck.message}` : ''
                    }`}
                  >
                    <span className="avatar" style={avatarStyle(a.uid)}>
                      {a.short_uid.slice(0, 2).toUpperCase()}
                    </span>
                    <div className="account-info">
                      <div className="account-top">
                        <span className="acct-label">{labelOf(a.uid, a.short_uid)}</span>
                        {a.taskCount > 0 && <span className="badge">{a.taskCount}</span>}
                      </div>
                      <div className="account-sub">
                        <code>{a.short_uid}</code>
                        <span className="sep">·</span>
                        {fresh ? (
                          <span
                            className={`checkin-tick ${
                              ck.state === 'error' ? 'bad' : ck.state === 'inactive' ? 'off' : ''
                            }`}
                            title={ck.message}
                          >
                            {ck.state === 'error' ? '签到失败' : ck.state === 'inactive' ? '无活动' : `已签 ${ck.streak_days} 天`}
                          </span>
                        ) : (
                          <span>凭证 {fmtDate(a.refresh_expires_at)}</span>
                        )}
                      </div>
                    </div>
                    {a.isCurrent ? (
                      <span className="current-tag">当前</span>
                    ) : (
                      <button
                        className="mini"
                        disabled={busy}
                        onClick={() => void openSwitchDialog(a.uid)}
                      >
                        切换
                      </button>
                    )}
                  </div>
                );
              })}
          </div>

          <div className="rail-foot">
            <section className="side-card checkin-card">
              <div className="side-card-head">
                <span className="side-card-title">每日签到</span>
                <span className="side-card-meta">
                  {checkinStats.done}/{checkinStats.total || accounts.length}
                </span>
              </div>
              <div className="checkin-bar">
                <i style={{ width: `${checkinPct}%` }} />
              </div>
              <p className="checkin-note">
                {checkinBusy
                  ? checkinStep || '正在签到…'
                  : checkin?.last_summary || '还没有签到记录'}
              </p>
              <div className="checkin-actions">
                <button
                  className="btn sm primary"
                  onClick={() => void doCheckin()}
                  disabled={checkinBusy || accounts.length === 0}
                >
                  {checkinBusy ? '签到中…' : '立即签到'}
                </button>
                <button
                  className={`toggle ${checkin?.auto ? 'on' : ''}`}
                  onClick={() => void toggleAutoCheckin(!checkin?.auto)}
                  aria-pressed={!!checkin?.auto}
                  title={checkin?.auto ? '自动签到已开启' : '自动签到已关闭'}
                >
                  <i />
                </button>
                <span className="checkin-auto-label">自动</span>
              </div>
            </section>

            {ownership?.unified_to && (
              <section className="side-card unified">
                <div className="side-card-head">
                  <span className="side-card-title">任务归集</span>
                  <span className="pill current-pill">已归集</span>
                </div>
                <p>
                  目标 {ownership.unified_to.slice(0, 8)} · {Object.keys(ownership.sessions).length}{' '}
                  条记录
                </p>
                <button className="mini wide" disabled={busy} onClick={() => void doRollback()}>
                  恢复原始归属
                </button>
              </section>
            )}

            {info && (
              <section className="side-card probe">
                <div className="probe-row">
                  <span>客户端</span>
                  <code>{info.exe ? '已找到' : '未找到'}</code>
                </div>
                <div className="probe-row">
                  <span>数据库</span>
                  <code>{info.db_ok ? '可读' : '不可读'}</code>
                </div>
                <div className="probe-row">
                  <span>密钥</span>
                  <code>{info.at_rest_key_id ?? '—'}</code>
                </div>
                <div className="probe-row">
                  <span>开机自启</span>
                  <button
                    className={`toggle sm ${autostart ? 'on' : ''}`}
                    onClick={() => void toggleAutostart(!autostart)}
                    aria-pressed={autostart}
                  >
                    <i />
                  </button>
                </div>
              </section>
            )}
          </div>
        </aside>

        <main className="main">
          <div className="subbar">
            <div className="filters">
              {['all', ...Object.keys(statusCounts)].map((s) => (
                <button
                  key={s}
                  className={`chip ${statusFilter === s ? 'on' : ''}`}
                  onClick={() => setStatusFilter(s)}
                >
                  {s === 'all' ? '全部' : STATUS_LABEL[s] ?? s}
                  <span className="chip-n">{s === 'all' ? tasks.length : statusCounts[s]}</span>
                </button>
              ))}
            </div>

            <div className="subbar-right">
              <span className={`conn ${info ? (info.exe ? 'ok' : 'bad') : ''}`}>
                <i />
                {info ? (info.exe ? '客户端已连接' : '未找到客户端') : '正在检测'}
                <em>·</em>
                {accounts.length} 个账号
              </span>
              <div className="seg">
                <button
                  className={groupBy === 'account' ? 'on' : ''}
                  onClick={() => setGroupBy('account')}
                >
                  按账号
                </button>
                <button
                  className={groupBy === 'time' ? 'on' : ''}
                  onClick={() => setGroupBy('time')}
                >
                  按时间
                </button>
              </div>
            </div>
          </div>

          <div className="stream">
            {error && <div className="error">读取失败：{error}</div>}

            {activeLimit && !limitMuted && (
              <div className="limit-banner">
                <div className="limit-main">
                  <div className="limit-title">
                    <b>检测到限流</b>
                    <span>
                      账号 <code>{labelOf(activeLimit.uid)}</code> ·{' '}
                      {activeLimit.kind === 'account' ? '账号额度' : '单模型'} ·{' '}
                      {activeLimit.reset_at
                        ? activeLimit.reset_at > Date.now() / 1000
                          ? `冷却至 ${fmtTime(activeLimit.reset_at)}`
                          : '已过重置时间，可继续使用'
                        : '按退避冷却'}
                    </span>
                  </div>
                  <code className="limit-msg">{activeLimit.message}</code>
                </div>
                <div className="limit-actions">
                  {limitAlt && (
                    <button
                      className="btn primary"
                      disabled={busy}
                      onClick={() => void openSwitchDialog(limitAlt.uid)}
                    >
                      换到 {labelOf(limitAlt.uid, limitAlt.short_uid)}
                    </button>
                  )}
                  <button className="btn" onClick={() => setLimitMuted(true)}>
                    忽略
                  </button>
                </div>
              </div>
            )}

            {!error && filtered.length === 0 && !loading && (
              <div className="empty-state">
                <p>没有匹配的任务</p>
                <span>试试清空搜索或换个筛选条件</span>
              </div>
            )}

            {grouped.map((g) => {
              const acc = accountMap.get(g.key);
              const label =
                groupBy === 'account' ? labelOf(g.key, acc?.short_uid) : g.key;
              return (
                <section className="group" key={g.key}>
                  <div className="group-head">
                    {groupBy === 'account' && acc && (
                      <span className="avatar xs" style={avatarStyle(acc.uid)}>
                        {acc.short_uid.slice(0, 2).toUpperCase()}
                      </span>
                    )}
                    <h2>{label}</h2>
                    {groupBy === 'account' && acc?.isCurrent && (
                      <span className="pill current-pill">当前登录</span>
                    )}
                    <span className="group-n">{g.rows.length} 个任务</span>
                  </div>
                  <div className="cards">
                    {g.rows.map((t) => (
                      <article
                        className="card"
                        key={t.id}
                        data-status={t.status}
                        onClick={() => setDetail(t)}
                        title={`${t.cwd}\n点击查看对话`}
                      >
                        <div className="card-top">
                          <h3>{t.title}</h3>
                          <span className={`status ${t.status}`}>
                            <i />
                            {STATUS_LABEL[t.status] ?? t.status}
                          </span>
                        </div>
                        <div className="card-meta">
                          <code>{labelOf(t.uid, t.short_uid)}</code>
                          <span className="sep">·</span>
                          <span>{fmtTime(t.updated_at)}</span>
                          <span className="sep">·</span>
                          <span className="path">{shortPath(t.cwd)}</span>
                        </div>
                      </article>
                    ))}
                  </div>
                </section>
              );
            })}
          </div>
        </main>
      </div>

      {detail && (
        <TaskDetail
          task={detail}
          accountLabel={labelOf(detail.uid, detail.short_uid)}
          onClose={() => setDetail(null)}
          onOpenFolder={(p) => void openFolder(p)}
        />
      )}

      {dialog && (
        <div className="modal-backdrop" onClick={() => !busy && setDialog(null)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <div className="modal-head">
              <h2>{dialog.kind === 'switch' ? '切换账号' : '同步任务到当前账号'}</h2>
              <p>
                {dialog.kind === 'switch'
                  ? `${sp ? labelOf(sp.current_uid, sp.current_short) : '—'} → ${dialog.targetShort}`
                  : `目标 ${dialog.targetShort}`}
              </p>
            </div>

            <div className="modal-body">
              {busy && dialog.kind === 'switch' ? (
                <div className="switching">
                  <div className="switch-spinner" />
                  <p className="switch-step">{progress || '正在切换…'}</p>
                  <p className="switch-hint">
                    客户端会关闭并重新打开，约 10 秒。过程中请勿关闭本窗口。
                  </p>
                </div>
              ) : (
                <>
                  {dialog.kind === 'switch' && sp && (
                    <>
                      <div className="kv">
                        <span>目标任务</span>
                        <b>{sp.target_task_count} 个</b>
                      </div>
                      <div className="kv">
                        <span>中断任务</span>
                        <b className={sp.working_tasks.length > 0 ? 'warn-text' : ''}>
                          {sp.working_tasks.length} 个
                        </b>
                      </div>
                    </>
                  )}

                  {up && (
                    <>
                      <div className="kv">
                        <span>归集任务</span>
                        <b>{up.to_update} 条</b>
                      </div>
                      <div className="kv">
                        <span>已在名下</span>
                        <b>{up.already_target} 条</b>
                      </div>
                    </>
                  )}

                  {up && up.by_account.length > 0 && (
                    <div className="dist">
                      <span className="dist-label">原始归属</span>
                      <div className="dist-rows">
                        {up.by_account.map(([uid, n]) => (
                          <span key={uid} className="dist-item">
                            <code>{labelOf(uid)}</code>
                            <b>{n}</b>
                          </span>
                        ))}
                      </div>
                    </div>
                  )}

                  {dialog.kind === 'switch' && sp && sp.working_tasks.length > 0 && (
                    <div className="modal-warn">
                      <p>以下任务正在运行，切换会中断它们：</p>
                      <ul>
                        {sp.working_tasks.map(([uid, title], i) => (
                          <li key={i}>
                            <code>{uid}</code> {title}
                          </li>
                        ))}
                      </ul>
                    </div>
                  )}

                  <div className="modal-note">
                    {dialog.kind === 'switch'
                      ? '操作会退出并重启 WorkBuddy 客户端，约 10 秒。凭据与数据库都会先备份。'
                      : '操作只改数据库中的任务归属，不会重启客户端；重启后生效。写库前会自动快照。'}
                  </div>
                </>
              )}
            </div>

            <div className="modal-foot">
              <button className="btn" onClick={() => setDialog(null)} disabled={busy}>
                取消
              </button>
              <button className="btn primary" onClick={() => void confirmDialog()} disabled={busy}>
                {busy ? progress || '执行中…' : dialog.kind === 'switch' ? '确认切换' : '确认同步'}
              </button>
            </div>
          </div>
        </div>
      )}

      {toast && <div className={`toast ${toast.kind}`}>{toast.text}</div>}
    </div>
  );
}

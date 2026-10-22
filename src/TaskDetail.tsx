import { useEffect, useMemo, useRef, useState } from 'react';
import { sessionDetail } from './api';
import type { TaskRow, Transcript, TranscriptEntry } from './types';

const STATUS_LABEL: Record<string, string> = {
  working: '进行中',
  completed: '已完成',
  error: '失败',
  pending: '待处理',
  planning: '规划中',
  archived: '已归档',
};

function fmtClock(sec: number): string {
  if (!sec) return '—';
  const d = new Date(sec * 1000);
  const pad = (n: number) => String(n).padStart(2, '0');
  const today = new Date().toDateString() === d.toDateString();
  return today
    ? `${pad(d.getHours())}:${pad(d.getMinutes())}`
    : `${d.getMonth() + 1}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function fmtSize(n: number): string {
  if (!n) return '—';
  if (n >= 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  if (n >= 1024) return `${Math.round(n / 1024)} KB`;
  return `${n} B`;
}

function fmtCount(n: number): string {
  return n >= 1000 ? `${(n / 1000).toFixed(1)}k` : String(n);
}

/** 只留最后两级，完整路径放 title 里 —— 抽屉底部放不下整条绝对路径。 */
function shortPath(p: string): string {
  const parts = p.split(/[\\/]/).filter(Boolean);
  return parts.length > 2 ? `…/${parts.slice(-2).join('/')}` : p;
}

interface Row {
  key: string;
  entry: TranscriptEntry;
  result: TranscriptEntry | null;
}

function ToolRow({
  entry,
  result,
}: {
  entry: TranscriptEntry;
  result: TranscriptEntry | null;
}) {
  const [open, setOpen] = useState(false);
  const clickable = !!result;
  return (
    <div className="tool-block">
      <div
        className={`tool-line ${clickable ? 'clickable' : ''}`}
        onClick={() => clickable && setOpen((v) => !v)}
        title={clickable ? '展开输出' : entry.name ?? ''}
      >
        <span className="tool-name">{entry.name ?? 'tool'}</span>
        <span className="tool-args">{entry.text || '—'}</span>
        {result && (
          <span className={`tool-status ${result.status === 'completed' ? '' : 'bad'}`}>
            {open ? '收起' : result.status ?? '结果'}
          </span>
        )}
      </div>
      {open && result && (
        <pre className="tool-output">
          {result.text || '（无输出）'}
          {result.truncated && <span className="trunc"> …已截断，仅显示前 {result.text.length} 字</span>}
        </pre>
      )}
    </div>
  );
}

export default function TaskDetail({
  task,
  accountLabel,
  onClose,
  onOpenFolder,
}: {
  task: TaskRow;
  /** 账号展示名（脱敏手机号或昵称），不传则回退 uid 前缀 */
  accountLabel?: string;
  onClose: () => void;
  onOpenFolder: (path: string) => void;
}) {
  const [data, setData] = useState<Transcript | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [filter, setFilter] = useState<'all' | 'talk' | 'tool'>('all');
  const bodyRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let alive = true;
    setLoading(true);
    setError(null);
    setData(null);
    sessionDetail(task.id)
      .then((d) => alive && setData(d))
      .catch((e) => alive && setError(e instanceof Error ? e.message : String(e)))
      .finally(() => alive && setLoading(false));
    return () => {
      alive = false;
    };
  }, [task.id]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  const entries = useMemo(() => {
    if (!data) return [];
    if (filter === 'talk') {
      return data.entries.filter((e) => e.kind === 'user' || e.kind === 'assistant');
    }
    if (filter === 'tool') {
      return data.entries.filter((e) => e.kind === 'tool' || e.kind === 'tool_result');
    }
    return data.entries;
  }, [data, filter]);

  // 把「工具调用 + 紧随其后的结果」合成一行，否则一次 Bash 会占两条高度
  const rows = useMemo<Row[]>(() => {
    const out: Row[] = [];
    for (let i = 0; i < entries.length; i++) {
      const e = entries[i];
      if (e.kind === 'tool') {
        const next = entries[i + 1];
        const result = next && next.kind === 'tool_result' ? next : null;
        if (result) i++;
        out.push({ key: `${e.ts}-${i}`, entry: e, result });
      } else if (e.kind === 'tool_result') {
        out.push({ key: `${e.ts}-${i}`, entry: e, result: null });
      } else {
        out.push({ key: `${e.ts}-${i}`, entry: e, result: null });
      }
    }
    return out;
  }, [entries]);

  useEffect(() => {
    const el = bodyRef.current;
    if (el && !loading) el.scrollTop = el.scrollHeight;
  }, [data, filter, loading]);

  const s = data?.stats;

  return (
    <div className="drawer-backdrop" onClick={onClose}>
      <aside className="drawer" onClick={(e) => e.stopPropagation()}>
        <header className="drawer-head">
          <div className="drawer-title">
            <h2>{task.title}</h2>
            <div className="drawer-sub">
              <code>{accountLabel ?? task.short_uid}</code>
              <span className={`status ${task.status}`}>
                <i />
                {STATUS_LABEL[task.status] ?? task.status}
              </span>
              <span>{fmtClock(task.updated_at)}</span>
              {data?.model && <span className="drawer-model">{data.model}</span>}
            </div>
          </div>
          <button className="icon-btn" onClick={onClose} title="关闭（Esc）">
            ×
          </button>
        </header>

        {loading && (
          <div className="drawer-loading">
            <div className="switch-spinner" />
            <p>正在读取会话正文…</p>
          </div>
        )}

        {error && <div className="error drawer-error">读取失败：{error}</div>}

        {data && (
          <>
            <div className="drawer-stats">
              <div>
                <b>{s?.user_turns ?? 0}</b>
                <span>发言</span>
              </div>
              <div>
                <b>{s?.assistant_turns ?? 0}</b>
                <span>回复</span>
              </div>
              <div>
                <b>{s?.tool_calls ?? 0}</b>
                <span>工具</span>
              </div>
              <div>
                <b>{fmtCount(s?.total_chars ?? 0)}</b>
                <span>正文字数</span>
              </div>
              <div>
                <b>{fmtCount(s?.reasoning_chars ?? 0)}</b>
                <span>思考字数</span>
              </div>
              <div>
                <b>{fmtSize(data.size_bytes)}</b>
                <span>文件</span>
              </div>
            </div>

            <div className="drawer-tools">
              <div className="filters">
                {(
                  [
                    ['all', '全部'],
                    ['talk', '对话'],
                    ['tool', '工具'],
                  ] as const
                ).map(([k, label]) => (
                  <button
                    key={k}
                    className={`chip ${filter === k ? 'on' : ''}`}
                    onClick={() => setFilter(k)}
                  >
                    {label}
                  </button>
                ))}
              </div>
              {s && s.tools.length > 0 && (
                <div className="tool-chips">
                  {s.tools.slice(0, 4).map(([name, n]) => (
                    <span key={name} className="tool-chip">
                      {name}
                      <b>{n}</b>
                    </span>
                  ))}
                </div>
              )}
            </div>

            {s && s.dropped_entries > 0 && (
              <div className="drawer-note">
                仅显示最近 {data.entries.length} 条，更早的 {s.dropped_entries} 条未加载
              </div>
            )}

            <div className="drawer-body" ref={bodyRef}>
              {rows.length === 0 && <div className="drawer-empty">这个筛选下没有内容</div>}
              {rows.map(({ key, entry, result }) => {
                if (entry.kind === 'tool') {
                  return <ToolRow key={key} entry={entry} result={result} />;
                }
                if (entry.kind === 'tool_result') {
                  return (
                    <pre className="tool-output standalone" key={key}>
                      {entry.text || '（无输出）'}
                    </pre>
                  );
                }
                return (
                  <div className={`msg ${entry.kind}`} key={key}>
                    <div className="msg-head">
                      <span className="msg-who">{entry.kind === 'user' ? '你' : 'WorkBuddy'}</span>
                      <span className="msg-time">{fmtClock(entry.ts)}</span>
                    </div>
                    <pre className="msg-text">
                      {entry.text}
                      {entry.truncated && <span className="trunc"> …已截断</span>}
                    </pre>
                  </div>
                );
              })}
            </div>
          </>
        )}

        <footer className="drawer-foot">
          <code className="drawer-path" title={data?.path ?? task.cwd}>
            {shortPath(data?.path ?? task.cwd)}
          </code>
          <button className="btn" onClick={() => onOpenFolder(task.cwd)}>
            打开目录
          </button>
        </footer>
      </aside>
    </div>
  );
}

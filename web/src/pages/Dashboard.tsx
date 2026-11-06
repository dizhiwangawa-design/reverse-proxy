import { useEffect, useState } from "react";
import { api, duration, formatNumber, formatRelative } from "../api";
import { Badge, Empty, Icon, Stat, StatusBadge } from "../components";
import type { Overview } from "../types";

export default function Dashboard({
  notify,
  onGoAccounts,
  onGoLogs,
}: {
  notify: (kind: "ok" | "err", message: string) => void;
  onGoAccounts: () => void;
  onGoLogs: () => void;
}) {
  const [data, setData] = useState<Overview | null>(null);
  const [error, setError] = useState("");
  const [importing, setImporting] = useState(false);

  const load = async () => {
    try {
      const d = await api<Overview>("/admin/api/overview");
      setData(d);
      setError("");
    } catch (e: any) {
      setError(e?.message || "加载失败");
    }
  };

  useEffect(() => {
    load();
    const t = window.setInterval(load, 5000);
    return () => window.clearInterval(t);
  }, []);

  const importLocal = async () => {
    setImporting(true);
    try {
      await api("/admin/api/accounts/import-local", { method: "POST" });
      notify("ok", "已从本机 WorkBuddy 导入账号");
      load();
    } catch (e: any) {
      notify("err", e?.message || "导入失败");
    } finally {
      setImporting(false);
    }
  };

  if (error && !data) {
    return <div className="card error-card">{error}</div>;
  }
  if (!data) {
    return <div className="card skeleton-card">加载中…</div>;
  }

  const t = data.totals;
  const successRate = t.requests ? (t.successes / t.requests) * 100 : 0;
  const avgLatency = t.requests ? t.total_latency_ms / t.requests : 0;
  const active = data.accounts.filter((a) => a.status === "active").length;
  const tokens = t.prompt_tokens + t.completion_tokens;

  return (
    <div className="stack">
      {data.accounts.length === 0 && (
        <div className="callout">
          <div className="callout-icon">
            <Icon name="zap" />
          </div>
          <div className="callout-body">
            <strong>还没有账号</strong>
            <p>
              可以直接导入本机已登录的 WorkBuddy 会话，也可以手动添加访问令牌。账号池支持轮询、失败冷却与自动刷新。
            </p>
          </div>
          <div className="callout-actions">
            <button className="primary" onClick={importLocal} disabled={importing}>
              {importing ? "导入中…" : "导入本机账号"}
            </button>
            <button className="ghost" onClick={onGoAccounts}>
              手动添加
            </button>
          </div>
        </div>
      )}

      <div className="stats-grid">
        <Stat
          label="总请求"
          value={formatNumber(t.requests)}
          sub={`成功 ${formatNumber(t.successes)} · 失败 ${formatNumber(t.failures)}`}
        />
        <Stat
          label="成功率"
          value={`${successRate.toFixed(1)}%`}
          tone={successRate >= 99 ? "tone-green" : successRate >= 95 ? "tone-amber" : "tone-red"}
          sub="按请求数统计"
        />
        <Stat
          label="Token 消耗"
          value={formatNumber(tokens)}
          sub={`输入 ${formatNumber(t.prompt_tokens)} · 输出 ${formatNumber(t.completion_tokens)}`}
        />
        <Stat label="平均延迟" value={`${avgLatency.toFixed(0)} ms`} sub="含上游排队" />
        <Stat
          label="可用账号"
          value={`${active} / ${data.accounts.length}`}
          tone={active > 0 ? "tone-green" : "tone-red"}
          sub={`冷却或异常 ${data.accounts.length - active} 个`}
        />
        <Stat label="运行时长" value={duration(data.now - data.started_at)} sub={`模型缓存 ${data.models_cached} 个`} />
      </div>

      <div className="two-col">
        <div className="card">
          <div className="card-head">
            <div>
              <h2>账号健康度</h2>
              <p className="muted">最近一次使用与错误状态</p>
            </div>
            <button className="ghost small" onClick={onGoAccounts}>
              管理账号
            </button>
          </div>
          {data.accounts.length === 0 ? (
            <Empty title="暂无账号" hint="导入本机会话或手动添加访问令牌" />
          ) : (
            <div className="list">
              {data.accounts.map((a) => {
                const total = a.stats.requests;
                const rate = total ? (a.stats.successes / total) * 100 : 100;
                return (
                  <div className="list-row" key={a.id}>
                    <div className={`avatar status-${a.status}`}>{a.name.slice(0, 1).toUpperCase()}</div>
                    <div className="list-main">
                      <div className="list-title">
                        {a.name}
                        <StatusBadge status={a.status} />
                      </div>
                      <div className="list-sub mono">{a.uid || "unknown uid"}</div>
                      {a.last_error && <div className="list-error">{a.last_error}</div>}
                    </div>
                    <div className="list-meta">
                      <div className="small">{total ? `${rate.toFixed(1)}%` : "未使用"}</div>
                      <div className="muted small">{formatRelative(a.last_used_at)}</div>
                    </div>
                  </div>
                );
              })}
            </div>
          )}
        </div>

        <div className="card">
          <div className="card-head">
            <div>
              <h2>最近请求</h2>
              <p className="muted">保留最近 100 条</p>
            </div>
            <button className="ghost small" onClick={onGoLogs}>
              查看全部
            </button>
          </div>
          {data.recent.length === 0 ? (
            <Empty title="还没有请求" hint="通过 /v1/chat/completions 发起一次调用即可看到记录" />
          ) : (
            <div className="table-wrap compact">
              <table>
                <thead>
                  <tr>
                    <th>时间</th>
                    <th>模型</th>
                    <th>账号</th>
                    <th>状态</th>
                    <th className="right">耗时</th>
                  </tr>
                </thead>
                <tbody>
                  {[...data.recent].reverse().slice(0, 12).map((r, i) => (
                    <tr key={`${r.at}-${i}`}>
                      <td className="muted small">{formatRelative(r.at)}</td>
                      <td>
                        <span className="mono small">{r.model}</span>
                        {r.stream && <span className="tag">stream</span>}
                      </td>
                      <td className="small">{r.account_name}</td>
                      <td>
                        {r.ok ? <Badge tone="green">成功</Badge> : <Badge tone="red">{r.status || "ERR"}</Badge>}
                      </td>
                      <td className="right mono small">{r.latency_ms} ms</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

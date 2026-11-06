import { useEffect, useMemo, useState } from "react";
import { api, formatNumber, formatRelative } from "../api";
import { Badge, Empty, ErrorBox, Icon, Stat } from "../components";
import type { LogsResponse } from "../types";

type Filter = "all" | "ok" | "failed";

export default function Logs({ notify }: { notify: (kind: "ok" | "err", message: string) => void }) {
  const [data, setData] = useState<LogsResponse | null>(null);
  const [error, setError] = useState("");
  const [filter, setFilter] = useState<Filter>("all");
  const [q, setQ] = useState("");
  const [auto, setAuto] = useState(true);

  const load = async (quiet = false) => {
    try {
      const d = await api<LogsResponse>("/admin/api/logs");
      setData(d);
      setError("");
    } catch (e: any) {
      if (!quiet) setError(e?.message || "加载失败");
    }
  };

  useEffect(() => {
    load();
  }, []);

  useEffect(() => {
    if (!auto) return;
    const t = window.setInterval(() => load(true), 5000);
    return () => window.clearInterval(t);
  }, [auto]);

  const rows = useMemo(() => {
    const needle = q.trim().toLowerCase();
    return [...(data?.recent ?? [])]
      .reverse()
      .filter((r) => (filter === "all" ? true : filter === "ok" ? r.ok : !r.ok))
      .filter(
        (r) =>
          !needle ||
          r.model.toLowerCase().includes(needle) ||
          r.account_name.toLowerCase().includes(needle) ||
          (r.error || "").toLowerCase().includes(needle)
      );
  }, [data, filter, q]);

  const totals = data?.totals;
  const rate = totals && totals.requests ? (totals.successes / totals.requests) * 100 : 0;
  const avg = totals && totals.requests ? totals.total_latency_ms / totals.requests : 0;
  const counts = useMemo(() => {
    const recent = data?.recent ?? [];
    return {
      all: recent.length,
      ok: recent.filter((r) => r.ok).length,
      failed: recent.filter((r) => !r.ok).length,
    };
  }, [data]);

  return (
    <div className="stack">
      <div className="stats-grid">
        <Stat label="总请求" value={formatNumber(totals?.requests ?? 0)} sub="自上次清空统计以来" />
        <Stat
          label="成功率"
          value={`${rate.toFixed(1)}%`}
          tone={rate >= 99 ? "tone-green" : rate >= 95 ? "tone-amber" : "tone-red"}
          sub={`失败 ${formatNumber(totals?.failures ?? 0)} 次`}
        />
        <Stat
          label="Token 消耗"
          value={formatNumber((totals?.prompt_tokens ?? 0) + (totals?.completion_tokens ?? 0))}
          sub={`输入 ${formatNumber(totals?.prompt_tokens ?? 0)} · 输出 ${formatNumber(
            totals?.completion_tokens ?? 0
          )}`}
        />
        <Stat label="平均延迟" value={`${avg.toFixed(0)} ms`} sub="含上游排队时间" />
      </div>

      <div className="card">
        <div className="card-head">
          <div>
            <h2>调用日志</h2>
            <p className="muted">保留最近 100 条请求，并随状态文件持久化。</p>
          </div>
          <div className="row-actions">
            <div className="search">
              <Icon name="search" size={16} />
              <input value={q} placeholder="搜索模型 / 账号 / 错误" onChange={(e) => setQ(e.target.value)} />
            </div>
            <label className="checkline">
              <input type="checkbox" checked={auto} onChange={(e) => setAuto(e.target.checked)} />
              自动刷新
            </label>
            <button
              className="ghost"
              onClick={() => {
                load();
                notify("ok", "日志已刷新");
              }}
            >
              <Icon name="refresh" size={16} />
              刷新
            </button>
          </div>
        </div>

        <div className="filter-row">
          {(
            [
              ["all", `全部 ${counts.all}`],
              ["ok", `成功 ${counts.ok}`],
              ["failed", `失败 ${counts.failed}`],
            ] as Array<[Filter, string]>
          ).map(([value, label]) => (
            <button
              key={value}
              className={`chip ${filter === value ? "chip-active" : ""}`}
              onClick={() => setFilter(value)}
            >
              {label}
            </button>
          ))}
          <span className="muted small">当前显示 {rows.length} 条</span>
        </div>

        {error && <ErrorBox message={error} />}
        {rows.length === 0 ? (
          <Empty title="没有匹配的请求" hint="调整筛选条件，或通过 /v1 接口发起一次调用" />
        ) : (
          <div className="table-wrap">
            <table className="logs-table">
              <thead>
                <tr>
                  <th>时间</th>
                  <th>模型</th>
                  <th>账号</th>
                  <th>方式</th>
                  <th>Token</th>
                  <th className="right">耗时</th>
                  <th>状态</th>
                  <th>错误</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((r, i) => (
                  <tr key={`${r.at}-${i}`}>
                    <td>
                      <div className="small">{formatRelative(r.at)}</div>
                      <div className="tiny muted mono">{new Date(r.at).toLocaleTimeString("zh-CN")}</div>
                    </td>
                    <td className="mono small">{r.model}</td>
                    <td className="small">{r.account_name || <span className="muted">-</span>}</td>
                    <td>{r.stream ? <Badge tone="blue">stream</Badge> : <Badge tone="neutral">block</Badge>}</td>
                    <td className="mono small">
                      {formatNumber(r.prompt_tokens)} / {formatNumber(r.completion_tokens)}
                    </td>
                    <td className="right mono small">{r.latency_ms} ms</td>
                    <td>
                      {r.ok ? <Badge tone="green">成功</Badge> : <Badge tone="red">{r.status || "ERR"}</Badge>}
                    </td>
                    <td>
                      {r.error ? (
                        <span className="cell-error" title={r.error}>
                          {r.error}
                        </span>
                      ) : (
                        <span className="muted">-</span>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </div>
  );
}

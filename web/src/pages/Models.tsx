import { useEffect, useMemo, useState } from "react";
import { api } from "../api";
import { Badge, Empty, ErrorBox, Icon } from "../components";
import type { ModelInfo } from "../types";

export default function Models({ notify }: { notify: (kind: "ok" | "err", message: string) => void }) {
  const [models, setModels] = useState<ModelInfo[]>([]);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [q, setQ] = useState("");

  const load = async (refresh = false) => {
    if (refresh) setRefreshing(true);
    try {
      const d = await api<{ models: ModelInfo[]; error: string | null }>(
        `/admin/api/models${refresh ? "?refresh=1" : ""}`
      );
      setModels(d.models);
      setError(d.error || "");
      if (refresh && !d.error) notify("ok", `已从上游刷新 ${d.models.length} 个模型`);
    } catch (e: any) {
      setError(e?.message || "加载失败");
    } finally {
      setLoading(false);
      setRefreshing(false);
    }
  };

  useEffect(() => {
    load(false);
  }, []);

  const filtered = useMemo(() => {
    const needle = q.trim().toLowerCase();
    if (!needle) return models;
    return models.filter(
      (m) =>
        m.id.toLowerCase().includes(needle) ||
        (m.name || "").toLowerCase().includes(needle) ||
        (m.description_zh || "").toLowerCase().includes(needle)
    );
  }, [models, q]);

  return (
    <div className="stack">
      <div className="card">
        <div className="card-head">
          <div>
            <h2>上游模型</h2>
            <p className="muted">来自 WorkBuddy 的模型目录，调用时直接把 id 填到 model 字段。</p>
          </div>
          <div className="row-actions">
            <div className="search">
              <Icon name="search" size={16} />
              <input value={q} placeholder="搜索模型" onChange={(e) => setQ(e.target.value)} />
            </div>
            <button className="ghost" onClick={() => load(true)} disabled={refreshing}>
              <Icon name="refresh" size={16} />
              {refreshing ? "刷新中…" : "刷新目录"}
            </button>
          </div>
        </div>
        {error && <ErrorBox message={error} />}
        {loading ? (
          <div className="skeleton-card">加载模型目录…</div>
        ) : filtered.length === 0 ? (
          <Empty title="没有匹配的模型" hint="清空搜索词或刷新目录后重试" />
        ) : (
          <div className="model-grid">
            {filtered.map((m) => (
              <div className="model-card" key={m.id}>
                <div className="model-top">
                  <div>
                    <div className="model-name">{m.name || m.id}</div>
                    <div className="model-id mono">{m.id}</div>
                  </div>
                  <button
                    className="icon-btn"
                    title="复制模型 id"
                    onClick={() => {
                      navigator.clipboard?.writeText(m.id);
                      notify("ok", `已复制 ${m.id}`);
                    }}
                  >
                    <Icon name="copy" size={15} />
                  </button>
                </div>
                {m.description_zh && <p className="model-desc">{m.description_zh}</p>}
                <div className="model-badges">
                  {m.supports_tools && <Badge tone="blue">工具</Badge>}
                  {m.supports_reasoning && <Badge tone="violet">推理</Badge>}
                  {m.supports_images && <Badge tone="green">视觉</Badge>}
                  {(m.tags || []).slice(0, 2).map((t) => (
                    <Badge key={t} tone="neutral">
                      {t.replace("badge:", "")}
                    </Badge>
                  ))}
                </div>
                <div className="model-meta">
                  <span>
                    上下文 <strong>{m.context_window ? `${Math.round(m.context_window / 1000)}K` : "-"}</strong>
                  </span>
                  <span>
                    输出 <strong>{m.max_output_tokens ? `${Math.round(m.max_output_tokens / 1000)}K` : "-"}</strong>
                  </span>
                </div>
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

import { useEffect, useState } from "react";
import { api, del, formatRelative, post } from "../api";
import { Badge, CopyButton, Empty, ErrorBox, Icon, Modal } from "../components";
import type { ApiKey } from "../types";

export default function Keys({ notify }: { notify: (kind: "ok" | "err", message: string) => void }) {
  const [keys, setKeys] = useState<ApiKey[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [showAdd, setShowAdd] = useState(false);
  const [name, setName] = useState("");
  const [created, setCreated] = useState<ApiKey | null>(null);

  const load = async () => {
    try {
      const d = await api<{ keys: ApiKey[] }>("/admin/api/keys");
      setKeys(d.keys);
      setError("");
    } catch (e: any) {
      setError(e?.message || "加载失败");
    } finally {
      setLoading(false);
    }
  };
  useEffect(() => {
    load();
  }, []);

  const create = async () => {
    try {
      const d = await post<{ key: ApiKey }>("/admin/api/keys", { name: name.trim() || undefined });
      setCreated(d.key);
      setShowAdd(false);
      setName("");
      notify("ok", "密钥已创建");
      load();
    } catch (e: any) {
      notify("err", e?.message || "创建失败");
    }
  };

  const remove = async (k: ApiKey) => {
    if (!window.confirm(`确定删除密钥「${k.name}」？使用该密钥的客户端会立即失效。`)) return;
    try {
      await del(`/admin/api/keys/${k.id}`);
      notify("ok", "密钥已删除");
      load();
    } catch (e: any) {
      notify("err", e?.message || "删除失败");
    }
  };

  const origin = window.location.origin;

  return (
    <div className="stack">
      <div className="card">
        <div className="card-head">
          <div>
            <h2>客户端密钥</h2>
            <p className="muted">
              其他 Agent 通过 <code>Authorization: Bearer &lt;key&gt;</code> 或{" "}
              <code>x-api-key</code> 访问本代理。
            </p>
          </div>
          <button className="primary" onClick={() => setShowAdd(true)}>
            <Icon name="plus" size={16} />
            新建密钥
          </button>
        </div>
        {error && <ErrorBox message={error} />}
        {loading ? (
          <div className="skeleton-card">加载密钥…</div>
        ) : keys.length === 0 ? (
          <Empty title="还没有密钥" hint="创建一个密钥后即可接入其他 Agent" />
        ) : (
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>名称</th>
                  <th>密钥</th>
                  <th>状态</th>
                  <th>调用次数</th>
                  <th>最近使用</th>
                  <th className="right">操作</th>
                </tr>
              </thead>
              <tbody>
                {keys.map((k) => (
                  <tr key={k.id}>
                    <td className="cell-title">{k.name}</td>
                    <td>
                      <div className="key-cell">
                        <code>{k.key}</code>
                        <CopyButton text={k.key} />
                      </div>
                    </td>
                    <td>{k.enabled ? <Badge tone="green">启用</Badge> : <Badge tone="neutral">停用</Badge>}</td>
                    <td className="mono small">{k.request_count}</td>
                    <td className="small">{formatRelative(k.last_used_at)}</td>
                    <td className="right">
                      <button className="icon-btn danger" onClick={() => remove(k)} title="删除">
                        <Icon name="trash" size={16} />
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      <div className="two-col">
        <div className="card">
          <div className="card-head">
            <div>
              <h2>OpenAI 兼容接入</h2>
              <p className="muted">适用于 Cline、Continue、OpenAI SDK、LangChain 等。</p>
            </div>
          </div>
          <div className="code-block">
            <div className="code-line c-muted"># Base URL</div>
            <div className="code-line mono">{origin}/v1</div>
            <div className="code-line c-muted" style={{ marginTop: 10 }}>
              # curl
            </div>
            <pre className="code-pre">{`curl ${origin}/v1/chat/completions \\
  -H "Authorization: Bearer $WB_KEY" \\
  -H "Content-Type: application/json" \\
  -d '{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"hi"}]}'`}</pre>
          </div>
        </div>
        <div className="card">
          <div className="card-head">
            <div>
              <h2>Anthropic 兼容接入</h2>
              <p className="muted">
                适用于 Claude Code、Anthropic SDK 等，支持 <code>x-api-key</code> 与{" "}
                <code>/v1/messages/count_tokens</code>。
              </p>
            </div>
          </div>
          <div className="code-block">
            <div className="code-line c-muted"># 环境变量</div>
            <pre className="code-pre">{`ANTHROPIC_BASE_URL=${origin}
ANTHROPIC_AUTH_TOKEN=sk-wb-...
ANTHROPIC_MODEL=deepseek-v4.1-flash`}</pre>
          </div>
        </div>
      </div>

      <Modal
        title="新建客户端密钥"
        open={showAdd}
        onClose={() => setShowAdd(false)}
        footer={
          <>
            <button className="ghost" onClick={() => setShowAdd(false)}>
              取消
            </button>
            <button className="primary" onClick={create}>
              创建
            </button>
          </>
        }
      >
        <label className="field">
          <span>名称</span>
          <input
            autoFocus
            value={name}
            placeholder="例如：claude-code / my-agent"
            onChange={(e) => setName(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && create()}
          />
        </label>
      </Modal>

      <Modal
        title="密钥已创建"
        open={!!created}
        onClose={() => setCreated(null)}
        footer={
          <button className="primary" onClick={() => setCreated(null)}>
            完成
          </button>
        }
      >
        {created && (
          <>
            <p className="muted">请立即复制保存。该密钥后续仍可在列表中查看。</p>
            <div className="key-reveal">
              <code>{created.key}</code>
              <CopyButton text={created.key} />
            </div>
          </>
        )}
      </Modal>
    </div>
  );
}

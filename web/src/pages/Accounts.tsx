import { useEffect, useState } from "react";
import { api, del, formatNumber, formatRelative, formatTime, patch, post } from "../api";
import { Badge, ErrorBox, Icon, Modal, StatusBadge } from "../components";
import type { Account } from "../types";

type Form = {
  name: string;
  access_token: string;
  refresh_token: string;
  uid: string;
  endpoint: string;
  weight: number;
};

const emptyForm: Form = {
  name: "",
  access_token: "",
  refresh_token: "",
  uid: "",
  endpoint: "",
  weight: 1,
};

export default function Accounts({ notify }: { notify: (kind: "ok" | "err", message: string) => void }) {
  const [accounts, setAccounts] = useState<Account[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState<string>("");
  const [showAdd, setShowAdd] = useState(false);
  const [form, setForm] = useState<Form>(emptyForm);
  const [addError, setAddError] = useState("");
  const [importing, setImporting] = useState(false);
  const [edit, setEdit] = useState<Account | null>(null);
  const [newToken, setNewToken] = useState("");
  const [newRefresh, setNewRefresh] = useState("");

  const load = async () => {
    try {
      const d = await api<{ accounts: Account[] }>("/admin/api/accounts");
      setAccounts(d.accounts);
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

  const importLocal = async () => {
    setImporting(true);
    try {
      const r = await post<{ account: Account }>("/admin/api/accounts/import-local");
      notify("ok", `已导入账号 ${r.account.name}`);
      load();
    } catch (e: any) {
      notify("err", e?.message || "导入失败");
    } finally {
      setImporting(false);
    }
  };

  const add = async () => {
    setAddError("");
    if (!form.access_token.trim()) {
      setAddError("访问令牌不能为空");
      return;
    }
    try {
      await post("/admin/api/accounts", {
        name: form.name.trim() || undefined,
        access_token: form.access_token.trim(),
        refresh_token: form.refresh_token.trim() || undefined,
        uid: form.uid.trim() || undefined,
        endpoint: form.endpoint.trim() || undefined,
        weight: form.weight || 1,
      });
      setShowAdd(false);
      setForm(emptyForm);
      notify("ok", "账号已添加");
      load();
    } catch (e: any) {
      setAddError(e?.message || "添加失败");
    }
  };

  const action = async (id: string, label: string, fn: () => Promise<unknown>) => {
    setBusy(id);
    try {
      await fn();
      notify("ok", label);
      await load();
    } catch (e: any) {
      notify("err", e?.message || "操作失败");
    } finally {
      setBusy("");
    }
  };

  const toggle = (a: Account) =>
    action(a.id, a.enabled ? "账号已停用" : "账号已启用", () =>
      patch(`/admin/api/accounts/${a.id}`, { enabled: !a.enabled })
    );

  const remove = (a: Account) => {
    if (!window.confirm(`确定删除账号「${a.name}」？此操作不可撤销。`)) return;
    action(a.id, "账号已删除", () => del(`/admin/api/accounts/${a.id}`));
  };

  const refresh = (a: Account) =>
    action(a.id, "令牌已刷新", () => post(`/admin/api/accounts/${a.id}/refresh`));

  const test = async (a: Account) => {
    setBusy(a.id);
    try {
      const r = await post<{ latency_ms: number }>(`/admin/api/accounts/${a.id}/test`);
      notify("ok", `连通正常，首包 ${r.latency_ms} ms`);
    } catch (e: any) {
      notify("err", e?.message || "连通测试失败");
    } finally {
      setBusy("");
    }
  };

  const openEdit = (a: Account) => {
    setEdit(a);
    setNewToken("");
    setNewRefresh("");
  };

  const saveEdit = async () => {
    if (!edit) return;
    try {
      await patch(`/admin/api/accounts/${edit.id}`, {
        name: edit.name,
        weight: edit.weight,
        access_token: newToken.trim() || undefined,
        refresh_token: newRefresh.trim() || undefined,
      });
      setEdit(null);
      setNewToken("");
      setNewRefresh("");
      notify("ok", "账号信息已更新");
      load();
    } catch (e: any) {
      notify("err", e?.message || "保存失败");
    }
  };

  if (loading) return <div className="card skeleton-card">加载账号池…</div>;

  return (
    <div className="stack">
      <div className="card">
        <div className="card-head">
          <div>
            <h2>账号池</h2>
            <p className="muted">
              每个账号对应一份 WorkBuddy 访问令牌。请求按策略轮询，失败会自动冷却并尝试下一个账号。
            </p>
          </div>
          <div className="row-actions">
            <button className="ghost" onClick={importLocal} disabled={importing}>
              <Icon name="zap" size={16} />
              {importing ? "导入中…" : "导入本机账号"}
            </button>
            <button className="primary" onClick={() => setShowAdd(true)}>
              <Icon name="plus" size={16} />
              添加账号
            </button>
          </div>
        </div>

        {error && <ErrorBox message={error} />}
        {accounts.length === 0 ? (
          <div className="empty">
            <div className="empty-title">账号池为空</div>
            <div className="empty-hint">导入本机 WorkBuddy 会话，或粘贴访问令牌手动添加。</div>
            <div className="empty-action">
              <button className="primary" onClick={importLocal}>
                导入本机账号
              </button>
            </div>
          </div>
        ) : (
          <div className="table-wrap">
            <table className="accounts-table">
              <thead>
                <tr>
                  <th>账号</th>
                  <th>状态</th>
                  <th>令牌</th>
                  <th>有效期</th>
                  <th>调用</th>
                  <th>最近使用</th>
                  <th className="right">操作</th>
                </tr>
              </thead>
              <tbody>
                {accounts.map((a) => {
                  const rate = a.stats.requests ? (a.stats.successes / a.stats.requests) * 100 : 100;
                  return (
                    <tr key={a.id}>
                      <td>
                        <div className="cell-title">{a.name}</div>
                        <div className="cell-sub mono">{a.uid || "未知 UID"}</div>
                        {a.last_error && <div className="cell-error">{a.last_error}</div>}
                      </td>
                      <td>
                        <StatusBadge status={a.status} />
                        {a.has_refresh_token && (
                          <div className="tiny muted" style={{ marginTop: 4 }}>
                            自动刷新
                          </div>
                        )}
                      </td>
                      <td className="mono small">{a.masked_token}</td>
                      <td>
                        <div className="small">{formatTime(a.expires_at)}</div>
                        <div className="tiny muted">
                          {a.expires_at && a.expires_at < Date.now()
                            ? "已过期"
                            : a.expires_at
                            ? formatRelative(a.expires_at)
                            : "未知"}
                        </div>
                      </td>
                      <td>
                        <div className="small">
                          {formatNumber(a.stats.requests)} 次 · {rate.toFixed(0)}%
                        </div>
                        <div className="tiny muted">
                          {formatNumber(a.stats.prompt_tokens + a.stats.completion_tokens)} tokens
                        </div>
                      </td>
                      <td className="small">{formatRelative(a.last_used_at)}</td>
                      <td className="right">
                        <div className="row-actions end">
                          <button className="ghost small" disabled={busy === a.id} onClick={() => test(a)}>
                            测试
                          </button>
                          <button
                            className="ghost small"
                            disabled={busy === a.id || !a.has_refresh_token}
                            onClick={() => refresh(a)}
                          >
                            刷新
                          </button>
                          <button className="ghost small" onClick={() => openEdit(a)}>
                            编辑
                          </button>
                          <button className="ghost small" onClick={() => toggle(a)}>
                            {a.enabled ? "停用" : "启用"}
                          </button>
                          <button className="icon-btn danger" onClick={() => remove(a)} title="删除">
                            <Icon name="trash" size={16} />
                          </button>
                        </div>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </div>

      <div className="card soft">
        <div className="card-head">
          <div>
            <h2>令牌从哪来</h2>
            <p className="muted">WorkBuddy 桌面端的登录会话保存在本机文件中，代理可以直接读取并自动刷新。</p>
          </div>
        </div>
        <div className="code-block">
          <div className="code-line">
            <span className="c-muted"># 本机默认路径</span>
          </div>
          <div className="code-line mono">~/.local/share/CodeBuddyExtension/Data/Public/auth/workbuddy-desktop.info</div>
          <div className="code-line c-muted" style={{ marginTop: 8 }}>
            多账号：在其他设备登录 WorkBuddy 后，把对应文件中的 accessToken / refreshToken 添加到此处即可。
          </div>
        </div>
      </div>

      <Modal
        title="添加 WorkBuddy 账号"
        open={showAdd}
        onClose={() => setShowAdd(false)}
        wide
        footer={
          <>
            <button className="ghost" onClick={() => setShowAdd(false)}>
              取消
            </button>
            <button className="primary" onClick={add}>
              添加账号
            </button>
          </>
        }
      >
        <div className="form-grid">
          <label className="field">
            <span>显示名称</span>
            <input
              value={form.name}
              placeholder="例如：主账号 / 备用账号"
              onChange={(e) => setForm({ ...form, name: e.target.value })}
            />
          </label>
          <label className="field">
            <span>UID（可选）</span>
            <input
              value={form.uid}
              placeholder="留空则从 JWT 中解析"
              onChange={(e) => setForm({ ...form, uid: e.target.value })}
            />
          </label>
          <label className="field span-2">
            <span>Access Token</span>
            <textarea
              rows={4}
              value={form.access_token}
              placeholder="粘贴 accessToken（支持带 Bearer 前缀）"
              onChange={(e) => setForm({ ...form, access_token: e.target.value })}
            />
          </label>
          <label className="field span-2">
            <span>Refresh Token（强烈建议填写，用于自动续期）</span>
            <textarea
              rows={3}
              value={form.refresh_token}
              placeholder="粘贴 refreshToken"
              onChange={(e) => setForm({ ...form, refresh_token: e.target.value })}
            />
          </label>
          <label className="field">
            <span>自定义上游地址（可选）</span>
            <input
              value={form.endpoint}
              placeholder="默认使用全局配置"
              onChange={(e) => setForm({ ...form, endpoint: e.target.value })}
            />
          </label>
          <label className="field">
            <span>权重</span>
            <input
              type="number"
              min={1}
              value={form.weight}
              onChange={(e) => setForm({ ...form, weight: Number(e.target.value) || 1 })}
            />
          </label>
        </div>
        {addError && <ErrorBox message={addError} />}
        <p className="hint">
          令牌只保存在本地 <code>data/state.json</code>（权限 600），不会发送到除 WorkBuddy 上游以外的任何服务。
        </p>
      </Modal>

      <Modal
        title="编辑账号"
        open={!!edit}
        onClose={() => setEdit(null)}
        footer={
          <>
            <button className="ghost" onClick={() => setEdit(null)}>
              取消
            </button>
            <button className="primary" onClick={saveEdit}>
              保存
            </button>
          </>
        }
      >
        {edit && (
          <div className="form-grid">
            <label className="field span-2">
              <span>显示名称</span>
              <input value={edit.name} onChange={(e) => setEdit({ ...edit, name: e.target.value })} />
            </label>
            <label className="field span-2">
              <span>权重（weighted 策略下生效）</span>
              <input
                type="number"
                min={1}
                value={edit.weight}
                onChange={(e) => setEdit({ ...edit, weight: Number(e.target.value) || 1 })}
              />
            </label>
            <label className="field span-2">
              <span>替换 Access Token（留空保持不变）</span>
              <textarea
                rows={3}
                value={newToken}
                placeholder="重新登录 WorkBuddy 后可在这里粘贴新的 accessToken"
                onChange={(e) => setNewToken(e.target.value)}
              />
            </label>
            <label className="field span-2">
              <span>替换 Refresh Token（留空保持不变）</span>
              <textarea
                rows={2}
                value={newRefresh}
                placeholder="可选；填写后会同时更新自动续期凭据"
                onChange={(e) => setNewRefresh(e.target.value)}
              />
            </label>
            <div className="meta-grid span-2">
              <div>
                <span className="muted small">UID</span>
                <div className="mono small">{edit.uid}</div>
              </div>
              <div>
                <span className="muted small">创建时间</span>
                <div className="small">{formatTime(edit.created_at)}</div>
              </div>
            </div>
          </div>
        )}
      </Modal>
    </div>
  );
}

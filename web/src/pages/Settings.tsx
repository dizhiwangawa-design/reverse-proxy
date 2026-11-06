import { useEffect, useState } from "react";
import { api, put } from "../api";
import { ErrorBox, Icon } from "../components";
import type { Info, Settings } from "../types";

const STRATEGIES = [
  {
    value: "round_robin",
    label: "轮询（round_robin）",
    desc: "按顺序把请求分发给可用账号，适合令牌额度相近的账号池。",
  },
  {
    value: "least_recent",
    label: "最少使用（least_recent）",
    desc: "优先选择最久未被使用的账号，流量分布更均匀。",
  },
  {
    value: "weighted",
    label: "加权随机（weighted）",
    desc: "按账号权重随机选择，权重越高被选中的概率越大。",
  },
] as const;

export default function SettingsPage({
  notify,
}: {
  notify: (kind: "ok" | "err", message: string) => void;
}) {
  const [settings, setSettings] = useState<Settings | null>(null);
  const [info, setInfo] = useState<Info | null>(null);
  const [error, setError] = useState("");
  const [saving, setSaving] = useState(false);

  const load = async () => {
    try {
      const [s, i] = await Promise.all([
        api<{ settings: Settings }>("/admin/api/settings"),
        api<Info>("/admin/api/info"),
      ]);
      setSettings(s.settings);
      setInfo(i);
    } catch (e: any) {
      setError(e?.message || "加载失败");
    }
  };
  useEffect(() => {
    load();
  }, []);

  const save = async () => {
    if (!settings) return;
    setSaving(true);
    try {
      const r = await put<{ settings: Settings }>("/admin/api/settings", settings);
      setSettings(r.settings);
      notify("ok", "设置已保存");
    } catch (e: any) {
      notify("err", e?.message || "保存失败");
    } finally {
      setSaving(false);
    }
  };

  if (!settings) {
    return <div className="card skeleton-card">{error || "加载设置…"}</div>;
  }

  const set = <K extends keyof Settings>(key: K, value: Settings[K]) =>
    setSettings({ ...settings, [key]: value });

  return (
    <div className="stack">
      {error && <ErrorBox message={error} />}
      <div className="card">
        <div className="card-head">
          <div>
            <h2>轮询策略</h2>
            <p className="muted">决定一个请求从哪个账号发出，以及失败后如何切换。</p>
          </div>
          <button className="primary" onClick={save} disabled={saving}>
            {saving ? "保存中…" : "保存设置"}
          </button>
        </div>
        <div className="form-grid">
          <label className="field span-2">
            <span>调度策略</span>
            <select
              value={settings.rotation_strategy}
              onChange={(e) => set("rotation_strategy", e.target.value as Settings["rotation_strategy"])}
            >
              {STRATEGIES.map((s) => (
                <option key={s.value} value={s.value}>
                  {s.label}
                </option>
              ))}
            </select>
            <small className="hint">
              {STRATEGIES.find((s) => s.value === settings.rotation_strategy)?.desc}
            </small>
          </label>
          <label className="field">
            <span>单请求最多尝试账号数</span>
            <input
              type="number"
              min={1}
              max={20}
              value={settings.max_retries}
              onChange={(e) => set("max_retries", Number(e.target.value) || 1)}
            />
          </label>
          <label className="field">
            <span>默认模型</span>
            <input
              value={settings.default_model}
              onChange={(e) => set("default_model", e.target.value)}
            />
            <small className="hint">请求未提供 model 字段时使用，例如 auto。</small>
          </label>
          <label className="field">
            <span>限流冷却（秒）</span>
            <input
              type="number"
              min={1}
              value={settings.cooldown_secs}
              onChange={(e) => set("cooldown_secs", Number(e.target.value) || 60)}
            />
            <small className="hint">上游返回 429 时，该账号暂停的时间。</small>
          </label>
          <label className="field">
            <span>失败冷却（秒）</span>
            <input
              type="number"
              min={1}
              value={settings.failure_cooldown_secs}
              onChange={(e) => set("failure_cooldown_secs", Number(e.target.value) || 15)}
            />
            <small className="hint">网络错误或 5xx 时，该账号暂停的时间。</small>
          </label>
          <label className="field">
            <span>提前刷新令牌（秒）</span>
            <input
              type="number"
              min={0}
              value={settings.refresh_before_secs}
              onChange={(e) => set("refresh_before_secs", Number(e.target.value) || 0)}
            />
            <small className="hint">距离过期还有多久时自动调用刷新接口。</small>
          </label>
          <label className="field">
            <span>本机认证文件</span>
            <input
              value={settings.local_auth_path}
              onChange={(e) => set("local_auth_path", e.target.value)}
            />
            <small className="hint">用于导入账号及同步桌面端刷新后的新令牌。</small>
          </label>
        </div>
        <div className="switch-row">
          <label className="switch">
            <input
              type="checkbox"
              checked={settings.sync_local_auth}
              onChange={(e) => set("sync_local_auth", e.target.checked)}
            />
            <span className="switch-ui" />
            <span>
              <strong>同步本机 WorkBuddy 会话</strong>
              <small>每分钟检查一次桌面端是否刷新了令牌，避免两边互相踢下线。</small>
            </span>
          </label>
          <label className="switch">
            <input
              type="checkbox"
              checked={settings.require_api_key}
              onChange={(e) => set("require_api_key", e.target.checked)}
            />
            <span className="switch-ui" />
            <span>
              <strong>要求客户端密钥</strong>
              <small>关闭后 /v1 接口无需鉴权，仅建议在本机调试时使用。</small>
            </span>
          </label>
        </div>
      </div>

      <div className="card soft">
        <div className="card-head">
          <div>
            <h2>服务信息</h2>
            <p className="muted">当前进程的上游与监听地址。</p>
          </div>
        </div>
        <div className="meta-grid">
          <div>
            <span className="muted small">上游地址</span>
            <div className="mono">{info?.upstream}</div>
          </div>
          <div>
            <span className="muted small">监听地址</span>
            <div className="mono">{info?.bind}</div>
          </div>
          <div>
            <span className="muted small">OpenAI 路径</span>
            <div className="mono">{info?.endpoints.openai.join("  ")}</div>
          </div>
          <div>
            <span className="muted small">Anthropic 路径</span>
            <div className="mono">{info?.endpoints.anthropic.join("  ")}</div>
          </div>
        </div>
        <div className="settings-note">
          <Icon name="warning" size={16} />
          修改设置会立即生效并写入 <code>data/state.json</code>。管理令牌与上游地址需通过环境变量或{" "}
          <code>config.toml</code> 修改。
        </div>
      </div>
    </div>
  );
}

import { useEffect, useMemo, useState } from "react";
import { api, getToken, post, setToken } from "./api";
import { Icon } from "./components";
import type { Info } from "./types";
import Dashboard from "./pages/Dashboard";
import Accounts from "./pages/Accounts";
import Models from "./pages/Models";
import Keys from "./pages/Keys";
import SettingsPage from "./pages/Settings";
import Playground from "./pages/Playground";
import Logs from "./pages/Logs";

type Page = "dashboard" | "accounts" | "models" | "keys" | "playground" | "logs" | "settings";

const PAGES: Array<{ id: Page; label: string; icon: string; hint: string }> = [
  { id: "dashboard", label: "运行概览", icon: "dashboard", hint: "状态与调用量" },
  { id: "accounts", label: "账号池", icon: "accounts", hint: "多账号轮询" },
  { id: "models", label: "模型目录", icon: "models", hint: "上游可用模型" },
  { id: "keys", label: "接入密钥", icon: "keys", hint: "客户端鉴权" },
  { id: "playground", label: "调试台", icon: "play", hint: "直接发起对话" },
  { id: "logs", label: "调用日志", icon: "logs", hint: "最近 100 条请求" },
  { id: "settings", label: "设置", icon: "settings", hint: "轮询与刷新策略" },
];

type Notice = { kind: "ok" | "err"; message: string };

export default function App() {
  const [token, setTokenState] = useState(getToken());
  const [info, setInfo] = useState<Info | null>(null);
  const [page, setPage] = useState<Page>("dashboard");
  const [notice, setNotice] = useState<Notice | null>(null);
  const [booting, setBooting] = useState(true);

  useEffect(() => {
    const onUnauthorized = () => {
      setToken("");
      setTokenState("");
      setInfo(null);
    };
    window.addEventListener("wb-unauthorized", onUnauthorized);
    return () => window.removeEventListener("wb-unauthorized", onUnauthorized);
  }, []);

  useEffect(() => {
    if (!token) {
      setBooting(false);
      return;
    }
    let alive = true;
    setBooting(true);
    api<Info>("/admin/api/info")
      .then((i) => {
        if (alive) {
          setInfo(i);
          setBooting(false);
        }
      })
      .catch(() => {
        if (alive) setBooting(false);
      });
    return () => {
      alive = false;
    };
  }, [token]);

  const notify = (kind: "ok" | "err", message: string) => {
    setNotice({ kind, message });
    window.setTimeout(() => setNotice(null), 4200);
  };

  const activePage = useMemo(() => PAGES.find((p) => p.id === page)!, [page]);

  if (!token) {
    return <Login onLogin={(t) => setTokenState(t)} />;
  }
  if (booting) {
    return (
      <div className="boot">
        <div className="boot-logo">WB</div>
        <div className="boot-text">正在连接 WorkBuddy Proxy…</div>
      </div>
    );
  }
  if (!info) {
    return <Login onLogin={(t) => setTokenState(t)} initialError="管理令牌无效或已过期" />;
  }

  return (
    <div className="shell">
      <aside className="sidebar">
        <div className="brand">
          <div className="brand-mark">WB</div>
          <div>
            <div className="brand-name">WorkBuddy</div>
            <div className="brand-sub">Proxy Console</div>
          </div>
        </div>
        <nav className="nav">
          {PAGES.map((p) => (
            <button
              key={p.id}
              className={`nav-item ${page === p.id ? "active" : ""}`}
              onClick={() => setPage(p.id)}
            >
              <span className="nav-icon">
                <Icon name={p.icon} />
              </span>
              <span className="nav-copy">
                <span className="nav-label">{p.label}</span>
                <span className="nav-hint">{p.hint}</span>
              </span>
            </button>
          ))}
        </nav>
        <div className="sidebar-foot">
          <div className="status-line">
            <span className="dot dot-green" />
            <span>服务正常</span>
          </div>
          <div className="mono small">{info.upstream}</div>
          <div className="muted small">v{info.version}</div>
          <button
            className="ghost logout"
            onClick={() => {
              setToken("");
              setTokenState("");
              setInfo(null);
            }}
          >
            <Icon name="logout" size={16} />
            退出管理
          </button>
        </div>
      </aside>

      <main className="main">
        <header className="topbar">
          <div>
            <div className="eyebrow">{activePage.hint}</div>
            <h1>{activePage.label}</h1>
          </div>
          <div className="topbar-right">
            <span className="pill">
              <span className="dot dot-green" />
              已连接
            </span>
            <span className="mono muted small">{info.bind}</span>
          </div>
        </header>

        <section className="content">
          {page === "dashboard" && (
            <Dashboard
              notify={notify}
              onGoAccounts={() => setPage("accounts")}
              onGoLogs={() => setPage("logs")}
            />
          )}
          {page === "accounts" && <Accounts notify={notify} />}
          {page === "models" && <Models notify={notify} />}
          {page === "keys" && <Keys notify={notify} />}
          {page === "playground" && <Playground notify={notify} />}
          {page === "logs" && <Logs notify={notify} />}
          {page === "settings" && <SettingsPage notify={notify} />}
        </section>
      </main>

      {notice && (
        <div className={`toast ${notice.kind === "ok" ? "toast-ok" : "toast-err"}`}>
          <Icon name={notice.kind === "ok" ? "check" : "warning"} size={16} />
          {notice.message}
        </div>
      )}
    </div>
  );
}

function Login({ onLogin, initialError }: { onLogin: (token: string) => void; initialError?: string }) {
  const [value, setValue] = useState("");
  const [error, setError] = useState(initialError || "");
  const [busy, setBusy] = useState(false);

  const submit = async () => {
    if (!value.trim() || busy) return;
    setBusy(true);
    setError("");
    try {
      await post("/admin/api/login", { token: value.trim() });
      setToken(value.trim());
      onLogin(value.trim());
    } catch (e: any) {
      setError(e?.message || "登录失败");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="login">
      <div className="login-card">
        <div className="login-brand">
          <div className="brand-mark large">WB</div>
          <div>
            <h1>WorkBuddy Proxy</h1>
            <p>多账号轮询 · OpenAI / Anthropic 兼容网关</p>
          </div>
        </div>
        <label className="field">
          <span>管理令牌</span>
          <input
            type="password"
            value={value}
            autoFocus
            placeholder="wbadmin_..."
            onChange={(e) => setValue(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && submit()}
          />
        </label>
        {error && <div className="login-error">{error}</div>}
        <button className="primary wide" onClick={submit} disabled={busy}>
          {busy ? "验证中…" : "进入控制台"}
        </button>
        <p className="login-hint">
          令牌位于服务端 <code>data/admin_token.txt</code>，也可通过环境变量{" "}
          <code>WB_ADMIN_TOKEN</code> 指定。
        </p>
      </div>
      <div className="login-foot">OpenAI 兼容 · Anthropic 兼容 · SSE 流式转发</div>
    </div>
  );
}

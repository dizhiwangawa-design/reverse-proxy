import { useEffect, useRef, useState } from "react";
import { api, getToken } from "../api";
import { Badge, ErrorBox, Icon } from "../components";
import type { ModelInfo } from "../types";

type Turn = { role: "user" | "assistant"; content: string };

export default function Playground({ notify }: { notify: (kind: "ok" | "err", message: string) => void }) {
  const [models, setModels] = useState<ModelInfo[]>([]);
  const [model, setModel] = useState("auto");
  const [system, setSystem] = useState("You are a helpful assistant.");
  const [prompt, setPrompt] = useState("");
  const [turns, setTurns] = useState<Turn[]>([]);
  const [stream, setStream] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [latency, setLatency] = useState<number | null>(null);
  const [usage, setUsage] = useState<{ prompt_tokens?: number; completion_tokens?: number } | null>(null);
  const [reasoning, setReasoning] = useState("");
  const abortRef = useRef<AbortController | null>(null);

  useEffect(() => {
    api<{ models: ModelInfo[] }>("/admin/api/models")
      .then((d) => {
        setModels(d.models);
        if (d.models.length && !d.models.some((m) => m.id === model)) {
          setModel(d.models[0].id);
        }
      })
      .catch(() => {});
  }, []);

  const send = async () => {
    const text = prompt.trim();
    if (!text || busy) return;
    setPrompt("");
    setError("");
    setReasoning("");
    setUsage(null);
    setLatency(null);
    const history: Turn[] = [...turns, { role: "user", content: text }];
    setTurns([...history, { role: "assistant", content: "" }]);
    setBusy(true);
    const started = performance.now();
    const controller = new AbortController();
    abortRef.current = controller;

    const messages = [
      ...(system.trim() ? [{ role: "system", content: system.trim() }] : []),
      ...history.map((t) => ({ role: t.role, content: t.content })),
    ];

    try {
      const res = await fetch("/v1/chat/completions", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          Authorization: `Bearer ${getToken()}`,
        },
        body: JSON.stringify({ model, messages, stream }),
        signal: controller.signal,
      });
      if (!res.ok) {
        const raw = await res.text();
        let msg = raw;
        try {
          msg = JSON.parse(raw)?.error?.message || raw;
        } catch {
          /* ignore */
        }
        throw new Error(msg || `HTTP ${res.status}`);
      }
      if (stream) {
        const reader = res.body?.getReader();
        if (!reader) throw new Error("响应不支持流式读取");
        const decoder = new TextDecoder();
        let buf = "";
        let acc = "";
        let think = "";
        while (true) {
          const { value, done } = await reader.read();
          if (done) break;
          buf += decoder.decode(value, { stream: true });
          const blocks = buf.split("\n\n");
          buf = blocks.pop() || "";
          for (const block of blocks) {
            for (const line of block.split("\n")) {
              if (!line.startsWith("data:")) continue;
              const data = line.slice(5).trim();
              if (data === "[DONE]") continue;
              try {
                const v = JSON.parse(data);
                const delta = v?.choices?.[0]?.delta;
                if (delta?.content) acc += delta.content;
                if (delta?.reasoning_content) think += delta.reasoning_content;
                if (v?.usage) setUsage(v.usage);
                if (delta?.content || delta?.reasoning_content) {
                  setTurns((prev) => {
                    const copy = [...prev];
                    copy[copy.length - 1] = { role: "assistant", content: acc };
                    return copy;
                  });
                  setReasoning(think);
                }
              } catch {
                /* ignore malformed chunk */
              }
            }
          }
        }
        setLatency(Math.round(performance.now() - started));
      } else {
        const data = await res.json();
        const content = data?.choices?.[0]?.message?.content || "";
        setTurns((prev) => {
          const copy = [...prev];
          copy[copy.length - 1] = { role: "assistant", content };
          return copy;
        });
        setUsage(data?.usage || null);
        setLatency(Math.round(performance.now() - started));
      }
    } catch (e: any) {
      if (e?.name === "AbortError") {
        notify("ok", "已停止生成");
      } else {
        setError(e?.message || "调用失败");
        notify("err", e?.message || "调用失败");
      }
    } finally {
      setBusy(false);
      abortRef.current = null;
    }
  };

  const stop = () => abortRef.current?.abort();

  return (
    <div className="stack">
      <div className="card">
        <div className="card-head">
          <div>
            <h2>在线调试</h2>
            <p className="muted">使用管理令牌直接调用本代理，验证账号、模型与流式输出。</p>
          </div>
          <div className="row-actions">
            <label className="inline-field">
              <span>模型</span>
              <select value={model} onChange={(e) => setModel(e.target.value)}>
                {models.map((m) => (
                  <option key={m.id} value={m.id}>
                    {m.name || m.id}
                  </option>
                ))}
                {models.length === 0 && <option value="auto">auto</option>}
              </select>
            </label>
            <label className="checkline">
              <input type="checkbox" checked={stream} onChange={(e) => setStream(e.target.checked)} />
              流式
            </label>
          </div>
        </div>

        <div className="form-grid">
          <label className="field span-2">
            <span>System Prompt</span>
            <textarea rows={2} value={system} onChange={(e) => setSystem(e.target.value)} />
          </label>
        </div>

        <div className="chat">
          {turns.length === 0 ? (
            <div className="chat-empty">
              <div className="chat-empty-mark">
                <Icon name="play" size={24} />
              </div>
              <div>输入一条消息开始调试</div>
            </div>
          ) : (
            turns.map((t, i) => (
              <div className={`bubble-row ${t.role}`} key={i}>
                <div className="bubble-avatar">{t.role === "user" ? "你" : "WB"}</div>
                <div className="bubble">
                  <div className="bubble-role">{t.role === "user" ? "User" : "Assistant"}</div>
                  <div className="bubble-text">{t.content || (busy && i === turns.length - 1 ? "…" : "")}</div>
                </div>
              </div>
            ))
          )}
          {reasoning && (
            <details className="reasoning">
              <summary>推理过程（{reasoning.length} 字符）</summary>
              <pre>{reasoning}</pre>
            </details>
          )}
        </div>

        {error && <ErrorBox message={error} />}

        <div className="composer">
          <textarea
            rows={3}
            value={prompt}
            placeholder="输入消息，Enter 发送，Shift+Enter 换行"
            onChange={(e) => setPrompt(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                send();
              }
            }}
          />
          <div className="composer-actions">
            <div className="composer-meta">
              {latency !== null && <span className="muted small">{latency} ms</span>}
              {usage?.prompt_tokens !== undefined && (
                <span className="muted small">
                  输入 {usage.prompt_tokens} · 输出 {usage.completion_tokens ?? 0} tokens
                </span>
              )}
              {busy && <Badge tone="blue">生成中</Badge>}
            </div>
            <div className="row-actions">
              <button className="ghost" onClick={() => setTurns([])} disabled={busy}>
                清空
              </button>
              {busy ? (
                <button className="danger-btn" onClick={stop}>
                  停止
                </button>
              ) : (
                <button className="primary" onClick={send} disabled={!prompt.trim()}>
                  <Icon name="arrow" size={16} />
                  发送
                </button>
              )}
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}

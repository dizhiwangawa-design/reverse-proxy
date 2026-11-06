export type AccountStatus = "active" | "cooldown" | "invalid" | "disabled";

export interface AccountStats {
  requests: number;
  successes: number;
  failures: number;
  prompt_tokens: number;
  completion_tokens: number;
  total_latency_ms: number;
}

export interface Account {
  id: string;
  name: string;
  uid: string;
  status: AccountStatus;
  enabled: boolean;
  invalid: boolean;
  masked_token: string;
  has_refresh_token: boolean;
  expires_at: number | null;
  refresh_expires_at: number | null;
  endpoint: string | null;
  weight: number;
  created_at: number;
  last_used_at: number | null;
  last_refresh_at: number | null;
  last_error: string | null;
  stats: AccountStats;
}

export interface RequestRecord {
  at: number;
  model: string;
  account_id: string;
  account_name: string;
  stream: boolean;
  ok: boolean;
  status: number;
  latency_ms: number;
  prompt_tokens: number;
  completion_tokens: number;
  error: string | null;
}

export interface Totals {
  requests: number;
  successes: number;
  failures: number;
  prompt_tokens: number;
  completion_tokens: number;
  total_latency_ms: number;
}

export interface LogsResponse {
  recent: RequestRecord[];
  totals: Totals;
}

export interface Settings {
  rotation_strategy: "round_robin" | "least_recent" | "weighted";
  max_retries: number;
  cooldown_secs: number;
  failure_cooldown_secs: number;
  refresh_before_secs: number;
  sync_local_auth: boolean;
  local_auth_path: string;
  require_api_key: boolean;
  default_model: string;
}

export interface Overview {
  started_at: number;
  now: number;
  totals: Totals;
  accounts: Array<{
    id: string;
    name: string;
    uid: string;
    status: AccountStatus;
    enabled: boolean;
    expires_at: number | null;
    last_used_at: number | null;
    last_error: string | null;
    stats: AccountStats;
  }>;
  recent: RequestRecord[];
  settings: Settings;
  upstream: { endpoint: string };
  models_cached: number;
}

export interface ModelInfo {
  id: string;
  name: string | null;
  context_window: number | null;
  max_output_tokens: number | null;
  supports_tools: boolean;
  supports_reasoning: boolean;
  supports_images: boolean;
  tags: string[];
  description_zh: string | null;
  description_en: string | null;
}

export interface ApiKey {
  id: string;
  name: string;
  key: string;
  enabled: boolean;
  created_at: number;
  last_used_at: number | null;
  request_count: number;
}

export interface Info {
  name: string;
  version: string;
  bind: string;
  upstream: string;
  started_at: number;
  now: number;
  endpoints: { openai: string[]; anthropic: string[] };
}

const TOKEN_KEY = "helix_admin_token";

export const UNAUTHORIZED_EVENT = "helix:unauthorized";

export function getToken(): string | null {
  return localStorage.getItem(TOKEN_KEY);
}

export function setToken(token: string) {
  localStorage.setItem(TOKEN_KEY, token);
}

export function clearToken() {
  localStorage.removeItem(TOKEN_KEY);
}

export class ApiError extends Error {
  status: number;

  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

async function apiFetch<T>(path: string, init?: RequestInit): Promise<T> {
  const token = getToken();
  const response = await fetch(path, {
    ...init,
    headers: {
      ...(init?.headers ?? {}),
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
    },
  });

  if (response.status === 401) {
    clearToken();
    window.dispatchEvent(new Event(UNAUTHORIZED_EVENT));
    throw new ApiError(401, "unauthorized");
  }

  if (!response.ok) {
    const body = await response.text().catch(() => "");
    throw new ApiError(response.status, body || response.statusText);
  }

  if (response.status === 204) {
    return undefined as T;
  }
  return (await response.json()) as T;
}

export interface Overview {
  num_network_validators: number;
  num_registered_validators: number;
  num_delivered_payloads: number;
  adjustments_enabled: boolean;
  kill_switch_enabled: boolean | null;
}

export interface Builder {
  pub_key: string;
  collateral: string;
  is_optimistic: boolean;
  is_optimistic_for_regional_filtering: boolean;
  builder_id: string | null;
  builder_ids: string[] | null;
}

export interface Demotion {
  public_key: string;
  demotion_time_ms: number;
  reason: string | null;
  block_hash: string | null;
  slot_number: number | null;
}

export interface DeliveredPayload {
  slot: string;
  parent_hash: string;
  block_hash: string;
  builder_pubkey: string;
  proposer_pubkey: string;
  proposer_fee_recipient: string;
  gas_limit: string;
  gas_used: string;
  value: string;
  block_number: string;
  num_tx: string;
}

export interface TrustedProposer {
  name: string;
  pubkey: string;
}

export const api = {
  overview: () => apiFetch<Overview>("/api/v1/overview"),
  builders: () => apiFetch<Builder[]>("/api/v1/builders"),
  demotions: (limit = 100) => apiFetch<Demotion[]>(`/api/v1/demotions?limit=${limit}`),
  payloads: (params: { cursor?: string; limit?: number }) => {
    const search = new URLSearchParams();
    if (params.cursor) search.set("cursor", params.cursor);
    search.set("limit", String(params.limit ?? 50));
    return apiFetch<DeliveredPayload[]>(`/api/v1/payloads?${search}`);
  },
  trustedProposers: () => apiFetch<TrustedProposer[]>("/api/v1/trusted-proposers"),

  setKillSwitch: (enabled: boolean) =>
    apiFetch<void>("/api/v1/actions/killswitch", { method: enabled ? "POST" : "DELETE" }),
  demoteBuilder: (pubkey: string, reason: string) =>
    apiFetch<void>(`/api/v1/actions/builders/${pubkey}/demote`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ reason: reason || null }),
    }),
  promoteBuilder: (pubkey: string) =>
    apiFetch<void>(`/api/v1/actions/builders/${pubkey}/promote`, { method: "POST" }),
  disableAdjustments: () =>
    apiFetch<void>("/api/v1/actions/adjustments/disable", { method: "POST" }),
};

const WEI_PER_ETH = 10n ** 18n;

export function formatEth(wei: string): string {
  let value: bigint;
  try {
    value = BigInt(wei);
  } catch {
    return wei;
  }
  const whole = value / WEI_PER_ETH;
  const frac = value % WEI_PER_ETH;
  const fracStr = frac.toString().padStart(18, "0").slice(0, 6);
  return `${whole}.${fracStr}`;
}

export function shortHex(hex: string, chars = 8): string {
  if (hex.length <= 2 + chars * 2) return hex;
  return `${hex.slice(0, 2 + chars)}…${hex.slice(-chars)}`;
}

export function formatCount(n: number): string {
  return n.toLocaleString("en-US");
}

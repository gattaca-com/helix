import { useQuery } from "@tanstack/react-query";
import { api } from "../lib/api";

export function useBuilders() {
  return useQuery({ queryKey: ["builders"], queryFn: api.builders });
}

/** Group key for a builder group: its `builder_id`, or its pubkey when ungrouped. */
export function groupKeyFor(builder: { builder_id: string | null; pub_key: string }): string {
  return builder.builder_id ? `id:${builder.builder_id}` : `pk:${builder.pub_key}`;
}

export function parseGroupKey(key: string): { builderId: string | null; pubKey: string | null } {
  if (key.startsWith("id:")) return { builderId: key.slice(3), pubKey: null };
  if (key.startsWith("pk:")) return { builderId: null, pubKey: key.slice(3) };
  return { builderId: null, pubKey: null };
}

/**
 * relay — use an api-relay instance as a pi provider, with its model list kept live.
 *
 * Models come from the relay's /v1/models, so models added upstream or in the relay's
 * admin page show up in pi without editing models.json. The last good list is cached
 * so pi still starts when the relay is unreachable.
 *
 * Config: <agent-dir>/relay.json  { "baseUrl": "https://relay.example.com", "apiKey": "sk-…" }
 * (or RELAY_BASE_URL / RELAY_API_KEY). Install: copy this file to <agent-dir>/extensions/.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

const AGENT_DIR = process.env.PI_CODING_AGENT_DIR ?? join(homedir(), ".pi", "agent");
const CONFIG_FILE = join(AGENT_DIR, "relay.json");
const CACHE_FILE = join(AGENT_DIR, "relay-models.json");

// Used when the upstream doesn't report a model's limits
const DEFAULT_CONTEXT = 200_000;
const DEFAULT_MAX_TOKENS = 32_768;

interface RelayModel {
	id: string;
	display_name?: string;
	context_length?: number;
	supports_reasoning?: boolean;
	supports_image_in?: boolean;
}

function readJson<T>(file: string): T | undefined {
	try {
		return JSON.parse(readFileSync(file, "utf8")) as T;
	} catch {
		return undefined;
	}
}

function loadConfig(): { baseUrl: string; apiKey: string } | undefined {
	const file = readJson<{ baseUrl?: string; apiKey?: string }>(CONFIG_FILE) ?? {};
	const baseUrl = process.env.RELAY_BASE_URL ?? file.baseUrl;
	const apiKey = process.env.RELAY_API_KEY ?? file.apiKey;
	return baseUrl && apiKey ? { baseUrl: baseUrl.replace(/\/+$/, ""), apiKey } : undefined;
}

async function fetchModels(baseUrl: string, apiKey: string, signal?: AbortSignal): Promise<RelayModel[]> {
	const res = await fetch(`${baseUrl}/v1/models`, {
		headers: { Authorization: `Bearer ${apiKey}` },
		signal: signal ?? AbortSignal.timeout(8000),
	});
	if (!res.ok) throw new Error(`relay /v1/models returned ${res.status}`);
	const body = (await res.json()) as { data?: RelayModel[] };
	return body.data ?? [];
}

function toPiModel(m: RelayModel) {
	return {
		id: m.id,
		name: m.display_name && m.display_name !== m.id ? `${m.id} (${m.display_name})` : m.id,
		reasoning: m.supports_reasoning ?? true,
		input: (m.supports_image_in ? ["text", "image"] : ["text"]) as ("text" | "image")[],
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
		contextWindow: m.context_length ?? DEFAULT_CONTEXT,
		maxTokens: DEFAULT_MAX_TOKENS,
	};
}

/** Fetch the live list and cache it; fall back to the cache when the relay is unreachable. */
async function currentModels(baseUrl: string, apiKey: string, signal?: AbortSignal) {
	try {
		const models = await fetchModels(baseUrl, apiKey, signal);
		writeFileSync(CACHE_FILE, JSON.stringify(models, null, 2));
		return models.map(toPiModel);
	} catch {
		return (readJson<RelayModel[]>(CACHE_FILE) ?? []).map(toPiModel);
	}
}

export default async function (pi: ExtensionAPI) {
	const config = loadConfig();
	if (!config) return;

	pi.registerProvider("relay", {
		name: "Relay",
		baseUrl: config.baseUrl,
		apiKey: config.apiKey,
		api: "anthropic-messages",
		models: await currentModels(config.baseUrl, config.apiKey),
		refreshModels: (context) => currentModels(config.baseUrl, config.apiKey, context.signal),
	});
}

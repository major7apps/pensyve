import * as http from "http";
import * as https from "https";
import * as url from "url";

/** Memory object returned by the Pensyve API. */
export interface Memory {
    id: string;
    content: string;
    memory_type: string;
    confidence: number;
    stability: number;
    score?: number;
}

/** Health check response. */
export interface HealthResponse {
    status: string;
    version: string;
}

/** Consolidation result. */
export interface ConsolidateResult {
    promoted: number;
    decayed: number;
    archived: number;
}

/** Memory statistics. */
export interface StatsResponse {
    namespace: string;
    entities: number;
    episodic_memories: number;
    semantic_memories: number;
    procedural_memories: number;
}

/**
 * Self-contained HTTP client for the Pensyve REST API.
 * Uses Node.js built-in http/https modules (no external dependencies)
 * to stay compatible with the VS Code extension runtime.
 */
export class PensyveClient {
    private baseUrl: string;
    private apiKey: string;

    constructor(baseUrl: string, apiKey: string = "") {
        this.baseUrl = baseUrl.replace(/\/$/, "");
        this.apiKey = apiKey;
    }

    /** Update the server URL (e.g., when settings change). */
    setBaseUrl(baseUrl: string): void {
        this.baseUrl = baseUrl.replace(/\/$/, "");
    }

    /** Update the API key. */
    setApiKey(apiKey: string): void {
        this.apiKey = apiKey;
    }

    /** Check server health and connectivity. */
    async health(): Promise<HealthResponse> {
        return this.request<HealthResponse>("GET", "/v1/health");
    }

    /** Recall memories matching a natural language query. */
    async recall(query: string, limit: number = 5, entity?: string): Promise<Memory[]> {
        const body: Record<string, unknown> = { query, limit };
        if (entity) {
            body.entity = entity;
        }
        return this.request<Memory[]>("POST", "/v1/recall", body);
    }

    /** Store a new fact for an entity. */
    async remember(entity: string, fact: string, confidence: number = 0.8): Promise<Memory> {
        return this.request<Memory>("POST", "/v1/remember", {
            entity,
            fact,
            confidence,
        });
    }

    /** Trigger memory consolidation. */
    async consolidate(): Promise<ConsolidateResult> {
        return this.request<ConsolidateResult>("POST", "/v1/consolidate");
    }

    /**
     * Fetch memory statistics.
     * Note: Requires the /v1/stats endpoint to be implemented on the server.
     */
    async stats(): Promise<StatsResponse> {
        return this.request<StatsResponse>("GET", "/v1/stats");
    }

    /** Low-level HTTP request helper using Node.js built-in modules. */
    private request<T>(method: string, path: string, body?: unknown): Promise<T> {
        return new Promise<T>((resolve, reject) => {
            const fullUrl = `${this.baseUrl}${path}`;
            const parsed = new url.URL(fullUrl);

            const headers: Record<string, string> = {
                "Accept": "application/json",
            };

            if (body !== undefined) {
                headers["Content-Type"] = "application/json";
            }

            if (this.apiKey) {
                headers["Authorization"] = `Bearer ${this.apiKey}`;
            }

            const options: http.RequestOptions = {
                hostname: parsed.hostname,
                port: parsed.port,
                path: parsed.pathname + parsed.search,
                method,
                headers,
                timeout: 10000,
            };

            const transport = parsed.protocol === "https:" ? https : http;

            const req = transport.request(options, (res) => {
                let data = "";
                res.on("data", (chunk: Buffer) => {
                    data += chunk.toString();
                });
                res.on("end", () => {
                    if (res.statusCode && res.statusCode >= 200 && res.statusCode < 300) {
                        try {
                            resolve(JSON.parse(data) as T);
                        } catch {
                            reject(new Error(`Invalid JSON response: ${data.slice(0, 200)}`));
                        }
                    } else {
                        reject(
                            new Error(
                                `HTTP ${res.statusCode}: ${res.statusMessage} — ${data.slice(0, 200)}`
                            )
                        );
                    }
                });
            });

            req.on("error", (err) => {
                reject(new Error(`Connection failed: ${err.message}`));
            });

            req.on("timeout", () => {
                req.destroy();
                reject(new Error("Request timed out"));
            });

            if (body !== undefined) {
                req.write(JSON.stringify(body));
            }

            req.end();
        });
    }
}

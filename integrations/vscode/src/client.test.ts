import * as assert from "node:assert/strict";
import * as http from "node:http";
import type { AddressInfo } from "node:net";
import { test } from "node:test";

import { Memory, PensyveClient } from "./client";

const memory: Memory = {
    id: "m1",
    content: "the extension talks to the gateway",
    memory_type: "semantic",
    confidence: 0.8,
    stability: 1,
    score: 0.5,
};

/** Serve one fixed JSON body for POST /v1/recall and record the request. */
async function withRecallServer(
    responseBody: unknown,
    run: (baseUrl: string, seen: { auth?: string; body?: unknown }) => Promise<void>,
): Promise<void> {
    const seen: { auth?: string; body?: unknown } = {};
    const server = http.createServer((req, res) => {
        let data = "";
        req.on("data", (chunk: Buffer) => (data += chunk.toString()));
        req.on("end", () => {
            assert.equal(req.method, "POST");
            assert.equal(req.url, "/v1/recall");
            seen.auth = req.headers.authorization;
            seen.body = JSON.parse(data);
            res.writeHead(200, { "Content-Type": "application/json" });
            res.end(JSON.stringify(responseBody));
        });
    });
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
    const { port } = server.address() as AddressInfo;
    try {
        await run(`http://127.0.0.1:${port}`, seen);
    } finally {
        await new Promise<void>((resolve) => server.close(() => resolve()));
    }
}

test("recall unwraps the gateway's { memories, contradictions } response", async () => {
    await withRecallServer({ memories: [memory], contradictions: [] }, async (baseUrl, seen) => {
        const client = new PensyveClient(baseUrl, "psy_test");
        const result = await client.recall("gateway", 3, "vscode");
        assert.deepEqual(result, [memory]);
        assert.equal(seen.auth, "Bearer psy_test");
        assert.deepEqual(seen.body, { query: "gateway", limit: 3, entity: "vscode" });
    });
});

test("recall accepts a legacy bare-array response", async () => {
    await withRecallServer([memory], async (baseUrl) => {
        const result = await new PensyveClient(baseUrl).recall("gateway");
        assert.deepEqual(result, [memory]);
    });
});

test("recall returns an empty list when the wrapper has no memories", async () => {
    await withRecallServer({ contradictions: [] }, async (baseUrl, seen) => {
        const result = await new PensyveClient(baseUrl).recall("gateway");
        assert.deepEqual(result, []);
        assert.equal(seen.auth, undefined);
    });
});

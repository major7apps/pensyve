/**
 * LangChain.js callback handler with intelligent memory capture.
 *
 * Buffers signals during chain execution and flushes at chain end.
 * Tier 1 (high-confidence) memories are auto-stored; tier 2 candidates
 * are held for review via `getPendingReview()`.
 *
 * Usage:
 *   import { PensyveCaptureHandler } from "./capture-handler";
 *   const handler = new PensyveCaptureHandler(client);
 *   await chain.invoke(inputs, { callbacks: [handler] });
 */

import { BaseCallbackHandler } from "@langchain/core/callbacks/base";
import type { Serialized } from "@langchain/core/load/serializable";
import type { LLMResult } from "@langchain/core/outputs";

import {
  type CaptureConfig,
  type RawSignal,
  MemoryCaptureCore,
} from "./_vendor/memory-capture-core";

/** Minimal client interface — matches PensyveClient from shared/ */
interface PensyveClientLike {
  remember(fact: string, confidence?: number): Promise<void>;
  entityName?: string;
  episodeStart?(participants: string[]): Promise<string>;
  episodeEnd?(episodeId: string, outcome: string): Promise<void>;
}

export class PensyveCaptureHandler extends BaseCallbackHandler {
  name = "PensyveCaptureHandler";

  private core: MemoryCaptureCore;
  private client: PensyveClientLike;
  private episodeId: string | null = null;

  constructor(client: PensyveClientLike, config?: Partial<CaptureConfig>) {
    super();
    this.core = new MemoryCaptureCore({ platform: "langchain-ts", ...config });
    this.client = client;
  }

  async handleChainStart(
    _serialized: Serialized,
    _inputs: Record<string, unknown>,
  ): Promise<void> {
    try {
      if (this.client.episodeStart) {
        this.episodeId = await this.client.episodeStart([
          "langchain",
          this.client.entityName ?? "agent",
        ]);
      }
    } catch {
      // Episode tracking is optional
    }
  }

  async handleToolEnd(output: string, _runId: string): Promise<void> {
    this.core.bufferSignal({
      type: "tool_use",
      content: output.slice(0, 512),
      timestamp: new Date().toISOString(),
      metadata: {},
    });
  }

  async handleLLMEnd(output: LLMResult): Promise<void> {
    const text =
      output.generations?.[0]?.[0]?.text ?? "";
    if (!text) return;

    // Only buffer if contains decision-like language
    const decisionKeywords = [
      "decided",
      "chose",
      "using",
      "switching",
      "let's use",
      "don't",
    ];
    const lower = text.toLowerCase();
    if (decisionKeywords.some((kw) => lower.includes(kw))) {
      this.core.bufferSignal({
        type: "user_statement",
        content: text.slice(0, 512),
        timestamp: new Date().toISOString(),
        metadata: {},
      });
    }
  }

  async handleChainError(error: Error): Promise<void> {
    this.core.bufferSignal({
      type: "error",
      content: String(error).slice(0, 512),
      timestamp: new Date().toISOString(),
      metadata: {},
    });
    // Flush and close episode on error — prevents data loss and resource leaks
    await this.flushAndClose("failure");
  }

  async handleChainEnd(
    _outputs: Record<string, unknown>,
  ): Promise<void> {
    await this.flushAndClose("success");
  }

  private async flushAndClose(outcome: string): Promise<void> {
    const [autoStore] = this.core.flush();
    for (const mem of autoStore) {
      try {
        await this.client.remember(mem.fact, mem.confidence);
      } catch {
        // Silent failure — capture should never break chains
      }
    }
    if (this.episodeId) {
      try {
        await this.client.episodeEnd?.(this.episodeId, outcome);
      } catch {
        // Silent failure
      }
      this.episodeId = null;
    }
  }

  /** Get tier 2 candidates for review. */
  getPendingReview() {
    return this.core.getPendingReview();
  }

  /** Clear reviewed candidates. */
  clearPendingReview() {
    this.core.clearPendingReview();
  }
}

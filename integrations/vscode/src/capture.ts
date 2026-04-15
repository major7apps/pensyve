import * as vscode from "vscode";
import { PensyveClient } from "./client";
import { MemoryCaptureCore, RawSignal, ClassifiedMemory } from "./memory-capture-core";

/**
 * Integrates intelligent memory capture into the VS Code extension.
 *
 * Listens for file-save events, buffers them as raw signals, and flushes
 * tier-1 memories to the Pensyve API on deactivation (or when the buffer
 * is explicitly flushed). Tier-2 candidates are logged for review.
 */
export class CaptureIntegration implements vscode.Disposable {
    private readonly core: MemoryCaptureCore;
    private readonly disposables: vscode.Disposable[] = [];

    /** Output channel shared with the rest of the extension. */
    private outputChannel: vscode.OutputChannel | undefined;

    constructor(
        private readonly client: PensyveClient,
        context: vscode.ExtensionContext
    ) {
        this.core = new MemoryCaptureCore({ platform: "vscode" });

        // Buffer a signal every time the user saves a document
        const saveWatcher = vscode.workspace.onDidSaveTextDocument((doc) => {
            this.onDocumentSaved(doc);
        });
        this.disposables.push(saveWatcher);

        context.subscriptions.push(this);
    }

    // ------------------------------------------------------------------
    // Event handlers
    // ------------------------------------------------------------------

    private onDocumentSaved(doc: vscode.TextDocument): void {
        const relativePath = vscode.workspace.asRelativePath(doc.uri, false);

        const signal: RawSignal = {
            type: "file_change",
            content: `Saved ${relativePath}`,
            timestamp: new Date().toISOString(),
            metadata: {
                file_path: relativePath,
                language_id: doc.languageId,
            },
        };

        this.core.bufferSignal(signal);
    }

    // ------------------------------------------------------------------
    // Flush — called on extension deactivation
    // ------------------------------------------------------------------

    async flush(): Promise<void> {
        if (this.core.bufferSize === 0) {
            return;
        }

        const [autoStore, review] = this.core.flush();

        // Store tier-1 memories automatically
        for (const mem of autoStore) {
            await this.storeSilently(mem);
        }

        // Log tier-2 candidates for visibility (not auto-stored)
        if (review.length > 0) {
            this.log(`[capture] ${review.length} memory candidate(s) pending review`);
            for (const mem of review) {
                this.log(`  [tier-2] ${mem.entity}: ${mem.fact}`);
            }
        }
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    private async storeSilently(mem: ClassifiedMemory): Promise<void> {
        try {
            await this.client.remember(mem.entity, mem.fact, mem.confidence);
            this.log(`[capture] Stored: ${mem.entity} — ${mem.fact}`);
        } catch {
            // Fail silently — capture should never interrupt the user's workflow
            this.log(`[capture] Failed to store memory for ${mem.entity}`);
        }
    }

    private log(message: string): void {
        if (!this.outputChannel) {
            this.outputChannel = vscode.window.createOutputChannel("Pensyve Capture");
        }
        this.outputChannel.appendLine(message);
    }

    // ------------------------------------------------------------------
    // Disposable
    // ------------------------------------------------------------------

    dispose(): void {
        for (const d of this.disposables) {
            d.dispose();
        }
        if (this.outputChannel) {
            this.outputChannel.dispose();
        }
    }
}

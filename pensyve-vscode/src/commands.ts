import * as vscode from "vscode";
import { PensyveClient } from "./client";

/** Dedicated output channel for displaying recall results. */
let outputChannel: vscode.OutputChannel | undefined;

function getOutputChannel(): vscode.OutputChannel {
    if (!outputChannel) {
        outputChannel = vscode.window.createOutputChannel("Pensyve");
    }
    return outputChannel;
}

/**
 * Recall command: prompts for a query, fetches matching memories,
 * and displays the results in the Pensyve output channel.
 */
export async function recallCommand(client: PensyveClient): Promise<void> {
    const query = await vscode.window.showInputBox({
        prompt: "Enter a query to recall memories",
        placeHolder: "e.g., What do I know about project deadlines?",
    });

    if (!query) {
        return;
    }

    const limitStr = await vscode.window.showInputBox({
        prompt: "Maximum number of results",
        placeHolder: "5",
        value: "5",
        validateInput: (val) => {
            const n = parseInt(val, 10);
            return isNaN(n) || n < 1 ? "Enter a positive number" : null;
        },
    });

    const limit = limitStr ? parseInt(limitStr, 10) : 5;

    const channel = getOutputChannel();
    channel.show(true);
    channel.appendLine(`\n--- Recall: "${query}" (limit: ${limit}) ---`);

    try {
        const memories = await client.recall(query, limit);

        if (memories.length === 0) {
            channel.appendLine("No memories found.");
            return;
        }

        for (const mem of memories) {
            channel.appendLine("");
            channel.appendLine(`  [${mem.memory_type}] ${mem.content}`);
            channel.appendLine(
                `  confidence: ${mem.confidence.toFixed(2)}  ` +
                `stability: ${mem.stability.toFixed(2)}` +
                (mem.score !== undefined ? `  score: ${mem.score.toFixed(3)}` : "")
            );
            channel.appendLine(`  id: ${mem.id}`);
        }

        channel.appendLine(`\n${memories.length} memory(ies) found.`);
    } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        channel.appendLine(`Error: ${message}`);
        vscode.window.showErrorMessage(`Pensyve recall failed: ${message}`);
    }
}

/**
 * Remember command: prompts for an entity name and a fact,
 * then stores the fact via the API.
 */
export async function rememberCommand(client: PensyveClient): Promise<void> {
    const entity = await vscode.window.showInputBox({
        prompt: "Entity name (who or what this fact is about)",
        placeHolder: "e.g., alice, project-x, my-agent",
    });

    if (!entity) {
        return;
    }

    const fact = await vscode.window.showInputBox({
        prompt: "Fact to remember",
        placeHolder: "e.g., Alice prefers Python over JavaScript",
    });

    if (!fact) {
        return;
    }

    try {
        const mem = await client.remember(entity, fact);
        vscode.window.showInformationMessage(
            `Remembered: "${mem.content}" (confidence: ${mem.confidence.toFixed(2)})`
        );
    } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        vscode.window.showErrorMessage(`Pensyve remember failed: ${message}`);
    }
}

/**
 * Stats command: fetches memory statistics and displays them
 * in an information message.
 */
export async function statsCommand(client: PensyveClient): Promise<void> {
    try {
        const stats = await client.stats();
        const lines = [
            `Namespace: ${stats.namespace}`,
            `Entities: ${stats.entities}`,
            `Episodic: ${stats.episodic_memories}`,
            `Semantic: ${stats.semantic_memories}`,
            `Procedural: ${stats.procedural_memories}`,
        ];
        vscode.window.showInformationMessage(`Pensyve Stats -- ${lines.join(" | ")}`);
    } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        vscode.window.showErrorMessage(`Pensyve stats failed: ${message}`);
    }
}

/**
 * Consolidate command: triggers memory consolidation and shows results.
 */
export async function consolidateCommand(client: PensyveClient): Promise<void> {
    try {
        const result = await client.consolidate();
        vscode.window.showInformationMessage(
            `Consolidation complete -- ` +
            `promoted: ${result.promoted}, ` +
            `decayed: ${result.decayed}, ` +
            `archived: ${result.archived}`
        );
    } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        vscode.window.showErrorMessage(`Pensyve consolidation failed: ${message}`);
    }
}

/** Dispose the output channel if it was created. */
export function disposeOutputChannel(): void {
    if (outputChannel) {
        outputChannel.dispose();
        outputChannel = undefined;
    }
}

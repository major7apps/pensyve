import * as vscode from "vscode";
import { PensyveClient } from "./client";
import { recallCommand, rememberCommand, statsCommand, consolidateCommand, disposeOutputChannel } from "./commands";
import { SidebarProvider } from "./sidebar";
import { CaptureIntegration } from "./capture";

let statusBarItem: vscode.StatusBarItem | undefined;
let client: PensyveClient;
let capture: CaptureIntegration | undefined;

/**
 * Called when the extension is activated.
 * Registers commands, initializes the sidebar provider, and creates the status bar item.
 */
export function activate(context: vscode.ExtensionContext): void {
    const config = vscode.workspace.getConfiguration("pensyve");
    const serverUrl = config.get<string>("serverUrl", "http://localhost:8000");
    const apiKey = config.get<string>("apiKey", "");

    client = new PensyveClient(serverUrl, apiKey);

    // Register commands
    context.subscriptions.push(
        vscode.commands.registerCommand("pensyve.recall", () => recallCommand(client)),
        vscode.commands.registerCommand("pensyve.remember", () => rememberCommand(client)),
        vscode.commands.registerCommand("pensyve.stats", () => statsCommand(client)),
        vscode.commands.registerCommand("pensyve.consolidate", () => consolidateCommand(client))
    );

    // Register sidebar webview provider
    const sidebarProvider = new SidebarProvider(context.extensionUri, client);
    context.subscriptions.push(
        vscode.window.registerWebviewViewProvider(SidebarProvider.viewType, sidebarProvider)
    );

    // Status bar item showing connection state
    statusBarItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100);
    statusBarItem.command = "pensyve.stats";
    statusBarItem.tooltip = "Pensyve Memory Runtime";
    context.subscriptions.push(statusBarItem);

    // Update status bar with connection info
    updateStatusBar();

    // React to configuration changes
    context.subscriptions.push(
        vscode.workspace.onDidChangeConfiguration((e) => {
            if (e.affectsConfiguration("pensyve")) {
                const updated = vscode.workspace.getConfiguration("pensyve");
                client.setBaseUrl(updated.get<string>("serverUrl", "http://localhost:8000"));
                client.setApiKey(updated.get<string>("apiKey", ""));
                updateStatusBar();
            }
        })
    );

    // Initialize intelligent memory capture (buffers file-save signals)
    capture = new CaptureIntegration(client, context);
}

/** Check server health and update the status bar text accordingly. */
async function updateStatusBar(): Promise<void> {
    if (!statusBarItem) {
        return;
    }

    statusBarItem.text = "$(sync~spin) Pensyve";
    statusBarItem.show();

    try {
        const health = await client.health();
        statusBarItem.text = `$(brain) Pensyve v${health.version}`;
        statusBarItem.backgroundColor = undefined;
    } catch {
        statusBarItem.text = "$(warning) Pensyve (offline)";
        statusBarItem.backgroundColor = new vscode.ThemeColor(
            "statusBarItem.warningBackground"
        );
    }
}

/** Called when the extension is deactivated. */
export async function deactivate(): Promise<void> {
    // Flush any buffered capture signals before shutting down
    if (capture) {
        await capture.flush();
        capture = undefined;
    }
    disposeOutputChannel();
    if (statusBarItem) {
        statusBarItem.dispose();
        statusBarItem = undefined;
    }
}

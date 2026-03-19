import * as vscode from "vscode";
import { PensyveClient, Memory } from "./client";

/**
 * Webview sidebar provider for browsing Pensyve memories.
 * Renders a simple search interface with results list and stats.
 */
export class SidebarProvider implements vscode.WebviewViewProvider {
    public static readonly viewType = "pensyve.sidebar";

    private view?: vscode.WebviewView;

    constructor(
        private readonly extensionUri: vscode.Uri,
        private readonly client: PensyveClient
    ) {}

    resolveWebviewView(
        webviewView: vscode.WebviewView,
        _context: vscode.WebviewViewResolveContext,
        _token: vscode.CancellationToken
    ): void {
        this.view = webviewView;

        webviewView.webview.options = {
            enableScripts: true,
        };

        webviewView.webview.html = this.getHtml();

        webviewView.webview.onDidReceiveMessage(async (message) => {
            switch (message.type) {
                case "search": {
                    await this.handleSearch(message.query as string);
                    break;
                }
                case "refresh-stats": {
                    await this.handleRefreshStats();
                    break;
                }
            }
        });

        // Load initial stats on open
        void this.handleRefreshStats();
    }

    private async handleSearch(query: string): Promise<void> {
        if (!this.view) {
            return;
        }

        try {
            const memories = await this.client.recall(query, 10);
            void this.view.webview.postMessage({
                type: "search-results",
                memories,
            });
        } catch (err) {
            const message = err instanceof Error ? err.message : String(err);
            void this.view.webview.postMessage({
                type: "error",
                message: `Search failed: ${message}`,
            });
        }
    }

    private async handleRefreshStats(): Promise<void> {
        if (!this.view) {
            return;
        }

        try {
            const health = await this.client.health();
            void this.view.webview.postMessage({
                type: "connection-status",
                connected: true,
                version: health.version,
            });
        } catch {
            void this.view.webview.postMessage({
                type: "connection-status",
                connected: false,
            });
        }

        try {
            const stats = await this.client.stats();
            void this.view.webview.postMessage({
                type: "stats",
                stats,
            });
        } catch {
            // Stats endpoint may not be available
        }
    }

    private getHtml(): string {
        return /* html */ `<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <style>
        body {
            font-family: var(--vscode-font-family);
            font-size: var(--vscode-font-size);
            color: var(--vscode-foreground);
            background: var(--vscode-sideBar-background);
            padding: 0 12px;
            margin: 0;
        }
        .section {
            margin-bottom: 16px;
        }
        .section-title {
            font-weight: bold;
            margin-bottom: 6px;
            font-size: 11px;
            text-transform: uppercase;
            letter-spacing: 0.5px;
            color: var(--vscode-sideBarSectionHeader-foreground);
        }
        .status {
            display: flex;
            align-items: center;
            gap: 6px;
            margin-bottom: 12px;
            font-size: 12px;
        }
        .status-dot {
            width: 8px;
            height: 8px;
            border-radius: 50%;
            display: inline-block;
        }
        .status-dot.connected { background: var(--vscode-testing-iconPassed); }
        .status-dot.disconnected { background: var(--vscode-testing-iconFailed); }
        .search-box {
            display: flex;
            gap: 4px;
            margin-bottom: 12px;
        }
        input[type="text"] {
            flex: 1;
            background: var(--vscode-input-background);
            color: var(--vscode-input-foreground);
            border: 1px solid var(--vscode-input-border);
            padding: 4px 8px;
            font-size: 13px;
            outline: none;
        }
        input[type="text"]:focus {
            border-color: var(--vscode-focusBorder);
        }
        button {
            background: var(--vscode-button-background);
            color: var(--vscode-button-foreground);
            border: none;
            padding: 4px 12px;
            cursor: pointer;
            font-size: 13px;
        }
        button:hover {
            background: var(--vscode-button-hoverBackground);
        }
        .memory-list {
            list-style: none;
            padding: 0;
            margin: 0;
        }
        .memory-item {
            padding: 8px;
            margin-bottom: 6px;
            background: var(--vscode-editor-background);
            border: 1px solid var(--vscode-panel-border);
            border-radius: 3px;
        }
        .memory-content {
            font-size: 13px;
            margin-bottom: 4px;
            word-wrap: break-word;
        }
        .memory-meta {
            font-size: 11px;
            color: var(--vscode-descriptionForeground);
        }
        .memory-type {
            display: inline-block;
            padding: 1px 5px;
            border-radius: 3px;
            font-size: 10px;
            font-weight: bold;
            text-transform: uppercase;
            margin-right: 6px;
        }
        .memory-type.episodic { background: var(--vscode-badge-background); color: var(--vscode-badge-foreground); }
        .memory-type.semantic { background: var(--vscode-statusBarItem-prominentBackground); color: var(--vscode-statusBarItem-prominentForeground); }
        .memory-type.procedural { background: var(--vscode-editorInfo-foreground); color: var(--vscode-editor-background); }
        .stats-grid {
            display: grid;
            grid-template-columns: 1fr 1fr;
            gap: 4px 12px;
            font-size: 12px;
        }
        .stats-label { color: var(--vscode-descriptionForeground); }
        .stats-value { text-align: right; font-weight: bold; }
        .empty-state {
            color: var(--vscode-descriptionForeground);
            font-style: italic;
            font-size: 12px;
            padding: 8px 0;
        }
        .error-message {
            color: var(--vscode-errorForeground);
            font-size: 12px;
            padding: 4px 0;
        }
    </style>
</head>
<body>
    <div class="section">
        <div class="status" id="connection-status">
            <span class="status-dot disconnected" id="status-dot"></span>
            <span id="status-text">Connecting...</span>
        </div>
    </div>

    <div class="section">
        <div class="section-title">Search Memories</div>
        <div class="search-box">
            <input type="text" id="search-input" placeholder="Enter query..." />
            <button id="search-btn">Search</button>
        </div>
        <div id="error-container"></div>
        <ul class="memory-list" id="results"></ul>
        <div class="empty-state" id="empty-state">Type a query to search memories</div>
    </div>

    <div class="section">
        <div class="section-title">
            Statistics
            <button id="refresh-btn" style="font-size: 11px; padding: 2px 6px; margin-left: 8px;">Refresh</button>
        </div>
        <div class="stats-grid" id="stats-grid">
            <span class="stats-label">Entities</span>
            <span class="stats-value" id="stat-entities">--</span>
            <span class="stats-label">Episodic</span>
            <span class="stats-value" id="stat-episodic">--</span>
            <span class="stats-label">Semantic</span>
            <span class="stats-value" id="stat-semantic">--</span>
            <span class="stats-label">Procedural</span>
            <span class="stats-value" id="stat-procedural">--</span>
        </div>
    </div>

    <script>
        const vscode = acquireVsCodeApi();

        const searchInput = document.getElementById("search-input");
        const searchBtn = document.getElementById("search-btn");
        const resultsEl = document.getElementById("results");
        const emptyState = document.getElementById("empty-state");
        const errorContainer = document.getElementById("error-container");
        const statusDot = document.getElementById("status-dot");
        const statusText = document.getElementById("status-text");
        const refreshBtn = document.getElementById("refresh-btn");

        searchBtn.addEventListener("click", () => {
            const query = searchInput.value.trim();
            if (query) {
                vscode.postMessage({ type: "search", query });
                emptyState.textContent = "Searching...";
                resultsEl.innerHTML = "";
                errorContainer.innerHTML = "";
            }
        });

        searchInput.addEventListener("keydown", (e) => {
            if (e.key === "Enter") {
                searchBtn.click();
            }
        });

        refreshBtn.addEventListener("click", () => {
            vscode.postMessage({ type: "refresh-stats" });
        });

        function renderMemory(mem) {
            return '<li class="memory-item">' +
                '<div class="memory-content">' +
                    '<span class="memory-type ' + escapeHtml(mem.memory_type) + '">' +
                        escapeHtml(mem.memory_type) +
                    '</span>' +
                    escapeHtml(mem.content) +
                '</div>' +
                '<div class="memory-meta">' +
                    'conf: ' + mem.confidence.toFixed(2) +
                    ' | stab: ' + mem.stability.toFixed(2) +
                    (mem.score !== undefined && mem.score !== null
                        ? ' | score: ' + mem.score.toFixed(3)
                        : '') +
                '</div>' +
            '</li>';
        }

        function escapeHtml(text) {
            const div = document.createElement("div");
            div.textContent = text;
            return div.innerHTML;
        }

        window.addEventListener("message", (event) => {
            const msg = event.data;
            switch (msg.type) {
                case "search-results": {
                    const memories = msg.memories;
                    if (memories.length === 0) {
                        emptyState.textContent = "No memories found.";
                        resultsEl.innerHTML = "";
                    } else {
                        emptyState.textContent = "";
                        resultsEl.innerHTML = memories.map(renderMemory).join("");
                    }
                    break;
                }
                case "connection-status": {
                    if (msg.connected) {
                        statusDot.className = "status-dot connected";
                        statusText.textContent = "Connected (v" + msg.version + ")";
                    } else {
                        statusDot.className = "status-dot disconnected";
                        statusText.textContent = "Disconnected";
                    }
                    break;
                }
                case "stats": {
                    const s = msg.stats;
                    document.getElementById("stat-entities").textContent = s.entities;
                    document.getElementById("stat-episodic").textContent = s.episodic_memories;
                    document.getElementById("stat-semantic").textContent = s.semantic_memories;
                    document.getElementById("stat-procedural").textContent = s.procedural_memories;
                    break;
                }
                case "error": {
                    emptyState.textContent = "";
                    errorContainer.innerHTML =
                        '<div class="error-message">' + escapeHtml(msg.message) + '</div>';
                    break;
                }
            }
        });
    </script>
</body>
</html>`;
    }
}

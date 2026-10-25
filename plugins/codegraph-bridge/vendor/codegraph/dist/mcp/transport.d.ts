/**
 * MCP Stdio Transport
 *
 * Handles JSON-RPC 2.0 communication over stdin/stdout for MCP protocol.
 */
/**
 * JSON-RPC 2.0 Request
 */
export interface JsonRpcRequest {
    jsonrpc: '2.0';
    id: string | number;
    method: string;
    params?: unknown;
}
/**
 * JSON-RPC 2.0 Response
 */
export interface JsonRpcResponse {
    jsonrpc: '2.0';
    id: string | number | null;
    result?: unknown;
    error?: JsonRpcError;
}
/**
 * JSON-RPC 2.0 Error
 */
export interface JsonRpcError {
    code: number;
    message: string;
    data?: unknown;
}
/**
 * JSON-RPC 2.0 Notification (no id, no response expected)
 */
export interface JsonRpcNotification {
    jsonrpc: '2.0';
    method: string;
    params?: unknown;
}
export declare const ErrorCodes: {
    readonly ParseError: -32700;
    readonly InvalidRequest: -32600;
    readonly MethodNotFound: -32601;
    readonly InvalidParams: -32602;
    readonly InternalError: -32603;
};
export type MessageHandler = (message: JsonRpcRequest | JsonRpcNotification) => Promise<void>;
/**
 * Stdio Transport for MCP
 *
 * Reads JSON-RPC messages from stdin and writes responses to stdout.
 */
export declare class StdioTransport {
    private rl;
    private messageHandler;
    private pending;
    private nextRequestId;
    /**
     * Start listening for messages on stdin
     */
    start(handler: MessageHandler): void;
    /**
     * Stop listening
     */
    stop(): void;
    /**
     * Send a server-initiated request to the client and await its response.
     *
     * MCP is bidirectional: the server can ask the client questions too. We use
     * this for `roots/list` — the spec-blessed way to learn the workspace root
     * when the client didn't pass one in `initialize` (see issue #196). Rejects
     * on timeout so callers can fall back rather than hang forever.
     */
    request(method: string, params?: unknown, timeoutMs?: number): Promise<unknown>;
    /**
     * Send a response
     */
    send(response: JsonRpcResponse): void;
    /**
     * Send a notification (no id)
     */
    notify(method: string, params?: unknown): void;
    /**
     * Send a success response
     */
    sendResult(id: string | number, result: unknown): void;
    /**
     * Send an error response
     */
    sendError(id: string | number | null, code: number, message: string, data?: unknown): void;
    /**
     * Handle an incoming line of JSON
     */
    private handleLine;
    /**
     * Resolve (or reject) the pending server-initiated request matching this
     * response's id. Unknown ids are ignored — the client may echo something we
     * never sent, or a request may have already timed out.
     */
    private handleResponse;
    /**
     * Check if message is a valid JSON-RPC 2.0 message
     */
    private isValidMessage;
}
//# sourceMappingURL=transport.d.ts.map
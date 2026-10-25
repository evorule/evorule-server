"use strict";
/**
 * MCP Stdio Transport
 *
 * Handles JSON-RPC 2.0 communication over stdin/stdout for MCP protocol.
 */
var __createBinding = (this && this.__createBinding) || (Object.create ? (function(o, m, k, k2) {
    if (k2 === undefined) k2 = k;
    var desc = Object.getOwnPropertyDescriptor(m, k);
    if (!desc || ("get" in desc ? !m.__esModule : desc.writable || desc.configurable)) {
      desc = { enumerable: true, get: function() { return m[k]; } };
    }
    Object.defineProperty(o, k2, desc);
}) : (function(o, m, k, k2) {
    if (k2 === undefined) k2 = k;
    o[k2] = m[k];
}));
var __setModuleDefault = (this && this.__setModuleDefault) || (Object.create ? (function(o, v) {
    Object.defineProperty(o, "default", { enumerable: true, value: v });
}) : function(o, v) {
    o["default"] = v;
});
var __importStar = (this && this.__importStar) || (function () {
    var ownKeys = function(o) {
        ownKeys = Object.getOwnPropertyNames || function (o) {
            var ar = [];
            for (var k in o) if (Object.prototype.hasOwnProperty.call(o, k)) ar[ar.length] = k;
            return ar;
        };
        return ownKeys(o);
    };
    return function (mod) {
        if (mod && mod.__esModule) return mod;
        var result = {};
        if (mod != null) for (var k = ownKeys(mod), i = 0; i < k.length; i++) if (k[i] !== "default") __createBinding(result, mod, k[i]);
        __setModuleDefault(result, mod);
        return result;
    };
})();
Object.defineProperty(exports, "__esModule", { value: true });
exports.StdioTransport = exports.ErrorCodes = void 0;
const readline = __importStar(require("readline"));
// Standard JSON-RPC error codes
exports.ErrorCodes = {
    ParseError: -32700,
    InvalidRequest: -32600,
    MethodNotFound: -32601,
    InvalidParams: -32602,
    InternalError: -32603,
};
/**
 * Stdio Transport for MCP
 *
 * Reads JSON-RPC messages from stdin and writes responses to stdout.
 */
class StdioTransport {
    rl = null;
    messageHandler = null;
    // Outstanding server-initiated requests (e.g. roots/list), keyed by the id
    // we sent. Responses from the client are matched back here.
    pending = new Map();
    nextRequestId = 1;
    /**
     * Start listening for messages on stdin
     */
    start(handler) {
        this.messageHandler = handler;
        this.rl = readline.createInterface({
            input: process.stdin,
            output: process.stdout,
            terminal: false,
        });
        this.rl.on('line', async (line) => {
            await this.handleLine(line);
        });
        this.rl.on('close', () => {
            process.exit(0);
        });
    }
    /**
     * Stop listening
     */
    stop() {
        // Fail any in-flight server-initiated requests so their awaiters don't hang.
        for (const { reject } of this.pending.values()) {
            reject(new Error('Transport stopped'));
        }
        this.pending.clear();
        if (this.rl) {
            this.rl.close();
            this.rl = null;
        }
    }
    /**
     * Send a server-initiated request to the client and await its response.
     *
     * MCP is bidirectional: the server can ask the client questions too. We use
     * this for `roots/list` — the spec-blessed way to learn the workspace root
     * when the client didn't pass one in `initialize` (see issue #196). Rejects
     * on timeout so callers can fall back rather than hang forever.
     */
    request(method, params, timeoutMs = 5000) {
        const id = `cg-srv-${this.nextRequestId++}`;
        return new Promise((resolve, reject) => {
            const timer = setTimeout(() => {
                this.pending.delete(id);
                reject(new Error(`Timed out after ${timeoutMs}ms waiting for "${method}" response`));
            }, timeoutMs);
            // Don't let a pending request keep the process alive on shutdown.
            timer.unref?.();
            this.pending.set(id, {
                resolve: (value) => { clearTimeout(timer); resolve(value); },
                reject: (error) => { clearTimeout(timer); reject(error); },
            });
            process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
        });
    }
    /**
     * Send a response
     */
    send(response) {
        const json = JSON.stringify(response);
        process.stdout.write(json + '\n');
    }
    /**
     * Send a notification (no id)
     */
    notify(method, params) {
        const notification = {
            jsonrpc: '2.0',
            method,
            params,
        };
        process.stdout.write(JSON.stringify(notification) + '\n');
    }
    /**
     * Send a success response
     */
    sendResult(id, result) {
        this.send({
            jsonrpc: '2.0',
            id,
            result,
        });
    }
    /**
     * Send an error response
     */
    sendError(id, code, message, data) {
        this.send({
            jsonrpc: '2.0',
            id,
            error: { code, message, data },
        });
    }
    /**
     * Handle an incoming line of JSON
     */
    async handleLine(line) {
        const trimmed = line.trim();
        if (!trimmed)
            return;
        let parsed;
        try {
            parsed = JSON.parse(trimmed);
        }
        catch {
            this.sendError(null, exports.ErrorCodes.ParseError, 'Parse error: invalid JSON');
            return;
        }
        // Response to a server-initiated request (has id + result/error, no method).
        // Route it to the awaiting requester instead of the message handler — these
        // used to be dropped as "Invalid Request" because they carry no method.
        const obj = parsed;
        if (obj?.jsonrpc === '2.0' &&
            typeof obj.method !== 'string' &&
            'id' in obj &&
            ('result' in obj || 'error' in obj)) {
            this.handleResponse(obj);
            return;
        }
        // Validate basic JSON-RPC structure
        if (!this.isValidMessage(parsed)) {
            this.sendError(null, exports.ErrorCodes.InvalidRequest, 'Invalid Request: not a valid JSON-RPC 2.0 message');
            return;
        }
        if (this.messageHandler) {
            try {
                await this.messageHandler(parsed);
            }
            catch (err) {
                const message = parsed;
                if ('id' in message) {
                    this.sendError(message.id, exports.ErrorCodes.InternalError, `Internal error: ${err instanceof Error ? err.message : String(err)}`);
                }
            }
        }
    }
    /**
     * Resolve (or reject) the pending server-initiated request matching this
     * response's id. Unknown ids are ignored — the client may echo something we
     * never sent, or a request may have already timed out.
     */
    handleResponse(msg) {
        const id = msg.id;
        const pending = this.pending.get(id);
        if (!pending)
            return;
        this.pending.delete(id);
        if ('error' in msg && msg.error) {
            const err = msg.error;
            pending.reject(new Error(err.message || 'Request failed'));
        }
        else {
            pending.resolve(msg.result);
        }
    }
    /**
     * Check if message is a valid JSON-RPC 2.0 message
     */
    isValidMessage(msg) {
        if (typeof msg !== 'object' || msg === null)
            return false;
        const obj = msg;
        if (obj.jsonrpc !== '2.0')
            return false;
        if (typeof obj.method !== 'string')
            return false;
        return true;
    }
}
exports.StdioTransport = StdioTransport;
//# sourceMappingURL=transport.js.map
"use strict";
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
exports.createShimmerProgress = createShimmerProgress;
const worker_threads_1 = require("worker_threads");
const path = __importStar(require("path"));
const PHASE_NAMES = {
    scanning: 'Scanning files',
    parsing: 'Parsing code',
    storing: 'Storing data',
    resolving: 'Resolving refs',
};
function createShimmerProgress() {
    let lastPhase = '';
    const workerPath = path.join(__dirname, 'shimmer-worker.js');
    const worker = new worker_threads_1.Worker(workerPath, {
        workerData: { startTime: Date.now() },
    });
    return {
        onProgress(progress) {
            const phaseName = PHASE_NAMES[progress.phase] || progress.phase;
            if (progress.phase !== lastPhase && lastPhase) {
                worker.postMessage({ type: 'finish-phase' });
            }
            lastPhase = progress.phase;
            let percent = -1;
            let count = 0;
            if (progress.total > 0) {
                percent = Math.round((progress.current / progress.total) * 100);
            }
            else if (progress.current > 0) {
                count = progress.current;
            }
            worker.postMessage({
                type: 'update',
                phase: progress.phase,
                phaseName,
                percent,
                count,
            });
        },
        stop() {
            return new Promise((resolve) => {
                const timeout = setTimeout(() => {
                    worker.terminate().then(() => resolve());
                }, 2000);
                worker.on('message', (msg) => {
                    if (msg.type === 'stopped') {
                        clearTimeout(timeout);
                        worker.terminate().then(() => resolve());
                    }
                });
                worker.postMessage({ type: 'stop' });
            });
        },
    };
}
//# sourceMappingURL=shimmer-progress.js.map
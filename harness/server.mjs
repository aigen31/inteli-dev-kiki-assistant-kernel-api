// Kiki harness runner: one HTTP hop around `dsh --profile headless`.
//
// It owns process execution, isolated workspaces and event capture. The kernel
// submits a task and polls the result, so a multi-minute site study never holds
// a chat request open. No third-party dependencies: the container ships node
// and the dsh CLI, nothing else.

import { spawn, spawnSync } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { mkdir, readFile, readdir, rm, writeFile } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import http from 'node:http';
import path from 'node:path';

const PORT = Number(process.env.PORT || 8090);
const DATA_DIR = process.env.DATA_DIR || '/data';
const DSH_BIN = process.env.DSH_BIN || 'dsh';
const DSH_PATCH = process.env.DSH_PATCH || '/app/lean.yml';
const DSH_MCP_PATCH = path.join(process.env.DATA_DIR || '/data', 'mcp.yml');
const DSH_HOME = process.env.DSH_HOME || path.join(DATA_DIR, 'dsh-home');
const TOKEN = process.env.HARNESS_TOKEN || '';
const CRM_MCP_URL = process.env.CRM_MCP_URL || '';
const CRM_MCP_TOKEN = process.env.CRM_MCP_TOKEN || '';
const API_KEY = process.env.DEEPSEEK_API_KEY || process.env.KERNEL_CLOUD_API_KEY || '';
const MAX_CONCURRENT = Math.max(1, Number(process.env.MAX_CONCURRENT || 2));
const DEFAULT_TIMEOUT = Math.min(900, Math.max(30, Number(process.env.DEFAULT_TIMEOUT || 300)));
const MAX_EVENTS = 2000;
const MAX_FILE_BYTES = 400_000;
const MAX_FILES = 12;
const FILE_EXTENSIONS = new Set(['.md', '.txt', '.json', '.yaml', '.yml']);

const WORKSPACES = path.join(DATA_DIR, 'workspaces');
const RUNS_DIR = path.join(DATA_DIR, 'runs');
const runs = new Map();
const queue = [];
let active = 0;

function log(level, message, extra = {}) {
    process.stdout.write(`${JSON.stringify({ level, message, ...extra, at: new Date().toISOString() })}\n`);
}

function cors(response) {
    response.setHeader('access-control-allow-origin', '*');
    response.setHeader('access-control-allow-headers', 'authorization, content-type, x-correlation-id');
    response.setHeader('access-control-allow-methods', 'GET, POST, OPTIONS');
}

function json(response, status, payload) {
    const body = Buffer.from(JSON.stringify(payload));
    response.writeHead(status, { 'content-type': 'application/json', 'content-length': body.length });
    response.end(body);
}

function authorized(request) {
    if (TOKEN === '') return false;
    const header = String(request.headers.authorization || '');
    const provided = header.startsWith('Bearer ') ? header.slice(7).trim() : '';
    if (provided.length !== TOKEN.length) return false;
    let diff = 0;
    for (let index = 0; index < TOKEN.length; index += 1) {
        diff |= TOKEN.charCodeAt(index) ^ provided.charCodeAt(index);
    }
    return diff === 0;
}

function readBody(request, limit = 1_000_000) {
    return new Promise((resolve, reject) => {
        let raw = '';
        request.on('data', (chunk) => {
            raw += chunk;
            if (raw.length > limit) {
                reject(new Error('body too large'));
                request.destroy();
            }
        });
        request.on('end', () => {
            try {
                resolve(raw ? JSON.parse(raw) : {});
            } catch {
                reject(new Error('invalid json'));
            }
        });
        request.on('error', reject);
    });
}

function safeName(name) {
    return /^[A-Za-z0-9._-]{1,80}$/.test(name) && !name.includes('..');
}

/**
 * Give the headless profile the CRM MCP server. DSH reads settings from the
 * active profile home; the volume is fresh on first boot, so write it here.
 */
async function ensureMcpPatch() {
    if (CRM_MCP_URL === '' || CRM_MCP_TOKEN === '') {
        log('warn', 'CRM MCP is not configured');
        return;
    }

    const patch = [
        '- insert:',
        '    - id: mcp-crm',
        "      name: '@deepseek-ai/dsh-mcp-client'",
        '      config:',
        '        transport: streamable-http',
        '        serverName: crm',
        `        url: ${JSON.stringify(CRM_MCP_URL)}`,
        '        headers:',
        `          Authorization: ${JSON.stringify('Bearer ' + CRM_MCP_TOKEN)}`,
        '        failOnStartupError: false',
        '',
    ].join('\n');

    await writeFile(DSH_MCP_PATCH, patch, 'utf8');
    log('info', 'CRM MCP configured', { url: CRM_MCP_URL, patch: DSH_MCP_PATCH });
}

async function persist(run) {
    const payload = {
        id: run.id,
        status: run.status,
        created_at: run.created_at,
        started_at: run.started_at,
        finished_at: run.finished_at,
        final: run.final,
        error: run.error,
        exit_code: run.exit_code,
        usage: run.usage,
        events: run.events.slice(-MAX_EVENTS),
        files: run.files,
        task: run.task.slice(0, 2000),
    };
    await writeFile(path.join(RUNS_DIR, `${run.id}.json`), JSON.stringify(payload)).catch((error) =>
        log('warn', 'cannot persist run', { run_id: run.id, error: error.message }),
    );
}

async function collectFiles(workspace) {
    const files = {};
    const markdownNames = new Set(['AGENTS.md', 'PRICE-NOTES.md', 'BRIEF.md']);

    async function walk(dir, depth) {
        if (depth > 2 || Object.keys(files).length >= MAX_FILES) return;
        const entries = await readdir(dir, { withFileTypes: true }).catch(() => []);
        for (const entry of entries) {
            if (Object.keys(files).length >= MAX_FILES) return;
            if (entry.name.startsWith('.')) continue;
            const full = path.join(dir, entry.name);
            if (entry.isDirectory()) {
                if (entry.name === 'node_modules' || entry.name === '.git') continue;
                await walk(full, depth + 1);
                continue;
            }
            const extension = path.extname(entry.name).toLowerCase();
            if (!FILE_EXTENSIONS.has(extension) && !markdownNames.has(entry.name)) continue;
            const relative = path.relative(workspace, full);
            if (relative === 'BRIEF.md') continue;
            const content = await readFile(full, 'utf8').catch(() => '');
            files[relative] = content.slice(0, MAX_FILE_BYTES);
        }
    }

    await walk(workspace, 0);
    return files;
}

function lastError(event) {
    const reason = event?.reason;
    if (!reason) return null;
    if (reason.kind === 'completed') return null;
    if (reason.error?.message) return String(reason.error.message).slice(0, 500);
    return reason.kind ? String(reason.kind) : null;
}

function handleEvent(run, event) {
    run.events.push(event);
    if (run.events.length > MAX_EVENTS) run.events.splice(0, run.events.length - MAX_EVENTS);

    if (event.type === 'final' && typeof event.text === 'string') {
        run.final = event.text;
    }

    if (event.type === 'status' && event.phase === 'step_end' && event.usage) {
        run.usage.input += Number(event.usage.inputTokens || 0);
        run.usage.output += Number(event.usage.outputTokens || 0);
        run.usage.cache_read += Number(event.usage.cacheReadTokens || 0);
    }

    if (event.type === 'status' && event.phase === 'turn_end') {
        const error = lastError(event);
        if (error) run.error = error;
    }
}

function finish(run, status, exitCode) {
    run.status = status;
    run.exit_code = exitCode;
    run.finished_at = new Date().toISOString();
    run.child = null;

    collectFiles(run.workspace)
        .then((files) => {
            run.files = files;
        })
        .catch(() => {
            run.files = {};
        })
        .finally(() => {
            persist(run).catch(() => {});
            active -= 1;
            drain();
            log('info', 'run finished', {
                run_id: run.id,
                status: run.status,
                duration_ms: Date.parse(run.finished_at) - Date.parse(run.started_at || run.created_at),
                usage: run.usage,
            });
        });
}

function launch(run) {
    active += 1;
    run.status = 'running';
    run.started_at = new Date().toISOString();

    const timeoutMs = run.timeout_seconds * 1000;
    const child = spawn(
        DSH_BIN,
        ['--profile', 'headless', '--patch', DSH_PATCH, '--patch', DSH_MCP_PATCH, '--json', run.task],
        {
            cwd: run.workspace,
            env: {
                ...process.env,
                HOME: process.env.HOME || '/home/node',
                DSH_HOME,
                DEEPSEEK_API_KEY: API_KEY,
                NO_COLOR: '1',
                TERM: 'dumb',
            },
            stdio: ['ignore', 'pipe', 'pipe'],
        },
    );

    run.child = child;
    let buffer = '';
    let stderr = '';

    child.stdout.on('data', (chunk) => {
        buffer += chunk.toString('utf8');
        let index = buffer.indexOf('\n');
        while (index >= 0) {
            const line = buffer.slice(0, index).trim();
            buffer = buffer.slice(index + 1);
            if (line) {
                try {
                    handleEvent(run, JSON.parse(line));
                } catch {
                    // A malformed line is a dsh diagnostic, not an event.
                }
            }
            index = buffer.indexOf('\n');
        }
    });

    child.stderr.on('data', (chunk) => {
        stderr += chunk.toString('utf8');
        if (stderr.length > 8000) stderr = stderr.slice(-8000);
    });

    const timer = setTimeout(() => {
        if (run.child) {
            run.error = `timeout after ${run.timeout_seconds}s`;
            run.timed_out = true;
            run.child.kill('SIGKILL');
        }
    }, timeoutMs);

    child.on('error', (error) => {
        clearTimeout(timer);
        run.error = `cannot start dsh: ${error.message}`;
        finish(run, 'failed', null);
    });

    child.on('close', (code) => {
        clearTimeout(timer);
        if (buffer.trim()) {
            try {
                handleEvent(run, JSON.parse(buffer.trim()));
            } catch {
                // ignore
            }
        }

        if (run.status === 'cancelled' || run.timed_out) {
            finish(run, run.timed_out ? 'failed' : 'cancelled', code);
            return;
        }

        if (code === 0 && !run.error) {
            finish(run, 'completed', code);
            return;
        }

        if (!run.error) {
            run.error = stderr.trim().split('\n').slice(-4).join('\n').slice(0, 500) || `dsh exited with ${code}`;
        }

        finish(run, 'failed', code);
    });

    log('info', 'run started', { run_id: run.id, timeout_seconds: run.timeout_seconds });
}

function drain() {
    while (active < MAX_CONCURRENT && queue.length > 0) {
        const runId = queue.shift();
        const run = runs.get(runId);
        if (run && run.status === 'queued') launch(run);
    }
}

async function createRun(payload, correlationId) {
    const task = typeof payload.task === 'string' ? payload.task.trim() : '';
    if (task === '' || task.length > 20_000) throw new Error('task must be 1..20000 characters');

    const timeout = Math.min(900, Math.max(30, Number(payload.timeout_seconds || DEFAULT_TIMEOUT)));
    const seed = payload.seed && typeof payload.seed === 'object' ? payload.seed : {};
    const entries = Object.entries(seed).slice(0, 10);
    if (entries.length !== Object.keys(seed).length) throw new Error('too many seed files');

    for (const [name, content] of entries) {
        if (!safeName(name)) throw new Error(`invalid seed filename: ${name}`);
        if (typeof content !== 'string' || content.length > 400_000) {
            throw new Error(`invalid seed content for ${name}`);
        }
    }

    const id = randomUUID();
    const workspace = path.join(WORKSPACES, id);
    await mkdir(workspace, { recursive: true });
    for (const [name, content] of entries) {
        await writeFile(path.join(workspace, name), content, 'utf8');
    }

    const run = {
        id,
        task,
        workspace,
        status: 'queued',
        created_at: new Date().toISOString(),
        started_at: null,
        finished_at: null,
        events: [],
        final: '',
        files: {},
        error: null,
        exit_code: null,
        usage: { input: 0, output: 0, cache_read: 0 },
        timeout_seconds: timeout,
        correlation_id: correlationId,
        child: null,
    };

    runs.set(id, run);
    queue.push(id);
    await persist(run);
    drain();

    return run;
}

function publicRun(run) {
    return {
        id: run.id,
        status: run.status,
        created_at: run.created_at,
        started_at: run.started_at,
        finished_at: run.finished_at,
        final: run.final,
        files: run.files,
        error: run.error,
        exit_code: run.exit_code,
        usage: run.usage,
        events: run.events.slice(-400),
    };
}

async function restore() {
    await mkdir(WORKSPACES, { recursive: true });
    await mkdir(RUNS_DIR, { recursive: true });
    await mkdir(DSH_HOME, { recursive: true });

    const entries = await readdir(RUNS_DIR).catch(() => []);
    for (const entry of entries) {
        if (!entry.endsWith('.json')) continue;
        const raw = await readFile(path.join(RUNS_DIR, entry), 'utf8').catch(() => null);
        if (!raw) continue;
        try {
            const saved = JSON.parse(raw);
            if (saved.status === 'running' || saved.status === 'queued') {
                saved.status = 'failed';
                saved.error = 'runner restarted while the study was running';
                saved.finished_at = new Date().toISOString();
                await writeFile(path.join(RUNS_DIR, entry), JSON.stringify(saved));
            }
        } catch {
            // ignore a broken snapshot
        }
    }

    const workspaces = await readdir(WORKSPACES).catch(() => []);
    for (const name of workspaces.sort().reverse().slice(30)) {
        await rm(path.join(WORKSPACES, name), { recursive: true, force: true }).catch(() => {});
    }
}

function checkReady() {
    if (!API_KEY) return 'DEEPSEEK_API_KEY is not set';
    if (!TOKEN) return 'HARNESS_TOKEN is not set';
    if (!existsSync(WORKSPACES)) return 'workspace directory is missing';
    const probe = spawnSync(DSH_BIN, ['--version'], { timeout: 10_000, encoding: 'utf8' });
    if (probe.status !== 0) return 'dsh CLI is not available';
    return null;
}

await ensureMcpPatch();

const server = http.createServer(async (request, response) => {
    cors(response);
    if (request.method === 'OPTIONS') {
        response.writeHead(204);
        response.end();
        return;
    }

    const url = new URL(request.url, 'http://localhost');
    const correlationId = String(request.headers['x-correlation-id'] || '');

    if (url.pathname === '/health/live') {
        json(response, 200, { status: 'live' });
        return;
    }

    if (url.pathname === '/health/ready') {
        const problem = checkReady();
        if (problem) {
            json(response, 503, { status: 'not_ready', reason: problem });
            return;
        }
        json(response, 200, { status: 'ready' });
        return;
    }

    if (!authorized(request)) {
        json(response, 401, { error: 'unauthorized' });
        return;
    }

    try {
        if (request.method === 'POST' && url.pathname === '/v1/runs') {
            const payload = await readBody(request);
            const run = await createRun(payload, correlationId);
            json(response, 202, { run_id: run.id, status: run.status });
            return;
        }

        const match = url.pathname.match(/^\/v1\/runs\/([A-Za-z0-9-]{1,64})(\/cancel)?$/);
        if (match) {
            const run = runs.get(match[1]);

            if (!run) {
                const saved = await readFile(path.join(RUNS_DIR, `${match[1]}.json`), 'utf8').catch(() => null);
                if (saved) {
                    json(response, 200, JSON.parse(saved));
                    return;
                }
                json(response, 404, { error: 'run not found' });
                return;
            }

            if (request.method === 'GET' && !match[2]) {
                json(response, 200, publicRun(run));
                return;
            }

            if (request.method === 'POST' && match[2]) {
                if (run.status === 'queued') {
                    run.status = 'cancelled';
                    run.finished_at = new Date().toISOString();
                    const index = queue.indexOf(run.id);
                    if (index >= 0) queue.splice(index, 1);
                    await persist(run);
                } else if (run.child) {
                    run.status = 'cancelled';
                    run.child.kill('SIGTERM');
                    setTimeout(() => run.child?.kill('SIGKILL'), 3000);
                }
                json(response, 200, publicRun(run));
                return;
            }
        }

        json(response, 404, { error: 'not found' });
    } catch (error) {
        log('warn', 'request failed', { error: error.message, correlation_id: correlationId });
        json(response, 400, { error: error.message });
    }
});

await restore();
server.listen(PORT, '0.0.0.0', () => {
    log('info', 'harness runner listening', {
        port: PORT,
        max_concurrent: MAX_CONCURRENT,
        dsh: DSH_BIN,
        ready: checkReady() || 'ok',
    });
});

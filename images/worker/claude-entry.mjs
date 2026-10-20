#!/usr/bin/env node
// Kiln entry point for the Claude Agent SDK (harness id `claude_sdk`).
//
// Protocol (driven by kiln-runner, see crates/harness/src/claude_sdk.rs):
//   stdin line 1   JSON config: { prompt, systemAppend, model, cwd, permissionMode,
//                  disallowedTools, additionalDirectories, maxTurns, mcp: { command, args, envVars } }
//   stdin line 2+  {"type":"feedback","text":"..."} — human feedback, delivered as a new
//                  user turn when the current turn finishes.
//   stdout         every SDK message as one JSON line, plus Kiln diagnostics
//                  {"type":"kiln_log","level","message"} / {"type":"kiln_error","message"}.
//
// Credentials come from the container env (ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN,
// or LITELLM_BASE_URL + LITELLM_API_KEY). Nothing secret is ever printed.

import { query } from '@anthropic-ai/claude-agent-sdk';
import readline from 'node:readline';

const out = (obj) => {
  try {
    process.stdout.write(JSON.stringify(obj) + '\n');
  } catch {
    // Unserializable message (should not happen); skip it.
  }
};
const log = (level, message) => out({ type: 'kiln_log', level, message });
const fail = (message, code = 1) => {
  out({ type: 'kiln_error', message });
  process.exit(code);
};

const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
const lines = rl[Symbol.asyncIterator]();

const first = await lines.next();
if (first.done) fail('no config line on stdin', 2);
let cfg;
try {
  cfg = JSON.parse(first.value);
} catch (e) {
  fail(`invalid config line: ${e.message}`, 2);
}
if (typeof cfg.prompt !== 'string' || !cfg.prompt.trim()) fail('config has no prompt', 2);

// --- environment --------------------------------------------------------------
const env = { ...process.env };
if (!env.ANTHROPIC_API_KEY && !env.CLAUDE_CODE_OAUTH_TOKEN && env.LITELLM_BASE_URL) {
  // Route through the user's LiteLLM gateway (Anthropic-compatible endpoint).
  // The SDK appends `/v1/messages` itself, so drop a trailing `/v1` (LiteLLM
  // URLs are usually given in their OpenAI form, `https://host/v1`).
  env.ANTHROPIC_BASE_URL = env.LITELLM_BASE_URL.replace(/\/+$/, '').replace(/\/v1$/, '');
  if (env.LITELLM_API_KEY) env.ANTHROPIC_AUTH_TOKEN = env.LITELLM_API_KEY;
  log('INFO', 'using LiteLLM gateway for Claude');
}
env.CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC ??= '1';
env.DISABLE_AUTOUPDATER ??= '1';

const mcpEnv = {};
for (const name of cfg.mcp?.envVars ?? []) {
  if (process.env[name] !== undefined) mcpEnv[name] = process.env[name];
}

// --- input stream: initial prompt, then feedback at turn boundaries -------------
const userMessage = (text) => ({
  type: 'user',
  message: { role: 'user', content: text },
  parent_tool_use_id: null,
  session_id: '',
});

const feedback = [];
let turns = 0;
let results = 0;
let finished = false;
let waiter = null;
const wake = () => {
  const w = waiter;
  waiter = null;
  if (w) w();
};

(async () => {
  try {
    for (;;) {
      const { value, done } = await lines.next();
      if (done) break;
      let msg;
      try {
        msg = JSON.parse(value);
      } catch {
        continue;
      }
      if (msg?.type === 'feedback' && typeof msg.text === 'string' && msg.text.trim()) {
        if (finished) {
          log('WARN', 'feedback arrived after the session finished; ignored');
          continue;
        }
        feedback.push(msg.text.trim());
        log('INFO', 'feedback received; it will be delivered when the current turn ends');
        wake();
      }
    }
  } catch {
    // stdin closed or broken: no more feedback.
  }
})();

async function* input() {
  turns = 1;
  yield userMessage(cfg.prompt);
  for (;;) {
    while (results < turns) await new Promise((r) => (waiter = r));
    if (feedback.length === 0) {
      finished = true;
      return;
    }
    const text = feedback.splice(0).join('\n\n');
    turns += 1;
    yield userMessage(`Feedback from the task owner (address it, then finish as instructed):\n\n${text}`);
  }
}

// --- run ----------------------------------------------------------------------
const abort = new AbortController();
for (const sig of ['SIGTERM', 'SIGINT']) {
  process.on(sig, () => {
    log('WARN', `received ${sig}; aborting`);
    abort.abort();
    setTimeout(() => process.exit(143), 3000).unref();
  });
}

const bypass = (cfg.permissionMode ?? 'bypassPermissions') === 'bypassPermissions';
const options = {
  abortController: abort,
  cwd: cfg.cwd || process.cwd(),
  model: cfg.model || undefined,
  permissionMode: cfg.permissionMode ?? 'bypassPermissions',
  allowDangerouslySkipPermissions: bypass,
  systemPrompt: cfg.systemAppend
    ? { type: 'preset', preset: 'claude_code', append: cfg.systemAppend }
    : { type: 'preset', preset: 'claude_code' },
  disallowedTools: cfg.disallowedTools ?? [],
  additionalDirectories: cfg.additionalDirectories ?? [],
  maxTurns: cfg.maxTurns ?? undefined,
  // Load CLAUDE.md written into the worktree from the memory pack.
  settingSources: ['project'],
  mcpServers: cfg.mcp?.command
    ? { kiln: { type: 'stdio', command: cfg.mcp.command, args: cfg.mcp.args ?? ['mcp'], env: mcpEnv } }
    : {},
  env,
  stderr: (data) => process.stderr.write(data),
};

let lastResult = null;
try {
  for await (const msg of query({ prompt: input(), options })) {
    out(msg);
    if (msg?.type === 'result') {
      lastResult = msg;
      results += 1;
      wake();
    }
  }
} catch (e) {
  if (abort.signal.aborted) process.exit(143);
  fail(`claude agent sdk failed: ${e?.message ?? e}`);
}

if (!lastResult) fail('session ended without a result');
process.exit(lastResult.is_error || lastResult.subtype !== 'success' ? 1 : 0);

/**
 * Extract the Electron IPC contract into a machine-readable manifest.
 *
 * The Rust server and the WASM client both generate from `manifest.json`, so the
 * wire format has exactly ONE source of truth. During the port the TypeScript
 * source stays authoritative and this is re-run; after cutover the manifest is
 * hand-maintained and this script is deleted along with src/preload.
 *
 * Why AST and not grep: the return types carry the shape the Rust side has to
 * model (`Promise<{ ok: boolean; error?: string }>` becomes a Result-ish enum),
 * and those span lines and nest. A regex reads the easy 80% and silently drops
 * the ones that matter most.
 */
import { createRequire } from 'node:module';
import { readFileSync, writeFileSync, readdirSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const ts = require(process.env.TS_LIB);

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const preloadPath = join(root, 'src/preload/index.ts');
const src = readFileSync(preloadPath, 'utf8');
const sf = ts.createSourceFile(preloadPath, src, ts.ScriptTarget.Latest, true);

/** Collect every string/template literal passed to ipcRenderer.<method>(...).
 *  Template literals (`pty:data:${id}`) become a pattern with a named hole, which
 *  is how the WS layer learns which streams are per-instance rather than global. */
function channelsIn(node) {
  const found = [];
  (function walk(n) {
    if (ts.isCallExpression(n) && ts.isPropertyAccessExpression(n.expression)) {
      const recv = n.expression.expression;
      if (ts.isIdentifier(recv) && recv.text === 'ipcRenderer') {
        const verb = n.expression.name.text;      // invoke | on | send | removeListener
        const arg = n.arguments[0];
        if (arg && ts.isStringLiteral(arg)) found.push({ verb, channel: arg.text, dynamic: false });
        else if (arg && ts.isIdentifier(arg)) found.push({ verb, channel: arg.text, dynamic: false, alias: true });
        else if (arg && ts.isTemplateExpression(arg)) {
          const pattern = arg.head.text +
            arg.templateSpans.map(s => `{${s.expression.getText(sf)}}` + s.literal.text).join('');
          found.push({ verb, channel: pattern, dynamic: true });
        }
      }
    }
    ts.forEachChild(n, walk);
  })(node);
  return found;
}

/** Resolve a local `const channel = 'x'` / template alias so subscriptions that
 *  bind the channel to a variable first still report their real channel. */
function inlineChannelConsts(node) {
  const consts = new Map();
  (function walk(n) {
    if (ts.isVariableDeclaration(n) && ts.isIdentifier(n.name) && n.initializer) {
      if (ts.isStringLiteral(n.initializer)) consts.set(n.name.text, n.initializer.text);
      else if (ts.isTemplateExpression(n.initializer)) {
        const t = n.initializer;
        consts.set(n.name.text, t.head.text +
          t.templateSpans.map(sp => `{${sp.expression.getText(sf)}}` + sp.literal.text).join(''));
      }
    }
    ts.forEachChild(n, walk);
  })(node);
  return consts;
}

function jsdocOf(node) {
  const docs = ts.getJSDocCommentsAndTags(node);
  if (!docs.length) return undefined;
  const text = docs.map(d => (typeof d.comment === 'string' ? d.comment
    : Array.isArray(d.comment) ? d.comment.map(c => c.text).join('') : '')).join('\n').trim();
  return text || undefined;
}

// Locate the object literal handed to contextBridge.exposeInMainWorld('cth', api).
let apiObject = null;
(function findApi(n) {
  if (ts.isVariableDeclaration(n) && ts.isIdentifier(n.name) && n.name.text === 'api'
      && n.initializer && ts.isObjectLiteralExpression(n.initializer)) apiObject = n.initializer;
  ts.forEachChild(n, findApi);
})(sf);
if (!apiObject) throw new Error('could not locate the `api` object literal in preload');

const methods = [];
for (const prop of apiObject.properties) {
  if (!ts.isPropertyAssignment(prop) && !ts.isMethodDeclaration(prop)) continue;
  const name = prop.name.getText(sf);
  const fn = ts.isPropertyAssignment(prop) ? prop.initializer : prop;
  if (!fn || (!ts.isArrowFunction(fn) && !ts.isFunctionExpression(fn) && !ts.isMethodDeclaration(fn))) continue;

  const consts = inlineChannelConsts(fn);
  const chans = channelsIn(fn).map(c => {
    const channel = consts.get(c.channel) ?? c.channel;
    return { ...c, channel, dynamic: c.dynamic || /\{[^}]+\}/.test(channel) };
  });

  const params = fn.parameters.map(p => ({
    name: p.name.getText(sf),
    type: p.type ? p.type.getText(sf).replace(/\s+/g, ' ') : 'unknown',
    optional: !!p.questionToken || !!p.initializer,
  }));
  const ret = fn.type ? fn.type.getText(sf).replace(/\s+/g, ' ') : 'unknown';

  // invoke => request/response RPC. on => a push stream the client subscribes to.
  const invokes = chans.filter(c => c.verb === 'invoke');
  const listens = chans.filter(c => c.verb === 'on');
  const syncs   = chans.filter(c => c.verb === 'sendSync');
  // `rpc-sync` is called out separately: Electron lets the renderer block on these,
  // the web cannot. Each one needs a redesign (prefetch into a signal, or accept
  // async) — see contract/PORTING-NOTES.md.
  const kind = syncs.length ? 'rpc-sync'
    : invokes.length ? 'rpc' : listens.length ? 'subscription' : 'local';
  const active = kind === 'rpc-sync' ? syncs : kind === 'rpc' ? invokes : listens;

  methods.push({
    name, kind, doc: jsdocOf(prop),
    channels: [...new Set(active.map(c => c.channel))],
    dynamic: active.some(c => c.dynamic),
    params, returns: ret,
    line: sf.getLineAndCharacterOfPosition(prop.getStart(sf)).line + 1,
  });
}

// Cross-check against what main actually registers, so a channel the renderer
// calls but main never handles (or vice versa) surfaces now rather than at runtime.
const mainDir = join(root, 'src/main');
const handled = new Set(), pushed = new Set();
for (const f of readdirSync(mainDir)) {
  if (!/\.(ts|cjs)$/.test(f)) continue;
  const t = readFileSync(join(mainDir, f), 'utf8');
  for (const m of t.matchAll(/ipcMain\.(?:handle|on)\(\s*['"]([^'"]+)['"]/g)) handled.add(m[1]);
  for (const m of t.matchAll(/\.send\(\s*['"]([^'"]+)['"]/g)) pushed.add(m[1]);
  for (const m of t.matchAll(/\.send\(\s*`([^`]+)`/g)) pushed.add(m[1].replace(/\$\{([^}]+)\}/g, '{$1}'));
}

const rpcChannels = new Set(methods.filter(m => m.kind === 'rpc' || m.kind === 'rpc-sync')
  .flatMap(m => m.channels));
const subChannels = new Set(methods.filter(m => m.kind === 'subscription').flatMap(m => m.channels));

const manifest = {
  $comment: 'GENERATED by contract/extract-contract.mjs — do not edit by hand while src/preload exists.',
  source: { preload: 'src/preload/index.ts', main: 'src/main' },
  counts: {
    methods: methods.length,
    rpc: methods.filter(m => m.kind === 'rpc').length,
    subscription: methods.filter(m => m.kind === 'subscription').length,
    local: methods.filter(m => m.kind === 'local').length,
    rpcSync: methods.filter(m => m.kind === 'rpc-sync').length,
    rpcChannels: rpcChannels.size,
    pushChannels: subChannels.size,
  },
  methods,
  // Channels main registers that no preload method calls: dead code, or called
  // from somewhere other than the bridge. Either way the port needs to know.
  unreferencedHandlers: [...handled].filter(c => !rpcChannels.has(c)).sort(),
  // Channels the bridge calls that main never registers: a runtime error waiting.
  missingHandlers: [...rpcChannels].filter(c => !handled.has(c)).sort(),
  mainPushChannels: [...pushed].sort(),
};

writeFileSync(join(root, 'contract/manifest.json'), JSON.stringify(manifest, null, 2) + '\n');
console.log(JSON.stringify(manifest.counts, null, 2));
console.log('unreferenced handlers:', manifest.unreferencedHandlers.length);
console.log('missing handlers    :', manifest.missingHandlers.length);

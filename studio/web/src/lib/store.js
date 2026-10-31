// Copied from Boardroom (sequoia-hope/boardroom, app/store/store.js at 32b8bdd), unchanged: the
// suite keeps one copy of the store and each app pins one. Change it there, then copy it again.
//
// The suite's store: how Boardroom, Circuit Composer and UltraRoute Studio read and write a
// project's files, with the same calls on this machine and in the browser alone
// (docs/demo-spec.md, "One store interface, two backends").
//
//   store.read(path)                               → { bytes, rev } or null
//   store.text(path)                               → string or null
//   store.write(path, data, { app, message, base }) → { rev }, or throws Conflict
//   store.history(path?)                           → [{ rev, path, app, time, message }], newest first
//   store.watch(fn)                                → stop(); fn({ paths, app }) on any app's change
//
// The server backend is Boardroom's server: files under /api/projects/<id>/files/, each read
// carrying its revision as an ETag and each write a PUT that names the revision it started from
// (If-Match), so a write over someone else's is refused (412). Git is the history; the bus says
// what changed.
//
// The browser backend is a workspace in IndexedDB: the published snapshot plus the files changed
// since, with every revision kept. A workspace belongs to one tab (its id in sessionStorage, so a
// reload keeps it); the apps a tab opens are handed the id (?ws=). BroadcastChannel stands in for
// the bus. It adds:
//
//   store.changes()       → [{ path, rev, app, time, message }]: the files changed from the snapshot
//   store.restore(rev)    → writes that revision's content again, as the newest
//   store.reset()         → back to the published project
//   store.download()      → Blob: a zip of the changed files at their project paths
//
// Plain JavaScript, no dependencies: the other apps copy this file at a pinned commit.

/** A write refused because the file moved on since `base`: `current` is its revision now. */
export class Conflict extends Error {
  constructor(path, base, current) {
    super(`${path} was changed by someone else (revision ${current}, not ${base})`);
    this.name = 'Conflict';
    this.path = path;
    this.base = base;
    this.current = current;
  }
}

const encoder = new TextEncoder();
const decoder = new TextDecoder();
const toBytes = (data) => (typeof data === 'string' ? encoder.encode(data) : new Uint8Array(data));
const segments = (path) => path.split('/').map(encodeURIComponent).join('/');

/**
 * The store a page was opened on, from its address: `?store=<root>` names the backend's root
 * (`…/api/projects/<id>/` for the server, the snapshot folder otherwise), `?pid=` the project
 * and `?ws=` the workspace. None given: null, and the app keeps its own demo boards.
 */
export function fromLocation(loc = globalThis.location) {
  const q = new URLSearchParams(loc.search);
  const root = q.get('store');
  if (!root) return null;
  const url = new URL(root, loc.href).href;
  if (/\/api\/projects\/[^/]+\/$/.test(new URL(url).pathname)) return serverStore(url);
  return browserStore({ project: q.get('pid') ?? 'project', snapshot: url, ws: q.get('ws') ?? workspaceId() });
}

/** The query that opens another app on `store`: append it to the app's address. */
export function query(store) {
  const q = new URLSearchParams({ store: store.root });
  if (store.kind === 'browser') {
    q.set('pid', store.project);
    q.set('ws', store.ws);
  }
  return q.toString();
}

/** A random id; `crypto.randomUUID` exists only on secure pages, and a LAN address isn't one. */
function newId() {
  const b = crypto.getRandomValues(new Uint8Array(16));
  return [...b].map((x) => x.toString(16).padStart(2, '0')).join('');
}

/** This tab's workspace id: `?ws=` if the page was given one, else the tab's own, else a new one. */
export function workspaceId() {
  const given = new URLSearchParams(globalThis.location?.search ?? '').get('ws');
  if (given) return given;
  try {
    let ws = sessionStorage.getItem('boardroom.ws');
    if (!ws) {
      ws = newId();
      sessionStorage.setItem('boardroom.ws', ws);
    }
    return ws;
  } catch {
    return newId();
  }
}

// --- On this machine: Boardroom's server ------------------------------------------------------

/** `root`: the project's API address, `…/api/projects/<id>/`. */
export function serverStore(root) {
  root = new URL(root.endsWith('/') ? root : `${root}/`, globalThis.location?.href).href;
  const fileUrl = (path) => `${root}files/${segments(path)}`;
  const revOf = (res) => (res.headers.get('etag') ?? '').replace(/^W\//, '').replace(/"/g, '') || null;
  return {
    kind: 'server',
    root,

    async read(path) {
      const res = await fetch(fileUrl(path), { cache: 'no-cache' });
      if (res.status === 404) return null;
      if (!res.ok) throw new Error(`${path}: ${res.status} ${await res.text()}`);
      return { bytes: new Uint8Array(await res.arrayBuffer()), rev: revOf(res) };
    },

    async text(path) {
      const f = await this.read(path);
      return f && decoder.decode(f.bytes);
    },

    async write(path, data, { app = 'unknown', message = '', base } = {}) {
      const headers = { 'content-type': 'application/octet-stream', 'x-boardroom-app': app, 'x-boardroom-message': encodeURIComponent(message) };
      if (base === null) headers['if-none-match'] = '*';
      else if (base !== undefined) headers['if-match'] = `"${base}"`;
      const res = await fetch(fileUrl(path), { method: 'PUT', headers, body: toBytes(data) });
      if (res.status === 412) throw new Conflict(path, base, revOf(res));
      if (!res.ok) throw new Error(`${path}: ${res.status} ${await res.text()}`);
      return { rev: revOf(res) };
    },

    async history(path) {
      const res = await fetch(`${root}history${path ? `?path=${encodeURIComponent(path)}` : ''}`, { cache: 'no-cache' });
      if (!res.ok) throw new Error(`history: ${res.status} ${await res.text()}`);
      return res.json();
    },

    watch(fn) {
      const id = decodeURIComponent(new URL(root).pathname.split('/').filter(Boolean).pop());
      const url = new URL('../../bus', root);
      url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
      let socket;
      let stopped = false;
      const connect = () => {
        socket = new WebSocket(url);
        socket.onmessage = (e) => {
          let msg;
          try {
            msg = JSON.parse(e.data);
          } catch {
            return;
          }
          if (msg.project !== id) return;
          if (msg.type === 'changed') fn({ paths: msg.paths ?? [], app: msg.app ?? null });
          else if (msg.type === 'status') fn({ paths: null, app: null });
        };
        socket.onclose = () => {
          if (!stopped) setTimeout(connect, 2000);
        };
      };
      connect();
      return () => {
        stopped = true;
        socket?.close();
      };
    },
  };
}

// --- In the browser alone: a workspace in IndexedDB --------------------------------------------

const DB = 'boardroom-store';
const KEEP_DAYS = 7;

function openDb() {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(DB, 1);
    req.onupgradeneeded = () => {
      const db = req.result;
      db.createObjectStore('workspaces', { keyPath: 'ws' });
      db.createObjectStore('files', { keyPath: ['ws', 'project', 'path'] }).createIndex('byProject', ['ws', 'project']);
      db.createObjectStore('history', { autoIncrement: true }).createIndex('byProject', ['ws', 'project']);
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

const done = (tx) =>
  new Promise((resolve, reject) => {
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
    tx.onabort = () => reject(tx.error ?? new Error('aborted'));
  });
const result = (req) =>
  new Promise((resolve, reject) => {
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });

/** Workspaces not opened for a week go, with their files and history. */
async function sweep(db, keep) {
  const cutoff = Date.now() - KEEP_DAYS * 86400e3;
  const tx = db.transaction(['workspaces', 'files', 'history'], 'readwrite');
  const old = (await result(tx.objectStore('workspaces').getAll())).filter((w) => w.seen < cutoff && w.ws !== keep);
  for (const w of old) {
    tx.objectStore('workspaces').delete(w.ws);
    for (const name of ['files', 'history']) {
      const range = IDBKeyRange.bound([w.ws], [w.ws, []]);
      const cursor = tx.objectStore(name).index('byProject').openKeyCursor(range);
      cursor.onsuccess = () => {
        const c = cursor.result;
        if (c) {
          tx.objectStore(name).delete(c.primaryKey);
          c.continue();
        }
      };
    }
  }
  await done(tx);
}

/**
 * `project`: its id; `snapshot`: the folder its published files are read from; `ws`: the
 * workspace (this tab's by default).
 */
export function browserStore({ project, snapshot, ws = workspaceId() }) {
  snapshot = new URL(snapshot.endsWith('/') ? snapshot : `${snapshot}/`, globalThis.location?.href).href;
  const channel = typeof BroadcastChannel === 'function' ? new BroadcastChannel('boardroom-store') : null;
  const listeners = new Set();
  const announce = (paths, app) => {
    for (const fn of listeners) fn({ paths, app });
    channel?.postMessage({ ws, project, paths, app });
  };
  let dbp = null;
  const db = () =>
    (dbp ??= openDb().then(async (d) => {
      const tx = d.transaction('workspaces', 'readwrite');
      const ws_ = tx.objectStore('workspaces');
      const w = (await result(ws_.get(ws))) ?? { ws, created: Date.now(), next: 1 };
      ws_.put({ ...w, seen: Date.now() });
      await done(tx);
      sweep(d, ws).catch(() => {});
      return d;
    }));
  let persisted = null;
  // Whether the published project has a file: asked once per path, since a snapshot doesn't change.
  const published = new Map();
  const inSnapshot = (path) => {
    if (!published.has(path)) {
      published.set(path, fetch(snapshot + segments(path), { method: 'HEAD', cache: 'no-cache' }).then((r) => r.ok, () => false));
    }
    return published.get(path);
  };

  async function entries() {
    const d = await db();
    const tx = d.transaction('history', 'readonly');
    const index = tx.objectStore('history').index('byProject');
    const [keys, values] = await Promise.all([result(index.getAllKeys([ws, project])), result(index.getAll([ws, project]))]);
    return values.map((v, i) => ({ ...v, key: keys[i] }));
  }

  return {
    kind: 'browser',
    root: snapshot,
    project,
    ws,
    /** Whether the browser agreed to keep the workspace when space runs low (asked on the first write). */
    get persisted() {
      return persisted;
    },

    async read(path) {
      const d = await db();
      const own = await result(d.transaction('files', 'readonly').objectStore('files').get([ws, project, path]));
      if (own) return own.deleted ? null : { bytes: new Uint8Array(own.bytes), rev: own.rev };
      const res = await fetch(snapshot + segments(path), { cache: 'no-cache' });
      if (res.status === 404) return null;
      if (!res.ok) throw new Error(`${path}: ${res.status}`);
      return { bytes: new Uint8Array(await res.arrayBuffer()), rev: 'published' };
    },

    async text(path) {
      const f = await this.read(path);
      return f && decoder.decode(f.bytes);
    },

    async write(path, data, { app = 'unknown', message = '', base } = {}) {
      const bytes = toBytes(data);
      // Asked before the transaction, which would close while a fetch is waited on.
      const there = base === undefined ? true : await inSnapshot(path);
      const d = await db();
      // One transaction: two windows writing the same file are serialized by the database, and
      // the second sees the first's revision.
      const tx = d.transaction(['workspaces', 'files', 'history'], 'readwrite');
      const files = tx.objectStore('files');
      // The tab's own revision; else the published file's, or none: a new file, or one deleted here.
      const own = await result(files.get([ws, project, path]));
      const current = own ? (own.deleted ? null : own.rev) : there ? 'published' : null;
      if (base !== undefined && base !== current) {
        tx.abort();
        await done(tx).catch(() => {});
        throw new Conflict(path, base, current);
      }
      const wsStore = tx.objectStore('workspaces');
      const w = await result(wsStore.get(ws));
      const rev = `r${w.next}`;
      wsStore.put({ ...w, next: w.next + 1, seen: Date.now() });
      const time = new Date().toISOString();
      const record = { ws, project, path, rev, app, time, message, bytes: bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) };
      files.put(record);
      tx.objectStore('history').add(record);
      await done(tx);
      if (persisted === null && navigator.storage?.persist) persisted = await navigator.storage.persist().catch(() => false);
      announce([path], app);
      return { rev };
    },

    async history(path) {
      return (await entries())
        .filter((e) => !path || e.path === path)
        .sort((a, b) => b.key - a.key)
        .map(({ rev, path, app, time, message }) => ({ rev, path, app, time, message }));
    },

    async changes() {
      const d = await db();
      const all = await result(d.transaction('files', 'readonly').objectStore('files').index('byProject').getAll([ws, project]));
      return all.map(({ path, rev, app, time, message }) => ({ path, rev, app, time, message })).sort((a, b) => a.path.localeCompare(b.path));
    },

    async restore(rev, { app = 'boardroom' } = {}) {
      const e = (await entries()).find((x) => x.rev === rev);
      if (!e) throw new Error(`no revision ${rev}`);
      return this.write(e.path, e.bytes, { app, message: `Back to ${rev} (${e.app}${e.message ? `: ${e.message}` : ''})` });
    },

    async reset() {
      const d = await db();
      const tx = d.transaction(['files', 'history'], 'readwrite');
      const paths = new Set();
      for (const name of ['files', 'history']) {
        const store = tx.objectStore(name);
        const keys = await result(store.index('byProject').getAllKeys([ws, project]));
        if (name === 'files') for (const k of keys) paths.add(k[2]);
        for (const k of keys) store.delete(k);
      }
      await done(tx);
      if (paths.size) announce([...paths], 'boardroom');
    },

    async download() {
      const d = await db();
      const all = await result(d.transaction('files', 'readonly').objectStore('files').index('byProject').getAll([ws, project]));
      return zip(all.filter((f) => !f.deleted).map((f) => ({ name: f.path, bytes: new Uint8Array(f.bytes), time: new Date(f.time) })));
    },

    watch(fn) {
      listeners.add(fn);
      const onmsg = (e) => {
        const m = e.data;
        if (m?.ws === ws && m.project === project) fn({ paths: m.paths, app: m.app });
      };
      channel?.addEventListener('message', onmsg);
      return () => {
        listeners.delete(fn);
        channel?.removeEventListener('message', onmsg);
      };
    },
  };
}

// --- A zip, stored (no compression): Download changes -----------------------------------------

const CRC = (() => {
  const t = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c >>> 0;
  }
  return t;
})();

function crc32(bytes) {
  let c = 0xffffffff;
  for (const b of bytes) c = CRC[(c ^ b) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

/** `files`: [{ name, bytes, time }] → a zip Blob, every entry stored, names in UTF-8. */
export function zip(files) {
  const parts = [];
  const central = [];
  let offset = 0;
  for (const f of files) {
    const name = encoder.encode(f.name);
    const crc = crc32(f.bytes);
    const t = f.time ?? new Date();
    const dosTime = (t.getHours() << 11) | (t.getMinutes() << 5) | (t.getSeconds() >> 1);
    const dosDate = ((t.getFullYear() - 1980) << 9) | ((t.getMonth() + 1) << 5) | t.getDate();
    const local = new DataView(new ArrayBuffer(30));
    local.setUint32(0, 0x04034b50, true);
    local.setUint16(4, 20, true);
    local.setUint16(6, 0x0800, true);
    local.setUint16(8, 0, true);
    local.setUint16(10, dosTime, true);
    local.setUint16(12, dosDate, true);
    local.setUint32(14, crc, true);
    local.setUint32(18, f.bytes.length, true);
    local.setUint32(22, f.bytes.length, true);
    local.setUint16(26, name.length, true);
    local.setUint16(28, 0, true);
    parts.push(local, name, f.bytes);
    const c = new DataView(new ArrayBuffer(46));
    c.setUint32(0, 0x02014b50, true);
    c.setUint16(4, 20, true);
    c.setUint16(6, 20, true);
    c.setUint16(8, 0x0800, true);
    c.setUint16(10, 0, true);
    c.setUint16(12, dosTime, true);
    c.setUint16(14, dosDate, true);
    c.setUint32(16, crc, true);
    c.setUint32(20, f.bytes.length, true);
    c.setUint32(24, f.bytes.length, true);
    c.setUint16(28, name.length, true);
    c.setUint32(42, offset, true);
    central.push(c, name);
    offset += 30 + name.length + f.bytes.length;
  }
  const size = central.reduce((n, p) => n + p.byteLength, 0);
  const end = new DataView(new ArrayBuffer(22));
  end.setUint32(0, 0x06054b50, true);
  end.setUint16(8, files.length, true);
  end.setUint16(10, files.length, true);
  end.setUint32(12, size, true);
  end.setUint32(16, offset, true);
  return new Blob([...parts, ...central, end], { type: 'application/zip' });
}

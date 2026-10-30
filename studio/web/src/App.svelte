<script>
  import { onMount } from 'svelte';
  import { Studio } from './core/studio_core.js';
  import Board from './lib/Board.svelte';
  import { DEFAULT_RULES_NOTE, KICAD_DEFAULT_PRO, readBoardFiles } from './lib/open-board.js';

  const studio = new Studio();
  studio.set_show_pours(true);
  // Built for a static host: no router to talk to, only recorded runs.
  const STATIC = import.meta.env.VITE_STATIC === '1';
  // Built to route in the tab itself: the core router runs here, in a worker, with no server.
  const LOCAL = import.meta.env.VITE_LOCAL === '1';
  // Parts are not moved by hand for now: on a phone, zooming kept catching a chip and moving it,
  // and the router nudges parts itself. Lock, select and route stay.
  const MOVE_BY_HAND = false;
  let version = $state(0);
  let board;

  let jobs = $state([]);
  let jobName = $state('');
  // A Boardroom project opened by path (?project=…&board=…&variant=…), and the board of it on
  // show: the core routes it on the server, and the result can be saved back over its file.
  let project = $state(null);
  let projectBoard = $state(null);
  let saveable = $state(false);
  let saveNote = $state('');
  // A board opened from the user's computer, read in the browser: it routes in this tab, in
  // every build, and the result is downloaded.
  let fileBoard = $state(null);
  let fileNote = $state('');
  let dragging = $state(false);
  let fileInput;
  let argsText = $state('');
  let envText = $state('');
  // A tab routes on one thread, several times slower than the server on a phone.
  let timeout = $state(LOCAL ? 600 : 120);
  let fps = $state(60);
  let keepGoing = $state(false);
  let rerouteOnDrop = $state(true);
  let retireDesign = $state(false);
  let placements = $state({});
  let locks = $state([]);
  let sentLocks = [];

  let connected = $state(false);
  let mode = $state('idle');
  let status = $state(null);
  let parts = $state([]);
  let selected = $state(null);
  let step = $state(0.25);
  let showCongestion = $state(false);
  let congestion = $state(null);
  let logs = $state([]);
  let message = $state('');
  let hover = $state({ part: null, net: '' });
  let frameRate = $state(0);
  let socket;
  let frameTimes = [];
  let refitNext = true;
  let rerouteTimer;
  // The router in this tab, when it routes here, and the last board it routed.
  let worker = null;
  let inTab = $state(LOCAL);
  let routedText = $state('');

  let recordings = $state([]);
  let recording = $state(null);
  let replaying = $state(false);
  let playing = $state(false);
  let speed = $state(1);
  let position = $state(0);
  let duration = $state(0);
  let lines = [];
  let times = [];
  let next = 0;
  let wallStart = 0;
  let positionStart = 0;

  const STEPS = [0.1, 0.25, 0.5, 1, 2];
  const job = $derived(
    fileBoard
      ? { name: fileBoard.name, kicad_pcb: fileBoard.pcbName, kicad_project: fileBoard.proName, presets: [] }
      : projectBoard
        ? { name: `${projectBoard.board}/${projectBoard.variant}`, kicad_pcb: projectBoard.kicad_pcb, kicad_project: projectBoard.kicad_project, presets: [] }
        : jobs.find((j) => j.name === jobName),
  );
  // A file board needs no server: the tab routes it.
  const canRoute = $derived(connected || !!fileBoard);
  const boardKey = (b) => `project:${b.board}/${b.variant}`;
  const presets = $derived(job?.presets ?? []);
  const running = $derived(mode !== 'idle');
  const designFixed = $derived(parts.filter((p) => p.fixed && !sentLocks.includes(p.name) && !locks.includes(p.name)));

  // A recipe's --lock list becomes the editable lock set; the rest stays as arguments.
  function splitLocks(args) {
    const rest = [];
    const found = [];
    for (let i = 0; i < args.length; i++) {
      if (args[i] === '--lock' && i + 1 < args.length) found.push(...args[++i].split(',').filter(Boolean));
      else if (args[i].startsWith('--lock=')) found.push(...args[i].slice(7).split(',').filter(Boolean));
      else rest.push(args[i]);
    }
    return { rest, found };
  }

  // A scored recipe, read into the router options.
  function choosePreset(name) {
    const preset = presets.find((p) => p.name === name);
    if (!preset) return;
    const { rest, found } = splitLocks(preset.args ?? []);
    const backend = rest.indexOf('--backend');
    router = backend >= 0 ? rest[backend + 1] : 'maze';
    fanout = !rest.includes('router.fanout.enabled=false');
    negotiate = preset.env?.COPPERROUTE_GRID_NEGOTIATE === '1';
    fineGrid = preset.env?.COPPERROUTE_GRID_CELL_MM === '0.05';
    if (found.length) locks = found;
    compose();
  }

  const ROUTERS = [
    { key: 'grid', label: 'Grid', title: 'GPU grid router with negotiated congestion; the maze router finishes what it leaves' },
    { key: 'maze', label: 'Maze', title: 'The maze router alone' },
    { key: 'topo', label: 'Topo', title: 'Topological rubber-band routing with rip-up, then the maze router' },
    { key: 'nudge', label: 'Nudge', title: 'Grid routing, then moves unlocked parts that block open connections (slow)' },
    { key: 'core', label: 'Core', title: 'The MIT core router, on the KiCad board itself: grid routing with negotiated congestion' },
  ];
  let ripUp = $state(true);
  let prefanout = $state(true);
  let diffPairs = $state(true);
  let moveParts = $state(true);
  let viewMode = $state('all');
  let flipped = $state(false);
  let highContrast = $state(false);
  let showPours = $state(true);
  let showHelp = $state(false);
  // On a phone the board fills the screen and the panel is a sheet over its lower part.
  let sheetOpen = $state(typeof location !== 'undefined' && location.hash === '#controls');
  let fullscreen = $state(false);
  const canFullscreen = typeof document !== 'undefined' && !!document.documentElement.requestFullscreen;

  function toggleFullscreen() {
    if (document.fullscreenElement) document.exitFullscreen();
    else document.documentElement.requestFullscreen({ navigationUI: 'hide' }).catch(() => {});
  }
  // Collapsed on a phone, where the board needs the room.
  let consoleOpen = $state(typeof matchMedia === 'undefined' || !matchMedia('(max-width: 720px)').matches);
  let consoleBox;
  $effect(() => {
    consoleLines;
    if (consoleBox) consoleBox.scrollTop = consoleBox.scrollHeight;
  });
  let consoleLines = $state([]);
  let eventsSeen = 0;
  const VIEWS = [
    { key: 'all', label: 'All layers' },
    { key: 'top', label: 'Top' },
    { key: 'bottom', label: 'Bottom' },
    { key: 'split', label: 'Side by side' },
  ];
  // The copper between top and bottom, each shown alone from the view bar's Inner menu.
  const innerLayers = $derived((status?.layers ?? []).slice(1, -1).map((l, k) => ({ key: `layer:${k + 1}`, label: l.name.replace(/\.Cu$/, '') })));
  let tighten = $state(true);
  const engine = $derived(router === 'core' ? 'core' : 'copperroute');
  // Copperroute reads the job's Specctra board, the core its KiCad board.
  const available = (key, j = job) => (key === 'core' ? !!j?.kicad_pcb : !!j?.dsn);
  let router = $state('grid');
  let negotiate = $state(true);
  let fineGrid = $state(true);
  let fanout = $state(false);

  // The router options as arguments and environment; the advanced fields show and can edit them.
  function compose() {
    const args = [];
    const env = {};
    if (router === 'core') {
      // Every stage on is the router's own default recipe (the benchmark's), named rather than
      // spelt out; moving parts is the one addition. Leaving a stage out lists the rest.
      if (negotiate && ripUp && diffPairs && prefanout && tighten) {
        args.push('--recipe', 'default');
        if (moveParts) args.push('--nudge');
        argsText = args.join(' ');
        envText = '';
        return;
      }
      if (negotiate) args.push('--negotiate');
      if (ripUp) args.push('--rip-up');
      if (diffPairs) args.push('--diff-pairs');
      if (prefanout) args.push('--prefanout');
      if (moveParts) args.push('--nudge');
      if (tighten) args.push('--tighten');
      argsText = args.join(' ');
      envText = '';
      return;
    }
    // Copperroute moves parts as a router of its own: grid routing, then nudging.
    const backend = moveParts ? 'nudge' : router;
    if (backend !== 'maze') args.push('--backend', backend);
    if (!fanout) args.push('--set', 'router.fanout.enabled=false');
    const grid = backend === 'grid' || backend === 'nudge';
    if (grid && negotiate) env.COPPERROUTE_GRID_NEGOTIATE = '1';
    if (grid && fineGrid) env.COPPERROUTE_GRID_CELL_MM = '0.05';
    if (backend === 'nudge') env.COPPERROUTE_NUDGE_BASE = 'grid';
    argsText = args.join(' ');
    envText = Object.entries(env)
      .map(([k, v]) => `${k}=${v}`)
      .join('\n');
  }

  // Leaves a board opened from a file, and its result, for a job or a project's board.
  function leaveBoard() {
    if (fileBoard) timeout = LOCAL || inTab ? 600 : 120;
    fileBoard = null;
    fileNote = '';
    routedText = '';
  }

  function chooseJob(name) {
    leaveBoard();
    jobName = name;
    projectBoard = null;
    saveable = false;
    saveNote = '';
    placements = {};
    selected = null;
    refitNext = true;
    const chosen = jobs.find((j) => j.name === name);
    const withLocks = chosen?.presets?.find((p) => splitLocks(p.args ?? []).found.length);
    locks = withLocks ? splitLocks(withLocks.args).found : [];
    if (!available(router, chosen)) router = available('grid', chosen) ? 'grid' : 'core';
    compose();
  }

  // A project's board: the core router on the server, with no lock or placement from before.
  function chooseProjectBoard(b) {
    leaveBoard();
    projectBoard = b;
    jobName = '';
    placements = {};
    selected = null;
    refitNext = true;
    locks = [];
    router = 'core';
    inTab = false;
    saveable = false;
    saveNote = '';
    compose();
    const url = new URL(location.href);
    url.searchParams.delete('job');
    url.searchParams.set('board', b.board);
    url.searchParams.set('variant', b.variant);
    history.replaceState(null, '', url);
  }

  function chooseBoard(value) {
    const b = project?.boards.find((x) => boardKey(x) === value);
    if (b) chooseProjectBoard(b);
    else chooseJob(value);
    load();
  }

  async function openProject(path, asked) {
    const query = new URLSearchParams({ project: path });
    for (const key of ['board', 'variant']) if (asked.get(key)) query.set(key, asked.get(key));
    const answer = await fetch(`../api/project?${query}`)
      .then((r) => r.json())
      .catch((e) => ({ error: e.message }));
    if (answer.error) {
      message = `couldn't open the project: ${answer.error}`;
      return;
    }
    project = answer;
    if (answer.chosen) chooseProjectBoard(answer.chosen);
    else {
      // Several boards and none named: nothing loads until one is chosen.
      jobName = '';
      message = `${answer.choose}: pick it under Board.`;
    }
  }

  // A board from the user's computer: the core routes it in this tab, with no lock or placement
  // from before. The address can't name a local file, so a reload returns to the default board.
  async function openFiles(fileList) {
    if (!fileList?.length) return;
    let opened;
    try {
      opened = await readBoardFiles(fileList);
    } catch (e) {
      message = `couldn't open the board: ${e.message}`;
      return;
    }
    if (running) stop();
    leaveReplay();
    fileBoard = opened;
    projectBoard = null;
    jobName = '';
    placements = {};
    selected = null;
    refitNext = true;
    locks = [];
    router = 'core';
    timeout = 600;
    routedText = '';
    saveable = false;
    saveNote = '';
    const notes = [...opened.notes];
    if (!opened.pro) notes.push(`no .kicad_pro was given, so it routes with ${DEFAULT_RULES_NOTE}`);
    if (opened.pro && !opened.dru) notes.push('no .kicad_dru was given, so no custom rules apply');
    fileNote = notes.length ? `${notes.join('; ')}.`.replace(/^./, (c) => c.toUpperCase()) : '';
    compose();
    load();
  }

  function dropFiles(event) {
    event.preventDefault();
    dragging = false;
    if (!STATIC) openFiles(event.dataTransfer?.files);
  }

  function dragOver(event) {
    if (STATIC || ![...(event.dataTransfer?.types ?? [])].includes('Files')) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = 'copy';
    dragging = true;
  }

  function saveToProject() {
    if (!socket || socket.readyState !== WebSocket.OPEN) return;
    socket.send(JSON.stringify({ cmd: 'save' }));
    saveable = false;
    saveNote = 'Saving…';
  }

  function environment() {
    const env = {};
    for (const line of envText.split('\n')) {
      const at = line.indexOf('=');
      if (at > 0) env[line.slice(0, at).trim()] = line.slice(at + 1).trim();
    }
    return env;
  }

  function withBackend(args, backend) {
    const out = [];
    for (let i = 0; i < args.length; i++) {
      if (args[i] === '--backend') i++;
      else if (!args[i].startsWith('--backend=')) out.push(args[i]);
    }
    return [...out, '--backend', backend];
  }

  function send(cmd, args, lockList, options = {}) {
    if (fileBoard || (inTab && engine === 'core' && !projectBoard)) return sendToTab(cmd, args, lockList, options);
    if (!socket || socket.readyState !== WebSocket.OPEN || !(jobName || projectBoard)) return;
    if (lockList.length) args = [...args, '--lock', lockList.join(',')];
    if (retireDesign) args = [...args, '--retire-fixed', 'all'];
    sentLocks = [...lockList];
    socket.send(
      JSON.stringify({
        cmd,
        ...(projectBoard ? { project: project.project, board: projectBoard.board, variant: projectBoard.variant } : { job: jobName }),
        args,
        env: cmd === 'load' ? {} : environment(),
        timeout: cmd === 'load' ? undefined : Number(timeout) || undefined,
        fps: Number(fps) || undefined,
        engine,
        place: placements,
        continue: options.continue ?? false,
      }),
    );
    mode = cmd === 'load' ? 'loading' : 'routing';
    message = '';
    logs = [];
    saveable = false;
    saveNote = '';
  }

  // A run in this tab, built as the server builds one for the core: the job's board, stripped
  // as the job says unless continuing from the last result, then placements, locks and options.
  async function sendToTab(cmd, args, lockList, options = {}) {
    if (!job?.kicad_pcb) return;
    worker?.terminate();
    const load = cmd === 'load';
    const input = `../jobs/${encodeURIComponent(jobName)}/input/`;
    const fetchText = (name) => fetch(input + encodeURIComponent(name)).then((r) => (r.ok ? r.text() : Promise.reject(new Error(`${name}: ${r.status}`))));
    const continuing = !load && options.continue && routedText;
    let pcb, pro, dru;
    // The custom rules beside the project are optional; a board without them has none. A board
    // without a project has KiCad's default rules, as a new KiCad project would.
    const druName = job.kicad_project?.replace(/\.kicad_pro$/, '.kicad_dru');
    if (fileBoard) {
      [pcb, pro, dru] = [continuing ? routedText : fileBoard.pcb, fileBoard.pro ?? KICAD_DEFAULT_PRO, fileBoard.dru];
    } else {
      try {
        [pcb, pro, dru] = await Promise.all([
          continuing ? routedText : fetchText(job.kicad_pcb),
          job.kicad_project ? fetchText(job.kicad_project) : KICAD_DEFAULT_PRO,
          druName ? fetchText(druName).catch(() => '') : '',
        ]);
      } catch (e) {
        message = `couldn't fetch the board: ${e.message}`;
        return;
      }
    }
    const all = [];
    if (!continuing) all.push(...(job.strip === 'all' ? ['--strip-all'] : job.strip === true ? ['--strip'] : []));
    all.push(...(typeof job.args === 'string' ? job.args.split(/\s+/).filter(Boolean) : []));
    if (load) all.push('--no-route');
    else if (Number(timeout)) all.push('--timeout', String(Number(timeout)));
    for (const [name, at] of Object.entries(placements)) all.push(`--place=${name}=${at[0].toFixed(4)},${at[1].toFixed(4)}`);
    if (lockList.length) all.push('--lock', lockList.join(','));
    all.push(...args);
    sentLocks = [...lockList];
    studio.clear_offsets();
    studio.clear_lock_overrides();
    mode = load ? 'loading' : 'routing';
    message = '';
    logs = [];
    worker = new Worker(new URL('./lib/router-worker.js', import.meta.url), { type: 'module' });
    worker.onmessage = ({ data }) => {
      if (data.line !== undefined) receive(data.line);
      else {
        if (data.error) message = data.error;
        else if (!load) routedText = data.done;
        mode = 'idle';
        worker?.terminate();
        worker = null;
      }
    };
    worker.onerror = (e) => {
      message = `the router stopped: ${e.message ?? e}`;
      mode = 'idle';
    };
    worker.postMessage({ pcb, pro, dru, options: all.join(' '), fps: Number(fps) || 30 });
  }

  function downloadRouted() {
    const url = URL.createObjectURL(new Blob([routedText], { type: 'text/plain' }));
    const a = Object.assign(document.createElement('a'), { href: url, download: `${(job?.name ?? 'board').replace(/\//g, '-')}-routed.kicad_pcb` });
    a.click();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  }

  async function openRecording(rec) {
    clearTimeout(rerouteTimer);
    if (mode !== 'idle') stop();
    recording = rec;
    replaying = true;
    playing = false;
    refitNext = true;
    const buffer = new Uint8Array(await (await fetch(`replays/${rec.file}`)).arrayBuffer());
    const gzipped = buffer[0] === 0x1f && buffer[1] === 0x8b;
    const text = gzipped
      ? await new Response(new Blob([buffer]).stream().pipeThrough(new DecompressionStream('gzip'))).text()
      : new TextDecoder().decode(buffer);
    lines = text.split('\n').filter(Boolean);
    times = lines.map((l) => Number(l.match(/"ms":(\d+)/)?.[1] ?? -1));
    duration = Math.max(0, ...times);
    seek(0);
    play();
  }

  // Replays from the start: frames carry only what changed, so a jump back rebuilds the board.
  function seek(to) {
    if (to < position || next === 0) {
      next = 0;
      studio.clear_offsets();
    }
    while (next < lines.length && times[next] <= to) receive(lines[next++], true);
    position = to;
    positionStart = to;
    wallStart = performance.now();
  }

  function play() {
    if (position >= duration) seek(0);
    playing = true;
    positionStart = position;
    wallStart = performance.now();
    requestAnimationFrame(tick);
  }

  function tick() {
    if (!playing || !replaying) return;
    const to = positionStart + (performance.now() - wallStart) * speed;
    while (next < lines.length && times[next] <= to) receive(lines[next++], true);
    position = Math.min(to, duration);
    if (next >= lines.length) {
      playing = false;
      return;
    }
    requestAnimationFrame(tick);
  }

  function pause() {
    playing = false;
  }

  function leaveReplay() {
    replaying = false;
    playing = false;
    recording = null;
  }

  function load() {
    leaveReplay();
    clearTimeout(rerouteTimer);
    send('load', engine === 'core' ? [] : ['--backend', 'none'], engine === 'core' ? [] : locks);
  }

  function route(options = {}) {
    clearTimeout(rerouteTimer);
    leaveReplay();
    send('run', argsText.split(/\s+/).filter(Boolean), locks, options);
  }

  // Copperroute nudges as a backend of its own; the core as a pass after routing.
  function nudgeArgs() {
    const args = argsText.split(/\s+/).filter(Boolean);
    return engine === 'core' ? [...args.filter((a) => a !== '--nudge'), '--nudge'] : withBackend(args, 'nudge');
  }

  // Lets the nudger move only this part: every other part is locked for the run.
  function nudgeThis() {
    if (!selected) return;
    const others = parts.map((p) => p.name).filter((n) => n !== selected.name);
    leaveReplay();
    send('run', nudgeArgs(), others, { continue: keepGoing });
  }

  function stop() {
    if (worker) {
      worker.terminate();
      worker = null;
      mode = 'idle';
      message = 'Stopped.';
      return;
    }
    socket?.send(JSON.stringify({ cmd: 'stop' }));
  }

  function afterPlacing() {
    clearTimeout(rerouteTimer);
    if (rerouteOnDrop) rerouteTimer = setTimeout(() => route({ continue: keepGoing }), 700);
  }

  function dropped(part, by) {
    placements = { ...placements, [part.name]: [round(part.at[0] + by[0]), round(part.at[1] + by[1])] };
    select(part);
    afterPlacing();
  }

  function moveSelected(dx, dy) {
    if (!selected) return;
    const part = JSON.parse(studio.part(selected.id) || 'null');
    if (!part) return;
    const from = placements[part.name] ?? part.at;
    const to = [round(from[0] + dx), round(from[1] + dy)];
    placements = { ...placements, [part.name]: to };
    studio.set_offset(part.id, to[0] - part.at[0], to[1] - part.at[1]);
    version++;
    afterPlacing();
  }

  function unplace(name) {
    const { [name]: _, ...rest } = placements;
    placements = rest;
    load();
  }

  function select(part) {
    selected = part ? { id: part.id, name: part.name } : null;
    if (part) sheetOpen = true;
    studio.set_selected(part ? part.id : 0);
    version++;
  }

  function selectByName(name) {
    const part = parts.find((p) => p.name === name);
    if (part) select(part);
  }

  function toggleLock(name) {
    const part = parts.find((p) => p.name === name);
    const locked = locks.includes(name);
    locks = locked ? locks.filter((n) => n !== name) : [...locks, name];
    if (part) studio.set_locked(part.id, !locked || part.fixed && !sentLocks.includes(name));
    version++;
  }

  function highlight(ids) {
    studio.set_highlight(ids.filter((id) => id != null));
    version++;
  }

  function round(v) {
    return Math.round(v * 1000) / 1000;
  }

  function receive(line, fromRecording = false) {
    if (replaying && !fromRecording && /"t":"(board|frame|done)"\}$/.test(line)) return;
    if (studio.apply(line)) {
      version++;
      if (line.includes('"t":"frame"')) frameTimes.push(performance.now());
      else if (line.includes('"t":"board"')) {
        if (refitNext) board?.refit();
        refitNext = false;
        if (selected) studio.set_selected(selected.id);
      }
      return;
    }
    let event;
    try {
      event = JSON.parse(line);
    } catch {
      return;
    }
    if (event.t === 'run') {
      studio.clear_offsets();
      studio.clear_lock_overrides();
    } else if (event.t === 'eof') {
      mode = 'idle';
    } else if (event.t === 'log') {
      if (/ (INFO|WARN|ERROR) /.test(event.line)) logs = [...logs.slice(-200), event.line];
      if (/ WARN --place/.test(event.line)) message = event.line.replace(/^.*WARN /, '');
    } else if (event.t === 'routed') {
      saveable = event.saveable;
      saveNote = event.status_error ? `design/status.json was not updated: ${event.status_error}` : '';
    } else if (event.t === 'saved') {
      if (event.ok) saveNote = `Saved over ${event.path}.${event.status_error ? ` design/status.json was not updated: ${event.status_error}` : ''}`;
      else {
        saveNote = '';
        message = event.message;
      }
    } else if (event.t === 'error') {
      message = event.message;
      mode = 'idle';
    }
  }

  function connect() {
    const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
    socket = new WebSocket(`${scheme}://${location.host}/ws`);
    socket.onopen = () => {
      connected = true;
      load();
    };
    socket.onclose = () => {
      connected = false;
      mode = 'idle';
      setTimeout(connect, 1500);
    };
    // A board opened from a file is the tab's alone: nothing the server sends is about it.
    socket.onmessage = (event) => fileBoard || receive(event.data);
  }

  function toggleLayer(index, hidden) {
    studio.set_layer_hidden(index, hidden);
    version++;
  }

  function toggleCongestion() {
    studio.set_congestion(showCongestion);
    version++;
  }

  function setActive(layer) {
    const count = status?.layers?.length ?? 1;
    studio.set_active_layer(Math.max(0, Math.min(count - 1, layer)));
    status = JSON.parse(studio.status());
    version++;
  }

  // KiCad's layer and display keys: PgUp and PgDn pick F.Cu and B.Cu, + and - step through the
  // copper, Ctrl+H toggles high contrast, Ctrl+Shift+F flips the board.
  function viewKey(event) {
    if (/INPUT|TEXTAREA|SELECT/.test(event.target.tagName)) return false;
    const count = status?.layers?.length ?? 1;
    const active = status?.layers?.findIndex((l) => l.active) ?? 0;
    if (event.key === 'PageUp') setActive(0);
    else if (event.key === 'PageDown') setActive(count - 1);
    else if (event.key === '+' || event.key === '=') setActive(active + 1);
    else if (event.key === '-') setActive(active - 1);
    else if (event.ctrlKey && event.key.toLowerCase() === 'h') {
      highContrast = !highContrast;
      studio.set_high_contrast(highContrast);
      version++;
    } else if (event.ctrlKey && event.shiftKey && event.key.toLowerCase() === 'f') flipped = !flipped;
    else if (event.key === '?') showHelp = !showHelp;
    else return false;
    event.preventDefault();
    return true;
  }

  function key(event) {
    if (viewKey(event)) return;
    if (!selected || /INPUT|TEXTAREA|SELECT/.test(event.target.tagName)) return;
    if (STATIC && event.key !== 'Escape') return;
    const k = event.shiftKey ? 5 : 1;
    const moves = { ArrowLeft: [-1, 0], ArrowRight: [1, 0], ArrowUp: [0, 1], ArrowDown: [0, -1] };
    if (moves[event.key] && MOVE_BY_HAND) {
      event.preventDefault();
      moveSelected(moves[event.key][0] * step * k, moves[event.key][1] * step * k);
    } else if (event.key === 'l') {
      toggleLock(selected.name);
    } else if (event.key === 'Escape') {
      select(null);
    }
  }

  onMount(async () => {
    const all = await (await fetch('../api/jobs')).json();
    // The studio offers the boards marked for it, when any are; routing here, only KiCad boards.
    jobs = all.some((j) => j.studio) ? all.filter((j) => j.studio) : all;
    if (LOCAL) jobs = jobs.filter((j) => j.kicad_pcb);
    jobs.sort((a, b) => (a.order ?? 1e9) - (b.order ?? 1e9));
    recordings = await fetch('replays/index.json')
      .then((r) => (r.ok ? r.json() : []))
      .catch(() => []);
    const asked = new URLSearchParams(location.search);
    const first = jobs.find((j) => j.name === asked.get('job')) ?? jobs[0];
    if (first) chooseJob(first.name);
    if (asked.get('project') && !STATIC && !LOCAL) await openProject(asked.get('project'), asked);
    if (LOCAL) {
      router = 'core';
      compose();
    }
    if (presets.some((p) => p.name === asked.get('recipe'))) {
      choosePreset(asked.get('recipe'));
    }
    const linked = recordings.find((r) => r.file === asked.get('replay'));
    if (linked) {
      openRecording(linked);
    } else if (STATIC) {
      const rec = recordings.find((r) => r.job === jobName) ?? recordings[0];
      if (rec) openRecording(rec);
    } else if (LOCAL) {
      connected = true;
      load();
    } else {
      connect();
    }
    const timer = setInterval(() => {
      const now = performance.now();
      frameTimes = frameTimes.filter((t) => now - t < 1000);
      frameRate = frameTimes.length;
      status = JSON.parse(studio.status());
      const fresh = JSON.parse(studio.events_since(eventsSeen));
      if (fresh.events.length) {
        const lines = [...consoleLines];
        for (const line of fresh.events) {
          // A progress line replaces the one before it from the same step.
          const step = line.match(/^(.*?: round \d+: )\d+ of \d+ rerouted$/)?.[1];
          if (step && lines.length && lines[lines.length - 1].startsWith(step)) lines[lines.length - 1] = line;
          else lines.push(line);
        }
        consoleLines = lines.slice(-400);
        eventsSeen = fresh.total;
      }
      parts = JSON.parse(studio.parts());
      congestion = JSON.parse(studio.congestion());
    }, 250);
    return () => clearInterval(timer);
  });

  const seconds = (ms) => (ms / 1000).toFixed(1);
  const percent = (v) => `${Math.round(v * 100)}%`;
  const selectedPart = $derived(selected ? parts.find((p) => p.id === selected.id) : null);
</script>

<svelte:window onkeydown={key} />

{#snippet recorded()}
  {#if recordings.length}
      <section>
        <h2>Recorded runs</h2>
        <ul class="recordings">
          {#each recordings as rec}
            <li>
              <button class="link" class:current={recording?.file === rec.file} onclick={() => openRecording(rec)}>
                {rec.job} · {rec.recipe}
              </button>
              <span class="muted small">{rec.open} open, {Math.round(rec.seconds)} s</span>
            </li>
          {/each}
        </ul>
        {#if replaying}
          <div class="player">
            <button onclick={() => (playing ? pause() : play())}>{playing ? 'Pause' : 'Play'}</button>
            <input
              type="range"
              min="0"
              max={duration}
              value={position}
              oninput={(e) => {
                pause();
                seek(Number(e.currentTarget.value));
              }}
            />
            <select bind:value={speed} onchange={() => playing && play()} title="Playback speed">
              {#each [1, 2, 4, 8] as s}<option value={s}>{s}×</option>{/each}
            </select>
          </div>
          <p class="muted small">{(position / 1000).toFixed(1)} s of {(duration / 1000).toFixed(1)} s</p>
        {/if}
      </section>
    {/if}
{/snippet}


<svelte:document onfullscreenchange={() => (fullscreen = !!document.fullscreenElement)} />

<main class:sheet-open={sheetOpen}>
  <aside>
    <header>
      <h1><a href="../">Ultraroute</a> Studio</h1>
      {#if !STATIC && !LOCAL}<span class="dot" class:on={connected} title={connected ? 'connected' : 'reconnecting'}></span>{/if}
    </header>

    {#if LOCAL}
      <p class="note">The router runs right here, in this tab: nothing is sent to a server. It routes on one thread, so a phone takes a few minutes.</p>
    {/if}
    {#if STATIC}
      <p class="note">
        These are recordings of real runs. To route live, drag parts and let the router follow, run the
        studio on your machine: see the <a href="https://github.com/sequoia-hope/ultraroute#run-the-studio">README</a>.
      </p>
    {/if}

    {#if STATIC}{@render recorded()}{/if}

    {#if !STATIC}
    <label>
      Board
      <select
        value={fileBoard ? 'file:' : projectBoard ? boardKey(projectBoard) : jobName}
        onchange={(e) => chooseBoard(e.currentTarget.value)}
      >
        {#if fileBoard}<option value="file:" disabled>{fileBoard.pcbName} (from your computer)</option>{/if}
        {#if project}
          {#if !projectBoard && !jobName}<option value="" disabled>Choose a board</option>{/if}
          <optgroup label="Project {project.name}">
            {#each project.boards as b}<option value={boardKey(b)}>{b.board} / {b.variant}</option>{/each}
          </optgroup>
          <optgroup label="Jobs">
            {#each jobs as j}<option value={j.name}>{j.name}</option>{/each}
          </optgroup>
        {:else}
          {#each jobs as j}<option value={j.name}>{j.name}</option>{/each}
        {/if}
      </select>
    </label>
    <div class="open-file">
      <button onclick={() => fileInput.click()} title="A .kicad_pcb, with its .kicad_pro and .kicad_dru if you have them. The files are read here and stay on your computer.">Open board…</button>
      <span class="muted small">or drop the files on the board</span>
      <input
        bind:this={fileInput}
        type="file"
        multiple
        accept=".kicad_pcb,.kicad_pro,.kicad_dru"
        hidden
        onchange={(e) => {
          openFiles(e.currentTarget.files);
          e.currentTarget.value = '';
        }}
      />
    </div>
    {#if fileBoard}
      <p class="note">
        {fileBoard.pcbName}{fileBoard.proName ? ` with ${fileBoard.proName}` : ''}{fileBoard.druName ? ` and ${fileBoard.druName}` : ''},
        opened from your computer. It routes here in this tab and nothing is uploaded; download the result when it's done.
        {#if fileNote}<br />{fileNote}{/if}
      </p>
    {/if}

    {#if job?.dsn && !LOCAL}
    <div class="field">
      <span class="label">Router</span>
      <div class="segmented">
        {#each ROUTERS as r}
          <button
            class:on={router === r.key}
            title={available(r.key) ? r.title : `${r.title} (needs a ${r.key === 'core' ? '.kicad_pcb' : '.dsn'} board in this job)`}
            disabled={!available(r.key)}
            onclick={() => { router = r.key; compose(); load(); }}>{r.label}</button>
        {/each}
      </div>
    </div>
    {:else}
      <p class="muted small">Router: the MIT core, on the board's own KiCad file{job?.strip === 'all' ? ', starting from bare pads' : ''}.</p>
    {/if}
    {#if router === 'grid' || router === 'nudge' || router === 'core'}
      <label class="check"><input type="checkbox" bind:checked={negotiate} onchange={compose} /> Negotiate congestion</label>
    {/if}
    {#if router === 'grid' || router === 'nudge'}
      <label class="check"><input type="checkbox" bind:checked={fineGrid} onchange={compose} /> Fine grid (0.05 mm cells)</label>
    {/if}
    {#if router === 'core'}
      {#if !LOCAL && !projectBoard && !fileBoard}
        <label class="check" title="Run the core router in this tab, compiled to WebAssembly, instead of on the server">
          <input type="checkbox" bind:checked={inTab} onchange={() => (timeout = inTab ? 600 : 120)} /> Route in this tab
        </label>
      {/if}
      <label class="check"><input type="checkbox" bind:checked={ripUp} onchange={compose} /> Rip up and retry what is left</label>
      <label class="check" title="Routes each differential pair (USB D+ and D-) first, its two tracks side by side at the pair's width and gap"><input type="checkbox" bind:checked={diffPairs} onchange={compose} /> Route USB pairs side by side</label>
      <label class="check" title="Escapes every fine-pitch pad to a via first, on a fine local grid"><input type="checkbox" bind:checked={prefanout} onchange={compose} /> Fan out fine-pitch parts first</label>
      <label class="check"><input type="checkbox" bind:checked={tighten} onchange={compose} /> Pull the copper tight</label>
    {:else}
      <label class="check"><input type="checkbox" bind:checked={fanout} onchange={compose} /> Fan out pads first</label>
    {/if}
    {#if presets[0]?.open != null}
      <p class="muted small">Best scored on this board: {presets[0].open} open ({presets[0].name})</p>
    {/if}

    <details>
      <summary>Arguments and environment</summary>
      <label>
        Router arguments
        <textarea rows="3" bind:value={argsText} spellcheck="false"></textarea>
      </label>
      <label>
        Environment
        <textarea rows="3" bind:value={envText} spellcheck="false"></textarea>
      </label>
      <div class="row">
        <label class="small">Time limit (s) <input type="number" min="5" bind:value={timeout} /></label>
        <label class="small">Frames/s <input type="number" min="1" max="240" bind:value={fps} /></label>
      </div>
    </details>

    <label class="check" title="After routing, the router may move unlocked parts a little to make room for what it could not route">
      <input type="checkbox" bind:checked={moveParts} onchange={compose} /> Let the router move parts
    </label>
    <label class="check"><input type="checkbox" bind:checked={keepGoing} /> Continue from the last result</label>
    <label class="check"><input type="checkbox" bind:checked={rerouteOnDrop} /> Reroute after a part is moved</label>
    {#if engine !== 'core'}
      <label class="check"><input type="checkbox" bind:checked={retireDesign} /> Let it rip the design's own wiring</label>
    {/if}

    <div class="buttons">
      <button class="primary" onclick={() => route({ continue: keepGoing })} disabled={!canRoute}>
        {mode === 'routing' ? 'Restart' : 'Route'}
      </button>
      <button onclick={stop} disabled={!running}>Stop</button>
    </div>
    <p class="muted small explain">
      Route lays the whole board{keepGoing ? ', keeping the last result’s copper' : ' from the start'}{moveParts ? ', then moves unlocked parts for what is left' : ''}.
      Each press is one complete run.
    </p>
    <button class="quiet" onclick={load} disabled={!canRoute}>Show the board as designed</button>
    {#if routedText && !running}
      <button class="quiet" onclick={downloadRouted}>Download the routed board (.kicad_pcb)</button>
    {/if}
    {#if projectBoard}
      <button
        class="quiet"
        onclick={saveToProject}
        disabled={!saveable || running}
        title="Writes the routed board over {project.project}/hw/{projectBoard.board}/{projectBoard.variant}/{projectBoard.kicad_pcb}, unless that file changed since it was loaded"
      >Save to project ({projectBoard.kicad_pcb})</button>
      {#if saveNote}<p class="muted small">{saveNote}</p>{/if}
      {#if projectBoard.routing}<p class="muted small">Routes with the project's {projectBoard.routing}.</p>{/if}
    {/if}
    {#if message}<p class="warn">{message}</p>{/if}
    {/if}

    {#if selectedPart}
      <section class="card">
        <h2>
          <span>{selectedPart.name}</span>
          <button class="link" onclick={() => select(null)}>close</button>
        </h2>
        <p class="muted">
          at {(placements[selectedPart.name] ?? selectedPart.at).map((v) => v.toFixed(2)).join(', ')} mm
          {#if placements[selectedPart.name]}· placed by hand{/if}
        </p>
        {#if !STATIC}
        {#if MOVE_BY_HAND}
        <div class="pad">
          <span></span><button onclick={() => moveSelected(0, step)} title="Up (↑)">↑</button><span></span>
          <button onclick={() => moveSelected(-step, 0)} title="Left (←)">←</button>
          <select bind:value={step} title="Step in millimetres">
            {#each STEPS as s}<option value={s}>{s} mm</option>{/each}
          </select>
          <button onclick={() => moveSelected(step, 0)} title="Right (→)">→</button>
          <span></span><button onclick={() => moveSelected(0, -step)} title="Down (↓)">↓</button><span></span>
        </div>
        {/if}
        <div class="buttons">
          <button onclick={() => toggleLock(selectedPart.name)}>
            {locks.includes(selectedPart.name) ? 'Unlock' : 'Lock'}
          </button>
          <button onclick={nudgeThis} title="One complete run in which the router may move only this part, a little, for connections it cannot otherwise make">Route, moving only this</button>
        </div>
        {/if}
        {#if selectedPart.fixed && !locks.includes(selectedPart.name) && !sentLocks.includes(selectedPart.name)}
          <p class="muted">Fixed in the design: the router won't move it{MOVE_BY_HAND ? ', but you can' : ''}.</p>
        {/if}
        {#if placements[selectedPart.name]}
          <button class="link" onclick={() => unplace(selectedPart.name)}>Put it back where the design has it</button>
        {/if}
        {#if !STATIC}<p class="hint">{MOVE_BY_HAND ? 'Arrow keys move it (Shift for 5×), ' : ''}L locks, Esc closes.</p>{/if}
      </section>
    {/if}

    <!-- Closed by default: a board's locked parts (the earring's LEDs) are many and mostly stay locked. -->
    <details class="locked">
      <summary
        onmouseenter={() => highlight([...locks.map((n) => parts.find((p) => p.name === n)?.id), ...designFixed.map((p) => p.id)])}
        onmouseleave={() => highlight([])}
      ><h2>Locked <span class="muted">{locks.length + designFixed.length}</span></h2></summary>
      {#if locks.length + designFixed.length === 0}
        <p class="muted">Nothing is locked.{STATIC ? '' : ' Select a part and press L, or Lock.'}</p>
      {:else}
        <div class="chips">
          {#each locks as name}
            <span class="chip lock" role="listitem" onmouseenter={() => highlight([parts.find((p) => p.name === name)?.id])} onmouseleave={() => highlight([])}>
              <button class="link" onclick={() => selectByName(name)}>{name}</button>
              <button class="link x" onclick={() => toggleLock(name)} title="Unlock">×</button>
            </span>
          {/each}
          {#each designFixed as p}
            <span class="chip fixed" title="Fixed in the design" role="listitem" onmouseenter={() => highlight([p.id])} onmouseleave={() => highlight([])}>
              <button class="link" onclick={() => selectByName(p.name)}>{p.name}</button>
            </span>
          {/each}
        </div>
      {/if}
    </details>

    {#if Object.keys(placements).length}
      <section>
        <h2>Placed by hand <button class="link" onclick={() => { placements = {}; load(); }}>clear</button></h2>
        <ul class="placed">
          {#each Object.entries(placements) as [name, at]}
            <li>
              <button class="link" onclick={() => selectByName(name)}><b>{name}</b></button>
              {at[0].toFixed(2)}, {at[1].toFixed(2)}
              <button class="link" onclick={() => unplace(name)}>×</button>
            </li>
          {/each}
        </ul>
      </section>
    {/if}

    {#if congestion}
      <section>
        <h2>
          Congestion
          <label class="check inline"><input type="checkbox" bind:checked={showCongestion} onchange={toggleCongestion} /> map</label>
        </h2>
        <div class="meter" title="Fullest 1 mm cell">
          <span style="width: {Math.min(100, congestion.peak * 100 / 1.5)}%" class:hot={congestion.peak >= 1}></span>
        </div>
        <p class="muted small">
          Peak {percent(congestion.peak)} · mean {percent(congestion.mean)} · {congestion.over} cells over 80%
        </p>
        {#if congestion.near.length}
          <p class="small">Tight near
            {#each congestion.near as name, i}{i ? ', ' : ' '}<button class="link" onclick={() => selectByName(name)}>{name}</button>{/each}
          </p>
        {/if}
      </section>
    {/if}

    {#if status}
      <section>
        <h2>{replaying ? (playing ? 'Playing a recording' : 'Recording paused') : mode === 'routing' ? 'Routing' : mode === 'loading' ? 'Loading' : status.done ? 'Finished' : 'Idle'}</h2>
        <dl>
          <dt>Open</dt><dd class="big">{status.open ?? '–'}</dd>
          <dt>Elapsed</dt><dd>{seconds(status.ms)} s</dd>
          <dt>Frames/s</dt><dd>{frameRate}</dd>
          <dt>Traces</dt><dd>{status.traces}</dd>
          <dt>Vias</dt><dd>{status.vias}</dd>
          {#if status.done && status.done.passes != null}
            <dt>Passes</dt><dd>{status.done.passes}</dd>
            <dt>Stopped</dt><dd>{status.done.stopped ?? 'finished'}</dd>
          {/if}
        </dl>
        <div class="layers">
          {#each status.layers as layer, i}
            <div class="layer-row" class:active={layer.active}>
              <input type="checkbox" checked={!layer.hidden} title="Show or hide" onchange={(e) => toggleLayer(i, !e.currentTarget.checked)} />
              <button class="link layer-name" title="Make this the active layer" onclick={() => setActive(i)}>
                <span class="swatch" style="background: rgb({layer.color.map((c) => c * 255).join(',')})"></span>
                {layer.name}
              </button>
            </div>
          {/each}
          <label class="check small-gap"><input type="checkbox" bind:checked={highContrast} onchange={() => { studio.set_high_contrast(highContrast); version++; }} /> High contrast (Ctrl+H)</label>
          <label class="check small-gap"><input type="checkbox" bind:checked={flipped} /> Flip board view (Ctrl+Shift+F)</label>
          <label class="check small-gap" title="Zones poured around the copper as KiCad refills them; before the router has poured them, the fills in the board file">
            <input type="checkbox" bind:checked={showPours} onchange={() => { studio.set_show_pours(showPours); version++; }} /> Pours
          </label>
        </div>
      </section>
      <details>
        <summary>Router log</summary>
        <pre>{logs.slice(-40).join('\n')}</pre>
      </details>
      <details>
        <summary>Events</summary>
        <pre>{status.events.join('\n')}</pre>
      </details>
    {/if}
    {#if !STATIC}{@render recorded()}{/if}
    <p class="hint">{STATIC ? 'Click a part to select it.' : 'Click a part to select it, drag to move it.'} Drag empty board to pan, wheel to zoom, double-click to fit.</p>
  </aside>

  <div class="stage">
    <div class="canvas-area" role="region" aria-label="Board" ondragover={dragOver} ondragleave={(e) => { if (!e.currentTarget.contains(e.relatedTarget)) dragging = false; }} ondrop={dropFiles}>
    <Board
      bind:this={board}
      {studio}
      {version}
      mode={viewMode}
      {flipped}
      layers={status?.layers?.map((l) => l.name) ?? []}
      editable={!STATIC && MOVE_BY_HAND}
      ondrop={dropped}
      onselect={select}
      onhover={(h) => (hover = h)}
    />
    <div class="views segmented" role="group" aria-label="Which side to show">
      {#each VIEWS as v}
        <button class:on={viewMode === v.key} onclick={() => (viewMode = v.key)}>{v.label}</button>
        {#if v.key === 'top' && innerLayers.length}
          <select
            class:on={viewMode.startsWith('layer:')}
            value={viewMode.startsWith('layer:') ? viewMode : ''}
            onchange={(e) => (viewMode = e.currentTarget.value || 'all')}
            title="An inner copper layer alone"
          >
            <option value="" disabled>Inner</option>
            {#each innerLayers as l}<option value={l.key}>{l.label}</option>{/each}
          </select>
        {/if}
      {/each}
      <button class="help-button" onclick={() => (showHelp = !showHelp)} title="View controls (?)">?</button>
    </div>
    {#if showHelp}
      <div class="help">
        <h2>View controls <button class="link" onclick={() => (showHelp = false)}>close</button></h2>
        <dl>
          <dt>Wheel</dt><dd>zoom about the pointer</dd>
          <dt>Middle or right drag</dt><dd>pan (left drag on empty board too)</dd>
          <dt>Home</dt><dd>zoom to fit (or double-click)</dd>
          <dt>F1 / F2</dt><dd>zoom in / out</dd>
          <dt>F4</dt><dd>centre on the pointer</dd>
          <dt>PgUp / PgDn</dt><dd>front / back copper active</dd>
          <dt>+ / −</dt><dd>next / previous copper layer</dd>
          <dt>Ctrl+H</dt><dd>high contrast</dd>
          <dt>Ctrl+Shift+F</dt><dd>flip board view</dd>
          <dt>Click / drag a part</dt><dd>select / move it</dd>
          <dt>Arrows, L, Esc</dt><dd>nudge, lock, deselect the selected part</dd>
        </dl>
      </div>
    {/if}
    {#if running && consoleLines.length}
      <div class="doing">
        <span class="pulse"></span>
        <b>{status?.focus ?? (mode === 'loading' ? 'loading' : 'routing')}</b>
        <span>{consoleLines[consoleLines.length - 1]?.replace(/^[^:]*: /, '')}</span>
      </div>
    {/if}
    {#if hover.part || hover.net}
      <div class="hover">
        {#if hover.part}<b>{hover.part.name}</b>{#if hover.part.fixed}<span class="muted">locked</span>{/if}{/if}
        {#if hover.net}<span>{hover.net}</span>{/if}
      </div>
    {/if}
    {#if dragging}
      <div class="drop-here">Drop a .kicad_pcb (with its .kicad_pro and .kicad_dru) to open it here</div>
    {/if}
    {#if showCongestion}
      <div class="legend">
        <span>free</span><span class="ramp"></span><span>full</span>
      </div>
    {/if}
    </div>
    {#if !STATIC}
      <div class="console" class:open={consoleOpen}>
        <button class="console-head" onclick={() => (consoleOpen = !consoleOpen)}>
          Router {consoleOpen ? '▾' : '▸'} <span class="muted">{consoleLines.length ? consoleLines[consoleLines.length - 1] : 'quiet'}</span>
        </button>
        {#if consoleOpen}
          <pre bind:this={consoleBox}>{#each consoleLines.slice(-120) as line}<span class="phase-{line.split(':')[0].replace(/\s+/g, '-')}">{line}</span>
{/each}</pre>
        {/if}
      </div>
    {/if}
    <div class="phone-bar">
      {#if !STATIC}
        {#if running}
          <button onclick={stop}>Stop</button>
        {:else}
          <button class="primary" onclick={() => route({ continue: keepGoing })} disabled={!canRoute}>Route</button>
        {/if}
      {/if}
      <span class="phone-stat" aria-live="polite"><b class:done={status?.open === 0}>{status?.open ?? '–'}</b> open · {seconds(status?.ms ?? 0)} s</span>
      <button class="sheet-toggle" onclick={() => (sheetOpen = !sheetOpen)} aria-expanded={sheetOpen}>{sheetOpen ? 'Hide' : 'Controls'}</button>
      {#if canFullscreen}<button class="icon" onclick={toggleFullscreen} title={fullscreen ? 'Exit full screen' : 'Full screen'} aria-label={fullscreen ? 'Exit full screen' : 'Full screen'}>{fullscreen ? '⤡' : '⤢'}</button>{/if}
    </div>
  </div>
</main>

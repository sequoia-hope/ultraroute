<script>
  import { onMount } from 'svelte';
  import { Studio } from './core/studio_core.js';
  import Board from './lib/Board.svelte';

  const studio = new Studio();
  // Built for a static host: no router to talk to, only recorded runs.
  const STATIC = import.meta.env.VITE_STATIC === '1';
  let version = $state(0);
  let board;

  let jobs = $state([]);
  let jobName = $state('');
  let argsText = $state('');
  let envText = $state('');
  let timeout = $state(120);
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
  const job = $derived(jobs.find((j) => j.name === jobName));
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
  let strip = $state(true);
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
      if (negotiate) args.push('--negotiate');
      if (strip) args.push('--strip');
      argsText = args.join(' ');
      envText = '';
      return;
    }
    if (router !== 'maze') args.push('--backend', router);
    if (!fanout) args.push('--set', 'router.fanout.enabled=false');
    const grid = router === 'grid' || router === 'nudge';
    if (grid && negotiate) env.COPPERROUTE_GRID_NEGOTIATE = '1';
    if (grid && fineGrid) env.COPPERROUTE_GRID_CELL_MM = '0.05';
    if (router === 'nudge') env.COPPERROUTE_NUDGE_BASE = 'grid';
    argsText = args.join(' ');
    envText = Object.entries(env)
      .map(([k, v]) => `${k}=${v}`)
      .join('\n');
  }

  function chooseJob(name) {
    jobName = name;
    placements = {};
    selected = null;
    refitNext = true;
    const chosen = jobs.find((j) => j.name === name);
    const withLocks = chosen?.presets?.find((p) => splitLocks(p.args ?? []).found.length);
    locks = withLocks ? splitLocks(withLocks.args).found : [];
    if (!available(router, chosen)) router = available('grid', chosen) ? 'grid' : 'core';
    compose();
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
    if (!socket || socket.readyState !== WebSocket.OPEN || !jobName) return;
    if (lockList.length) args = [...args, '--lock', lockList.join(',')];
    if (retireDesign) args = [...args, '--retire-fixed', 'all'];
    sentLocks = [...lockList];
    socket.send(
      JSON.stringify({
        cmd,
        job: jobName,
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

  function nudgeAll() {
    leaveReplay();
    send('run', withBackend(argsText.split(/\s+/).filter(Boolean), 'nudge'), locks, { continue: keepGoing });
  }

  // Lets the nudger move only this part: every other part is locked for the run.
  function nudgeThis() {
    if (!selected) return;
    const others = parts.map((p) => p.name).filter((n) => n !== selected.name);
    leaveReplay();
    send('run', withBackend(argsText.split(/\s+/).filter(Boolean), 'nudge'), others, { continue: keepGoing });
  }

  function stop() {
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
    socket.onmessage = (event) => receive(event.data);
  }

  function toggleLayer(index, hidden) {
    studio.set_layer_hidden(index, hidden);
    version++;
  }

  function toggleCongestion() {
    studio.set_congestion(showCongestion);
    version++;
  }

  function key(event) {
    if (!selected || /INPUT|TEXTAREA|SELECT/.test(event.target.tagName)) return;
    if (STATIC && event.key !== 'Escape') return;
    const k = event.shiftKey ? 5 : 1;
    const moves = { ArrowLeft: [-1, 0], ArrowRight: [1, 0], ArrowUp: [0, 1], ArrowDown: [0, -1] };
    if (moves[event.key]) {
      event.preventDefault();
      moveSelected(moves[event.key][0] * step * k, moves[event.key][1] * step * k);
    } else if (event.key === 'l') {
      toggleLock(selected.name);
    } else if (event.key === 'Escape') {
      select(null);
    }
  }

  onMount(async () => {
    jobs = await (await fetch('../api/jobs')).json();
    recordings = await fetch('replays/index.json')
      .then((r) => (r.ok ? r.json() : []))
      .catch(() => []);
    const asked = new URLSearchParams(location.search);
    const first = jobs.find((j) => j.name === asked.get('job')) ?? jobs.find((j) => j.name === 'bluebuzz-1024') ?? jobs[0];
    if (first) chooseJob(first.name);
    if (presets.some((p) => p.name === asked.get('recipe'))) {
      choosePreset(asked.get('recipe'));
    }
    if (STATIC) {
      const rec = recordings.find((r) => r.job === jobName) ?? recordings[0];
      if (rec) openRecording(rec);
    } else {
      connect();
    }
    const timer = setInterval(() => {
      const now = performance.now();
      frameTimes = frameTimes.filter((t) => now - t < 1000);
      frameRate = frameTimes.length;
      status = JSON.parse(studio.status());
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


<main>
  <aside>
    <header>
      <h1><a href="../">Ultraroute</a> Studio</h1>
      {#if !STATIC}<span class="dot" class:on={connected} title={connected ? 'connected' : 'reconnecting'}></span>{/if}
    </header>

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
        value={jobName}
        onchange={(e) => {
          chooseJob(e.currentTarget.value);
          load();
        }}
      >
        {#each jobs as j}<option value={j.name}>{j.name}</option>{/each}
      </select>
    </label>

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
    {#if router === 'grid' || router === 'nudge' || router === 'core'}
      <label class="check"><input type="checkbox" bind:checked={negotiate} onchange={compose} /> Negotiate congestion</label>
    {/if}
    {#if router === 'grid' || router === 'nudge'}
      <label class="check"><input type="checkbox" bind:checked={fineGrid} onchange={compose} /> Fine grid (0.05 mm cells)</label>
    {/if}
    {#if router === 'core'}
      <label class="check"><input type="checkbox" bind:checked={strip} onchange={compose} /> Rip up unlocked copper first</label>
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

    <label class="check"><input type="checkbox" bind:checked={keepGoing} /> Continue from the last result</label>
    <label class="check"><input type="checkbox" bind:checked={rerouteOnDrop} /> Reroute after a part is moved</label>
    <label class="check"><input type="checkbox" bind:checked={retireDesign} /> Let it rip the design's own wiring</label>

    <div class="buttons">
      <button class="primary" onclick={() => route({ continue: keepGoing })} disabled={!connected}>
        {mode === 'routing' ? 'Restart' : 'Route'}
      </button>
      <button onclick={nudgeAll} disabled={!connected || engine === 'core'} title="Route, then move unlocked parts that block open connections">Nudge</button>
      <button onclick={stop} disabled={!running}>Stop</button>
    </div>
    <button class="quiet" onclick={load} disabled={!connected}>Show the board as designed</button>
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
        <div class="pad">
          <span></span><button onclick={() => moveSelected(0, step)} title="Up (↑)">↑</button><span></span>
          <button onclick={() => moveSelected(-step, 0)} title="Left (←)">←</button>
          <select bind:value={step} title="Step in millimetres">
            {#each STEPS as s}<option value={s}>{s} mm</option>{/each}
          </select>
          <button onclick={() => moveSelected(step, 0)} title="Right (→)">→</button>
          <span></span><button onclick={() => moveSelected(0, -step)} title="Down (↓)">↓</button><span></span>
        </div>
        <div class="buttons">
          <button onclick={() => toggleLock(selectedPart.name)}>
            {locks.includes(selectedPart.name) ? 'Unlock' : 'Lock'}
          </button>
          <button onclick={nudgeThis} disabled={engine === 'core'} title="Route, then let the nudger move only this part">Nudge it for me</button>
        </div>
        {/if}
        {#if selectedPart.fixed && !locks.includes(selectedPart.name) && !sentLocks.includes(selectedPart.name)}
          <p class="muted">Fixed in the design: the router won't move it, but you can.</p>
        {/if}
        {#if placements[selectedPart.name]}
          <button class="link" onclick={() => unplace(selectedPart.name)}>Put it back where the design has it</button>
        {/if}
        {#if !STATIC}<p class="hint">Arrow keys move it (Shift for 5×), L locks, Esc closes.</p>{/if}
      </section>
    {/if}

    <section>
      <h2>Locked <span class="muted">{locks.length + designFixed.length}</span></h2>
      {#if locks.length + designFixed.length === 0}
        <p class="muted">Nothing is locked.{STATIC ? '' : ' Select a part and press L, or Lock.'}</p>
      {:else}
        <div class="chips">
          {#each locks as name}
            <span class="chip lock">
              <button class="link" onclick={() => selectByName(name)}>{name}</button>
              <button class="link x" onclick={() => toggleLock(name)} title="Unlock">×</button>
            </span>
          {/each}
          {#each designFixed as p}
            <span class="chip fixed" title="Fixed in the design">
              <button class="link" onclick={() => selectByName(p.name)}>{p.name}</button>
            </span>
          {/each}
        </div>
      {/if}
    </section>

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
            <label class="check">
              <input type="checkbox" checked={!layer.hidden} onchange={(e) => toggleLayer(i, !e.currentTarget.checked)} />
              <span class="swatch" style="background: rgb({layer.color.map((c) => c * 255).join(',')})"></span>
              {layer.name}
            </label>
          {/each}
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
    <Board bind:this={board} {studio} {version} editable={!STATIC} ondrop={dropped} onselect={select} onhover={(h) => (hover = h)} />
    {#if hover.part || hover.net}
      <div class="hover">
        {#if hover.part}<b>{hover.part.name}</b>{#if hover.part.fixed}<span class="muted">locked</span>{/if}{/if}
        {#if hover.net}<span>{hover.net}</span>{/if}
      </div>
    {/if}
    {#if showCongestion}
      <div class="legend">
        <span>free</span><span class="ramp"></span><span>full</span>
      </div>
    {/if}
  </div>
</main>

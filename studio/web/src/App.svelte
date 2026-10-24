<script>
  import { onMount } from 'svelte';
  import { Studio } from './core/studio_core.js';
  import Board from './lib/Board.svelte';

  const studio = new Studio();
  let version = $state(0);
  let board;

  let jobs = $state([]);
  let jobName = $state('');
  let presetName = $state('');
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

  function choosePreset(name) {
    const preset = presets.find((p) => p.name === name);
    const { rest, found } = splitLocks(preset?.args ?? []);
    argsText = rest.join(' ');
    locks = found;
    envText = Object.entries(preset?.env ?? {})
      .map(([k, v]) => `${k}=${v}`)
      .join('\n');
  }

  function chooseJob(name) {
    jobName = name;
    placements = {};
    selected = null;
    refitNext = true;
    const first = jobs.find((j) => j.name === name)?.presets?.[0];
    presetName = first?.name ?? '';
    choosePreset(presetName);
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
        place: placements,
        continue: options.continue ?? false,
      }),
    );
    mode = cmd === 'load' ? 'loading' : 'routing';
    message = '';
    logs = [];
  }

  function load() {
    clearTimeout(rerouteTimer);
    send('load', ['--backend', 'none'], locks);
  }

  function route(options = {}) {
    clearTimeout(rerouteTimer);
    send('run', argsText.split(/\s+/).filter(Boolean), locks, options);
  }

  function nudgeAll() {
    send('run', withBackend(argsText.split(/\s+/).filter(Boolean), 'nudge'), locks, { continue: keepGoing });
  }

  // Lets the nudger move only this part: every other part is locked for the run.
  function nudgeThis() {
    if (!selected) return;
    const others = parts.map((p) => p.name).filter((n) => n !== selected.name);
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

  function receive(line) {
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
    jobs = await (await fetch('/api/jobs')).json();
    const asked = new URLSearchParams(location.search);
    const first = jobs.find((j) => j.name === asked.get('job')) ?? jobs.find((j) => j.name === 'bluebuzz-1024') ?? jobs[0];
    if (first) chooseJob(first.name);
    if (presets.some((p) => p.name === asked.get('recipe'))) {
      presetName = asked.get('recipe');
      choosePreset(presetName);
    }
    connect();
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

<main>
  <aside>
    <header>
      <h1><a href="/">Ultraroute</a> Studio</h1>
      <span class="dot" class:on={connected} title={connected ? 'connected' : 'reconnecting'}></span>
    </header>

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

    <label>
      Recipe
      <select
        value={presetName}
        onchange={(e) => {
          presetName = e.currentTarget.value;
          choosePreset(presetName);
          load();
        }}
      >
        {#each presets as p}
          <option value={p.name}>{p.name}{p.open != null ? ` — ${p.open} open` : ''}</option>
        {/each}
      </select>
    </label>

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
      <button onclick={nudgeAll} disabled={!connected} title="Route, then move unlocked parts that block open connections">Nudge</button>
      <button onclick={stop} disabled={!running}>Stop</button>
    </div>
    <button class="quiet" onclick={load} disabled={!connected}>Show the board as designed</button>
    {#if message}<p class="warn">{message}</p>{/if}

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
          <button onclick={nudgeThis} title="Route, then let the nudger move only this part">Nudge it for me</button>
        </div>
        {#if selectedPart.fixed && !locks.includes(selectedPart.name) && !sentLocks.includes(selectedPart.name)}
          <p class="muted">Fixed in the design: the router won't move it, but you can.</p>
        {/if}
        {#if placements[selectedPart.name]}
          <button class="link" onclick={() => unplace(selectedPart.name)}>Put it back where the design has it</button>
        {/if}
        <p class="hint">Arrow keys move it (Shift for 5×), L locks, Esc closes.</p>
      </section>
    {/if}

    <section>
      <h2>Locked <span class="muted">{locks.length + designFixed.length}</span></h2>
      {#if locks.length + designFixed.length === 0}
        <p class="muted">Nothing is locked. Select a part and press L, or Lock.</p>
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
        <h2>{mode === 'routing' ? 'Routing' : mode === 'loading' ? 'Loading' : status.done ? 'Finished' : 'Idle'}</h2>
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
    <p class="hint">Click a part to select it, drag to move it. Drag empty board to pan, wheel to zoom, double-click to fit.</p>
  </aside>

  <div class="stage">
    <Board bind:this={board} {studio} {version} ondrop={dropped} onselect={select} onhover={(h) => (hover = h)} />
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

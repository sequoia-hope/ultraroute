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

  let connected = $state(false);
  let running = $state(false);
  let status = $state(null);
  let logs = $state([]);
  let message = $state('');
  let hover = $state({ part: null, net: '' });
  let frameRate = $state(0);
  let socket;
  let frameTimes = [];

  const job = $derived(jobs.find((j) => j.name === jobName));
  const presets = $derived(job?.presets ?? []);

  function choosePreset(name) {
    const preset = presets.find((p) => p.name === name);
    argsText = (preset?.args ?? []).join(' ');
    envText = Object.entries(preset?.env ?? {})
      .map(([k, v]) => `${k}=${v}`)
      .join('\n');
  }

  function chooseJob(name) {
    jobName = name;
    placements = {};
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

  function route(options = {}) {
    if (!socket || socket.readyState !== WebSocket.OPEN || !jobName) return;
    const args = argsText.split(/\s+/).filter(Boolean);
    if (retireDesign) args.push('--retire-fixed', 'all');
    socket.send(
      JSON.stringify({
        cmd: 'run',
        job: jobName,
        args,
        env: environment(),
        timeout: Number(timeout) || undefined,
        fps: Number(fps) || undefined,
        place: placements,
        continue: options.continue ?? false,
      }),
    );
    message = '';
    logs = [];
  }

  function stop() {
    socket?.send(JSON.stringify({ cmd: 'stop' }));
  }

  function dropped(part, by) {
    placements = { ...placements, [part.name]: [round(part.at[0] + by[0]), round(part.at[1] + by[1])] };
    if (rerouteOnDrop) route({ continue: keepGoing });
  }

  function unplace(name) {
    const { [name]: _, ...rest } = placements;
    placements = rest;
  }

  function round(v) {
    return Math.round(v * 1000) / 1000;
  }

  function receive(line) {
    if (studio.apply(line)) {
      version++;
      if (line.includes('"t":"frame"')) frameTimes.push(performance.now());
      else if (line.includes('"t":"board"')) board?.refit();
      return;
    }
    let event;
    try {
      event = JSON.parse(line);
    } catch {
      return;
    }
    if (event.t === 'run') {
      running = true;
      studio.clear_offsets();
    } else if (event.t === 'eof') {
      running = false;
    } else if (event.t === 'log') {
      if (/ (INFO|WARN|ERROR) /.test(event.line)) logs = [...logs.slice(-200), event.line];
    } else if (event.t === 'error') {
      message = event.message;
    }
  }

  function connect() {
    const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
    socket = new WebSocket(`${scheme}://${location.host}/ws`);
    socket.onopen = () => (connected = true);
    socket.onclose = () => {
      connected = false;
      running = false;
      setTimeout(connect, 1500);
    };
    socket.onmessage = (event) => receive(event.data);
  }

  function toggleLayer(index, hidden) {
    studio.set_layer_hidden(index, hidden);
    version++;
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
    }, 250);
    return () => clearInterval(timer);
  });

  const seconds = (ms) => (ms / 1000).toFixed(1);
</script>

<main>
  <aside>
    <header>
      <h1>Ultraroute Studio</h1>
      <span class="dot" class:on={connected} title={connected ? 'connected' : 'reconnecting'}></span>
    </header>

    <label>
      Board
      <select value={jobName} onchange={(e) => chooseJob(e.currentTarget.value)}>
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
        }}
      >
        {#each presets as p}
          <option value={p.name}>{p.name}{p.open != null ? ` — ${p.open} open` : ''}</option>
        {/each}
      </select>
    </label>

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

    <label class="check"><input type="checkbox" bind:checked={keepGoing} /> Continue from the last result</label>
    <label class="check"><input type="checkbox" bind:checked={rerouteOnDrop} /> Reroute when a part is dropped</label>
    <label class="check"><input type="checkbox" bind:checked={retireDesign} /> Let it rip the design's own wiring</label>

    <div class="buttons">
      <button class="primary" onclick={() => route({ continue: keepGoing })} disabled={!connected}>
        {running ? 'Restart' : 'Route'}
      </button>
      <button onclick={stop} disabled={!running}>Stop</button>
    </div>
    {#if message}<p class="warn">{message}</p>{/if}

    {#if Object.keys(placements).length}
      <section>
        <h2>Placed by hand <button class="link" onclick={() => (placements = {})}>clear</button></h2>
        <ul class="placed">
          {#each Object.entries(placements) as [name, at]}
            <li><b>{name}</b> {at[0].toFixed(2)}, {at[1].toFixed(2)} <button class="link" onclick={() => unplace(name)}>×</button></li>
          {/each}
        </ul>
      </section>
    {/if}

    {#if status}
      <section>
        <h2>{running ? 'Routing' : status.done ? 'Finished' : 'Idle'}</h2>
        <dl>
          <dt>Open</dt><dd class="big">{status.open ?? '–'}</dd>
          <dt>Elapsed</dt><dd>{seconds(status.ms)} s</dd>
          <dt>Frames/s</dt><dd>{frameRate}</dd>
          <dt>Traces</dt><dd>{status.traces}</dd>
          <dt>Vias</dt><dd>{status.vias}</dd>
          {#if status.done}
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
    <p class="hint">Drag a part to place it. Drag empty board to pan, wheel to zoom, double-click to fit.</p>
  </aside>

  <div class="stage">
    <Board bind:this={board} {studio} {version} ondrop={dropped} onhover={(h) => (hover = h)} />
    {#if hover.part || hover.net}
      <div class="hover">
        {#if hover.part}<b>{hover.part.name}</b>{/if}
        {#if hover.net}<span>{hover.net}</span>{/if}
      </div>
    {/if}
  </div>
</main>

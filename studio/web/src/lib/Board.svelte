<script>
  import { onMount } from 'svelte';
  import { Renderer } from './renderer.js';

  // studio: the WASM scene. version: bumped whenever the scene changed.
  let { studio, version, editable = true, ondrop, onselect, onhover } = $props();

  let canvas;
  let renderer;
  let error = $state('');
  let view = { cx: 0, cy: 0, zoom: 4 };
  let fitted = false;
  let drawn = -1;
  let drawnZoom = 0;
  let viewChanged = true;
  let drag = null;
  let hoverPart = 0;
  let hoverNet = 0;

  const BACKGROUND = [0.063, 0.11, 0.145];

  function fit() {
    const b = studio.bbox();
    if (!(b[2] > b[0])) return false;
    const w = canvas.width, h = canvas.height;
    view.cx = (b[0] + b[2]) / 2;
    view.cy = (b[1] + b[3]) / 2;
    view.zoom = 0.92 * Math.min(w / (b[2] - b[0]), h / (b[3] - b[1]));
    viewChanged = true;
    return true;
  }

  export function refit() {
    fitted = fit();
  }

  function toBoard(event) {
    const rect = canvas.getBoundingClientRect();
    const dpr = canvas.width / rect.width;
    const x = (event.clientX - rect.left) * dpr - canvas.width / 2;
    const y = canvas.height / 2 - (event.clientY - rect.top) * dpr;
    return [view.cx + x / view.zoom, view.cy + y / view.zoom];
  }

  function frame() {
    if (renderer) {
      if (!fitted && version > 0) fitted = fit();
      const pxChanged = Math.abs(drawnZoom - view.zoom) / view.zoom > 0.15;
      if (version !== drawn || pxChanged) {
        renderer.upload(studio.mesh(1 / view.zoom));
        drawn = version;
        drawnZoom = view.zoom;
        viewChanged = true;
      }
      if (viewChanged) {
        renderer.draw(view, BACKGROUND);
        viewChanged = false;
      }
    }
    requestAnimationFrame(frame);
  }

  function resize() {
    const dpr = window.devicePixelRatio || 1;
    const rect = canvas.getBoundingClientRect();
    canvas.width = Math.max(1, Math.round(rect.width * dpr));
    canvas.height = Math.max(1, Math.round(rect.height * dpr));
    viewChanged = true;
  }

  function wheel(event) {
    event.preventDefault();
    const [x, y] = toBoard(event);
    const factor = Math.exp(-event.deltaY * 0.0015);
    view.zoom = Math.min(2000, Math.max(0.2, view.zoom * factor));
    const [x2, y2] = toBoard(event);
    view.cx += x - x2;
    view.cy += y - y2;
    viewChanged = true;
  }

  function down(event) {
    canvas.setPointerCapture(event.pointerId);
    const at = toBoard(event);
    const part = event.button === 0 && !event.shiftKey ? studio.part_at(at[0], at[1]) : 0;
    drag = { part, start: at, client: [event.clientX, event.clientY], view: { ...view }, moved: false };
    if (part) {
      studio.set_picked(part);
      drawn = -1;
    }
  }

  function move(event) {
    const at = toBoard(event);
    if (drag) {
      drag.moved = true;
      if (drag.part && !editable) {
        return;
      } else if (drag.part) {
        studio.set_offset(drag.part, at[0] - drag.start[0], at[1] - drag.start[1]);
        drawn = -1;
      } else {
        const rect = canvas.getBoundingClientRect();
        const dpr = canvas.width / rect.width;
        view.cx = drag.view.cx - ((event.clientX - drag.client[0]) * dpr) / view.zoom;
        view.cy = drag.view.cy + ((event.clientY - drag.client[1]) * dpr) / view.zoom;
        viewChanged = true;
      }
      return;
    }
    const part = studio.part_at(at[0], at[1]);
    const net = studio.net_at(at[0], at[1], 4 / view.zoom);
    if (part !== hoverPart || net !== hoverNet) {
      hoverPart = part;
      hoverNet = net;
      studio.set_hot_net(net);
      studio.set_picked(part);
      drawn = -1;
      onhover?.({ part: part ? JSON.parse(studio.part(part)) : null, net: net ? studio.net_name(net) : '' });
    }
  }

  function up(event) {
    const at = toBoard(event);
    const dragged = drag && Math.hypot(at[0] - drag.start[0], at[1] - drag.start[1]) * view.zoom > 3;
    if (drag?.part && dragged && editable) {
      const info = JSON.parse(studio.part(drag.part));
      ondrop?.(info, [at[0] - drag.start[0], at[1] - drag.start[1]]);
    } else if (drag && !dragged) {
      onselect?.(drag.part ? JSON.parse(studio.part(drag.part)) : null);
    }
    drag = null;
  }

  onMount(() => {
    try {
      renderer = new Renderer(canvas);
    } catch (e) {
      error = String(e.message || e);
    }
    const observer = new ResizeObserver(resize);
    observer.observe(canvas);
    resize();
    requestAnimationFrame(frame);
    return () => observer.disconnect();
  });
</script>

<div class="board">
  <canvas
    bind:this={canvas}
    onwheel={wheel}
    onpointerdown={down}
    onpointermove={move}
    onpointerup={up}
    ondblclick={refit}
  ></canvas>
  {#if error}<p class="error">{error}</p>{/if}
</div>

<style>
  .board {
    position: relative;
    width: 100%;
    height: 100%;
  }
  canvas {
    width: 100%;
    height: 100%;
    display: block;
    touch-action: none;
    cursor: crosshair;
  }
  .error {
    position: absolute;
    inset: 40% 0 auto;
    text-align: center;
    color: var(--warn);
  }
</style>

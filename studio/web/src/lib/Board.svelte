<script>
  import { onMount, untrack } from 'svelte';
  import { Renderer } from './renderer.js';

  // studio: the WASM scene. version: bumped whenever the scene changed.
  // mode: 'all', 'top', 'bottom' or 'split' (top and bottom side by side, the bottom seen
  // from below). layers: the layer names, front to back.
  // flipped: the whole board seen from below, as KiCad's Flip Board View.
  let { studio, version, mode = 'all', flipped = false, layers = [], editable = true, ondrop, onselect, onhover } = $props();

  let canvas;
  let renderer;
  let error = $state('');
  let view = { cx: 0, cy: 0, zoom: 4 };
  let fitted = false;
  let drawn = -1;
  let drawnZoom = 0;
  let drawnMode = '';
  let viewChanged = true;
  let drag = null;
  let hoverPart = 0;
  let hoverNet = 0;

  // KiCad's default background.
  const BACKGROUND = [0, 16 / 255, 35 / 255];
  let pointer = null;
  const ALL = 0xffffffff;
  // Fingers on the canvas, by pointer id; two of them pinch to zoom and pan together.
  const touches = new Map();
  let pinch = null;
  let lastTap = 0;

  const top = () => 1;
  const bottom = () => (layers.length > 1 ? 1 << (layers.length - 1) : 1);

  // The panes the canvas is split into, each with its layers and whether it is mirrored.
  function panes() {
    const w = canvas.width, h = canvas.height;
    if (mode === 'split') {
      const half = Math.floor(w / 2);
      return [
        { rect: { x: 0, y: 0, w: half, h }, mask: top(), mirror: false },
        { rect: { x: half, y: 0, w: w - half, h }, mask: bottom(), mirror: true },
      ];
    }
    const mask = mode === 'top' ? top() : mode === 'bottom' ? bottom() : ALL;
    return [{ rect: { x: 0, y: 0, w, h }, mask, mirror: mode === 'bottom' || (mode === 'all' && flipped) }];
  }

  function fit() {
    const b = studio.bbox();
    if (!(b[2] > b[0]) || !canvas) return false;
    const pane = panes()[0].rect;
    view.cx = (b[0] + b[2]) / 2;
    view.cy = (b[1] + b[3]) / 2;
    view.zoom = 0.92 * Math.min(pane.w / (b[2] - b[0]), pane.h / (b[3] - b[1]));
    viewChanged = true;
    return true;
  }

  export function refit() {
    fitted = fit();
  }

  // The board point under the pointer, in the pane the pointer is over, with `using` as the view.
  function toBoard(event, using = view) {
    const rect = canvas.getBoundingClientRect();
    const dpr = canvas.width / rect.width;
    const px = (event.clientX - rect.left) * dpr;
    const py = canvas.height - (event.clientY - rect.top) * dpr;
    const pane = panes().find((p) => px >= p.rect.x && px < p.rect.x + p.rect.w) ?? panes()[0];
    let x = px - (pane.rect.x + pane.rect.w / 2);
    const y = py - (pane.rect.y + pane.rect.h / 2);
    if (pane.mirror) x = -x;
    return [using.cx + x / using.zoom, using.cy + y / using.zoom];
  }

  function frame() {
    if (renderer && canvas) {
      const now = performance.now();
      if (!fitted && version > 0) fitted = fit();
      const pxChanged = Math.abs(drawnZoom - view.zoom) / view.zoom > 0.15;
      const moving = studio.animating(now);
      if (version !== drawn || pxChanged || mode + flipped !== drawnMode || moving) {
        panes().forEach((p, i) => renderer.upload(i, studio.mesh_view(1 / view.zoom, p.mask, now)));
        drawn = version;
        drawnZoom = view.zoom;
        drawnMode = mode + flipped;
        viewChanged = true;
      }
      if (viewChanged) {
        renderer.clear(BACKGROUND);
        panes().forEach((p, i) => renderer.draw(i, view, p.rect, p.mirror));
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

  // KiCad's zoom keys: Home fits, F1 and F2 zoom in and out about the pointer, F4 centres on it.
  function zoomAbout(factor, event) {
    const at = event ? toBoard(event) : [view.cx, view.cy];
    view.zoom = Math.min(2000, Math.max(0.2, view.zoom * factor));
    if (event) {
      const after = toBoard(event);
      view.cx += at[0] - after[0];
      view.cy += at[1] - after[1];
    }
    viewChanged = true;
  }

  function key(event) {
    if (/INPUT|TEXTAREA|SELECT/.test(event.target.tagName)) return;
    const handled = { Home: () => refit(), F1: () => zoomAbout(1.4, pointer), F2: () => zoomAbout(1 / 1.4, pointer) };
    if (event.key === 'F4' && pointer) {
      const at = toBoard(pointer);
      view.cx = at[0];
      view.cy = at[1];
      viewChanged = true;
      event.preventDefault();
    } else if (handled[event.key]) {
      handled[event.key]();
      event.preventDefault();
    }
  }

  const middle = () => {
    const [a, b] = [...touches.values()];
    return { clientX: (a.clientX + b.clientX) / 2, clientY: (a.clientY + b.clientY) / 2, apart: Math.hypot(a.clientX - b.clientX, a.clientY - b.clientY) };
  };

  function startPinch() {
    const m = middle();
    pinch = { at: toBoard(m), apart: Math.max(m.apart, 1), zoom: view.zoom };
    if (drag?.part) studio.set_offset(drag.part, 0, 0);
    drag = null;
    drawn = -1;
  }

  // The board point first under the fingers' midpoint stays under it as they spread and move.
  function movePinch() {
    const m = middle();
    view.zoom = Math.min(2000, Math.max(0.2, (pinch.zoom * m.apart) / pinch.apart));
    const now = toBoard(m);
    view.cx += pinch.at[0] - now[0];
    view.cy += pinch.at[1] - now[1];
    viewChanged = true;
  }

  function down(event) {
    canvas.setPointerCapture(event.pointerId);
    if (event.pointerType === 'touch') {
      touches.set(event.pointerId, { clientX: event.clientX, clientY: event.clientY });
      if (touches.size === 2) return startPinch();
      if (touches.size > 2) return;
    }
    const at = toBoard(event);
    // Middle and right buttons pan, as in KiCad; the left picks a part or pans empty board.
    const part = event.button === 0 && !event.shiftKey ? studio.part_at(at[0], at[1]) : 0;
    drag = { part, start: at, screen: [event.clientX, event.clientY], view: { ...view }, moved: false };
    if (part) {
      studio.set_picked(part);
      drawn = -1;
    }
  }

  function move(event) {
    if (touches.has(event.pointerId)) {
      touches.set(event.pointerId, { clientX: event.clientX, clientY: event.clientY });
      if (pinch) return movePinch();
    }
    if (event.pointerType === 'touch' && !drag) return;
    pointer = { clientX: event.clientX, clientY: event.clientY };
    const at = toBoard(event);
    if (drag) {
      drag.moved = true;
      // A part held where parts can't be moved pans the board, as empty board does: on a phone a
      // finger landing on a chip still scrolls.
      if (drag.part && editable) {
        studio.set_offset(drag.part, at[0] - drag.start[0], at[1] - drag.start[1]);
        drawn = -1;
      } else {
        // Keep the point grabbed under the pointer, in either pane, mirrored or not.
        const held = toBoard(event, drag.view);
        view.cx = drag.view.cx - (held[0] - drag.start[0]);
        view.cy = drag.view.cy - (held[1] - drag.start[1]);
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
    touches.delete(event.pointerId);
    if (pinch) {
      if (touches.size < 2) pinch = null;
      return;
    }
    const at = toBoard(event);
    // Measured on the screen: a pan keeps the board point under the finger, so the board's own
    // distance would call every pan a tap.
    const dragged = drag && Math.hypot(event.clientX - drag.screen[0], event.clientY - drag.screen[1]) > 3;
    // A double tap on empty board fits it, as a double click does.
    if (event.pointerType === 'touch' && drag && !dragged && !drag.part) {
      const now = performance.now();
      if (now - lastTap < 320) {
        refit();
        lastTap = 0;
        drag = null;
        return;
      }
      lastTap = now;
    }
    if (drag?.part && dragged && editable) {
      const info = JSON.parse(studio.part(drag.part));
      ondrop?.(info, [at[0] - drag.start[0], at[1] - drag.start[1]]);
    } else if (drag && !dragged) {
      onselect?.(drag.part ? JSON.parse(studio.part(drag.part)) : null);
    }
    drag = null;
  }

  // Refit when the view changes, and only then: fitting reads the layers, which are renewed
  // with every status update.
  $effect(() => {
    mode;
    flipped;
    untrack(() => {
      if (canvas) fitted = fit();
    });
  });

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

<svelte:window onkeydown={key} />

<div class="board">
  <canvas
    oncontextmenu={(e) => e.preventDefault()}
    onpointerleave={() => (pointer = null)}
    bind:this={canvas}
    onwheel={wheel}
    onpointerdown={down}
    onpointermove={move}
    onpointerup={up}
    onpointercancel={up}
    ondblclick={refit}
  ></canvas>
  {#if mode === 'split'}
    <span class="side left">Top</span>
    <span class="side right">Bottom, seen from below</span>
    <span class="divider"></span>
  {:else if mode === 'bottom'}
    <span class="side left">Bottom, seen from below</span>
  {:else if mode === 'top'}
    <span class="side left">Top</span>
  {:else if flipped}
    <span class="side left">Flipped: seen from below</span>
  {/if}
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
  .side {
    position: absolute;
    top: 10px;
    font-size: 12px;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--muted);
    pointer-events: none;
  }
  .left {
    left: 14px;
  }
  .right {
    left: calc(50% + 14px);
  }
  .divider {
    position: absolute;
    top: 0;
    bottom: 0;
    left: 50%;
    border-left: 1px solid var(--line);
    pointer-events: none;
  }
  .error {
    position: absolute;
    inset: 40% 0 auto;
    text-align: center;
    color: var(--warn);
  }
</style>

// Draws the core's mesh: every vertex carries its segment's ends and radius, and the fragment
// shader cuts each quad down to that capsule. A negative radius is a plain fill.

const STRIDE = 11;

const VERTEX = `#version 300 es
in vec2 p; in vec2 a; in vec2 b; in float r; in vec4 c;
uniform vec2 scale; uniform vec2 offset;
out vec2 vp; flat out vec2 va; flat out vec2 vb; flat out float vr; out vec4 vc;
void main() {
  vp = p; va = a; vb = b; vr = r; vc = c;
  gl_Position = vec4(p * scale + offset, 0.0, 1.0);
}`;

const FRAGMENT = `#version 300 es
precision highp float;
in vec2 vp; flat in vec2 va; flat in vec2 vb; flat in float vr; in vec4 vc;
uniform float px;
out vec4 o;
void main() {
  float alpha = vc.a;
  if (vr >= 0.0) {
    vec2 ab = vb - va;
    float len2 = dot(ab, ab);
    float t = len2 > 0.0 ? clamp(dot(vp - va, ab) / len2, 0.0, 1.0) : 0.0;
    float d = length(vp - va - t * ab) - vr;
    alpha *= clamp(0.5 - d / px, 0.0, 1.0);
    if (alpha <= 0.0) discard;
  }
  o = vec4(vc.rgb * alpha, alpha);
}`;

function compile(gl, type, source) {
  const shader = gl.createShader(type);
  gl.shaderSource(shader, source);
  gl.compileShader(shader);
  if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) {
    throw new Error(gl.getShaderInfoLog(shader));
  }
  return shader;
}

export class Renderer {
  constructor(canvas) {
    const gl = canvas.getContext('webgl2', { antialias: false, premultipliedAlpha: true });
    if (!gl) throw new Error('This browser has no WebGL2.');
    this.gl = gl;
    const program = gl.createProgram();
    gl.attachShader(program, compile(gl, gl.VERTEX_SHADER, VERTEX));
    gl.attachShader(program, compile(gl, gl.FRAGMENT_SHADER, FRAGMENT));
    gl.linkProgram(program);
    if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
      throw new Error(gl.getProgramInfoLog(program));
    }
    this.program = program;
    this.uniforms = {
      scale: gl.getUniformLocation(program, 'scale'),
      offset: gl.getUniformLocation(program, 'offset'),
      px: gl.getUniformLocation(program, 'px'),
    };
    // One mesh per pane: side by side, the top and the bottom are drawn from different meshes.
    this.slots = [];
  }

  slot(index) {
    const gl = this.gl;
    while (this.slots.length <= index) {
      const vao = gl.createVertexArray();
      const buffer = gl.createBuffer();
      gl.bindVertexArray(vao);
      gl.bindBuffer(gl.ARRAY_BUFFER, buffer);
      const attributes = [['p', 2, 0], ['a', 2, 2], ['b', 2, 4], ['r', 1, 6], ['c', 4, 7]];
      for (const [name, size, at] of attributes) {
        const location = gl.getAttribLocation(this.program, name);
        gl.enableVertexAttribArray(location);
        gl.vertexAttribPointer(location, size, gl.FLOAT, false, STRIDE * 4, at * 4);
      }
      this.slots.push({ vao, buffer, count: 0, capacity: 0 });
    }
    return this.slots[index];
  }

  upload(index, mesh) {
    const gl = this.gl;
    const slot = this.slot(index);
    gl.bindBuffer(gl.ARRAY_BUFFER, slot.buffer);
    if (mesh.length > slot.capacity) {
      slot.capacity = Math.ceil(mesh.length * 1.5);
      gl.bufferData(gl.ARRAY_BUFFER, slot.capacity * 4, gl.DYNAMIC_DRAW);
    }
    gl.bufferSubData(gl.ARRAY_BUFFER, 0, mesh);
    slot.count = mesh.length / STRIDE;
  }

  clear(background) {
    const gl = this.gl;
    gl.disable(gl.SCISSOR_TEST);
    gl.viewport(0, 0, gl.canvas.width, gl.canvas.height);
    gl.clearColor(...background, 1);
    gl.clear(gl.COLOR_BUFFER_BIT);
  }

  // view: centre in mm and pixels per mm; rect: the pane in device pixels, from the bottom left;
  // mirror: draw it as seen from the other side of the board.
  draw(index, view, rect, mirror = false) {
    const gl = this.gl;
    const slot = this.slot(index);
    gl.enable(gl.SCISSOR_TEST);
    gl.scissor(rect.x, rect.y, rect.w, rect.h);
    gl.viewport(rect.x, rect.y, rect.w, rect.h);
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.ONE, gl.ONE_MINUS_SRC_ALPHA);
    gl.useProgram(this.program);
    const sx = ((mirror ? -2 : 2) * view.zoom) / rect.w;
    const sy = (2 * view.zoom) / rect.h;
    gl.uniform2f(this.uniforms.scale, sx, sy);
    gl.uniform2f(this.uniforms.offset, -view.cx * sx, -view.cy * sy);
    gl.uniform1f(this.uniforms.px, 1 / view.zoom);
    gl.bindVertexArray(slot.vao);
    gl.drawArrays(gl.TRIANGLES, 0, slot.count);
  }
}

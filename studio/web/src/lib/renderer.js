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
    this.vao = gl.createVertexArray();
    this.buffer = gl.createBuffer();
    gl.bindVertexArray(this.vao);
    gl.bindBuffer(gl.ARRAY_BUFFER, this.buffer);
    const attributes = [['p', 2, 0], ['a', 2, 2], ['b', 2, 4], ['r', 1, 6], ['c', 4, 7]];
    for (const [name, size, at] of attributes) {
      const location = gl.getAttribLocation(program, name);
      gl.enableVertexAttribArray(location);
      gl.vertexAttribPointer(location, size, gl.FLOAT, false, STRIDE * 4, at * 4);
    }
    this.count = 0;
    this.capacity = 0;
  }

  upload(mesh) {
    const gl = this.gl;
    gl.bindBuffer(gl.ARRAY_BUFFER, this.buffer);
    if (mesh.length > this.capacity) {
      this.capacity = Math.ceil(mesh.length * 1.5);
      gl.bufferData(gl.ARRAY_BUFFER, this.capacity * 4, gl.DYNAMIC_DRAW);
    }
    gl.bufferSubData(gl.ARRAY_BUFFER, 0, mesh);
    this.count = mesh.length / STRIDE;
  }

  // view: centre in mm and pixels per mm; the canvas is in device pixels.
  draw(view, background) {
    const gl = this.gl;
    const { width, height } = gl.canvas;
    gl.viewport(0, 0, width, height);
    gl.clearColor(...background, 1);
    gl.clear(gl.COLOR_BUFFER_BIT);
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.ONE, gl.ONE_MINUS_SRC_ALPHA);
    gl.useProgram(this.program);
    const sx = (2 * view.zoom) / width;
    const sy = (2 * view.zoom) / height;
    gl.uniform2f(this.uniforms.scale, sx, sy);
    gl.uniform2f(this.uniforms.offset, -view.cx * sx, -view.cy * sy);
    gl.uniform1f(this.uniforms.px, 1 / view.zoom);
    gl.bindVertexArray(this.vao);
    gl.drawArrays(gl.TRIANGLES, 0, this.count);
  }
}

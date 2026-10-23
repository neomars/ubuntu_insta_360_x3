// Visionneuse 360° WebGL : assemble les deux fisheye (ou lit un équirectangulaire),
// applique la stabilisation + la vue, et projette en rectiligne / fisheye / petite planète.
// Les formules sont celles de x3studio/render.py (vérifiées contre FFmpeg v360).
import { IDENTITY, mul3, transpose3, effectiveBlend, lensMatrices } from './geometry.js';

const VERT = `
attribute vec2 aPos;
varying vec2 vP;
void main() { vP = aPos; gl_Position = vec4(aPos, 0.0, 1.0); }`;

const FRAG = `
precision highp float;
varying vec2 vP;
uniform sampler2D uTex;
uniform mat3 uM, uRA, uRB;
uniform float uFov, uAspect, uLensHalf, uBlend, uSwap, uSeam, uDist;
uniform int uProj, uKind;
const float PI = 3.14159265359;

vec3 outRay() {
  vec2 u = vP;
  if (uProj == 0) {
    if (uDist < 1e-6) {
      float t = tan(uFov * 0.5);
      return normalize(vec3(u.x * t, u.y * t * uAspect, 1.0));
    }
    // perspective générale : r = (d+1) sin θ / (d + cos θ), inverse en forme close (miroir de render.rs::general_ray)
    float d = uDist;
    float lim = d >= 1.0 ? acos(-1.0 / d) : acos(-d);
    float hf = min(uFov * 0.5, lim * 0.999);
    float ge = (d + 1.0) * sin(hf) / (d + cos(hf));
    vec2 p = vec2(u.x, u.y * uAspect) * ge;
    float r = length(p);
    float rr = sqrt((d + 1.0) * (d + 1.0) + r * r);
    float th = atan(r, d + 1.0) + asin(clamp(r * d / rr, -1.0, 1.0));
    float s = r > 1e-6 ? sin(th) / r : 1.0;
    return vec3(p * s, cos(th));
  }
  if (uProj == 1) {
    vec2 a = vec2(u.x, u.y * uAspect) * uFov * 0.5;
    float th = length(a);
    float s = th > 1e-6 ? sin(th) / th : 1.0;
    return vec3(a * s, cos(th));
  }
  float kx = tan(uFov * 0.25), ky = tan(uFov * uAspect * 0.25);
  vec2 xs = vec2(u.x * kx, u.y * ky);
  float r = length(xs);
  float th = 2.0 * atan(r);
  float s = r > 1e-6 ? sin(th) / r : 2.0;
  return vec3(xs * s, cos(th));
}

vec4 lens(vec3 d, float side, out float theta) {
  float rho = length(d.xy);
  theta = atan(rho, d.z);
  float k = theta / uLensHalf;
  vec2 uv = rho > 1e-6 ? d.xy / rho * k : vec2(0.0);
  vec2 p = vec2(uv.x, -uv.y) * 0.5 + 0.5;
  p = clamp(p, 0.002, 0.998);
  return texture2D(uTex, vec2((p.x + side) * 0.5, p.y));
}

void main() {
  vec3 d = uM * outRay();
  if (uKind == 1) {
    float lon = atan(d.x, d.z);
    float lat = asin(clamp(d.y, -1.0, 1.0));
    gl_FragColor = vec4(texture2D(uTex, vec2(lon / (2.0 * PI) + 0.5, 0.5 - lat / PI)).rgb, 1.0);
    return;
  }
  float thA, thB;
  vec4 cA = lens(uRA * d, uSwap, thA);
  vec4 cB = lens(uRB * d, 1.0 - uSwap, thB);
  float lim = uLensHalf - 0.0052;
  float wa;
  if (uBlend <= 0.0) wa = thA < PI * 0.5 ? 1.0 : 0.0;
  else {
    float x = clamp((degrees(thA) - (90.0 - uBlend * 0.5)) / uBlend, 0.0, 1.0);
    wa = 1.0 - x * x * (3.0 - 2.0 * x);
  }
  float wA = wa * step(thA, lim), wB = (1.0 - wa) * step(thB, lim);
  float s = wA + wB;
  float w = s > 1e-5 ? wA / s : 0.5;
  vec3 col = mix(cB.rgb, cA.rgb, w);
  if (uSeam > 0.5) col = mix(col, vec3(1.0, 0.0, 1.0), 0.6 * (1.0 - abs(2.0 * w - 1.0)));
  gl_FragColor = vec4(col, 1.0);
}`;

const PROJ = { flat: 0, fisheye: 1, planet: 2 };

function compile(gl, type, src) {
  const sh = gl.createShader(type);
  gl.shaderSource(sh, src);
  gl.compileShader(sh);
  if (!gl.getShaderParameter(sh, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(sh));
  return sh;
}

export class Viewer {
  constructor(canvas) {
    this.canvas = canvas;
    const gl = canvas.getContext('webgl', { antialias: false, preserveDrawingBuffer: true });
    if (!gl) throw new Error('WebGL indisponible');
    this.gl = gl;
    const prog = gl.createProgram();
    gl.attachShader(prog, compile(gl, gl.VERTEX_SHADER, VERT));
    gl.attachShader(prog, compile(gl, gl.FRAGMENT_SHADER, FRAG));
    gl.linkProgram(prog);
    if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(prog));
    gl.useProgram(prog);
    this.prog = prog;
    this.u = {};
    for (const n of ['uTex', 'uM', 'uRA', 'uRB', 'uFov', 'uAspect', 'uLensHalf', 'uBlend', 'uSwap', 'uSeam', 'uDist', 'uProj', 'uKind'])
      this.u[n] = gl.getUniformLocation(prog, n);
    const buf = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, buf);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]), gl.STATIC_DRAW);
    const loc = gl.getAttribLocation(prog, 'aPos');
    gl.enableVertexAttribArray(loc);
    gl.vertexAttribPointer(loc, 2, gl.FLOAT, false, 0, 0);
    this.tex = gl.createTexture();
    gl.bindTexture(gl.TEXTURE_2D, this.tex);
    for (const [k, v] of [[gl.TEXTURE_MIN_FILTER, gl.LINEAR], [gl.TEXTURE_MAG_FILTER, gl.LINEAR],
      [gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE], [gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE]]) gl.texParameteri(gl.TEXTURE_2D, k, v);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, 1, 1, 0, gl.RGBA, gl.UNSIGNED_BYTE, new Uint8Array([20, 22, 28, 255]));
    this.video = null;
    this.kind = 'dual';
    this.calib = { fov: 200, blend: 10, swap_lenses: false, a_yaw: 0, a_pitch: 0, a_roll: 0, b_yaw: 0, b_pitch: 0, b_roll: 0 };
    this.projection = 'flat';
    this.view = { yaw: 0, pitch: 0, roll: 0, fov: 90, distortion: 0 };
    this.stab = IDENTITY;       // matrice courante (sortie -> sphère de la caméra)
    this.seam = false;
    this.hasFrame = false;
  }

  setVideo(video, kind) { this.video = video; this.kind = kind; this.hasFrame = false; }
  setCalibration(c) { this.calib = c; }
  setProjection(p) { this.projection = p; }
  setView(v) { this.view = v; }
  setStabMatrix(m) { this.stab = m; }

  uploadFrame() {
    const v = this.video;
    if (!v || v.readyState < 2) return false;
    const gl = this.gl;
    gl.bindTexture(gl.TEXTURE_2D, this.tex);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, v);
    this.hasFrame = true;
    return true;
  }

  resize() {
    const c = this.canvas, dpr = Math.min(window.devicePixelRatio || 1, 2);
    const w = Math.max(2, Math.round(c.clientWidth * dpr)), h = Math.max(2, Math.round(c.clientHeight * dpr));
    if (c.width !== w || c.height !== h) { c.width = w; c.height = h; }
  }

  // matrice de vue : sortie -> sphère = stab · V(yaw,pitch,roll) (voir geometry.py)
  draw(viewMatrix) {
    const gl = this.gl, u = this.u, c = this.calib;
    this.resize();
    gl.viewport(0, 0, this.canvas.width, this.canvas.height);
    const M = mul3(this.stab, viewMatrix);
    const [ra, rb] = lensMatrices(c);
    const col = (m) => new Float32Array(transpose3(m));   // GLSL attend du colonne-majeur
    gl.useProgram(this.prog);
    gl.bindTexture(gl.TEXTURE_2D, this.tex);
    gl.uniform1i(u.uTex, 0);
    gl.uniformMatrix3fv(u.uM, false, col(M));
    gl.uniformMatrix3fv(u.uRA, false, col(ra));
    gl.uniformMatrix3fv(u.uRB, false, col(rb));
    gl.uniform1f(u.uFov, this.view.fov * Math.PI / 180);
    gl.uniform1f(u.uAspect, this.canvas.height / this.canvas.width);
    gl.uniform1f(u.uLensHalf, c.fov * Math.PI / 360);
    gl.uniform1f(u.uBlend, effectiveBlend(c));
    gl.uniform1f(u.uSwap, c.swap_lenses ? 1 : 0);
    gl.uniform1f(u.uSeam, this.seam ? 1 : 0);
    gl.uniform1f(u.uDist, this.projection === 'flat' ? Math.min(2, Math.max(0, this.view.distortion || 0)) : 0);
    gl.uniform1i(u.uProj, PROJ[this.projection] ?? 0);
    gl.uniform1i(u.uKind, this.kind === 'equirect' ? 1 : 0);
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
  }
}

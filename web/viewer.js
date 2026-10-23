// Visionneuse 360° WebGL : assemble les deux fisheye (ou lit un équirectangulaire),
// applique la stabilisation + la vue, et projette en rectiligne / fisheye / petite planète.
// Les formules sont celles de x3studio/render.py (vérifiées contre FFmpeg v360).
import { IDENTITY, GRADE_DEFAULT, STICK_DEFAULT, mul3, transpose3, effectiveBlend, lensMatrices, gradeChromaShift, gradeColorIsNeutral, stickUniforms } from './geometry.js';

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
uniform vec4 uGA;     // réglages d'image : luminosité, contraste, 1/gamma, saturation
uniform vec2 uGB;     // décalage de chrominance (Cb, Cr) : chaleur + teinte
uniform float uGOn;
uniform vec4 uStA;    // perche : axe (repère de la caméra) et mode (0 aucun, 1 miroir, 2 disque)
uniform vec4 uStB;    // rayon, rayon opaque (rad), afficher la zone
uniform vec3 uStP;    // vecteur orthogonal à l'axe
uniform vec3 uStC;    // couleur du disque
const float PI = 3.14159265359;

// Perche à selfie / nadir : miroir de src/stick.rs (d : direction dans le repère de la caméra).
vec3 stickMap(vec3 d, out float cover) {
  cover = 0.0;
  if (uStA.w < 0.5) return d;
  float c = clamp(dot(d, uStA.xyz), -1.0, 1.0);
  float a = acos(c);
  if (uStA.w < 1.5) {
    if (a >= uStB.x) return d;
    vec3 e = d - c * uStA.xyz;
    float s = length(e);
    vec3 u = s > 1e-9 ? e / s : uStP;
    float a2 = 2.0 * uStB.x - a;
    return cos(a2) * uStA.xyz + sin(a2) * u;
  }
  float t = clamp((a - uStB.y) / (uStB.x - uStB.y), 0.0, 1.0);
  cover = 1.0 - t * t * (3.0 - 2.0 * t);
  return d;
}

// Réglages d'image dans l'espace Y'CbCr BT.709 : miroir de src/grade.rs.
vec3 grade(vec3 c) {
  float y = dot(c, vec3(0.2126, 0.7152, 0.0722));
  float cb = (c.b - y) / 1.8556, cr = (c.r - y) / 1.5748;
  y = pow(max(clamp(y, 0.0, 1.0), 1e-6), uGA.z);
  y = clamp((y - 0.5) * uGA.y + 0.5 + uGA.x, 0.0, 1.0);
  cb = clamp(cb * uGA.w + uGB.x, -0.5, 0.5);
  cr = clamp(cr * uGA.w + uGB.y, -0.5, 0.5);
  return clamp(vec3(y + 1.5748 * cr, y - 0.187324 * cb - 0.468124 * cr, y + 1.8556 * cb), 0.0, 1.0);
}

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

// Zone de la perche surlignée dans l'aperçu (jamais exportée).
vec3 stickShow(vec3 col, vec3 d0) {
  if (uStB.z < 0.5 || uStA.w < 0.5) return col;
  float a = acos(clamp(dot(d0, uStA.xyz), -1.0, 1.0));
  return a < uStB.x ? mix(col, vec3(1.0, 0.85, 0.0), 0.4) : col;
}

void main() {
  vec3 d0 = uM * outRay();
  float cover;
  vec3 d = stickMap(d0, cover);
  if (uKind == 1) {
    float lon = atan(d.x, d.z);
    float lat = asin(clamp(d.y, -1.0, 1.0));
    vec3 c = texture2D(uTex, vec2(lon / (2.0 * PI) + 0.5, 0.5 - lat / PI)).rgb;
    c = mix(c, uStC, cover);
    gl_FragColor = vec4(stickShow(uGOn > 0.5 ? grade(c) : c, d0), 1.0);
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
  vec3 col = mix(mix(cB.rgb, cA.rgb, w), uStC, cover);
  if (uGOn > 0.5) col = grade(col);
  col = stickShow(col, d0);
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
    for (const n of ['uTex', 'uM', 'uRA', 'uRB', 'uFov', 'uAspect', 'uLensHalf', 'uBlend', 'uSwap', 'uSeam', 'uDist', 'uProj', 'uKind', 'uGA', 'uGB', 'uGOn', 'uStA', 'uStB', 'uStP', 'uStC'])
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
    this.grade = { ...GRADE_DEFAULT };
    this.stick = { ...STICK_DEFAULT };
    this.showStick = false;     // surligne la zone de la perche (aperçu seulement)
    this.hasFrame = false;
  }

  setVideo(video, kind) { this.video = video; this.kind = kind; this.hasFrame = false; }
  setCalibration(c) { this.calib = c; }
  setProjection(p) { this.projection = p; }
  setView(v) { this.view = v; }
  setStabMatrix(m) { this.stab = m; }
  setGrade(g) { this.grade = { ...GRADE_DEFAULT, ...g }; }
  setStick(s) { this.stick = { ...STICK_DEFAULT, ...s }; }

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
    const g = this.grade, shift = gradeChromaShift(g);
    gl.uniform4f(u.uGA, g.brightness, g.contrast, 1 / g.gamma, g.saturation);
    gl.uniform2f(u.uGB, shift[0], shift[1]);
    gl.uniform1f(u.uGOn, gradeColorIsNeutral(g) ? 0 : 1);
    const st = stickUniforms(this.stick);
    gl.uniform4f(u.uStA, st.axis[0], st.axis[1], st.axis[2], st.mode);
    gl.uniform4f(u.uStB, st.rho, st.rhoIn, this.showStick ? 1 : 0, 0);
    gl.uniform3f(u.uStP, st.perp[0], st.perp[1], st.perp[2]);
    gl.uniform3f(u.uStC, st.rgb[0], st.rgb[1], st.rgb[2]);
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
  }
}

// Géométrie : miroir de x3studio/geometry.py (mêmes conventions que le filtre v360 de FFmpeg).
// Matrices 3x3 stockées par lignes dans des tableaux de 9 nombres.

const D2R = Math.PI / 180;

export function mul3(a, b) {
  const o = new Array(9);
  for (let i = 0; i < 3; i++)
    for (let j = 0; j < 3; j++)
      o[i * 3 + j] = a[i * 3] * b[j] + a[i * 3 + 1] * b[3 + j] + a[i * 3 + 2] * b[6 + j];
  return o;
}

export function transpose3(a) {
  return [a[0], a[3], a[6], a[1], a[4], a[7], a[2], a[5], a[8]];
}

export const IDENTITY = [1, 0, 0, 0, 1, 0, 0, 0, 1];

export function rotYaw(a) { const c = Math.cos(a), s = Math.sin(a); return [c, 0, s, 0, 1, 0, -s, 0, c]; }
export function rotPitch(a) { const c = Math.cos(a), s = Math.sin(a); return [1, 0, 0, 0, c, s, 0, -s, c]; }
export function rotRoll(a) { const c = Math.cos(a), s = Math.sin(a); return [c, s, 0, -s, c, 0, 0, 0, 1]; }

// Angles en degrés -> matrice (sortie -> sphère), ordre ypr.
export function yprToMatrix(yaw, pitch, roll) {
  return mul3(mul3(rotYaw(yaw * D2R), rotPitch(pitch * D2R)), rotRoll(roll * D2R));
}

export function wrap180(a) { return ((a + 180) % 360 + 360) % 360 - 180; }

const smooth = (x) => { x = Math.min(1, Math.max(0, x)); return x * x * (3 - 2 * x); };

export const VIEW_FIELDS = ['yaw', 'pitch', 'roll', 'fov', 'distortion'];

// Vue (yaw, pitch, roll, fov) à l'instant t : interpolation douce entre keyframes.
export function interpolateView(keyframes, t, base) {
  const def = Object.assign({ yaw: 0, pitch: 0, roll: 0, fov: 90, distortion: 0 }, base || {});
  if (!keyframes.length) return def;
  const k = [...keyframes].sort((a, b) => a.t - b.t);
  const pick = (kf) => { const o = {}; for (const f of VIEW_FIELDS) o[f] = kf[f] ?? def[f]; return o; };
  if (t <= k[0].t || k.length === 1) return pick(k[0]);
  if (t >= k[k.length - 1].t) return pick(k[k.length - 1]);
  for (let i = 0; i < k.length - 1; i++) {
    const a = k[i], b = k[i + 1];
    if (a.t <= t && t <= b.t) {
      const span = b.t - a.t;
      const u = span > 0 ? smooth((t - a.t) / span) : 1;
      const o = {};
      for (const f of VIEW_FIELDS) {
        const va = a[f] ?? def[f], vb = b[f] ?? def[f];
        o[f] = f === 'yaw' ? va + wrap180(vb - va) * u : va + (vb - va) * u;
      }
      return o;
    }
  }
  return def;
}

// Matrices (sphère -> objectif) de A et B, comme Calibration.lens_matrices().
export function lensMatrices(c) {
  const ra = yprToMatrix(c.a_yaw, c.a_pitch, c.a_roll);
  const rb = mul3(yprToMatrix(c.b_yaw, c.b_pitch, c.b_roll), rotYaw(Math.PI));
  return [ra, rb];
}

export function effectiveBlend(c) { return Math.max(0, Math.min(c.blend, c.fov - 180)); }

export const DEFAULT_FOV = { flat: 90, fisheye: 180, planet: 250 };
export const FOV_RANGE = { flat: [20, 150], fisheye: [60, 300], planet: [120, 340] };
export const MAX_DISTORTION = 2;

// Demi-angle maximal (rad) de la perspective générale de distorsion d (miroir de render.rs::perspective_limit).
export function perspectiveLimit(d) { return d >= 1 ? Math.acos(-1 / d) : Math.acos(-Math.max(0, d)); }

// Champ de vision permis (degrés) : la perspective plate s'ouvre dès que la distorsion est non nulle.
export function fovRange(projection, distortion = 0) {
  const [lo, hi] = FOV_RANGE[projection];
  if (projection !== 'flat' || distortion <= 1e-6) return [lo, hi];
  return [lo, Math.max(hi, Math.min(340, 2 * perspectiveLimit(distortion) * 180 / Math.PI * 0.999))];
}

// Réglages d'image : miroir de src/grade.rs (mêmes bornes, mêmes décalages de chrominance).
export const GRADE_DEFAULT = { brightness: 0, contrast: 1, saturation: 1, gamma: 1, warmth: 0, tint: 0, sharpen: 0, denoise: 0 };
export const GRADE_RANGE = {
  brightness: [-0.5, 0.5], contrast: [0.5, 2], saturation: [0, 2], gamma: [0.5, 2],
  warmth: [-1, 1], tint: [-1, 1], sharpen: [0, 1], denoise: [0, 1],
};
const WARMTH_SHIFT = [-0.08, 0.07], TINT_SHIFT = [0.051, 0.06];

export function gradeWithDefaults(g) { return Object.assign({}, GRADE_DEFAULT, g || {}); }

// Décalage de chrominance (Cb, Cr) dû à la chaleur et à la teinte.
export function gradeChromaShift(g) {
  return [WARMTH_SHIFT[0] * g.warmth + TINT_SHIFT[0] * g.tint, WARMTH_SHIFT[1] * g.warmth + TINT_SHIFT[1] * g.tint];
}

// Vrai si les réglages de couleur ne changent rien (netteté et bruit n'agissent qu'à l'export).
export function gradeColorIsNeutral(g) {
  return ['brightness', 'contrast', 'saturation', 'gamma', 'warmth', 'tint'].every((k) => g[k] === GRADE_DEFAULT[k]);
}

// Plusieurs perspectives : miroir de src/multi.rs (mêmes fractions, mêmes arrondis à l'entier pair).
export const LAYOUT_TILES = { single: 1, side: 2, stack: 2, pip: 2, trio: 3, quad: 4 };
const LAYOUT_FRACTIONS = {
  single: [[0, 0, 1, 1]],
  side: [[0, 0, 0.5, 1], [0.5, 0, 1, 1]],
  stack: [[0, 0, 1, 0.5], [0, 0.5, 1, 1]],
  pip: [[0, 0, 1, 1], [0.67, 0.67, 0.97, 0.97]],
  trio: [[0, 0, 2 / 3, 1], [2 / 3, 0, 1, 0.5], [2 / 3, 0.5, 1, 1]],
  quad: [[0, 0, 0.5, 0.5], [0.5, 0, 1, 0.5], [0, 0.5, 0.5, 1], [0.5, 0.5, 1, 1]],
};
const evenPx = (v) => Math.max(0, Math.round(v / 2) * 2);

// Rectangles {x, y, w, h} des vues d'une image w x h ; `gap` (multiple de 4) est pris sur les bords communs.
export function layoutRects(layout, w, h, gap = 0) {
  const inset = layout === 'pip' ? 0 : gap / 2;
  return (LAYOUT_FRACTIONS[layout] || LAYOUT_FRACTIONS.single).map((f) => {
    let x0 = evenPx(f[0] * w), x1 = evenPx(f[2] * w), y0 = evenPx(f[1] * h), y1 = evenPx(f[3] * h);
    if (f[0] > 0) x0 += inset;
    if (f[2] < 1) x1 -= inset;
    if (f[1] > 0) y0 += inset;
    if (f[3] < 1) y1 -= inset;
    return { x: x0, y: y0, w: x1 - x0, h: y1 - y0 };
  });
}

export const MULTI_DEFAULT = { layout: 'single', gap: 0, views: [] };

// Vue par défaut de la vue n° `index` (1 = la deuxième) d'une disposition de `count` vues (miroir de Multi::default_tile).
export function defaultTileView(index, count) {
  return { projection: 'flat', view: { yaw: wrap180(360 * index / Math.max(1, count)), pitch: 0, roll: 0, fov: 90, distortion: 0 }, keyframes: [] };
}

// Réglages « multi » complétés : toutes les vues de la disposition existent.
export function multiWithDefaults(m) {
  const out = Object.assign({}, MULTI_DEFAULT, m || {});
  out.views = (out.views || []).map((v) => Object.assign(defaultTileView(1, 2), v, { view: Object.assign({ yaw: 0, pitch: 0, roll: 0, fov: 90, distortion: 0 }, v.view || {}), keyframes: v.keyframes || [] }));
  const n = LAYOUT_TILES[out.layout] || 1;
  for (let i = out.views.length + 1; i < n; i++) out.views.push(defaultTileView(i, n));
  return out;
}

// Perche à selfie / nadir : miroir de src/stick.rs.
export const STICK_DEFAULT = { mode: 'off', radius: 20, softness: 0.3, yaw: 0, pitch: -90, color: '#202020' };
export const STICK_MODES = { off: 0, mirror: 1, cover: 2 };
const STICK_MIN_SOFTNESS = 0.02;

export function stickWithDefaults(st) { return Object.assign({}, STICK_DEFAULT, st || {}); }

// Valeurs des uniformes du shader : axe (repère de la caméra), vecteur orthogonal, rayons (rad), couleur RVB 0..1.
export function stickUniforms(st) {
  const yaw = st.yaw * D2R, pitch = st.pitch * D2R;
  const axis = [Math.cos(pitch) * Math.sin(yaw), Math.sin(pitch), Math.cos(pitch) * Math.cos(yaw)];
  const helper = Math.abs(axis[0]) < 0.9 ? [1, 0, 0] : [0, 0, 1];
  const c = [axis[1] * helper[2] - axis[2] * helper[1], axis[2] * helper[0] - axis[0] * helper[2], axis[0] * helper[1] - axis[1] * helper[0]];
  const n = Math.hypot(c[0], c[1], c[2]);
  const rho = Math.min(60, Math.max(1, st.radius)) * D2R;
  const soft = Math.min(1, Math.max(STICK_MIN_SOFTNESS, st.softness));
  const hex = /^#[0-9a-fA-F]{6}$/.test(st.color) ? st.color : STICK_DEFAULT.color;
  const rgb = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255);
  return { axis, perp: c.map((v) => v / n), mode: STICK_MODES[st.mode] ?? 0, rho, rhoIn: rho * (1 - soft), rgb };
}

// Réglages d'image appliqués à une couleur RVB 0..1 (miroir du shader) : le fond des écarts entre les vues.
export function gradeColor(g, rgb) {
  const [r, gr, b] = rgb;
  let y = 0.2126 * r + 0.7152 * gr + 0.0722 * b;
  let cb = (b - y) / 1.8556, cr = (r - y) / 1.5748;
  const [sb, sr] = gradeChromaShift(g);
  y = Math.min(1, Math.max(0, (Math.pow(Math.min(1, Math.max(0, y)), 1 / g.gamma) - 0.5) * g.contrast + 0.5 + g.brightness));
  cb = Math.min(0.5, Math.max(-0.5, cb * g.saturation + sb));
  cr = Math.min(0.5, Math.max(-0.5, cr * g.saturation + sr));
  return [y + 1.5748 * cr, y - 0.187324 * cb - 0.468124 * cr, y + 1.8556 * cb].map((v) => Math.min(1, Math.max(0, v)));
}

export function fmtTime(t) {
  if (!isFinite(t)) return '0:00.0';
  const m = Math.floor(t / 60), s = t - m * 60;
  return `${m}:${s < 10 ? '0' : ''}${s.toFixed(1)}`;
}

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

export function fmtTime(t) {
  if (!isFinite(t)) return '0:00.0';
  const m = Math.floor(t / 60), s = t - m * 60;
  return `${m}:${s < 10 ? '0' : ''}${s.toFixed(1)}`;
}

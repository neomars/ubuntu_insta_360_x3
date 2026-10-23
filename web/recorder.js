// Enregistrement du cadrage pendant la lecture : la vue est échantillonnée à chaque image affichée, puis le tracé est
// simplifié en quelques keyframes (algorithme de Douglas-Peucker sur toutes les grandeurs à la fois) qui le suivent à
// une tolérance près. Les keyframes enregistrés s'interpolent linéairement : ils suivent déjà le mouvement de la main.
import { wrap180 } from './geometry.js';

const FIELDS = ['yaw', 'pitch', 'roll', 'fov', 'distortion'];
const ANGLES = ['yaw', 'roll'];            // grandeurs cycliques (360°)

// Écart toléré entre le tracé simplifié et le tracé enregistré (degrés ; distorsion sans unité).
export const REC_TOLERANCE = { yaw: 0.8, pitch: 0.8, roll: 0.8, fov: 1.5, distortion: 0.03 };
// Deux keyframes consécutifs ne doivent pas être à plus de 150° l'un de l'autre en cap ou en roulis : l'interpolation
// prend le chemin le plus court, un grand tour doit donc être découpé.
const MAX_STEP = 150;

// Suite continue (sans saut de 360°) d'une grandeur cyclique.
function unwrap(values) {
  const out = [];
  values.forEach((v, i) => out.push(i === 0 ? v : out[i - 1] + wrap180(v - out[i - 1])));
  return out;
}

// Moyenne mobile triangulaire (1, 2, 3, 2, 1) : gomme les à-coups de la main. Aux extrémités, le tracé est prolongé par
// symétrie impaire autour du point d'extrémité : une droite reste une droite et les extrémités ne bougent pas.
function smooth(values) {
  const w = [1, 2, 3, 2, 1], n = values.length;
  const at = (j) => (j < 0 ? 2 * values[0] - values[Math.min(n - 1, -j)] : j >= n ? 2 * values[n - 1] - values[Math.max(0, 2 * (n - 1) - j)] : values[j]);
  return values.map((_, i) => {
    let sum = 0;
    for (let k = -2; k <= 2; k++) sum += at(i + k) * w[k + 2];
    return sum / 9;
  });
}

/**
 * Keyframes qui suivent l'échantillonnage `samples` ([{ t, yaw, pitch, roll, fov, distortion }], `t` croissant) à
 * `tolerance` près. Renvoie [] pour moins de deux échantillons. Chaque keyframe porte `ease: 'linear'`.
 */
export function simplifySamples(samples, tolerance = REC_TOLERANCE) {
  const pts = [];
  for (const s of samples) if (!pts.length || s.t > pts[pts.length - 1].t + 1e-6) pts.push(s);
  const n = pts.length;
  if (n < 2) return [];
  const cols = {};
  for (const f of FIELDS) {
    const raw = pts.map((p) => p[f] ?? 0);
    cols[f] = smooth(ANGLES.includes(f) ? unwrap(raw) : raw);
  }
  const t = pts.map((p) => p.t);
  // écart (en tolérances) du point k à la corde i -> j
  const error = (i, j, k) => {
    const u = (t[k] - t[i]) / (t[j] - t[i]);
    let worst = 0;
    for (const f of FIELDS) worst = Math.max(worst, Math.abs(cols[f][k] - (cols[f][i] + (cols[f][j] - cols[f][i]) * u)) / tolerance[f]);
    return worst;
  };
  const keep = new Set([0, n - 1]);
  const stack = [[0, n - 1]];
  while (stack.length) {
    const [i, j] = stack.pop();
    if (j - i < 2) continue;
    let best = -1, worst = -1;
    for (let k = i + 1; k < j; k++) {
      const e = error(i, j, k);
      if (e > worst) { worst = e; best = k; }
    }
    const tooFar = ANGLES.some((f) => Math.abs(cols[f][j] - cols[f][i]) > MAX_STEP);
    if (worst > 1 || tooFar) {
      keep.add(best);
      stack.push([i, best], [best, j]);
    }
  }
  const round = (v) => Math.round(v * 1000) / 1000;
  return [...keep].sort((a, b) => a - b).map((k) => ({
    t: round(t[k]),
    yaw: round(wrap180(cols.yaw[k])), pitch: round(cols.pitch[k]), roll: round(wrap180(cols.roll[k])),
    fov: round(cols.fov[k]), distortion: round(cols.distortion[k]),
    ease: 'linear',
  }));
}

/**
 * Remplace les keyframes de l'intervalle [t0, t1] par `recorded` (ceux d'avant et d'après sont conservés), triés.
 */
export function replaceKeyframes(existing, recorded, t0, t1) {
  const eps = 1e-3;
  const kept = existing.filter((k) => k.t < t0 - eps || k.t > t1 + eps);
  return [...kept, ...recorded].sort((a, b) => a.t - b.t);
}

// Enregistrement du cadrage : simplification du tracé échantillonné en keyframes (sans navigateur).
import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import { interpolateView, wrap180 } from '../../web/geometry.js';
import { REC_TOLERANCE, replaceKeyframes, simplifySamples } from '../../web/recorder.js';

/** Échantillons à 60 Hz d'une fonction du temps. */
const sample = (seconds, fn, hz = 60) => Array.from({ length: Math.round(seconds * hz) + 1 }, (_, i) => {
  const t = i / hz;
  return { t, yaw: 0, pitch: 0, roll: 0, fov: 90, distortion: 0, ...fn(t) };
});

/** Plus grand écart (en unités de tolérance) entre les keyframes et le tracé d'origine. */
function worstError(samples, keyframes) {
  let worst = 0;
  for (const s of samples) {
    const v = interpolateView(keyframes, s.t, {});
    for (const f of Object.keys(REC_TOLERANCE)) {
      const d = f === 'yaw' || f === 'roll' ? Math.abs(wrap180(v[f] - s[f])) : Math.abs(v[f] - s[f]);
      worst = Math.max(worst, d / REC_TOLERANCE[f]);
    }
  }
  return worst;
}

describe("enregistrement du cadrage", () => {
  it('un mouvement à vitesse constante tient en deux keyframes', () => {
    const s = sample(2, (t) => ({ yaw: 10 + 20 * t }));
    const k = simplifySamples(s);
    assert.equal(k.length, 2);
    assert.ok(k.every((x) => x.ease === 'linear'));
    assert.deepEqual([k[0].t, k[1].t], [0, 2]);
    assert.ok(Math.abs(k[0].yaw - 10) < 1e-9 && Math.abs(k[1].yaw - 50) < 1e-9, JSON.stringify(k));
    assert.ok(worstError(s, k) < 0.1);
  });

  it('une vue immobile donne deux keyframes identiques, sans bruit de tracé', () => {
    const k = simplifySamples(sample(1.5, () => ({ yaw: 33, pitch: -12, fov: 70 })));
    assert.equal(k.length, 2);
    assert.deepEqual([k[0].yaw, k[0].pitch, k[0].fov], [33, -12, 70]);
  });

  it('une courbe douce reste fidèle avec peu de keyframes', () => {
    const smooth = (x) => x * x * (3 - 2 * x);
    const s = sample(6, (t) => ({
      yaw: -40 + 120 * smooth(Math.min(1, Math.max(0, (t - 1) / 3))),
      pitch: 15 * Math.sin(t / 6 * Math.PI),
      fov: 90 - 25 * smooth(Math.min(1, t / 6)),
    }));
    const k = simplifySamples(s);
    assert.ok(k.length >= 5 && k.length <= 24, `${k.length} keyframes`);
    assert.ok(worstError(s, k) < 1.6, `écart ${worstError(s, k).toFixed(2)} tolérances`);
    assert.ok(k.every((x, i) => i === 0 || x.t > k[i - 1].t), 'temps strictement croissants');
  });

  it('le bruit de la main est gommé', () => {
    let seed = 7;
    const rnd = () => { seed = (seed * 1664525 + 1013904223) >>> 0; return seed / 2 ** 32 - 0.5; };
    const s = sample(3, (t) => ({ yaw: 30 * t + 0.5 * rnd(), pitch: 0.5 * rnd() }));
    const k = simplifySamples(s);
    assert.ok(k.length <= 6, `${k.length} keyframes pour une panoramique régulière bruitée`);
    // le tracé obtenu reste sur la droite idéale à une tolérance près
    for (const x of s) assert.ok(Math.abs(interpolateView(k, x.t, {}).yaw - 30 * x.t) < 1.0);
  });

  it('un passage par ±180° suit le chemin le plus court', () => {
    const s = sample(2, (t) => ({ yaw: wrap180(160 + 30 * t) }));   // 160° -> 220° = -140°
    const k = simplifySamples(s);
    assert.equal(k.length, 2);
    assert.ok(worstError(s, k) < 0.1);
    assert.ok(Math.abs(wrap180(interpolateView(k, 1, {}).yaw + 170)) < 0.7, 'à 1 s, 160 + 30 = 190° = -170°');
  });

  it('un grand tour est découpé : jamais plus de 150° entre deux keyframes', () => {
    const s = sample(4, (t) => ({ yaw: wrap180(100 * t), roll: wrap180(-80 * t) }));   // 400° de cap, -320° de roulis
    const k = simplifySamples(s);
    assert.ok(k.length >= 4, `${k.length} keyframes`);
    for (let i = 1; i < k.length; i++) {
      assert.ok(Math.abs(wrap180(k[i].yaw - k[i - 1].yaw)) <= 150 && Math.abs(wrap180(k[i].roll - k[i - 1].roll)) <= 150);
    }
    assert.ok(worstError(s, k) < 1.5, `écart ${worstError(s, k).toFixed(2)}`);
  });

  it('champ et distorsion sont suivis comme le cap', () => {
    const s = sample(2, (t) => ({ fov: 90 + 40 * t, distortion: Math.min(1, t) }));
    const k = simplifySamples(s);
    assert.ok(k.length >= 3 && k.length <= 5, `${k.length} keyframes (un coude à 1 s)`);
    assert.ok(worstError(s, k) < 1.2);
    assert.ok(Math.abs(k[k.length - 1].fov - 170) < 1e-6 && Math.abs(k[k.length - 1].distortion - 1) < 1e-6);
  });

  it('trop peu d\'échantillons : rien à enregistrer', () => {
    assert.deepEqual(simplifySamples([]), []);
    assert.deepEqual(simplifySamples([{ t: 1, yaw: 0, pitch: 0, roll: 0, fov: 90, distortion: 0 }]), []);
    // des échantillons au même instant comptent pour un seul
    const dup = [{ t: 1, yaw: 0 }, { t: 1, yaw: 5 }];
    assert.deepEqual(simplifySamples(dup), []);
  });

  it("remplace les keyframes de l'intervalle enregistré et garde les autres", () => {
    const old = [0, 1, 2, 3, 4, 5].map((t) => ({ t, yaw: t * 10 }));
    const rec = [{ t: 1.5, yaw: 1 }, { t: 3.2, yaw: 2 }];
    const out = replaceKeyframes(old, rec, 1.5, 3.2);
    assert.deepEqual(out.map((k) => k.t), [0, 1, 1.5, 3.2, 4, 5]);
    assert.deepEqual(out.filter((k) => k.t === 1.5 || k.t === 3.2).map((k) => k.yaw), [1, 2]);
    // un keyframe pile au début ou à la fin de l'intervalle est remplacé
    const out2 = replaceKeyframes(old, rec, 1, 3);
    assert.deepEqual(out2.map((k) => k.t), [0, 1.5, 3.2, 4, 5]);
  });
});

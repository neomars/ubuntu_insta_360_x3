// Aides communes aux tests de l'interface : serveur x3studio réel, vidéo de démonstration, Chromium sans écran.
import { execFileSync, spawn } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { chromium } from 'playwright';

export const BIN = process.env.X3STUDIO_BIN || path.resolve(import.meta.dirname, '../../target/debug/x3studio');

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/**
 * Attend qu'une fonction évaluée dans la page renvoie une valeur vraie. Interrogation par `page.evaluate` (protocole
 * du navigateur) plutôt que `page.waitForFunction`, qui évalue du texte dans la page : la politique de sécurité de
 * l'interface (`script-src 'self'`, sans `unsafe-eval`) l'interdit, et les tests doivent la laisser active.
 */
export async function waitFor(page, fn, { timeout = 30000, interval = 100, arg } = {}) {
  const t0 = Date.now();
  for (;;) {
    const value = await page.evaluate(fn, arg);
    if (value) return value;
    if (Date.now() - t0 > timeout) throw new Error(`délai dépassé en attendant : ${fn}`);
    await sleep(interval);
  }
}

/** Vidéo `.insv` synthétique (deux objectifs fisheye, caméra qui tremble) générée par `x3studio demo`. */
export function makeDemo(file, { seconds = 2.5, lens = 480, motion = 'shake' } = {}) {
  execFileSync(BIN, ['demo', file, '--seconds', String(seconds), '--lens', String(lens), '--motion', motion], { stdio: 'pipe' });
  return file;
}

/** Lance `x3studio gui --no-browser` avec des dossiers temporaires et attend l'adresse annoncée. */
export async function startApp() {
  const root = mkdtempSync(path.join(tmpdir(), 'x3ui-'));
  const dirs = { root, home: path.join(root, 'home'), out: path.join(root, 'sorties'), media: path.join(root, 'médias') };
  const child = spawn(BIN, ['gui', '--no-browser'], {
    env: { ...process.env, X3STUDIO_HOME: dirs.home, X3STUDIO_OUTPUT: dirs.out },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let log = '';
  const url = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`le serveur n'a pas démarré :\n${log}`)), 30000);
    const onData = (d) => {
      log += d;
      const m = log.match(/http:\/\/127\.0\.0\.1:(\d+)\/\?key=([0-9a-f]+)/);
      if (m) {
        clearTimeout(timer);
        resolve(m[0]);
      }
    };
    child.stdout.on('data', onData);
    child.stderr.on('data', (d) => { log += d; });
    child.on('exit', (code) => reject(new Error(`le serveur s'est arrêté (code ${code}) :\n${log}`)));
  });
  const port = Number(new URL(url).port);
  return {
    url, port, ...dirs,
    async stop() {
      const exited = new Promise((r) => child.once('exit', r));
      child.kill('SIGINT');
      await Promise.race([exited, sleep(5000)]);
      child.kill('SIGKILL');
      rmSync(root, { recursive: true, force: true });
    },
  };
}

export async function launchBrowser() {
  return chromium.launch({
    executablePath: process.env.X3_CHROMIUM || undefined,
    args: ['--no-sandbox', '--use-gl=angle', '--use-angle=swiftshader', '--enable-unsafe-swiftshader', '--ignore-gpu-blocklist',
      '--autoplay-policy=no-user-gesture-required'],
  });
}

/** Page ouverte sur l'interface avec la clé de lancement ; collecte erreurs et requêtes. */
export async function openUi(browser, app) {
  const page = await browser.newPage({ viewport: { width: 1300, height: 820 } });
  page.errors = [];
  page.requests = [];
  page.on('pageerror', (e) => page.errors.push(String(e)));
  page.on('console', (m) => { if (m.type() === 'error') page.errors.push(`console : ${m.text()}`); });
  page.on('request', (r) => page.requests.push(r.url()));
  await page.goto(app.url);
  await page.waitForSelector('#btn-open');
  return page;
}

/** Ouvre un fichier par la boîte de dialogue de l'interface, puis attend aperçu vidéo et analyse du mouvement. */
export async function openAndWait(page, file) {
  await page.click('#btn-open');
  await page.waitForSelector('#fs-list li');
  await page.fill('#fs-path', path.dirname(file));
  await page.click('#fs-go');
  await page.click(`#fs-list li:has-text("${path.basename(file)}")`);
  await waitFor(page, () => window.__x3.S.videoReady === true, { timeout: 120000 });
  for (let i = 0; i < 300; i++) {
    const ready = await page.evaluate("window.__x3.api('GET', '/api/project/' + window.__x3.S.proj.id).then(p => p.analysis.ready)");
    if (ready) break;
    await sleep(200);
  }
  await sleep(600);
}

/** Attend que la vidéo ait fini de se positionner (pas de saut en cours, image décodée) puis que l'aperçu soit redessiné. */
export async function settle(page, timeout = 20000) {
  await waitFor(page, () => {
    const v = document.getElementById('video');
    return !v.seeking && v.readyState >= 2;
  }, { timeout });
  await page.evaluate(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))));
  await sleep(250);
}

/** Écart de pixels (0..1) entre le canvas WebGL et l'image de la vue calculée par le serveur (`/frame`), réduits 3x par moyenne de blocs. */
export function previewVsExport(page) {
  return page.evaluate(async () => {
    const X = window.__x3;
    const gl = document.getElementById('gl');
    const w = (gl.width >> 1) << 1, h = (gl.height >> 1) << 1;
    const pid = X.S.proj.id;
    const token = document.querySelector('meta[name=x3-token]').content;
    const frame = await fetch(`/api/project/${pid}/frame?t=${X.S.t}&w=${w}&h=${h}&token=${token}`);
    if (!frame.ok) throw new Error(`/frame : ${frame.status}`);
    const bmp = await createImageBitmap(await frame.blob());
    const grab = (src, sw, sh) => {
      const c = document.createElement('canvas'); c.width = w; c.height = h;
      const ctx = c.getContext('2d'); ctx.drawImage(src, 0, 0, sw, sh, 0, 0, w, h);
      return ctx.getImageData(0, 0, w, h).data;
    };
    const shot = await new Promise((res) => { const i = new Image(); i.onload = () => res(i); i.src = gl.toDataURL('image/png'); });
    const a = grab(shot, shot.width, shot.height), b = grab(bmp, bmp.width, bmp.height);
    const K = 3, bw = Math.floor(w / K), bh = Math.floor(h / K);
    let sum = 0, n = 0;
    for (let by = 0; by < bh; by++) for (let bx = 0; bx < bw; bx++) for (let ch = 0; ch < 3; ch++) {
      let sa = 0, sb = 0;
      for (let dy = 0; dy < K; dy++) for (let dx = 0; dx < K; dx++) {
        const i = ((by * K + dy) * w + bx * K + dx) * 4 + ch; sa += a[i]; sb += b[i];
      }
      sum += Math.abs(sa - sb) / (K * K); n++;
    }
    return { diff: sum / n / 255, w, h };
  });
}

/** Image du canvas WebGL : écart type des niveaux de gris (une vraie scène, pas un écran uni) et moyenne. */
export function canvasStats(page) {
  return page.evaluate(async () => {
    const gl = document.getElementById('gl');
    const img = await new Promise((res) => { const i = new Image(); i.onload = () => res(i); i.src = gl.toDataURL('image/png'); });
    const c = document.createElement('canvas'); c.width = img.width; c.height = img.height;
    const ctx = c.getContext('2d'); ctx.drawImage(img, 0, 0);
    const d = ctx.getImageData(0, 0, c.width, c.height).data;
    let s = 0, s2 = 0, n = 0;
    for (let i = 0; i < d.length; i += 4) { const g = 0.299 * d[i] + 0.587 * d[i + 1] + 0.114 * d[i + 2]; s += g; s2 += g * g; n++; }
    const mean = s / n;
    return { mean, std: Math.sqrt(s2 / n - mean * mean), w: c.width, h: c.height };
  });
}

/** Image du canvas (données brutes RGBA) réduite, pour comparer deux états de l'aperçu. */
export function canvasThumb(page, size = 64) {
  return page.evaluate(async (size) => {
    const gl = document.getElementById('gl');
    const img = await new Promise((res) => { const i = new Image(); i.onload = () => res(i); i.src = gl.toDataURL('image/png'); });
    const c = document.createElement('canvas'); c.width = size; c.height = size;
    const ctx = c.getContext('2d'); ctx.drawImage(img, 0, 0, size, size);
    return Array.from(ctx.getImageData(0, 0, size, size).data);
  }, size);
}

export const meanAbsDiff = (a, b) => a.reduce((s, v, i) => s + Math.abs(v - b[i]), 0) / a.length;

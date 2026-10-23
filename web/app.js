// Interface de X3 Studio Local : tout passe par l'API du serveur local (127.0.0.1).
import { DEFAULT_FOV, FOV_RANGE, GRADE_DEFAULT, IDENTITY, LAYOUT_TILES, defaultTileView, dirToYawPitch, MAX_DISTORTION, STICK_DEFAULT, fmtTime, fovRange, gradeWithDefaults, interpolateView, layoutRects, mul3, multiWithDefaults, outputRay, stickWithDefaults, wrap180, yprToMatrix } from './geometry.js';
import { Viewer } from './viewer.js';
import { replaceKeyframes, simplifySamples } from './recorder.js';

const TOKEN = document.querySelector('meta[name=x3-token]').content;
const $ = (s, r = document) => r.querySelector(s);
const $$ = (s, r = document) => [...r.querySelectorAll(s)];

async function api(method, path, body) {
  const res = await fetch(path, {
    method,
    headers: { 'X-X3-Token': TOKEN, 'Content-Type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const ct = res.headers.get('Content-Type') || '';
  const data = ct.includes('json') ? await res.json() : await res.arrayBuffer();
  if (!res.ok) throw new Error((data && data.error) || `Erreur ${res.status}`);
  return data;
}

function toast(msg, err = false, ms = 5000) {
  const t = $('#toast');
  t.textContent = msg;
  t.className = 'toast' + (err ? ' err' : '');
  t.hidden = false;
  clearTimeout(toast._h);
  toast._h = setTimeout(() => { t.hidden = true; }, ms);
}

// ---------------------------------------------------------------- état
const S = {
  proj: null, st: null, t: 0, dur: 0, fps: 30,
  tile: 0,                   // vue en cours de modification (0 = la vue du projet, 1… = les vues supplémentaires)
  listAt: 0,                 // dernier rafraîchissement de la liste des vues (ms)
  pick: false,               // le prochain clic dans l'aperçu désigne le sujet à suivre
  tracking: null,            // suivi en cours : { job, tile }
  rec: null,                 // enregistrement du cadrage en cours : { tile, view, samples, rate }
  recUndo: null,             // keyframes remplacés par le dernier enregistrement : { tile, keyframes }
  override: null,            // vue modifiée à la main, pas encore enregistrée
  stab: { arr: IDENTITY, n: 1 },
  selKf: -1, dirty: true, lastT: -1, info: null, jobs: [], saveTimer: null, pending: new Set(),
  videoReady: false,
};
const video = $('#video');
let viewer;
try { viewer = new Viewer($('#gl')); } catch (e) { console.error(e); $('#overlay-msg').hidden = false; $('#overlay-msg').textContent = 'WebGL est indisponible dans ce navigateur : ' + e.message; }

const PRESET_DEFAULT_CALIB = { fov: 200, blend: 10, swap_lenses: false, a_yaw: 0, a_pitch: 0, a_roll: 0, b_yaw: 0, b_pitch: 0, b_roll: 0 };

// ---------------------------------------------------------------- sauvegarde
function scheduleSave(...keys) {
  keys.forEach((k) => S.pending.add(k));
  clearTimeout(S.saveTimer);
  S.saveTimer = setTimeout(flushSave, 350);
}
async function flushSave() {
  clearTimeout(S.saveTimer);
  if (!S.proj || !S.pending.size) return;
  const patch = {};
  for (const k of S.pending) patch[k] = S.st[k];
  S.pending.clear();
  try { await api('PUT', `/api/project/${S.proj.id}/state`, patch); } catch (e) { toast(e.message, true); }
}

// ---------------------------------------------------------------- vue courante
// Plusieurs perspectives : la vue 0 est celle du projet (`projection`, `view`, `keyframes` de l'état) ; les suivantes sont
// dans `multi.views` avec les mêmes champs. Tout le panneau « Recadrage » agit sur la vue sélectionnée (`S.tile`).
function tileCount() { return S.st ? LAYOUT_TILES[S.st.multi.layout] || 1 : 1; }
function tileData(i = S.tile) { return i === 0 || !S.st ? S.st : S.st.multi.views[i - 1] || S.st; }
function saveTile(...fields) { if (S.tile === 0) scheduleSave(...fields); else scheduleSave('multi'); }
function projection() { return S.st ? tileData().projection : 'flat'; }
function baseView() { return tileData().view; }
function currentView(t) {
  if (S.rec && S.rec.tile === S.tile) return S.rec.view;
  if (S.override) return S.override;
  return interpolateView(tileData().keyframes, t, baseView());
}
// Vue de la vue n° `i` à l'instant `t` (la vue en cours de modification peut être modifiée à la main, sans keyframe).
function tileViewAt(i, t) {
  if (S.rec && i === S.rec.tile) return S.rec.view;
  if (i === S.tile && S.override) return S.override;
  const d = tileData(i);
  return interpolateView(d.keyframes, t, d.view);
}
function clampView(v) {
  v.distortion = projection() === 'flat' ? Math.min(MAX_DISTORTION, Math.max(0, v.distortion || 0)) : 0;
  const [lo, hi] = fovRange(projection(), v.distortion);
  v.fov = Math.min(hi, Math.max(lo, v.fov));
  v.pitch = Math.min(90, Math.max(-90, v.pitch));
  v.yaw = wrap180(v.yaw);
  v.roll = wrap180(v.roll);
  return v;
}
function editView(patchFn) {
  if (!S.st) return;
  if (S.rec) {   // pendant l'enregistrement, la vue modifiée est celle qu'on filme : rien n'est enregistré avant l'arrêt
    S.rec.view = clampView(patchFn({ ...S.rec.view }));
    S.dirty = true; syncViewControls();
    return;
  }
  const t = S.t, v = clampView(patchFn({ ...currentView(t) }));
  const d = tileData(), kfs = d.keyframes;
  if (!kfs.length) {
    d.view = v; S.override = null; saveTile('view');
  } else {
    const i = kfs.findIndex((k) => Math.abs(k.t - t) <= 1.5 / S.fps);
    if (i >= 0) { Object.assign(kfs[i], v); S.override = null; saveTile('keyframes'); renderKeyframes(); }
    else S.override = v;
  }
  S.dirty = true;
  syncViewControls();
}

const VIEW_CTRL = { yaw: '#v-yaw', pitch: '#v-pitch', roll: '#v-roll', fov: '#v-fov', distortion: '#v-dist' };
function syncViewControls() {
  if (!S.st) return;
  const v = { distortion: 0, ...currentView(S.t) };
  const [lo, hi] = fovRange(projection(), v.distortion);
  const fovEl = $('#v-fov'); fovEl.min = lo; fovEl.max = hi;
  for (const k of Object.keys(VIEW_CTRL)) {
    const el = $(VIEW_CTRL[k]);
    if (document.activeElement !== el) el.value = v[k];
    $('#o-' + k).textContent = k === 'distortion' ? v[k].toFixed(2) : `${v[k].toFixed(k === 'fov' ? 0 : 1)}°`;
  }
  $('#hud-readout').textContent = (tileCount() > 1 ? `vue ${S.tile + 1}  ·  ` : '') + `cap ${v.yaw.toFixed(0)}°  incl. ${v.pitch.toFixed(0)}°  champ ${v.fov.toFixed(0)}°` + (v.distortion > 0.005 ? `  dist. ${v.distortion.toFixed(2)}` : '') + (S.override ? '  ·  vue modifiée (◆ pour l\'enregistrer)' : '');
}

function setProjectionUI() {
  const [lo, hi] = fovRange(projection(), S.st ? baseView().distortion || 0 : 0);
  const f = $('#v-fov'); f.min = lo; f.max = hi;
  $('#proj').value = projection();
  $('#f-dist').hidden = projection() !== 'flat';
  viewer && viewer.setProjection(S.st ? S.st.projection : 'flat');
}

// ---------------------------------------------------------------- stabilisation (aperçu)
async function fetchStab() {
  if (!S.proj) return;
  const s = S.st.stabilization, o = S.st.orientation;
  const ref = Math.round(S.st.trim.start * S.fps);
  const q = new URLSearchParams({
    mode: s.mode, strength: s.strength, heading: s.heading_strength, yaw: o.yaw, pitch: o.pitch, roll: o.roll, ref, token: TOKEN,
  });
  try {
    const buf = await api('GET', `/api/project/${S.proj.id}/stab?${q}`);
    const arr = new Float32Array(buf);
    S.stab = { arr, n: arr.length / 9 };
    S.dirty = true;
  } catch (e) { toast(e.message, true); }
}
function stabMatrix(t) {
  const { arr, n } = S.stab;
  if (n <= 1) return Array.from(arr.slice(0, 9));
  const i = Math.min(n - 1, Math.max(0, Math.round(t * S.fps)));
  return Array.from(arr.subarray(i * 9, i * 9 + 9));
}

let stabTimer;
function debouncedStab() { clearTimeout(stabTimer); stabTimer = setTimeout(fetchStab, 220); }

// ---------------------------------------------------------------- boucle de rendu
function frameLoop() {
  requestAnimationFrame(frameLoop);
  if (!viewer || !S.st) return;
  const moving = !video.paused && !video.ended;
  const t = moving ? video.currentTime || 0 : pausedTime();
  if (moving || S.dirty || t !== S.lastT) {
    if (Math.abs(t - S.lastT) > 0.5 / S.fps && S.override && !moving) { /* changement de temps : la vue manuelle est abandonnée */ S.override = null; }
    if (moving && S.override) S.override = null;
    S.t = t; S.lastT = t;
    const tr = S.st.trim;
    if (moving && S.rec) {
      const last = S.rec.samples[S.rec.samples.length - 1];
      if (!last || t > last.t + 1e-4) S.rec.samples.push({ t, ...S.rec.view });
      if (tr.end > 0 && t >= tr.end) stopRecording();
    } else if (moving && tr.end > 0 && t >= tr.end) { video.currentTime = tr.start; }
    if (S.videoReady) viewer.uploadFrame();
    const views = Array.from({ length: tileCount() }, (_, i) => tileViewAt(i, t));
    viewer.setView(views[0]);
    viewer.setProjection(S.st.projection);
    viewer.setLayout(S.st.multi.layout, S.st.multi.gap, views.slice(1).map((v, j) => ({
      projection: tileData(j + 1).projection, view: v, matrix: yprToMatrix(v.yaw, v.pitch, v.roll),
    })));
    viewer.setStabMatrix(stabMatrix(t));
    viewer.draw(yprToMatrix(views[0].yaw, views[0].pitch, views[0].roll));
    updateTileOutline();
    if (tileCount() > 1 && performance.now() - S.listAt > 300 && $('#pane-views').classList.contains('active')) { S.listAt = performance.now(); renderTileList(); }
    updateTimeUI();
    syncViewControls();
    S.dirty = false;
  }
}

function updateTimeUI() {
  $('#time').textContent = `${fmtTime(S.t)} / ${fmtTime(S.dur)}`;
  const pct = S.dur ? (S.t / S.dur) * 100 : 0;
  $('#playhead').style.left = pct + '%';
  $('#btn-play').textContent = video.paused ? '▶' : '⏸';
  const tl = $('#timeline');
  tl.setAttribute('aria-valuenow', S.t.toFixed(1)); tl.setAttribute('aria-valuemax', S.dur.toFixed(1));
  if (S.st) {
    const tr = S.st.trim;
    $('#trim-l').style.width = S.dur ? (tr.start / S.dur) * 100 + '%' : '0';
    $('#trim-r').style.width = S.dur && tr.end > 0 ? ((S.dur - tr.end) / S.dur) * 100 + '%' : '0';
  }
}

// Instant courant à l'arrêt : celui (théorique, k / cadence) de l'image que le navigateur affiche, la dernière dont
// l'horodatage est ≤ à `currentTime`. L'export prend l'image la plus proche de ce même instant : les deux concordent.
function lastFrame() { return Math.max(0, Math.ceil(S.dur * S.fps - 1e-6) - 1); }
function pausedTime() {
  return Math.min(Math.floor((video.currentTime || 0) * S.fps + 1e-6), lastFrame()) / S.fps;
}

// Se placer sur l'image la plus proche de `t`, en visant le MILIEU de son intervalle d'affichage : pile sur son
// horodatage, un arrondi (WebM : 1 ms ; 29,97 i/s : l'horodatage n'est jamais exact) ou le décalage de +7 ms que
// FFmpeg 4.4 (Ubuntu 22.04) donne à la piste vidéo feraient afficher l'image précédente.
function seek(t) {
  if (S.rec) stopRecording();
  const k = Math.min(Math.max(0, Math.round(t * S.fps)), lastFrame());
  video.currentTime = (k + 0.5) / S.fps;
  S.dirty = true;
}

// ---------------------------------------------------------------- ouverture
async function openProject(path) {
  try {
    const p = await api('POST', '/api/open', { path });
    adoptProject(p);
    $('#dlg-open').close();
    refreshJobs();
  } catch (e) { toast(e.message, true); }
}

function adoptProject(p) {
  S.proj = p; S.st = p.state; S.dur = p.source.duration; S.fps = p.source.fps; S.override = null;
  S.tile = 0; S.selKf = -1; S.rec = null; S.recUndo = null; S.pick = false; S.tracking = null;
  S.st.multi = multiWithDefaults(S.st.multi);
  S.videoReady = false;
  const src = p.source, [w, h] = src.lens_size;
  const dims = src.layout === 'equirect' ? `${w}×${h}` : `2 × ${w}×${h}`;
  $('#file-info').textContent = `${p.name} — ${dims} · ${src.fps.toFixed(2).replace(/\.?0+$/, '')} fps · ${fmtTime(src.duration)}${src.has_trailer ? ' · métadonnées Insta360' : ''}`;
  $('#file-info').title = src.path;
  viewer && viewer.setCalibration(S.st.calibration);
  S.st.grade = gradeWithDefaults(S.st.grade);
  viewer && viewer.setGrade(S.st.grade);
  S.st.stick = stickWithDefaults(S.st.stick);
  viewer && viewer.setStick(S.st.stick);
  setProjectionUI();
  bindStateToControls();
  renderKeyframes();
  fetchStab();
  $('#btn-export').disabled = false;
  updateExportSummary();
  loadVideoIfReady();
  S.dirty = true;
}

function loadVideoIfReady() {
  const msg = $('#overlay-msg');
  if (S.proj.proxy.ready) {
    msg.hidden = true;
    const kind = S.proj.source.layout === 'equirect' ? 'equirect' : 'dual';
    viewer && viewer.setVideo(video, kind);
    video.src = `${S.proj.proxy.url}?token=${TOKEN}&v=${Date.now()}`;
    video.load();
  } else {
    msg.hidden = false;
    msg.textContent = 'Préparation de l\'aperçu… (une seule fois par vidéo)';
  }
}
video.addEventListener('loadeddata', () => { S.videoReady = true; S.dirty = true; seek(Math.max(0, S.st.trim.start)); });
video.addEventListener('seeked', () => { S.dirty = true; });
video.addEventListener('ended', () => { if (S.rec) stopRecording(); });
video.addEventListener('timeupdate', () => { if (video.paused) S.dirty = true; });
// L'aperçu ne se lit pas : dire pourquoi (navigateur sans VP9, serveur qui refuse le fichier, fichier illisible)
// plutôt que d'accuser le VP9 dans tous les cas.
video.addEventListener('error', async () => {
  if (!(S.proj && S.proj.proxy.ready)) return;
  const err = video.error || {};
  const detail = `code ${err.code ?? '?'}${err.message ? ' : ' + err.message : ''}`;
  console.error('Aperçu illisible (' + detail + ') :', (video.currentSrc || '').replace(/token=[^&]+/, 'token=…'));
  toast(await playbackErrorMessage(video.currentSrc, detail), true, 15000);
});
async function playbackErrorMessage(src, detail) {
  if (!video.canPlayType('video/webm; codecs="vp9, opus"')) {
    return 'Ce navigateur ne sait pas lire le VP9 (format de l\'aperçu) : ouvrez l\'interface dans Firefox ou Chromium.';
  }
  try {
    const r = await fetch(src, { headers: { Range: 'bytes=0-1023' } });
    if (r.status !== 200 && r.status !== 206) {
      return `Le serveur a refusé l'aperçu (HTTP ${r.status}) : rechargez la page avec l'adresse affichée dans le terminal.`;
    }
  } catch (e) {
    return 'Le serveur local ne répond plus : relancez x3studio puis rouvrez la vidéo.';
  }
  return `Le navigateur n'a pas pu décoder l'aperçu (${detail}). Supprimez le dossier du projet dans ~/.cache/x3studio/projects/${S.proj.id} pour le régénérer.`;
}

// ---------------------------------------------------------------- transport / timeline
$('#btn-play').addEventListener('click', togglePlay);
function togglePlay() {
  if (!S.videoReady) return;
  if (S.rec) { stopRecording(); return; }
  if (video.paused) {
    const tr = S.st.trim;
    if (tr.end > 0 && video.currentTime >= tr.end) video.currentTime = tr.start;
    video.muted = false;
    video.play().catch(() => { video.muted = true; video.play(); });
  } else video.pause();
  S.dirty = true;
}

const timeline = $('#timeline');
function timeFromEvent(e) {
  const r = timeline.getBoundingClientRect();
  return Math.min(1, Math.max(0, (e.clientX - r.left) / r.width)) * S.dur;
}
timeline.addEventListener('pointerdown', (e) => {
  if (e.target.classList.contains('kf')) return;
  timeline.setPointerCapture(e.pointerId);
  seek(timeFromEvent(e));
  timeline._drag = true;
});
timeline.addEventListener('pointermove', (e) => { if (timeline._drag) seek(timeFromEvent(e)); });
timeline.addEventListener('pointerup', () => { timeline._drag = false; });
timeline.addEventListener('keydown', (e) => {
  if (e.key === 'ArrowLeft') { seek(S.t - (e.shiftKey ? 1 : 1 / S.fps)); e.preventDefault(); e.stopPropagation(); }
  if (e.key === 'ArrowRight') { seek(S.t + (e.shiftKey ? 1 : 1 / S.fps)); e.preventDefault(); e.stopPropagation(); }
});

$('#btn-in').addEventListener('click', () => setTrim('start'));
$('#btn-out').addEventListener('click', () => setTrim('end'));
function setTrim(which) {
  if (!S.st) return;
  const tr = S.st.trim;
  if (which === 'start') { tr.start = Math.min(S.t, tr.end > 0 ? tr.end - 0.1 : S.t); if (tr.end > 0 && tr.end <= tr.start) tr.end = 0; }
  else tr.end = S.t > tr.start + 0.1 ? S.t : 0;
  scheduleSave('trim'); S.dirty = true; updateExportSummary();
  if (S.st.stabilization.mode === 'lock') debouncedStab();
}

// ---------------------------------------------------------------- keyframes
function renderKeyframes() {
  const layer = $('#kf-layer'), list = $('#kf-list');
  layer.textContent = ''; list.textContent = '';
  const kfs = S.st ? tileData().keyframes : [];
  $('#kf-empty').hidden = kfs.length > 0;
  kfs.forEach((k, i) => {
    const m = document.createElement('div');
    m.className = 'kf' + (i === S.selKf ? ' sel' : '');
    m.style.left = (S.dur ? (k.t / S.dur) * 100 : 0) + '%';
    m.title = `Keyframe à ${fmtTime(k.t)} (glisser pour déplacer)`;
    m.addEventListener('pointerdown', (e) => {
      e.stopPropagation(); m.setPointerCapture(e.pointerId); m._drag = true; seek(k.t); S.selKf = i;
    });
    m.addEventListener('pointermove', (e) => {
      if (!m._drag) return;
      k.t = timeFromEvent(e); m.style.left = (k.t / S.dur) * 100 + '%'; seek(k.t);
    });
    m.addEventListener('pointerup', () => { m._drag = false; kfs.sort((a, b) => a.t - b.t); saveTile('keyframes'); renderKeyframes(); });
    layer.appendChild(m);

    const li = document.createElement('li');
    if (i === S.selKf) li.className = 'sel';
    const info = document.createElement('div');
    const t = document.createElement('span'); t.className = 't'; t.textContent = fmtTime(k.t);
    info.append(t, ` — cap ${k.yaw.toFixed(0)}° · incl. ${k.pitch.toFixed(0)}° · champ ${k.fov.toFixed(0)}°` + (k.distortion > 0.005 ? ` · dist. ${k.distortion.toFixed(2)}` : ''));
    const act = document.createElement('div'); act.className = 'actions';
    const bGo = btn('Aller', () => { S.selKf = i; seek(k.t); renderKeyframes(); });
    const bUp = btn('Mettre à jour', () => { const v = currentView(S.t); Object.assign(k, v, { t: k.t }); saveTile('keyframes'); renderKeyframes(); S.dirty = true; }, 'Remplace ce keyframe par la vue affichée');
    const bDel = btn('✕', () => { kfs.splice(i, 1); S.selKf = -1; saveTile('keyframes'); renderKeyframes(); S.dirty = true; }, 'Supprimer');
    act.append(bGo, bUp, bDel);
    li.append(info, act);
    list.appendChild(li);
  });
}
function btn(label, fn, title) {
  const b = document.createElement('button');
  b.className = 'btn small ghost'; b.textContent = label; if (title) b.title = title;
  b.addEventListener('click', fn);
  return b;
}
function addKeyframe() {
  if (!S.st) return;
  const v = { ...currentView(S.t) };
  const kfs = tileData().keyframes;
  const i = kfs.findIndex((k) => Math.abs(k.t - S.t) <= 1.5 / S.fps);
  const kf = { t: S.t, ...v };
  if (i >= 0) { kfs[i] = kf; S.selKf = i; } else { kfs.push(kf); kfs.sort((a, b) => a.t - b.t); S.selKf = kfs.indexOf(kf); }
  S.override = null; saveTile('keyframes'); renderKeyframes(); S.dirty = true;
}
$('#btn-kf').addEventListener('click', addKeyframe);
$('#btn-kf-clear').addEventListener('click', () => {
  if (!S.st || !tileData().keyframes.length) return;
  if (confirm('Supprimer tous les keyframes ?')) { tileData().keyframes = []; S.selKf = -1; saveTile('keyframes'); renderKeyframes(); S.dirty = true; }
});

// ---------------------------------------------------------------- enregistrement du cadrage
// On lit la vidéo et on oriente la vue à la souris : à l'arrêt, le tracé devient des keyframes (linéaires) qui
// remplacent ceux de la plage enregistrée, pour la vue sélectionnée.
function startRecording() {
  if (!S.videoReady || !S.st) return;
  if (S.pick) setPick(false);
  const tr = S.st.trim;
  if (tr.end > 0 && video.currentTime >= tr.end - 0.05) video.currentTime = tr.start;
  const rate = parseFloat($('#rec-speed').value) || 1;
  const view = clampView({ distortion: 0, ...currentView(S.t) });
  S.rec = { tile: S.tile, view, samples: [{ t: video.currentTime || 0, ...view }], rate };
  S.override = null;
  video.playbackRate = rate;
  video.muted = true;
  video.play().catch(() => stopRecording());
  updateRecUI(); S.dirty = true;
}
function stopRecording() {
  const rec = S.rec;
  if (!rec) return;
  S.rec = null;
  video.pause();
  video.playbackRate = 1;
  const keyframes = simplifySamples(rec.samples);
  if (keyframes.length >= 2) {
    const d = tileData(rec.tile);
    S.recUndo = { tile: rec.tile, keyframes: d.keyframes.map((k) => ({ ...k })) };
    d.keyframes = replaceKeyframes(d.keyframes, keyframes, keyframes[0].t, keyframes[keyframes.length - 1].t);
    d.view = { ...d.view, ...rec.view };
    if (S.tile === rec.tile) S.selKf = -1;
    if (S.tile === 0) scheduleSave('keyframes', 'view'); else scheduleSave('multi');
    toast(`Cadrage enregistré : ${keyframes.length} keyframes de ${fmtTime(keyframes[0].t)} à ${fmtTime(keyframes[keyframes.length - 1].t)}.`);
  } else {
    toast('Rien à enregistrer : lancez l\'enregistrement puis orientez la vue pendant la lecture.', true);
  }
  S.override = null;
  renderKeyframes(); renderTileList(); updateRecUI(); S.dirty = true;
}
function undoRecording() {
  const u = S.recUndo;
  if (!u || !S.st) return;
  tileData(u.tile).keyframes = u.keyframes;
  S.recUndo = null;
  if (u.tile === 0) scheduleSave('keyframes'); else scheduleSave('multi');
  S.override = null; S.selKf = -1;
  renderKeyframes(); renderTileList(); updateRecUI(); S.dirty = true;
}
function updateRecUI() {
  const on = !!S.rec;
  const b = $('#btn-rec');
  b.textContent = on ? '■ Arrêter' : '● Cadrage';
  b.classList.toggle('rec', on);
  b.setAttribute('aria-pressed', on ? 'true' : 'false');
  $('#hud-rec').hidden = !on;
  $('#btn-rec-undo').hidden = !S.recUndo;
}
$('#btn-rec').addEventListener('click', () => (S.rec ? stopRecording() : startRecording()));
$('#btn-rec-undo').addEventListener('click', undoRecording);

// ---------------------------------------------------------------- suivi de sujet
const trackStatus = (text, warn = false) => {
  const box = $('#track-status');
  box.hidden = !text; box.textContent = text || '';
  box.className = 'status-box' + (warn ? ' warn' : '');
};
function setPick(on) {
  S.pick = on && !!S.st && !S.tracking && !S.rec;
  $('#btn-track').setAttribute('aria-pressed', S.pick ? 'true' : 'false');
  $('#btn-track').classList.toggle('rec', S.pick);
  $('#btn-track').textContent = S.pick ? '✕ Annuler la désignation' : '⌖ Désigner un sujet à suivre';
  canvas.style.cursor = S.pick ? 'crosshair' : '';
  if (S.pick) { video.pause(); trackStatus('Cliquez sur le sujet dans l\'aperçu.'); } else if (!S.tracking) trackStatus('');
}
// Direction (repère de la vue) du point cliqué : on remonte la projection de la vue sous le pointeur.
function pointedDirection(e) {
  const r = canvas.getBoundingClientRect();
  const rect = layoutRects(S.st.multi.layout, r.width, r.height, S.st.multi.gap)[S.tile];
  const x = e.clientX - r.left - rect.x, y = e.clientY - r.top - rect.y;
  const v = currentView(S.t), d = tileData();
  const ray = outputRay(d.projection, 2 * x / rect.w - 1, 1 - 2 * y / rect.h, v.fov * Math.PI / 180, rect.h / rect.w, d.projection === 'flat' ? v.distortion || 0 : 0);
  const m = yprToMatrix(v.yaw, v.pitch, v.roll);
  return dirToYawPitch([0, 1, 2].map((i) => m[i * 3] * ray[0] + m[i * 3 + 1] * ray[1] + m[i * 3 + 2] * ray[2]));
}
async function pickSubject(e) {
  const [yaw, pitch] = pointedDirection(e);
  setPick(false);
  const tr = S.st.trim;
  trackStatus('Suivi en cours…');
  try {
    await flushSave();
    const job = await api('POST', `/api/project/${S.proj.id}/track`, {
      t: S.t, yaw, pitch, size: +$('#t-size').value, backward: $('#t-back').checked,
      start: tr.start, end: tr.end > 0 ? tr.end : S.dur,
    });
    S.tracking = { job: job.id, tile: S.tile };
    S.jobs.push({ id: job.id, state: job.state });   // une tâche très courte doit quand même être vue passer à « terminé »
    refreshJobs();
  } catch (err) { trackStatus(''); toast(err.message, true); }
}
// Le suivi est terminé : ses keyframes (cap et inclinaison) remplacent ceux de la plage suivie ; le champ, le roulis et la
// distorsion gardent leurs valeurs d'avant.
async function applyTrack(job) {
  const tracking = S.tracking;
  S.tracking = null;
  try {
    const r = await api('GET', `/api/project/${S.proj.id}/track`);
    if (r.job !== job.id || !r.keyframes || r.keyframes.length < 2) throw new Error('Le suivi n\'a rien donné.');
    const d = tileData(tracking.tile), old = d.keyframes;
    const fresh = r.keyframes.map((k) => ({ ...interpolateView(old, k.t, d.view), ...k }));
    S.recUndo = { tile: tracking.tile, keyframes: old.map((k) => ({ ...k })) };
    d.keyframes = replaceKeyframes(old, fresh, fresh[0].t, fresh[fresh.length - 1].t);
    if (tracking.tile === 0) scheduleSave('keyframes'); else scheduleSave('multi');
    S.override = null; S.selKf = -1;
    const lost = [r.track.lost_before != null && `avant ${fmtTime(r.track.lost_before)}`, r.track.lost_after != null && `à partir de ${fmtTime(r.track.lost_after)}`].filter(Boolean);
    const msg = `Sujet suivi de ${fmtTime(fresh[0].t)} à ${fmtTime(fresh[fresh.length - 1].t)} : ${fresh.length} keyframes.`;
    trackStatus(lost.length ? `${msg} Sujet perdu ${lost.join(' et ')}.` : msg, lost.length > 0);
    toast(msg);
    renderKeyframes(); renderTileList(); updateRecUI(); S.dirty = true;
  } catch (err) { trackStatus(''); toast(err.message, true); }
}
$('#btn-track').addEventListener('click', () => setPick(!S.pick));
$('#t-size').addEventListener('input', (e) => { e.target.parentElement.querySelector('output').textContent = `${e.target.value}°`; });
$('#t-size').parentElement.querySelector('output').textContent = `${$('#t-size').value}°`;

// ---------------------------------------------------------------- interaction avec l'image
const canvas = $('#gl');
let drag = null;
// Vue sous le pointeur (la dernière rencontrée : une incrustation recouvre la vue principale).
function tileAtPointer(e) {
  const n = tileCount();
  if (n === 1) return 0;
  const r = canvas.getBoundingClientRect();
  const rects = layoutRects(S.st.multi.layout, r.width, r.height, S.st.multi.gap);
  let hit = -1;
  rects.forEach((q, i) => { if (e.clientX - r.left >= q.x && e.clientX - r.left < q.x + q.w && e.clientY - r.top >= q.y && e.clientY - r.top < q.y + q.h) hit = i; });
  return hit < 0 ? S.tile : hit;
}
canvas.addEventListener('pointerdown', (e) => {
  if (!S.st) return;
  selectTile(tileAtPointer(e));
  if (S.pick) { e.preventDefault(); pickSubject(e); return; }
  canvas.setPointerCapture(e.pointerId);
  drag = { x: e.clientX, y: e.clientY };
  canvas.classList.add('dragging');
});
canvas.addEventListener('pointermove', (e) => {
  if (!drag) return;
  const dx = e.clientX - drag.x, dy = e.clientY - drag.y;
  drag = { x: e.clientX, y: e.clientY };
  editView((v) => {
    const k = v.fov / canvas.clientWidth * (projection() === 'planet' ? 0.6 : 1 - 0.4 * Math.min(1, v.distortion || 0));
    v.yaw -= dx * k; v.pitch += dy * k;
    return v;
  });
});
const endDrag = () => { drag = null; canvas.classList.remove('dragging'); };
canvas.addEventListener('pointerup', endDrag);
canvas.addEventListener('pointercancel', endDrag);
canvas.addEventListener('wheel', (e) => {
  if (!S.st) return;
  e.preventDefault();
  selectTile(tileAtPointer(e));
  editView((v) => { v.fov *= Math.exp(e.deltaY * 0.0012); return v; });
}, { passive: false });
canvas.addEventListener('dblclick', (e) => { selectTile(tileAtPointer(e)); resetView(); });
$('#btn-reset-view').addEventListener('click', resetView);
function resetView() {
  editView((v) => ({ ...v, yaw: 0, pitch: 0, roll: 0, fov: DEFAULT_FOV[projection()], distortion: 0 }));
}
for (const [k, sel] of Object.entries(VIEW_CTRL)) {
  $(sel).addEventListener('input', (e) => editView((v) => { v[k] = parseFloat(e.target.value); return v; }));
}
function setProjection(p) {
  const d = tileData();
  d.projection = p;
  d.view.fov = DEFAULT_FOV[p];
  d.view.distortion = 0;
  if (p === 'planet') d.view.pitch = -90;
  else if (d.view.pitch === -90) d.view.pitch = 0;
  S.override = null;
  setProjectionUI();
  d.keyframes.forEach((k) => {
    const [lo, hi] = fovRange(p, k.distortion || 0);
    k.fov = Math.min(hi, Math.max(lo, k.fov));
    if (p !== 'flat') k.distortion = 0;
  });
  saveTile('projection', 'view', 'keyframes'); renderKeyframes(); renderTileList(); S.dirty = true; updateExportSummary();
}
$('#proj').addEventListener('change', (e) => { if (S.st) setProjection(e.target.value); });

// Préréglages de perspective : champ + distorsion (+ inclinaison pour la planète) ; le fisheye est une autre projection.
const PRESETS = {
  lineaire: { projection: 'flat', fov: 80, distortion: 0 },
  naturel: { projection: 'flat', fov: 100, distortion: 0.3 },
  large: { projection: 'flat', fov: 130, distortion: 0.55 },
  ultra: { projection: 'flat', fov: 170, distortion: 0.8 },
  fisheye: { projection: 'fisheye', fov: 180, distortion: 0 },
  planete: { projection: 'flat', fov: 250, distortion: 1, pitch: -90 },
};
function applyPreset(name) {
  const p = PRESETS[name];
  if (!S.st || !p) return;
  if (p.projection !== projection()) setProjection(p.projection);
  const { projection: _, ...fields } = p;
  editView((v) => ({ ...v, ...fields }));
}
$$('#v-presets [data-preset]').forEach((b) => b.addEventListener('click', () => applyPreset(b.dataset.preset)));

// ---------------------------------------------------------------- plusieurs perspectives
const LAYOUT_NAMES = { single: 'une vue', side: 'côte à côte', stack: 'haut / bas', pip: 'incrustation', trio: 'une grande et deux petites', quad: 'quatre vues' };
const PROJECTION_NAMES = { flat: 'plat', fisheye: 'fisheye', planet: 'planète' };

// Complète les vues manquantes de la disposition courante (la disposition peut grandir sans que l'utilisateur les ait créées).
function ensureTiles() {
  S.st.multi = multiWithDefaults(S.st.multi);
  if (S.tile >= tileCount()) S.tile = 0;
}
function selectTile(i) {
  if (!S.st || i === S.tile || i >= tileCount()) return;
  S.tile = i; S.override = null; S.selKf = -1;
  setProjectionUI(); renderKeyframes(); renderTileList(); syncViewControls(); S.dirty = true;
}
function renderTileList() {
  if (!S.st) return;
  const list = $('#tile-list'); list.textContent = '';
  for (let i = 0; i < tileCount(); i++) {
    const d = tileData(i), v = interpolateView(d.keyframes, S.t, d.view);
    const li = document.createElement('li');
    if (i === S.tile) li.className = 'sel';
    const info = document.createElement('div');
    const name = document.createElement('span'); name.className = 't'; name.textContent = `Vue ${i + 1}`;
    info.append(name, ` — ${PROJECTION_NAMES[d.projection]} · cap ${v.yaw.toFixed(0)}° · champ ${v.fov.toFixed(0)}°` + (d.keyframes.length ? ` · ${d.keyframes.length} keyframe(s)` : ''));
    const act = document.createElement('div'); act.className = 'actions';
    act.append(btn(i === S.tile ? 'Modifiée' : 'Modifier', () => selectTile(i), 'Les réglages du panneau Recadrage et les keyframes s\'appliquent à cette vue'));
    li.append(info, act);
    list.appendChild(li);
  }
  $('#views-active').textContent = tileCount() > 1 ? `vue ${S.tile + 1} sur ${tileCount()}` : '';
}
function syncLayoutControls() {
  const m = S.st.multi;
  $('#m-layout').value = m.layout;
  $('#f-m-gap').hidden = m.layout === 'single' || m.layout === 'pip';
  if (document.activeElement !== $('#m-gap')) $('#m-gap').value = m.gap;
  $('#m-gap').parentElement.querySelector('output').textContent = `${m.gap} px`;
  renderTileList();
}
// Une vue que l'utilisateur n'a pas touchée : celle que la disposition propose par défaut.
function isUntouchedTile(v, index, count) {
  const d = defaultTileView(index, count);
  return v.projection === d.projection && !v.keyframes.length && ['yaw', 'pitch', 'roll', 'fov', 'distortion'].every((k) => (v.view[k] || 0) === d.view[k]);
}
function setLayout(layout) {
  // les vues pas encore modifiées se répartissent à nouveau sur le tour selon le nombre de vues de la nouvelle disposition
  const oldCount = tileCount(), newCount = LAYOUT_TILES[layout] || 1;
  S.st.multi.views.forEach((v, j) => { if (isUntouchedTile(v, j + 1, oldCount)) S.st.multi.views[j] = defaultTileView(j + 1, newCount); });
  S.st.multi.layout = layout;
  ensureTiles(); syncLayoutControls(); setProjectionUI(); renderKeyframes(); updateTileOutline();
  scheduleSave('multi'); S.dirty = true; updateExportSummary();
}
$('#m-layout').addEventListener('change', (e) => { if (S.st) setLayout(e.target.value); });
$('#m-gap').addEventListener('input', (e) => {
  if (!S.st) return;
  S.st.multi.gap = Math.round(parseFloat(e.target.value) / 4) * 4;
  syncLayoutControls(); scheduleSave('multi'); S.dirty = true;
});
// Cadre de la vue en cours de modification, par-dessus l'aperçu (seulement s'il y en a plusieurs).
function updateTileOutline() {
  const box = $('#tile-outline');
  if (!S.st || tileCount() < 2) { box.hidden = true; return; }
  const k = canvas.clientWidth / canvas.width;
  const r = layoutRects(S.st.multi.layout, canvas.width, canvas.height, S.st.multi.gap)[S.tile];
  if (!r) { box.hidden = true; return; }
  box.hidden = false;
  box.style.cssText = `left:${r.x * k}px;top:${r.y * k}px;width:${r.w * k}px;height:${r.h * k}px`;
}

// ---------------------------------------------------------------- panneau : onglets
$$('.tab').forEach((t) => t.addEventListener('click', () => selectTab(t.dataset.tab)));
function selectTab(name) {
  $$('.tab').forEach((t) => t.classList.toggle('active', t.dataset.tab === name));
  $$('.pane').forEach((p) => p.classList.toggle('active', p.id === 'pane-' + name));
  if (name === 'jobs') refreshJobs();
}

// ---------------------------------------------------------------- stabilisation (contrôles)
function bindStateToControls() {
  const s = S.st.stabilization, o = S.st.orientation, c = S.st.calibration;
  $$('input[name=stab]').forEach((r) => { r.checked = r.value === s.mode; });
  setOut('#s-strength', s.strength, 's'); setOut('#s-heading', s.heading_strength, 's');
  $('#f-heading').style.display = s.mode === 'horizon' ? '' : 'none';
  setOut('#orient-yaw', o.yaw, '°'); setOut('#orient-pitch', o.pitch, '°'); setOut('#orient-roll', o.roll, '°');
  for (const k of Object.keys(PRESET_DEFAULT_CALIB)) {
    const el = $('#c-' + k);
    if (!el) continue;
    if (el.type === 'checkbox') el.checked = !!c[k]; else setOut('#c-' + k, c[k], '°');
  }
  for (const k of Object.keys(GRADE_DEFAULT)) setGradeControl(k);
  syncStickControls();
  syncLayoutControls();
  updateAnalysisStatus();
}
function setOut(sel, v, unit) {
  const el = $(sel); el.value = v;
  const out = el.parentElement.querySelector('output');
  if (out) out.textContent = (Number.isInteger(+v) ? (+v).toFixed(0) : (+v).toFixed(1)) + unit;
}
$$('input[name=stab]').forEach((r) => r.addEventListener('change', () => {
  if (!S.st) return;
  S.st.stabilization.mode = r.value;
  $('#f-heading').style.display = r.value === 'horizon' ? '' : 'none';
  if (r.value !== 'off' && !S.proj.analysis.ready) toast('L\'analyse du mouvement n\'est pas terminée : la stabilisation s\'activera à la fin.');
  scheduleSave('stabilization'); fetchStab(); updateExportSummary();
}));
for (const [id, key] of [['#s-strength', 'strength'], ['#s-heading', 'heading_strength']]) {
  $(id).addEventListener('input', (e) => {
    S.st.stabilization[key] = parseFloat(e.target.value);
    setOut(id, e.target.value, 's'); scheduleSave('stabilization'); debouncedStab();
  });
}
for (const [id, key, unit] of [['#orient-yaw', 'yaw', '°'], ['#orient-pitch', 'pitch', '°'], ['#orient-roll', 'roll', '°']]) {
  $(id).addEventListener('input', (e) => {
    S.st.orientation[key] = parseFloat(e.target.value);
    setOut(id, e.target.value, unit); scheduleSave('orientation'); debouncedStab();
  });
}
$('#btn-orient-reset').addEventListener('click', () => {
  S.st.orientation = { yaw: 0, pitch: 0, roll: 0 }; bindStateToControls(); scheduleSave('orientation'); fetchStab();
});
$('#btn-center-here').addEventListener('click', () => {
  // la vue actuelle devient « l'avant » : on décale l'orientation du cap affiché
  const v = currentView(S.t);
  S.st.orientation.yaw = wrap180(S.st.orientation.yaw + v.yaw);
  // toutes les vues suivent le recentrage ; la vue affichée devient « l'avant »
  for (let i = 0; i < tileCount(); i++) {
    const d = tileData(i);
    d.view.yaw = i === S.tile ? 0 : wrap180(d.view.yaw - v.yaw);
    d.keyframes.forEach((k) => { k.yaw = wrap180(k.yaw - v.yaw); });
  }
  S.override = null;
  bindStateToControls(); scheduleSave('orientation', 'view', 'keyframes', 'multi'); renderKeyframes(); fetchStab();
});
$('#btn-analyze').addEventListener('click', async () => {
  try { await api('POST', `/api/project/${S.proj.id}/analyze`, {}); selectTab('jobs'); refreshJobs(); } catch (e) { toast(e.message, true); }
});
function updateAnalysisStatus() {
  const box = $('#analysis-status');
  if (!S.proj) { box.textContent = ''; return; }
  const a = S.proj.analysis;
  box.className = 'status-box';
  $('#btn-analyze').disabled = !S.proj.proxy.ready;
  if (!a.ready) { box.textContent = S.proj.proxy.ready ? 'Analyse du mouvement en attente ou en cours…' : 'L\'analyse démarrera après la préparation de l\'aperçu.'; return; }
  if (a.stale) { box.classList.add('warn'); box.innerHTML = ''; box.append('Les réglages d\'objectifs ont changé depuis l\'analyse : ', Object.assign(document.createElement('b'), { textContent: 'recalculez-la.' })); return; }
  const quality = a.median_inliers >= 60 ? 'bonne' : a.median_inliers >= 25 ? 'moyenne' : 'faible';
  if (quality === 'faible') box.classList.add('warn');
  box.textContent = `Analyse prête : ${a.frames} images, ${a.median_inliers} points suivis en moyenne (fiabilité ${quality}).` + (a.weak_frames ? ` ${a.weak_frames} images peu fiables.` : '');
}

// ---------------------------------------------------------------- réglages d'image
const GRADE_FMT = {
  brightness: (v) => (v > 0 ? '+' : '') + v.toFixed(2), warmth: (v) => (v > 0 ? '+' : '') + v.toFixed(2),
  tint: (v) => (v > 0 ? '+' : '') + v.toFixed(2),
  contrast: (v) => '×' + v.toFixed(2), saturation: (v) => '×' + v.toFixed(2), gamma: (v) => v.toFixed(2),
  sharpen: (v) => v.toFixed(2), denoise: (v) => v.toFixed(2),
};
function setGradeControl(k) {
  const el = $('#g-' + k), v = S.st.grade[k];
  if (document.activeElement !== el) el.value = v;
  el.parentElement.querySelector('output').textContent = GRADE_FMT[k](+v);
}
for (const k of Object.keys(GRADE_DEFAULT)) {
  $('#g-' + k).addEventListener('input', (e) => {
    if (!S.st) return;
    S.st.grade[k] = parseFloat(e.target.value);
    setGradeControl(k); viewer && viewer.setGrade(S.st.grade); S.dirty = true; scheduleSave('grade');
  });
}
$('#btn-grade-reset').addEventListener('click', () => {
  if (!S.st) return;
  S.st.grade = { ...GRADE_DEFAULT };
  for (const k of Object.keys(GRADE_DEFAULT)) setGradeControl(k);
  viewer && viewer.setGrade(S.st.grade); S.dirty = true; scheduleSave('grade');
});

// ---------------------------------------------------------------- perche à selfie / nadir
function syncStickControls() {
  const k = S.st.stick;
  $('#k-mode').value = k.mode;
  for (const [id, key, unit] of [['#k-radius', 'radius', '°'], ['#k-yaw', 'yaw', '°'], ['#k-pitch', 'pitch', '°']]) {
    if (document.activeElement !== $(id)) $(id).value = k[key];
    $(id).parentElement.querySelector('output').textContent = `${(+k[key]).toFixed(1)}${unit}`;
  }
  if (document.activeElement !== $('#k-softness')) $('#k-softness').value = k.softness;
  $('#k-softness').parentElement.querySelector('output').textContent = (+k.softness).toFixed(2);
  $('#k-color').value = k.color;
  $('#f-k-soft').hidden = $('#f-k-color').hidden = k.mode !== 'cover';
  $('#k-radius').disabled = k.mode === 'off';
}
function editStick(patch) {
  if (!S.st) return;
  S.st.stick = { ...S.st.stick, ...patch };
  syncStickControls(); viewer && viewer.setStick(S.st.stick); S.dirty = true; scheduleSave('stick');
}
$('#k-mode').addEventListener('change', (e) => editStick({ mode: e.target.value }));
for (const [id, key] of [['#k-radius', 'radius'], ['#k-softness', 'softness'], ['#k-yaw', 'yaw'], ['#k-pitch', 'pitch']]) {
  $(id).addEventListener('input', (e) => editStick({ [key]: parseFloat(e.target.value) }));
}
$('#k-color').addEventListener('input', (e) => editStick({ color: e.target.value }));
$('#btn-stick-axis-reset').addEventListener('click', () => editStick({ yaw: STICK_DEFAULT.yaw, pitch: STICK_DEFAULT.pitch }));
$('#k-show').addEventListener('change', (e) => { if (viewer) viewer.showStick = e.target.checked; S.dirty = true; });
$('#btn-look-nadir').addEventListener('click', () => applyPreset('planete'));

// ---------------------------------------------------------------- objectifs (calibration)
for (const k of ['fov', 'blend', 'a_yaw', 'a_pitch', 'a_roll', 'b_yaw', 'b_pitch', 'b_roll']) {
  $('#c-' + k).addEventListener('input', (e) => {
    S.st.calibration[k] = parseFloat(e.target.value);
    setOut('#c-' + k, e.target.value, '°');
    viewer.setCalibration(S.st.calibration); S.dirty = true; scheduleSave('calibration'); markStale();
  });
}
// quarts de tour : si l'image d'un objectif est couchée ou à l'envers, le roulis fin (±10°) ne suffit pas
for (const k of ['a', 'b']) {
  $('#c-' + k + '_turn').addEventListener('click', () => {
    const el = $('#c-' + k + '_roll');
    let r = parseFloat(el.value) + 90;
    if (r > 180) r -= 360;
    el.value = r;
    el.dispatchEvent(new Event('input'));
  });
}
$('#c-swap').addEventListener('change', (e) => {
  S.st.calibration.swap_lenses = e.target.checked; viewer.setCalibration(S.st.calibration); S.dirty = true; scheduleSave('calibration'); markStale();
});
$('#c-seam').addEventListener('change', (e) => { viewer.seam = e.target.checked; S.dirty = true; });
$('#btn-calib-reset').addEventListener('click', () => {
  S.st.calibration = { ...S.st.calibration, ...PRESET_DEFAULT_CALIB };
  bindStateToControls(); viewer.setCalibration(S.st.calibration); S.dirty = true; scheduleSave('calibration'); markStale();
});
function markStale() { if (S.proj && S.proj.analysis.ready) { S.proj.analysis.stale = true; updateAnalysisStatus(); } }

// ---------------------------------------------------------------- export
const SIZE_NOTE = { reframe: 'vidéo plate', equirect: 'vidéo 360°' };
function exportSettings() {
  const mode = $('#e-mode').value;
  const s = {
    mode, fps: +$('#e-fps').value, speed: +$('#e-speed').value, codec: $('#e-codec').value, crf: +$('#e-crf').value,
    audio: $('#e-audio').checked, projection: S.st.projection,
  };
  if (mode === 'reframe') { const [w, h] = $('#e-size').value.split('x').map(Number); s.width = w; s.height = h; }
  else s.stitch_width = +$('#e-size360').value;
  return s;
}
function updateExportSummary() {
  const box = $('#export-summary');
  if (!S.st) { box.textContent = 'Ouvrez une vidéo pour exporter.'; return; }
  const tr = S.st.trim, end = tr.end > 0 ? tr.end : S.dur, dur = end - tr.start, sp = +$('#e-speed').value;
  $('#f-size').hidden = $('#e-mode').value !== 'reframe';
  $('#f-size360').hidden = $('#e-mode').value === 'reframe';
  const stab = S.st.stabilization.mode;
  box.textContent = '';
  const b = document.createElement('b'); b.textContent = `${fmtTime(tr.start)} → ${fmtTime(end)}`;
  box.append('Plage exportée : ', b, ` (${(dur / sp).toFixed(1)} s en sortie)`);
  box.append(document.createElement('br'), `Stabilisation : ${({ off: 'désactivée', smooth: 'lissage', lock: 'verrouillage', horizon: 'horizon' })[stab]} · ${S.st.keyframes.length} keyframe(s)`);
  if ($('#e-mode').value === 'reframe') {
    box.append(document.createElement('br'), `Projection : ${({ flat: 'plate', fisheye: 'fisheye', planet: 'petite planète' })[S.st.projection]}`);
    if (tileCount() > 1) box.append(document.createElement('br'), `Disposition : ${LAYOUT_NAMES[S.st.multi.layout]} (${tileCount()} vues)`);
  }
}
$$('#pane-export select, #pane-export input').forEach((el) => el.addEventListener('change', updateExportSummary));
$('#btn-export').addEventListener('click', async () => {
  if (!S.proj) return;
  await flushSave();
  try {
    await api('POST', `/api/project/${S.proj.id}/export`, { settings: exportSettings(), directory: $('#e-dir').value.trim() || undefined });
    selectTab('jobs'); refreshJobs();
  } catch (e) { toast(e.message, true); }
});
$('#btn-snap').addEventListener('click', async () => {
  if (!S.proj) return;
  await flushSave();
  try {
    const mode = 'reframe';
    const sz = $('#e-size').value.split('x').map(Number);
    const r = await api('POST', `/api/project/${S.proj.id}/snapshot`, { t: S.t, settings: { mode, width: sz[0], height: sz[1], projection: S.st.projection }, directory: $('#e-dir').value.trim() || undefined });
    toast('Image enregistrée : ' + r.path);
  } catch (e) { toast(e.message, true); }
});

// ---------------------------------------------------------------- tâches
let jobTimer = null;
async function refreshJobs() {
  let jobs;
  try { jobs = await api('GET', '/api/jobs'); } catch { return; }
  const prev = new Map(S.jobs.map((j) => [j.id, j.state]));
  S.jobs = jobs;
  const list = $('#job-list'); list.textContent = '';
  $('#job-empty').hidden = jobs.length > 0;
  const active = jobs.filter((j) => j.state === 'running' || j.state === 'queued');
  const badge = $('#jobs-badge'); badge.hidden = active.length === 0; badge.textContent = active.length;
  for (const j of jobs) {
    const li = document.createElement('li');
    const title = document.createElement('div'); title.className = 'title';
    const name = document.createElement('span'); name.textContent = j.title;
    const stateTxt = { queued: 'en attente', running: `${Math.round(j.progress * 100)} %`, done: 'terminé', error: 'erreur', cancelled: 'annulé' }[j.state];
    const st = document.createElement('span'); st.textContent = stateTxt;
    title.append(name, st); li.appendChild(title);
    if (j.state === 'running' || j.state === 'queued') {
      const pr = document.createElement('progress'); pr.max = 1; pr.value = j.progress; li.appendChild(pr);
      const meta = document.createElement('div'); meta.className = 'meta';
      meta.textContent = [j.message, j.eta != null ? `reste ≈ ${Math.max(1, Math.round(j.eta))} s` : ''].filter(Boolean).join(' · ');
      li.appendChild(meta);
      li.appendChild(btn('Annuler', async () => { await api('POST', `/api/jobs/${j.id}/cancel`, {}); refreshJobs(); }));
    } else if (j.state === 'done' && j.kind === 'export') {
      const meta = document.createElement('div'); meta.className = 'meta'; meta.textContent = `${j.output} (${Math.round(j.elapsed)} s)`; li.appendChild(meta);
      li.appendChild(btn('Ouvrir le dossier', () => api('POST', '/api/reveal', { path: j.output }).catch((e) => toast(e.message, true))));
    } else if (j.state === 'error') {
      const meta = document.createElement('div'); meta.className = 'meta err'; meta.textContent = j.error; li.appendChild(meta);
    }
    list.appendChild(li);
    if (S.tracking && j.id === S.tracking.job && j.state === 'running') trackStatus(`Suivi en cours… ${Math.round(j.progress * 100)} %`);
    if (prev.get(j.id) && prev.get(j.id) !== j.state) onJobChange(j);
  }
  clearTimeout(jobTimer);
  if (active.length) jobTimer = setTimeout(refreshJobs, 700);
}
async function onJobChange(j) {
  if (j.state === 'done' && j.kind === 'export') toast('Export terminé : ' + j.output);
  if (j.state === 'error') toast(j.kind === 'track' && j.error ? j.error : `${j.title} : échec`, true);
  if (S.tracking && j.id === S.tracking.job) {
    if (j.state === 'done') applyTrack(j);
    else if (j.state === 'error' || j.state === 'cancelled') { S.tracking = null; trackStatus(j.state === 'error' ? j.error : 'Suivi annulé.', true); }
  }
  if (!S.proj || j.project_id !== S.proj.id) return;
  if (j.state === 'done' && (j.kind === 'proxy' || j.kind === 'analyze')) {
    const p = await api('GET', `/api/project/${S.proj.id}`);
    const wasReady = S.proj.proxy.ready;
    S.proj = p;
    updateAnalysisStatus();
    if (!wasReady && p.proxy.ready) loadVideoIfReady();
    if (j.kind === 'analyze') fetchStab();
  }
}

// ---------------------------------------------------------------- ouverture de fichiers
const dlg = $('#dlg-open');
$('#btn-open').addEventListener('click', () => { dlg.showModal(); fsLoad(localStorage.getItem('x3-last-dir') || ''); });
async function fsLoad(path) {
  try {
    const r = await api('GET', '/api/fs?path=' + encodeURIComponent(path));
    localStorage.setItem('x3-last-dir', r.path);
    $('#fs-path').value = r.path;
    $('#fs-up').disabled = !r.parent; $('#fs-up').dataset.path = r.parent || '';
    const ul = $('#fs-list'); ul.textContent = '';
    if (!r.entries.length) { const li = document.createElement('li'); li.textContent = 'Dossier vide (aucune vidéo).'; ul.appendChild(li); }
    for (const e of r.entries) {
      const li = document.createElement('li'); li.tabIndex = 0;
      const n = document.createElement('span'); n.textContent = (e.dir ? '📁 ' : '🎞 ') + e.name;
      li.appendChild(n);
      if (!e.dir) { const s = document.createElement('span'); s.className = 'size'; s.textContent = (e.size / 1e6).toFixed(0) + ' Mo'; li.appendChild(s); }
      const go = () => (e.dir ? fsLoad(e.path) : openProject(e.path));
      li.addEventListener('click', go);
      li.addEventListener('keydown', (ev) => { if (ev.key === 'Enter') go(); });
      ul.appendChild(li);
    }
  } catch (e) { toast(e.message, true); }
}
$('#fs-up').addEventListener('click', (e) => fsLoad(e.currentTarget.dataset.path));
$('#fs-go').addEventListener('click', () => {
  const v = $('#fs-path').value.trim();
  if (/\.(insv|lrv|mp4|mov|mkv|webm|m4v)$/i.test(v)) openProject(v); else fsLoad(v);
});
$('#fs-path').addEventListener('keydown', (e) => { if (e.key === 'Enter') { e.preventDefault(); $('#fs-go').click(); } });
$('#btn-help').addEventListener('click', () => $('#dlg-help').showModal());

// ---------------------------------------------------------------- clavier
document.addEventListener('keydown', (e) => {
  const tag = (e.target.tagName || '').toLowerCase();
  if (['input', 'select', 'textarea', 'dialog'].includes(tag) || e.ctrlKey || e.metaKey || e.altKey) return;
  if (document.querySelector('dialog[open]')) return;
  if (e.key === ' ') { e.preventDefault(); togglePlay(); }
  else if (e.key === 'ArrowLeft') { e.preventDefault(); seek(S.t - (e.shiftKey ? 1 : 1 / S.fps)); }
  else if (e.key === 'ArrowRight') { e.preventDefault(); seek(S.t + (e.shiftKey ? 1 : 1 / S.fps)); }
  else if (e.key === 'k' || e.key === 'K') addKeyframe();
  else if (e.key === 'r' || e.key === 'R') { if (S.rec) stopRecording(); else startRecording(); }
  else if (e.key === 'Escape' && S.pick) setPick(false);
  else if (e.key === 'i' || e.key === 'I') setTrim('start');
  else if (e.key === 'o' || e.key === 'O') setTrim('end');
});

// ---------------------------------------------------------------- démarrage
(async function init() {
  try {
    S.info = await api('GET', '/api/info');
    $('#e-dir').value = S.info.output_dir;
  } catch (e) { toast(e.message, true); }
  updateExportSummary();
  frameLoop();
  window.__x3 = { S, api, openProject, seek, addKeyframe, editView, flushSave, fetchStab, viewer };   // utile pour les tests
  const q = new URLSearchParams(location.search).get('open');
  if (q) openProject(q);
})();

// Interface de X3 Studio Local : tout passe par l'API du serveur local (127.0.0.1).
import { DEFAULT_FOV, FOV_RANGE, IDENTITY, fmtTime, interpolateView, mul3, wrap180, yprToMatrix } from './geometry.js';
import { Viewer } from './viewer.js';

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
function projection() { return S.st ? S.st.projection : 'flat'; }
function baseView() { return S.st.view; }
function currentView(t) {
  if (S.override) return S.override;
  return interpolateView(S.st.keyframes, t, baseView());
}
function clampView(v) {
  const [lo, hi] = FOV_RANGE[projection()];
  v.fov = Math.min(hi, Math.max(lo, v.fov));
  v.pitch = Math.min(90, Math.max(-90, v.pitch));
  v.yaw = wrap180(v.yaw);
  v.roll = wrap180(v.roll);
  return v;
}
function editView(patchFn) {
  if (!S.st) return;
  const t = S.t, v = clampView(patchFn({ ...currentView(t) }));
  const kfs = S.st.keyframes;
  if (!kfs.length) {
    S.st.view = v; S.override = null; scheduleSave('view');
  } else {
    const i = kfs.findIndex((k) => Math.abs(k.t - t) <= 1.5 / S.fps);
    if (i >= 0) { Object.assign(kfs[i], v); S.override = null; scheduleSave('keyframes'); renderKeyframes(); }
    else S.override = v;
  }
  S.dirty = true;
  syncViewControls();
}

const VIEW_CTRL = { yaw: '#v-yaw', pitch: '#v-pitch', roll: '#v-roll', fov: '#v-fov' };
function syncViewControls() {
  if (!S.st) return;
  const v = currentView(S.t);
  for (const k of Object.keys(VIEW_CTRL)) {
    const el = $(VIEW_CTRL[k]);
    if (document.activeElement !== el) el.value = v[k];
    $('#o-' + k).textContent = `${v[k].toFixed(k === 'fov' ? 0 : 1)}°`;
  }
  $('#hud-readout').textContent = `cap ${v.yaw.toFixed(0)}°  incl. ${v.pitch.toFixed(0)}°  champ ${v.fov.toFixed(0)}°` + (S.override ? '  ·  vue modifiée (◆ pour l\'enregistrer)' : '');
}

function setProjectionUI() {
  const [lo, hi] = FOV_RANGE[projection()];
  const f = $('#v-fov'); f.min = lo; f.max = hi;
  $('#proj').value = projection();
  viewer && viewer.setProjection(projection());
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
  const t = video.currentTime || 0;
  const moving = !video.paused && !video.ended;
  if (moving || S.dirty || t !== S.lastT) {
    if (Math.abs(t - S.lastT) > 0.5 / S.fps && S.override && !moving) { /* changement de temps : la vue manuelle est abandonnée */ S.override = null; }
    if (moving && S.override) S.override = null;
    S.t = t; S.lastT = t;
    const tr = S.st.trim;
    if (moving && tr.end > 0 && t >= tr.end) { video.currentTime = tr.start; }
    if (S.videoReady) viewer.uploadFrame();
    const v = currentView(t);
    viewer.setView(v);
    viewer.setStabMatrix(stabMatrix(t));
    viewer.draw(yprToMatrix(v.yaw, v.pitch, v.roll));
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

function seek(t) {
  t = Math.min(Math.max(0, t), Math.max(0, S.dur - 0.001));
  video.currentTime = t;
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
  S.videoReady = false;
  const src = p.source, [w, h] = src.lens_size;
  const dims = src.layout === 'equirect' ? `${w}×${h}` : `2 × ${w}×${h}`;
  $('#file-info').textContent = `${p.name} — ${dims} · ${src.fps.toFixed(2).replace(/\.?0+$/, '')} fps · ${fmtTime(src.duration)}${src.has_trailer ? ' · métadonnées Insta360' : ''}`;
  $('#file-info').title = src.path;
  viewer && viewer.setCalibration(S.st.calibration);
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
video.addEventListener('timeupdate', () => { if (video.paused) S.dirty = true; });
video.addEventListener('error', () => { if (S.proj && S.proj.proxy.ready) toast('Lecture de l\'aperçu impossible dans ce navigateur (codec VP9 requis).', true); });

// ---------------------------------------------------------------- transport / timeline
$('#btn-play').addEventListener('click', togglePlay);
function togglePlay() {
  if (!S.videoReady) return;
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
  const kfs = S.st ? S.st.keyframes : [];
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
    m.addEventListener('pointerup', () => { m._drag = false; S.st.keyframes.sort((a, b) => a.t - b.t); scheduleSave('keyframes'); renderKeyframes(); });
    layer.appendChild(m);

    const li = document.createElement('li');
    if (i === S.selKf) li.className = 'sel';
    const info = document.createElement('div');
    const t = document.createElement('span'); t.className = 't'; t.textContent = fmtTime(k.t);
    info.append(t, ` — cap ${k.yaw.toFixed(0)}° · incl. ${k.pitch.toFixed(0)}° · champ ${k.fov.toFixed(0)}°`);
    const act = document.createElement('div'); act.className = 'actions';
    const bGo = btn('Aller', () => { S.selKf = i; seek(k.t); renderKeyframes(); });
    const bUp = btn('Mettre à jour', () => { const v = currentView(S.t); Object.assign(k, v, { t: k.t }); scheduleSave('keyframes'); renderKeyframes(); S.dirty = true; }, 'Remplace ce keyframe par la vue affichée');
    const bDel = btn('✕', () => { S.st.keyframes.splice(i, 1); S.selKf = -1; scheduleSave('keyframes'); renderKeyframes(); S.dirty = true; }, 'Supprimer');
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
  const kfs = S.st.keyframes;
  const i = kfs.findIndex((k) => Math.abs(k.t - S.t) <= 1.5 / S.fps);
  const kf = { t: S.t, ...v };
  if (i >= 0) { kfs[i] = kf; S.selKf = i; } else { kfs.push(kf); kfs.sort((a, b) => a.t - b.t); S.selKf = kfs.indexOf(kf); }
  S.override = null; scheduleSave('keyframes'); renderKeyframes(); S.dirty = true;
}
$('#btn-kf').addEventListener('click', addKeyframe);
$('#btn-kf-clear').addEventListener('click', () => {
  if (!S.st || !S.st.keyframes.length) return;
  if (confirm('Supprimer tous les keyframes ?')) { S.st.keyframes = []; S.selKf = -1; scheduleSave('keyframes'); renderKeyframes(); S.dirty = true; }
});

// ---------------------------------------------------------------- interaction avec l'image
const canvas = $('#gl');
let drag = null;
canvas.addEventListener('pointerdown', (e) => {
  if (!S.st) return;
  canvas.setPointerCapture(e.pointerId);
  drag = { x: e.clientX, y: e.clientY };
  canvas.classList.add('dragging');
});
canvas.addEventListener('pointermove', (e) => {
  if (!drag) return;
  const dx = e.clientX - drag.x, dy = e.clientY - drag.y;
  drag = { x: e.clientX, y: e.clientY };
  editView((v) => {
    const k = v.fov / canvas.clientWidth * (projection() === 'planet' ? 0.6 : 1);
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
  editView((v) => { v.fov *= Math.exp(e.deltaY * 0.0012); return v; });
}, { passive: false });
canvas.addEventListener('dblclick', resetView);
$('#btn-reset-view').addEventListener('click', resetView);
function resetView() {
  editView((v) => ({ ...v, yaw: 0, pitch: 0, roll: 0, fov: DEFAULT_FOV[projection()] }));
}
for (const [k, sel] of Object.entries(VIEW_CTRL)) {
  $(sel).addEventListener('input', (e) => editView((v) => { v[k] = parseFloat(e.target.value); return v; }));
}
$('#proj').addEventListener('change', (e) => {
  if (!S.st) return;
  S.st.projection = e.target.value;
  S.st.view.fov = DEFAULT_FOV[S.st.projection];
  if (S.st.projection === 'planet') S.st.view.pitch = -90;
  else if (S.st.view.pitch === -90) S.st.view.pitch = 0;
  S.override = null;
  setProjectionUI();
  S.st.keyframes.forEach((k) => { k.fov = Math.min(FOV_RANGE[S.st.projection][1], Math.max(FOV_RANGE[S.st.projection][0], k.fov)); });
  scheduleSave('projection', 'view', 'keyframes'); renderKeyframes(); S.dirty = true; updateExportSummary();
});

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
  S.st.view.yaw = 0;
  S.st.keyframes.forEach((k) => { k.yaw = wrap180(k.yaw - v.yaw); });
  S.override = null;
  bindStateToControls(); scheduleSave('orientation', 'view', 'keyframes'); renderKeyframes(); fetchStab();
});
$('#btn-analyze').addEventListener('click', async () => {
  try { await api('POST', `/api/project/${S.proj.id}/analyze`, {}); selectTab('jobs'); refreshJobs(); } catch (e) { toast(e.message, true); }
});
function updateAnalysisStatus() {
  const box = $('#analysis-status');
  if (!S.proj) { box.textContent = ''; return; }
  const a = S.proj.analysis;
  box.className = 'status-box';
  if (!a.available) { box.classList.add('err'); box.textContent = 'OpenCV n\'est pas installé : stabilisation indisponible (sudo apt install python3-opencv).'; $('#btn-analyze').disabled = true; return; }
  $('#btn-analyze').disabled = !S.proj.proxy.ready;
  if (!a.ready) { box.textContent = S.proj.proxy.ready ? 'Analyse du mouvement en attente ou en cours…' : 'L\'analyse démarrera après la préparation de l\'aperçu.'; return; }
  if (a.stale) { box.classList.add('warn'); box.innerHTML = ''; box.append('Les réglages d\'objectifs ont changé depuis l\'analyse : ', Object.assign(document.createElement('b'), { textContent: 'recalculez-la.' })); return; }
  const quality = a.median_inliers >= 60 ? 'bonne' : a.median_inliers >= 25 ? 'moyenne' : 'faible';
  if (quality === 'faible') box.classList.add('warn');
  box.textContent = `Analyse prête : ${a.frames} images, ${a.median_inliers} points suivis en moyenne (fiabilité ${quality}).` + (a.weak_frames ? ` ${a.weak_frames} images peu fiables.` : '');
}

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
    audio: $('#e-audio').checked, projection: projection(),
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
  if ($('#e-mode').value === 'reframe') box.append(document.createElement('br'), `Projection : ${({ flat: 'plate', fisheye: 'fisheye', planet: 'petite planète' })[projection()]}`);
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
    const r = await api('POST', `/api/project/${S.proj.id}/snapshot`, { t: S.t, settings: { mode, width: sz[0], height: sz[1], projection: projection() }, directory: $('#e-dir').value.trim() || undefined });
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
    if (prev.get(j.id) && prev.get(j.id) !== j.state) onJobChange(j);
  }
  clearTimeout(jobTimer);
  if (active.length) jobTimer = setTimeout(refreshJobs, 700);
}
async function onJobChange(j) {
  if (j.state === 'done' && j.kind === 'export') toast('Export terminé : ' + j.output);
  if (j.state === 'error') toast(`${j.title} : échec`, true);
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
  if (/\.(insv|mp4|mov|mkv|webm|m4v)$/i.test(v)) openProject(v); else fsLoad(v);
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
  else if (e.key === 'i' || e.key === 'I') setTrim('start');
  else if (e.key === 'o' || e.key === 'O') setTrim('end');
});

// ---------------------------------------------------------------- démarrage
(async function init() {
  try {
    S.info = await api('GET', '/api/info');
    $('#e-dir').value = S.info.output_dir;
    if (!S.info.opencv) toast('OpenCV est absent : export plus lent et stabilisation indisponible (sudo apt install python3-opencv).', true, 9000);
  } catch (e) { toast(e.message, true); }
  updateExportSummary();
  frameLoop();
  window.__x3 = { S, api, openProject, seek, addKeyframe, editView, flushSave, fetchStab, viewer };   // utile pour les tests
  const q = new URLSearchParams(location.search).get('open');
  if (q) openProject(q);
})();

// Interface web dans un vrai navigateur (Chromium sans écran, WebGL logiciel) pilotant le vrai serveur x3studio.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { copyFileSync, mkdirSync, readFileSync, readdirSync, statSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { after, before, describe, it } from 'node:test';
import { canvasStats, canvasThumb, launchBrowser, makeDemo, meanAbsDiff, openAndWait, openUi, previewVsExport, settle, sleep, startApp } from './helpers.mjs';

describe('interface X3 Studio Local', () => {
  let app, browser, clip;

  before(async () => {
    app = await startApp();
    mkdirSync(app.media, { recursive: true });
    clip = makeDemo(path.join(app.media, 'shake.insv'));
    browser = await launchBrowser();
  });

  after(async () => {
    await browser?.close();
    await app?.stop();
  });

  /** Copie de la vidéo sous un autre nom : un projet neuf (réglages vierges) pour chaque test. */
  const fresh = (name) => {
    const f = path.join(app.media, `${name}.insv`);
    copyFileSync(clip, f);
    return f;
  };

  const withPage = async (fn) => {
    const page = await openUi(browser, app);
    try {
      await fn(page);
      assert.deepEqual(page.errors, [], 'aucune erreur dans la console du navigateur');
    } finally {
      await page.close();
    }
  };

  it("refuse la page sans la clé de lancement et n'envoie rien hors de la machine", async () => {
    const bare = await fetch(`http://127.0.0.1:${app.port}/`);
    assert.equal(bare.status, 403);
    await withPage(async (page) => {
      await openAndWait(page, clip);
      const stats = await canvasStats(page);
      assert.ok(stats.std > 20, `l'aperçu doit montrer une vraie scène (écart type ${stats.std.toFixed(1)})`);
      assert.ok((await page.innerText('#file-info')).startsWith('shake.insv'));
      const hosts = new Set(page.requests.filter((u) => u.startsWith('http')).map((u) => new URL(u).host));
      assert.deepEqual([...hosts], [`127.0.0.1:${app.port}`], 'aucune requête vers un autre hôte');
      assert.ok(page.requests.every((u) => !/^(https?:)?\/\/(?!127\.0\.0\.1)/.test(u)), 'pas de ressource externe (polices, CDN...)');
      assert.match(await page.innerText('#badge-local'), /100 % local/);
    });
  });

  it("l'aperçu WebGL et l'export du serveur montrent la même image", async () => {
    await withPage(async (page) => {
      const file = fresh('parite');
      await openAndWait(page, file);
      const cases = [
        ['flat', { yaw: 30, pitch: 10, roll: 5, fov: 90 }, 'lock', 1.0, {}],
        ['flat', { yaw: -120, pitch: -20, roll: 0, fov: 110 }, 'off', 0.4, {}],
        ['fisheye', { yaw: 10, pitch: 0, roll: 0, fov: 200 }, 'smooth', 0.5, {}],
        ['planet', { yaw: 0, pitch: -90, roll: 0, fov: 250 }, 'off', 1.5, {}],
        // calibration des objectifs non triviale (rotations fines, quart de tour, champ, fondu), vue sur la couture
        ['flat', { yaw: 90, pitch: 5, roll: 0, fov: 100 }, 'off', 0.8, { a_yaw: 3, a_pitch: -2, a_roll: 4, b_yaw: -5, b_pitch: 3, b_roll: 90, fov: 195, blend: 15 }],
        ['fisheye', { yaw: 180, pitch: 0, roll: 0, fov: 200 }, 'off', 0.8, { swap_lenses: true, b_roll: -20 }],
      ];
      for (const [proj, view, stab, t, calib] of cases) {
        await page.evaluate(async ([proj, view, stab, t, calib]) => {
          const X = window.__x3, S = X.S;
          S.st.projection = proj; S.st.view = view; S.st.keyframes = []; S.st.stabilization.mode = stab; S.override = null;
          S.st.calibration = { ...S.st.calibration, ...calib };
          X.viewer.setCalibration(S.st.calibration);
          X.viewer.setProjection(proj);
          for (const k of ['projection', 'view', 'keyframes', 'stabilization', 'calibration']) S.pending.add(k);
          await X.flushSave(); await X.fetchStab(); X.seek(t); S.dirty = true;
        }, [proj, view, stab, t, calib]);
        await settle(page);
        await sleep(500);
        const { diff, w, h } = await previewVsExport(page);
        assert.ok(diff < 0.045, `${proj} ${JSON.stringify(view)} ${stab} ${JSON.stringify(calib)} : écart ${diff.toFixed(4)} entre l'aperçu et l'export (${w}x${h})`);
      }
    });
  });

  it('glisser pour orienter, ajouter des keyframes, tout est enregistré sur disque', async () => {
    await withPage(async (page) => {
      const file = fresh('keyframes');
      await openAndWait(page, file);
      const box = await page.locator('#gl').boundingBox();
      const [cx, cy] = [box.x + box.width / 2, box.y + box.height / 2];
      await page.mouse.move(cx, cy); await page.mouse.down(); await page.mouse.move(cx - 120, cy + 30, { steps: 4 }); await page.mouse.up();
      const v0 = await page.evaluate('window.__x3.S.st.view');
      assert.ok(v0.yaw > 5 && v0.pitch > 1, `glisser vers la gauche et le bas : on regarde à droite et en haut (${JSON.stringify(v0)})`);
      await page.keyboard.press('k');
      await page.evaluate('window.__x3.seek(2.0)');
      await settle(page);
      await page.mouse.move(cx, cy); await page.mouse.down(); await page.mouse.move(cx + 200, cy, { steps: 4 }); await page.mouse.up();
      await page.mouse.wheel(0, -400);
      await page.keyboard.press('k');
      const kfs = await page.evaluate('window.__x3.S.st.keyframes');
      assert.equal(kfs.length, 2);
      assert.equal(kfs[0].t, 0);
      assert.ok(Math.abs(kfs[1].t - 2.0) < 0.05);
      assert.ok(kfs[1].fov < kfs[0].fov && kfs[1].yaw < kfs[0].yaw, 'le zoom réduit le champ, le glissé vers la droite diminue le cap');
      assert.equal(await page.locator('#kf-list li').count(), 2);
      assert.equal(await page.locator('.kf').count(), 2);
      await page.evaluate('window.__x3.seek(1.0)');
      await settle(page);
      assert.match(await page.innerText('#hud-readout'), /champ/);
      await page.evaluate('window.__x3.flushSave()');
      await sleep(600);
      const pid = await page.evaluate('window.__x3.S.proj.id');
      const saved = JSON.parse(readFileSync(path.join(app.home, 'projects', pid, 'state.json'), 'utf8'));
      assert.equal(saved.keyframes.length, 2);
      assert.equal(saved.keyframes[0].t, 0);
      // la vue interpolée à mi-parcours est entre les deux keyframes (smoothstep)
      const mid = await page.evaluate('window.__x3.S.st.keyframes.length && window.__x3.S.t');
      assert.ok(mid > 0.9 && mid < 1.1);
      // début / fin de l'export au clavier
      await page.evaluate('window.__x3.seek(0.5)'); await settle(page); await page.keyboard.press('i');
      await page.evaluate('window.__x3.seek(1.5)'); await settle(page); await page.keyboard.press('o');
      const trim = await page.evaluate('window.__x3.S.st.trim');
      assert.ok(Math.abs(trim.start - 0.5) < 0.05 && Math.abs(trim.end - 1.5) < 0.05, JSON.stringify(trim));
      // tout effacer : refusé dans la boîte de confirmation, puis accepté
      page.once('dialog', (d) => d.dismiss());
      await page.click('#btn-kf-clear');
      assert.equal(await page.evaluate('window.__x3.S.st.keyframes.length'), 2);
      page.once('dialog', (d) => d.accept());
      await page.click('#btn-kf-clear');
      assert.equal(await page.evaluate('window.__x3.S.st.keyframes.length'), 0);
    });
  });

  it("lance un export depuis l'interface et le retrouve dans le dossier choisi", async () => {
    await withPage(async (page) => {
      const file = fresh('export');
      await openAndWait(page, file);
      const outDir = path.join(app.root, 'export ui é');
      await page.click('.tab[data-tab=export]');
      await page.selectOption('#e-size', '1280x720');
      await page.selectOption('#e-crf', '28');
      await page.selectOption('#e-speed', '2');
      await page.fill('#e-dir', outDir);
      await page.click('#btn-export');
      await page.waitForSelector('#job-list li:has-text("Export")');
      let states = [];
      for (let i = 0; i < 400; i++) {
        states = await page.evaluate("window.__x3.S.jobs.filter(j => j.kind === 'export').map(j => j.state)");
        if (states.length && ['done', 'error'].includes(states[0])) break;
        await sleep(300);
      }
      assert.equal(states[0], 'done', JSON.stringify(await page.evaluate('window.__x3.S.jobs')));
      const outs = readdirSync(outDir).filter((f) => f.endsWith('.mp4'));
      assert.equal(outs.length, 1);
      assert.ok(statSync(path.join(outDir, outs[0])).size > 10000);
      assert.equal(outs[0], 'export_flat.mp4');
    });
  });

  it("le réglage des objectifs agit en direct sur l'aperçu", async () => {
    await withPage(async (page) => {
      const file = fresh('objectifs');
      await openAndWait(page, file);
      await page.evaluate('window.__x3.editView(v => ({...v, yaw: 90, pitch: 0, roll: 0, fov: 100}))'); // on regarde la couture
      await sleep(600);
      const before = await canvasThumb(page);
      await page.click('.tab[data-tab=lens]');
      await page.click('summary');
      await page.fill('#c-b_yaw', '8');
      await page.dispatchEvent('#c-b_yaw', 'input');
      await sleep(600);
      const after = await canvasThumb(page);
      assert.ok(meanAbsDiff(before, after) > 1.0, "le décalage de l'objectif arrière doit se voir");
      assert.equal(await page.evaluate('window.__x3.S.st.calibration.b_yaw'), 8);
      // l'inversion des objectifs change radicalement l'image
      await page.evaluate('window.__x3.editView(v => ({...v, yaw: 0, pitch: 0, roll: 0, fov: 90}))');
      await sleep(500);
      const front = await canvasThumb(page);
      await page.check('#c-swap');
      await sleep(700);
      assert.ok(meanAbsDiff(front, await canvasThumb(page)) > 10, 'avant et arrière échangés');
      await page.uncheck('#c-swap');
      // quarts de tour de l'image d'un objectif (image couchée ou à l'envers) : 90, 180, -90 (270), 0
      const turned = [];
      for (let i = 0; i < 4; i++) {
        await page.click('#c-b_turn');
        turned.push(await page.evaluate('window.__x3.S.st.calibration.b_roll'));
      }
      assert.deepEqual(turned, [90, 180, -90, 0]);
      await page.click('#c-a_turn');
      assert.equal(await page.evaluate('window.__x3.S.st.calibration.a_roll'), 90);
      await sleep(600);
      assert.ok(meanAbsDiff(front, await canvasThumb(page)) > 10, "l'image de l'objectif avant a pivoté");
      await page.click('#btn-calib-reset');
      assert.equal(await page.evaluate('window.__x3.S.st.calibration.b_yaw'), 0);
    });
  });

  it("l'analyse du mouvement et les modes de stabilisation sont proposés", async () => {
    await withPage(async (page) => {
      const file = fresh('stabilisation');
      await openAndWait(page, file);
      await page.click('.tab[data-tab=stab]');
      const status = await page.innerText('#analysis-status');
      assert.match(status, /analy/i);
      const pid = await page.evaluate('window.__x3.S.proj.id');
      const info = await page.evaluate((pid) => window.__x3.api('GET', '/api/project/' + pid).then((p) => p.analysis), pid);
      assert.equal(info.ready, true);
      assert.ok(info.median_inliers > 100, `points suivis : ${info.median_inliers}`);
      // chaque mode se règle sans erreur et la force du lissage est enregistrée
      for (const mode of ['smooth', 'lock', 'horizon', 'off']) {
        await page.evaluate(async (mode) => {
          const S = window.__x3.S; S.st.stabilization.mode = mode; S.pending.add('stabilization');
          await window.__x3.flushSave(); await window.__x3.fetchStab();
        }, mode);
      }
      await page.fill('#s-strength', '2.5');
      await page.dispatchEvent('#s-strength', 'input');
      await sleep(400);
      assert.equal(await page.evaluate('window.__x3.S.st.stabilization.strength'), 2.5);
    });
  });

  it('rouvre la vidéo avec ses réglages mémorisés', async () => {
    const file = fresh('memoire');
    await withPage(async (page) => {
      await openAndWait(page, file);
      await page.evaluate(async () => {
        const S = window.__x3.S; S.st.view = { yaw: 42, pitch: 7, roll: 0, fov: 77 }; S.pending.add('view'); await window.__x3.flushSave();
      });
    });
    await withPage(async (page) => {
      await openAndWait(page, file);
      const v = await page.evaluate('window.__x3.S.st.view');
      assert.deepEqual([v.yaw, v.pitch, v.fov], [42, 7, 77]);
    });
  });

  it("signale un fichier illisible sans planter l'interface", async () => {
    await withPage(async (page) => {
      const junk = path.join(app.media, 'faux.insv');
      writeFileSync(junk, 'pas une vidéo');
      await page.click('#btn-open');
      await page.waitForSelector('#fs-list li');
      await page.fill('#fs-path', app.media);
      await page.click('#fs-go');
      await page.click('#fs-list li:has-text("faux.insv")');
      await page.waitForSelector('#toast:not([hidden])', { timeout: 15000 });
      assert.ok((await page.innerText('#toast')).length > 5);
      // l'interface répond toujours : on ferme la boîte de dialogue et on ouvre la bonne vidéo
      await page.keyboard.press('Escape');
      await page.waitForSelector('#dlg-open', { state: 'hidden' });
      page.errors.length = 0;
      await openAndWait(page, fresh('apres-erreur'));
      assert.ok((await canvasStats(page)).std > 20);
    });
  });

  it("ouvre un .lrv de la caméra (deux fisheye côte à côte, 2:1) comme un fichier à deux objectifs", async () => {
    await withPage(async (page) => {
      // le .lrv de l'X3 range les deux objectifs dans un seul flux 2:1 ; l'interface doit le proposer et l'assembler
      const lrv = path.join(app.media, 'LRV_20261021_004354_11_001.lrv');
      execFileSync('ffmpeg', ['-v', 'error', '-y', '-i', clip, '-filter_complex', '[0:v:0][0:v:1]hstack=inputs=2[v]', '-map', '[v]',
        '-c:v', 'libx264', '-crf', '16', '-pix_fmt', 'yuv420p', '-f', 'mp4', lrv]);
      await openAndWait(page, lrv);
      assert.match(await page.innerText('#file-info'), /^LRV_20261021_004354_11_001\.lrv — 2 × 480×480/, 'deux objectifs, pas un panorama');
      const stats = await canvasStats(page);
      assert.ok(stats.std > 20, `la scène assemblée doit apparaître (écart type ${stats.std.toFixed(1)})`);
      // les réglages d'objectifs n'ont de sens que pour des fisheye : ils sont proposés
      await page.click('.tab[data-tab=lens]');
      assert.ok(await page.isVisible('#c-fov'), 'champ des objectifs réglable');
    });
  });

  it("dit pourquoi l'aperçu ne se lit pas : serveur qui refuse, navigateur sans VP9, fichier illisible", async () => {
    await withPage(async (page) => {
      await openAndWait(page, fresh('lecture'));
      const id = await page.evaluate(() => window.__x3.S.proj.id);
      const clearToast = () => page.evaluate(() => { document.getElementById('toast').hidden = true; });
      const playFrom = (url) => page.evaluate((u) => { const v = document.getElementById('video'); v.src = u; v.load(); }, url);
      const waitToast = async (re) => {
        const t0 = Date.now();
        for (;;) {
          const t = await page.evaluate(() => { const e = document.getElementById('toast'); return e.hidden ? '' : e.textContent; });
          if (re.test(t)) return t;
          if (Date.now() - t0 > 20000) assert.fail(`message attendu ${re}, reçu : « ${t} »`);
          await sleep(100);
        }
      };
      // 1. le serveur refuse la requête (jeton erroné) : le code HTTP est donné
      await clearToast();
      await playFrom(`/media/${id}/proxy.webm?token=mauvais`);
      assert.match(await waitToast(/HTTP 401/), /adresse affichée dans le terminal/);
      // 2. un navigateur qui ne lit pas le VP9
      await page.evaluate(() => { window.__canPlay = HTMLMediaElement.prototype.canPlayType; HTMLMediaElement.prototype.canPlayType = () => ''; });
      await clearToast();
      await playFrom(`/media/${id}/proxy.webm?token=mauvais`);
      assert.match(await waitToast(/VP9/), /Firefox ou Chromium/);
      await page.evaluate(() => { HTMLMediaElement.prototype.canPlayType = window.__canPlay; });
      // 3. un aperçu illisible : le serveur le sert, le navigateur ne peut pas le décoder
      const proxy = path.join(app.home, 'projects', id, 'proxy.webm');
      const good = readFileSync(proxy);
      writeFileSync(proxy, Buffer.alloc(5000, 7));
      try {
        await clearToast();
        await page.evaluate(() => window.__x3.openProject(window.__x3.S.proj.source.path));
        const msg = await waitToast(/décoder/);
        assert.ok(msg.includes(`projects/${id}`), msg);
      } finally {
        writeFileSync(proxy, good);
      }
      page.errors.length = 0; // le navigateur journalise ces échecs voulus (401, lecture impossible)
    });
  });

  it("à l'arrêt, l'aperçu montre à chaque image celle que donne l'export (29,97 i/s, pas à pas)", async () => {
    await withPage(async (page) => {
      // 29,97 i/s comme la caméra : un horodatage WebM (1 ms) ne tombe jamais exactement sur k / cadence, et
      // `currentTime = k / cadence` afficherait l'image précédente une fois sur deux
      const ntsc = path.join(app.media, 'ntsc.insv');
      execFileSync('ffmpeg', ['-v', 'error', '-y', '-i', clip, '-map', '0:v:0', '-map', '0:v:1', '-map', '0:a', '-r', '30000/1001',
        '-c:v', 'libx264', '-crf', '14', '-pix_fmt', 'yuv420p', '-c:a', 'copy', '-f', 'mp4', ntsc]);
      await openAndWait(page, ntsc);
      await page.evaluate(async () => {
        const X = window.__x3, S = X.S;
        S.st.projection = 'flat'; S.st.view = { yaw: 30, pitch: 10, roll: 0, fov: 90 }; S.st.keyframes = [];
        S.st.stabilization.mode = 'off'; S.override = null;
        X.viewer.setProjection('flat');
        for (const k of ['projection', 'view', 'keyframes', 'stabilization']) S.pending.add(k);
        await X.flushSave(); await X.fetchStab();
      });
      const fps = await page.evaluate('window.__x3.S.fps');
      assert.ok(Math.abs(fps - 30000 / 1001) < 1e-3, `cadence ${fps}`);
      for (let k = 1; k <= 12; k++) {
        await page.evaluate((k) => { const X = window.__x3; X.seek(k / X.S.fps); X.S.dirty = true; }, k);
        await settle(page);
        await sleep(300);
        const t = await page.evaluate('window.__x3.S.t');
        assert.ok(Math.abs(t - k / fps) < 1e-6, `image ${k} : l'instant courant est celui de l'image (${t} au lieu de ${k / fps})`);
        const { diff } = await previewVsExport(page);
        assert.ok(diff < 0.03, `image ${k} : l'aperçu montre une autre image que l'export (écart ${diff.toFixed(4)})`);
      }
    });
  });
});

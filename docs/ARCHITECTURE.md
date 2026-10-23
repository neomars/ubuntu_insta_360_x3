# Architecture

X3 Studio Local est une crate Rust unique, `x3studio` (bibliothèque + binaire), et une interface web
(`web/`) embarquée dans le binaire. FFmpeg est le seul programme externe, et il ne fait que **décoder**
et **encoder**.

## Modules

| Module | Rôle |
|---|---|
| `media` | Sonde (`ffprobe`) : organisation du fichier (deux flux, deux fichiers `_00_`/`_10_`, fisheye côte à côte, équirectangulaire), cadence, durée, audio, signature de fin des `.insv`. Un flux 2:1 est « fisheye côte à côte » s'il porte une marque Insta360 (`.insv`, `.lrv`, signature) ou si une image montre deux cercles sur fond noir ; sinon c'est un panorama |
| `geometry` | Rotations, quaternions, interpolation de keyframes (smoothstep ou linéaire selon `ease`, cap et roulis par le plus court chemin) |
| `calibration` | Modèle des deux objectifs (champ, rotations fines, fondu, inversion) |
| `render` | Rendu direct objectifs → vue (`flat`, `fisheye`, `planet`, `equirect`), parallèle (`rayon`) |
| `grade` | Réglages d'image : tables de correction de la couleur (Y'CbCr BT.709) appliquées à l'image rendue, filtres FFmpeg de netteté et de bruit |
| `stick` | Perche à selfie / nadir : cône fixe dans le repère de la caméra, remplacé par son contour réfléchi (« miroir ») ou une couleur |
| `multi` | Plusieurs perspectives : dispositions (rectangles pairs), vues supplémentaires |
| `stabilize` | Estimation du mouvement (Shi-Tomasi, Lucas-Kanade pyramidal, Kabsch) et matrices de stabilisation (lissage, verrouillage, horizon) |
| `tracker` | Suivi d'un sujet par corrélation croisée normalisée dans des vues rectilignes stabilisées, et simplification du chemin en keyframes |
| `pipeline` | Plans d'export : commandes FFmpeg de décodage et d'encodage, une vue (`TilePlan`) par rectangle de la disposition, exécution décodeur → rendu → composition → encodeur |
| `project`, `jobs`, `app` | Projets et réglages sur disque, file de tâches, logique partagée par l'interface et la CLI |
| `server` | Serveur HTTP local (axum), sécurité, API JSON |
| `cli` | Sous-commandes `gui`, `probe`, `reframe`, `equirect`, `snapshot`, `track`, `demo` |
| `spherical` | Balises « vidéo sphérique » (Google Spherical Video V1) ajoutées à un MP4 |
| `synth` | Vidéos synthétiques à vérité terrain connue (démo et tests) |

## Conventions géométriques

Repère main droite : **x vers la droite, y vers le haut, z vers l'avant**. Angles en degrés.

* `M(yaw, pitch, roll) = Ry(yaw) · Rp(pitch) · Rr(roll)` transforme la direction d'un pixel de la *sortie*
  en la direction échantillonnée dans l'*entrée* : `d_entrée = M · d_sortie`. `yaw > 0` regarde à droite,
  `pitch > 0` regarde vers le haut.
* Matrice finale d'un pixel de sortie vers la sphère de la caméra : `M_t = T_t · V_t`, où `V_t` est la vue
  (keyframes interpolés) et `T_t = Q_t⁻¹ · Z_t` la stabilisation (`Q_t` : orientation estimée de la caméra,
  `Z_t` : orientation voulue selon le mode, orientation de l'utilisateur incluse).
* Objectifs : fisheye **équidistants**, `r = θ / (fov/2)` (le bord de l'image carrée correspond à `fov/2`) ;
  l'objectif A (flux 0) regarde vers l'avant, l'objectif B vers l'arrière (`Ry(π)`) ; `fov` par défaut 200°.
* Projections de sortie, `u, v ∈ [-1, 1]` (`v` vers le bas), `aspect = h / w`, `fov` horizontal :
  * `flat` : `a ∝ (u·t, −v·t·aspect, 1)`, `t = tan(fov/2)` ; avec une **distorsion** `d > 0` (keyframable), perspective
    générale : un rayon d'angle `θ` à l'axe arrive à `r = (d+1)·sin θ / (d + cos θ)`, normalisé pour que le bord
    horizontal (`|u| = 1`) reste à `fov/2` ; inverse en forme close `θ = atan2(r, d+1) + asin(r·d / √((d+1)² + r²))`.
    `d = 0` : rectiligne ; `d = 1` : stéréographique (la petite planète) ; `d → ∞` : orthographique. La formule
    n'existe que jusqu'à `θ_max = acos(−d)` (`d < 1`) ou `acos(−1/d)` (`d > 1`), qui borne le champ (`2·θ_max`) ;
  * `fisheye` : `θ = |(u, v·aspect)| · fov/2` ;
  * `planet` (stéréographique) : `θ = 2·atan(|(u·tan(fov/4), v·tan(fov·aspect/4))|)` ;
  * `equirect` : `lon = u·π`, `lat = −v·π/2`.

La visionneuse WebGL (`web/viewer.js`, `web/geometry.js`) applique les mêmes formules ; un test d'interface
compare l'aperçu au rendu du serveur.

### Réglages d'image

Dans l'espace Y'CbCr normalisé BT.709 (Y' de 0 à 1, Cb et Cr de −0,5 à 0,5), appliqués à l'image de sortie I420 par
une table de 256 valeurs par plan :

```
Y'  = clamp((clamp(Y)^(1/gamma) − 0,5) · contraste + 0,5 + luminosité, 0, 1)
Cb' = clamp(Cb · saturation − 0,08 · chaleur + 0,051 · teinte, −0,5, 0,5)
Cr' = clamp(Cr · saturation + 0,07 · chaleur + 0,060 · teinte, −0,5, 0,5)
```

Le shader refait ce calcul sur la couleur affichée (RVB → Y'CbCr → RVB, `grade()` dans `web/viewer.js`) : l'écart
aperçu/export est de l'ordre de 1 %. Cela suppose que les images soient en **BT.709, plage limitée** (c'est ce que
filme la caméra) : les vidéos écrites (aperçu VP9, exports) sont étiquetées `bt709`, et les captures passent par une
conversion RVB explicite (BT.709 pour le PNG, BT.709 → BT.601 pour le JPEG, que les lecteurs lisent en BT.601).
Netteté (`unsharp=5:5:1,5·s:5:5:0`) et réduction du bruit (`hqdn3d=8d:6d:12d:9d`, appliquée à chaque objectif avant
l'assemblage) sont des filtres FFmpeg : ils n'apparaissent pas dans l'aperçu.

### Perche à selfie / nadir

Dans le rendu, `d_caméra = M · rayon_sortie` est la direction **avant** les petits réglages de chaque objectif. Avec
un axe `a` (cap, inclinaison ; par défaut vers le bas) et un demi-angle `ρ`, pour `α = ∠(d, a) < ρ` :
`miroir` échantillonne la direction d'angle `2ρ − α` à l'azimut inchangé (continu au bord) ; `disque` mélange la couleur
du disque avec une opacité `1 − smoothstep(ρ(1−adoucissement), ρ, α)`. La zone est donc fixe par rapport au boîtier,
quelles que soient la vue et la stabilisation. Le shader refait le même calcul.

### Plusieurs perspectives

`multi.layout` ∈ `single`, `side`, `stack`, `pip`, `trio`, `quad` ; les rectangles sont des fractions de l'image arrondies
aux pixels pairs (`multi::layout_rects`, miroir `layoutRects` dans `web/geometry.js`, mêmes valeurs vérifiées des deux
côtés). `multi.gap` (multiple de 4) est pris sur les bords communs. Chaque vue est rendue par son propre
`LensRenderer` à sa taille, puis copiée dans l'image de sortie (fond noir) ; les réglages d'image s'appliquent à
l'image composée. La vue 1 est celle du projet ; les suivantes sont `multi.views[i]` = `{projection, view, keyframes}`.

## Moteur de rendu

Pour chaque image de sortie, `render` calcule sur une grille grossière (division 4 ou 8 de la taille de sortie)
les coordonnées dans chaque objectif et le poids de fondu, puis les interpole bilinéairement pixel par pixel
(même sémantique que `cv2.resize`). L'image est lue en **I420** (plans Y, U, V) avec interpolation bilinéaire ;
la couture est fondue en virgule fixe. Le rendu est découpé en bandes de lignes traitées en parallèle. Une
source équirectangulaire est rendue pixel par pixel avec raccord en longitude.

## Pipeline d'export

```
ffmpeg (décodeur)  ──I420 brut──▶  rendu (rayon)  ──I420 brut──▶  ffmpeg (encodeur + audio)
   thread lecteur                   thread principal                 thread écrivain
```

* Décodeur : `-ss/-t`, filtres `fps`, `format`, `hstack` ; l'échange éventuel des objectifs est fait par le
  rendu. Le décodage démarre une demi-image avant `-ss` (sinon l'image dont l'horodatage est un peu inférieur
  à la valeur arrondie de `-ss` serait perdue).
* Encodeur : `-f rawvideo`, `setpts`/`fps`/`format`, `atempo` pour le son, `libx264`/`libx265`, `+faststart`
  (sauf export 360° : `moov` doit rester à la fin pour les balises sphériques).
* Filtres FFmpeg utilisés : `scale`, `fps`, `format`, `hstack`, `split`, `crop`, `setpts`, `atempo`. Compatibles
  FFmpeg 4.4 (Ubuntu 22.04) et 6.1 (24.04). Aucun filtre `v360`/`remap`/`overlay` dans le produit.
* Les processus sont lancés sans shell, leur stderr est lu en continu (sinon ils se bloquent), ils sont tués et
  attendus dans tous les cas de sortie (annulation, erreur, panique) : jamais d'orphelin. Une sortie
  partielle est supprimée.
* Instant → image : l'export prend l'image dont l'horodatage est la plus proche de `t`. L'interface s'y place en
  visant le **milieu** de l'intervalle d'affichage de cette image (`seek` dans `web/app.js`) : le navigateur affiche
  la dernière image dont l'horodatage est ≤ `currentTime`, et un horodatage WebM (arrondi à 1 ms, décalé de +7 ms
  par FFmpeg 4.4) ne tombe jamais exactement sur `k / cadence`. À l'arrêt, l'instant courant est `k / cadence`.
* Les noms de sortie sont **réservés** (fichier créé en exclusivité) au lancement de l'export : deux exports en
  file ne visent jamais le même fichier.

## Suivi de sujet

`tracker::track_video` décode le proxy (par paquets de 90 images, vers l'avant, ou en marche arrière pour la partie
qui précède l'instant désigné), le convertit en équirectangulaire gris 960×480 comme l'analyse du mouvement, puis,
pour chaque image : (1) prévoit la direction du sujet dans le repère de la vue (position précédente + 50 % de la
vitesse) ; (2) rend une vue rectiligne 144×144 px centrée sur cette direction, en appliquant la stabilisation de
l'image ; (3) y cherche le motif (40×40 px, 0,8 × la taille du sujet donnée) par NCC zéro-moyenne dans ±36 px, avec
images intégrales pour les statistiques des fenêtres, fenêtres quasi uniformes écartées, sommet affiné par parabole ;
(4) convertit la position en direction (cap, inclinaison) ; (5) fait glisser le motif vers l'apparence courante (12 %)
quand la corrélation dépasse 0,7. Sous 0,35 pendant 6 images de suite, le sujet est perdu. Le chemin est lissé
(moyenne triangulaire, deux passes) et simplifié (Douglas-Peucker, tolérance 0,8°) en keyframes `ease: linear`.
L'enregistrement du cadrage (`web/recorder.js`) emploie le même algorithme pour tous les paramètres de la vue.

## Estimation du mouvement

Sur le proxy (assemblé en équirectangulaire gris 960×480), cinq vues rectilignes de 224 px (champ 100°) sont
extraites. Pour chaque paire d'images : jusqu'à 120 coins par vue, suivis en avant et en arrière
(Lucas-Kanade pyramidal à 4 niveaux, fenêtre 21×21), points incohérents écartés (écart aller-retour > 0,6 px,
nadir ignoré), directions 3D, rotation de Kabsch par SVD avec élimination itérative des aberrants. L'orientation
`Q_t` est cumulée en quaternions (signes continus). Le lissage est une moyenne gaussienne de quaternions
(valeurs propres de Markley). Les paires d'images sont traitées en parallèle.

## Fichiers sur disque

`$X3STUDIO_HOME` (défaut `~/.cache/x3studio`) :

```
projects/<id>/            id = 16 hexadécimaux (SHA-256 du chemin, de la taille et de la date du fichier)
  source.json             sonde du fichier
  state.json              réglages (calibration, plage, projection, vue, keyframes, stabilisation, orientation,
                          grade, stick, multi, export)
  track.json              dernier suivi de sujet : {job, params, track: {samples, lost_after, lost_before}, keyframes}
  proxy.webm              aperçu VP9
  orientation.bin         orientation estimée (binaire, versionné, vérifié à la lecture)
```

## API HTTP (127.0.0.1)

Toutes les routes `/api/*` et `/media/*` exigent le jeton (en-tête `X-X3-Token` ; `?token=` accepté pour les
lectures seulement). `Host` et `Origin` doivent désigner le serveur.

| Route | Rôle |
|---|---|
| `GET /` | page (clé de lancement ou cookie de session) ; `/app.js`, `/viewer.js`, `/geometry.js`, `/recorder.js`, `/style.css` |
| `GET /api/info` · `GET /api/fs?path=` · `POST /api/open {path}` · `POST /api/reveal {path}` | environnement, navigation de fichiers, ouverture d'une vidéo |
| `GET /api/jobs` · `GET /api/jobs/jN` · `POST /api/jobs/jN/cancel` | tâches |
| `GET /api/project/<id>` · `PUT .../state` · `POST .../prepare` · `POST .../analyze` | projet, réglages, aperçu, analyse |
| `GET .../stab?mode&strength&heading&yaw&pitch&roll&ref` | matrices de stabilisation : `N × 9` flottants `f32` little-endian, en-têtes `X-Frames`, `X-Fps` |
| `POST .../export {settings, directory}` · `POST .../snapshot {t, format, directory, settings}` | export vidéo (tâche), capture d'image |
| `GET .../frame?t&w&h` | image JPEG de la vue (aperçu image par image) |
| `POST .../track {t, yaw, pitch, size?, start?, end?, backward?}` · `GET .../track` | suivi d'un sujet (tâche `track`) · résultat du dernier suivi avec ses keyframes |
| `GET /media/<id>/proxy.webm` | aperçu vidéo (requêtes `Range`) |

Les erreurs sont `{"error": "message"}` : 400 pour une erreur de l'utilisateur (paramètres, fichier illisible),
401/403 pour la sécurité, 404, 413 (corps de plus de 4 Mio), 500 pour le reste.

## Tests

* Unitaires (`src/**`) : formules, cas limites, valeurs de référence de la version Python (`tests/golden/`).
* Intégration (`tests/*.rs`, ffmpeg requis) : vidéos synthétiques à vérité terrain ; export complet, mises en
  page des fichiers, stabilisation, serveur (sécurité, API, plages, tâches), CLI (binaire réel, Ctrl+C),
  fichiers abîmés, balises sphériques.
* Interface (`tests/ui`) : Chromium sans écran, WebGL logiciel, pilotant le vrai serveur ; parité aperçu/export
  (y compris réglages d'image, perche, plusieurs vues, keyframes enregistrés ou suivis), keyframes au pointeur et au
  clavier, export, calibration en direct, aucune requête hors de `127.0.0.1` ; `recorder.test.mjs` teste la
  simplification des tracés sans navigateur. La vidéo de test comporte au besoin du bruit, une perche magenta liée à
  la caméra et un sujet mobile (`synth`), de vérité terrain connue.

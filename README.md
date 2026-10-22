# X3 Studio Local

Traitement **100 % local** des vidéos de l'**Insta360 X3** sur Ubuntu, dans l'esprit de l'application
mobile Insta360 (import, recadrage 360° par keyframes, stabilisation, projections créatives, export).
Aucune donnée ne quitte votre machine : pas de compte, pas de cloud, pas de télémétrie. Le serveur de
l'interface n'écoute que sur `127.0.0.1`.

```
┌──────────────────────────────── navigateur (interface locale) ───────────────────────────────┐
│  visionneuse 360° WebGL · timeline + keyframes · stabilisation · calibration · export       │
└───────────────▲──────────────────────────────────────────────────────────────────────────────┘
                │ API JSON locale (jeton + clé de session)
┌───────────────┴────────────── x3studio (Python) ─────────────────────────────────────────────┐
│  sonde .insv · assemblage des 2 fisheye · estimation du mouvement · rendu OpenCV/NumPy        │
└───────────────▲──────────────────────────────────────────────────────────────────────────────┘
                │ pipes
             FFmpeg (décodage HEVC/H.264, filtre v360, encodage H.264/H.265, audio)
```

## Fonctionnalités

| Application Insta360 | X3 Studio Local |
|---|---|
| Import des `.insv` (deux objectifs) | Détection automatique : deux flux dans un fichier (X3, X2, ONE R), deux fichiers `_00_`/`_10_` (ONE X), fisheye côte à côte, ou vidéo 360° déjà assemblée |
| Assemblage 360° | Fisheye → équirectangulaire avec fondu de la couture ; calibration par objectif ajustable en direct |
| Recadrage (« Reframe ») | Caméra virtuelle : cap, inclinaison, roulis, champ de vision, **keyframes** avec interpolation douce, déplaçables sur la timeline |
| Perspectives | Plat (rectiligne), fisheye, **petite planète** |
| FlowState / horizon | Stabilisation par analyse d'image : **lissage**, **horizon à plat** (cap lissé), **verrouillage** de la direction ; correction manuelle de l'horizon |
| Montage | Début/fin d'export, vitesse ×0,25 à ×8, cadence, son conservé |
| Export | Vidéo plate 16:9, 9:16, 4:5, 1:1, 720p/1080p/4K ; **vidéo 360° 5,7K/4K/3K** avec balises « vidéo sphérique » (YouTube, VLC…) ; H.264 ou H.265 ; capture d'image JPEG |

## Installation (Ubuntu 22.04 / 24.04)

### Paquet .deb (recommandé)

```bash
git clone https://github.com/neomars/ubuntu_insta_360_x3 && cd ubuntu_insta_360_x3
sudo apt install build-essential debhelper dh-sequence-python3 pybuild-plugin-pyproject \
                 python3-all python3-setuptools fakeroot
./packaging/build-deb.sh                          # -> dist/x3studio_0.1.0_all.deb
sudo apt install ./dist/x3studio_0.1.0_all.deb    # tire ffmpeg, python3-numpy et python3-opencv
```

Le paquet (`Architecture: all`) installe `/usr/bin/x3studio`, le module Python dans
`/usr/lib/python3/dist-packages/x3studio/`, une entrée de menu « X3 Studio Local » avec son icône, la page de
manuel (`man x3studio`) et la documentation. Dépendances : `ffmpeg`, `python3-numpy` ; recommandés :
`python3-opencv` (stabilisation et rendu rapide), `xdg-utils`. Désinstallation : `sudo apt remove x3studio`
(les caches et réglages de `~/.cache/x3studio` et les exports restent à vous).

### Sans paquet

```bash
./install.sh          # apt : ffmpeg, python3-numpy, python3-opencv ; venv ; commande `x3studio`
```

Ou, depuis le dépôt : `sudo apt install ffmpeg python3-numpy python3-opencv` puis `python3 -m x3studio`.
OpenCV est facultatif mais recommandé : sans lui il n'y a ni stabilisation ni moteur de rendu rapide
(l'export se fait alors uniquement avec FFmpeg, bien plus lentement dès que la vue bouge).

## Utilisation

```bash
x3studio                       # ouvre l'interface dans votre navigateur
x3studio mon_clip.insv         # ... directement sur un fichier
x3studio demo essai.insv       # génère un .insv synthétique pour essayer sans caméra
```

Dans l'interface : **Ouvrir une vidéo** → l'aperçu et l'analyse du mouvement se préparent une fois
(quelques secondes à quelques minutes selon la durée) → orientez la vue en glissant, `K` ajoute un
keyframe, `I`/`O` fixent le début/la fin → onglet **Export**.

Ligne de commande (traitement par lot, scripts) :

```bash
x3studio probe clip.insv
x3studio reframe clip.insv -o plat.mp4 --size 1920x1080 --yaw 30 --fov 100 --stabilize horizon
x3studio reframe clip.insv -o planete.mp4 --projection planet --pitch -90 --size 1080x1080
x3studio reframe clip.insv -o suivi.mp4 --keyframes keyframes.json --stabilize smooth --strength 1.5
x3studio equirect clip.insv -o clip_360.mp4 --stabilize lock --codec h265 --crf 24
x3studio snapshot clip.insv -o image.jpg --t 12.5 --yaw -45 --fov 90
```

`keyframes.json` : `[{"t": 0, "yaw": 0, "pitch": 0, "roll": 0, "fov": 90}, {"t": 4.5, "yaw": 80, "pitch": 10, "fov": 70}]`
(`t` en secondes dans la vidéo source ; angles en degrés ; `--dry-run` affiche les commandes sans les exécuter).

Les réglages d'un projet sont enregistrés dans `~/.cache/x3studio/` (aperçu, analyse, keyframes…) ; les
exports vont dans `~/Vidéos/X3Studio` (modifiable dans l'interface, ou `X3STUDIO_OUTPUT`).

## Comment ça marche

1. **Aperçu** : un proxy WebM léger (deux fisheye 960 px côte à côte) est écrit une fois. La visionneuse WebGL
   fait l'assemblage et la projection *dans le navigateur*, en direct (calibration, stabilisation et vue se
   règlent sans réencoder).
2. **Mouvement** : sur le proxy, des points sont suivis sur cinq vues rectilignes (avant, droite, arrière,
   gauche, haut), convertis en directions 3D ; une rotation (Kabsch + rejet des aberrants) est estimée entre
   images consécutives. L'orientation de la caméra obtenue sert à lisser (moyenne gaussienne de quaternions),
   verrouiller ou mettre l'horizon à plat. Aucun gyroscope n'est nécessaire.
3. **Export** : deux moteurs aux résultats équivalents (testés l'un contre l'autre).
   * `cv` (défaut dès que la vue bouge) : FFmpeg décode → NumPy/OpenCV calcule, pour chaque image, la rotation
     puis lit directement les deux fisheye (`cv2.remap`, tables interpolées sur une grille grossière, fondu de la
     couture) → FFmpeg encode. Évite l'image 360° intermédiaire et les recalculs de `v360` (≈ 0,1 s par
     commande et par image).
   * `v360` : un seul graphe FFmpeg (`v360` + `sendcmd`). Sans OpenCV, ou pour un simple assemblage statique.

Conventions géométriques (axes, ordre de rotation `ypr`, projections) : `x3studio/geometry.py`, vérifiées
contre FFmpeg dans `tests/`.

## Performances

Mesurées sur un conteneur à 4 cœurs, avec une vidéo synthétique de la taille de l'X3 (2 × 2880², H.264) :

| Tâche | Vitesse |
|---|---|
| Aperçu + analyse du mouvement | ≈ 24 images/s |
| Export 1080p plat avec keyframes + stabilisation | ≈ 10–14 images/s |
| Export 4K plat | ≈ 6 images/s |
| Export 360° 5,7K tourné/stabilisé | ≈ 4 images/s (`v360` seul : 0,4) |

Le décodage HEVC réel de l'X3 est plus coûteux que le H.264 de ce test. Pas d'accélération GPU pour l'instant.

## Limites connues — à lire

* **Non validé sur un vrai fichier X3 :** je n'avais pas de fichier de la caméra. Tout a été vérifié sur des
  vidéos synthétiques à géométrie connue (mêmes conventions que `v360`). Les valeurs par défaut — champ de
  chaque objectif **200°**, flux 0 = avant, flux 1 = arrière, image circulaire remplissant le carré — sont des
  hypothèses courantes. Si la couture est visible ou l'orientation fausse, ouvrez l'onglet **Objectifs** : champ,
  rotations fines par objectif, inversion avant/arrière et affichage de la zone de fondu se règlent en direct
  (typiquement 195–210°).
* **Stabilisation par l'image, pas par le gyroscope :** robuste quand la scène a de la texture ; moins fiable de
  nuit, en ciel uni ou avec un sujet occupant tout le champ. L'onglet Stabilisation indique la fiabilité de
  l'analyse. L'horizon est celui de la première image (corrigeable à la main) ; les données gyro/accéléromètre
  du `.insv` ne sont pas encore exploitées.
* Vidéo uniquement (pas les `.insp`), 8 bits (les modes 10 bits sont convertis), pas de HDR, son stéréo, projection
  commune à tous les keyframes, un seul clip à la fois (pas de montage multi-clips).
* L'aperçu requiert un navigateur lisant le VP9 (Firefox, Chrome, Chromium) et WebGL.

## Confidentialité et sécurité

* Traitement local : FFmpeg et OpenCV tournent sur votre machine ; l'interface n'a aucune dépendance externe
  (ni police, ni CDN, ni script tiers — vérifié par un test).
* Le serveur écoute sur `127.0.0.1` seulement ; contrôle de l'en-tête `Host` (anti « DNS rebinding »), refus des
  origines étrangères, jeton aléatoire par lancement pour l'API, clé de session dans l'URL de la page d'accueil.
  À utiliser sur une machine personnelle (les arguments de la commande d'ouverture du navigateur sont visibles
  des autres comptes locaux).

## Développement

```bash
pip install -e '.[dev]'     # numpy, opencv, pytest, playwright (Chromium pour les tests d'interface)
pytest                      # ≈ 2 min : géométrie, assemblage, moteurs, stabilisation, serveur, CLI, interface
pytest -m "not ui"          # sans navigateur
```

Les tests génèrent leurs vidéos (`x3studio/synth.py`) : scène sphérique texturée, caméra qui tremble avec une
vérité terrain connue, deux fisheye écrits comme deux flux d'un `.insv`.

## Pistes

Gyroscope/accéléromètre du `.insv` (horizon automatique), accélération GPU, projection par keyframe, suivi d'un
sujet, montage multi-clips, photos `.insp`.

## Licence

MIT

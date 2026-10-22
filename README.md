# X3 Studio Local

[![Tests](https://github.com/neomars/ubuntu_insta_360_x3/actions/workflows/ci.yml/badge.svg)](https://github.com/neomars/ubuntu_insta_360_x3/actions/workflows/ci.yml)

Traitement **100 % local** des vidéos de l'**Insta360 X3** sur Ubuntu, dans l'esprit de l'application
mobile Insta360 (import, recadrage 360° par keyframes, stabilisation, projections créatives, export).
Aucune donnée ne quitte votre machine : pas de compte, pas de cloud, pas de télémétrie. Le serveur de
l'interface n'écoute que sur `127.0.0.1`.

Écrit en **Rust** : un seul binaire, `ffmpeg` pour seule dépendance (il ne fait que décoder et encoder).

```
┌──────────────────────────────── navigateur (interface locale) ───────────────────────────────┐
│  visionneuse 360° WebGL · timeline + keyframes · stabilisation · calibration · export       │
└───────────────▲──────────────────────────────────────────────────────────────────────────────┘
                │ API JSON locale (jeton + clé de session)
┌───────────────┴────────────── x3studio (Rust) ────────────────────────────────────────────────┐
│  sonde .insv · assemblage des 2 fisheye · estimation du mouvement · rendu parallèle (rayon)   │
└───────────────▲──────────────────────────────────────────────────────────────────────────────┘
                │ pipes d'images brutes
             FFmpeg (décodage HEVC/H.264, encodage H.264/H.265, audio)
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
| Export | Vidéo plate 16:9, 9:16, 4:5, 1:1, 720p/1080p/4K ; **vidéo 360° 5,7K/4K/3K** avec balises « vidéo sphérique » (YouTube, VLC…) ; H.264 ou H.265 ; capture d'image JPEG ou PNG |

## Installation (Ubuntu 22.04 / 24.04, amd64)

### Paquet .deb (recommandé)

Téléchargez `x3studio_<version>_amd64.deb` dans les [Releases](https://github.com/neomars/ubuntu_insta_360_x3/releases),
puis :

```bash
sudo apt install ./x3studio_*_amd64.deb      # tire ffmpeg
```

Le paquet installe `/usr/bin/x3studio`, une entrée de menu « X3 Studio Local » avec son icône, la page de
manuel (`man x3studio`) et la documentation. Il est construit sur Ubuntu 22.04 (glibc 2.35) et s'installe donc
aussi sur 24.04. Désinstallation : `sudo apt remove x3studio` (les caches de `~/.cache/x3studio` et vos
exports restent à vous). Une somme SHA-256 (`SHA256SUMS`) accompagne chaque release.

### Depuis les sources

```bash
sudo apt install ffmpeg build-essential           # + Rust : https://rustup.rs
git clone https://github.com/neomars/ubuntu_insta_360_x3 && cd ubuntu_insta_360_x3
cargo build --release                             # -> target/release/x3studio
./target/release/x3studio demo essai.insv         # essayer sans caméra
./packaging/build-deb.sh                          # ou construire le .deb (cargo-deb) -> dist/
```

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
Toutes les options : `x3studio --help`, `x3studio reframe --help`, `man x3studio`.

Les réglages d'un projet sont enregistrés dans `~/.cache/x3studio/` (aperçu, analyse, keyframes…) ; les
exports vont dans `~/Vidéos/X3Studio` (modifiable dans l'interface, avec `--output-dir`, ou `X3STUDIO_OUTPUT`).
Un export ne remplace jamais un fichier existant : `_2`, `_3`… sont ajoutés au nom.

## Comment ça marche

1. **Aperçu** : un proxy WebM léger (deux fisheye 960 px côte à côte) est écrit une fois. La visionneuse WebGL
   fait l'assemblage et la projection *dans le navigateur*, en direct (calibration, stabilisation et vue se
   règlent sans réencoder).
2. **Mouvement** : sur le proxy, les deux objectifs sont assemblés en une image équirectangulaire grise ; des
   points (Shi-Tomasi) sont suivis (Lucas-Kanade pyramidal, contrôle aller-retour) sur cinq vues rectilignes
   (avant, droite, arrière, gauche, haut), convertis en directions 3D ; une rotation (Kabsch + rejet des
   aberrants) est estimée entre images consécutives. L'orientation de la caméra obtenue sert à lisser (moyenne
   gaussienne de quaternions), verrouiller ou mettre l'horizon à plat. Aucun gyroscope n'est nécessaire, et
   aucune bibliothèque de vision externe : tout est dans le binaire.
3. **Export** : FFmpeg décode les deux objectifs → le moteur de rendu calcule, pour chaque image, la rotation
   (vue, stabilisation, horizon) puis lit directement les deux fisheye (tables de correspondance interpolées sur
   une grille grossière, interpolation bilinéaire, fondu de la couture, parallélisme sur tous les cœurs) →
   FFmpeg encode. Pas d'image 360° intermédiaire, et les trois étapes s'exécutent en parallèle.

Conventions géométriques, formats et API : [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Performances

Mesurées sur un conteneur à 4 cœurs, avec une vidéo synthétique de la taille de l'X3 (2 × 2880², H.264, 90 images) :

| Tâche | Vitesse |
|---|---|
| Aperçu WebM (VP9) | ≈ 22 images/s |
| Analyse du mouvement | ≈ 30 images/s |
| Export 1080p plat avec keyframes + stabilisation (x264 `veryfast`) | ≈ 12 images/s |
| Export 4K plat | ≈ 5 images/s |
| Export 360° 5,7K stabilisé | ≈ 3 images/s |

Le rendu seul est plus rapide (1080p : ≈ 30 ms par image ; 4K : ≈ 85 ms) ; le décodage et l'encodage se partagent les
mêmes cœurs, donc plus de cœurs = plus vite. Le décodage HEVC réel de l'X3 est plus coûteux que le H.264 de ce
test. Pas d'accélération GPU pour l'instant.

## Limites connues — à lire

* **Non validé sur un vrai fichier X3 :** je n'avais pas de fichier de la caméra. Tout a été vérifié sur des
  vidéos synthétiques à géométrie connue. Les valeurs par défaut — champ de chaque objectif **200°**, flux 0 =
  avant, flux 1 = arrière, image circulaire remplissant le carré — sont des hypothèses courantes. Si la couture
  est visible ou l'orientation fausse, ouvrez l'onglet **Objectifs** : champ, rotations fines par objectif,
  inversion avant/arrière et affichage de la zone de fondu se règlent en direct (typiquement 195–210°).
* **Stabilisation par l'image, pas par le gyroscope :** robuste quand la scène a de la texture ; moins fiable de
  nuit, en ciel uni ou avec un sujet occupant tout le champ. L'onglet Stabilisation indique la fiabilité de
  l'analyse. L'horizon est celui de la première image (corrigeable à la main) ; les données gyro/accéléromètre
  du `.insv` ne sont pas encore exploitées.
* Vidéo uniquement (pas les `.insp`), 8 bits (les modes 10 bits sont convertis), pas de HDR, son stéréo, projection
  commune à tous les keyframes, un seul clip à la fois (pas de montage multi-clips).
* L'aperçu requiert un navigateur lisant le VP9 (Firefox, Chrome, Chromium) et WebGL.

## Confidentialité et sécurité

* Traitement local : FFmpeg et le moteur Rust tournent sur votre machine ; l'interface n'a aucune dépendance
  externe (ni police, ni CDN, ni script tiers — vérifié par un test).
* Le serveur écoute sur `127.0.0.1` seulement ; contrôle de l'en-tête `Host` (anti « DNS rebinding »), refus des
  origines étrangères, jeton aléatoire par lancement pour l'API, clé de session dans l'URL de la page d'accueil
  puis cookie `HttpOnly; SameSite=Strict`. À utiliser sur une machine personnelle (les arguments de la commande
  d'ouverture du navigateur sont visibles des autres comptes locaux).
* Les sous-processus (`ffmpeg`) sont lancés sans shell ; les valeurs reçues de l'interface (codec, préréglage,
  tailles…) sont validées avant d'atteindre leur ligne de commande.

## Développement

```bash
cargo test                       # ≈ 2 min : unitaires + intégration (vidéos synthétiques, ffmpeg requis)
cargo clippy --all-targets -- -D warnings && cargo fmt --check
cd tests/ui && npm ci && npx playwright install chromium && npm test    # interface dans un vrai navigateur
```

Les tests génèrent leurs vidéos (`src/synth.rs`) : scène sphérique texturée, caméra qui tremble avec une vérité
terrain connue, deux fisheye écrits comme deux flux d'un `.insv`. Les vues exportées sont comparées à ce que doit
montrer la scène (formules réécrites dans `tests/common`), l'orientation estimée à la vraie, et l'aperçu WebGL à
l'export du serveur. `tests/golden/` contient des valeurs de référence calculées par la première version
(Python) du logiciel.

Organisation : `src/` (moteur : `render`, `stabilize`, `pipeline`, `server`, `cli`…), `web/` (interface, embarquée
dans le binaire), `tests/` (intégration, `tests/ui` : navigateur), `packaging/` (.deb), `.github/workflows/`.

## Publier une version

Le workflow **Build** construit le `.deb` (tests, lintian, installation d'essai) et le joint à une release GitHub :

```bash
git tag v0.2.0 && git push origin v0.2.0     # tag sur le dernier commit de main
```

Une préversion (`v0.2.0-beta.1`) reste marquée « pre-release ». Voir l'en-tête de `.github/workflows/build.yml`
pour le lancement manuel (ajouter un `.deb` à une release existante, ou simple essai).

## Pistes

Gyroscope/accéléromètre du `.insv` (horizon automatique), accélération GPU, projection par keyframe, suivi d'un
sujet, montage multi-clips, photos `.insp`.

## Licence

MIT

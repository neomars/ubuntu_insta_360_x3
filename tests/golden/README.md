# Vecteurs de référence

Ces fichiers JSON ont été produits par la **première version de X3 Studio Local, écrite en Python**
(commit `f956e3f`), qui sert de spécification exécutable au moteur Rust. Les tests Rust les comparent
au calcul du moteur (`cargo test`) : ils verrouillent les formules, les valeurs par défaut et les cas
limites.

| Fichier | Contenu | Test |
|---|---|---|
| `render.json` | images rendues (flat, fisheye, planète, équirectangulaire ; avec/sans objectifs échangés) | `src/render.rs::matches_the_python_reference` |
| `stabilize.json` | lissage de quaternions et matrices de stabilisation (tous les modes) | `src/stabilize.rs::matches_the_python_reference` |
| `project.json` | état par défaut d'un projet et fusion profonde des réglages | `src/project.rs` |

Pour les régénérer (rarement utile : le Python n'est plus maintenu), voir l'en-tête des scripts de
`tools/golden/` — ils lisent la version Python depuis un `git worktree` du commit de référence.

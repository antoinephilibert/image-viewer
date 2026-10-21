# vu

Une visionneuse d'images minimaliste pour Linux, écrite en Rust. On ouvre, on navigue, on regarde. Rien d'autre.

`vu` ne sait ni éditer, ni supprimer, ni renommer. Elle fait une seule chose et vise à la faire vite : l'interface ne doit jamais figer, même dans un dossier de plusieurs milliers de photos de 20 Mo.

## Points forts

- **Rapide par construction.** Aucun décodage, aucune lecture de dossier et aucune réduction d'image ne se fait sur le thread de l'interface. Un pool de workers traite les requêtes selon une file de priorités.
- **Navigation sans à-coups.** Les images voisines (n±1, puis n±2) sont préchargées. Quand la sélection bouge vite, les requêtes obsolètes sont abandonnées avant ou après décodage, sans créer de texture inutile.
- **Vignettes partagées.** Le cache disque suit la [spécification freedesktop](https://specifications.freedesktop.org/thumbnail-spec/latest/) (`~/.cache/thumbnails/large/`). Une vignette déjà générée par un autre programme est réutilisée, et celles de `vu` le sont par les autres.
- **Décodage JPEG réduit.** Les vignettes de JPEG passent par `libjpeg-turbo` (réduction par DCT) : environ 5 fois plus rapide que décoder l'image entière puis la réduire (voir [Performances](#performances)).
- **Zoom fluide.** Molette centrée sur le curseur, déplacement à la souris, taille réelle au pixel près (HiDPI compris).
- **SVG net à tout zoom.** Les SVG sont rastérisés à la taille d'affichage et re-rastérisés quand le zoom se stabilise.
- **Wayland natif**, avec un `app_id` (`vu`) pour les règles de fenêtre du compositeur.

## Installation

### Prérequis

- Une chaîne Rust récente (édition 2024).
- `libjpeg-turbo` avec `libturbojpeg` et ses en-têtes, ainsi que `pkg-config`.
- Une session **Wayland**. `vu` n'est pas compilée avec le support X11.

Paquets usuels pour `libturbojpeg` :

| Distribution | Paquet |
|---|---|
| Debian, Ubuntu | `libturbojpeg0-dev` |
| Fedora | `turbojpeg-devel` |
| Arch | `libjpeg-turbo` |
| Void | `libjpeg-turbo-devel` |

### Compilation

```sh
git clone <url-du-depot> vu
cd vu
cargo install --path .
```

Utilise toujours le profil *release* (c'est celui de `cargo install`) pour juger de la fluidité : en *debug*, la réduction d'image est nettement plus lente.

## Utilisation

```sh
vu                  # ouvre le dossier courant
vu ~/Images         # ouvre ce dossier
vu photo.jpg        # ouvre le dossier parent et sélectionne photo.jpg
```

La fenêtre s'affiche avant tout décodage.

### Formats

JPEG, PNG, WebP, GIF (première image seulement), BMP, TIFF et SVG. L'orientation EXIF est appliquée. La transparence s'affiche sur le fond neutre, sans damier.

### Le panneau de gauche

Deux modes, on bascule avec `Tab`.

- **Arbre** (par défaut) : la racine est le dossier d'ouverture. Seuls les dossiers et les images supportées sont affichés, dossiers d'abord, puis tri naturel insensible à la casse (`img2` avant `img10`). Le contenu d'un dossier n'est lu que lorsqu'on le déplie.
- **Vignettes** : grille des images du dossier courant. Le nombre de colonnes suit la largeur du panneau, et seules les vignettes visibles (plus une marge) sont demandées.

### Raccourcis

| Touche | Action |
|---|---|
| `↑` `↓` | Élément précédent / suivant (arbre ou grille) |
| `←` `→` | Arbre : replier / déplier. Grille : vignette précédente / suivante |
| `Entrée` | Déplier / replier un dossier |
| `Espace`, `Maj+Espace` | Image suivante / précédente du dossier, quel que soit le mode |
| `Tab` | Basculer arbre ↔ vignettes |
| `Retour arrière`, ou clic sur `..` | Re-raciner l'arbre sur le dossier parent |
| `.` | Afficher / masquer les fichiers cachés |
| `f` | Ajuster à la fenêtre |
| `1` | Taille réelle (100 %) |
| `+` `-` | Zoom avant / arrière |
| `F11` | Plein écran (masque le panneau et la barre d'état) |
| `F5` | Relire le dossier courant |
| `q` | Quitter |
| `Échap` | Quitter le plein écran, ou quitter hors plein écran |

À la souris : la molette zoome autour du curseur, le glisser déplace l'image, le double-clic bascule entre ajusté et 100 %.

## Performances

Mesure sur une image JPEG synthétique de 20 Mpx (5472 × 3648), profil *release*, un seul cœur :

| Chemin | Durée par vignette |
|---|---|
| Décodage complet puis réduction | ~180 ms |
| `libjpeg-turbo` (décodage réduit par DCT) | ~33 ms |

L'image est plus lisse qu'une vraie photo : les temps réels seront un peu plus élevés, mais le rapport entre les deux chemins devrait rester du même ordre. Une vignette déjà en cache se relit en moins d'une milliseconde.

Pour refaire la mesure sur ta machine :

```sh
VU_BENCH_DIR=/tmp cargo test --release bench_jpeg -- --ignored --nocapture
```

## Architecture

```
src/
  main.rs     ligne de commande, ouverture de la fenêtre
  app.rs      état de l'application, boucle egui, panneaux et clavier
  tree.rs     arbre paresseux et tri naturel
  thumbs.rs   grille, cache disque freedesktop (PNG 256 px, clé MD5 de l'URI)
  loader.rs   workers, file de priorités, générations, caches LRU
  viewer.rs   zoom et déplacement (fonctions pures, testées)
  formats.rs  détection par extension, décodage raster, JPEG turbo, SVG
```

Le flux entre l'interface et le loader :

1. Un changement de sélection incrémente la **génération** et émet une requête pour l'image courante et ses voisines.
2. Les workers dépilent par priorité : image courante, voisines, vignettes visibles, marge.
3. Un résultat dont l'image n'est plus voulue est jeté. Sinon il est renvoyé par un canal, suivi de `ctx.request_repaint()`.
4. L'interface crée la texture et l'insère dans un cache LRU : environ 5 textures pleine taille, et un budget de 192 Mo pour les vignettes.

Stack : [`eframe`/`egui`](https://github.com/emilk/egui), [`image`](https://crates.io/crates/image), [`fast_image_resize`](https://crates.io/crates/fast_image_resize), [`resvg`](https://crates.io/crates/resvg), [`turbojpeg`](https://crates.io/crates/turbojpeg), [`png`](https://crates.io/crates/png).

## Développement

```sh
cargo run --release -- <dossier>
cargo test
cargo clippy --all-targets
```

## Hors périmètre

Volontairement absents : édition, suppression, renommage, diaporama, panneau EXIF, fichier de configuration, surveillance du système de fichiers.

## Limites connues

- Wayland uniquement.
- Les SVG qui référencent des images externes ne sont pas garantis.
- Pas de fichier de configuration : les réglages sont dans le code.

## Licence

Distribué sous licence [Apache-2.0](LICENSE).

## Avertissement

Ce projet a été écrit avec [Claude Code](https://claude.com/claude-code), l'assistant de programmation d'Anthropic, à partir d'un cahier des charges rédigé par l'auteur. Le code est testé (tests unitaires, `clippy` sans avertissement) mais il est fourni tel quel, sans garantie : relis-le avant de lui faire confiance.

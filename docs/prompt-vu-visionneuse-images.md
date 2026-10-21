# Projet : `vu`, visionneuse d'images minimaliste en Rust

## Contexte
- Machine : Void Linux, compositeur Wayland niri (pas de XWayland), GPU AMD.
- Installation via `cargo install --path .` (pas via Nix).
- Contrôle de version : Jujutsu (`jj`), jamais git. Un changement jj par étape livrée.
- Code, identifiants et messages de commit en anglais. Commentaires et documentation en français.

## Objectif
Une visionneuse d'images rapide et belle, sans aucune fonction d'édition. On ouvre, on navigue, on regarde. La performance est le critère numéro un : l'interface ne doit jamais figer, même dans un dossier de 3000 photos de 20 Mo.

## Ligne de commande
- `vu` : ouvre le dossier courant.
- `vu <dossier>` : ouvre ce dossier.
- `vu <fichier>` : ouvre le dossier parent et sélectionne ce fichier.
- La fenêtre doit apparaître en moins de 100 ms, avant tout décodage.

## Stack
- `eframe`/`egui` (backend wgpu ou glow, Wayland natif via winit). Définir un `app_id` Wayland (`vu`) pour que les window rules de niri fonctionnent.
- Crate `image` (features : jpeg, png, webp, gif, bmp, tiff) pour le décodage.
- `resvg` pour le SVG.
- `fast_image_resize` pour les réductions (SIMD).
- Icônes : `egui-phosphor` ou équivalent.
- Profils Cargo : release avec `lto = "fat"`, `codegen-units = 1`, `opt-level = 3`. En dev, `[profile.dev.package."*"] opt-level = 3` pour que les décodeurs restent rapides en debug.

## Mise en page
- Panneau gauche redimensionnable, image à droite (zone centrale).
- Barre d'état discrète en bas : nom du fichier, dimensions, poids, position (ex. 12/340), niveau de zoom.
- Thème sombre soigné : espacements cohérents, sélection bien visible, aucune bordure inutile. Fond neutre foncé derrière l'image.

## Panneau gauche : deux modes, bascule avec `Tab`

### Mode arbre (par défaut)
- Racine = dossier d'ouverture. Une entrée `..` (ou `Backspace`) re-racine l'arbre sur le dossier parent.
- N'affiche que les dossiers et les images supportées. Fichiers cachés masqués, `.` pour les afficher.
- Tri : dossiers d'abord, puis tri naturel insensible à la casse (img2 avant img10).
- Chargement paresseux : le contenu d'un dossier n'est lu qu'à son dépliage, et la lecture se fait hors du thread UI.
- Sélectionner une image l'affiche immédiatement à droite (aperçu au fil de la navigation clavier).

### Mode vignettes
- Grille des images du dossier courant (dossier de l'image sélectionnée, ou dossier sélectionné). Nom du dossier en en-tête.
- Le nombre de colonnes s'adapte à la largeur du panneau. L'élément sélectionné est mis en évidence et reste visible (auto-scroll).
- Seules les vignettes visibles (plus une marge) sont demandées au loader.

## Clavier
- `↑`/`↓` : élément précédent/suivant dans le panneau (arbre ou grille).
- Arbre : `→` déplie, `←` replie (ou remonte au parent), `Entrée` déplie/replie un dossier.
- Grille : flèches dans les quatre directions.
- `Espace` / `Shift+Espace` : image suivante/précédente du dossier courant, peu importe le mode.
- `f` : ajuster à la fenêtre. `1` : taille réelle (100 %). `+`/`-` : zoom.
- `F11` : plein écran, masque le panneau et la barre d'état.
- `F5` : relire le dossier courant.
- `q` ou `Échap` (hors plein écran) : quitter.
- Souris : molette = zoom centré sur le curseur, glisser = déplacer l'image, double-clic = basculer ajusté / 100 %.

## Affichage de l'image
- Ajustée à la fenêtre par défaut, jamais agrandie au-delà de 100 % en mode ajusté.
- Orientation EXIF appliquée (`ImageDecoder::orientation()` + `apply_orientation` de la crate `image`).
- Si l'image dépasse la taille maximale de texture du GPU, la réduire au décodage.
- SVG : rastériser à la taille d'affichage, re-rastériser quand le zoom change (avec un léger debounce).
- GIF : première frame seulement.
- Transparence : afficher sur le fond neutre, sans damier.

## Performance (exigences)
- Aucun décodage, aucune lecture de dossier, aucune réduction sur le thread UI. Pool de workers (ex. `rayon` ou threads dédiés) qui renvoient les résultats par channel, suivi de `ctx.request_repaint()`.
- File de priorités : image courante > préchargement des voisines (n±1, puis n±2) > vignettes visibles > reste.
- Annulation : chaque requête porte un numéro de génération. Quand la sélection bouge vite, les requêtes obsolètes sont abandonnées avant ou après décodage, sans jamais créer de texture inutile.
- Caches LRU en mémoire : textures pleine taille (environ 5) et textures de vignettes (budget en Mo, pas en nombre).
- Vignettes sur disque selon la spec freedesktop (`~/.cache/thumbnails/large/`, PNG 256 px, clé MD5 de l'URI `file://`, métadonnées `Thumb::URI` et `Thumb::MTime`). Une vignette existante et à jour est lue au lieu d'être régénérée; c'est partagé avec les autres applications.
- Mesurer avant d'optimiser davantage. Si le décodage JPEG pleine taille reste le goulot pour les vignettes, évaluer `turbojpeg` (décodage réduit par DCT) et me présenter le compromis (dépendance C) avant de l'adopter.

## Hors périmètre
Édition, suppression, renommage, diaporama, panneau EXIF, fichier de configuration, surveillance du système de fichiers. Ne pas les ajouter, même partiellement.

## Structure suggérée
- `main.rs` : CLI
- `app.rs` : état et boucle egui
- `tree.rs` : arbre et tri
- `thumbs.rs` : grille et cache disque
- `loader.rs` : workers, priorités, générations, caches LRU
- `viewer.rs` : zoom et pan
- `formats.rs` : détection par extension, raster et SVG

## Livraison par étapes
1. Fenêtre, CLI, arbre paresseux, affichage d'une image (synchrone accepté à cette étape seulement).
2. Loader asynchrone, priorités, générations, préchargement des voisines.
3. Zoom, pan, ajustement, plein écran, barre d'état.
4. Mode vignettes et cache disque freedesktop.
5. SVG, finition visuelle.

À chaque étape : `cargo clippy` sans avertissement, tests unitaires pour ce qui est pur (tri naturel, clé de cache, file de priorités, calcul de zoom), puis un changement jj. Avant l'étape 1, présente un plan court des types principaux et du flux de messages entre l'UI et le loader.

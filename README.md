# gh_package_finder

Outil Rust de découverte de `package.json` publics sur GitHub et de détection de
dependency confusion. Architecture modulaire :

- `main.rs` — CLI, orchestration des 3 modes
- `db.rs` — schéma et accès SQLite (WAL, occurrences, cache npm, curseur crawl...)
- `github.rs` — API GitHub (recherche, listing, CDN raw)
- `npm.rs` — vérifications parallélisées sur le registre npm
- `analysis.rs` — extraction de dépendances, parsing lockfiles/.npmrc, rapport
- `analysis_queue.rs` — file d'analyse en continu, pool de threads configurable

## Compilation

```
cargo build --release
```

Nécessite `libssl-dev`/`pkg-config` (TLS natif) — `sqlite` est compilé en bundle
automatiquement (feature `bundled` de `rusqlite`).

## Les quatre modes

### 1. Recherche + téléchargement

```
./gh_package_finder token.txt
./gh_package_finder token.txt --analyse --threads 12
```

### 2. Crawl exhaustif

```
./gh_package_finder --crawl token.txt
./gh_package_finder --crawl --analyse --threads 16 token.txt
```

Énumère tous les dépôts publics via `/repositories?since=ID`, sonde chaque racine via le
CDN `raw.githubusercontent.com` (hors quota API). Reprend automatiquement (curseur en
base). L'ordre des arguments n'a pas d'importance.

### 3. Analyse seule / rattrapage

```
./gh_package_finder --analyse
./gh_package_finder --analyse --threads 4
```

### 4. Analyse d'un package.json local (`--file`)

```
./gh_package_finder --file chemin/vers/package.json
```

Mode entièrement local : ne touche ni à GitHub (aucun token requis) ni à la base SQLite
du crawl. Utile pour tester rapidement un projet en local ou dans une CI.

- Extrait les dépendances (`dependencies`/`devDependencies`/`peerDependencies`/
  `optionalDependencies`) avec les mêmes règles de filtrage que les autres modes
  (auto-référence, noms invalides, specs non installables comme `file:`/`workspace:`/
  `git+`...).
- Sonde `package-lock.json`, `yarn.lock`, `pnpm-lock.yaml` (présence uniquement, non
  parsé — même limitation qu'ailleurs) et `.npmrc` **dans le même dossier**, en lecture
  disque directe (pas de requête réseau pour ça, contrairement aux autres modes qui
  passent par le CDN GitHub).
- Vérifie chaque nom sur le registre npm public, puis applique les mêmes réductions de
  faux positifs que le reste de l'outil : scope confirmé privé via `.npmrc`, et scope npm
  déjà revendiqué par un tiers (voir sections dédiées plus bas).
- Écrit `dependency_confusion_report_local.csv` (même format que le rapport principal) et
  affiche un résumé console coloré.
- **Non couvert par ce mode**, par nature (pas de dépôt GitHub associé) : score de
  sensibilité (`repo_sensitivity_score`/`_label` restent vides dans le CSV), détection
  monorepo/poly-repo, brouillon de divulgation responsable, et notification Telegram
  (`--notify`/`[notify]` sont ignorés ou rejetés dans ce mode).
- Équivalent en configuration TOML (`--config fichier.toml`) :
  ```toml
  mode = "file"
  file_path = "chemin/vers/package.json"
  ```

## Architecture de l'analyse : file continue plutôt que passes périodiques

Chaque `package.json` téléchargé (recherche ou crawl) est immédiatement envoyé dans une
**file de tâches** (`std::sync::mpsc`), consommée par un **pool de threads dédié**
(paramétrable via `--threads N`, défaut 8), complètement découplé du téléchargement :
le crawl/la recherche ne sont jamais ralentis par les vérifications npm, et les
vérifications npm ne sont jamais bloquées en attendant le prochain lot de téléchargement.

Chaque thread du pool, pour chaque tâche reçue :
1. Lit et parse le `package.json` sur disque, extrait les dépendances.
2. **Persiste immédiatement** ces occurrences dans la table `dependency_occurrences` —
   avant même la vérification npm. Si le programme s'arrête à cet instant précis, le
   travail de parsing n'est pas perdu.
3. Détermine les noms de paquets pas encore vérifiés (un `HashSet` partagé en mémoire,
   protégé par mutex, garantit qu'un même nom n'est jamais vérifié deux fois par deux
   threads en parallèle).
4. Vérifie ces noms sur npm, persiste le résultat dans `npm_registry_cache`, et affiche une
   **alerte en temps réel** dans la console dès qu'un paquet absent de npm est détecté
   (`[ALERTE dependency confusion] ...`) — pas besoin d'attendre la fin d'une passe.
5. Marque le `package.json` comme analysé (`packages.analyzed = 1`).

### Résilience à l'arrêt (le point clé de cette version)

Toute l'architecture est pilotée par l'état persistant en base, pas par de la mémoire
volatile :
- Un `package.json` téléchargé est écrit sur disque + son enregistrement SQLite créé avec
  `analyzed = 0` **avant** d'être mis en file. Si le programme est tué (Ctrl+C, crash...)
  alors que la tâche est encore dans la file (jamais consommée) ou en cours de traitement,
  rien n'est perdu : au redémarrage, **toute exécution** (recherche, crawl ou `--analyse`)
  recherche d'abord les `package.json` avec `analyzed = 0` et les remet en file avant de
  reprendre son activité normale.
- Chaque étape (extraction → persistée, vérification npm → persistée, marquage analysé)
  est validée indépendamment : une interruption entre deux étapes ne fait perdre que
  l'étape non encore validée, jamais celles d'avant.
- Mode **WAL** activé sur la base SQLite : un `--analyse` peut être lancé dans un second
  terminal pendant qu'un `--crawl` tourne toujours dans le premier, sans verrou mutuel.

### `--analyse` devient un export, pas un calcul

Grâce à cette persistance continue, `--analyse` (une fois le rattrapage des tâches en
attente terminé) ne fait plus qu'une chose : **interroger la base** et écrire le CSV — zéro
appel réseau si tout est déjà traité. Testé : 60ms sur un jeu de données déjà analysé,
contre plusieurs secondes/minutes avec l'ancienne passe périodique qui reparsait et
revérifiait tout à chaque fois.

## Optimisations réseau (inchangées, cumulées avec ce qui précède)

1. **CDN raw en priorité** (`raw.githubusercontent.com`, hors quota API) pour tout
   téléchargement de contenu, avec repli sur l'API contents en cas d'échec.
2. **Sondage opportuniste** des lockfiles et `.npmrc` (gratuit en quota API).
3. **Crawl découplé** du rate limit `/search` (30 req/min) — `/repositories?since=` traite
   ~100 dépôts par appel API, le reste passe par le CDN.
4. Dédoublonnage, cache SQLite, requêtes HEAD sur npm, parallélisation, reprise sur
   interruption — inchangés.

## Mode `--analyse` : détection de dependency confusion

Signaux croisés pour prioriser les vrais positifs :

- **Absence sur le registre npm public** (`registry.npmjs.org`).
- **Lockfile `resolved`** vers une URL hors npmjs.org (`package-lock.json`, `yarn.lock` —
  `pnpm-lock.yaml` est téléchargé mais pas parsé sémantiquement, limitation assumée).
- **Scope `.npmrc`** configuré vers un registre privé.

Le rapport CSV (`dependency_confusion_report.csv`) inclut une colonne `confidence`
(élevée si confirmé par lockfile/.npmrc, moyenne sinon) et `lockfile_resolved_url`.

## Limites connues

- `pnpm-lock.yaml` : fetché mais pas parsé.
- `yarn.lock` : parseur simple, couverture non garantie à 100% sur Yarn Berry (v2+).
- `--crawl` ne sonde que la racine de chaque dépôt (pas les sous-dossiers) ; le mode
  recherche reste nécessaire en complément pour les monorepos.
- Un fork est ignoré par défaut en mode `--crawl`.
- Le `total_count` de `/search/code` reste une estimation GitHub, pas un nombre exact.

## Sortie colorée

Dès qu'un paquet potentiellement vulnérable à la dependency confusion est détecté :

- **Alerte en temps réel** (dès qu'un thread d'analyse la détecte, pas besoin d'attendre
  un rapport) en **rouge vif** : `[ALERTE dependency confusion] "nom" absent de npm — ...`
- **Résumé final** (`generate_report`) : le nombre de paquets suspects s'affiche en **rouge**
  s'il y en a, en **vert** s'il n'y en a aucun. Chaque paquet listé est coloré selon sa
  confiance :
  - **rouge vif** : confirmé par un lockfile ou un scope `.npmrc` privé (vulnérabilité
    probable)
  - **jaune** : absent de npm mais sans confirmation (à vérifier manuellement)

La couleur se désactive automatiquement quand la sortie n'est pas un vrai terminal
(redirection vers un fichier, pipe vers `grep`/`less`, etc. — testé) pour ne pas polluer
les logs, et respecte la convention `NO_COLOR=1` (testé) si vous voulez la désactiver
explicitement même dans un terminal.

## Réduction des faux positifs

Deux sources fréquentes de faux positifs sont maintenant filtrées :

### 1. Résolution monorepo/workspace

Si une dépendance "absente de npm" correspond en fait au **nom déclaré d'un autre
package.json du même dépôt** (ex: `packages/app` dépend de `@acme/ui-kit`, qui est lui-même
défini dans `packages/ui-kit/package.json` du même repo), c'est très probablement un
package de workspace résolu localement — pas un vrai risque. Ce cas est **exclu** du
rapport final.

Si le nom correspond à un package.json d'un **autre dépôt de la même organisation
GitHub** (poly-repo), c'est un signal plus faible : il est **gardé** mais avec une
confiance dédiée `faible (nom vu dans un autre dépôt du même org — probable poly-repo)`
plutôt qu'exclu, car moins certain qu'un vrai monorepo.

**Nuance sur l'alerte temps réel** : l'alerte `[ALERTE dependency confusion]` s'affiche dès
qu'un thread termine sa vérification npm pour UN fichier, sans attendre que tous les
`package.json` du même dépôt aient été traités — elle peut donc mentionner un nom qui sera
ensuite exclu du rapport final une fois le reste du monorepo indexé. Le rapport CSV /
`--analyse` reste la source fiable, recalculée à chaque fois à partir de l'état complet en
base.

### 2. Fichiers non représentatifs (templates, fixtures, exemples)

Un `package.json` dont le chemin contient un segment comme `template(s)`, `boilerplate`,
`scaffold`, `starter`, `example(s)`, `sample(s)`, `demo(s)`, `fixture(s)`, `mock(s)`,
`stub(s)` est **entièrement ignoré** (aucune dépendance n'en est extraite) : ces fichiers
appartiennent typiquement à des générateurs de projet ou de la documentation, avec des noms
de paquets fictifs/placeholders.

Volontairement **exclu** de cette liste : `test`/`tests`/`spec`/`e2e` — un dossier de tests
peut contenir un vrai package.json avec de vraies dépendances internes à risque, ce n'est
pas un signal fiable de contenu fictif contrairement aux templates.

### 3. Auto-référence

Un package qui se liste lui-même comme dépendance (son propre champ `"name"` apparaissant
dans `dependencies`/`devDependencies`/etc., rencontré dans certains monorepos/outillages de
build) est ignoré.

Ces trois filtres sont couverts par des tests unitaires (`cargo test`).

### 4. Validation stricte des noms de paquets npm

Certains `package.json` non standards (dépendances vendorisées committées, dossiers de
dédoublonnage npm/pnpm sous `node_modules`, exports d'outillage interne...) contiennent des
clés qui ne sont pas de vrais noms de paquets, par exemple `_eslint-scope@4.0.3@eslint-scope`
— un artefact de dossier `node_modules` dédupliqué, pas une dépendance déclarée par un
humain. Ces clés génèrent systématiquement un faux "absent de npm" puisqu'elles ne
correspondent à rien de toute façon.

Une validation inspirée des règles réelles de npm (`validate-npm-package-name`) rejette
maintenant tout nom qui n'est pas structurellement valide (majuscules, espaces, préfixe `.`
ou `_`, multiples `@`, scope malformé, etc.) avant même de le vérifier sur le registre.

En complément, `node_modules`, `bower_components`, `.pnpm`, `.yarn`, `vendor` ont été ajoutés
à la liste des segments de chemin exclus : un `package.json` trouvé à l'intérieur d'un de
ces dossiers est un artefact de dépendances déjà installées, jamais le manifeste réel du
projet analysé.

## Vérification de revendication de scope npm (réduction de faux positifs supplémentaire)

Un paquet **scoped** (`@scope/nom`) ne peut être publié sur npm que par le titulaire du
scope. Si `@scope` est déjà revendiqué par quelqu'un (org ou compte utilisateur), un
attaquant externe ne peut PAS publier dessous, même si `@scope/nom` précis n'existe pas
encore — ce n'est donc pas un risque exploitable aujourd'hui.

L'outil vérifie maintenant, pour chaque paquet scoped signalé "absent de npm", si son
**scope** est revendiqué (via `HEAD https://registry.npmjs.org/-/org/{scope}/package`,
mis en cache en base comme les autres vérifications) :

- **Scope non revendiqué** → risque réel, confirmé : n'importe qui peut créer ce scope
  gratuitement aujourd'hui et y publier le paquet. Alerte rouge + notification (si
  `--notify` actif).
- **Scope déjà revendiqué** → risque très réduit : affiché en jaune comme information
  seulement, **exclu du décompte de vulnérabilités** et du rapport CSV principal (mais
  compté séparément pour rester transparent sur ce qui a été filtré).

Testé en conditions réelles : `@babel/un-faux-plugin-inexistant` (scope `@babel` bien
revendiqué) → exclu ; `@scope-totalement-libre-999/x` (scope non revendiqué) → conservé.

## Option `--notify` : alertes Telegram en temps réel

```
./gh_package_finder token.txt --analyse --notify telegram.txt
./gh_package_finder --crawl --analyse --notify telegram.txt token.txt
```

Fichier de configuration (`telegram.txt`) : deux lignes obligatoires (le token du bot puis
l'ID du chat), et une troisième ligne optionnelle (score de sensibilité minimum, 0-10,
défaut **4/10** — voir section suivante).

```
123456789:AAExxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
-1001234567890
5
```

Pour créer un bot Telegram : parler à [@BotFather](https://t.me/BotFather), commande
`/newbot`, récupérer le token fourni. Pour le chat ID : ajouter le bot à un groupe/canal,
ou lui parler en privé, puis consulter `https://api.telegram.org/bot<TOKEN>/getUpdates`
pour lire l'ID.

Une notification est envoyée **uniquement pour les découvertes réellement exploitables** —
c'est-à-dire les mêmes cas qui déclenchent l'alerte rouge en console (paquet non scoped
absent de npm, ou paquet scoped dont le **scope** n'est pas revendiqué). Les cas à risque
réduit (scope déjà revendiqué par un tiers) ne déclenchent **pas** de notification, pour
éviter le bruit.

### Filtrage par sensibilité du dépôt

Au-delà de l'exploitabilité, chaque découverte est aussi filtrée par le **score de
sensibilité** du dépôt concerné (`sensitivity.rs` : étoiles, forks, activité récente —
0 à 10, voir `score_label` pour le barème complet). Seules les découvertes sur un dépôt
dont le score est **supérieur ou égal** au seuil configuré (`min_sensitivity`, défaut
**4/10 = "modéré"**) déclenchent une notification Telegram :

- Configurable via la 3ᵉ ligne du fichier `--notify` (`telegram.txt` ci-dessus), ou via
  `min_sensitivity` dans le bloc `[notify]` d'un fichier `--config` TOML.
- `0` désactive le filtrage (tout ce qui est exploitable notifie, comme avant l'ajout de
  cette option). `10` restreint aux dépôts au score maximal uniquement.
- Une découverte sous le seuil n'est **jamais perdue** : elle reste affichée en console
  (`[notify] Alerte retenue en console uniquement : ...`), reste dans le rapport CSV, et
  génère toujours son brouillon de divulgation dans `disclosures/` — seule la notification
  Telegram en temps réel est filtrée.
- Le score étant calculé de façon asynchrone (appel API GitHub, mis en cache), une panne
  ou un rate-limit ponctuel sur cet appel ne bloque **jamais** une notification : en cas
  d'échec de récupération du score, l'outil notifie quand même par prudence plutôt que de
  risquer de faire disparaître silencieusement une vraie alerte.

Un échec d'envoi (token invalide, réseau indisponible...) est loggé mais ne fait jamais
planter le programme — testé en conditions réelles avec un token invalide : la requête
atteint bien l'API Telegram (confirmé par une réponse 403 de leur serveur), l'erreur est
affichée proprement et le programme continue normalement.

**Limite à connaître** : je n'ai pas pu tester une livraison Telegram réussie de bout en
bout dans mon environnement de test (accès réseau restreint à certains domaines), mais la
construction de la requête HTTP (endpoint, méthode POST, champs de formulaire) suit
exactement l'API officielle Telegram Bot (`sendMessage`) et a été validée jusqu'à la
réponse du vrai serveur Telegram. À tester avec un vrai token de ton côté pour confirmer la
réception.

## Divulgation responsable (dossier `disclosures/`)

Pour chaque vulnérabilité **réellement retenue** dans le rapport final (après exclusion des
faux positifs monorepo/scope-déjà-revendiqué), l'outil génère automatiquement un brouillon
de message prêt à envoyer, dans `disclosures/{dépôt}__{paquet}.txt`.

Le message (en anglais, ces dépôts appartenant le plus souvent à des tiers dans le monde
entier) décrit le risque de dependency confusion, propose une remédiation, et indique
comment contacter les mainteneurs :

- Si un `SECURITY.md` est détecté (via l'API "community profile" de GitHub) : son URL est
  citée directement.
- Sinon : un lien vers le signalement privé de vulnérabilité de GitHub
  (`https://github.com/{repo}/security/advisories/new`, disponible par défaut, visible
  uniquement des mainteneurs).
- Si la vérification elle-même échoue (rate limit, réseau) : le message le dit
  explicitement plutôt que d'affirmer à tort l'absence de politique de sécurité — **bug
  découvert et corrigé pendant les tests** (un rate-limit anonyme faisait initialement
  passer une vérification ratée pour un "SECURITY.md confirmé absent", y compris sur des
  dépôts qui en ont réellement un comme `nodejs/node`). Ce résultat indéterminé n'est
  jamais mis en cache, il est donc automatiquement retenté au prochain lancement.

**L'outil ne publie jamais rien lui-même** — il ne fait que préparer l'information et le
message pour que l'envoi reste un choix humain, adressé au bon interlocuteur.

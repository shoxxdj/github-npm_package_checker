mod analysis;
mod analysis_queue;
mod colors;
mod config;
mod db;
mod disclosure;
mod github;
mod notify;
mod npm;
mod sensitivity;

use analysis_queue::{AnalysisQueue, AnalysisTask};
use notify::TelegramConfig;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;
use ureq::Agent;

const PER_PAGE: u32 = 100;
// L'API de recherche de code GitHub plafonne à 1000 résultats (10 pages de 100) PAR requête.
const MAX_PAGES: u32 = 10;
const DB_PATH: &str = "gh_package_finder.db";
const REPORT_PATH: &str = "dependency_confusion_report.csv";
const LOCAL_REPORT_PATH: &str = "dependency_confusion_report_local.csv";
const CRAWL_PAGE_SIZE_HINT: &str = "100 dépôts/appel";
// Nombre de threads d'analyse par défaut (voir --threads N pour ajuster).
const DEFAULT_ANALYSIS_THREADS: usize = 8;

// GitHub n'indexe que les fichiers de moins de 384 Ko (393216 octets).
// On découpe la recherche en tranches de taille de fichier (qualifieur "size", en octets)
// pour contourner le plafond de 1000 résultats d'UNE SEULE requête.
const SIZE_BUCKETS: &[&str] = &[
    "size:<200",
    "size:200..499",
    "size:500..999",
    "size:1000..1999",
    "size:2000..4999",
    "size:5000..9999",
    "size:10000..19999",
    "size:20000..49999",
    "size:50000..99999",
    "size:100000..199999",
    "size:200000..393216",
];

#[derive(Default)]
struct Stats {
    downloaded_new: u32,
    downloaded_updated: u32,
    skipped_unchanged: u32,
    skipped_duplicate: u32,
    errors: u32,
}

fn read_token(path: &str) -> Result<String, Box<dyn std::error::Error>> {
    let mut content = String::new();
    fs::File::open(path)?.read_to_string(&mut content)?;
    Ok(content.trim().to_string())
}

fn sanitize_folder_name(full_name: &str) -> String {
    full_name.replace('/', "_")
}

fn local_repo_dir(full_name: &str) -> PathBuf {
    Path::new("repositories").join(sanitize_folder_name(full_name))
}

// Ouvre la base et met en file toutes les tâches restées non analysées d'une exécution
// précédente (fichier déjà téléchargé, mais interrompu avant que son analyse ne débute ou
// ne se termine) : c'est ce qui garantit qu'aucun arrêt du programme ne fait perdre de
// travail, tout étant piloté par l'état persistant en base.
fn requeue_pending_analysis(conn: &rusqlite::Connection, queue: &AnalysisQueue) -> rusqlite::Result<usize> {
    let pending = db::unanalyzed_packages(conn)?;
    let count = pending.len();
    for (repo_full_name, path, repo_url) in pending {
        queue.submit(AnalysisTask { repo_full_name, path, repo_url });
    }
    Ok(count)
}

// =====================================================================================
// Mode par défaut : recherche via /search/code + téléchargement (raw CDN prioritaire)
// =====================================================================================

fn run_search_query(
    agent: &Agent,
    token: &str,
    query: &str,
    conn: &rusqlite::Connection,
    seen: &mut HashSet<(String, String)>,
    stats: &mut Stats,
    analysis_queue: Option<&AnalysisQueue>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut page: u32 = 1;

    loop {
        if page > MAX_PAGES {
            println!(
                "  Limite de {} pages atteinte pour cette tranche (plafond API de 1000 résultats)",
                MAX_PAGES
            );
            break;
        }

        println!("  Recherche page {} pour \"{}\"...", page, query);

        let search_result = match github::search_code_page(agent, token, query, PER_PAGE, page)? {
            Some(r) => r,
            None => {
                stats.errors += 1;
                break;
            }
        };

        if page == 1 {
            println!(
                "  {} résultats trouvés pour cette tranche (max {} téléchargeables)",
                search_result.total_count,
                MAX_PAGES * PER_PAGE
            );
        }

        if search_result.items.is_empty() {
            println!("  Plus aucun résultat pour cette tranche, passage à la suivante.");
            break;
        }

        for item in &search_result.items {
            // Le qualifieur "filename:" de GitHub fait du matching flou (ex: il retourne aussi
            // "package.json.bak", "package.json.md", etc.). On ne garde que les correspondances exactes.
            let is_exact_match = Path::new(&item.path)
                .file_name()
                .map(|n| n == "package.json")
                .unwrap_or(false);
            if !is_exact_match {
                continue;
            }

            let dedup_key = (item.repository.full_name.clone(), item.path.clone());
            if !seen.insert(dedup_key) {
                stats.skipped_duplicate += 1;
                continue;
            }

            let existing_sha = db::known_sha(conn, &item.repository.full_name, &item.path)?;
            if existing_sha.as_deref() == Some(item.sha.as_str()) {
                stats.skipped_unchanged += 1;
                println!(
                    "Déjà en base et inchangé, ignoré : {}/{}",
                    item.repository.full_name, item.path
                );
                continue;
            }
            let is_update = existing_sha.is_some();

            let repo_dir = local_repo_dir(&item.repository.full_name);
            let file_path: PathBuf = repo_dir.join(&item.path);

            let parent_dir = match file_path.parent() {
                Some(p) => p,
                None => {
                    eprintln!("Chemin invalide pour {}: {}", item.repository.full_name, item.path);
                    stats.errors += 1;
                    continue;
                }
            };
            if let Err(e) = fs::create_dir_all(parent_dir) {
                eprintln!("Impossible de créer le dossier {}: {}", parent_dir.display(), e);
                stats.errors += 1;
                continue;
            }

            sleep(Duration::from_millis(150));

            // Téléchargement : CDN raw en priorité (hors quota API), repli sur l'API
            // "contents" uniquement si le CDN échoue.
            let content = github::fetch_file_content(
                agent,
                token,
                &item.repository.full_name,
                &item.path,
                &item.url,
            );

            let Some(bytes) = content else {
                eprintln!(
                    "Téléchargement impossible (raw + repli API) pour {}/{}",
                    item.repository.full_name, item.path
                );
                stats.errors += 1;
                continue;
            };

            if let Err(e) = fs::write(&file_path, &bytes) {
                eprintln!("Écriture impossible pour {}: {}", file_path.display(), e);
                stats.errors += 1;
                continue;
            }

            let repo_url = item.repository.url();
            if let Err(e) = db::upsert_package(
                conn,
                &item.repository.full_name,
                &item.path,
                &repo_url,
                &item.sha,
                &db::now_iso8601(),
            ) {
                eprintln!("Écriture en base impossible pour {}: {}", item.repository.full_name, e);
                stats.errors += 1;
                continue;
            }

            if is_update {
                stats.downloaded_updated += 1;
                println!("[maj] -> {}", file_path.display());
            } else {
                stats.downloaded_new += 1;
                println!("[nouveau] -> {}", file_path.display());
            }

            // Sondage opportuniste des lockfiles et .npmrc voisins (gratuit en quota API,
            // reste dans le thread principal car déjà très léger).
            analysis::probe_sibling_files(agent, conn, &item.repository.full_name, &item.path, &repo_dir);

            // La partie coûteuse (extraction + vérifications npm) part dans la file
            // d'analyse, traitée par le pool de threads en parallèle du téléchargement.
            if let Some(queue) = analysis_queue {
                queue.submit(AnalysisTask {
                    repo_full_name: item.repository.full_name.clone(),
                    path: item.path.clone(),
                    repo_url: repo_url.clone(),
                });
            }
        }

        page += 1;
        // Pause entre deux pages de recherche (limite stricte de 30 req/min sur /search)
        sleep(Duration::from_secs(2));
    }

    Ok(())
}

fn run_search_download(
    token: &str,
    also_analyse: bool,
    threads: usize,
    telegram: Option<TelegramConfig>,
) -> Result<(), Box<dyn std::error::Error>> {
    let agent = github::build_agent()?;
    if let Some(cfg) = &telegram {
        notify::send_startup_notification(&agent, cfg, "recherche + téléchargement (/search/code)");
    }
    let conn = db::init_db(DB_PATH)?;
    println!("Base SQLite : {}", DB_PATH);

    let queue = if also_analyse {
        let analysis_conn = db::init_db(DB_PATH)?;
        let q = analysis_queue::spawn_workers(&agent, analysis_conn, threads, telegram, token.to_string());
        let requeued = requeue_pending_analysis(&conn, &q)?;
        println!(
            "File d'analyse démarrée sur {} thread(s) ({} tâche(s) en attente rattrapées).",
            threads, requeued
        );
        Some(q)
    } else {
        None
    };

    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut stats = Stats::default();

    println!(
        "Recherche en {} tranches de taille de fichier pour maximiser la couverture...",
        SIZE_BUCKETS.len()
    );

    for (idx, bucket) in SIZE_BUCKETS.iter().enumerate() {
        let query = format!("filename:package.json {}", bucket);
        println!("\n=== Tranche {}/{} : {} ===", idx + 1, SIZE_BUCKETS.len(), bucket);
        if let Err(e) = run_search_query(&agent, token, &query, &conn, &mut seen, &mut stats, queue.as_ref()) {
            eprintln!("Erreur sur la tranche \"{}\": {}", bucket, e);
        }
    }

    println!(
        "\nTéléchargement terminé. {} nouveaux, {} mis à jour, {} inchangés, {} doublons ignorés, {} erreurs.",
        stats.downloaded_new, stats.downloaded_updated, stats.skipped_unchanged, stats.skipped_duplicate, stats.errors
    );

    if let Some(queue) = queue {
        println!("Attente de la fin du traitement de la file d'analyse...");
        queue.shutdown_and_join();
        generate_report(&conn, &agent, token)?;
    }
    Ok(())
}

// =====================================================================================
// Mode --crawl : parcours exhaustif de /repositories?since=, au-delà de l'index de
// recherche (qui n'est ni exhaustif ni garanti à jour). Chaque dépôt ne coûte qu'un appel
// au listing (mutualisé sur 100 dépôts) + des requêtes raw.githubusercontent.com HORS
// quota API. Reprend automatiquement où il s'était arrêté grâce au curseur en base.
// =====================================================================================

// Fréquence (en nombre de lots de ~100 dépôts) à laquelle le rapport CSV est régénéré à
// partir de la base pendant un --crawl combiné à --analyse. La file d'analyse tourne en
// continu (les vérifications npm ne sont plus jamais mises en pause) ; seule la
// régénération du fichier CSV lui-même est périodique, par souci de lisibilité en continu.
const REPORT_REFRESH_EVERY_N_BATCHES: u64 = 10;

fn run_crawl(
    token: &str,
    also_analyse: bool,
    threads: usize,
    telegram: Option<TelegramConfig>,
) -> Result<(), Box<dyn std::error::Error>> {
    let agent = github::build_agent()?;
    if let Some(cfg) = &telegram {
        notify::send_startup_notification(&agent, cfg, "crawl exhaustif (/repositories?since=)");
    }
    let conn = db::init_db(DB_PATH)?;
    println!("Base SQLite : {}", DB_PATH);
    println!("Mode crawl exhaustif ({} par appel de listing).", CRAWL_PAGE_SIZE_HINT);

    let queue = if also_analyse {
        let analysis_conn = db::init_db(DB_PATH)?;
        let q = analysis_queue::spawn_workers(&agent, analysis_conn, threads, telegram, token.to_string());
        let requeued = requeue_pending_analysis(&conn, &q)?;
        println!(
            "File d'analyse démarrée sur {} thread(s) en parallèle du crawl ({} tâche(s) rattrapées).",
            threads, requeued
        );
        Some(q)
    } else {
        None
    };

    let mut since_id = db::get_crawl_cursor(&conn)?;
    println!("Reprise à partir de l'ID de dépôt {}", since_id);

    let mut total_scanned: u64 = 0;
    let mut total_found: u32 = 0;
    let mut batch_num: u64 = 0;

    loop {
        let repos = github::list_repositories_since(&agent, token, since_id)?;
        if repos.is_empty() {
            println!("Fin du listing des dépôts publics (ou erreur réseau). Arrêt.");
            break;
        }

        batch_num += 1;
        total_scanned += repos.len() as u64;

        for repo in &repos {
            if repo.fork {
                continue; // les forks dupliquent en général le contenu du dépôt d'origine
            }

            let head_etag = match github::raw_head_etag(&agent, &repo.full_name, "package.json") {
                Some(e) => e,
                None => continue, // pas de package.json à la racine, ou dépôt inaccessible
            };
            let etag_str = head_etag.clone().unwrap_or_default();

            let existing = db::known_sha(&conn, &repo.full_name, "package.json")?;
            if existing.as_deref() == Some(etag_str.as_str()) {
                continue; // déjà connu et inchangé
            }

            let Some(result) = github::raw_get(&agent, &repo.full_name, "package.json") else {
                continue;
            };

            let repo_dir = local_repo_dir(&repo.full_name);
            let file_path = repo_dir.join("package.json");
            if let Some(parent) = file_path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if fs::write(&file_path, &result.body).is_err() {
                continue;
            }

            let repo_url = format!("https://github.com/{}", repo.full_name);
            db::upsert_package(&conn, &repo.full_name, "package.json", &repo_url, &etag_str, &db::now_iso8601())?;
            println!("[crawl] -> {}", file_path.display());
            total_found += 1;

            analysis::probe_sibling_files(&agent, &conn, &repo.full_name, "package.json", &repo_dir);

            if let Some(queue) = &queue {
                queue.submit(AnalysisTask {
                    repo_full_name: repo.full_name.clone(),
                    path: "package.json".to_string(),
                    repo_url,
                });
            }
        }

        let last_id = repos.last().map(|r| r.id).unwrap_or(since_id);
        since_id = last_id;
        db::set_crawl_cursor(&conn, since_id)?;

        if batch_num % 10 == 0 {
            println!(
                "Progression : {} dépôts scannés, {} package.json trouvés, curseur = {}",
                total_scanned, total_found, since_id
            );
        }

        if also_analyse && batch_num % REPORT_REFRESH_EVERY_N_BATCHES == 0 {
            if let Err(e) = generate_report(&conn, &agent, token) {
                eprintln!("Erreur pendant la régénération du rapport : {}", e);
            } else {
                println!("(Rapport {} rafraîchi — file d'analyse toujours active)", REPORT_PATH);
            }
        }
    }

    if let Some(queue) = queue {
        println!("Attente de la fin du traitement de la file d'analyse...");
        queue.shutdown_and_join();
        generate_report(&conn, &agent, token)?;
    }

    println!(
        "\nTerminé (ou interrompu). {} dépôts scannés, {} package.json trouvés/mis à jour. Curseur sauvegardé : {}.",
        total_scanned, total_found, since_id
    );
    Ok(())
}

// =====================================================================================
// Mode --analyse : rattrape toute tâche non traitée via un pool de threads temporaire,
// puis génère le rapport à partir de l'état déjà persisté en base (aucune reparsing/
// revérification npm des paquets déjà connus — c'est justement tout l'intérêt de la file
// d'analyse continue : ce mode devient quasi instantané une fois le rattrapage terminé).
// =====================================================================================

fn run_analysis(threads: usize, telegram: Option<TelegramConfig>) -> Result<(), Box<dyn std::error::Error>> {
    let agent = github::build_agent()?;
    if let Some(cfg) = &telegram {
        notify::send_startup_notification(&agent, cfg, "analyse (--analyse)");
    }
    let conn = db::init_db(DB_PATH)?;
    println!("Base SQLite : {}", DB_PATH);

    let rows = db::all_packages(&conn)?;
    if rows.is_empty() {
        println!("Aucun package.json en base. Lancez d'abord une recherche/téléchargement ou --crawl.");
        return Ok(());
    }

    let analysis_conn = db::init_db(DB_PATH)?;
    let queue = analysis_queue::spawn_workers(&agent, analysis_conn, threads, telegram, String::new());
    let requeued = requeue_pending_analysis(&conn, &queue)?;
    println!(
        "{} package.json en base, {} en attente de traitement (pool de {} thread(s))...",
        rows.len(),
        requeued,
        threads
    );
    queue.shutdown_and_join();

    generate_report(&conn, &agent, "")
}

// =====================================================================================
// Mode --file : analyse un package.json local (pas de GitHub, pas d'écriture dans la base
// SQLite du crawl). Utile pour tester rapidement un projet en local, ou dans une CI, sans
// passer par la recherche/le crawl GitHub. Sonde les lockfiles/.npmrc voisins sur disque
// (lecture locale, gratuite — contrairement au mode GitHub qui doit les récupérer via le
// CDN raw), vérifie chaque dépendance sur le registre npm public, puis applique les mêmes
// réductions de faux positifs que le reste de l'outil (scope .npmrc privé confirmé, scope
// npm déjà revendiqué par un tiers). Aucune notion de dépôt GitHub ici : pas de score de
// sensibilité (pas d'étoiles/forks à récupérer), pas de détection monorepo/poly-repo (un
// seul package.json), pas de brouillon de divulgation (pas de mainteneur GitHub à
// contacter), et --notify n'est pas pris en charge dans ce mode (voir README).
// =====================================================================================

fn run_local_file_analysis(file_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let agent = github::build_agent()?; // simple client HTTP, aucun token GitHub requis ici

    let content = fs::read_to_string(file_path)
        .map_err(|e| format!("Impossible de lire {} : {}", file_path, e))?;
    let json: Value = serde_json::from_str(&content)
        .map_err(|e| format!("{} n'est pas un JSON valide : {}", file_path, e))?;

    // "Nom de dépôt" fictif, uniquement pour réutiliser les structures/fonctions partagées
    // avec le reste de l'outil (DependencyOccurrence, write_report_csv...) qui sont toutes
    // clés par (repo_full_name, package_name).
    let display_name = json.get("name").and_then(|v| v.as_str()).unwrap_or(file_path);
    let repo_full_name = format!("local:{}", display_name);
    let repo_url = format!("file://{}", file_path);

    let mut occurrences = Vec::new();
    analysis::extract_dependencies(&json, &repo_full_name, &repo_url, file_path, &mut occurrences);

    if occurrences.is_empty() {
        println!(
            "Aucune dépendance exploitable trouvée dans {} (fichier ignoré comme non représentatif, \
             aucune dependencies/devDependencies/peerDependencies/optionalDependencies, ou toutes les \
             versions ne sont pas résolvables depuis le registre npm — file:/link:/workspace:/git+...).",
            file_path
        );
        return Ok(());
    }

    // --- Sondage des lockfiles/.npmrc voisins, en local (gratuit, pas de réseau) ---------
    let dir = Path::new(file_path).parent().unwrap_or_else(|| Path::new("."));
    let mut lockfile_resolved: HashMap<(String, String), String> = HashMap::new();
    let mut private_scopes: HashSet<String> = HashSet::new();

    let package_lock_path = dir.join("package-lock.json");
    if let Ok(raw) = fs::read_to_string(&package_lock_path) {
        match serde_json::from_str::<Value>(&raw) {
            Ok(lock_json) => {
                let resolved = analysis::parse_package_lock(&lock_json);
                println!("  {} trouvé ({} entrée(s) résolue(s)).", package_lock_path.display(), resolved.len());
                for (name, url) in resolved {
                    lockfile_resolved.insert((repo_full_name.clone(), name), url);
                }
            }
            Err(e) => eprintln!("  {} trouvé mais JSON invalide, ignoré : {}", package_lock_path.display(), e),
        }
    }

    let yarn_lock_path = dir.join("yarn.lock");
    if let Ok(raw) = fs::read_to_string(&yarn_lock_path) {
        let resolved = analysis::parse_yarn_lock(&raw);
        println!("  {} trouvé ({} entrée(s) résolue(s)).", yarn_lock_path.display(), resolved.len());
        for (name, url) in resolved {
            lockfile_resolved.entry((repo_full_name.clone(), name)).or_insert(url);
        }
    }

    let pnpm_lock_path = dir.join("pnpm-lock.yaml");
    if pnpm_lock_path.exists() {
        println!("  {} présent mais non parsé sémantiquement (limitation connue, voir README).", pnpm_lock_path.display());
    }

    let npmrc_path = dir.join(".npmrc");
    if let Ok(raw) = fs::read_to_string(&npmrc_path) {
        let scopes = analysis::parse_npmrc_private_scopes(&raw);
        println!("  {} trouvé ({} scope(s) privé(s)).", npmrc_path.display(), scopes.len());
        for (scope, _url) in scopes {
            private_scopes.insert(scope);
        }
    }

    // --- Vérification npm (registre public) -----------------------------------------------
    let mut names: Vec<String> = occurrences.iter().map(|o| o.package_name.clone()).collect();
    names.sort();
    names.dedup();
    println!("Vérification de {} paquet(s) sur le registre npm public...", names.len());
    let npm_status: HashMap<String, bool> = npm::check_npm_batch(&agent, &names).into_iter().collect();

    // --- Vérification de revendication de scope (même logique que le reste de l'outil) ---
    let mut scope_claimed_safe: HashSet<(String, String)> = HashSet::new();
    for occ in occurrences.iter().filter(|o| npm_status.get(&o.package_name) == Some(&false)) {
        let Some(scope) = npm::extract_scope(&occ.package_name) else { continue };
        if npm::check_scope_claimed(&agent, scope) == Some(true) {
            scope_claimed_safe.insert((repo_full_name.clone(), occ.package_name.clone()));
        }
    }

    let flagged_count = analysis::write_report_csv(
        LOCAL_REPORT_PATH,
        &occurrences,
        &npm_status,
        &lockfile_resolved,
        &private_scopes,
        &HashSet::new(), // pas de détection monorepo/workspace : un seul package.json
        &HashSet::new(), // pas de notion d'"autre dépôt du même org" pour un fichier isolé
        &scope_claimed_safe,
        &HashMap::new(), // pas de score de sensibilité : pas de dépôt GitHub à interroger
    )?;

    let flagged_names: HashSet<&str> = occurrences
        .iter()
        .filter(|o| {
            let key = (o.repo_full_name.clone(), o.package_name.clone());
            npm_status.get(&o.package_name) == Some(&false) && !scope_claimed_safe.contains(&key)
        })
        .map(|o| o.package_name.as_str())
        .collect();

    let vuln_count_display = if flagged_names.is_empty() {
        colors::safe(&flagged_names.len().to_string())
    } else {
        colors::alert(&flagged_names.len().to_string())
    };
    println!(
        "=== Rapport : {} dépendance(s) analysée(s), {} paquet(s) absent(s) de npm ({} occurrence(s)) ===",
        occurrences.len(),
        vuln_count_display,
        flagged_count
    );
    if !scope_claimed_safe.is_empty() {
        println!(
            "({} faux positif(s) exclu(s) : scope npm déjà revendiqué par un tiers — publication non autorisée pour un attaquant externe)",
            scope_claimed_safe.len()
        );
    }

    let mut sorted: Vec<&str> = flagged_names.into_iter().collect();
    sorted.sort();
    for name in sorted {
        let confirmed = analysis::is_confirmed_private_scope(name, &private_scopes)
            || lockfile_resolved.keys().any(|(_, n)| n == name);
        if confirmed {
            println!(
                "  - {}",
                colors::confirmed(&format!("{}  [confirmé privé — vulnérabilité probable]", name))
            );
        } else {
            println!(
                "  - {}",
                colors::medium_confidence(&format!("{}  [absent de npm, à vérifier]", name))
            );
        }
    }
    if flagged_count == 0 {
        println!("{}", colors::safe("Aucun paquet suspect détecté."));
    }
    println!("Rapport détaillé écrit dans : {}", LOCAL_REPORT_PATH);

    Ok(())
}


// (dependency_occurrences + npm_registry_cache + lockfiles + npmrc_configs). Ne fait AUCUN
// appel réseau : c'est un pur export, appelable à tout moment (y compris pendant qu'un
// --crawl tourne dans un autre processus, grâce au mode WAL de SQLite).
fn generate_report(conn: &rusqlite::Connection, agent: &Agent, token: &str) -> Result<(), Box<dyn std::error::Error>> {
    let occurrence_rows = db::all_occurrences(conn)?;
    let occurrences: Vec<analysis::DependencyOccurrence> = occurrence_rows
        .into_iter()
        .map(|r| analysis::DependencyOccurrence {
            package_name: r.package_name,
            version_spec: r.version_spec,
            dep_type: leak_dep_type(&r.dep_type),
            repo_full_name: r.repo_full_name,
            repo_url: r.repo_url,
            path: r.path,
        })
        .collect();

    // --- Signaux forts : lockfiles (resolved URL) + .npmrc (scopes privés) ---
    let lockfile_rows = db::all_lockfiles(conn)?;
    let mut lockfile_resolved: HashMap<(String, String), String> = HashMap::new();
    let mut pnpm_skipped = 0u32;

    for (repo_full_name, path, kind) in &lockfile_rows {
        let file_path = local_repo_dir(repo_full_name).join(path);
        let resolved_map: HashMap<String, String> = match kind.as_str() {
            "package-lock.json" => match fs::read_to_string(&file_path)
                .ok()
                .and_then(|c| serde_json::from_str::<Value>(&c).ok())
            {
                Some(json) => analysis::parse_package_lock(&json),
                None => continue,
            },
            "yarn.lock" => match fs::read_to_string(&file_path).ok() {
                Some(content) => analysis::parse_yarn_lock(&content),
                None => continue,
            },
            _ => {
                pnpm_skipped += 1;
                continue;
            }
        };
        for (name, url) in resolved_map {
            lockfile_resolved.insert((repo_full_name.clone(), name), url);
        }
    }
    let private_scopes = db::all_private_scopes(conn)?;

    let npm_status = db::load_npm_cache(conn)?;

    // --- Réduction des faux positifs : résolution monorepo/poly-repo -------------------
    // Une dépendance "absente de npm" qui correspond en fait au nom d'un AUTRE package.json
    // du même dépôt est très probablement un package de workspace résolu localement, pas un
    // vrai risque : on l'exclut entièrement. Si le nom correspond à un package.json d'un
    // AUTRE dépôt de la même organisation GitHub, c'est un signal plus faible (poly-repo
    // probable) : on le garde mais avec une confiance dédiée plutôt que de l'exclure.
    let mut monorepo_resolved: HashSet<(String, String)> = HashSet::new();
    let mut org_match: HashSet<(String, String)> = HashSet::new();

    for occ in occurrences.iter().filter(|o| npm_status.get(&o.package_name) == Some(&false)) {
        let key = (occ.repo_full_name.clone(), occ.package_name.clone());
        if monorepo_resolved.contains(&key) || org_match.contains(&key) {
            continue;
        }
        if db::name_declared_in_same_repo(conn, &occ.repo_full_name, &occ.package_name, &occ.path)? {
            monorepo_resolved.insert(key);
            continue;
        }
        let org = occ.repo_full_name.split('/').next().unwrap_or("");
        if !org.is_empty()
            && db::name_declared_in_same_org(conn, org, &occ.package_name, &occ.repo_full_name)?
        {
            org_match.insert(key);
        }
    }

    // --- Réduction des faux positifs : scopes npm déjà revendiqués ----------------------
    // Un paquet scoped (@scope/nom) ne peut être publié que par le titulaire du scope. Si
    // le scope est déjà revendiqué (par l'org légitime, vraisemblablement), aucun
    // attaquant externe ne peut publier dessous : ce n'est pas un risque exploitable
    // aujourd'hui, même si ce nom précis n'existe pas encore sur npm.
    let mut scope_claimed_safe: HashSet<(String, String)> = HashSet::new();

    for occ in occurrences.iter().filter(|o| npm_status.get(&o.package_name) == Some(&false)) {
        let key = (occ.repo_full_name.clone(), occ.package_name.clone());
        if monorepo_resolved.contains(&key) {
            continue; // déjà exclu pour une autre raison
        }
        let Some(scope) = npm::extract_scope(&occ.package_name) else {
            continue; // pas un paquet scoped
        };
        let claimed = match db::known_scope_claimed(conn, scope)? {
            Some(c) => Some(c),
            None => match npm::check_scope_claimed(agent, scope) {
                Some(c) => {
                    let _ = db::save_scope_claimed(conn, scope, c);
                    Some(c)
                }
                None => None,
            },
        };
        if claimed == Some(true) {
            scope_claimed_safe.insert(key);
        }
    }

    // --- Score de sensibilité des dépôts (étoiles/forks/dernière activité) --------------
    // Calculé uniquement pour les dépôts qui apparaissent réellement dans le rapport final
    // (après toutes les exclusions ci-dessus), pour prioriser les découvertes par impact
    // potentiel plutôt que de traiter chaque dépôt de façon égale.
    let mut repo_scores: HashMap<String, u8> = HashMap::new();
    for occ in occurrences.iter().filter(|o| npm_status.get(&o.package_name) == Some(&false)) {
        let key = (occ.repo_full_name.clone(), occ.package_name.clone());
        if monorepo_resolved.contains(&key) || scope_claimed_safe.contains(&key) {
            continue;
        }
        if repo_scores.contains_key(&occ.repo_full_name) {
            continue;
        }
        if let Some(score) = sensitivity::get_sensitivity_score(conn, agent, token, &occ.repo_full_name) {
            repo_scores.insert(occ.repo_full_name.clone(), score);
        }
    }

    let flagged_count = analysis::write_report_csv(
        REPORT_PATH,
        &occurrences,
        &npm_status,
        &lockfile_resolved,
        &private_scopes,
        &monorepo_resolved,
        &org_match,
        &scope_claimed_safe,
        &repo_scores,
    )?;

    let flagged_names: HashSet<&str> = occurrences
        .iter()
        .filter(|o| {
            let key = (o.repo_full_name.clone(), o.package_name.clone());
            npm_status.get(&o.package_name) == Some(&false)
                && !monorepo_resolved.contains(&key)
                && !scope_claimed_safe.contains(&key)
        })
        .map(|o| o.package_name.as_str())
        .collect();

    let vuln_count_display = if flagged_names.is_empty() {
        colors::safe(&flagged_names.len().to_string())
    } else {
        colors::alert(&flagged_names.len().to_string())
    };

    println!(
        "=== Rapport : {} occurrences indexées, {} lockfiles ({} pnpm non parsés), {} scope(s) privé(s), {} paquets absents de npm ({} occurrences) ===",
        occurrences.len(),
        lockfile_rows.len(),
        pnpm_skipped,
        private_scopes.len(),
        vuln_count_display,
        flagged_count
    );
    if !monorepo_resolved.is_empty() {
        println!(
            "({} faux positif(s) exclu(s) : nom résolu par un package.json du même dépôt — probable monorepo/workspace)",
            monorepo_resolved.len()
        );
    }
    if !scope_claimed_safe.is_empty() {
        println!(
            "({} faux positif(s) exclu(s) : scope npm déjà revendiqué par un tiers — publication non autorisée pour un attaquant externe)",
            scope_claimed_safe.len()
        );
    }
    let mut sorted: Vec<&str> = flagged_names.into_iter().collect();
    sorted.sort();
    for name in sorted {
        let confirmed = analysis::is_confirmed_private_scope(name, &private_scopes)
            || lockfile_resolved.keys().any(|(_, n)| n == name);
        let is_org_match = org_match.iter().any(|(_, n)| n == name);

        // Score de sensibilité le plus élevé parmi les dépôts référençant ce nom (priorise
        // l'affichage sur le pire cas : le dépôt le plus exposé si le nom apparaît ailleurs).
        let max_score = occurrences
            .iter()
            .filter(|o| o.package_name == name)
            .filter_map(|o| repo_scores.get(&o.repo_full_name))
            .max()
            .copied();
        let score_tag = match max_score {
            Some(s) => format!(" (sensibilité dépôt : {}/10)", s),
            None => String::new(),
        };

        if confirmed {
            println!(
                "  - {}",
                colors::confirmed(&format!(
                    "{}  [confirmé privé — vulnérabilité probable]{}",
                    name, score_tag
                ))
            );
        } else if is_org_match {
            println!(
                "  - {}",
                colors::medium_confidence(&format!(
                    "{}  [nom vu dans un autre dépôt de l'org — probable poly-repo, à vérifier]{}",
                    name, score_tag
                ))
            );
        } else {
            println!(
                "  - {}",
                colors::medium_confidence(&format!("{}  [absent de npm, à vérifier]{}", name, score_tag))
            );
        }
    }
    if flagged_count == 0 {
        println!("{}", colors::safe("Aucun paquet suspect détecté."));
    }
    println!("Rapport détaillé écrit dans : {}", REPORT_PATH);

    // --- Divulgation responsable -------------------------------------------------------
    // Pour chaque (dépôt, paquet) réellement retenu dans le rapport final (donc après
    // exclusion des faux positifs monorepo/scope-déjà-revendiqué), on récupère le contact
    // sécurité du dépôt et on génère un brouillon de message prêt à envoyer. Aucune
    // publication npm n'est jamais effectuée par l'outil.
    let mut drafts_written = 0u32;
    for occ in &occurrences {
        if npm_status.get(&occ.package_name) != Some(&false) {
            continue;
        }
        let key = (occ.repo_full_name.clone(), occ.package_name.clone());
        if monorepo_resolved.contains(&key) || scope_claimed_safe.contains(&key) {
            continue;
        }

        let confidence = if analysis::is_confirmed_private_scope(&occ.package_name, &private_scopes)
            || lockfile_resolved.contains_key(&key)
        {
            "high (confirmed by lockfile/.npmrc)"
        } else if org_match.contains(&key) {
            "low (name seen in another repo of the same org — likely poly-repo)"
        } else {
            "medium (absent from npm only)"
        };

        let contact = disclosure::get_security_contact(conn, agent, token, &occ.repo_full_name);
        let message = disclosure::generate_disclosure_message(
            &occ.package_name,
            occ.dep_type,
            &occ.repo_full_name,
            &contact,
            confidence,
        );
        let sensitivity_note = repo_scores
            .get(&occ.repo_full_name)
            .map(|&s| (s, sensitivity::score_label(s)));
        if disclosure::write_disclosure_draft(&occ.repo_full_name, &occ.package_name, &message, sensitivity_note)
            .is_ok()
        {
            drafts_written += 1;
        }
    }
    if drafts_written > 0 {
        println!(
            "{} brouillon(s) de divulgation responsable écrit(s) dans le dossier disclosures/",
            drafts_written
        );
    }

    Ok(())
}

// dep_type doit être 'static pour DependencyOccurrence ; comme il vient maintenant de la
// base (String) et non plus d'une constante de code, on le fixe sur un petit ensemble
// connu de valeurs possibles plutôt que d'ajouter du unsafe leaking arbitraire.
fn leak_dep_type(s: &str) -> &'static str {
    match s {
        "dependencies" => "dependencies",
        "devDependencies" => "devDependencies",
        "peerDependencies" => "peerDependencies",
        "optionalDependencies" => "optionalDependencies",
        _ => "dependencies",
    }
}

// =====================================================================================

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let raw_args: Vec<String> = env::args().collect();
    let usage = format!(
        "Usage:\n  \
         {0} <fichier_token> [--analyse] [--threads N] [--notify fichier]   Recherche + téléchargement\n  \
         {0} --crawl <fichier_token> [--analyse] [--threads N] [--notify fichier]   Parcours exhaustif\n  \
         {0} --analyse [--threads N] [--notify fichier]   Rattrape et régénère le rapport\n  \
         {0} --file chemin/vers/package.json   Analyse un package.json local (pas de GitHub)\n  \
         {0} --config fichier.toml   Configuration complète depuis un fichier TOML (exclusif des autres options)\n\n\
         --analyse démarre un pool de threads (par défaut {1}) qui analysent les\n\
         package.json au fil de l'eau, dans une file séparée du téléchargement.\n\
         --threads N ajuste ce nombre de threads.\n\
         --notify fichier envoie une notification Telegram pour chaque découverte à risque\n\
         (fichier de config : ligne 1 = token du bot, ligne 2 = chat ID, ligne 3 optionnelle\n\
         = score de sensibilité minimum 0-10, défaut 4), ainsi qu'une notification de\n\
         démarrage dès que l'exécution commence. Non pris en charge avec --file.\n\
         --file lit et analyse un package.json local (dependency confusion + faux positifs\n\
         réduits via les lockfiles/.npmrc voisins sur disque), sans toucher à la base SQLite\n\
         ni au réseau GitHub. Exclusif des autres modes.\n\
         --config fichier.toml remplace tous les arguments ci-dessus par un fichier TOML\n\
         (voir README pour le format).",
        raw_args[0], DEFAULT_ANALYSIS_THREADS
    );

    if raw_args.len() < 2 {
        eprintln!("{}", usage);
        std::process::exit(1);
    }

    // --config est prioritaire et exclusif : si présent, tout le reste des arguments est
    // ignoré, pour éviter toute ambiguïté sur la source de vérité en cas de conflit.
    if let Some(idx) = raw_args.iter().position(|a| a == "--config") {
        let Some(config_path) = raw_args.get(idx + 1) else {
            eprintln!("--config doit être suivi d'un chemin de fichier TOML.\n\n{}", usage);
            std::process::exit(1);
        };
        return run_from_config(config_path);
    }

    // --file est également exclusif et prioritaire : ce mode ne touche ni la base SQLite du
    // crawl ni GitHub, il n'a donc aucun sens de le combiner avec --crawl/--analyse/--notify.
    if let Some(idx) = raw_args.iter().position(|a| a == "--file") {
        let Some(file_path) = raw_args.get(idx + 1) else {
            eprintln!("--file doit être suivi d'un chemin vers un package.json.\n\n{}", usage);
            std::process::exit(1);
        };
        return run_local_file_analysis(file_path);
    }

    // Parsing indépendant de l'ordre. "--threads" et "--notify" consomment l'argument
    // suivant ; tout le reste commençant par "--" est un drapeau simple ; ce qui reste est
    // le fichier token (au plus un argument positionnel attendu).
    let mut flags: HashSet<String> = HashSet::new();
    let mut positional: Vec<String> = Vec::new();
    let mut threads = DEFAULT_ANALYSIS_THREADS;
    let mut telegram: Option<TelegramConfig> = None;

    let rest = &raw_args[1..];
    let mut i = 0;
    while i < rest.len() {
        let arg = &rest[i];
        if arg == "--threads" {
            i += 1;
            match rest.get(i).and_then(|v| v.parse::<usize>().ok()) {
                Some(n) if n >= 1 => threads = n,
                _ => {
                    eprintln!("--threads doit être suivi d'un nombre entier >= 1.\n\n{}", usage);
                    std::process::exit(1);
                }
            }
        } else if arg == "--notify" {
            i += 1;
            let Some(config_file) = rest.get(i) else {
                eprintln!("--notify doit être suivi d'un fichier de configuration.\n\n{}", usage);
                std::process::exit(1);
            };
            telegram = Some(notify::read_telegram_config(config_file)?);
        } else if let Some(stripped) = arg.strip_prefix("--") {
            flags.insert(format!("--{}", stripped));
        } else {
            positional.push(arg.clone());
        }
        i += 1;
    }

    let wants_crawl = flags.contains("--crawl");
    let wants_analyse = flags.contains("--analyse") || flags.contains("--analyze");

    if wants_crawl {
        let Some(token_file) = positional.first() else {
            eprintln!("Le mode --crawl nécessite un fichier token.\n\n{}", usage);
            std::process::exit(1);
        };
        let token = read_token(token_file)?;
        return run_crawl(&token, wants_analyse, threads, telegram);
    }

    if wants_analyse && positional.is_empty() {
        return run_analysis(threads, telegram);
    }

    if let Some(token_file) = positional.first() {
        let token = read_token(token_file)?;
        return run_search_download(&token, wants_analyse, threads, telegram);
    }

    eprintln!("{}", usage);
    std::process::exit(1);
}

// Point d'entrée quand --config fichier.toml est utilisé : traduit le fichier en le même
// jeu de paramètres que le parsing CLI, puis délègue aux mêmes fonctions run_*.
fn run_from_config(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let cfg = config::load_config(path)?;
    let threads = cfg.threads.unwrap_or(DEFAULT_ANALYSIS_THREADS);
    let telegram = cfg.notify.map(|n| TelegramConfig {
        bot_token: n.bot_token,
        chat_id: n.chat_id,
        min_sensitivity: n.min_sensitivity,
    });

    match cfg.mode.as_str() {
        "analyse" | "analyze" => run_analysis(threads, telegram),
        "file" => {
            let file_path = cfg
                .file_path
                .ok_or("file_path manquant pour le mode file (validé normalement par load_config)")?;
            run_local_file_analysis(&file_path)
        }
        "crawl" => {
            let token_file = cfg
                .token_file
                .ok_or("token_file manquant pour le mode crawl (validé normalement par load_config)")?;
            let token = read_token(&token_file)?;
            run_crawl(&token, cfg.analyse, threads, telegram)
        }
        _ => {
            let token_file = cfg
                .token_file
                .ok_or("token_file manquant pour le mode search (validé normalement par load_config)")?;
            let token = read_token(&token_file)?;
            run_search_download(&token, cfg.analyse, threads, telegram)
        }
    }
}

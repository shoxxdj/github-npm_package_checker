use crate::analysis;
use crate::db;
use crate::notify::{self, TelegramConfig};
use crate::npm;
use crate::sensitivity;
use rusqlite::Connection;
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use ureq::Agent;

// Tâche envoyée dès qu'un package.json est téléchargé (nouveau ou mis à jour), consommée
// par le pool de threads d'analyse. Découplé du téléchargement : le crawl/la recherche ne
// sont jamais ralentis par les vérifications npm.
pub struct AnalysisTask {
    pub repo_full_name: String,
    pub path: String,
    pub repo_url: String,
}

pub struct AnalysisQueue {
    sender: Sender<AnalysisTask>,
    handles: Vec<thread::JoinHandle<()>>,
}

impl AnalysisQueue {
    pub fn submit(&self, task: AnalysisTask) {
        // Le récepteur ne peut être fermé qu'après la jonction des threads ; un échec
        // d'envoi ne devrait donc se produire qu'en toute fin de programme.
        let _ = self.sender.send(task);
    }

    // Ferme la file (plus aucune tâche ne sera acceptée) et attend que les threads en cours
    // terminent leur travail actuel — aucune tâche déjà en file n'est perdue, elle est
    // simplement traitée avant l'arrêt.
    pub fn shutdown_and_join(self) {
        drop(self.sender);
        for h in self.handles {
            let _ = h.join();
        }
    }
}

// Démarre `thread_count` threads d'analyse partageant :
// - une connexion SQLite protégée par mutex (SQLite ne permet qu'un seul écrivain de toute
//   façon ; un mutex simple évite toute contention "database is locked")
// - un ensemble en mémoire des noms de paquets déjà vérifiés ou en cours de vérification,
//   pour qu'un même nom ne soit jamais vérifié deux fois par deux threads en parallèle
pub fn spawn_workers(
    agent: &Agent,
    conn: Connection,
    thread_count: usize,
    telegram: Option<TelegramConfig>,
    token: String,
) -> AnalysisQueue {
    let (tx, rx) = mpsc::channel::<AnalysisTask>();
    let rx = Arc::new(Mutex::new(rx));
    let conn = Arc::new(Mutex::new(conn));
    let telegram = Arc::new(telegram);
    let token = Arc::new(token);

    // Pré-charge les noms déjà connus (vérifiés ou en cache) pour ne pas les revérifier.
    let known_names: HashSet<String> = {
        let c = conn.lock().unwrap();
        db::load_npm_cache(&c).map(|m| m.into_keys().collect()).unwrap_or_default()
    };
    let in_progress_or_done = Arc::new(Mutex::new(known_names));

    let mut handles = Vec::with_capacity(thread_count);
    for worker_id in 0..thread_count {
        let rx = Arc::clone(&rx);
        let conn = Arc::clone(&conn);
        let in_progress_or_done = Arc::clone(&in_progress_or_done);
        let agent = agent.clone();
        let telegram = Arc::clone(&telegram);
        let token = Arc::clone(&token);

        let handle = thread::spawn(move || {
            worker_loop(worker_id, rx, conn, in_progress_or_done, agent, telegram, token);
        });
        handles.push(handle);
    }

    AnalysisQueue { sender: tx, handles }
}

fn worker_loop(
    worker_id: usize,
    rx: Arc<Mutex<Receiver<AnalysisTask>>>,
    conn: Arc<Mutex<Connection>>,
    in_progress_or_done: Arc<Mutex<HashSet<String>>>,
    agent: Agent,
    telegram: Arc<Option<TelegramConfig>>,
    token: Arc<String>,
) {
    loop {
        let task = {
            let guard = rx.lock().unwrap();
            guard.recv()
        };
        let task = match task {
            Ok(t) => t,
            Err(_) => break, // plus aucun émetteur : la file est définitivement fermée
        };

        if let Err(e) = process_task(&task, &conn, &in_progress_or_done, &agent, &telegram, &token) {
            eprintln!(
                "[analyse#{}] Erreur sur {}/{}: {}",
                worker_id, task.repo_full_name, task.path, e
            );
        }
    }
}

// Comme check_scope_with_cache ci-dessous : lit le cache, relâche le mutex pendant l'appel
// réseau (get_repo_metrics), puis reprend le mutex pour sauvegarder. Éviter de tenir le
// verrou pendant tout un appel HTTP, qui bloquerait les autres threads du pool sur chaque
// accès DB pendant ce temps.
fn get_sensitivity_with_cache(
    agent: &Agent,
    conn: &Arc<Mutex<Connection>>,
    token: &str,
    repo_full_name: &str,
) -> Option<u8> {
    let cached = {
        let c = conn.lock().unwrap();
        db::known_sensitivity(&c, repo_full_name).ok().flatten()
    };
    if let Some(row) = cached {
        return Some(row.score as u8);
    }

    let metrics = crate::github::get_repo_metrics(agent, token, repo_full_name)?;
    let score = sensitivity::compute_score(metrics.stars, metrics.forks, metrics.pushed_at.as_deref());
    {
        let c = conn.lock().unwrap();
        let _ = db::save_sensitivity(
            &c,
            repo_full_name,
            score,
            metrics.stars,
            metrics.forks,
            metrics.pushed_at.as_deref(),
        );
    }
    Some(score)
}

fn local_file_path(repo_full_name: &str, path: &str) -> PathBuf {
    let folder = repo_full_name.replace('/', "_");
    Path::new("repositories").join(folder).join(path)
}

// Vérifie (avec cache DB) si le scope d'un paquet scoped est revendiqué sur npm. Ne
// s'applique qu'aux noms scoped ("@scope/nom") ; renvoie None pour un nom non scoped.
fn check_scope_with_cache(
    agent: &Agent,
    conn: &Arc<Mutex<Connection>>,
    package_name: &str,
) -> Option<bool> {
    let scope = npm::extract_scope(package_name)?;

    let cached = {
        let c = conn.lock().unwrap();
        db::known_scope_claimed(&c, scope).ok().flatten()
    };
    if let Some(claimed) = cached {
        return Some(claimed);
    }

    let claimed = npm::check_scope_claimed(agent, scope)?;
    let c = conn.lock().unwrap();
    let _ = db::save_scope_claimed(&c, scope, claimed);
    Some(claimed)
}

fn process_task(
    task: &AnalysisTask,
    conn: &Arc<Mutex<Connection>>,
    in_progress_or_done: &Arc<Mutex<HashSet<String>>>,
    agent: &Agent,
    telegram: &Arc<Option<TelegramConfig>>,
    token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let file_path = local_file_path(&task.repo_full_name, &task.path);
    let content = fs::read_to_string(&file_path)?;
    let json: Value = serde_json::from_str(&content)?;

    // Persiste le nom propre déclaré (utilisé pour détecter les faux positifs de type
    // monorepo/workspace au moment de générer le rapport).
    if let Some(name) = json.get("name").and_then(|v| v.as_str()) {
        let c = conn.lock().unwrap();
        db::upsert_declared_name(&c, &task.repo_full_name, &task.path, name)?;
    }

    let mut occurrences = Vec::new();
    analysis::extract_dependencies(&json, &task.repo_full_name, &task.repo_url, &task.path, &mut occurrences);

    // Persiste les occurrences immédiatement : même si le programme s'arrête juste après,
    // le travail de parsing n'est pas perdu.
    {
        let c = conn.lock().unwrap();
        for occ in &occurrences {
            db::upsert_occurrence(
                &c,
                &occ.repo_full_name,
                &occ.path,
                &occ.package_name,
                occ.dep_type,
                &occ.version_spec,
                &occ.repo_url,
            )?;
        }
    }

    // Détermine quels noms ce thread doit vérifier lui-même (marquage atomique pour éviter
    // qu'un autre thread ne vérifie le même nom en double).
    let names_to_check: Vec<String> = {
        let mut set = in_progress_or_done.lock().unwrap();
        occurrences
            .iter()
            .map(|o| o.package_name.clone())
            .filter(|name| set.insert(name.clone())) // insert renvoie true si nouveau
            .collect()
    };

    if !names_to_check.is_empty() {
        let results = npm::check_npm_batch(agent, &names_to_check);
        {
            let c = conn.lock().unwrap();
            let mut conn_mut = c; // besoin de &mut Connection pour la transaction
            db::save_npm_cache_batch(&mut conn_mut, &results)?;
        }

        for (name, exists) in &results {
            if *exists {
                continue;
            }
            let dep_type = occurrences
                .iter()
                .find(|o| &o.package_name == name)
                .map(|o| o.dep_type)
                .unwrap_or("dependencies");

            // Un paquet scoped ne peut être publié que par le titulaire du scope. Si le
            // scope est déjà revendiqué par quelqu'un (l'org légitime, vraisemblablement),
            // aucun attaquant externe ne peut publier dessous : le risque réel est bien
            // plus faible qu'un nom non scoped absent de npm.
            let scope_claimed = check_scope_with_cache(agent, conn, name);
            let is_scoped = npm::extract_scope(name).is_some();

            let exploitable = match (is_scoped, scope_claimed) {
                (true, Some(true)) => false, // scope déjà revendiqué : pas exploitable par un tiers
                _ => true,                   // non scoped, ou scope non revendiqué/indéterminé
            };

            let message = format!(
                "[ALERTE dependency confusion] \"{}\" absent de npm — {}/{} ({})",
                name, task.repo_full_name, task.path, dep_type
            );

            if exploitable {
                println!("{}", crate::colors::alert(&message));
                if let Some(cfg) = telegram.as_ref() {
                    // Score de sensibilité du dépôt (étoiles/forks/activité, voir
                    // sensitivity.rs), mis en cache en base. En cas d'échec de récupération
                    // (rate limit, réseau...) on choisit de notifier quand même plutôt que
                    // de risquer de faire silencieusement disparaître une vraie alerte :
                    // seul un score effectivement CONNU et sous le seuil supprime l'envoi.
                    let score = get_sensitivity_with_cache(agent, conn, token, &task.repo_full_name);
                    let sufficiently_sensitive = match score {
                        Some(s) => s >= cfg.min_sensitivity,
                        None => true,
                    };

                    if sufficiently_sensitive {
                        let reason = if is_scoped {
                            "scope npm non revendiqué — squattable immédiatement"
                        } else {
                            "paquet absent de npm — squattable immédiatement"
                        };
                        let text = notify::format_alert_message(
                            name,
                            &task.repo_full_name,
                            &task.repo_url,
                            &task.path,
                            reason,
                        );
                        notify::send_telegram_message(agent, cfg, &text);
                    } else if let Some(s) = score {
                        println!(
                            "{}",
                            crate::colors::medium_confidence(&notify::format_low_sensitivity_skip(
                                &task.repo_full_name,
                                s,
                                cfg.min_sensitivity
                            ))
                        );
                    }
                }
            } else {
                println!(
                    "{}",
                    crate::colors::medium_confidence(&format!(
                        "[info] \"{}\" absent de npm mais scope déjà revendiqué (risque faible) — {}/{}",
                        name, task.repo_full_name, task.path
                    ))
                );
            }
        }
    }

    {
        let c = conn.lock().unwrap();
        db::mark_analyzed(&c, &task.repo_full_name, &task.path)?;
    }

    Ok(())
}

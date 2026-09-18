use std::sync::mpsc;
use std::thread;
use std::thread::sleep;
use std::time::Duration;
use ureq::Agent;

const NPM_REGISTRY_BASE: &str = "https://registry.npmjs.org/";
// Nombre de threads pour paralléliser les vérifications sur le registre npm. Le registre
// (Fastly/Cloudflare) supporte une charge bien plus élevée que l'API GitHub.
const ANALYSIS_CONCURRENCY: usize = 16;

// Encode un nom de paquet scoped ("@scope/name") pour l'URL du registre npm, qui attend
// le "/" encodé en "%2F".
fn encode_npm_name(name: &str) -> String {
    if let Some(rest) = name.strip_prefix('@') {
        if let Some(slash_idx) = rest.find('/') {
            return format!("@{}%2F{}", &rest[..slash_idx], &rest[slash_idx + 1..]);
        }
    }
    name.to_string()
}

// Some(true)  -> le paquet existe publiquement sur npm
// Some(false) -> confirmé absent (404) : candidat à la dependency confusion
// None        -> statut indéterminé (erreur réseau, rate limit, 5xx...)
fn check_npm_exists(agent: &Agent, name: &str) -> Option<bool> {
    let url = format!("{}{}", NPM_REGISTRY_BASE, encode_npm_name(name));
    match agent.head(&url).set("User-Agent", "gh_package_finder/0.1").call() {
        Ok(_) => Some(true),
        Err(ureq::Error::Status(404, _)) => Some(false),
        Err(_) => None,
    }
}

// Un paquet scoped (@scope/nom) ne peut être publié que par le titulaire du scope sur npm
// — cette vérification est le point clé qui distingue un vrai risque de dependency
// confusion (scope PAS revendiqué, donc squattable par n'importe qui gratuitement) d'un
// faux positif (scope déjà revendiqué par le propriétaire légitime, un attaquant externe
// ne peut donc pas publier dessous, même si ce paquet précis n'existe pas encore).
//
// Some(true)  -> le scope est revendiqué (par un compte utilisateur ou une organisation)
// Some(false) -> confirmé non revendiqué (404) : n'importe qui peut le créer aujourd'hui
// None        -> statut indéterminé (erreur réseau, rate limit...)
pub fn check_scope_claimed(agent: &Agent, scope: &str) -> Option<bool> {
    // `scope` est attendu SANS le "@" initial (ex: "acme-corp" pour "@acme-corp/...").
    let url = format!("https://registry.npmjs.org/-/org/{}/package", scope);
    match agent.head(&url).set("User-Agent", "gh_package_finder/0.1").call() {
        Ok(_) => Some(true),
        Err(ureq::Error::Status(404, _)) => Some(false),
        Err(_) => None,
    }
}

// Extrait le scope (sans le "@") d'un nom de paquet, si scoped.
pub fn extract_scope(package_name: &str) -> Option<&str> {
    package_name.strip_prefix('@').and_then(|rest| rest.split('/').next())
}

// Répartit `names` sur ANALYSIS_CONCURRENCY threads pour paralléliser les vérifications.
pub fn check_npm_batch(agent: &Agent, names: &[String]) -> Vec<(String, bool)> {
    if names.is_empty() {
        return Vec::new();
    }

    let thread_count = ANALYSIS_CONCURRENCY.min(names.len()).max(1);
    let chunk_size = (names.len() + thread_count - 1) / thread_count;
    let (tx, rx) = mpsc::channel();

    thread::scope(|scope| {
        for chunk in names.chunks(chunk_size) {
            let tx = tx.clone();
            let agent = agent.clone();
            scope.spawn(move || {
                for name in chunk {
                    if let Some(exists) = check_npm_exists(&agent, name) {
                        let _ = tx.send((name.clone(), exists));
                    }
                    sleep(Duration::from_millis(50));
                }
            });
        }
        drop(tx);
    });

    rx.into_iter().collect()
}

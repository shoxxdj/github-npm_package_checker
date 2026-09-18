use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::Deserialize;
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;
use ureq::{Agent, AgentBuilder, Response};

pub fn build_agent() -> Result<Agent, Box<dyn std::error::Error>> {
    // ureq ne configure pas de backend TLS par défaut avec la feature "native-tls" :
    // il faut construire explicitement un Agent avec le connecteur natif.
    Ok(AgentBuilder::new()
        .tls_connector(Arc::new(native_tls::TlsConnector::new()?))
        .build())
}

#[derive(Deserialize, Debug)]
pub struct SearchResponse {
    pub total_count: u64,
    pub items: Vec<CodeItem>,
}

#[derive(Deserialize, Debug)]
pub struct CodeItem {
    pub path: String,
    pub repository: Repository,
    // Hash git du blob correspondant à cette version précise du fichier. Utilisé UNIQUEMENT
    // pour la détection "a changé / n'a pas changé" (skip avant tout appel réseau) : ce n'est
    // PAS une référence valide pour raw.githubusercontent.com (qui exige un ref de type
    // commit/branche, pas un sha de blob isolé).
    pub sha: String,
    // URL de l'API "contents" pour ce fichier précis (repli en cas d'échec du CDN raw)
    pub url: String,
}

#[derive(Deserialize, Debug)]
pub struct Repository {
    pub full_name: String,
    pub html_url: Option<String>,
}

impl Repository {
    pub fn url(&self) -> String {
        self.html_url
            .clone()
            .unwrap_or_else(|| format!("https://github.com/{}", self.full_name))
    }
}

#[derive(Deserialize, Debug)]
struct ContentResponse {
    content: Option<String>,
    encoding: Option<String>,
}

// Respecte les limites de débit de l'API GitHub en observant les en-têtes de réponse
pub fn wait_if_rate_limited(resp: &Response) {
    if let Some(remaining_str) = resp.header("x-ratelimit-remaining") {
        if let Ok(remaining_val) = remaining_str.parse::<i64>() {
            if remaining_val <= 1 {
                if let Some(reset_str) = resp.header("x-ratelimit-reset") {
                    if let Ok(reset_ts) = reset_str.parse::<i64>() {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_secs() as i64;
                        let wait_secs = (reset_ts - now).max(1) as u64;
                        println!("Limite de débit atteinte, pause de {} secondes...", wait_secs);
                        sleep(Duration::from_secs(wait_secs));
                    }
                }
            }
        }
    }
}

pub fn search_code_page(
    agent: &Agent,
    token: &str,
    query: &str,
    per_page: u32,
    page: u32,
) -> Result<Option<SearchResponse>, Box<dyn std::error::Error>> {
    let call = agent
        .get("https://api.github.com/search/code")
        .query("q", query)
        .query("per_page", &per_page.to_string())
        .query("page", &page.to_string())
        .set("Authorization", &format!("token {}", token))
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .set("User-Agent", "gh_package_finder/0.1")
        .call();

    let resp = match call {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            eprintln!("Erreur API GitHub (recherche), statut {}: {}", code, body);
            return Ok(None);
        }
        Err(e) => {
            eprintln!("Erreur réseau lors de la recherche: {}", e);
            return Ok(None);
        }
    };

    wait_if_rate_limited(&resp);
    let search_result: SearchResponse = resp.into_json()?;
    Ok(Some(search_result))
}

// --- Listing exhaustif des dépôts publics (mode --crawl) -----------------------------

#[derive(Deserialize, Debug)]
pub struct RepoListItem {
    pub id: i64,
    pub full_name: String,
    pub fork: bool,
}

// GET /repositories?since=ID : énumère TOUS les dépôts publics par ordre d'ID croissant,
// indépendamment de l'index de recherche de code (qui n'est ni exhaustif ni toujours à
// jour). Pagination gérée uniquement via le paramètre "since" (pas de "page").
pub fn list_repositories_since(
    agent: &Agent,
    token: &str,
    since_id: i64,
) -> Result<Vec<RepoListItem>, Box<dyn std::error::Error>> {
    let mut req = agent
        .get("https://api.github.com/repositories")
        .query("since", &since_id.to_string())
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .set("User-Agent", "gh_package_finder/0.1");
    if !token.is_empty() {
        req = req.set("Authorization", &format!("token {}", token));
    }
    let call = req.call();

    let resp = match call {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            eprintln!("Erreur API GitHub (repositories), statut {}: {}", code, body);
            return Ok(Vec::new());
        }
        Err(e) => {
            eprintln!("Erreur réseau lors du listing: {}", e);
            return Ok(Vec::new());
        }
    };

    wait_if_rate_limited(&resp);
    let items: Vec<RepoListItem> = resp.into_json()?;
    Ok(items)
}

// --- Contact sécurité (divulgation responsable) -----------------------------------------

// Interroge l'API "community profile" de GitHub pour savoir si le dépôt a un SECURITY.md.
// Fonctionne aussi sans token (limite anonyme de 60 req/h), mais un token permet le quota
// principal (5000/h) — n'est de toute façon appelé que pour les dépôts réellement signalés,
// un sous-ensemble généralement restreint.
pub fn get_security_policy_url(agent: &Agent, token: &str, repo_full_name: &str) -> Option<Option<String>> {
    let url = format!("https://api.github.com/repos/{}/community/profile", repo_full_name);
    let mut req = agent
        .get(&url)
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .set("User-Agent", "gh_package_finder/0.1");
    if !token.is_empty() {
        req = req.set("Authorization", &format!("token {}", token));
    }

    let resp = match req.call() {
        Ok(r) => r,
        Err(_) => return None, // 404 (fork sans profil), rate limit, erreur réseau...
    };
    wait_if_rate_limited(&resp);

    let json: serde_json::Value = resp.into_json().ok()?;
    let security_url = json
        .get("files")
        .and_then(|f| f.get("security"))
        .and_then(|s| s.get("html_url"))
        .and_then(|u| u.as_str())
        .map(|s| s.to_string());

    Some(security_url)
}

// Encode un chemin pour raw.githubusercontent.com en préservant les "/" mais en
// pourcent-encodant chaque segment (espaces, caractères spéciaux, etc.).
fn encode_path_segments(path: &str) -> String {
    path.split('/')
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn encode_segment(seg: &str) -> String {
    let mut out = String::new();
    for b in seg.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'@' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

// --- Métriques de dépôt (score de sensibilité) ------------------------------------------

pub struct RepoMetrics {
    pub stars: i64,
    pub forks: i64,
    // Date ISO 8601 du dernier push, telle que renvoyée par l'API (ex: "2024-05-01T12:00:00Z")
    pub pushed_at: Option<String>,
}

// Récupère les métriques de base d'un dépôt (étoiles, forks, dernière activité). Fonctionne
// sans token (limite anonyme de 60 req/h), mais n'est appelé que pour les dépôts déjà
// signalés — un sous-ensemble généralement restreint.
pub fn get_repo_metrics(agent: &Agent, token: &str, repo_full_name: &str) -> Option<RepoMetrics> {
    let url = format!("https://api.github.com/repos/{}", repo_full_name);
    let mut req = agent
        .get(&url)
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .set("User-Agent", "gh_package_finder/0.1");
    if !token.is_empty() {
        req = req.set("Authorization", &format!("token {}", token));
    }

    let resp = req.call().ok()?;
    wait_if_rate_limited(&resp);

    let json: serde_json::Value = resp.into_json().ok()?;
    Some(RepoMetrics {
        stars: json.get("stargazers_count").and_then(|v| v.as_i64()).unwrap_or(0),
        forks: json.get("forks_count").and_then(|v| v.as_i64()).unwrap_or(0),
        pushed_at: json.get("pushed_at").and_then(|v| v.as_str()).map(str::to_string),
    })
}

pub struct RawFetchResult {
    pub body: String,
    pub etag: Option<String>,
}

// Récupère un fichier via le CDN raw.githubusercontent.com/{repo}/HEAD/{path} : ce CDN
// n'est PAS soumis au rate limit de l'API principale (api.github.com), contrairement à
// l'API "contents". "HEAD" est un alias officiel pour la branche par défaut, donc aucun
// appel supplémentaire n'est nécessaire pour connaître le nom de cette branche.
pub fn raw_get(agent: &Agent, full_name: &str, path: &str) -> Option<RawFetchResult> {
    let url = format!(
        "https://raw.githubusercontent.com/{}/HEAD/{}",
        full_name,
        encode_path_segments(path)
    );
    match agent.get(&url).set("User-Agent", "gh_package_finder/0.1").call() {
        Ok(resp) => {
            let etag = resp.header("etag").map(|s| s.to_string());
            match resp.into_string() {
                Ok(body) => Some(RawFetchResult { body, etag }),
                Err(_) => None,
            }
        }
        Err(_) => None,
    }
}

// Simple HEAD, utilisé pour vérifier l'existence/l'ETag d'un fichier sans télécharger son
// contenu (ex: sonder la présence d'un lockfile avant de le récupérer en entier).
pub fn raw_head_etag(agent: &Agent, full_name: &str, path: &str) -> Option<Option<String>> {
    let url = format!(
        "https://raw.githubusercontent.com/{}/HEAD/{}",
        full_name,
        encode_path_segments(path)
    );
    match agent.head(&url).set("User-Agent", "gh_package_finder/0.1").call() {
        Ok(resp) => Some(resp.header("etag").map(|s| s.to_string())),
        Err(_) => None,
    }
}

// Repli sur l'API "contents" (compte dans le quota principal) si le CDN raw échoue, par
// exemple pour un dépôt renommé/déplacé entre l'indexation et le téléchargement.
pub fn contents_api_get(agent: &Agent, token: &str, contents_url: &str) -> Option<Vec<u8>> {
    let call = agent
        .get(contents_url)
        .set("Authorization", &format!("token {}", token))
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .set("User-Agent", "gh_package_finder/0.1")
        .call();

    let resp = match call {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            eprintln!("Repli API contents échoué, statut {}: {}", code, body);
            return None;
        }
        Err(e) => {
            eprintln!("Repli API contents : erreur réseau: {}", e);
            return None;
        }
    };

    wait_if_rate_limited(&resp);

    let content_data: ContentResponse = match resp.into_json() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Réponse invalide (repli contents API): {}", e);
            return None;
        }
    };

    match (content_data.content, content_data.encoding.as_deref()) {
        (Some(encoded), Some("base64")) => {
            let cleaned: String = encoded.chars().filter(|c| !c.is_whitespace()).collect();
            STANDARD.decode(cleaned).ok()
        }
        _ => None,
    }
}

// Récupère le contenu d'un fichier en tentant d'abord le CDN raw (gratuit en quota), puis
// en repliant sur l'API contents (payante en quota) si le CDN échoue.
pub fn fetch_file_content(
    agent: &Agent,
    token: &str,
    full_name: &str,
    path: &str,
    contents_url: &str,
) -> Option<Vec<u8>> {
    if let Some(result) = raw_get(agent, full_name, path) {
        return Some(result.body.into_bytes());
    }
    contents_api_get(agent, token, contents_url)
}

use crate::db;
use crate::github;
use rusqlite::Connection;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use ureq::Agent;

// Fichiers qu'on sonde systématiquement à côté de chaque package.json trouvé, via le CDN
// raw.githubusercontent.com (gratuit en quota API). kind=None pour .npmrc (traité à part).
const LOCKFILE_CANDIDATES: &[(&str, &str)] = &[
    ("package-lock.json", "package-lock.json"),
    ("yarn.lock", "yarn.lock"),
    ("pnpm-lock.yaml", "pnpm-lock.yaml"),
];
const NPMRC_FILENAME: &str = ".npmrc";

#[derive(Debug, Clone)]
pub struct DependencyOccurrence {
    pub package_name: String,
    pub version_spec: String,
    pub dep_type: &'static str,
    pub repo_full_name: String,
    pub repo_url: String,
    pub path: String,
}

// Certaines "dépendances" ne sont jamais résolues depuis le registre npm public : elles ne
// représentent donc aucun risque de dependency confusion et sont exclues de l'analyse.
fn is_registry_installable(version_spec: &str) -> bool {
    let v = version_spec.trim();
    if v.is_empty() {
        return false;
    }
    let excluded_prefixes = [
        "file:", "link:", "workspace:", "git+", "git:", "github:", "gitlab:", "bitbucket:",
        "http://", "https://", "npm:", "portal:", "patch:",
    ];
    !excluded_prefixes.iter().any(|p| v.starts_with(p))
}

// De nombreux faux positifs viennent de package.json qui ne représentent pas un vrai
// projet en production : templates de générateurs de projet (Yeoman, create-*, etc.),
// fixtures de tests, exemples de documentation, ou des dépendances déjà installées et
// committées telles quelles (node_modules vendorisé, snapshot de build)... Leurs noms de
// paquets sont souvent fictifs/placeholders ou des artefacts internes d'outillage, jamais
// vraiment "demandés" par un humain. On exclut donc entièrement ces fichiers plutôt que de
// les signaler.
//
// Volontairement PAS de "test"/"tests"/"spec"/"e2e" ici : un dossier de tests peut très
// bien contenir un package.json réel avec de vraies dépendances internes à risque — ce
// n'est pas un signal fiable de contenu fictif, contrairement à "template"/"fixture"/etc.
const NON_PRODUCTION_PATH_SEGMENTS: &[&str] = &[
    "template", "templates", "boilerplate", "boilerplates", "scaffold", "scaffolding",
    "starter", "starters", "example", "examples", "sample", "samples", "demo", "demos",
    "fixture", "fixtures", "__fixtures__", "__mocks__", "mock", "mocks", "stub", "stubs",
    // Dépendances déjà installées/vendorisées committées dans le dépôt : leurs propres
    // package.json ne représentent jamais les dépendances VOULUES par le projet analysé,
    // et peuvent même contenir des clés de noms internes malformées (ex: dossiers de
    // dédoublonnage npm/pnpm du type "_paquet@1.2.3@paquet") qui ne sont pas de vrais noms
    // de dépendances à vérifier.
    "node_modules", "bower_components", ".pnpm", ".yarn", "vendor", "vendored",
];

pub fn is_likely_non_production_path(path: &str) -> bool {
    Path::new(path)
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .any(|seg| {
            let lower = seg.to_lowercase();
            NON_PRODUCTION_PATH_SEGMENTS.contains(&lower.as_str())
        })
}

// Validation stricte inspirée des règles réelles de npm (validate-npm-package-name) :
// rejette tout ce qui n'est de toute façon pas un nom de paquet npm valide. Filet de
// sécurité contre les clés malformées trouvées dans certains package.json non standards
// (générés par des outils internes, exports de lockfiles, snapshots...), qui donneraient
// systématiquement un faux "absent de npm" sans représenter un vrai nom de dépendance.
pub fn is_valid_npm_package_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 214 {
        return false;
    }
    if name.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    if name.chars().any(|c| c.is_uppercase()) {
        return false; // npm force les noms en minuscules
    }

    let (scope, rest) = match name.strip_prefix('@') {
        Some(after_at) => match after_at.split_once('/') {
            Some((scope, rest)) => (Some(scope), rest),
            None => return false, // '@' sans '/' associé : pas un scope valide
        },
        None => (None, name),
    };

    let is_valid_segment = |s: &str| {
        !s.is_empty()
            && !s.starts_with('.')
            && !s.starts_with('_')
            && !s.contains('@')
            && !s.contains('/')
            && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~'))
    };

    if let Some(scope) = scope {
        if !is_valid_segment(scope) {
            return false;
        }
    }
    is_valid_segment(rest)
}

pub fn extract_dependencies(
    json: &Value,
    repo_full_name: &str,
    repo_url: &str,
    path: &str,
    out: &mut Vec<DependencyOccurrence>,
) {
    if is_likely_non_production_path(path) {
        return; // template/fixture/exemple : pas un vrai projet, on ignore entièrement
    }

    // Le nom propre du package, pour exclure l'auto-référence (un package qui se liste
    // lui-même en dépendance — arrive dans certains monorepos/outillages de build).
    let own_name = json.get("name").and_then(|v| v.as_str()).map(str::to_string);

    const FIELDS: &[&str] = &[
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ];
    for &field in FIELDS {
        if let Some(obj) = json.get(field).and_then(|v| v.as_object()) {
            for (name, version_val) in obj {
                if own_name.as_deref() == Some(name.as_str()) {
                    continue; // auto-référence : pas un vrai risque
                }
                if !is_valid_npm_package_name(name) {
                    continue; // pas un nom de paquet npm valide : bruit, pas une vraie dépendance
                }
                let version_spec = version_val.as_str().unwrap_or("").to_string();
                if !is_registry_installable(&version_spec) {
                    continue;
                }
                out.push(DependencyOccurrence {
                    package_name: name.clone(),
                    version_spec,
                    dep_type: field,
                    repo_full_name: repo_full_name.to_string(),
                    repo_url: repo_url.to_string(),
                    path: path.to_string(),
                });
            }
        }
    }
}

// --- Sondage des fichiers voisins (lockfiles + .npmrc) --------------------------------
//
// Contrairement à package.json, ces fichiers ne sont PAS recherchés via /search/code (ce
// qui économise énormément de requêtes sur l'API bridée à 30/min) : on profite simplement
// d'avoir déjà localisé un package.json pour sonder, dans le même dossier, la présence de
// ses lockfiles et d'un .npmrc via le CDN raw (gratuit en quota).

pub fn probe_sibling_files(
    agent: &Agent,
    conn: &Connection,
    repo_full_name: &str,
    package_json_path: &str,
    local_repo_dir: &Path,
) {
    let dir = Path::new(package_json_path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    for (kind, filename) in LOCKFILE_CANDIDATES {
        let remote_path = join_remote(&dir, filename);
        probe_and_save_lockfile(agent, conn, repo_full_name, &remote_path, kind, local_repo_dir);
    }

    let npmrc_remote_path = join_remote(&dir, NPMRC_FILENAME);
    probe_and_save_npmrc(agent, conn, repo_full_name, &npmrc_remote_path, local_repo_dir);
}

fn join_remote(dir: &str, filename: &str) -> String {
    if dir.is_empty() {
        filename.to_string()
    } else {
        format!("{}/{}", dir, filename)
    }
}

fn probe_and_save_lockfile(
    agent: &Agent,
    conn: &Connection,
    repo_full_name: &str,
    remote_path: &str,
    kind: &str,
    local_repo_dir: &Path,
) {
    // HEAD léger d'abord : évite de retélécharger un gros lockfile inchangé.
    let head_etag = match github::raw_head_etag(agent, repo_full_name, remote_path) {
        Some(etag) => etag,
        None => return, // absent (404) ou erreur : rien à faire
    };

    if let Ok(Some(known_etag)) = db::known_lockfile_etag(conn, repo_full_name, remote_path) {
        if known_etag == head_etag {
            return; // inchangé depuis la dernière fois, on ne retélécharge pas
        }
    }

    let Some(result) = github::raw_get(agent, repo_full_name, remote_path) else {
        return;
    };

    let local_path: PathBuf = local_repo_dir.join(remote_path);
    if let Some(parent) = local_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if fs::write(&local_path, &result.body).is_err() {
        return;
    }

    println!("  [lockfile] -> {}", local_path.display());
    let _ = db::upsert_lockfile(
        conn,
        repo_full_name,
        remote_path,
        kind,
        result.etag.as_deref(),
        &db::now_iso8601(),
    );
}

fn probe_and_save_npmrc(
    agent: &Agent,
    conn: &Connection,
    repo_full_name: &str,
    remote_path: &str,
    local_repo_dir: &Path,
) {
    let Some(result) = github::raw_get(agent, repo_full_name, remote_path) else {
        return;
    };

    let local_path: PathBuf = local_repo_dir.join(remote_path);
    if let Some(parent) = local_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&local_path, &result.body);

    let scopes = parse_npmrc_private_scopes(&result.body);
    if !scopes.is_empty() {
        println!(
            "  [.npmrc] {} scope(s) privé(s) détecté(s) : {}",
            scopes.len(),
            scopes.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>().join(", ")
        );
    }
    for (scope, registry_url) in scopes {
        let _ = db::upsert_npmrc_config(
            conn,
            repo_full_name,
            remote_path,
            &scope,
            &registry_url,
            &db::now_iso8601(),
        );
    }
}

// --- Parsing .npmrc --------------------------------------------------------------------

// Cherche les lignes du type "@scope:registry=https://..." pointant vers un registre autre
// que le registre npm public — signal fort qu'un scope est utilisé en interne.
pub fn parse_npmrc_private_scopes(content: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') || !line.starts_with('@') {
            continue;
        }
        if let Some(idx) = line.find(":registry=") {
            let scope = line[..idx].to_string();
            let url = line[idx + ":registry=".len()..].trim().to_string();
            if !url.contains("registry.npmjs.org") {
                out.push((scope, url));
            }
        }
    }
    out
}

// --- Parsing package-lock.json (npm v1/v2/v3) -------------------------------------------

pub fn parse_package_lock(json: &Value) -> HashMap<String, String> {
    let mut map = HashMap::new();

    // Format v2/v3 : "packages": { "node_modules/name": { "resolved": "url", ... } }
    if let Some(packages) = json.get("packages").and_then(|v| v.as_object()) {
        for (key, val) in packages {
            if key.is_empty() {
                continue; // entrée racine du projet lui-même
            }
            if let Some(name) = key.rsplit("node_modules/").next() {
                if let Some(resolved) = val.get("resolved").and_then(|r| r.as_str()) {
                    map.insert(name.to_string(), resolved.to_string());
                }
            }
        }
    }

    // Format v1 : "dependencies": { "name": { "resolved": "url", "dependencies": {...} } }
    if let Some(deps) = json.get("dependencies").and_then(|v| v.as_object()) {
        collect_v1_lock_deps(deps, &mut map);
    }

    map
}

fn collect_v1_lock_deps(deps: &serde_json::Map<String, Value>, map: &mut HashMap<String, String>) {
    for (name, val) in deps {
        if let Some(resolved) = val.get("resolved").and_then(|r| r.as_str()) {
            map.insert(name.clone(), resolved.to_string());
        }
        if let Some(nested) = val.get("dependencies").and_then(|v| v.as_object()) {
            collect_v1_lock_deps(nested, map);
        }
    }
}

// --- Parsing yarn.lock (format classique v1) --------------------------------------------
//
// Parseur volontairement simple, ligne par ligne (pas de dépendance à un parseur YAML
// complet, le format yarn.lock classique n'étant de toute façon pas du YAML valide).
// Limitation connue : le format "Yarn Berry" (v2+) peut différer légèrement et n'est pas
// garanti d'être couvert à 100%.

pub fn parse_yarn_lock(content: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut current_names: Vec<String> = Vec::new();

    for line in content.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !line.starts_with(' ') && !line.starts_with('\t') && line.trim_end().ends_with(':') {
            current_names.clear();
            let header = line.trim_end().trim_end_matches(':');
            for part in header.split(',') {
                let part = part.trim().trim_matches('"');
                if let Some(name) = extract_yarn_pkg_name(part) {
                    current_names.push(name);
                }
            }
        } else if let Some(rest) = line.trim().strip_prefix("resolved ") {
            let url = rest.trim().trim_matches('"').to_string();
            for name in &current_names {
                map.insert(name.clone(), url.clone());
            }
        }
    }

    map
}

fn extract_yarn_pkg_name(spec: &str) -> Option<String> {
    if let Some(rest) = spec.strip_prefix('@') {
        rest.find('@').map(|idx| format!("@{}", &rest[..idx]))
    } else {
        spec.find('@').map(|idx| spec[..idx].to_string())
    }
}

// --- Corrélation avec les scopes privés détectés ----------------------------------------

// Un paquet est "confirmé privé" si son scope correspond à un scope déclaré dans un .npmrc
// pointant vers un registre non-npm, détecté n'importe où dans le jeu de données.
pub fn is_confirmed_private_scope(package_name: &str, private_scopes: &HashSet<String>) -> bool {
    if let Some(idx) = package_name.find('/') {
        if package_name.starts_with('@') {
            return private_scopes.contains(&package_name[..idx]);
        }
    }
    false
}

// --- Rapport CSV -------------------------------------------------------------------------

pub fn write_report_csv(
    path: &str,
    occurrences: &[DependencyOccurrence],
    npm_status: &HashMap<String, bool>,
    lockfile_resolved: &HashMap<(String, String), String>, // (repo, package_name) -> resolved url
    private_scopes: &HashSet<String>,
    monorepo_resolved: &HashSet<(String, String)>, // (repo, package_name) exclus : résolu localement
    org_match: &HashSet<(String, String)>,         // (repo, package_name) vu dans un autre dépôt du même org
    scope_claimed_safe: &HashSet<(String, String)>, // (repo, package_name) exclus : scope npm déjà revendiqué
    repo_scores: &HashMap<String, u8>,             // repo_full_name -> score de sensibilité 0-10
) -> std::io::Result<usize> {
    let mut lines = vec![
        "package_name,dep_type,version_spec,repo_full_name,repo_url,path,confidence,lockfile_resolved_url,repo_sensitivity_score,repo_sensitivity_label"
            .to_string(),
    ];
    let mut count = 0;

    for occ in occurrences {
        if npm_status.get(&occ.package_name) != Some(&false) {
            continue; // pas confirmé absent de npm : pas de risque de confusion
        }
        let key = (occ.repo_full_name.clone(), occ.package_name.clone());
        if monorepo_resolved.contains(&key) {
            continue; // faux positif : nom résolu par un autre package.json du même dépôt
        }
        if scope_claimed_safe.contains(&key) {
            continue; // faux positif : scope npm déjà revendiqué, non publiable par un tiers
        }
        count += 1;

        let resolved = lockfile_resolved
            .get(&(occ.repo_full_name.clone(), occ.package_name.clone()))
            .cloned();
        let scope_confirmed = is_confirmed_private_scope(&occ.package_name, private_scopes);

        let confidence = if resolved.is_some() || scope_confirmed {
            "élevée (confirmé par lockfile/.npmrc)"
        } else if org_match.contains(&key) {
            "faible (nom vu dans un autre dépôt du même org — probable poly-repo)"
        } else {
            "moyenne (absent de npm uniquement)"
        };

        let score = repo_scores.get(&occ.repo_full_name).copied();
        let score_display = score.map(|s| s.to_string()).unwrap_or_default();
        let label_display = score.map(crate::sensitivity::score_label).unwrap_or("");

        lines.push(format!(
            "{},{},{},{},{},{},{},{},{},{}",
            csv_escape(&occ.package_name),
            occ.dep_type,
            csv_escape(&occ.version_spec),
            csv_escape(&occ.repo_full_name),
            csv_escape(&occ.repo_url),
            csv_escape(&occ.path),
            confidence,
            csv_escape(resolved.as_deref().unwrap_or("")),
            score_display,
            csv_escape(label_display),
        ));
    }

    fs::write(path, lines.join("\n"))?;
    Ok(count)
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_npmrc_private_scopes() {
        let content = "@acme-corp:registry=https://npm.acme-corp.internal/\n@types:registry=https://registry.npmjs.org/\nregistry=https://registry.npmjs.org/\n";
        let scopes = parse_npmrc_private_scopes(content);
        assert_eq!(scopes.len(), 1);
        assert_eq!(scopes[0].0, "@acme-corp");
        assert_eq!(scopes[0].1, "https://npm.acme-corp.internal/");
    }

    #[test]
    fn test_parse_package_lock_v3() {
        let json: Value = serde_json::from_str(
            r#"{
                "packages": {
                    "": {"name": "test"},
                    "node_modules/react": {"resolved": "https://registry.npmjs.org/react/-/react-18.0.0.tgz"},
                    "node_modules/acme-secret-lib": {"resolved": "https://npm.acme-corp.internal/acme-secret-lib/-/acme-secret-lib-2.0.0.tgz"}
                }
            }"#,
        )
        .unwrap();
        let map = parse_package_lock(&json);
        assert_eq!(map.get("react").unwrap(), "https://registry.npmjs.org/react/-/react-18.0.0.tgz");
        assert_eq!(
            map.get("acme-secret-lib").unwrap(),
            "https://npm.acme-corp.internal/acme-secret-lib/-/acme-secret-lib-2.0.0.tgz"
        );
    }

    #[test]
    fn test_parse_yarn_lock() {
        let content = "\"@types/node@^18.0.0\":\n  version \"18.15.0\"\n  resolved \"https://registry.yarnpkg.com/@types/node/-/node-18.15.0.tgz#abc\"\n\nacme-internal-lib@^1.0.0, acme-internal-lib@^1.2.0:\n  version \"1.2.0\"\n  resolved \"https://npm.acme-corp.internal/acme-internal-lib/-/acme-internal-lib-1.2.0.tgz#def\"\n";
        let map = parse_yarn_lock(content);
        assert_eq!(
            map.get("@types/node").unwrap(),
            "https://registry.yarnpkg.com/@types/node/-/node-18.15.0.tgz#abc"
        );
        assert_eq!(
            map.get("acme-internal-lib").unwrap(),
            "https://npm.acme-corp.internal/acme-internal-lib/-/acme-internal-lib-1.2.0.tgz#def"
        );
    }

    #[test]
    fn test_is_registry_installable() {
        assert!(is_registry_installable("^1.0.0"));
        assert!(!is_registry_installable("file:../local"));
        assert!(!is_registry_installable("workspace:*"));
        assert!(!is_registry_installable("git+https://github.com/x/y.git"));
        assert!(!is_registry_installable(""));
    }

    #[test]
    fn test_is_valid_npm_package_name() {
        // Cas réel signalé : artefact de dossier node_modules dédupliqué, pas un vrai nom.
        assert!(!is_valid_npm_package_name("_eslint-scope@4.0.3@eslint-scope"));
        assert!(!is_valid_npm_package_name("_leading-underscore"));
        assert!(!is_valid_npm_package_name(".leading-dot"));
        assert!(!is_valid_npm_package_name("Has-Uppercase"));
        assert!(!is_valid_npm_package_name(""));
        assert!(!is_valid_npm_package_name("has space"));
        assert!(!is_valid_npm_package_name("@scope-no-slash"));
        assert!(!is_valid_npm_package_name("two@at@signs"));

        assert!(is_valid_npm_package_name("react"));
        assert!(is_valid_npm_package_name("eslint-scope"));
        assert!(is_valid_npm_package_name("@types/node"));
        assert!(is_valid_npm_package_name("@acme-corp/ui-kit"));
        assert!(is_valid_npm_package_name("lodash.merge"));
    }

    #[test]
    fn test_extract_dependencies_rejects_malformed_names() {
        let json: Value = serde_json::from_str(
            r#"{"dependencies": {"_eslint-scope@4.0.3@eslint-scope": "4.0.3", "react": "^18.0.0"}}"#,
        )
        .unwrap();
        let mut out = Vec::new();
        extract_dependencies(&json, "org/repo", "https://github.com/org/repo", "package.json", &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].package_name, "react");
    }

    #[test]
    fn test_is_likely_non_production_path_node_modules() {
        assert!(is_likely_non_production_path(
            "vendor/some-lib/node_modules/_eslint-scope@4.0.3@eslint-scope/package.json"
        ));
        assert!(is_likely_non_production_path("node_modules/react/package.json"));
        assert!(!is_likely_non_production_path("packages/app/package.json"));
    }

    #[test]
    fn test_is_confirmed_private_scope() {
        let mut scopes = HashSet::new();
        scopes.insert("@acme-corp".to_string());
        assert!(is_confirmed_private_scope("@acme-corp/some-lib", &scopes));
        assert!(!is_confirmed_private_scope("@other/some-lib", &scopes));
        assert!(!is_confirmed_private_scope("plain-lib", &scopes));
    }

    #[test]
    fn test_is_likely_non_production_path() {
        assert!(is_likely_non_production_path("templates/react-app/package.json"));
        assert!(is_likely_non_production_path("packages/generator/templates/default/package.json"));
        assert!(is_likely_non_production_path("examples/basic-usage/package.json"));
        assert!(is_likely_non_production_path("test/__fixtures__/broken-app/package.json"));
        assert!(!is_likely_non_production_path("packages/backend-api/package.json"));
        assert!(!is_likely_non_production_path("apps/web/test/package.json")); // "test" seul: pas exclu
        assert!(!is_likely_non_production_path("package.json"));
    }

    #[test]
    fn test_extract_dependencies_excludes_self_reference() {
        let json: Value = serde_json::from_str(
            r#"{
                "name": "my-package",
                "dependencies": {
                    "my-package": "^1.0.0",
                    "react": "^18.0.0"
                }
            }"#,
        )
        .unwrap();
        let mut out = Vec::new();
        extract_dependencies(&json, "org/repo", "https://github.com/org/repo", "package.json", &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].package_name, "react");
    }

    #[test]
    fn test_extract_dependencies_skips_template_paths() {
        let json: Value =
            serde_json::from_str(r#"{"dependencies": {"totally-fake-placeholder-pkg": "^1.0.0"}}"#).unwrap();
        let mut out = Vec::new();
        extract_dependencies(
            &json,
            "org/repo",
            "https://github.com/org/repo",
            "templates/default/package.json",
            &mut out,
        );
        assert!(out.is_empty());
    }
}

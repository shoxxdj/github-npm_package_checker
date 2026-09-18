use rusqlite::{params, Connection, OptionalExtension};

pub fn init_db(path: &str) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;

    // Mode WAL : autorise un écrivain (le crawl/la recherche en cours) et plusieurs
    // lecteurs simultanés (ex: lancer `--analyse` dans un second terminal pendant qu'un
    // --crawl tourne toujours) sans verrouillage mutuel.
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;

    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS packages (
            repo_full_name TEXT NOT NULL,
            path           TEXT NOT NULL,
            repo_url       TEXT NOT NULL,
            file_sha       TEXT NOT NULL,
            discovered_at  TEXT NOT NULL,
            analyzed       INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (repo_full_name, path)
        );
        CREATE INDEX IF NOT EXISTS idx_packages_repo ON packages(repo_full_name);
        CREATE INDEX IF NOT EXISTS idx_packages_analyzed ON packages(analyzed);

        -- Cache des vérifications sur le registre npm public.
        CREATE TABLE IF NOT EXISTS npm_registry_cache (
            package_name   TEXT PRIMARY KEY,
            exists_on_npm  INTEGER NOT NULL,
            checked_at     TEXT NOT NULL
        );

        -- Lockfiles trouvés à côté d'un package.json déjà connu (package-lock.json,
        -- yarn.lock, pnpm-lock.yaml). 'etag' vient de la réponse raw.githubusercontent.com
        -- et sert d'identifiant de version (pas de sha fourni par la recherche pour ces
        -- fichiers, puisqu'on ne les recherche pas via /search/code).
        CREATE TABLE IF NOT EXISTS lockfiles (
            repo_full_name TEXT NOT NULL,
            path           TEXT NOT NULL,
            kind           TEXT NOT NULL,
            etag           TEXT,
            discovered_at  TEXT NOT NULL,
            PRIMARY KEY (repo_full_name, path)
        );

        -- Scopes npm configurés vers un registre privé, détectés dans un .npmrc trouvé à
        -- côté d'un package.json. Sert de signal fort pour confirmer un vrai paquet interne.
        CREATE TABLE IF NOT EXISTS npmrc_configs (
            repo_full_name TEXT NOT NULL,
            path           TEXT NOT NULL,
            scope          TEXT NOT NULL,
            registry_url   TEXT NOT NULL,
            discovered_at  TEXT NOT NULL,
            PRIMARY KEY (repo_full_name, path, scope)
        );

        -- Curseur de progression pour le mode --crawl (parcours exhaustif de
        -- /repositories?since=), afin de pouvoir reprendre une exécution interrompue.
        CREATE TABLE IF NOT EXISTS crawl_state (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        -- Occurrences de dépendances déjà extraites d'un package.json par un thread
        -- d'analyse. Persistées immédiatement (indépendamment de la vérification npm) afin
        -- qu'une interruption du programme ne fasse perdre ni le parsing déjà effectué, ni
        -- la trace des paquets restant à vérifier.
        CREATE TABLE IF NOT EXISTS dependency_occurrences (
            repo_full_name TEXT NOT NULL,
            path           TEXT NOT NULL,
            package_name   TEXT NOT NULL,
            dep_type       TEXT NOT NULL,
            version_spec   TEXT NOT NULL,
            repo_url       TEXT NOT NULL,
            PRIMARY KEY (repo_full_name, path, package_name, dep_type)
        );
        CREATE INDEX IF NOT EXISTS idx_dep_occ_name ON dependency_occurrences(package_name);

        -- Nom propre déclaré ('name' field) de chaque package.json indexé. Sert à détecter
        -- les faux positifs de type monorepo : une dépendance 'absente de npm' qui
        -- correspond en fait au nom d'un AUTRE package.json du même dépôt (ou d'un dépôt de
        -- la même organisation) est très probablement résolue localement via workspace,
        -- pas un vrai risque de dependency confusion.
        CREATE TABLE IF NOT EXISTS declared_package_names (
            repo_full_name TEXT NOT NULL,
            path           TEXT NOT NULL,
            name           TEXT NOT NULL,
            PRIMARY KEY (repo_full_name, path)
        );
        CREATE INDEX IF NOT EXISTS idx_declared_names_name ON declared_package_names(name);

        -- Cache des vérifications de revendication de scope npm (un scope claimé/non
        -- claimé change rarement, mais jamais aussi vite qu'un paquet précis).
        CREATE TABLE IF NOT EXISTS npm_scope_cache (
            scope        TEXT PRIMARY KEY,
            claimed      INTEGER NOT NULL,
            checked_at   TEXT NOT NULL
        );

        -- Contact sécurité connu pour un dépôt (SECURITY.md détecté via l'API GitHub
        -- 'community profile'), mis en cache pour ne pas revérifier à chaque rapport.
        CREATE TABLE IF NOT EXISTS security_contacts (
            repo_full_name      TEXT PRIMARY KEY,
            has_security_policy INTEGER NOT NULL,
            security_policy_url TEXT,
            checked_at          TEXT NOT NULL
        );

        -- Score de sensibilité du dépôt (0-10, basé sur étoiles/forks/dernière activité),
        -- mis en cache pour ne pas revérifier à chaque rapport. Les métriques brutes sont
        -- conservées pour affichage/débogage.
        CREATE TABLE IF NOT EXISTS repo_sensitivity (
            repo_full_name TEXT PRIMARY KEY,
            score          INTEGER NOT NULL,
            stars          INTEGER NOT NULL,
            forks          INTEGER NOT NULL,
            pushed_at      TEXT,
            checked_at     TEXT NOT NULL
        );
        ",
    )?;
    Ok(conn)
}

pub fn now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339()
}

// --- packages ------------------------------------------------------------

pub fn known_sha(conn: &Connection, repo_full_name: &str, path: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT file_sha FROM packages WHERE repo_full_name = ?1 AND path = ?2",
        params![repo_full_name, path],
        |row| row.get(0),
    )
    .optional()
}

pub fn upsert_package(
    conn: &Connection,
    repo_full_name: &str,
    path: &str,
    repo_url: &str,
    sha: &str,
    discovered_at: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO packages (repo_full_name, path, repo_url, file_sha, discovered_at, analyzed)
         VALUES (?1, ?2, ?3, ?4, ?5, 0)
         ON CONFLICT(repo_full_name, path)
         DO UPDATE SET file_sha = excluded.file_sha, repo_url = excluded.repo_url, analyzed = 0",
        params![repo_full_name, path, repo_url, sha, discovered_at],
    )?;
    Ok(())
}

pub fn mark_analyzed(conn: &Connection, repo_full_name: &str, path: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE packages SET analyzed = 1 WHERE repo_full_name = ?1 AND path = ?2",
        params![repo_full_name, path],
    )?;
    Ok(())
}

// Dépôts dont le package.json n'a pas encore été traité par un thread d'analyse — utilisé
// au démarrage pour rattraper le travail laissé en suspens par une exécution interrompue
// (fichier déjà téléchargé, mais encore dans la file au moment de l'arrêt).
pub fn unanalyzed_packages(conn: &Connection) -> rusqlite::Result<Vec<(String, String, String)>> {
    let mut stmt =
        conn.prepare("SELECT repo_full_name, path, repo_url FROM packages WHERE analyzed = 0")?;
    let rows = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn all_packages(conn: &Connection) -> rusqlite::Result<Vec<(String, String, String)>> {
    let mut stmt = conn.prepare("SELECT repo_full_name, path, repo_url FROM packages")?;
    let rows = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// --- npm_registry_cache ----------------------------------------------------

pub fn load_npm_cache(conn: &Connection) -> rusqlite::Result<std::collections::HashMap<String, bool>> {
    let mut stmt = conn.prepare("SELECT package_name, exists_on_npm FROM npm_registry_cache")?;
    let rows = stmt.query_map([], |row| {
        let name: String = row.get(0)?;
        let exists: i64 = row.get(1)?;
        Ok((name, exists != 0))
    })?;
    let mut map = std::collections::HashMap::new();
    for r in rows {
        let (name, exists) = r?;
        map.insert(name, exists);
    }
    Ok(map)
}

pub fn save_npm_cache_batch(conn: &mut Connection, results: &[(String, bool)]) -> rusqlite::Result<()> {
    let now = now_iso8601();
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO npm_registry_cache (package_name, exists_on_npm, checked_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(package_name)
             DO UPDATE SET exists_on_npm = excluded.exists_on_npm, checked_at = excluded.checked_at",
        )?;
        for (name, exists) in results {
            stmt.execute(params![name, *exists as i64, now])?;
        }
    }
    tx.commit()?;
    Ok(())
}

// --- lockfiles -------------------------------------------------------------

pub fn known_lockfile_etag(
    conn: &Connection,
    repo_full_name: &str,
    path: &str,
) -> rusqlite::Result<Option<Option<String>>> {
    conn.query_row(
        "SELECT etag FROM lockfiles WHERE repo_full_name = ?1 AND path = ?2",
        params![repo_full_name, path],
        |row| row.get(0),
    )
    .optional()
}

pub fn upsert_lockfile(
    conn: &Connection,
    repo_full_name: &str,
    path: &str,
    kind: &str,
    etag: Option<&str>,
    discovered_at: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO lockfiles (repo_full_name, path, kind, etag, discovered_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(repo_full_name, path) DO UPDATE SET etag = excluded.etag",
        params![repo_full_name, path, kind, etag, discovered_at],
    )?;
    Ok(())
}

pub fn all_lockfiles(conn: &Connection) -> rusqlite::Result<Vec<(String, String, String)>> {
    let mut stmt = conn.prepare("SELECT repo_full_name, path, kind FROM lockfiles")?;
    let rows = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// --- npmrc_configs -----------------------------------------------------------

pub fn upsert_npmrc_config(
    conn: &Connection,
    repo_full_name: &str,
    path: &str,
    scope: &str,
    registry_url: &str,
    discovered_at: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO npmrc_configs (repo_full_name, path, scope, registry_url, discovered_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(repo_full_name, path, scope) DO UPDATE SET registry_url = excluded.registry_url",
        params![repo_full_name, path, scope, registry_url, discovered_at],
    )?;
    Ok(())
}

// Retourne l'ensemble des scopes ("@scope") connus comme pointant vers un registre privé.
pub fn all_private_scopes(conn: &Connection) -> rusqlite::Result<std::collections::HashSet<String>> {
    let mut stmt = conn.prepare("SELECT DISTINCT scope FROM npmrc_configs")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut set = std::collections::HashSet::new();
    for r in rows {
        set.insert(r?);
    }
    Ok(set)
}

// --- dependency_occurrences ----------------------------------------------------

pub fn upsert_occurrence(
    conn: &Connection,
    repo_full_name: &str,
    path: &str,
    package_name: &str,
    dep_type: &str,
    version_spec: &str,
    repo_url: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO dependency_occurrences
             (repo_full_name, path, package_name, dep_type, version_spec, repo_url)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(repo_full_name, path, package_name, dep_type)
         DO UPDATE SET version_spec = excluded.version_spec, repo_url = excluded.repo_url",
        params![repo_full_name, path, package_name, dep_type, version_spec, repo_url],
    )?;
    Ok(())
}

pub struct OccurrenceRow {
    pub package_name: String,
    pub dep_type: String,
    pub version_spec: String,
    pub repo_full_name: String,
    pub repo_url: String,
    pub path: String,
}

pub fn all_occurrences(conn: &Connection) -> rusqlite::Result<Vec<OccurrenceRow>> {
    let mut stmt = conn.prepare(
        "SELECT package_name, dep_type, version_spec, repo_full_name, repo_url, path
         FROM dependency_occurrences",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(OccurrenceRow {
                package_name: row.get(0)?,
                dep_type: row.get(1)?,
                version_spec: row.get(2)?,
                repo_full_name: row.get(3)?,
                repo_url: row.get(4)?,
                path: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// --- declared_package_names -----------------------------------------------------

pub fn upsert_declared_name(
    conn: &Connection,
    repo_full_name: &str,
    path: &str,
    name: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO declared_package_names (repo_full_name, path, name)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(repo_full_name, path) DO UPDATE SET name = excluded.name",
        params![repo_full_name, path, name],
    )?;
    Ok(())
}

// Le nom correspond-il à un package.json déclaré dans le MÊME dépôt (autre que celui
// d'origine) ? Signal fort : très probablement un package de workspace résolu localement.
pub fn name_declared_in_same_repo(
    conn: &Connection,
    repo_full_name: &str,
    name: &str,
    exclude_path: &str,
) -> rusqlite::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM declared_package_names
         WHERE repo_full_name = ?1 AND name = ?2 AND path != ?3",
        params![repo_full_name, name, exclude_path],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

// Le nom correspond-il à un package.json déclaré dans un AUTRE dépôt de la même
// organisation/utilisateur GitHub ? Signal plus faible (poly-repo probable, mais moins
// certain qu'un monorepo) : on ne l'exclut pas, on le signale avec une confiance dédiée.
pub fn name_declared_in_same_org(
    conn: &Connection,
    org: &str,
    name: &str,
    exclude_repo: &str,
) -> rusqlite::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM declared_package_names
         WHERE name = ?1 AND repo_full_name != ?2 AND repo_full_name LIKE ?3",
        params![name, exclude_repo, format!("{}/%", org)],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

// --- npm_scope_cache -----------------------------------------------------------

pub fn known_scope_claimed(conn: &Connection, scope: &str) -> rusqlite::Result<Option<bool>> {
    conn.query_row(
        "SELECT claimed FROM npm_scope_cache WHERE scope = ?1",
        params![scope],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .map(|v| v.map(|c| c != 0))
}

pub fn save_scope_claimed(conn: &Connection, scope: &str, claimed: bool) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO npm_scope_cache (scope, claimed, checked_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(scope) DO UPDATE SET claimed = excluded.claimed, checked_at = excluded.checked_at",
        params![scope, claimed as i64, now_iso8601()],
    )?;
    Ok(())
}

// --- security_contacts -----------------------------------------------------------

pub struct SecurityContactRow {
    pub has_security_policy: bool,
    pub security_policy_url: Option<String>,
}

pub fn known_security_contact(
    conn: &Connection,
    repo_full_name: &str,
) -> rusqlite::Result<Option<SecurityContactRow>> {
    conn.query_row(
        "SELECT has_security_policy, security_policy_url FROM security_contacts WHERE repo_full_name = ?1",
        params![repo_full_name],
        |row| {
            Ok(SecurityContactRow {
                has_security_policy: row.get::<_, i64>(0)? != 0,
                security_policy_url: row.get(1)?,
            })
        },
    )
    .optional()
}

pub fn save_security_contact(
    conn: &Connection,
    repo_full_name: &str,
    has_security_policy: bool,
    security_policy_url: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO security_contacts (repo_full_name, has_security_policy, security_policy_url, checked_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(repo_full_name)
         DO UPDATE SET has_security_policy = excluded.has_security_policy,
                       security_policy_url = excluded.security_policy_url,
                       checked_at = excluded.checked_at",
        params![repo_full_name, has_security_policy as i64, security_policy_url, now_iso8601()],
    )?;
    Ok(())
}

// --- repo_sensitivity -----------------------------------------------------------

pub struct SensitivityRow {
    pub score: i64,
    pub stars: i64,
    pub forks: i64,
    pub pushed_at: Option<String>,
}

pub fn known_sensitivity(conn: &Connection, repo_full_name: &str) -> rusqlite::Result<Option<SensitivityRow>> {
    conn.query_row(
        "SELECT score, stars, forks, pushed_at FROM repo_sensitivity WHERE repo_full_name = ?1",
        params![repo_full_name],
        |row| {
            Ok(SensitivityRow {
                score: row.get(0)?,
                stars: row.get(1)?,
                forks: row.get(2)?,
                pushed_at: row.get(3)?,
            })
        },
    )
    .optional()
}

pub fn save_sensitivity(
    conn: &Connection,
    repo_full_name: &str,
    score: u8,
    stars: i64,
    forks: i64,
    pushed_at: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO repo_sensitivity (repo_full_name, score, stars, forks, pushed_at, checked_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(repo_full_name)
         DO UPDATE SET score = excluded.score, stars = excluded.stars, forks = excluded.forks,
                       pushed_at = excluded.pushed_at, checked_at = excluded.checked_at",
        params![repo_full_name, score as i64, stars, forks, pushed_at, now_iso8601()],
    )?;
    Ok(())
}

// --- crawl_state -------------------------------------------------------------

pub fn get_crawl_cursor(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT value FROM crawl_state WHERE key = 'last_repo_id'",
        [],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map(|v| v.and_then(|s| s.parse::<i64>().ok()).unwrap_or(0))
}

pub fn set_crawl_cursor(conn: &Connection, since_id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO crawl_state (key, value) VALUES ('last_repo_id', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![since_id.to_string()],
    )?;
    Ok(())
}

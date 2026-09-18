use crate::db;
use crate::github;
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use ureq::Agent;

// Calcule un score de sensibilité 0-10 à partir des étoiles, forks et de la date du
// dernier push. L'idée : un dépôt très suivi (beaucoup d'étoiles/forks) et activement
// maintenu représente un risque bien plus élevé en cas de dependency confusion réussie
// (plus de monde installe/exécute son code) qu'un petit dépôt personnel à l'abandon.
//
// Barème (10 points au total) :
//   - étoiles  : jusqu'à 4 points, échelle logarithmique (100 000+ étoiles = max)
//   - forks    : jusqu'à 3 points, échelle logarithmique (20 000+ forks = max)
//   - activité : jusqu'à 3 points, selon l'ancienneté du dernier push
pub fn compute_score(stars: i64, forks: i64, pushed_at: Option<&str>) -> u8 {
    let stars_component = log_scale(stars, 100_000.0) * 4.0;
    let forks_component = log_scale(forks, 20_000.0) * 3.0;
    let recency_component = recency_score(pushed_at) * 3.0;

    let total = stars_component + forks_component + recency_component;
    total.round().clamp(0.0, 10.0) as u8
}

// Échelle logarithmique bornée à [0.0, 1.0] : évite qu'un dépôt à 500 000 étoiles écrase
// totalement le score par rapport à un dépôt à 50 000 (la différence de risque réel entre
// les deux est marginale comparée à un dépôt à 10 étoiles).
fn log_scale(value: i64, max_value: f64) -> f64 {
    if value <= 0 {
        return 0.0;
    }
    let ratio = ((value as f64) + 1.0).ln() / (max_value + 1.0).ln();
    ratio.clamp(0.0, 1.0)
}

fn recency_score(pushed_at: Option<&str>) -> f64 {
    let Some(pushed_at) = pushed_at else { return 0.0 };
    let Ok(pushed) = DateTime::parse_from_rfc3339(pushed_at) else {
        return 0.0;
    };
    let days_since = (Utc::now() - pushed.with_timezone(&Utc)).num_days();

    match days_since {
        d if d <= 30 => 1.0,
        d if d <= 90 => 0.8,
        d if d <= 180 => 0.6,
        d if d <= 365 => 0.4,
        d if d <= 730 => 0.2,
        _ => 0.0,
    }
}

// Description courte du niveau, pour l'affichage.
pub fn score_label(score: u8) -> &'static str {
    match score {
        0..=1 => "très faible (dépôt personnel peu actif)",
        2..=3 => "faible",
        4..=5 => "modéré",
        6..=7 => "élevé",
        8..=10 => "très élevé (projet largement utilisé et actif)",
        _ => "inconnu",
    }
}

// Récupère (avec cache) le score de sensibilité d'un dépôt.
pub fn get_sensitivity_score(conn: &Connection, agent: &Agent, token: &str, repo_full_name: &str) -> Option<u8> {
    if let Ok(Some(row)) = db::known_sensitivity(conn, repo_full_name) {
        return Some(row.score as u8);
    }

    let metrics = github::get_repo_metrics(agent, token, repo_full_name)?;
    let score = compute_score(metrics.stars, metrics.forks, metrics.pushed_at.as_deref());
    let _ = db::save_sensitivity(
        conn,
        repo_full_name,
        score,
        metrics.stars,
        metrics.forks,
        metrics.pushed_at.as_deref(),
    );
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_score_high_profile_active() {
        // Style express/react : très étoilé, très forké, mis à jour très récemment.
        let recent = Utc::now().to_rfc3339();
        let score = compute_score(60_000, 15_000, Some(&recent));
        assert!(score >= 9, "score attendu >= 9, obtenu {}", score);
    }

    #[test]
    fn test_compute_score_small_inactive_personal_repo() {
        // Dépôt personnel, 0 étoile, 0 fork, dernier push il y a 3 ans.
        let old = (Utc::now() - chrono::Duration::days(365 * 3)).to_rfc3339();
        let score = compute_score(0, 0, Some(&old));
        assert!(score <= 1, "score attendu <= 1, obtenu {}", score);
    }

    #[test]
    fn test_compute_score_no_pushed_at() {
        let score = compute_score(0, 0, None);
        assert_eq!(score, 0);
    }

    #[test]
    fn test_compute_score_monotonic_with_stars() {
        let recent = Utc::now().to_rfc3339();
        let low = compute_score(10, 5, Some(&recent));
        let high = compute_score(10_000, 5, Some(&recent));
        assert!(high > low);
    }

    #[test]
    fn test_compute_score_bounds() {
        let recent = Utc::now().to_rfc3339();
        let score = compute_score(10_000_000, 5_000_000, Some(&recent));
        assert!(score <= 10);
    }

    #[test]
    fn test_score_label_covers_full_range() {
        for s in 0..=10u8 {
            assert_ne!(score_label(s), "inconnu");
        }
    }
}

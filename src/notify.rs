use std::fs;
use std::io::Read;
use ureq::Agent;

#[derive(Clone)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub chat_id: String,
    // Score de sensibilité minimum (0-10, voir sensitivity.rs) requis pour qu'une
    // découverte exploitable déclenche une notification Telegram. Une découverte sur un
    // dépôt en-dessous du seuil est toujours affichée en console (rien n'est perdu), mais
    // n'est pas envoyée sur Telegram, pour ne pas noyer le canal avec des petits dépôts
    // personnels peu suivis.
    pub min_sensitivity: u8,
}

// Seuil par défaut si la 3e ligne du fichier --notify (ou min_sensitivity du TOML) est
// absente : "modéré" (4/10) ou plus, voir sensitivity::score_label.
pub const DEFAULT_MIN_SENSITIVITY: u8 = 4;

// Fichier attendu : deux ou trois lignes.
//   123456:ABC-DEF...           <- token du bot
//   -1001234567890              <- ID du chat
//   5                           <- (optionnel) score de sensibilité minimum, défaut 4/10
pub fn read_telegram_config(path: &str) -> Result<TelegramConfig, Box<dyn std::error::Error>> {
    let mut content = String::new();
    fs::File::open(path)?.read_to_string(&mut content)?;
    let mut lines = content.lines().map(str::trim).filter(|l| !l.is_empty());
    let bot_token = lines
        .next()
        .ok_or("Fichier --notify invalide : première ligne (token du bot) manquante")?
        .to_string();
    let chat_id = lines
        .next()
        .ok_or("Fichier --notify invalide : deuxième ligne (chat ID) manquante")?
        .to_string();
    let min_sensitivity = match lines.next() {
        Some(raw) => raw
            .parse::<u8>()
            .map_err(|_| format!("Fichier --notify invalide : seuil de sensibilité \"{}\" n'est pas un entier 0-10", raw))?
            .min(10),
        None => DEFAULT_MIN_SENSITIVITY,
    };
    Ok(TelegramConfig { bot_token, chat_id, min_sensitivity })
}

// Envoie un message texte via l'API Bot Telegram (https://core.telegram.org/bots/api#sendmessage).
// Échoue silencieusement (log une erreur, ne fait jamais planter le programme) : une
// notification ratée ne doit jamais interrompre le crawl/l'analyse en cours.
pub fn send_telegram_message(agent: &Agent, config: &TelegramConfig, text: &str) {
    let url = format!("https://api.telegram.org/bot{}/sendMessage", config.bot_token);
    let result = agent
        .post(&url)
        .send_form(&[("chat_id", &config.chat_id), ("text", text), ("disable_web_page_preview", "true")]);

    if let Err(e) = result {
        eprintln!("[notify] Échec de l'envoi Telegram : {}", e);
    }
}

// Construit le message envoyé pour une découverte de dependency confusion probable.
pub fn format_alert_message(
    package_name: &str,
    repo_full_name: &str,
    repo_url: &str,
    path: &str,
    reason: &str,
) -> String {
    format!(
        "🚨 Dependency confusion probable\n\nPaquet : {}\nRaison : {}\nDépôt : {}\nFichier : {}\nURL : {}",
        package_name, reason, repo_full_name, path, repo_url
    )
}

// Message console affiché quand une découverte exploitable n'est PAS envoyée sur
// Telegram car le score de sensibilité du dépôt est sous le seuil configuré. La
// découverte reste dans le rapport CSV / les brouillons de divulgation : seule la
// notification en temps réel est filtrée.
pub fn format_low_sensitivity_skip(repo_full_name: &str, score: u8, min_sensitivity: u8) -> String {
    format!(
        "[notify] Alerte retenue en console uniquement : {} a un score de sensibilité {}/10 (seuil : {}/10)",
        repo_full_name, score, min_sensitivity
    )
}

// Notification envoyée au tout début d'une exécution, dès que --notify est actif : permet
// de confirmer que la configuration Telegram fonctionne sans attendre une éventuelle
// première découverte, et de savoir qu'un run a bien démarré (utile pour un --crawl
// destiné à tourner longtemps en arrière-plan).
pub fn send_startup_notification(agent: &Agent, config: &TelegramConfig, mode_description: &str) {
    let text = format!("✅ gh_package_finder démarré — mode : {}", mode_description);
    send_telegram_message(agent, config, &text);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // Écrit un fichier temporaire avec le contenu donné et renvoie son chemin (dans un
    // sous-dossier unique de std::env::temp_dir pour éviter toute collision entre tests
    // exécutés en parallèle).
    fn write_temp_config(name: &str, content: &str) -> String {
        let dir = std::env::temp_dir().join(format!("gh_pkg_finder_test_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join(name);
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path.to_string_lossy().to_string()
    }

    #[test]
    fn test_read_telegram_config_without_threshold_uses_default() {
        let path = write_temp_config(
            "no_threshold.txt",
            "123456:ABCDEF\n-1001234567890\n",
        );
        let cfg = read_telegram_config(&path).unwrap();
        assert_eq!(cfg.bot_token, "123456:ABCDEF");
        assert_eq!(cfg.chat_id, "-1001234567890");
        assert_eq!(cfg.min_sensitivity, DEFAULT_MIN_SENSITIVITY);
    }

    #[test]
    fn test_read_telegram_config_with_explicit_threshold() {
        let path = write_temp_config(
            "with_threshold.txt",
            "123456:ABCDEF\n-1001234567890\n7\n",
        );
        let cfg = read_telegram_config(&path).unwrap();
        assert_eq!(cfg.min_sensitivity, 7);
    }

    #[test]
    fn test_read_telegram_config_threshold_clamped_to_ten() {
        // Un seuil > 10 n'a pas de sens (le score max est 10) : on le ramène à 10 plutôt
        // que de rendre la notification impossible à déclencher par erreur de config
        // silencieuse (ex: 99 au lieu de 9).
        let path = write_temp_config(
            "clamped_threshold.txt",
            "123456:ABCDEF\n-1001234567890\n99\n",
        );
        let cfg = read_telegram_config(&path).unwrap();
        assert_eq!(cfg.min_sensitivity, 10);
    }

    #[test]
    fn test_read_telegram_config_invalid_threshold_errors() {
        let path = write_temp_config(
            "invalid_threshold.txt",
            "123456:ABCDEF\n-1001234567890\npas-un-nombre\n",
        );
        let result = read_telegram_config(&path);
        assert!(result.is_err());
    }

    #[test]
    fn test_read_telegram_config_ignores_blank_lines_before_threshold() {
        let path = write_temp_config(
            "blank_lines.txt",
            "123456:ABCDEF\n\n-1001234567890\n\n6\n",
        );
        let cfg = read_telegram_config(&path).unwrap();
        assert_eq!(cfg.min_sensitivity, 6);
    }

    #[test]
    fn test_format_low_sensitivity_skip_mentions_repo_and_scores() {
        let msg = format_low_sensitivity_skip("acme/tiny-repo", 2, 4);
        assert!(msg.contains("acme/tiny-repo"));
        assert!(msg.contains("2/10"));
        assert!(msg.contains("4/10"));
    }
}

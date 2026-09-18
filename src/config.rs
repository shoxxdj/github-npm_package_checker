use serde::Deserialize;
use std::fs;

// Représente l'intégralité de ce qui est configurable en ligne de commande, mais depuis un
// fichier TOML. L'utilisateur choisit soit ce mécanisme (--config fichier.toml), soit les
// arguments CLI classiques — jamais les deux en même temps, pour éviter toute ambiguïté sur
// la source de vérité en cas de conflit entre un flag et le fichier.
#[derive(Debug, Deserialize)]
pub struct FileConfig {
    /// "search" (défaut), "crawl", "analyse", ou "file"
    #[serde(default = "default_mode")]
    pub mode: String,
    /// Chemin du fichier contenant le token GitHub (requis pour "search"/"crawl")
    pub token_file: Option<String>,
    /// Chemin du package.json local à analyser (requis pour le mode "file")
    pub file_path: Option<String>,
    /// Équivalent de --analyse : lance l'analyse dependency confusion au fil de l'eau
    #[serde(default)]
    pub analyse: bool,
    /// Équivalent de --threads
    pub threads: Option<usize>,
    /// Bloc [notify] optionnel, équivalent de --notify (mais inline plutôt qu'un fichier séparé).
    /// Non pris en charge avec mode = "file" (voir README).
    pub notify: Option<NotifyTomlConfig>,
}

#[derive(Debug, Deserialize)]
pub struct NotifyTomlConfig {
    pub bot_token: String,
    pub chat_id: String,
    /// Score de sensibilité minimum (0-10) requis pour notifier une découverte
    /// exploitable. Défaut : 4/10 ("modéré", voir sensitivity::score_label).
    #[serde(default = "default_min_sensitivity")]
    pub min_sensitivity: u8,
}

fn default_min_sensitivity() -> u8 {
    crate::notify::DEFAULT_MIN_SENSITIVITY
}

fn default_mode() -> String {
    "search".to_string()
}

pub fn load_config(path: &str) -> Result<FileConfig, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(path)
        .map_err(|e| format!("Impossible de lire le fichier de configuration {}: {}", path, e))?;
    let config: FileConfig = toml::from_str(&content)
        .map_err(|e| format!("Fichier TOML invalide ({}): {}", path, e))?;

    match config.mode.as_str() {
        "search" | "crawl" | "analyse" | "analyze" | "file" => {}
        other => return Err(format!("mode inconnu dans {}: \"{}\" (attendu: search, crawl, analyse, ou file)", path, other).into()),
    }
    if (config.mode == "search" || config.mode == "crawl") && config.token_file.is_none() {
        return Err(format!("le mode \"{}\" nécessite \"token_file\" dans {}", config.mode, path).into());
    }
    if config.mode == "file" {
        if config.file_path.is_none() {
            return Err(format!("le mode \"file\" nécessite \"file_path\" dans {}", path).into());
        }
        if config.notify.is_some() {
            return Err(format!("le mode \"file\" ne prend pas en charge [notify] dans {} (pas de dépôt GitHub à interroger)", path).into());
        }
    }

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_search_config() {
        let toml_str = r#"
            mode = "search"
            token_file = "token.txt"
        "#;
        let config: FileConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.mode, "search");
        assert_eq!(config.token_file.as_deref(), Some("token.txt"));
        assert!(!config.analyse);
        assert!(config.threads.is_none());
        assert!(config.notify.is_none());
    }

    #[test]
    fn test_parse_full_crawl_config_with_notify() {
        let toml_str = r#"
            mode = "crawl"
            token_file = "token.txt"
            analyse = true
            threads = 12

            [notify]
            bot_token = "123456:ABC"
            chat_id = "-100123"
        "#;
        let config: FileConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.mode, "crawl");
        assert!(config.analyse);
        assert_eq!(config.threads, Some(12));
        let notify = config.notify.unwrap();
        assert_eq!(notify.bot_token, "123456:ABC");
        assert_eq!(notify.chat_id, "-100123");
        assert_eq!(notify.min_sensitivity, crate::notify::DEFAULT_MIN_SENSITIVITY);
    }

    #[test]
    fn test_parse_notify_config_with_explicit_min_sensitivity() {
        let toml_str = r#"
            mode = "analyse"

            [notify]
            bot_token = "123456:ABC"
            chat_id = "-100123"
            min_sensitivity = 7
        "#;
        let config: FileConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.notify.unwrap().min_sensitivity, 7);
    }

    #[test]
    fn test_parse_analyse_only_config() {
        let toml_str = r#"mode = "analyse""#;
        let config: FileConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.mode, "analyse");
        assert!(config.token_file.is_none());
    }

    #[test]
    fn test_default_mode_is_search() {
        let toml_str = r#"token_file = "token.txt""#;
        let config: FileConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.mode, "search");
    }

    #[test]
    fn test_parse_file_mode_config() {
        let toml_str = r#"
            mode = "file"
            file_path = "./package.json"
        "#;
        let config: FileConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.mode, "file");
        assert_eq!(config.file_path.as_deref(), Some("./package.json"));
    }

    #[test]
    fn test_load_config_file_mode_requires_file_path() {
        let dir = std::env::temp_dir().join(format!("gh_pkg_finder_cfgtest_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("missing_file_path.toml");
        std::fs::write(&path, "mode = \"file\"\n").unwrap();
        let result = load_config(path.to_str().unwrap());
        assert!(result.is_err());
    }

    #[test]
    fn test_load_config_file_mode_rejects_notify() {
        let dir = std::env::temp_dir().join(format!("gh_pkg_finder_cfgtest_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("file_mode_with_notify.toml");
        std::fs::write(
            &path,
            "mode = \"file\"\nfile_path = \"./package.json\"\n\n[notify]\nbot_token = \"1:a\"\nchat_id = \"1\"\n",
        )
        .unwrap();
        let result = load_config(path.to_str().unwrap());
        assert!(result.is_err());
    }
}

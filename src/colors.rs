use std::io::IsTerminal;
use std::sync::OnceLock;

const RED_BOLD: &str = "\x1b[1;31m";
const YELLOW: &str = "\x1b[33m";
const GREEN_BOLD: &str = "\x1b[1;32m";
const RESET: &str = "\x1b[0m";

fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        // Respecte la convention NO_COLOR (https://no-color.org/) et désactive
        // automatiquement si la sortie est redirigée vers un fichier/pipe plutôt qu'un
        // vrai terminal (évite de polluer les logs avec des codes d'échappement ANSI).
        std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
    })
}

fn wrap(code: &str, text: &str) -> String {
    if enabled() {
        format!("{}{}{}", code, text, RESET)
    } else {
        text.to_string()
    }
}

pub fn alert(text: &str) -> String {
    wrap(RED_BOLD, text)
}

pub fn confirmed(text: &str) -> String {
    wrap(RED_BOLD, text)
}

pub fn medium_confidence(text: &str) -> String {
    wrap(YELLOW, text)
}

pub fn safe(text: &str) -> String {
    wrap(GREEN_BOLD, text)
}

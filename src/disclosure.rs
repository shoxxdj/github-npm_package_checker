use crate::db;
use crate::github;
use rusqlite::Connection;
use std::fs;
use std::path::Path;
use ureq::Agent;

const DISCLOSURE_DIR: &str = "disclosures";

pub struct SecurityContact {
    pub advisory_url: String,      // signalement privé GitHub (toujours disponible)
    pub security_policy_url: Option<String>, // SECURITY.md du dépôt, si détecté
    pub checked: bool, // true si l'API a bien répondu (trouvé ou non) ; false si indéterminé
}

// Récupère (avec cache) les informations de contact sécurité d'un dépôt : l'URL de
// signalement privé GitHub (toujours valide, même sans SECURITY.md — GitHub propose une
// interface de "private vulnerability reporting" par défaut) et, si détecté via l'API
// "community profile", le lien vers son éventuel SECURITY.md.
//
// Important : une erreur réseau/rate-limit ne doit JAMAIS être interprétée comme "pas de
// SECURITY.md" — ce serait une affirmation fausse dans un message envoyé à un tiers. Le
// champ `checked` distingue explicitement "vérifié, absent" de "vérification impossible".
pub fn get_security_contact(
    conn: &Connection,
    agent: &Agent,
    token: &str,
    repo_full_name: &str,
) -> SecurityContact {
    let advisory_url = format!("https://github.com/{}/security/advisories/new", repo_full_name);

    if let Ok(Some(row)) = db::known_security_contact(conn, repo_full_name) {
        return SecurityContact {
            advisory_url,
            security_policy_url: if row.has_security_policy { row.security_policy_url } else { None },
            checked: true,
        };
    }

    match github::get_security_policy_url(agent, token, repo_full_name) {
        Some(security_policy_url) => {
            // Réponse API obtenue avec certitude (trouvé ou confirmé absent) : on met en cache.
            let _ = db::save_security_contact(
                conn,
                repo_full_name,
                security_policy_url.is_some(),
                security_policy_url.as_deref(),
            );
            SecurityContact { advisory_url, security_policy_url, checked: true }
        }
        None => {
            // Échec de la vérification (rate limit, réseau...) : on ne met RIEN en cache
            // (retenté à la prochaine exécution) et on ne prétend surtout pas qu'il n'y a
            // pas de politique de sécurité.
            SecurityContact { advisory_url, security_policy_url: None, checked: false }
        }
    }
}

// Génère un message de signalement prêt à envoyer (en anglais : ces dépôts appartiennent
// le plus souvent à des tiers dans le monde entier, l'anglais reste le choix le plus
// pragmatique pour une divulgation responsable). Le message décrit le risque sans jamais
// démontrer d'exploitation réelle (aucune publication n'est effectuée par l'outil).
pub fn generate_disclosure_message(
    package_name: &str,
    dep_type: &str,
    repo_full_name: &str,
    contact: &SecurityContact,
    confidence: &str,
) -> String {
    let scope_note = if package_name.starts_with('@') {
        " (and the scope itself does not appear to be claimed on the public registry, meaning anyone could register it today)"
    } else {
        ""
    };

    let contact_section = match (&contact.security_policy_url, contact.checked) {
        (Some(url), _) => format!(
            "This repository has a published security policy — please follow its instructions:\n{}",
            url
        ),
        (None, true) => format!(
            "No SECURITY.md was found for this repository. You can open a private vulnerability \
             report directly on GitHub (visible only to maintainers) here:\n{}",
            contact.advisory_url
        ),
        (None, false) => format!(
            "I could not automatically determine whether this repository publishes a security \
             policy. Please check for a SECURITY.md file, or open a private vulnerability report \
             directly on GitHub (visible only to maintainers) here:\n{}",
            contact.advisory_url
        ),
    };

    format!(
        "Subject: Potential dependency confusion risk in {repo}\n\
         \n\
         Hello,\n\
         \n\
         While researching public supply-chain security exposure, I found that {repo} \
         references a dependency named \"{package}\" (in {dep_type}) that does not currently \
         exist on the public npm registry{scope_note}.\n\
         \n\
         This can expose the project to a \"dependency confusion\" attack: anyone could publish \
         a package under this exact name on npmjs.com, and depending on your build/CI \
         configuration, it could be installed instead of the intended internal package.\n\
         \n\
         Suggested remediation:\n\
         - If this package is meant to stay private, consider claiming/reserving the name (or \
         the whole scope) on the public npm registry, even as an empty placeholder, to prevent \
         squatting.\n\
         - Alternatively, pin this dependency to a private registry via .npmrc scope \
         configuration, and make sure this is enforced in CI (not only on developer machines).\n\
         - Double-check that your lockfile (package-lock.json / yarn.lock) resolves this \
         package from the source you actually intend.\n\
         \n\
         Confidence level of this finding: {confidence}\n\
         \n\
         {contact_section}\n\
         \n\
         This message is sent as part of a responsible disclosure effort. No package was \
         published and no exploitation was attempted — this is purely based on what is already \
         publicly visible in the repository.\n\
         \n\
         Best regards,\n",
        repo = repo_full_name,
        package = package_name,
        dep_type = dep_type,
        scope_note = scope_note,
        confidence = confidence,
        contact_section = contact_section,
    )
}

fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

// Écrit un fichier de brouillon par (dépôt, paquet) sous disclosures/. `sensitivity` est
// une note de triage interne (score 0-10 + libellé), préfixée au fichier mais clairement
// signalée comme à retirer avant l'envoi réel du message.
pub fn write_disclosure_draft(
    repo_full_name: &str,
    package_name: &str,
    message: &str,
    sensitivity: Option<(u8, &str)>,
) -> std::io::Result<()> {
    fs::create_dir_all(DISCLOSURE_DIR)?;
    let filename = format!(
        "{}__{}.txt",
        sanitize_filename(repo_full_name),
        sanitize_filename(package_name)
    );

    let full_content = match sensitivity {
        Some((score, label)) => format!(
            "[NOTE INTERNE — à retirer avant envoi] Sensibilité du dépôt : {}/10 ({})\n\n{}",
            score, label, message
        ),
        None => message.to_string(),
    };

    fs::write(Path::new(DISCLOSURE_DIR).join(filename), full_content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_disclosure_message_with_policy() {
        let contact = SecurityContact {
            advisory_url: "https://github.com/acme/webapp/security/advisories/new".to_string(),
            security_policy_url: Some("https://github.com/acme/webapp/security/policy".to_string()),
            checked: true,
        };
        let msg = generate_disclosure_message(
            "@acme/ui-kit",
            "dependencies",
            "acme/webapp",
            &contact,
            "moyenne (absent de npm uniquement)",
        );
        assert!(msg.contains("@acme/ui-kit"));
        assert!(msg.contains("acme/webapp"));
        assert!(msg.contains("security/policy"));
        assert!(msg.contains("scope itself does not appear to be claimed"));
    }

    #[test]
    fn test_generate_disclosure_message_without_policy() {
        let contact = SecurityContact {
            advisory_url: "https://github.com/acme/webapp/security/advisories/new".to_string(),
            security_policy_url: None,
            checked: true,
        };
        let msg = generate_disclosure_message(
            "unscoped-pkg",
            "devDependencies",
            "acme/webapp",
            &contact,
            "moyenne (absent de npm uniquement)",
        );
        assert!(msg.contains("No SECURITY.md was found"));
        assert!(msg.contains("security/advisories/new"));
        assert!(!msg.contains("scope itself"));
    }

    #[test]
    fn test_generate_disclosure_message_unchecked() {
        let contact = SecurityContact {
            advisory_url: "https://github.com/acme/webapp/security/advisories/new".to_string(),
            security_policy_url: None,
            checked: false,
        };
        let msg = generate_disclosure_message(
            "unscoped-pkg",
            "dependencies",
            "acme/webapp",
            &contact,
            "moyenne (absent de npm uniquement)",
        );
        // Ne doit JAMAIS affirmer l'absence d'une politique de sécurité si on n'a pas pu vérifier.
        assert!(!msg.contains("No SECURITY.md was found"));
        assert!(msg.contains("could not automatically determine"));
    }

    #[test]
    fn test_sanitize_filename() {
        assert_eq!(sanitize_filename("acme/webapp"), "acme_webapp");
        assert_eq!(sanitize_filename("@acme/ui-kit"), "_acme_ui-kit");
    }
}

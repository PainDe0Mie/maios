//! Fichiers `.desktop` (Desktop Entry Specification 1.5) : groupe `[Desktop Entry]`, valeurs
//! localisées (`Name[fr_FR]`, `Name[fr]`, puis `Name`), échappements, et découpage de `Exec`
//! (guillemets, échappements, codes de champ `%f %u %i %c %k`…).

use alloc::string::{String, ToString};
use alloc::vec::Vec;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DesktopEntry {
    pub name: String,
    pub generic_name: String,
    pub comment: String,
    pub exec: String,
    pub try_exec: String,
    pub icon: String,
    pub terminal: bool,
    /// `NoDisplay` ou `Hidden` : ne pas montrer dans les menus.
    pub hidden: bool,
    pub categories: Vec<String>,
    pub startup_wm_class: String,
    pub kind: String,
}

/// Valeur d'une chaîne avec ses échappements (`\s \n \t \r \\`).
fn unescape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut it = v.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn boolean(v: &str) -> bool {
    v.trim() == "true"
}

/// Rang d'une variante localisée pour la langue voulue (`lang` = « fr_FR » par exemple) :
/// 3 = `lang_COUNTRY` exact, 2 = langue seule, 1 = sans localisation, 0 = autre langue.
fn locale_rank(key_locale: Option<&str>, lang: &str) -> u8 {
    match key_locale {
        None => 1,
        Some(l) => {
            let l = l.split('.').next().unwrap_or(l);
            let l = l.split('@').next().unwrap_or(l);
            if l == lang {
                3
            } else if Some(l) == lang.split('_').next() {
                2
            } else {
                0
            }
        }
    }
}

/// Analyse un `.desktop` ; `lang` choisit les traductions (« fr_FR »).
pub fn parse(text: &str, lang: &str) -> Option<DesktopEntry> {
    let mut e = DesktopEntry::default();
    let mut ranks = [0u8; 3]; // name, generic_name, comment
    let mut in_main = false;
    let mut seen_main = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_main = line == "[Desktop Entry]";
            seen_main |= in_main;
            continue;
        }
        if !in_main {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        let (base, loc) = match k.find('[') {
            Some(i) if k.ends_with(']') => (&k[..i], Some(&k[i + 1..k.len() - 1])),
            _ => (k, None),
        };
        let r = locale_rank(loc, lang);
        let slot = match base {
            "Name" => Some(0),
            "GenericName" => Some(1),
            "Comment" => Some(2),
            _ => None,
        };
        if let Some(s) = slot {
            if r > ranks[s] {
                ranks[s] = r;
                let val = unescape(v);
                match s {
                    0 => e.name = val,
                    1 => e.generic_name = val,
                    _ => e.comment = val,
                }
            }
            continue;
        }
        if loc.is_some() {
            continue;
        }
        match base {
            "Type" => e.kind = v.to_string(),
            "Exec" => e.exec = unescape(v),
            "TryExec" => e.try_exec = unescape(v),
            "Icon" => e.icon = unescape(v),
            "Terminal" => e.terminal = boolean(v),
            "NoDisplay" | "Hidden" => e.hidden |= boolean(v),
            "StartupWMClass" => e.startup_wm_class = unescape(v),
            "Categories" => e.categories = v.split(';').filter(|s| !s.is_empty()).map(String::from).collect(),
            _ => {}
        }
    }
    if !seen_main || e.name.is_empty() {
        return None;
    }
    Some(e)
}

/// Découpe `Exec` en arguments : guillemets doubles (échappements `\" \` \$ \\`), codes de
/// champ retirés (`%f %F %u %U %d %D %n %N %v %m %i`), `%c` → nom, `%k` → chemin du fichier,
/// `%%` → `%`. `None` si les guillemets ne se ferment pas.
pub fn exec_args(exec: &str, name: &str, path: &str) -> Option<Vec<String>> {
    let mut args: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut has = false;
    let mut it = exec.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '"' => {
                has = true;
                loop {
                    match it.next()? {
                        '"' => break,
                        '\\' => match it.next()? {
                            q @ ('"' | '`' | '$' | '\\') => cur.push(q),
                            o => {
                                cur.push('\\');
                                cur.push(o);
                            }
                        },
                        o => cur.push(o),
                    }
                }
            }
            ' ' | '\t' => {
                if has {
                    args.push(core::mem::take(&mut cur));
                    has = false;
                }
            }
            '%' => match it.next() {
                Some('%') => {
                    cur.push('%');
                    has = true;
                }
                Some('c') => {
                    cur.push_str(name);
                    has = true;
                }
                Some('k') => {
                    cur.push_str(path);
                    has = true;
                }
                // Codes de fichiers ou d'URL (aucun à passer) et codes obsolètes.
                Some(_) | None => {}
            },
            o => {
                cur.push(o);
                has = true;
            }
        }
    }
    if has {
        args.push(cur);
    }
    Some(args)
}

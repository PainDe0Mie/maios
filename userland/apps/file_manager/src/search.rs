//! Recherche de fichiers : correspondance approchée notée et parcours récursif incrémental.
//!
//! **Notation** : les lettres de la requête doivent apparaître dans l'ordre dans le nom
//! (sous-séquence) ; le score suit l'algorithme v1 de fzf (J. Choi) :
//! - bonus pour une correspondance en début de mot (après `/ _ - . espace`) ou sur une
//!   majuscule d'un nom en *camelCase*, et pour les lettres consécutives ;
//! - pénalité pour chaque trou, plus forte à son ouverture qu'à son prolongement ;
//! - après le premier passage avant, un passage arrière resserre la fenêtre de
//!   correspondance (la plus courte qui finit au même endroit), comme fzf v1.
//!
//! La comparaison ignore la casse et les accents. Le parcours des dossiers se fait par
//! étapes (pile explicite) avec un budget de temps par image : l'interface reste fluide même
//! sur une grande arborescence, et les résultats arrivent au fil de l'eau.

use alloc::string::String;
use alloc::vec::Vec;

use fs_node::{DirRef, FileOrDir};

use crate::fsops::{fold, join};

const SCORE_MATCH: i32 = 16;
const GAP_START: i32 = -3;
const GAP_EXT: i32 = -1;
const BONUS_BOUNDARY: i32 = 8;
const BONUS_CAMEL: i32 = 7;
const BONUS_CONSECUTIVE: i32 = 4;
const BONUS_FIRST_CHAR: i32 = 2;
/// Bonus d'une correspondance exacte du début du nom.
const BONUS_PREFIX: i32 = 24;

/// Correspondance trouvée : score et positions (en caractères) des lettres retrouvées.
#[derive(Clone, Debug)]
pub struct Match {
    pub score: i32,
    pub positions: Vec<usize>,
}

fn is_boundary(c: char) -> bool { matches!(c, '/' | '_' | '-' | '.' | ' ' | '(' | '[') }

/// Note `name` pour la requête `query` (déjà repliée en minuscules sans accents).
pub fn fuzzy(query: &[char], name: &str) -> Option<Match> {
    if query.is_empty() {
        return Some(Match { score: 0, positions: Vec::new() });
    }
    let orig: Vec<char> = name.chars().collect();
    let low: Vec<char> = orig.iter().map(|&c| fold(c)).collect();
    // Passage avant : première sous-séquence.
    let mut qi = 0;
    let mut end = None;
    for (i, &c) in low.iter().enumerate() {
        if c == query[qi] {
            qi += 1;
            if qi == query.len() {
                end = Some(i);
                break;
            }
        }
    }
    let end = end?;
    // Passage arrière : début le plus tardif qui permet de finir en `end`.
    let mut qj = query.len();
    let mut start = end;
    let mut i = end as isize;
    while i >= 0 && qj > 0 {
        if low[i as usize] == query[qj - 1] {
            qj -= 1;
            start = i as usize;
        }
        i -= 1;
    }
    // Notation dans la fenêtre [start, end] : on choisit gloutonnement chaque lettre en
    // préférant une frontière de mot si elle arrive avant la prochaine occurrence ordinaire.
    let mut positions = Vec::with_capacity(query.len());
    let mut score = 0;
    let mut k = 0;
    let mut prev: Option<usize> = None;
    let mut idx = start;
    while k < query.len() && idx <= end {
        if low[idx] == query[k] {
            let mut bonus = 0;
            let boundary = idx == 0 || is_boundary(orig[idx - 1]);
            let camel = idx > 0 && orig[idx].is_uppercase() && orig[idx - 1].is_lowercase();
            if boundary {
                bonus += BONUS_BOUNDARY;
            } else if camel {
                bonus += BONUS_CAMEL;
            }
            if let Some(p) = prev {
                if p + 1 == idx {
                    bonus += BONUS_CONSECUTIVE;
                } else {
                    let gap = (idx - p - 1) as i32;
                    score += GAP_START + GAP_EXT * (gap - 1);
                }
            } else if idx == 0 {
                bonus += BONUS_FIRST_CHAR;
            }
            score += SCORE_MATCH + bonus;
            positions.push(idx);
            prev = Some(idx);
            k += 1;
        }
        idx += 1;
    }
    if k < query.len() {
        return None;
    }
    if positions.first() == Some(&0) && positions.windows(2).all(|w| w[1] == w[0] + 1) {
        score += BONUS_PREFIX;
    }
    // Les noms courts l'emportent à score égal.
    score -= (orig.len() as i32 / 8).min(8);
    Some(Match { score, positions })
}

/// Résultat de recherche.
#[derive(Clone, Debug)]
pub struct Hit {
    /// Chemin du dossier contenant.
    pub dir: String,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub m: Match,
}

impl Hit {
    pub fn path(&self) -> String { join(&self.dir, &self.name) }
}

/// Dossier en cours de parcours : noms restant à examiner.
struct Frame {
    dir: DirRef,
    path: String,
    depth: u32,
    names: Option<Vec<String>>,
    next: usize,
}

/// Dossiers synthétiques, régénérés à chaque accès : pas de recherche dedans.
const SKIP: &[&str] = &["/tasks"];

/// Recherche récursive incrémentale.
///
/// Le budget de temps est vérifié **à chaque élément** (et non à chaque dossier) : un dossier
/// lent (disque FAT32 en PIO, milliers d'entrées) ne fige pas la fenêtre.
pub struct Search {
    pub query: String,
    q: Vec<char>,
    stack: Vec<Frame>,
    pub hits: Vec<Hit>,
    pub scanned: usize,
    pub done: bool,
    /// Plafonds (protègent la mémoire sur une arborescence géante).
    max_hits: usize,
    max_depth: u32,
    max_scanned: usize,
}

impl Search {
    pub fn new(root: DirRef, root_path: &str, query: &str) -> Search {
        Search {
            query: String::from(query),
            q: query.chars().map(fold).filter(|c| !c.is_whitespace()).collect(),
            stack: alloc::vec![Frame { dir: root, path: String::from(root_path), depth: 0, names: None, next: 0 }],
            hits: Vec::new(),
            scanned: 0,
            done: false,
            max_hits: 5000,
            max_depth: 32,
            max_scanned: 500_000,
        }
    }

    /// Avance le parcours pendant au plus `budget_ns` ; renvoie `true` si l'affichage doit
    /// être rafraîchi (résultats ou progression).
    pub fn run(&mut self, budget_ns: u64) -> bool {
        let start = task::scheduler::clock_ns();
        let before = (self.hits.len(), self.scanned);
        while let Some(frame) = self.stack.last_mut() {
            if frame.names.is_none() {
                let names = frame.dir.lock().list();
                frame.names = Some(names);
            }
            let names = frame.names.as_ref().map_or(0, |n| n.len());
            if frame.next >= names {
                self.stack.pop();
                continue;
            }
            let name = frame.names.as_ref().map(|n| n[frame.next].clone()).unwrap_or_default();
            frame.next += 1;
            // Le verrou du dossier n'est pas tenu pendant l'examen de l'enfant.
            let node = frame.dir.lock().get(&name);
            let (path, depth) = (frame.path.clone(), frame.depth);
            self.scanned += 1;
            if let Some(node) = node {
                let is_dir = matches!(node, FileOrDir::Dir(_));
                if let Some(m) = fuzzy(&self.q, &name) {
                    if self.hits.len() < self.max_hits {
                        let size = match &node {
                            FileOrDir::File(f) => f.lock().len() as u64,
                            FileOrDir::Dir(_) => 0,
                        };
                        self.hits.push(Hit { dir: path.clone(), name: name.clone(), is_dir, size, m });
                    }
                }
                if let FileOrDir::Dir(d) = node {
                    let sub = join(&path, &name);
                    if depth < self.max_depth && !SKIP.contains(&sub.as_str()) {
                        let f = Frame { dir: d, path: sub, depth: depth + 1, names: None, next: 0 };
                        // Les crates des namespaces sont nombreuses et rarement cherchées :
                        // parcourues en dernier (bas de la pile).
                        if f.path.starts_with("/namespaces") {
                            self.stack.insert(0, f);
                        } else {
                            self.stack.push(f);
                        }
                    }
                }
            }
            if self.scanned >= self.max_scanned {
                self.stack.clear();
                break;
            }
            if task::scheduler::clock_ns().saturating_sub(start) >= budget_ns {
                break;
            }
        }
        if self.stack.is_empty() {
            self.done = true;
        }
        if self.hits.len() != before.0 {
            // Meilleur score d'abord ; à égalité, chemin le plus court.
            self.hits.sort_by(|a, b| b.m.score.cmp(&a.m.score).then(a.dir.len().cmp(&b.dir.len())).then(a.name.cmp(&b.name)));
        }
        (self.hits.len(), self.scanned) != before || self.done
    }
}

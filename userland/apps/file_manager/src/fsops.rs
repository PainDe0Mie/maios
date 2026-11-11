//! Opérations sur le système de fichiers virtuel : lecture des dossiers, chemins, types de
//! fichiers (octets magiques), copie et déplacement, corbeille, annulation.
//!
//! - Le **type** d'un fichier est reconnu par ses premiers octets (comme `file(1)` et
//!   `libmagic`), pas seulement par son extension : ELF (statique, dynamique, PIE, objet),
//!   PE, scripts `#!`, WebAssembly, archives, images, sons, WAD de Doom…
//! - Le **déplacement** et le **renommage** ne copient rien : le nœud change de parent
//!   (`fs_node::move_node`), comme `rename(2)` dans un même système de fichiers.
//! - La **copie** est un travail découpé en étapes (un fichier ou un morceau de 1 Mio par
//!   étape) que la boucle de l'application exécute avec un budget de temps par image : la
//!   fenêtre reste fluide et la copie peut être annulée.
//! - La **corbeille** (`/corbeille`) garde le chemin d'origine de chaque élément dans un index
//!   (`.index`, une ligne `nom<TAB>chemin`), comme la spécification *Trash* de freedesktop.org :
//!   les éléments se restaurent, même depuis une autre instance de l'application.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cmp::Ordering;

use fs_node::{DirRef, FileOrDir, FileRef, FsNode};

/// Erreur de [`trash`] : le volume ne permet pas la corbeille (comme les volumes amovibles
/// sous Windows) ; l'interface propose alors la suppression définitive.
pub const NO_TRASH: &str = "Ce volume ne gère pas la corbeille";

/// Dossier de la corbeille.
pub const TRASH: &str = "/corbeille";
const TRASH_INDEX: &str = ".index";
/// Taille des morceaux de copie.
const CHUNK: usize = 1 << 20;

// ================================================================
// TYPES DE FICHIERS (mai_ui::files)
// ================================================================

pub use mai_ui::files::{classify, FileKind};

// ================================================================
// ENTRÉES D'UN DOSSIER
// ================================================================

/// Élément d'un dossier.
#[derive(Clone, Debug)]
pub struct Item {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Nombre d'enfants (dossiers).
    pub children: usize,
    pub kind: FileKind,
}

impl Item {
    pub fn hidden(&self) -> bool { self.name.starts_with('.') }
}

/// Lit les premiers octets d'un fichier.
pub fn read_head(f: &FileRef, n: usize) -> Vec<u8> {
    let mut l = f.lock();
    let len = l.len().min(n);
    let mut buf = alloc::vec![0u8; len];
    if len > 0 {
        let got = l.read_at(&mut buf, 0).unwrap_or(0);
        buf.truncate(got);
    }
    buf
}

/// Lit tout un fichier (borné à `max` octets).
pub fn read_all(f: &FileRef, max: usize) -> Vec<u8> { read_head(f, max) }

/// Enfants d'un dossier, sans tenir son verrou pendant la lecture de chaque enfant (certains
/// systèmes de fichiers, comme `task_fs`, reprennent des verrous du VFS dans `get`).
pub fn children(dir: &DirRef) -> Vec<FileOrDir> {
    let names = dir.lock().list();
    names.iter().filter_map(|n| dir.lock().get(n)).collect()
}

/// Contenu d'un dossier.
pub fn list(dir: &DirRef, path: &str) -> Vec<Item> {
    let names = dir.lock().list();
    let nodes: Vec<(String, Option<FileOrDir>)> = names.into_iter().map(|n| {
        let node = dir.lock().get(&n);
        (n, node)
    }).collect();
    let mut out = Vec::with_capacity(nodes.len());
    for (name, node) in nodes {
        match node {
            Some(FileOrDir::Dir(d)) => {
                let children = d.lock().list().len();
                let volume = path == "/" && (name == "disk" || name.starts_with("disk"));
                out.push(Item { name, is_dir: true, size: 0, children, kind: if volume { FileKind::Volume } else { FileKind::Folder } });
            }
            Some(FileOrDir::File(f)) => {
                let size = f.lock().len() as u64;
                let head = read_head(&f, 64);
                let full = join(path, &name);
                let kind = classify(&name, &full, &head, size as usize);
                out.push(Item { name, is_dir: false, size, children: 0, kind });
            }
            None => {}
        }
    }
    out
}

/// Empreinte rapide d'un dossier (noms, et tailles des fichiers tant que le dossier est
/// petit), pour détecter un changement. Un seul `list()` au-delà de 256 entrées : sur un disque
/// FAT32 en PIO, chaque `get()` relit le dossier.
pub fn fingerprint(dir: &DirRef) -> u64 {
    let names = dir.lock().list();
    let small = names.len() <= 256;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a
    for n in &names {
        for b in n.bytes() {
            h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
        }
        if small {
            let node = dir.lock().get(n);
            if let Some(FileOrDir::File(f)) = node {
                h = (h ^ f.lock().len() as u64).wrapping_mul(0x100_0000_01b3);
            }
        }
    }
    h
}

// ================================================================
// TRI NATUREL
// ================================================================

/// Comparaison « naturelle » : les nombres se comparent par valeur (`image2` < `image10`),
/// sans tenir compte de la casse, comme `strverscmp` de la glibc et le tri de l'Explorateur
/// Windows (`StrCmpLogicalW`).
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut na: u128 = 0;
                let mut la = 0;
                while let Some(c) = ai.peek().copied().filter(|c| c.is_ascii_digit()) {
                    na = na.saturating_mul(10).saturating_add(c as u128 - '0' as u128);
                    la += 1;
                    ai.next();
                }
                let mut nb: u128 = 0;
                let mut lb = 0;
                while let Some(c) = bi.peek().copied().filter(|c| c.is_ascii_digit()) {
                    nb = nb.saturating_mul(10).saturating_add(c as u128 - '0' as u128);
                    lb += 1;
                    bi.next();
                }
                match na.cmp(&nb).then(la.cmp(&lb)) {
                    Ordering::Equal => {}
                    o => return o,
                }
            }
            (Some(x), Some(y)) => {
                let (lx, ly) = (fold(x), fold(y));
                if lx != ly {
                    return lx.cmp(&ly);
                }
                ai.next();
                bi.next();
            }
        }
    }
}

/// Minuscule sans accent (tri et recherche insensibles aux accents).
pub fn fold(c: char) -> char {
    match c {
        'à' | 'â' | 'ä' | 'á' | 'À' | 'Â' | 'Ä' => 'a',
        'é' | 'è' | 'ê' | 'ë' | 'É' | 'È' | 'Ê' | 'Ë' => 'e',
        'î' | 'ï' | 'Î' | 'Ï' => 'i',
        'ô' | 'ö' | 'Ô' | 'Ö' => 'o',
        'ù' | 'û' | 'ü' | 'Ù' | 'Û' | 'Ü' => 'u',
        'ç' | 'Ç' => 'c',
        c => c.to_ascii_lowercase(),
    }
}

// ================================================================
// CHEMINS
// ================================================================

pub fn join(dir: &str, name: &str) -> String {
    if dir == "/" { format!("/{}", name) } else { format!("{}/{}", dir, name) }
}

/// Parent d'un chemin absolu.
pub fn parent_of(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => path[..i].to_string(),
    }
}

pub fn basename(path: &str) -> &str { path.rsplit('/').next().unwrap_or(path) }

/// Normalise un chemin saisi (`.`, `..`, `//`) relativement à `cwd`.
pub fn normalize(cwd: &str, input: &str) -> String {
    let mut parts: Vec<&str> = if input.starts_with('/') { Vec::new() } else { cwd.split('/').filter(|s| !s.is_empty()).collect() };
    for seg in input.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    let mut out = String::from("/");
    out.push_str(&parts.join("/"));
    out
}

/// Nœud d'un chemin absolu.
pub fn lookup(path: &str) -> Option<FileOrDir> {
    let mut cur: DirRef = root::get_root().clone();
    let mut segs = path.split('/').filter(|s| !s.is_empty()).peekable();
    if segs.peek().is_none() {
        return Some(FileOrDir::Dir(cur));
    }
    while let Some(seg) = segs.next() {
        let next = cur.lock().get(seg)?;
        if segs.peek().is_none() {
            return Some(next);
        }
        match next {
            FileOrDir::Dir(d) => cur = d,
            FileOrDir::File(_) => return None,
        }
    }
    None
}

pub fn lookup_dir(path: &str) -> Option<DirRef> {
    match lookup(path)? {
        FileOrDir::Dir(d) => Some(d),
        FileOrDir::File(_) => None,
    }
}

/// Nom libre dans `dir` : `nom`, puis `nom (copie)`, `nom (copie 2)`… (extension conservée).
pub fn unique_name(dir: &DirRef, wanted: &str, suffix: &str) -> String {
    let d = dir.lock();
    if d.get(wanted).is_none() {
        return wanted.to_string();
    }
    let (stem, ext) = match wanted.rfind('.') {
        Some(i) if i > 0 => (&wanted[..i], &wanted[i..]),
        _ => (wanted, ""),
    };
    for n in 1..10_000 {
        let cand = if n == 1 { format!("{} ({}){}", stem, suffix, ext) } else { format!("{} ({} {}){}", stem, suffix, n, ext) };
        if d.get(&cand).is_none() {
            return cand;
        }
    }
    format!("{}-{}", wanted, task::scheduler::clock_ns())
}

/// Nom de fichier valide.
pub fn valid_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("Le nom ne peut pas être vide");
    }
    if name == "." || name == ".." {
        return Err("Nom réservé");
    }
    if name.contains('/') {
        return Err("Le nom ne peut pas contenir « / »");
    }
    if name.chars().any(|c| c.is_control()) {
        return Err("Le nom contient des caractères de contrôle");
    }
    if name.len() > 255 {
        return Err("Nom trop long (255 octets au plus)");
    }
    Ok(())
}

/// Taille et nombre d'éléments d'une arborescence, bornés en nombre de nœuds et en temps
/// (`budget_ns`, 0 : sans limite) ; le booléen indique un résultat partiel.
pub fn tree_size(dir: &DirRef, max_nodes: usize) -> (u64, usize, usize, bool) { tree_size_budget(dir, max_nodes, 0) }

pub fn tree_size_budget(dir: &DirRef, max_nodes: usize, budget_ns: u64) -> (u64, usize, usize, bool) {
    let start = task::scheduler::clock_ns();
    let mut stack = alloc::vec![dir.clone()];
    let (mut bytes, mut files, mut dirs) = (0u64, 0usize, 0usize);
    while let Some(d) = stack.pop() {
        if budget_ns > 0 && task::scheduler::clock_ns().saturating_sub(start) > budget_ns {
            return (bytes, files, dirs, true);
        }
        let nodes = children(&d);
        for n in nodes {
            match n {
                FileOrDir::File(f) => {
                    files += 1;
                    bytes += f.lock().len() as u64;
                }
                FileOrDir::Dir(sub) => {
                    dirs += 1;
                    stack.push(sub);
                }
            }
            if files + dirs >= max_nodes {
                return (bytes, files, dirs, true);
            }
        }
    }
    (bytes, files, dirs, false)
}

// ================================================================
// CRÉATION, RENOMMAGE, DÉPLACEMENT
// ================================================================

pub fn create_dir(parent: &DirRef, name: &str) -> Result<DirRef, String> {
    valid_name(name).map_err(String::from)?;
    if parent.lock().get(name).is_some() {
        return Err(format!("« {} » existe déjà", name));
    }
    vfs_node::VFSDirectory::create(name.to_string(), parent).map_err(String::from)
}

pub fn create_file(parent: &DirRef, name: &str, content: &[u8]) -> Result<FileRef, String> {
    valid_name(name).map_err(String::from)?;
    if parent.lock().get(name).is_some() {
        return Err(format!("« {} » existe déjà", name));
    }
    let f = memfs::MemFile::create(name.to_string(), parent).map_err(String::from)?;
    if !content.is_empty() {
        f.lock().write_at(content, 0).map_err(|_| String::from("écriture impossible"))?;
    }
    Ok(f)
}

/// Renomme `dir/old` en `dir/new`.
pub fn rename(dir: &DirRef, old: &str, new: &str) -> Result<(), String> {
    valid_name(new).map_err(String::from)?;
    if old == new {
        return Ok(());
    }
    if dir.lock().get(new).is_some() {
        return Err(format!("« {} » existe déjà", new));
    }
    let node = dir.lock().get(old).ok_or_else(|| format!("« {} » introuvable", old))?;
    fs_node::move_node(&node, dir, dir, new).map(|_| ()).map_err(String::from)
}

/// Déplace le nœud `src_path` dans `dst_dir` (sous un nom libre) ; renvoie le nouveau chemin.
pub fn move_to(src_path: &str, dst_dir_path: &str) -> Result<String, String> {
    let node = lookup(src_path).ok_or_else(|| format!("{} introuvable", src_path))?;
    let from = lookup_dir(&parent_of(src_path)).ok_or("dossier source introuvable")?;
    let to = lookup_dir(dst_dir_path).ok_or("dossier de destination introuvable")?;
    if fs_node::same_dir(&from, &to) {
        return Ok(src_path.to_string());
    }
    let name = unique_name(&to, basename(src_path), "déplacé");
    match fs_node::move_node(&node, &from, &to, &name) {
        Ok(_) => Ok(join(dst_dir_path, &name)),
        Err(e) if e == fs_node::NOT_MOVABLE => Err(NEEDS_COPY.to_string()),
        Err(e) => Err(String::from(e)),
    }
}

/// Erreur de [`move_to`] : déplacer exige de copier puis supprimer.
pub const NEEDS_COPY: &str = "copie nécessaire";

/// Supprime définitivement un nœud (le contenu est libéré quand plus personne ne le tient).
pub fn remove(path: &str) -> Result<(), String> {
    let node = lookup(path).ok_or_else(|| format!("{} introuvable", path))?;
    let parent = lookup_dir(&parent_of(path)).ok_or("dossier parent introuvable")?;
    if path == "/" || parent_of(path) == "/" && matches!(basename(path), "namespaces" | "extra_files") {
        return Err("Élément système protégé".to_string());
    }
    // Certains systèmes de fichiers (FAT32) renvoient toujours `None` : on vérifie plutôt que
    // le nom a disparu.
    let removed = parent.lock().remove(&node);
    if removed.is_some() || lookup(path).is_none() {
        Ok(())
    } else {
        Err("suppression impossible".to_string())
    }
}

// ================================================================
// CORBEILLE
// ================================================================

pub fn trash_dir() -> Result<DirRef, String> {
    if let Some(d) = lookup_dir(TRASH) {
        return Ok(d);
    }
    vfs_node::VFSDirectory::create(basename(TRASH).to_string(), root::get_root()).map_err(String::from)
}

/// Index de la corbeille : nom dans la corbeille → chemin d'origine.
pub fn trash_index() -> Vec<(String, String)> {
    let Some(FileOrDir::File(f)) = lookup(&join(TRASH, TRASH_INDEX)) else { return Vec::new() };
    let bytes = read_all(&f, 1 << 20);
    let text = String::from_utf8_lossy(&bytes);
    text.lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

pub fn write_trash_index(entries: &[(String, String)]) -> Result<(), String> {
    let dir = trash_dir()?;
    let mut text = String::new();
    for (a, b) in entries {
        text.push_str(a);
        text.push('\t');
        text.push_str(b);
        text.push('\n');
    }
    let f = match dir.lock().get(TRASH_INDEX) {
        Some(FileOrDir::File(f)) => Some(f),
        _ => None,
    };
    let f = match f {
        Some(f) => f,
        None => memfs::MemFile::create(TRASH_INDEX.to_string(), &dir).map_err(String::from)?,
    };
    let mut l = f.lock();
    l.set_len(0).map_err(String::from)?;
    if !text.is_empty() {
        l.write_at(text.as_bytes(), 0).map_err(|_| String::from("écriture de l'index impossible"))?;
    }
    Ok(())
}

/// Met `path` à la corbeille ; renvoie son nom dans la corbeille.
pub fn trash(path: &str) -> Result<String, String> {
    if path.starts_with(TRASH) {
        return Err("Déjà dans la corbeille".to_string());
    }
    let node = lookup(path).ok_or_else(|| format!("{} introuvable", path))?;
    let from = lookup_dir(&parent_of(path)).ok_or("dossier parent introuvable")?;
    let tdir = trash_dir()?;
    if !node.is_movable() || !from.lock().is_movable() {
        return Err(NO_TRASH.to_string());
    }
    let name = unique_name(&tdir, basename(path), "supprimé");
    fs_node::move_node(&node, &from, &tdir, &name).map_err(String::from)?;
    let mut idx = trash_index();
    idx.retain(|(n, _)| n != &name);
    idx.push((name.clone(), path.to_string()));
    write_trash_index(&idx)?;
    Ok(name)
}

/// Restaure l'élément `name` de la corbeille à son emplacement d'origine (sous un nom libre
/// si l'emplacement est pris) ; renvoie le chemin restauré.
pub fn restore(name: &str) -> Result<String, String> {
    let mut idx = trash_index();
    let original = idx.iter().find(|(n, _)| n == name).map(|(_, p)| p.clone()).unwrap_or_else(|| join("/", name));
    let tdir = trash_dir()?;
    let node = tdir.lock().get(name).ok_or_else(|| format!("« {} » n'est plus dans la corbeille", name))?;
    let parent_path = parent_of(&original);
    let parent = match lookup_dir(&parent_path) {
        Some(d) => d,
        None => mkdir_p(&parent_path)?,
    };
    let target = unique_name(&parent, basename(&original), "restauré");
    fs_node::move_node(&node, &tdir, &parent, &target).map_err(String::from)?;
    idx.retain(|(n, _)| n != name);
    write_trash_index(&idx)?;
    Ok(join(&parent_path, &target))
}

/// Vide la corbeille ; renvoie le nombre d'éléments supprimés.
pub fn empty_trash() -> Result<usize, String> {
    let tdir = trash_dir()?;
    let nodes: Vec<FileOrDir> = {
        let l = tdir.lock();
        l.list().iter().filter(|n| n.as_str() != TRASH_INDEX).filter_map(|n| l.get(n)).collect()
    };
    let n = nodes.len();
    for node in nodes {
        tdir.lock().remove(&node);
    }
    write_trash_index(&[])?;
    Ok(n)
}

/// Crée un dossier et ses parents manquants.
pub fn mkdir_p(path: &str) -> Result<DirRef, String> {
    let mut cur: DirRef = root::get_root().clone();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        let next = cur.lock().get(seg);
        cur = match next {
            Some(FileOrDir::Dir(d)) => d,
            Some(FileOrDir::File(_)) => return Err(format!("« {} » est un fichier", seg)),
            None => vfs_node::VFSDirectory::create(seg.to_string(), &cur).map_err(String::from)?,
        };
    }
    Ok(cur)
}

// ================================================================
// TRAVAUX DE COPIE (incrémentaux)
// ================================================================

/// Étape d'un travail de copie.
enum Step {
    /// Copier le nœud `src` dans le dossier `dst` sous le nom `name`.
    Node { src: FileOrDir, dst: DirRef, name: String },
    /// Continuer la copie d'un fichier à partir de `offset`.
    Chunk { src: FileRef, dst: FileRef, offset: usize },
}

/// Copie récursive exécutée par petites étapes.
pub struct CopyJob {
    steps: VecDeque<Step>,
    pub done_bytes: u64,
    pub total_bytes: u64,
    pub done_items: usize,
    pub total_items: usize,
    pub errors: Vec<String>,
    /// Chemins créés au premier niveau (annulation).
    pub created: Vec<String>,
    pub label: String,
    /// Sources à supprimer quand la copie a réussi (déplacement par copie).
    pub move_sources: Vec<String>,
}

impl CopyJob {
    /// Prépare la copie de `sources` (chemins absolus) dans le dossier `dst_path`.
    pub fn new(sources: &[String], dst_path: &str) -> Result<CopyJob, String> {
        let dst = lookup_dir(dst_path).ok_or("dossier de destination introuvable")?;
        let mut job = CopyJob {
            steps: VecDeque::new(),
            done_bytes: 0,
            total_bytes: 0,
            done_items: 0,
            total_items: 0,
            errors: Vec::new(),
            created: Vec::new(),
            move_sources: Vec::new(),
            label: if sources.len() == 1 { format!("Copie de « {} »", basename(&sources[0])) } else { format!("Copie de {} éléments", sources.len()) },
        };
        for s in sources {
            let node = lookup(s).ok_or_else(|| format!("{} introuvable", s))?;
            if let FileOrDir::Dir(d) = &node {
                if fs_node::is_within(&dst, d) {
                    return Err(format!("Impossible de copier « {} » dans lui-même", basename(s)));
                }
                let (b, f, n, _) = tree_size(d, 1_000_000);
                job.total_bytes += b;
                job.total_items += f + n + 1;
            } else {
                job.total_bytes += node.len_bytes();
                job.total_items += 1;
            }
            let name = unique_name(&dst, basename(s), "copie");
            job.created.push(join(dst_path, &name));
            job.steps.push_back(Step::Node { src: node, dst: dst.clone(), name });
        }
        Ok(job)
    }

    pub fn finished(&self) -> bool { self.steps.is_empty() }

    /// Transforme la copie en déplacement : les sources seront supprimées après une copie
    /// sans erreur.
    pub fn into_move(mut self, sources: &[String]) -> CopyJob {
        self.move_sources = sources.to_vec();
        self.label = self.label.replacen("Copie", "Déplacement", 1);
        self
    }

    /// Avancement (‰).
    pub fn progress_pm(&self) -> u32 {
        if self.total_bytes > 0 {
            (self.done_bytes * 1000 / self.total_bytes.max(1)).min(1000) as u32
        } else {
            (self.done_items * 1000 / self.total_items.max(1)).min(1000) as u32
        }
    }

    /// Exécute des étapes jusqu'à épuiser `budget_ns`.
    pub fn run(&mut self, budget_ns: u64) {
        let start = task::scheduler::clock_ns();
        while let Some(step) = self.steps.pop_front() {
            self.step(step);
            if task::scheduler::clock_ns().saturating_sub(start) >= budget_ns {
                break;
            }
        }
    }

    fn step(&mut self, step: Step) {
        match step {
            Step::Node { src: FileOrDir::Dir(d), dst, name } => {
                match vfs_node::VFSDirectory::create(name.clone(), &dst) {
                    Ok(nd) => {
                        let kids = children(&d);
                        // Ordre en profondeur d'abord : les enfants passent devant le reste.
                        for k in kids.into_iter().rev() {
                            let kn = k.get_name();
                            self.steps.push_front(Step::Node { src: k, dst: nd.clone(), name: kn });
                        }
                    }
                    Err(e) => self.errors.push(format!("{} : {}", name, e)),
                }
                self.done_items += 1;
            }
            Step::Node { src: FileOrDir::File(f), dst, name } => {
                match memfs::MemFile::create(name.clone(), &dst) {
                    Ok(nf) => {
                        // Capacité réservée d'un coup : pas de réallocations pendant la copie.
                        let len = f.lock().len();
                        if len > 0 {
                            let _ = nf.lock().set_len(len);
                        }
                        self.steps.push_front(Step::Chunk { src: f, dst: nf, offset: 0 });
                    }
                    Err(e) => self.errors.push(format!("{} : {}", name, e)),
                }
                self.done_items += 1;
            }
            Step::Chunk { src, dst, offset } => {
                let len = src.lock().len();
                if offset >= len {
                    return;
                }
                let n = (len - offset).min(CHUNK);
                let mut buf = alloc::vec![0u8; n];
                let got = src.lock().read_at(&mut buf, offset).unwrap_or(0);
                if got == 0 {
                    self.errors.push("lecture interrompue".to_string());
                    return;
                }
                if dst.lock().write_at(&buf[..got], offset).is_err() {
                    self.errors.push("écriture impossible (mémoire pleine ?)".to_string());
                    return;
                }
                self.done_bytes += got as u64;
                if offset + got < len {
                    self.steps.push_front(Step::Chunk { src, dst, offset: offset + got });
                }
            }
        }
    }

    /// Annule le reste du travail (les copies partielles restent, comme sous les autres
    /// gestionnaires de fichiers ; l'annulation de l'opération les supprime).
    pub fn cancel(&mut self) { self.steps.clear(); }
}

trait LenBytes {
    fn len_bytes(&self) -> u64;
}

impl LenBytes for FileOrDir {
    fn len_bytes(&self) -> u64 {
        match self {
            FileOrDir::File(f) => f.lock().len() as u64,
            FileOrDir::Dir(_) => 0,
        }
    }
}

// ================================================================
// ANNULATION
// ================================================================

/// Opération annulable.
#[derive(Clone, Debug)]
pub enum Undo {
    /// Renommage dans `dir` : `old` → `new`.
    Rename { dir: String, old: String, new: String },
    /// Déplacements : (chemin d'origine, chemin actuel).
    Move(Vec<(String, String)>),
    /// Mises à la corbeille : noms dans la corbeille.
    Trash(Vec<String>),
    /// Éléments créés (copie, nouveau dossier ou fichier) : chemins.
    Create(Vec<String>),
}

impl Undo {
    pub fn describe(&self) -> String {
        match self {
            Undo::Rename { old, .. } => format!("renommage de « {} »", old),
            Undo::Move(v) => format!("déplacement de {} élément(s)", v.len()),
            Undo::Trash(v) => format!("mise à la corbeille de {} élément(s)", v.len()),
            Undo::Create(v) => format!("création de {} élément(s)", v.len()),
        }
    }

    /// Défait l'opération.
    pub fn revert(&self) -> Result<(), String> {
        match self {
            Undo::Rename { dir, old, new } => {
                let d = lookup_dir(dir).ok_or("dossier introuvable")?;
                rename(&d, new, old)
            }
            Undo::Move(v) => {
                for (orig, now) in v {
                    let node = lookup(now).ok_or_else(|| format!("{} introuvable", now))?;
                    let from = lookup_dir(&parent_of(now)).ok_or("dossier introuvable")?;
                    let to = lookup_dir(&parent_of(orig)).ok_or("dossier d'origine introuvable")?;
                    let name = unique_name(&to, basename(orig), "restauré");
                    fs_node::move_node(&node, &from, &to, &name).map_err(String::from)?;
                }
                Ok(())
            }
            Undo::Trash(v) => {
                for n in v {
                    restore(n)?;
                }
                Ok(())
            }
            Undo::Create(v) => {
                for p in v {
                    remove(p)?;
                }
                Ok(())
            }
        }
    }
}

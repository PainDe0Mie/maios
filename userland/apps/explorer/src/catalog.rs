//! Catalogue des applications du bureau et du menu MaiOS.
//!
//! Deux sources :
//! - les raccourcis natifs `root:/desktop/*.desk` (créés au premier démarrage), montrés sur le
//!   bureau (avec tout le reste de `/desktop` : `desk.rs`) et dans le menu ;
//! - les applications Linux de la racine ext4 (`/usr/share/applications/*.desktop`), montrées
//!   dans le menu et lancées par l'appli `run` dans l'environnement d'une session Wayland
//!   (`/opt/maios/apprun.sh`).
//!
//! Format `.desk` (texte, une clé=valeur par ligne, `#` pour les commentaires) :
//!
//! ```text
//! name=Terminal          nom affiché
//! prefix=shell-          préfixe du crate de l'appli dans l'espace de noms
//! color=1ABC9C           couleur de la tuile (hexadécimal RGB) quand il n'y a pas d'icône
//! args=/bin/sh x.sh      (facultatif) arguments, séparés par des blancs
//! needs=/usr/bin/x       (facultatif) raccourci caché si ce chemin n'existe pas
//! glyph=globe            (facultatif) pictogramme de la tuile : terminal, monitor, folder, globe
//! icon=firefox-esr       (facultatif) icône du thème (hicolor, pixmaps) ou chemin absolu
//! id=firefox-esr         (facultatif) identifiant : `app_id` des fenêtres de l'appli
//! comment=...            (facultatif) description
//! ```

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use fs_node::{DirRef, FileOrDir};

use crate::paint::{rgb, Glyph, Rgb};

/// Une application lançable.
pub struct App {
    /// Identifiant (`app_id` des fenêtres de l'appli, nom du fichier `.desktop`).
    pub id: String,
    pub name: String,
    pub comment: String,
    /// Icône du thème ou chemin absolu.
    pub icon: Option<String>,
    /// Tuile de secours : couleur et pictogramme.
    pub color: Rgb,
    pub glyph: Glyph,
    /// Clés (en minuscules) qui rattachent une fenêtre à l'appli : identifiant, classe de
    /// fenêtre, nom d'icône, nom du programme.
    pub keys: Vec<String>,
    /// Crate de l'appli dans l'espace de noms, et ses arguments.
    pub path: String,
    pub args: Vec<String>,
    /// Programme Linux : une seule instance (une seconde copie de Firefox refuse le profil
    /// déjà ouvert) ; cliquer de nouveau ramène sa fenêtre.
    pub single: bool,
}

impl App {
    /// La fenêtre d'identifiant `app_id` appartient-elle à cette appli ?
    pub fn matches(&self, app_id: &str) -> bool {
        if app_id.is_empty() {
            return false;
        }
        let a = app_id.to_lowercase();
        self.keys.iter().any(|k| *k == a)
    }

    /// Critère de la recherche du menu : rang (0 : le nom commence par la requête, 1 : un mot
    /// du nom, 2 : ailleurs dans le nom, la description ou les clés), `None` si rien ne
    /// correspond.
    pub fn rank(&self, query: &str) -> Option<u8> {
        if query.is_empty() {
            return Some(0);
        }
        let name = self.name.to_lowercase();
        if name.starts_with(query) {
            Some(0)
        } else if name.split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with(query)) {
            Some(1)
        } else if name.contains(query) || self.comment.to_lowercase().contains(query) || self.keys.iter().any(|k| k.contains(query)) {
            Some(2)
        } else {
            None
        }
    }
}

// ================================================================
// RACCOURCIS NATIFS
// ================================================================

/// Raccourcis par défaut : (fichier, contenu).
const DEFAULT_SHORTCUTS: &[(&str, &str)] = &[
    ("terminal.desk", "name=Terminal\nprefix=shell-\ncolor=1ABC9C\nid=maios.terminal\nglyph=terminal\ncomment=Interpréteur de commandes de MaiOS\n"),
    ("taskmanager.desk", "name=Gestionnaire des tâches\nprefix=task_manager-\ncolor=9B59B6\nid=maios.taskmanager\nglyph=monitor\ncomment=Processus, processeurs et mémoire\n"),
    ("files.desk", "name=Fichiers\nprefix=file_manager-\ncolor=E0AF68\nid=maios.files\nglyph=folder\ncomment=Parcourir les fichiers\n"),
    // Firefox de la racine Linux (`build/rootfs.ext4`, attachée en NVMe), lancé par `run` dans
    // l'environnement d'une session Wayland (`apprun.sh`, un an au plus).
    ("firefox.desk", "name=Firefox\nprefix=run-\ncolor=FF7139\nid=firefox-esr\nicon=firefox-esr\nglyph=globe\n\
args=/bin/bash /opt/maios/apprun.sh 31536000 firefox-esr\nneeds=/usr/lib/firefox-esr/firefox-esr\ncomment=Navigateur web\n"),
];

#[derive(Default)]
struct Desk {
    name: Option<String>,
    prefix: Option<String>,
    color: Option<u32>,
    args: Vec<String>,
    needs: Option<String>,
    glyph: Option<String>,
    icon: Option<String>,
    id: Option<String>,
    comment: String,
}

fn parse_desk(content: &str) -> Desk {
    let mut d = Desk::default();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        match k.trim() {
            "name" => d.name = Some(v.to_string()),
            "prefix" => d.prefix = Some(v.to_string()),
            "color" => d.color = u32::from_str_radix(v, 16).ok(),
            "args" => d.args = v.split_whitespace().map(String::from).collect(),
            "needs" => d.needs = Some(v.to_string()),
            "glyph" => d.glyph = Some(v.to_string()),
            "icon" => d.icon = Some(v.to_string()),
            "id" => d.id = Some(v.to_string()),
            "comment" => d.comment = v.to_string(),
            _ => {}
        }
    }
    d
}

/// Crée `root:/desktop` et ses raccourcis par défaut s'il n'existe pas.
pub fn init_desktop() {
    let root = root::get_root();
    let exists = matches!(root.lock().get("desktop"), Some(FileOrDir::Dir(_)));
    if exists {
        return;
    }
    let desktop: DirRef = match vfs_node::VFSDirectory::create("desktop".to_string(), root) {
        Ok(d) => d,
        Err(e) => {
            error!("[explorer] impossible de créer /desktop : {e}");
            return;
        }
    };
    for (file, content) in DEFAULT_SHORTCUTS {
        match memfs::MemFile::create(file.to_string(), &desktop) {
            Ok(f) => {
                let _ = f.lock().write_at(content.as_bytes(), 0);
            }
            Err(e) => warn!("[explorer] impossible de créer {file} : {e}"),
        }
    }
}

/// Contenu d'un fichier (liens suivis), `None` s'il manque, n'est pas un fichier ou dépasse
/// `limit` octets.
pub fn read_file(path: &str, limit: usize) -> Option<Vec<u8>> {
    let p: &path::Path = path.as_ref();
    let FileOrDir::File(f) = p.get(root::get_root())? else { return None };
    let len = f.lock().len();
    if len > limit {
        return None;
    }
    let mut buf = alloc::vec![0u8; len];
    let n = f.lock().read_at(&mut buf, 0).ok()?;
    buf.truncate(n);
    Some(buf)
}

pub fn exists(path: &str) -> bool {
    let p: &path::Path = path.as_ref();
    p.get(root::get_root()).is_some()
}

/// Noms des entrées d'un dossier, triés.
pub fn list_dir(path: &str) -> Vec<String> {
    let p: &path::Path = path.as_ref();
    let Some(FileOrDir::Dir(d)) = p.get(root::get_root()) else { return Vec::new() };
    let mut names = d.lock().list();
    names.sort();
    names
}

/// Chemin du crate de l'appli dont le nom commence par `prefix` dans l'espace de noms.
pub fn crate_path(prefix: &str) -> Option<String> {
    let ns = task::with_current_task(|t| t.namespace.dir().clone()).ok()?;
    let f = ns.get_file_starting_with(prefix)?;
    let p = f.lock().get_absolute_path();
    Some(p)
}

fn glyph_named(name: Option<&str>, app_name: &str) -> Glyph {
    match name {
        Some("terminal") => Glyph::Terminal,
        Some("monitor") => Glyph::Monitor,
        Some("folder") => Glyph::Folder,
        Some("globe") => Glyph::Globe,
        _ => Glyph::Letter(app_name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_ascii_uppercase()).unwrap_or('?')),
    }
}

/// Couleur de tuile stable tirée d'un nom (applis sans icône ni couleur).
pub fn color_for(name: &str) -> Rgb {
    const PALETTE: [u32; 8] = [0x7AA2F7, 0xBB9AF7, 0x7DCFFF, 0x9ECE6A, 0xE0AF68, 0xF7768E, 0x2AC3DE, 0xFF9E64];
    let h = name.bytes().fold(0x811C_9DC5u32, |h, b| (h ^ b as u32).wrapping_mul(0x0100_0193));
    rgb(PALETTE[(h % PALETTE.len() as u32) as usize])
}

fn push_key(keys: &mut Vec<String>, k: &str) {
    let k = k.trim().to_lowercase();
    if !k.is_empty() && !keys.contains(&k) {
        keys.push(k);
    }
}

/// Raccourci `/desktop/<file>` (`.desk`) : `Ok(None)` s'il est masqué (`needs=` absent),
/// `Err` s'il est invalide (le bureau le montre alors comme un simple fichier).
pub fn desk_app(file: &str) -> Result<Option<App>, ()> {
    let text = read_file(&format!("/desktop/{file}"), 64 * 1024).ok_or(())?;
    let text = core::str::from_utf8(&text).map_err(|_| ())?;
    let d = parse_desk(text);
    let (Some(app_name), Some(prefix)) = (d.name, d.prefix) else {
        warn!("[explorer] /desktop/{file} : name= ou prefix= manquant");
        return Err(());
    };
    if let Some(needs) = &d.needs {
        if !exists(needs) {
            info!("[explorer] '{app_name}' masqué : {needs} introuvable");
            return Ok(None);
        }
    }
    let Some(path) = crate_path(&prefix) else {
        warn!("[explorer] appli '{app_name}' (préfixe '{prefix}') absente de l'espace de noms");
        return Err(());
    };
    let id = d.id.unwrap_or_else(|| file.trim_end_matches(".desk").to_string());
    let mut keys = Vec::new();
    push_key(&mut keys, &id);
    if let Some(i) = d.icon.as_deref().filter(|i| !i.starts_with('/')) {
        push_key(&mut keys, i);
    }
    let single = prefix == "run-";
    Ok(Some(App {
        color: d.color.map(rgb).unwrap_or_else(|| color_for(&app_name)),
        glyph: glyph_named(d.glyph.as_deref(), &app_name),
        id,
        comment: d.comment,
        icon: d.icon,
        keys,
        path,
        args: d.args,
        single,
        name: app_name,
    }))
}

fn native_apps() -> Vec<App> {
    list_dir("/desktop").iter().filter(|n| n.ends_with(".desk")).filter_map(|n| desk_app(n).ok().flatten()).collect()
}

// ================================================================
// APPLICATIONS LINUX (.desktop)
// ================================================================

const APPS_DIR: &str = "/usr/share/applications";
pub const APPRUN: &str = "/opt/maios/apprun.sh";
const PATH_DIRS: [&str; 6] = ["/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin", "/usr/games"];

/// Entrées qui ne marchent pas seules dans une session MaiLand : clients X11 (pas de serveur X
/// sans `xtest.sh`), client et serveur de foot (le serveur attend des connexions sans fenêtre).
const UNSUPPORTED: [&str; 4] = ["debian-xterm", "debian-uxterm", "footclient", "foot-server"];

/// Chemin du programme `prog` (absolu tel quel, sinon cherché dans le `PATH` de `apprun.sh`).
fn which(prog: &str) -> Option<String> {
    if prog.starts_with('/') {
        return exists(prog).then(|| prog.to_string());
    }
    if prog.contains('/') {
        return None;
    }
    PATH_DIRS.iter().map(|d| format!("{d}/{prog}")).find(|p| exists(p))
}

/// Entrée `.desktop` de chemin `full` (identifiant `id`) : `None` si elle ne se lance pas
/// (type, `Hidden`, `TryExec` ou programme absent, mode texte sans `foot`).
fn linux_app(full: &str, id: &str, foot: Option<&String>, run_path: &str) -> Option<App> {
    let text = read_file(full, 256 * 1024)?;
    let text = core::str::from_utf8(&text).ok()?;
    let e = mai_icons::desktop::parse(text, "fr_FR")?;
    if e.kind != "Application" || e.hidden || e.exec.is_empty() {
        return None;
    }
    if !e.try_exec.is_empty() && which(&e.try_exec).is_none() {
        return None;
    }
    let mut argv = mai_icons::desktop::exec_args(&e.exec, &e.name, full)?;
    let prog = argv.first().cloned()?;
    which(&prog)?;
    let base = prog.rsplit('/').next().unwrap_or(&prog).to_string();
    if e.terminal {
        // Programme en mode texte : dans un terminal Wayland.
        let mut v = alloc::vec![foot?.clone(), "-e".to_string()];
        v.append(&mut argv);
        argv = v;
    }
    let mut keys = Vec::new();
    push_key(&mut keys, id);
    push_key(&mut keys, &e.startup_wm_class);
    if !e.icon.starts_with('/') {
        push_key(&mut keys, &e.icon);
    }
    push_key(&mut keys, &base);
    let mut args = alloc::vec!["/bin/bash".to_string(), APPRUN.to_string(), "31536000".to_string()];
    args.extend(argv);
    Some(App {
        id: id.to_string(),
        color: color_for(&e.name),
        glyph: glyph_named(None, &e.name),
        comment: if e.comment.is_empty() { e.generic_name.clone() } else { e.comment.clone() },
        icon: (!e.icon.is_empty()).then(|| e.icon.clone()),
        keys,
        path: run_path.to_string(),
        args,
        single: true,
        name: e.name,
    })
}

fn linux_apps(run_path: &str) -> Vec<App> {
    let foot = which("foot");
    list_dir(APPS_DIR)
        .iter()
        .filter_map(|file| {
            let id = file.strip_suffix(".desktop")?;
            if UNSUPPORTED.contains(&id) {
                return None;
            }
            linux_app(&format!("{APPS_DIR}/{file}"), id, foot.as_ref(), run_path)
        })
        .collect()
}

/// Lanceur `.desktop` posé sur le bureau (chemin absolu) : `None` s'il ne se lance pas ici.
pub fn desktop_entry(path: &str) -> Option<App> {
    if !exists(APPRUN) {
        return None;
    }
    let run = crate_path("run-")?;
    let id = path.rsplit('/').next()?.strip_suffix(".desktop")?;
    linux_app(path, id, which("foot").as_ref(), &run)
}

/// Toutes les applications : natives d'abord (ordre de `/desktop`), puis Linux ; triées par
/// nom pour le menu par l'appelant. Une appli Linux déjà couverte par un raccourci natif
/// (même identifiant, Firefox) n'apparaît qu'une fois.
pub fn load() -> Vec<App> {
    let mut apps = native_apps();
    if exists(APPRUN) {
        if let Some(run) = crate_path("run-") {
            for a in linux_apps(&run) {
                if !apps.iter().any(|n| a.keys.iter().any(|k| n.matches(k))) {
                    apps.push(a);
                }
            }
        }
    }
    info!("[explorer] {} application(s) au catalogue", apps.len());
    apps
}

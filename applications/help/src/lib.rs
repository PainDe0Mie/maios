//! Commande `help` — affiche les commandes disponibles dans MaiOS.
//!
//! Usage :
//!   help            — liste toutes les commandes
//!   help <commande> — détail d'une commande spécifique
//!   help --builtins — affiche les commandes intégrées au shell

#![no_std]
extern crate alloc;
#[macro_use]
extern crate app_io;
extern crate task;
extern crate fs_node;
extern crate getopts;

use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;
use getopts::Options;

// ────────────────────────────────────────────────────────────────
// Base de données des commandes avec descriptions
// ────────────────────────────────────────────────────────────────

struct CmdInfo {
    name: &'static str,
    category: &'static str,
    description: &'static str,
}

const COMMANDS: &[CmdInfo] = &[
    // ── Fichiers & Répertoires ──
    CmdInfo { name: "ls",       category: "Fichiers",  description: "Lister le contenu d'un répertoire" },
    CmdInfo { name: "cat",      category: "Fichiers",  description: "Afficher le contenu d'un fichier" },
    CmdInfo { name: "cp",       category: "Fichiers",  description: "Copier un fichier" },
    CmdInfo { name: "mv",       category: "Fichiers",  description: "Déplacer ou renommer un fichier" },
    CmdInfo { name: "rm",       category: "Fichiers",  description: "Supprimer un fichier" },
    CmdInfo { name: "mkdir",    category: "Fichiers",  description: "Créer un répertoire" },
    CmdInfo { name: "touch",    category: "Fichiers",  description: "Créer un fichier vide" },
    CmdInfo { name: "pwd",      category: "Fichiers",  description: "Afficher le répertoire courant" },
    CmdInfo { name: "cd",       category: "Fichiers",  description: "Changer de répertoire (builtin)" },
    CmdInfo { name: "head",     category: "Fichiers",  description: "Afficher les premières lignes d'un fichier" },
    CmdInfo { name: "tail",     category: "Fichiers",  description: "Afficher les dernières lignes d'un fichier" },
    CmdInfo { name: "wc",       category: "Fichiers",  description: "Compter lignes, mots et octets" },
    CmdInfo { name: "less",     category: "Fichiers",  description: "Naviguer dans un fichier page par page" },

    // ── Traitement de texte ──
    CmdInfo { name: "grep",     category: "Texte",     description: "Rechercher un motif dans des fichiers" },
    CmdInfo { name: "sort",     category: "Texte",     description: "Trier les lignes" },
    CmdInfo { name: "uniq",     category: "Texte",     description: "Supprimer les lignes dupliquées" },
    CmdInfo { name: "tr",       category: "Texte",     description: "Traduire ou supprimer des caractères" },
    CmdInfo { name: "rev",      category: "Texte",     description: "Inverser les lignes" },
    CmdInfo { name: "nl",       category: "Texte",     description: "Numéroter les lignes" },
    CmdInfo { name: "tee",      category: "Texte",     description: "Lire stdin et écrire dans stdout + fichier" },
    CmdInfo { name: "xxd",      category: "Texte",     description: "Afficher un dump hexadécimal" },

    // ── Chemins ──
    CmdInfo { name: "basename", category: "Chemins",   description: "Extraire le nom de fichier d'un chemin" },
    CmdInfo { name: "dirname",  category: "Chemins",   description: "Extraire le répertoire d'un chemin" },

    // ── Système ──
    CmdInfo { name: "ps",       category: "Système",   description: "Lister les processus en cours" },
    CmdInfo { name: "kill",     category: "Système",   description: "Terminer un processus" },
    CmdInfo { name: "uptime",   category: "Système",   description: "Afficher le temps de fonctionnement" },
    CmdInfo { name: "hostname", category: "Système",   description: "Afficher le nom de la machine" },
    CmdInfo { name: "free",     category: "Système",   description: "Afficher l'utilisation mémoire" },
    CmdInfo { name: "df",       category: "Système",   description: "Afficher l'espace disque" },
    CmdInfo { name: "du",       category: "Système",   description: "Estimer l'utilisation d'espace par fichier" },
    CmdInfo { name: "date",     category: "Système",   description: "Afficher la date et l'heure" },
    CmdInfo { name: "lspci",    category: "Système",   description: "Lister les périphériques PCI" },
    CmdInfo { name: "deps",     category: "Système",   description: "Afficher les dépendances d'un crate" },
    CmdInfo { name: "ns",       category: "Système",   description: "Afficher les espaces de noms du noyau" },
    CmdInfo { name: "rq",       category: "Système",   description: "Afficher les files d'attente d'exécution" },
    CmdInfo { name: "print_fault_log", category: "Système", description: "Afficher le journal des fautes système" },

    // ── Environnement ──
    CmdInfo { name: "env",      category: "Environnement", description: "Afficher les variables d'environnement" },
    CmdInfo { name: "printenv", category: "Environnement", description: "Afficher une variable d'environnement" },
    CmdInfo { name: "export",   category: "Environnement", description: "Définir une variable (builtin)" },

    // ── Utilitaires ──
    CmdInfo { name: "cal",      category: "Utilitaires", description: "Afficher un calendrier" },
    CmdInfo { name: "seq",      category: "Utilitaires", description: "Générer une séquence de nombres" },
    CmdInfo { name: "factor",   category: "Utilitaires", description: "Décomposer en facteurs premiers" },
    CmdInfo { name: "yes",      category: "Utilitaires", description: "Afficher une chaîne en boucle" },
    CmdInfo { name: "true_cmd", category: "Utilitaires", description: "Retourner le code de succès (0)" },
    CmdInfo { name: "false_cmd",category: "Utilitaires", description: "Retourner le code d'erreur (1)" },
    CmdInfo { name: "sleep_cmd",category: "Utilitaires", description: "Attendre N secondes" },
    CmdInfo { name: "hello",    category: "Utilitaires", description: "Afficher un message de bienvenue" },
    CmdInfo { name: "bm",       category: "Utilitaires", description: "Benchmark de performances système" },

    // ── Réseau ──
    CmdInfo { name: "ping",     category: "Réseau",    description: "Envoyer des paquets ICMP" },
    CmdInfo { name: "download", category: "Réseau",    description: "Télécharger un fichier via HTTP" },

    // ── Audio ──
    CmdInfo { name: "audio_test",   category: "Audio", description: "Tester la sortie audio Intel HDA" },

    // ── Applications graphiques ──
    CmdInfo { name: "file_manager",  category: "GUI",  description: "Gestionnaire de fichiers graphique" },
    CmdInfo { name: "explorer",      category: "GUI",  description: "Bureau avec icônes et barre des tâches" },
    CmdInfo { name: "task_manager",  category: "GUI",  description: "Gestionnaire de tâches graphique" },
    CmdInfo { name: "taskbar",       category: "GUI",  description: "Barre des tâches" },

    // ── Exécution ──
    CmdInfo { name: "run",      category: "Exécution", description: "Exécuter un binaire ELF Linux via MEB" },
    CmdInfo { name: "loadc",    category: "Exécution", description: "Charger et exécuter un binaire C/ELF natif" },
    CmdInfo { name: "wasm",     category: "Exécution", description: "Exécuter un module WebAssembly (.wasm)" },

    // ── Shell ──
    CmdInfo { name: "hull",     category: "Shell",     description: "Shell interactif MaiOS (Hull)" },
    CmdInfo { name: "shell",    category: "Shell",     description: "Shell interactif MaiOS (legacy)" },
    CmdInfo { name: "help",     category: "Shell",     description: "Afficher cette aide" },
];

const BUILTINS: &[(&str, &str)] = &[
    ("cd",      "Changer de répertoire"),
    ("exit",    "Quitter le shell"),
    ("history", "Afficher l'historique des commandes"),
    ("jobs",    "Lister les tâches en arrière-plan"),
    ("bg",      "Reprendre une tâche en arrière-plan"),
    ("fg",      "Reprendre une tâche au premier plan"),
    ("alias",   "Définir un alias"),
    ("unalias", "Supprimer un alias"),
    ("export",  "Définir une variable d'environnement"),
    ("unset",   "Supprimer une variable d'environnement"),
    ("set",     "Afficher ou modifier les options du shell"),
    ("exec",    "Remplacer le shell par une commande"),
    ("wait",    "Attendre la fin d'un processus"),
    ("echo",    "Afficher du texte"),
    ("whoami",  "Afficher l'utilisateur courant"),
    ("uname",   "Afficher les informations système"),
    ("clear",   "Effacer le terminal (alias: cls)"),
    ("help",    "Afficher l'aide"),
];

// ────────────────────────────────────────────────────────────────
// Point d'entrée
// ────────────────────────────────────────────────────────────────

pub fn main(args: Vec<String>) -> isize {
    let mut opts = Options::new();
    opts.optflag("h", "help", "Afficher l'aide");
    opts.optflag("b", "builtins", "Afficher les commandes intégrées au shell");
    opts.optflag("a", "all", "Afficher aussi les commandes de développement/test");

    let matches = match opts.parse(args) {
        Ok(m) => m,
        Err(f) => {
            println!("{}", f);
            return -1;
        }
    };

    if matches.opt_present("h") {
        println!("{}", opts.usage(USAGE));
        return 0;
    }

    if matches.opt_present("b") {
        print_builtins();
        return 0;
    }

    // help <command> — rechercher une commande spécifique
    if !matches.free.is_empty() {
        let query = &matches.free[0];
        return search_command(query);
    }

    // Par défaut : lister toutes les commandes par catégorie
    print_all_commands(matches.opt_present("a"));
    0
}

fn print_all_commands(show_dev: bool) {
    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║            MaiOS — Commandes disponibles               ║");
    println!("╚══════════════════════════════════════════════════════════╝");
    println!();

    let categories = [
        "Fichiers", "Texte", "Chemins", "Système", "Environnement",
        "Utilitaires", "Réseau", "Audio", "GUI", "Exécution", "Shell",
    ];

    for cat in &categories {
        let cmds: Vec<&CmdInfo> = COMMANDS.iter()
            .filter(|c| c.category == *cat)
            .collect();
        if cmds.is_empty() { continue; }

        println!("── {} ──", cat);
        for cmd in &cmds {
            println!("  {:16} {}", cmd.name, cmd.description);
        }
        println!();
    }

    if show_dev {
        println!("── Développement & Tests ──");
        println!("  {:16} {}", "swap", "Échanger un crate à chaud (live patching)");
        println!("  {:16} {}", "upd", "Mettre à jour un crate depuis le réseau (OTA)");
        println!("  {:16} {}", "syscall_trace", "Tracer les appels système en temps réel");
        println!("  {:16} {}", "raw_mode", "Passer le terminal en mode brut");
        println!("  {:16} {}", "serial_echo", "Echo sur le port série");
        println!("  {:16} {}", "seconds_counter", "Compteur de secondes (test timer)");
        println!("  {:16} {}", "qemu_test", "Tests spécifiques à QEMU");
        println!("  {:16} {}", "heap_eval", "Benchmark de l'allocateur (shbench)");
        println!("  {:16} {}", "scheduler_eval", "Benchmark du scheduler MKS");
        println!("  {:16} {}", "channel_eval", "Benchmark des canaux IPC");
        println!("  {:16} {}", "rq_eval", "Benchmark des run queues");
        println!("  {:16} {}", "pmu_sample_start", "Démarrer l'échantillonnage PMU (x86)");
        println!("  {:16} {}", "pmu_sample_stop", "Arrêter l'échantillonnage PMU");
        println!("  {:16} {}", "test_*", "Suites de tests (test_async, test_libc, test_win_compat, ...)");
        println!();
    }

    // Résumé dynamique depuis l'espace de noms
    let app_count = list_namespace_apps().len();
    println!("Total: {} commandes chargées dans l'espace de noms", app_count);
    println!();
    println!("Tapez 'help <commande>' pour plus de détails.");
    println!("Tapez 'help --builtins' pour les commandes intégrées au shell.");
    println!("Tapez '<commande> --help' pour l'aide d'une commande.");
}

fn print_builtins() {
    println!("── Commandes intégrées au shell (builtins) ──");
    println!();
    for (name, desc) in BUILTINS {
        println!("  {:12} {}", name, desc);
    }
    println!();
    println!("Les builtins s'exécutent directement dans le shell sans créer de processus.");
}

fn search_command(query: &str) -> isize {
    // Chercher dans la base de données
    if let Some(cmd) = COMMANDS.iter().find(|c| c.name == query) {
        println!("{} — {}", cmd.name, cmd.description);
        println!("  Catégorie : {}", cmd.category);
        println!();
        println!("Tapez '{} --help' pour l'usage détaillé.", cmd.name);
        return 0;
    }

    // Chercher dans les builtins
    if let Some((name, desc)) = BUILTINS.iter().find(|(n, _)| *n == query) {
        println!("{} — {} (builtin)", name, desc);
        println!();
        println!("Commande intégrée au shell. S'exécute dans le processus du shell.");
        return 0;
    }

    // Chercher dans l'espace de noms
    let apps = list_namespace_apps();
    let matches: Vec<&String> = apps.iter()
        .filter(|a| a.starts_with(query))
        .collect();

    if matches.is_empty() {
        println!("Commande '{}' non trouvée.", query);
        println!("Tapez 'help' pour voir toutes les commandes disponibles.");
        return 1;
    }

    println!("Commandes correspondant à '{}' :", query);
    for m in matches {
        println!("  {}", m);
    }
    0
}

fn list_namespace_apps() -> Vec<String> {
    let mut apps = Vec::new();
    if let Ok(ns) = task::with_current_task(|t| t.namespace.clone()) {
        let dir = ns.dir();
        let locked = dir.lock();
        for name in locked.list() {
            // Les applications sont préfixées par le nom du crate + '-'
            if let Some(app_name) = name.split('-').next() {
                if !apps.contains(&alloc::string::String::from(app_name)) {
                    apps.push(alloc::string::String::from(app_name));
                }
            }
        }
    }
    apps.sort();
    apps
}

const USAGE: &str = "Usage: help [OPTIONS] [COMMANDE]
Affiche les commandes disponibles dans MaiOS.

Sans argument, liste toutes les commandes par catégorie.
Avec un argument, affiche les détails d'une commande.";

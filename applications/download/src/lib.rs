//! `download` — Telecharger des fichiers via HTTP sur MaiOS.
//!
//! Usage :
//!   download <url> [destination]
//!   download http://example.com/file.bin /disk/downloads/file.bin
//!   download http://10.0.2.2:8080/doom.wad /disk/apps/doom.wad
//!
//! Le fichier est telecharge via HTTP GET et sauvegarde dans le VFS.
//! Si aucune destination n'est specifiee, le fichier est sauvegarde
//! dans /disk/downloads/ avec le nom extrait de l'URL.
//!
//! Options :
//!   -o, --output <path>   Chemin de destination
//!   -v, --verbose         Afficher les details de la requete
//!   -h, --help            Afficher l'aide

#![no_std]
extern crate alloc;
#[macro_use]
extern crate app_io;
extern crate getopts;
extern crate net;
extern crate http_client;
extern crate dns_resolver;
extern crate heapfile;
extern crate vfs_node;
extern crate root;
extern crate path;
extern crate task;
extern crate fs_node;
extern crate time;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::format;
use getopts::Options;
use fs_node::{FileOrDir, DirRef, Directory};
use net::IpEndpoint;

// ────────────────────────────────────────────────────────────────
// URL PARSING (minimal, HTTP seulement)
// ────────────────────────────────────────────────────────────────
struct ParsedUrl {
    host: String,
    port: u16,
    path: String,
    filename: String,
}

fn parse_url(url: &str) -> Result<ParsedUrl, &'static str> {
    let url = url.trim();

    // Retirer le schema http://
    let rest = if let Some(r) = url.strip_prefix("http://") {
        r
    } else if url.contains("://") {
        return Err("seul HTTP est supporte (pas de HTTPS)");
    } else {
        url
    };

    // Separer host:port et path
    let (host_port, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };

    // Separer host et port
    let (host, port) = match host_port.rfind(':') {
        Some(i) => {
            let port_str = &host_port[i + 1..];
            let port = port_str.parse::<u16>().unwrap_or(80);
            (&host_port[..i], port)
        }
        None => (host_port, 80),
    };

    // Extraire le nom de fichier
    let filename = path.rsplit('/').next().unwrap_or("download");
    let filename = if filename.is_empty() { "download" } else { filename };

    Ok(ParsedUrl {
        host: String::from(host),
        port,
        path: String::from(path),
        filename: String::from(filename),
    })
}

// ────────────────────────────────────────────────────────────────
// NAVIGATION VFS
// ────────────────────────────────────────────────────────────────
fn navigate_to(path_str: &str) -> Option<DirRef> {
    let root_dir = root::get_root().clone();
    if path_str == "/" { return Some(root_dir); }
    let mut cur = root_dir;
    for segment in path_str.split('/').filter(|s| !s.is_empty()) {
        let next = cur.lock().get(segment)
            .and_then(|fod| if let FileOrDir::Dir(d) = fod { Some(d) } else { None })?;
        cur = next;
    }
    Some(cur)
}

fn ensure_dir(path_str: &str) -> Option<DirRef> {
    if let Some(d) = navigate_to(path_str) {
        return Some(d);
    }
    // Tenter de creer le chemin
    let root_dir = root::get_root().clone();
    let mut cur = root_dir;
    for segment in path_str.split('/').filter(|s| !s.is_empty()) {
        let next = {
            let locked = cur.lock();
            locked.get(segment)
                .and_then(|fod| if let FileOrDir::Dir(d) = fod { Some(d) } else { None })
        };
        cur = match next {
            Some(d) => d,
            None => {
                match vfs_node::VFSDirectory::create(String::from(segment), &cur) {
                    Ok(d) => d,
                    Err(_) => return None,
                }
            }
        };
    }
    Some(cur)
}

// ────────────────────────────────────────────────────────────────
// POINT D'ENTREE
// ────────────────────────────────────────────────────────────────
pub fn main(args: Vec<String>) -> isize {
    let mut opts = Options::new();
    opts.optopt("o", "output", "Chemin de destination", "PATH");
    opts.optflag("v", "verbose", "Mode verbeux");
    opts.optflag("h", "help", "Afficher l'aide");

    let matches = match opts.parse(&args) {
        Ok(m) => m,
        Err(f) => {
            println!("Erreur: {}", f);
            return -1;
        }
    };

    if matches.opt_present("h") || matches.free.is_empty() {
        println!("download — Telecharger des fichiers via HTTP");
        println!();
        println!("Usage: download [OPTIONS] <url>");
        println!();
        println!("Options:");
        println!("  -o, --output <path>   Chemin de destination");
        println!("  -v, --verbose         Mode verbeux");
        println!("  -h, --help            Afficher l'aide");
        println!();
        println!("Exemples:");
        println!("  download http://10.0.2.2:8080/doom.wad");
        println!("  download http://example.com/file.bin -o /disk/apps/file.bin");
        println!();
        println!("Note: seul HTTP est supporte (pas de HTTPS).");
        println!("Le serveur QEMU host est accessible via 10.0.2.2");
        return 0;
    }

    let verbose = matches.opt_present("v");
    let url_str = &matches.free[0];

    // Parser l'URL
    let url = match parse_url(url_str) {
        Ok(u) => u,
        Err(e) => {
            println!("Erreur URL: {}", e);
            return -1;
        }
    };

    if verbose {
        println!("Host: {}:{}", url.host, url.port);
        println!("Path: {}", url.path);
        println!("Fichier: {}", url.filename);
    }

    // Determiner la destination
    let (dest_dir_path, dest_filename) = if let Some(output) = matches.opt_str("o") {
        if let Some((dir, file)) = output.rsplit_once('/') {
            let dir = if dir.is_empty() { "/" } else { dir };
            (String::from(dir), String::from(file))
        } else {
            ("/disk/downloads".to_string(), output)
        }
    } else {
        ("/disk/downloads".to_string(), url.filename.clone())
    };

    // Resoudre l'IP
    println!("Resolution de {}...", url.host);
    let ip = if url.host.contains('.') && url.host.chars().all(|c| c.is_ascii_digit() || c == '.') {
        // Adresse IP directe (ex: 10.0.2.2)
        let parts: Vec<&str> = url.host.split('.').collect();
        if parts.len() != 4 {
            println!("Erreur: adresse IP invalide: {}", url.host);
            return -1;
        }
        let mut octets = [0u8; 4];
        for (i, part) in parts.iter().enumerate() {
            match part.parse::<u8>() {
                Ok(v) => octets[i] = v,
                Err(_) => {
                    println!("Erreur: octet IP invalide: {}", part);
                    return -1;
                }
            }
        }
        net::wire::Ipv4Address::new(octets[0], octets[1], octets[2], octets[3])
    } else {
        // Resolution DNS
        match dns_resolver::resolve(&url.host) {
            Ok(ip) => {
                println!("Resolu: {} -> {}", url.host, ip);
                ip
            }
            Err(e) => {
                println!("Erreur DNS: {}", e);
                return -1;
            }
        }
    };

    let endpoint = IpEndpoint::new(
        net::wire::IpAddress::Ipv4(ip),
        url.port,
    );

    // Obtenir l'interface reseau
    let iface = match net::get_default_interface() {
        Some(i) => i,
        None => {
            println!("Erreur: aucune interface reseau disponible");
            return -1;
        }
    };

    // Construire la requete HTTP
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: MaiOS/0.1\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        url.path, url.host,
    );

    println!("Connexion a {}:{}...", ip, url.port);

    // Choisir un port local pseudo-aleatoire base sur l'horloge
    let now = time::Instant::now();
    let local_port = 49152 + (now.elapsed().as_millis() % 16000) as u16;

    let mut client = match http_client::HttpClient::new(&iface, local_port, endpoint) {
        Ok(c) => c,
        Err(e) => {
            println!("Erreur connexion: {}", e);
            return -1;
        }
    };

    println!("Telechargement de {}...", url.path);

    let timeout = time::Duration::from_secs(30);
    let response = match client.send(request, Some(timeout)) {
        Ok(r) => r,
        Err(e) => {
            println!("Erreur HTTP: {}", e);
            return -1;
        }
    };

    match response.as_result() {
        Ok(content) => {
            let size = content.len();
            println!("Recu: {} octets (HTTP {})", size, response.status_code);

            if verbose {
                if let Ok(headers) = core::str::from_utf8(response.header_bytes()) {
                    println!("--- Headers ---");
                    println!("{}", headers);
                    println!("---------------");
                }
            }

            // S'assurer que le repertoire destination existe
            let dest_dir = match ensure_dir(&dest_dir_path) {
                Some(d) => d,
                None => {
                    println!("Erreur: impossible de creer le repertoire {}", dest_dir_path);
                    return -1;
                }
            };

            // Supprimer le fichier existant s'il y en a un
            {
                let mut locked = dest_dir.lock();
                if let Some(existing) = locked.get(&dest_filename) {
                    locked.remove(&existing);
                }
            }

            // Sauvegarder le fichier
            match heapfile::HeapFile::from_vec(
                content.to_vec(),
                dest_filename.clone(),
                &dest_dir,
            ) {
                Ok(_) => {
                    let full_path = format!("{}/{}", dest_dir_path, dest_filename);
                    println!("Sauvegarde: {} ({} octets)", full_path, size);

                    // Affichage taille humaine
                    let size_str = if size < 1024 {
                        format!("{} B", size)
                    } else if size < 1024 * 1024 {
                        format!("{:.1} KB", size as f64 / 1024.0)
                    } else {
                        format!("{:.1} MB", size as f64 / (1024.0 * 1024.0))
                    };
                    println!("Telechargement termine: {} ({})", dest_filename, size_str);
                }
                Err(e) => {
                    println!("Erreur ecriture fichier: {}", e);
                    return -1;
                }
            }
        }
        Err((code, reason)) => {
            println!("Erreur HTTP {}: {}", code, reason);
            return -1;
        }
    }

    0
}

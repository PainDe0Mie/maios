//! Tests sur l'hôte. Les vecteurs (zlib de toutes stratégies, PNG de tous types) viennent de
//! `tests/mkvectors.py` : `MAI_ICONS_VECTORS=<dossier> cargo test`. Sans la variable, seuls
//! les tests autonomes tournent.

use super::*;
use std::vec;

fn vectors() -> Option<std::path::PathBuf> {
    std::env::var_os("MAI_ICONS_VECTORS").map(Into::into)
}

#[test]
fn zlib_vectors() {
    let Some(dir) = vectors() else { return };
    let mut n = 0;
    for i in 0.. {
        let z = dir.join(format!("z{i:03}.zlib"));
        let Ok(data) = std::fs::read(&z) else { break };
        let raw = std::fs::read(dir.join(format!("z{i:03}.raw"))).unwrap();
        let out = inflate::zlib_decompress(&data, raw.len()).unwrap_or_else(|e| panic!("{z:?} : {e:?}"));
        assert_eq!(out, raw, "{z:?}");
        if !raw.is_empty() {
            // Une sortie bornée trop court est refusée, jamais tronquée en silence.
            assert_eq!(inflate::zlib_decompress(&data, raw.len() - 1), Err(inflate::Error::TooLarge), "{z:?}");
        }
        // Flux coupé : erreur, jamais de panique.
        for cut in [1, data.len() / 2, data.len() - 1] {
            assert!(inflate::zlib_decompress(&data[..cut.min(data.len())], raw.len()).is_err());
        }
        n += 1;
    }
    assert!(n > 0);
}

#[test]
fn png_vectors() {
    let Some(dir) = vectors() else { return };
    let mut n = 0;
    for i in 0.. {
        let p = dir.join(format!("p{i:03}.png"));
        let Ok(data) = std::fs::read(&p) else { break };
        let exp = std::fs::read(dir.join(format!("p{i:03}.rgba"))).unwrap();
        let w = u32::from_le_bytes(exp[0..4].try_into().unwrap()) as usize;
        let h = u32::from_le_bytes(exp[4..8].try_into().unwrap()) as usize;
        let img = png::decode(&data, 1 << 20).unwrap_or_else(|e| panic!("{p:?} : {e:?}"));
        assert_eq!((img.width, img.height), (w, h), "{p:?}");
        let got: Vec<u8> = img.pixels.iter().flatten().copied().collect();
        assert_eq!(got, exp[8..], "{p:?}");
        // CRC faux : refusé.
        let mut bad = data.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert_eq!(png::decode(&bad, 1 << 20), Err(png::Error::BadCrc));
        // Borne de taille respectée.
        if w * h > 1 {
            assert_eq!(png::decode(&data, w * h - 1), Err(png::Error::TooLarge));
        }
        n += 1;
    }
    assert!(n > 0);
}

/// Icônes réelles de Debian (`firefox-esr.png`, `mini.xterm_48x48.xpm`…) copiées dans le
/// dossier des vecteurs sous `real_*` : elles doivent se décoder et avoir des pixels visibles.
#[test]
fn real_icons() {
    let Some(dir) = vectors() else { return };
    for e in std::fs::read_dir(&dir).unwrap().flatten() {
        let name = e.file_name().into_string().unwrap();
        if !name.starts_with("real_") {
            continue;
        }
        let data = std::fs::read(e.path()).unwrap();
        let img = decode(&data, 1 << 22).unwrap_or_else(|| panic!("{name} illisible"));
        assert!(img.pixels.iter().any(|p| p[3] > 0), "{name} vide");
        assert!(img.pixels.iter().all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]), "{name} non prémultiplié");
        let s = img.fit_square(22);
        assert_eq!(s.pixels.len(), 22 * 22);
        println!("{name} : {}x{}", img.width, img.height);
    }
}

#[test]
fn stored_and_fixed_blocks() {
    // Bloc stocké écrit à la main : « hello ».
    let data = [0x78, 0x01, 0x01, 0x05, 0x00, 0xfa, 0xff, b'h', b'e', b'l', b'l', b'o', 0x06, 0x2c, 0x02, 0x15];
    assert_eq!(inflate::zlib_decompress(&data, 100).unwrap(), b"hello");
    // NLEN faux.
    let mut bad = data;
    bad[5] = 0;
    assert_eq!(inflate::zlib_decompress(&bad, 100), Err(inflate::Error::BadStoredLength));
    // Somme de contrôle fausse.
    let mut bad = data;
    bad[15] ^= 1;
    assert_eq!(inflate::zlib_decompress(&bad, 100), Err(inflate::Error::BadChecksum));
    // Type de bloc réservé.
    assert_eq!(inflate::inflate(&[0x07], 100).err(), Some(inflate::Error::BadBlockType));
    assert_eq!(inflate::adler32(b"Wikipedia"), 0x11E6_0398);
}

#[test]
fn distance_before_start() {
    // Bloc fixe : longueur 3 (symbole 257 = 0000001), distance 1 (code 0) dès le début.
    // Bits (poids faible d'abord) : BFINAL 1, BTYPE 01, puis 0000001 (code de 257), 00000.
    let mut bits: Vec<u8> = vec![1, 1, 0];
    bits.extend([0, 0, 0, 0, 0, 0, 1]);
    bits.extend([0, 0, 0, 0, 0]);
    let mut bytes = vec![0u8; (bits.len() + 7) / 8];
    for (i, b) in bits.iter().enumerate() {
        bytes[i / 8] |= b << (i % 8);
    }
    assert_eq!(inflate::inflate(&bytes, 100).err(), Some(inflate::Error::BadDistance));
}

#[test]
fn xpm_basic() {
    let text = r#"/* XPM */
static char *x[] = {
/* commentaire */
"4 2 3 1",
"  c None",
". c #FF0000",
"X c gray50 m black",
" .X.",
"XX  "};"#;
    let img = xpm::decode(text, 100).unwrap();
    assert_eq!((img.width, img.height), (4, 2));
    assert_eq!(img.pixels[0], [0, 0, 0, 0]);
    assert_eq!(img.pixels[1], [255, 0, 0, 255]);
    assert_eq!(img.pixels[2], [128, 128, 128, 255]);
    assert_eq!(decode(text.as_bytes(), 100).unwrap(), img);
    // Pixel absent de la table.
    assert!(xpm::decode(&text.replace("XX  ", "XZ  "), 100).is_err());
}

#[test]
fn scaling() {
    // 4×4 opaque rouge → 2×2 rouge ; damier noir/blanc → gris moyen.
    let red = Image { width: 4, height: 4, pixels: vec![[255, 0, 0, 255]; 16] };
    assert_eq!(red.resized(2, 2).pixels, vec![[255, 0, 0, 255]; 4]);
    let checker = Image {
        width: 2,
        height: 2,
        pixels: vec![[0, 0, 0, 255], [255, 255, 255, 255], [255, 255, 255, 255], [0, 0, 0, 255]],
    };
    assert_eq!(checker.resized(1, 1).pixels, vec![[128, 128, 128, 255]]);
    // Agrandissement : les coins gardent leur couleur.
    let up = checker.resized(8, 8);
    assert_eq!(up.pixels[0], [0, 0, 0, 255]);
    assert_eq!(up.pixels[7], [255, 255, 255, 255]);
    // Rapport 3:2 → largeur pleine, centré verticalement.
    let wide = Image { width: 30, height: 20, pixels: vec![[10, 10, 10, 255]; 600] };
    let sq = wide.fit_square(12);
    assert_eq!(sq.width, 12);
    assert_eq!(sq.pixels[0], [0, 0, 0, 0]);
    assert_eq!(sq.pixels[6 * 12 + 6], [10, 10, 10, 255]);
    // Réduction non entière (48 → 22) : couleur uniforme conservée.
    let big = Image { width: 48, height: 48, pixels: vec![[40, 80, 120, 200]; 48 * 48] };
    assert!(big.resized(22, 22).pixels.iter().all(|p| *p == [40, 80, 120, 200]));
}

#[test]
fn desktop_entries() {
    let text = "# commentaire\n[Desktop Entry]\nType=Application\nName=Firefox ESR\nName[fr]=Firefox ESR (fr)\n\
                Name[de]=Feuerfuchs\nComment=Web\nExec=/usr/lib/firefox-esr/firefox-esr %u\nIcon=firefox-esr\n\
                Terminal=false\nCategories=Network;WebBrowser;\n[Desktop Action new-window]\nName=Nouvelle\nExec=autre\n";
    let e = desktop::parse(text, "fr_FR").unwrap();
    assert_eq!(e.name, "Firefox ESR (fr)");
    assert_eq!(e.exec, "/usr/lib/firefox-esr/firefox-esr %u");
    assert_eq!(e.icon, "firefox-esr");
    assert_eq!(e.categories, vec!["Network", "WebBrowser"]);
    assert!(!e.terminal && !e.hidden);
    assert_eq!(desktop::parse(text, "de_DE").unwrap().name, "Feuerfuchs");
    assert_eq!(desktop::parse("[Desktop Entry]\nName=X\nNoDisplay=true\n", "fr").unwrap().hidden, true);
    assert!(desktop::parse("[Other]\nName=X\n", "fr").is_none());

    let a = desktop::exec_args(r#"foo "a b" c\ d %U %% "x\"y" %c"#, "Nom", "/p").unwrap();
    assert_eq!(a, vec!["foo", "a b", r"c\", "d", "%", "x\"y", "Nom"]);
    assert_eq!(desktop::exec_args("/usr/bin/python3.13", "", "").unwrap(), vec!["/usr/bin/python3.13"]);
    assert!(desktop::exec_args("foo \"open", "", "").is_none());
}

#[test]
fn lookup() {
    let l = lookup_order("firefox-esr", 22);
    assert_eq!(l[0], "/usr/share/icons/hicolor/22x22/apps/firefox-esr.png");
    assert_eq!(l[1], "/usr/share/icons/hicolor/24x24/apps/firefox-esr.png");
    assert!(l.contains(&String::from("/usr/share/pixmaps/firefox-esr_48x48.xpm")));
    assert_eq!(lookup_order("/usr/share/pixmaps/python3.13.xpm", 48), vec!["/usr/share/pixmaps/python3.13.xpm"]);
    assert!(lookup_order("../etc/passwd", 48).is_empty());
    assert!(lookup_order("a/b", 48).is_empty());
}

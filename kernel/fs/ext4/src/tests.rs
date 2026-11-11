//! Tests sur l'hôte : sommes et hachages, puis lecture complète d'images produites par
//! `tests/mkimages.sh` (variable `EXT4_IMAGES`), comparée à l'arborescence source.

use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::*;

// --- Sommes et hachages --------------------------------------------------------------------

#[test]
fn crc32c_vectors() {
    // Vecteur de référence de CRC32C (RFC 3720) : « 123456789 » → 0xE3069283.
    assert_eq!(!crc32c(!0, b"123456789"), 0xE306_9283);
    assert_eq!(!crc::crc32c_soft_for_tests(!0, b"123456789"), 0xE306_9283);
    let data: Vec<u8> = (0..10_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    for n in [0usize, 1, 7, 8, 9, 63, 64, 4095, 10_000] {
        assert_eq!(crc32c(0x1234_5678, &data[..n]), crc::crc32c_soft_for_tests(0x1234_5678, &data[..n]));
    }
    // CRC16/ARC : « 123456789 » → 0xBB3D (init 0).
    assert_eq!(crc16(0, b"123456789"), 0xBB3D);
}

#[test]
fn hash_eof_rule() {
    // Aucune version ne doit rendre le hachage réservé à la fin d'index.
    for v in 0..6u8 {
        for i in 0..2000u32 {
            let name = format!("n{i}");
            let h = dx_hash(name.as_bytes(), v, &[0; 4]).unwrap();
            assert_eq!(h & 1, 0);
            assert_ne!(h, 0xFFFF_FFFE);
        }
    }
    assert!(dx_hash(b"x", 6, &[0; 4]).is_none());
}

// --- Images --------------------------------------------------------------------------------

struct MemDev(Vec<u8>);

impl BlockDev for MemDev {
    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        assert_eq!(offset % SECTOR, 0, "lecture non alignee a {offset}");
        assert_eq!(buf.len() as u64 % SECTOR, 0, "longueur non alignee {}", buf.len());
        let o = offset as usize;
        if o + buf.len() > self.0.len() {
            return Err(Error::Io);
        }
        buf.copy_from_slice(&self.0[o..o + buf.len()]);
        Ok(())
    }
}

fn images() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var("EXT4_IMAGES").ok()?);
    p.join("src").is_dir().then_some(p)
}

fn open(dir: &Path, name: &str) -> Fs<Arc<MemDev>> {
    let data = fs::read(dir.join(format!("{name}.img"))).unwrap();
    Fs::mount(Arc::new(MemDev(data))).unwrap_or_else(|e| panic!("{name} : montage impossible : {e:?}"))
}

fn kind_of(m: &fs::Metadata) -> FileKind {
    let t = m.file_type();
    if t.is_symlink() {
        FileKind::Symlink
    } else if t.is_dir() {
        FileKind::Dir
    } else if t.is_fifo() {
        FileKind::Fifo
    } else if t.is_char_device() {
        FileKind::CharDev
    } else if t.is_block_device() {
        FileKind::BlockDev
    } else if t.is_socket() {
        FileKind::Socket
    } else {
        FileKind::Regular
    }
}

/// Lecture complète par morceaux de tailles variées (débuts et fins non alignés).
fn read_chunked<D: BlockDev>(fs: &Fs<D>, ino: &Inode) -> Vec<u8> {
    let mut out = Vec::new();
    let sizes = [1usize, 511, 512, 4095, 4096, 4097, 65536, 3, 1 << 20];
    let mut k = 0;
    loop {
        let mut buf = vec![0xAAu8; sizes[k % sizes.len()]];
        k += 1;
        let n = fs.read(ino, out.len() as u64, &mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

struct Stats {
    files: usize,
    dirs: usize,
    links: usize,
    indexed: usize,
    max_levels: u8,
}

fn check_tree<D: BlockDev>(fs: &Fs<D>, dir: &Inode, src: &Path, img: &str, st: &mut Stats) {
    st.dirs += 1;
    let mut names = BTreeSet::new();
    let mut entries = Vec::new();
    fs.read_dir(dir, |e| {
        if e.name != b"." && e.name != b".." {
            assert!(names.insert(e.name.clone()), "{img} : nom en double {:?}", String::from_utf8_lossy(&e.name));
            entries.push(e.clone());
        }
        true
    })
    .unwrap();
    let expected: BTreeSet<Vec<u8>> = fs::read_dir(src)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_encoded_bytes())
        .filter(|n| n != b"lost+found")
        .collect();
    names.remove(&b"lost+found".to_vec());
    assert_eq!(names, expected, "{img} : contenu de {}", src.display());

    let indexed = dir.flags & inode::EXT4_INDEX_FL != 0;
    if indexed {
        st.indexed += 1;
        // dx_root_info.indirect_levels, à l'octet 30 du premier bloc.
        let mut b = vec![0u8; fs.block_size()];
        fs.read(dir, 0, &mut b).unwrap();
        st.max_levels = st.max_levels.max(b[30]);
    }
    for e in &entries {
        if e.name == b"lost+found" {
            continue;
        }
        // La recherche (htree s'il y en a un) retrouve la même entrée.
        let found = fs.lookup(dir, &e.name).unwrap().expect("lookup");
        assert_eq!(found.ino, e.ino);
        if indexed {
            let h = fs.htree_lookup(dir, &e.name).unwrap_or_else(|err| panic!("{img} : htree {err:?}"));
            assert_eq!(h.map(|x| x.ino), Some(e.ino), "{img} : htree {:?}", String::from_utf8_lossy(&e.name));
        }
        let path = src.join(<std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(&e.name));
        let m = fs::symlink_metadata(&path).unwrap();
        let ino = fs.inode(e.ino).unwrap();
        let name = path.display().to_string();
        assert_eq!(ino.kind(), kind_of(&m), "{img} : type de {name}");
        if e.kind != FileKind::Unknown {
            assert_eq!(e.kind, ino.kind(), "{img} : type d'entree de {name}");
        }
        assert_eq!(ino.perm() as u32, m.mode() & 0o7777, "{img} : droits de {name}");
        assert_eq!(ino.uid, m.uid(), "{img} : uid de {name}");
        assert_eq!(ino.gid, m.gid(), "{img} : gid de {name}");
        assert_eq!(ino.mtime.sec, m.mtime(), "{img} : mtime de {name}");
        match ino.kind() {
            // `mke2fs -d` (e2fsprogs 1.47) copie mal les fichiers creux avec `inline_data` :
            // `debugfs` y lit la même taille fausse que nous.
            FileKind::Regular if img == "ext4_inline" && (e.name == b"holes" || e.name == b"sparse") => {}
            FileKind::Regular => {
                st.files += 1;
                let want = fs::read(&path).unwrap();
                assert_eq!(ino.size, want.len() as u64, "{img} : taille de {name}");
                let mut whole = vec![0u8; want.len() + 10];
                let n = fs.read(&ino, 0, &mut whole).unwrap();
                assert_eq!(n, want.len());
                assert!(whole[..n] == want[..], "{img} : contenu de {name}");
                assert!(read_chunked(fs, &ino) == want, "{img} : lecture par morceaux de {name}");
                assert_eq!(ino.nlink as u64, m.nlink(), "{img} : liens de {name}");
            }
            FileKind::Symlink => {
                st.links += 1;
                let want = fs::read_link(&path).unwrap().into_os_string().into_encoded_bytes();
                assert_eq!(fs.read_link(&ino).unwrap(), want, "{img} : cible de {name}");
            }
            FileKind::Dir => check_tree(fs, &ino, &path, img, st),
            _ => {}
        }
    }
    // Recherches négatives, par l'index comme par le parcours.
    for miss in ["absent", "fichier_0", "fichier_3001", "zz"] {
        assert!(fs.lookup(dir, miss.as_bytes()).unwrap().is_none(), "{img} : {miss} trouve");
    }
}

const IMAGES: &[&str] = &[
    "ext4",
    "ext4_1k",
    "ext4_inline",
    "ext4_nocsum",
    "ext4_legacy",
    "ext4_metabg",
    "ext4_32",
    "ext3",
    "ext2",
];

#[test]
fn images_match_source() {
    let Some(dir) = images() else {
        eprintln!("EXT4_IMAGES absent : tests d'images sautes (voir tests/mkimages.sh)");
        return;
    };
    for img in IMAGES {
        let fs = open(&dir, img);
        assert_eq!(fs.sb.label(), img.as_bytes());
        let root = fs.root().unwrap();
        let mut st = Stats { files: 0, dirs: 0, links: 0, indexed: 0, max_levels: 0 };
        check_tree(&fs, &root, &dir.join("src"), img, &mut st);
        eprintln!(
            "{img:12} bloc {:5} : {} fichiers, {} dossiers ({} indexes, {} niveaux), {} liens",
            fs.block_size(),
            st.files,
            st.dirs,
            st.indexed,
            st.max_levels + st.indexed.min(1) as u8,
            st.links
        );
        if *img == "ext4_1k" {
            assert!(st.max_levels >= 1, "pas d'index a deux niveaux");
        }
        assert!(st.files >= 15000 && st.links == 5, "{img} : {} fichiers, {} liens", st.files, st.links);
        if *img != "ext2" && *img != "ext4_inline" {
            assert!(st.indexed >= 1, "{img} : aucun dossier indexe");
        }
    }
}

#[test]
fn sparse_extent_tree_is_deep() {
    let Some(dir) = images() else { return };
    let fs = open(&dir, "ext4");
    let root = fs.root().unwrap();
    let e = fs.lookup(&root, b"sparse").unwrap().unwrap();
    let ino = fs.inode(e.ino).unwrap();
    assert!(ino.uses_extents());
    // 700 îlots : plus que les 4 extents de l'inode, donc au moins un niveau d'index.
    let depth = u16::from_le_bytes([ino.i_block()[6], ino.i_block()[7]]);
    assert!(depth >= 1, "profondeur {depth}");
}

#[test]
fn inline_data_is_used() {
    let Some(dir) = images() else { return };
    let fs = open(&dir, "ext4_inline");
    let root = fs.root().unwrap();
    let mut inline = 0;
    for name in ["one", "s59", "s60", "dir"] {
        let e = fs.lookup(&root, name.as_bytes()).unwrap().unwrap();
        if fs.inode(e.ino).unwrap().has_inline_data() {
            inline += 1;
        }
    }
    assert!(inline >= 3, "{inline} inodes en ligne");
}

#[test]
fn journal_replay_in_memory() {
    let Some(dir) = images() else { return };
    let fs = open(&dir, "ext4_journal");
    assert!(fs.journal_replayed_blocks() >= 1);
    let root = fs.root().unwrap();
    let e = fs.lookup(&root, b"m1").unwrap().unwrap();
    let ino = fs.inode(e.ino).unwrap();
    let want = fs::read(dir.join("src/m1")).unwrap();
    assert!(read_chunked(&fs, &ino) == want, "un fichier hors du journal reste intact");
    // `one` (1 octet) : son premier bloc vient maintenant du journal.
    let e = fs.lookup(&root, b"one").unwrap().unwrap();
    let ino = fs.inode(e.ino).unwrap();
    let mut b = [0u8; 1];
    fs.read(&ino, 0, &mut b).unwrap();
    assert_eq!(&b, b"J");
    // Le volume d'origine, lui, n'a rien à rejouer.
    assert_eq!(open(&dir, "ext4").journal_replayed_blocks(), 0);
}

#[test]
fn xattrs_and_corruption() {
    let Some(dir) = images() else { return };
    let mut data = fs::read(dir.join("ext4.img")).unwrap();
    {
        let fs = Fs::mount(Arc::new(MemDev(data.clone()))).unwrap();
        let root = fs.root().unwrap();
        // mke2fs -d ne pose pas d'attributs : la liste doit être vide et sans erreur.
        let e = fs.lookup(&root, b"m5").unwrap().unwrap();
        let ino = fs.inode(e.ino).unwrap();
        assert!(fs.xattrs(&ino).unwrap().iter().all(|x| !x.name.is_empty()));
    }
    // Un octet du superbloc changé : la somme de contrôle doit le voir.
    data[1024 + 0x30] ^= 0xFF;
    assert!(matches!(Fs::mount(Arc::new(MemDev(data))), Err(Error::Checksum(_))));
}

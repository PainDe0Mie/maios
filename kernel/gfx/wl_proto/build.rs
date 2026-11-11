//! Générateur des signatures Wayland (l'équivalent de `wayland-scanner`, sans dépendance).
//!
//! Lit chaque fichier de `protocols/*.xml` et produit `$OUT_DIR/protocols.rs` : un module par
//! interface (`wl_surface`, `xdg_toplevel`…) avec son descripteur `INTERFACE`, les numéros des
//! requêtes (`req::ATTACH`) et des événements (`evt::ENTER`), et les valeurs des énumérations
//! (`error::INVALID_SCALE`). Les tables sont des `static` nommées, référencées entre elles.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;
use std::{env, fs};

/// Plus grand nombre d'arguments d'un message (borne du décodeur, `wire::MAX_ARGS`).
const MAX_ARGS: usize = 16;

// ---------------------------------------------------------------------------------------------
// XML minimal : éléments, attributs entre guillemets, commentaires, prologue ; texte ignoré.
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Elem {
    name: String,
    attrs: Vec<(String, String)>,
    children: Vec<Elem>,
}

impl Elem {
    fn attr(&self, k: &str) -> Option<&str> {
        self.attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
    }
    fn req_attr(&self, k: &str, file: &str) -> String {
        self.attr(k).unwrap_or_else(|| panic!("{file} : <{}> sans attribut {k}", self.name)).to_string()
    }
    fn kids<'a>(&'a self, n: &'a str) -> impl Iterator<Item = &'a Elem> + 'a {
        self.children.iter().filter(move |c| c.name == n)
    }
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

fn parse_xml(src: &str, file: &str) -> Elem {
    let b = src.as_bytes();
    let mut i = 0;
    let mut stack: Vec<Elem> = vec![Elem { name: String::from("#document"), ..Default::default() }];
    let skip_to = |i: &mut usize, pat: &str| {
        let rest = &src[*i..];
        let p = rest.find(pat).unwrap_or_else(|| panic!("{file} : {pat} manquant"));
        *i += p + pat.len();
    };
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        let rest = &src[i..];
        if rest.starts_with("<?") {
            skip_to(&mut i, "?>");
        } else if rest.starts_with("<!--") {
            skip_to(&mut i, "-->");
        } else if rest.starts_with("<![CDATA[") {
            skip_to(&mut i, "]]>");
        } else if rest.starts_with("<!") {
            skip_to(&mut i, ">");
        } else if rest.starts_with("</") {
            let end = rest.find('>').unwrap_or_else(|| panic!("{file} : balise fermante non terminée"));
            let name = rest[2..end].trim();
            let e = stack.pop().unwrap();
            assert_eq!(e.name, name, "{file} : </{name}> ferme <{}>", e.name);
            stack.last_mut().unwrap_or_else(|| panic!("{file} : </{name}> en trop")).children.push(e);
            i += end + 1;
        } else {
            // Balise ouvrante : nom, attributs, puis `>` ou `/>`.
            i += 1;
            let start = i;
            while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' && b[i] != b'/' {
                i += 1;
            }
            let mut e = Elem { name: src[start..i].to_string(), ..Default::default() };
            let self_closing;
            loop {
                while i < b.len() && b[i].is_ascii_whitespace() {
                    i += 1;
                }
                assert!(i < b.len(), "{file} : <{}> non terminée", e.name);
                if b[i] == b'>' {
                    self_closing = false;
                    i += 1;
                    break;
                }
                if b[i] == b'/' {
                    assert_eq!(b.get(i + 1), Some(&b'>'), "{file} : `/` hors de `/>`");
                    self_closing = true;
                    i += 2;
                    break;
                }
                let ks = i;
                while i < b.len() && b[i] != b'=' && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
                let k = src[ks..i].to_string();
                while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'=') {
                    i += 1;
                }
                let q = b[i];
                assert!(q == b'"' || q == b'\'', "{file} : valeur de {k} sans guillemets");
                i += 1;
                let vs = i;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
                e.attrs.push((k, unescape(&src[vs..i])));
                i += 1;
            }
            if self_closing {
                stack.last_mut().unwrap().children.push(e);
            } else {
                stack.push(e);
            }
        }
    }
    assert_eq!(stack.len(), 1, "{file} : éléments non fermés");
    let mut doc = stack.pop().unwrap();
    assert_eq!(doc.children.len(), 1, "{file} : un seul élément racine attendu");
    doc.children.pop().unwrap()
}

// ---------------------------------------------------------------------------------------------
// Génération.
// ---------------------------------------------------------------------------------------------

const KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "dyn", "else", "enum", "extern", "false", "fn", "for", "if",
    "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "static", "struct", "trait",
    "true", "type", "unsafe", "use", "where", "while", "abstract", "become", "box", "do", "final", "macro", "override",
    "priv", "typeof", "unsized", "virtual", "yield", "try",
];

/// Identifiant de module (minuscules) sûr.
fn module_ident(s: &str) -> String {
    if KEYWORDS.contains(&s) {
        format!("r#{s}")
    } else {
        s.to_string()
    }
}

/// Constante (majuscules) ; préfixe `_` si le nom commence par un chiffre (`wl_output.transform`).
fn const_ident(s: &str) -> String {
    let u = s.to_ascii_uppercase();
    if u.starts_with(|c: char| c.is_ascii_digit()) {
        format!("_{u}")
    } else {
        u
    }
}

fn kind_of(ty: &str, file: &str) -> &'static str {
    match ty {
        "int" => "Int",
        "uint" => "Uint",
        "fixed" => "Fixed",
        "string" => "String",
        "object" => "Object",
        "new_id" => "NewId",
        "array" => "Array",
        "fd" => "Fd",
        _ => panic!("{file} : type d'argument inconnu {ty}"),
    }
}

fn gen_messages(
    out: &mut String,
    file: &str,
    iface: &Elem,
    tag: &str,
    prefix: &str,
    known: &BTreeSet<String>,
) -> Vec<String> {
    let mut names = Vec::new();
    let mut descs = String::new();
    for (i, m) in iface.kids(tag).enumerate() {
        let name = m.req_attr("name", file);
        let since: u32 = m.attr("since").map_or(1, |s| s.parse().expect("since"));
        let destructor = m.attr("type") == Some("destructor");
        let args: Vec<&Elem> = m.kids("arg").collect();
        assert!(args.len() <= MAX_ARGS, "{file} : {name} a trop d'arguments");
        let _ = writeln!(out, "    static {prefix}_{i}: [ArgDesc; {}] = [", args.len());
        for a in &args {
            let an = a.req_attr("name", file);
            let kind = kind_of(&a.req_attr("type", file), file);
            let nullable = a.attr("allow-null") == Some("true");
            let (iref, iname) = match a.attr("interface") {
                Some(n) if known.contains(n) => (format!("Some(&super::{}::INTERFACE)", module_ident(n)), format!("Some({n:?})")),
                Some(n) => (String::from("None"), format!("Some({n:?})")),
                None => (String::from("None"), String::from("None")),
            };
            let _ = writeln!(
                out,
                "        ArgDesc {{ name: {an:?}, kind: ArgKind::{kind}, nullable: {nullable}, interface: {iref}, interface_name: {iname} }},"
            );
        }
        let _ = writeln!(out, "    ];");
        let _ = writeln!(
            descs,
            "        MessageDesc {{ name: {name:?}, since: {since}, destructor: {destructor}, args: &{prefix}_{i} }},"
        );
        names.push(name);
    }
    let n = names.len();
    let _ = writeln!(out, "    static {prefix}: [MessageDesc; {n}] = [\n{descs}    ];");
    names
}

fn gen_interface(out: &mut String, file: &str, iface: &Elem, known: &BTreeSet<String>) {
    let name = iface.req_attr("name", file);
    let version: u32 = iface.req_attr("version", file).parse().expect("version");
    let _ = writeln!(out, "/// Interface `{name}` (version {version}, `{file}`).");
    let _ = writeln!(out, "#[allow(non_upper_case_globals, dead_code)]");
    let _ = writeln!(out, "pub mod {} {{", module_ident(&name));
    let _ = writeln!(out, "    use crate::types::*;");
    let _ = writeln!(out, "    pub const NAME: &str = {name:?};");
    let _ = writeln!(out, "    pub const VERSION: u32 = {version};");
    let reqs = gen_messages(out, file, iface, "request", "REQ", known);
    let evts = gen_messages(out, file, iface, "event", "EVT", known);
    let _ = writeln!(
        out,
        "    pub static INTERFACE: Interface = Interface {{ name: NAME, version: VERSION, requests: &REQ, events: &EVT }};"
    );
    for (tag, list) in [("req", &reqs), ("evt", &evts)] {
        let _ = writeln!(out, "    /// Numéros des {}.", if tag == "req" { "requêtes" } else { "événements" });
        let _ = writeln!(out, "    pub mod {tag} {{");
        for (i, n) in list.iter().enumerate() {
            let _ = writeln!(out, "        pub const {}: u16 = {i};", const_ident(n));
        }
        let _ = writeln!(out, "    }}");
    }
    for e in iface.kids("enum") {
        let en = e.req_attr("name", file);
        assert!(en != "req" && en != "evt", "{file} : énumération {en} en conflit avec un module généré");
        let _ = writeln!(out, "    /// Énumération `{name}.{en}`{}.", if e.attr("bitfield") == Some("true") { " (champ de bits)" } else { "" });
        let _ = writeln!(out, "    pub mod {} {{", module_ident(&en));
        for entry in e.kids("entry") {
            let n = entry.req_attr("name", file);
            let v = entry.req_attr("value", file);
            let _ = writeln!(out, "        pub const {}: u32 = {v};", const_ident(&n));
        }
        let _ = writeln!(out, "    }}");
    }
    let _ = writeln!(out, "}}\n");
}

fn main() {
    let dir = Path::new("protocols");
    println!("cargo:rerun-if-changed=protocols");
    let mut files: Vec<_> = fs::read_dir(dir)
        .expect("protocols/")
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "xml"))
        .collect();
    files.sort();
    let mut docs = Vec::new();
    for p in &files {
        println!("cargo:rerun-if-changed={}", p.display());
        let src = fs::read_to_string(p).unwrap();
        let file = p.file_name().unwrap().to_string_lossy().to_string();
        let root = parse_xml(&src, &file);
        assert_eq!(root.name, "protocol", "{file} : racine <protocol> attendue");
        docs.push((file, root));
    }
    let mut known = BTreeSet::new();
    for (file, root) in &docs {
        for i in root.kids("interface") {
            assert!(known.insert(i.req_attr("name", file)), "{file} : interface en double");
        }
    }
    let mut out = String::from("// Généré par build.rs depuis protocols/*.xml : ne pas modifier.\n\n");
    for (file, root) in &docs {
        for i in root.kids("interface") {
            gen_interface(&mut out, file, i, &known);
        }
    }
    let _ = writeln!(out, "/// Toutes les interfaces connues.");
    let _ = writeln!(out, "pub static ALL: [&crate::types::Interface; {}] = [", known.len());
    for n in &known {
        let _ = writeln!(out, "    &{}::INTERFACE,", module_ident(n));
    }
    let _ = writeln!(out, "];");
    let _ = writeln!(out, "/// Plus grand nombre d'arguments d'un message.\npub const MAX_ARGS: usize = {MAX_ARGS};");
    let dest = Path::new(&env::var("OUT_DIR").unwrap()).join("protocols.rs");
    fs::write(dest, out).unwrap();
}

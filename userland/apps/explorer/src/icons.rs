//! Icônes des applications : vraies icônes du thème (`hicolor`, `pixmaps`, PNG et XPM décodés
//! par `mai_icons`), sinon une tuile dessinée (dégradé et pictogramme). Tout est rendu à la
//! taille demandée une seule fois puis gardé (les échecs aussi).

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use framebuffer::AlphaPixel;
use mai_icons::Image;

use crate::catalog::{self, App};
use crate::paint::{self, Canvas, Glyph, Rgb, WHITE};

/// Plus grande icône décodée (pixels) : un PNG de 512 × 512.
const MAX_PIXELS: usize = 512 * 512;
/// Plus gros fichier d'icône lu.
const MAX_FILE: usize = 2 * 1024 * 1024;
/// Vignettes des images du bureau : fichier et image décodée au plus.
const THUMB_FILE: usize = 8 * 1024 * 1024;
const THUMB_PIXELS: usize = 2048 * 2048;

/// Vignette d'une image : l'image à `size` pixels (centrée, marges transparentes) et la taille
/// de sa partie visible.
pub struct Thumb {
    pub img: Arc<Image>,
    pub w: usize,
    pub h: usize,
}

#[derive(Default)]
pub struct Icons {
    themed: BTreeMap<(String, usize), Option<Arc<Image>>>,
    tiles: BTreeMap<(u32, char, u8, usize), Arc<Image>>,
    /// Vignettes : (chemin, taille du fichier, côté) → vignette (les échecs aussi).
    thumbs: BTreeMap<(String, usize, usize), Option<Arc<Thumb>>>,
}

impl Icons {
    pub fn new() -> Self {
        Self::default()
    }

    /// Icône `name` du thème (ou fichier, si `name` est un chemin absolu) à `size` pixels.
    pub fn themed(&mut self, name: &str, size: usize) -> Option<Arc<Image>> {
        let key = (name.to_string(), size);
        if let Some(v) = self.themed.get(&key) {
            return v.clone();
        }
        let img = mai_icons::lookup_order(name, size).iter().find_map(|path| {
            let data = catalog::read_file(path, MAX_FILE)?;
            let img = mai_icons::decode(&data, MAX_PIXELS)?;
            Some(Arc::new(img.fit_square(size)))
        });
        if img.is_none() {
            info!("[explorer] icône '{name}' introuvable");
        }
        self.themed.insert(key, img.clone());
        img
    }

    /// Tuile dessinée : dégradé de `color`, pictogramme `glyph`.
    pub fn tile(&mut self, color: Rgb, glyph: Glyph, size: usize) -> Arc<Image> {
        let (g, ch) = match glyph {
            Glyph::Terminal => (0, ' '),
            Glyph::Monitor => (1, ' '),
            Glyph::Folder => (2, ' '),
            Glyph::Globe => (3, ' '),
            Glyph::Letter(c) => (4, c),
        };
        let key = ((color.r as u32) << 16 | (color.g as u32) << 8 | color.b as u32, ch, g, size);
        if let Some(t) = self.tiles.get(&key) {
            return t.clone();
        }
        // Les pictogrammes sont tracés pour une tuile de 48 pixels.
        const T: usize = 48;
        let mut buf: Vec<AlphaPixel> = alloc::vec![paint::CLEAR; T * T];
        {
            let mut c = Canvas::new(&mut buf, T, T);
            let t = T as isize;
            c.rrect_grad(1, 1, t - 2, t - 2, 13, color.lighten(70), color.darken(70), 255);
            c.rrect_outline(1, 1, t - 2, t - 2, 13, WHITE, 45);
            c.glyph(glyph, 0, 0, t, color);
        }
        let img = paint::to_image(&buf, T, T);
        let img = Arc::new(if size == T { img } else { img.fit_square(size) });
        self.tiles.insert(key, img.clone());
        img
    }

    /// Vignette de l'image `path` (PNG ou XPM) de `len` octets, à `size` pixels ; `None` si
    /// elle ne se décode pas (autre format, trop grande).
    pub fn thumb(&mut self, path: &str, len: usize, size: usize) -> Option<Arc<Thumb>> {
        let key = (path.to_string(), len, size);
        if let Some(t) = self.thumbs.get(&key) {
            return t.clone();
        }
        let t = (len <= THUMB_FILE)
            .then(|| catalog::read_file(path, THUMB_FILE))
            .flatten()
            .and_then(|data| mai_icons::decode(&data, THUMB_PIXELS))
            .filter(|img| img.width > 0 && img.height > 0)
            .map(|img| {
                let m = img.width.max(img.height);
                let (w, h) = ((img.width * size / m).max(1), (img.height * size / m).max(1));
                Arc::new(Thumb { img: Arc::new(img.fit_square(size)), w, h })
            });
        self.thumbs.insert(key, t.clone());
        t
    }

    /// Oublie les vignettes des fichiers qui ne sont plus dans `paths`.
    pub fn keep_thumbs(&mut self, paths: &[&str]) {
        self.thumbs.retain(|(p, _, _), _| paths.contains(&p.as_str()));
    }

    /// Icône d'une appli du catalogue.
    pub fn app(&mut self, app: &App, size: usize) -> Arc<Image> {
        if let Some(name) = app.icon.clone() {
            if let Some(img) = self.themed(&name, size) {
                return img;
            }
        }
        self.tile(app.color, app.glyph, size)
    }

    /// Icône d'une fenêtre : son appli si elle en a une, sinon l'icône du thème qui porte son
    /// `app_id`, sinon une tuile à l'initiale de son titre.
    pub fn window(&mut self, app: Option<&App>, app_id: &str, title: &str, size: usize) -> Arc<Image> {
        if let Some(a) = app {
            return self.app(a, size);
        }
        if !app_id.is_empty() && !app_id.contains('/') {
            if let Some(img) = self.themed(&app_id.to_lowercase(), size) {
                return img;
            }
        }
        let label = if title.is_empty() { app_id } else { title };
        let letter = label.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_ascii_uppercase()).unwrap_or('?');
        self.tile(catalog::color_for(if app_id.is_empty() { title } else { app_id }), Glyph::Letter(letter), size)
    }
}

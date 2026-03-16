#![no_std]

extern crate alloc;
extern crate color;
extern crate shapes;
extern crate framebuffer;
extern crate framebuffer_drawer;
#[macro_use]
extern crate log;
extern crate spin;

use alloc::boxed::Box;
use alloc::vec::Vec;
use color::Color;
use shapes::{Coord, Rectangle};
use framebuffer::{Framebuffer, AlphaPixel};
use spin::{Mutex, Once};

/// L'instance globale de MGI.
pub static MGI: Once<Mutex<Mgi>> = Once::new();

/// Initialise le sous-système graphique MGI.
pub fn init() -> Result<(), &'static str> {
    let fb = framebuffer::init::<AlphaPixel>()?;
    let provider = Box::new(SoftwareGraphicsProvider::new(fb)?);
    let mgi = Mgi::new(provider);
    MGI.call_once(|| Mutex::new(mgi));
    Ok(())
}

/// MGI: Mai Graphics Infrastructure
/// Le point d'entrée principal pour toutes les opérations graphiques du kernel.
pub struct Mgi {
    /// Le pilote graphique sous-jacent (peut être logiciel ou matériel).
    provider: Box<dyn GraphicsProvider + Send>,
}

impl Mgi {
    /// Initialise MGI avec un pilote spécifique.
    pub fn new(provider: Box<dyn GraphicsProvider + Send>) -> Self {
        Self { provider }
    }

    /// Soumet une liste de commandes de dessin à exécuter.
    pub fn submit<'a>(&mut self, commands: &[DrawCommand<'a>]) {
        self.provider.submit(commands);
    }

    /// Affiche le résultat final à l'écran (V-Sync / Swap Buffers).
    pub fn present(&mut self) {
        self.provider.present();
    }
    
    /// Retourne la résolution de l'écran (largeur, hauteur).
    pub fn resolution(&self) -> (usize, usize) {
        self.provider.resolution()
    }
}

/// Les commandes de dessin primitives supportées par MGI.
/// C'est ce que les fenêtres et le WM vont générer.
#[derive(Debug, Clone)]
pub enum DrawCommand<'a> {
    /// Remplir un rectangle avec une couleur unie.
    FillRect {
        rect: Rectangle,
        color: Color,
    },
    /// Dessiner le contour d'un rectangle.
    DrawRect {
        rect: Rectangle,
        color: Color,
    },
    /// Dessiner une ligne.
    DrawLine {
        start: Coord,
        end: Coord,
        color: Color,
    },
    /// Dessiner un cercle.
    DrawCircle {
        center: Coord,
        radius: usize,
        color: Color,
    },
    /// Copier une image (Blit) vers l'écran.
    Blit {
        src: &'a [AlphaPixel],
        src_width: usize,
        dest_top_left: Coord,
    },

    BlitRegion {
        src: &'a [AlphaPixel],
        src_width: usize,
        dest_top_left: Coord,
        region: Rectangle, // zone à copier (coords écran absolues)
    },
}

/// Le Trait que tous les pilotes graphiques (Software, VirtIO, Intel, Nvidia...) devront implémenter.
pub trait GraphicsProvider {
    fn submit<'a>(&mut self, commands: &[DrawCommand<'a>]);
    fn present(&mut self);
    fn resolution(&self) -> (usize, usize);
}

/// Une implémentation logicielle (CPU) du fournisseur graphique.
/// Elle utilise un "Double Buffering" strict : on dessine tout en RAM (backbuffer)
/// et on copie vers la VRAM (frontbuffer) uniquement lors du `present()`.
pub struct SoftwareGraphicsProvider {
    backbuffer: Framebuffer<AlphaPixel>,
    frontbuffer: Framebuffer<AlphaPixel>,
    dirty_rects: Vec<Rectangle>,
}

impl SoftwareGraphicsProvider {
    /// Crée un nouveau pilote logiciel en utilisant le framebuffer UEFI/VESA existant.
    pub fn new(frontbuffer: Framebuffer<AlphaPixel>) -> Result<Self, &'static str> {
        let (width, height) = frontbuffer.get_size();
        // On alloue le backbuffer en RAM avec la même taille que l'écran.
        let backbuffer = Framebuffer::new(width, height, None)?;
        
        Ok(Self {
            backbuffer,
            frontbuffer,
            dirty_rects: Vec::with_capacity(32),
        })
    }

    /// Marque une zone comme "sale" (à redessiner).
    fn mark_dirty(&mut self, rect: Rectangle) {
        // Au lieu de fusionner immédiatement et de créer un rectangle géant inutile,
        // on ajoute simplement le rectangle à la liste.
        // Le `present` décidera s'il faut fusionner ou traiter séparément.
        self.dirty_rects.push(rect);
    }

    pub fn draw_cursor_direct(&mut self, pos: Coord, pixel: AlphaPixel) {
        // Dessine le curseur DIRECTEMENT sur le frontbuffer sans passer par le backbuffer
        // → pas de dirty rect, pas de blit
        self.frontbuffer.overwrite_pixel(pos, pixel);
        // (dessiner un petit carré 8x8 pareil)
        for dy in 0..12isize {
            for dx in 0..8isize {
                self.frontbuffer.overwrite_pixel(pos + (dx, dy), pixel);
            }
        }
    }
}

impl GraphicsProvider for SoftwareGraphicsProvider {
    fn submit<'a>(&mut self, commands: &[DrawCommand<'a>]) {
        for cmd in commands {
            match cmd {
                DrawCommand::FillRect { rect, color } => {
                    framebuffer_drawer::fill_rectangle(
                        &mut self.backbuffer,
                        rect.top_left,
                        rect.width(),
                        rect.height(),
                        (*color).into(),
                    );
                    self.mark_dirty(*rect);
                }
                DrawCommand::DrawRect { rect, color } => {
                    framebuffer_drawer::draw_rectangle(
                        &mut self.backbuffer,
                        rect.top_left,
                        rect.width(),
                        rect.height(),
                        (*color).into(),
                    );
                    self.mark_dirty(*rect);
                }
                DrawCommand::DrawLine { start, end, color } => {
                    framebuffer_drawer::draw_line(
                        &mut self.backbuffer,
                        *start,
                        *end,
                        (*color).into(),
                    );
                    // Approximation du rectangle sale pour une ligne
                    let min_x = core::cmp::min(start.x, end.x);
                    let min_y = core::cmp::min(start.y, end.y);
                    let max_x = core::cmp::max(start.x, end.x) + 1;
                    let max_y = core::cmp::max(start.y, end.y) + 1;
                    self.mark_dirty(Rectangle { top_left: Coord::new(min_x, min_y), bottom_right: Coord::new(max_x, max_y) });
                }
                DrawCommand::DrawCircle { center, radius, color } => {
                    framebuffer_drawer::draw_circle(
                        &mut self.backbuffer,
                        *center,
                        *radius,
                        (*color).into(),
                    );
                    let r = *radius as isize;
                    self.mark_dirty(Rectangle { 
                        top_left: *center - (r, r), 
                        bottom_right: *center + (r + 1, r + 1) 
                    });
                }
                DrawCommand::Blit { src, src_width, dest_top_left } => {
                    let (fb_width, fb_height) = self.backbuffer.get_size();
                    let src_height = src.len() / src_width;
                    
                    for i in 0..src_height {
                        let y = dest_top_left.y + i as isize;
                        if y >= 0 && y < fb_height as isize {
                            let start_x = dest_top_left.x;
                            let x_start = core::cmp::max(0, start_x);
                            let x_end = core::cmp::min(fb_width as isize, start_x + *src_width as isize);
                            
                            if x_start < x_end {
                                let src_offset = (x_start - start_x) as usize;
                                let len = (x_end - x_start) as usize;
                                let dest_idx = (y as usize) * fb_width + x_start as usize;
                                let row_src = &src[i * src_width .. (i + 1) * src_width];
                                self.backbuffer.composite_buffer(&row_src[src_offset .. src_offset + len], dest_idx);
                            }
                        }
                    }
                    self.mark_dirty(Rectangle {
                        top_left: *dest_top_left,
                        bottom_right: *dest_top_left + (*src_width as isize, src_height as isize),
                    });
                }
                DrawCommand::BlitRegion { src, src_width, dest_top_left, region } => {
                    let (fb_width, fb_height) = self.backbuffer.get_size();
                    let src_height = src.len() / src_width;

                    // Clipper la région aux limites de l'écran ET du src
                    let start_y = core::cmp::max(region.top_left.y, dest_top_left.y);
                    let end_y   = core::cmp::min(region.bottom_right.y, dest_top_left.y + src_height as isize);
                    let start_y = core::cmp::max(0, start_y) as usize;
                    let end_y   = core::cmp::min(fb_height as isize, end_y) as usize;

                    let start_x = core::cmp::max(region.top_left.x, dest_top_left.x);
                    let end_x   = core::cmp::min(region.bottom_right.x, dest_top_left.x + *src_width as isize);
                    let start_x = core::cmp::max(0, start_x) as usize;
                    let end_x   = core::cmp::min(fb_width as isize, end_x as isize) as usize;

                    if start_x >= end_x || start_y >= end_y { continue; } // 'continue' dans le match → utilise return ou skip

                    for y in start_y..end_y {
                        let row_in_src = y as isize - dest_top_left.y;
                        if row_in_src < 0 || row_in_src >= src_height as isize { continue; }
                        let src_offset_x = start_x as isize - dest_top_left.x;
                        if src_offset_x < 0 { continue; }
                        let row_src = &src[row_in_src as usize * src_width .. (row_in_src as usize + 1) * src_width];
                        let len = end_x - start_x;
                        let dest_idx = y * fb_width + start_x;
                        self.backbuffer.composite_buffer(&row_src[src_offset_x as usize .. src_offset_x as usize + len], dest_idx);
                    }
                    self.mark_dirty(*region);
                }
            }
        }
    }

    fn present(&mut self) {
        info!("MGI present: {} dirty rects", self.dirty_rects.len());
        if self.dirty_rects.is_empty() {
            return;
        }

        let (fb_width, fb_height) = self.frontbuffer.get_size();

        // --- FUSION DES DIRTY RECTS ---
        // Si on a trop de rects, on les fusionne intelligemment par paires
        // au lieu de faire une bounding box géante de tout l'écran.
        if self.dirty_rects.len() > 8 {
            // Trier par position Y pour faciliter la fusion de rects proches
            self.dirty_rects.sort_by_key(|r| r.top_left.y);

            let mut merged: Vec<Rectangle> = Vec::with_capacity(8);
            for rect in self.dirty_rects.drain(..) {
                // Chercher un rect existant à fusionner avec celui-ci
                let mut found = false;
                for m in merged.iter_mut() {
                    // Fusionner si ils se chevauchent ou sont adjacents (marge de 4px)
                    let gap = 4isize;
                    let overlaps = rect.top_left.x <= m.bottom_right.x + gap
                        && rect.bottom_right.x >= m.top_left.x - gap
                        && rect.top_left.y <= m.bottom_right.y + gap
                        && rect.bottom_right.y >= m.top_left.y - gap;

                    if overlaps {
                        // Agrandir le rect existant pour englober les deux
                        m.top_left.x = core::cmp::min(m.top_left.x, rect.top_left.x);
                        m.top_left.y = core::cmp::min(m.top_left.y, rect.top_left.y);
                        m.bottom_right.x = core::cmp::max(m.bottom_right.x, rect.bottom_right.x);
                        m.bottom_right.y = core::cmp::max(m.bottom_right.y, rect.bottom_right.y);
                        found = true;
                        break;
                    }
                }
                if !found {
                    // Pas de voisin trouvé → ajouter tel quel
                    // Si on a déjà trop de rects, fusionner avec le dernier quand même
                    if merged.len() >= 8 {
                        if let Some(last) = merged.last_mut() {
                            last.top_left.x = core::cmp::min(last.top_left.x, rect.top_left.x);
                            last.top_left.y = core::cmp::min(last.top_left.y, rect.top_left.y);
                            last.bottom_right.x = core::cmp::max(last.bottom_right.x, rect.bottom_right.x);
                            last.bottom_right.y = core::cmp::max(last.bottom_right.y, rect.bottom_right.y);
                        }
                    } else {
                        merged.push(rect);
                    }
                }
            }
            self.dirty_rects = merged;
        }

        // --- BLIT VERS LE FRONTBUFFER (VRAM) ---
        // On copie uniquement les zones marquées comme "sales".
        // On accède directement aux slices pour éviter les emprunts multiples.
        let back_ptr = self.backbuffer.buffer().as_ptr();
        let front_ptr = self.frontbuffer.buffer_mut().as_mut_ptr();
        let total_pixels = fb_width * fb_height;

        for rect in self.dirty_rects.drain(..) {
            // Clipper strictement aux dimensions de l'écran
            let start_x = core::cmp::max(0isize, rect.top_left.x) as usize;
            let start_y = core::cmp::max(0isize, rect.top_left.y) as usize;
            let end_x   = core::cmp::min(fb_width as isize, rect.bottom_right.x) as usize;
            let end_y   = core::cmp::min(fb_height as isize, rect.bottom_right.y) as usize;

            if start_x >= end_x || start_y >= end_y {
                continue;
            }

            let copy_width = end_x - start_x;

            // Copie ligne par ligne — uniquement la zone concernée
            for y in start_y..end_y {
                let offset = y * fb_width + start_x;

                // Vérification de bornes explicite pour éviter les panics en cas de bug
                if offset + copy_width > total_pixels {
                    break;
                }

                // SAFETY: Les offsets sont clippés et vérifiés ci-dessus.
                // On utilise copy_nonoverlapping car backbuffer et frontbuffer
                // sont deux allocations distinctes → jamais d'overlap.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        back_ptr.add(offset),
                        front_ptr.add(offset),
                        copy_width,
                    );
                }
            }
        }
    }

    fn resolution(&self) -> (usize, usize) {
        self.frontbuffer.get_size()
    }
}

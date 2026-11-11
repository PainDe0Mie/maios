//! Scanout de virtio-gpu, vu par MGI comme une [`mgi::Scanout`].
//!
//! * **Tampon** : `width × height` pixels B8G8R8X8 dans des frames quelconques du noyau
//!   (projection contiguë, frames dispersées). Le *backing* de la ressource est la liste de
//!   leurs segments physiques, fusionnés quand les frames se suivent. MGI en fait son tampon
//!   avant : le compositeur y écrit, l'appareil y lit.
//! * **Présentation** : un `TRANSFER_TO_HOST_2D` par rectangle abîmé (décalage du coin du
//!   rectangle dans le *backing*, § 5.7.6.8), au plus [`MAX_RECTS`] (fusion des voisins qui
//!   gaspillent le moins), puis un `RESOURCE_FLUSH` de leur enveloppe avec fence. La rafale ne
//!   sonne l'appareil qu'une fois. Rien n'est attendu : [`Display::wait_idle`] attend, avant la
//!   prochaine écriture du compositeur, que l'appareil ait fini de lire.
//! * **Curseur** : ressource 64 × 64 B8G8R8A8 (ARGB 8888), recopiée à chaque changement
//!   d'image puis `UPDATE_CURSOR` ; les mouvements sont des `MOVE_CURSOR` sur la `cursorq`.
//!   La position donnée est celle du point chaud (sémantique de `dpy_mouse_set` dans QEMU).

use alloc::format;
use alloc::string::ToString;
use alloc::vec::Vec;
use core::sync::atomic::Ordering::Relaxed;

use framebuffer::{AlphaPixel, Framebuffer};
use log::{error, warn};
use memory::{MappedPages, PteFlags, VirtualAddress};
use mgi::{CursorPlane, GpuReport, Scanout, ScanoutRect};
use spin::Mutex;
use virtio::Dma;

use crate::proto::{self, cmd, format, Rect, CURSOR_SIDE, FLAG_FENCE};
use crate::{now_ns, stat_max, Ticket, VirtioGpu};

/// Rectangles transférés par image au plus (au-delà : fusion).
pub const MAX_RECTS: usize = 32;

struct Pending {
    tickets: Vec<Ticket>,
    /// Soumission de la première commande en attente (ns monotones).
    t0: u64,
}

struct Cursor {
    resource: u32,
    dma: Option<Dma>,
    visible: bool,
}

pub struct Display {
    gpu: &'static VirtioGpu,
    scanout: u32,
    resource: u32,
    width: u32,
    height: u32,
    segments: usize,
    pages: Mutex<Option<MappedPages>>,
    pending: Mutex<Pending>,
    cursor: Mutex<Cursor>,
    /// Curseur matériel créé (fixé à la création : lu sans verrou par [`Scanout::report`]).
    hw_cursor: bool,
    /// Une erreur de soumission a déjà été journalisée.
    broken: core::sync::atomic::AtomicBool,
}

impl Display {
    pub(crate) fn new(gpu: &'static VirtioGpu, scanout: u32, width: u32, height: u32) -> Result<Display, &'static str> {
        if width == 0 || height == 0 || width > 16384 || height > 16384 {
            return Err("virtio-gpu : taille de scanout invalide");
        }
        let bytes = width as usize * height as usize * 4;
        let resource = gpu.alloc_resource_id();
        gpu.command(&proto::resource_create_2d(resource, format::B8G8R8X8_UNORM, width, height), proto::HDR_LEN)?;

        let pages = memory::create_mapping(bytes, PteFlags::new().valid(true).writable(true))?;
        let va = pages.start_address().value();
        // SAFETY : projection neuve, à nous, d'au moins `bytes` octets.
        unsafe { core::ptr::write_bytes(va as *mut u8, 0, bytes) };
        let mut entries: Vec<(u64, u32)> = Vec::new();
        let page = kernel_config::memory::PAGE_SIZE;
        for off in (0..bytes).step_by(page) {
            let pa = memory::translate(VirtualAddress::new_canonical(va + off))
                .ok_or("virtio-gpu : page du scanout non projetée")?
                .value() as u64;
            let len = page.min(bytes - off) as u32;
            match entries.last_mut() {
                Some(last) if last.0 + last.1 as u64 == pa && last.1 as u64 + len as u64 <= u32::MAX as u64 => last.1 += len,
                _ => entries.push((pa, len)),
            }
        }
        if entries.len() > 16384 {
            return Err("virtio-gpu : backing trop morcelé (16384 segments au plus)");
        }
        gpu.attach_backing(resource, &entries)?;
        gpu.command(&proto::set_scanout(scanout, resource, Rect { x: 0, y: 0, width, height }), proto::HDR_LEN)?;

        // Curseur matériel ; sans lui, le compositeur garde son curseur logiciel.
        let cursor = match Self::create_cursor(gpu) {
            Ok(c) => c,
            Err(e) => {
                warn!("virtio-gpu : pas de curseur matériel : {}", e);
                Cursor { resource: 0, dma: None, visible: false }
            }
        };
        warn!(
            "virtio-gpu : scanout {} : ressource {} de {}x{} (B8G8R8X8), backing {} Kio en {} segment(s), curseur {}",
            scanout,
            resource,
            width,
            height,
            bytes / 1024,
            entries.len(),
            if cursor.resource != 0 { "materiel 64x64" } else { "logiciel" }
        );
        Ok(Display {
            gpu,
            scanout,
            resource,
            width,
            height,
            segments: entries.len(),
            pages: Mutex::new(Some(pages)),
            pending: Mutex::new(Pending { tickets: Vec::new(), t0: 0 }),
            hw_cursor: cursor.resource != 0,
            cursor: Mutex::new(cursor),
            broken: core::sync::atomic::AtomicBool::new(false),
        })
    }

    fn create_cursor(gpu: &'static VirtioGpu) -> Result<Cursor, &'static str> {
        let id = gpu.alloc_resource_id();
        let side = CURSOR_SIDE as u32;
        gpu.command(&proto::resource_create_2d(id, format::B8G8R8A8_UNORM, side, side), proto::HDR_LEN)?;
        let dma = Dma::new(CURSOR_SIDE * CURSOR_SIDE * 4)?;
        gpu.attach_backing(id, &[(dma.phys(), (CURSOR_SIDE * CURSOR_SIDE * 4) as u32)])?;
        Ok(Cursor { resource: id, dma: Some(dma), visible: false })
    }

    /// `(largeur, hauteur)`.
    pub fn dims(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Segments physiques du *backing*.
    pub fn segments(&self) -> usize {
        self.segments
    }

    fn fail(&self, what: &str, e: &str) {
        if !self.broken.swap(true, Relaxed) {
            error!("virtio-gpu : {} : {} (présentation suspendue)", what, e);
        }
    }

    /// Rectangles bornés à l'écran, au plus [`MAX_RECTS`].
    fn prepare(&self, rects: &[ScanoutRect]) -> Vec<Rect> {
        let mut v: Vec<Rect> = rects
            .iter()
            .filter_map(|r| {
                let x0 = r.x.min(self.width);
                let y0 = r.y.min(self.height);
                let x1 = r.x.saturating_add(r.width).min(self.width);
                let y1 = r.y.saturating_add(r.height).min(self.height);
                (x1 > x0 && y1 > y0).then_some(Rect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 })
            })
            .collect();
        coalesce(&mut v, MAX_RECTS);
        v
    }

    /// Le tampon du scanout comme `Framebuffer` (une seule fois).
    pub fn framebuffer(&self) -> Option<Framebuffer<AlphaPixel>> {
        let mp = self.pages.lock().take()?;
        match Framebuffer::from_mapped_pages(self.width as usize, self.height as usize, mp) {
            Ok(fb) => Some(fb),
            Err(e) => {
                error!("virtio-gpu : tampon du scanout : {}", e);
                None
            }
        }
    }

    fn do_present(&self, rects: &[ScanoutRect]) {
        if self.broken.load(Relaxed) {
            return;
        }
        let rects = self.prepare(rects);
        if rects.is_empty() {
            return;
        }
        let stats = &self.gpu.stats;
        let mut p = self.pending.lock();
        if p.tickets.is_empty() {
            p.t0 = now_ns();
        }
        let mut bbox = rects[0];
        for r in rects.iter() {
            bbox = bbox.union(r);
            let offset = (r.y as u64 * self.width as u64 + r.x as u64) * 4;
            match self.gpu.command_async(&proto::transfer_to_host_2d(self.resource, *r, offset, 0, 0), false) {
                Ok(t) => p.tickets.push(t),
                Err(e) => return self.fail("TRANSFER_TO_HOST_2D", e),
            }
            stats.transfers.fetch_add(1, Relaxed);
            stats.transfer_bytes.fetch_add(r.area() * 4, Relaxed);
        }
        let fence = self.gpu.alloc_fence();
        match self.gpu.command_async(&proto::resource_flush(self.resource, bbox, FLAG_FENCE, fence), true) {
            Ok(t) => p.tickets.push(t),
            Err(e) => return self.fail("RESOURCE_FLUSH", e),
        }
        stats.flushes.fetch_add(1, Relaxed);
        stats.presents.fetch_add(1, Relaxed);
        stats.present_rects.fetch_add(rects.len() as u64, Relaxed);
    }

    fn do_wait_idle(&self) {
        let (tickets, t0) = {
            let mut p = self.pending.lock();
            if p.tickets.is_empty() {
                return;
            }
            (core::mem::take(&mut p.tickets), p.t0)
        };
        let start = now_ns();
        let mut last_done = 0;
        for t in tickets {
            match self.gpu.wait(&self.gpu.ctrl, t) {
                Ok(r) => {
                    if !r.ok() {
                        warn!("virtio-gpu : présentation : {} ({:#x})", proto::resp::name(r.ty), r.ty);
                    }
                    last_done = last_done.max(r.done_ns);
                }
                Err(e) => self.fail("attente de la présentation", e),
            }
        }
        let stats = &self.gpu.stats;
        stats.present_wait_ns.fetch_add(now_ns().saturating_sub(start), Relaxed);
        if last_done > t0 {
            let lat = last_done - t0;
            stats.present_lat_total_ns.fetch_add(lat, Relaxed);
            stats.present_lat_count.fetch_add(1, Relaxed);
            stat_max(&stats.present_lat_max_ns, lat);
        }
    }

    fn do_set_cursor(&self, image: Option<&CursorPlane<'_>>, x: i32, y: i32) {
        let mut c = self.cursor.lock();
        if c.resource == 0 {
            return;
        }
        let (x, y) = (x.max(0) as u32, y.max(0) as u32);
        let Some(img) = image else {
            if c.visible {
                c.visible = false;
                if let Err(e) = self.gpu.cursor_command(&proto::cursor(cmd::UPDATE_CURSOR, self.scanout, x, y, 0, 0, 0)) {
                    warn!("virtio-gpu : curseur : {}", e);
                }
            }
            return;
        };
        if img.width > CURSOR_SIDE || img.height > CURSOR_SIDE || img.argb.len() < img.width * img.height {
            warn!("virtio-gpu : image de curseur {}x{} refusée", img.width, img.height);
            return;
        }
        let Some(dma) = c.dma.as_ref() else { return };
        let base = dma.va() as *mut u32;
        for row in 0..CURSOR_SIDE {
            for col in 0..CURSOR_SIDE {
                let v = if row < img.height && col < img.width { img.argb[row * img.width + col] } else { 0 };
                // SAFETY : pixel (row, col) des 64 × 64 de la Dma du curseur.
                unsafe { base.add(row * CURSOR_SIDE + col).write_volatile(v) };
            }
        }
        let side = CURSOR_SIDE as u32;
        let full = Rect { x: 0, y: 0, width: side, height: side };
        // La copie doit être achevée avant UPDATE_CURSOR, qui lit l'image de la ressource.
        if let Err(e) = self.gpu.command(&proto::transfer_to_host_2d(c.resource, full, 0, 0, 0), proto::HDR_LEN) {
            warn!("virtio-gpu : image du curseur : {}", e);
            return;
        }
        let hx = img.hot_x.min(side - 1);
        let hy = img.hot_y.min(side - 1);
        match self.gpu.cursor_command(&proto::cursor(cmd::UPDATE_CURSOR, self.scanout, x, y, c.resource, hx, hy)) {
            Ok(()) => {
                c.visible = true;
                self.gpu.stats.cursor_updates.fetch_add(1, Relaxed);
            }
            Err(e) => warn!("virtio-gpu : curseur : {}", e),
        }
    }

    fn do_move_cursor(&self, x: i32, y: i32) {
        let c = self.cursor.lock();
        if c.resource == 0 || !c.visible {
            return;
        }
        let (x, y) = (x.max(0) as u32, y.max(0) as u32);
        // MOVE_CURSOR garde la ressource : QEMU cache le curseur si elle vaut 0.
        if self.gpu.cursor_command(&proto::cursor(cmd::MOVE_CURSOR, self.scanout, x, y, c.resource, 0, 0)).is_ok() {
            self.gpu.stats.cursor_moves.fetch_add(1, Relaxed);
        }
    }
}

/// Fusionne des rectangles voisins (triés par ligne puis colonne) jusqu'à en avoir au plus
/// `max`, en choisissant chaque fois la paire dont l'enveloppe ajoute le moins de pixels.
pub fn coalesce(v: &mut Vec<Rect>, max: usize) {
    v.sort_unstable_by_key(|r| (r.y, r.x));
    while v.len() > max.max(1) {
        let mut best = 0;
        let mut best_waste = u64::MAX;
        for i in 0..v.len() - 1 {
            let u = v[i].union(&v[i + 1]);
            let waste = u.area().saturating_sub(v[i].area() + v[i + 1].area());
            if waste < best_waste {
                best_waste = waste;
                best = i;
            }
        }
        let u = v[best].union(&v[best + 1]);
        v[best] = u;
        v.remove(best + 1);
    }
}

impl Scanout for Display {
    fn name(&self) -> &str {
        "virtio-gpu"
    }

    fn size(&self) -> (usize, usize) {
        (self.width as usize, self.height as usize)
    }

    fn take_framebuffer(&self) -> Option<Framebuffer<AlphaPixel>> {
        self.framebuffer()
    }

    fn present(&self, rects: &[ScanoutRect]) {
        self.do_present(rects)
    }

    fn wait_idle(&self) {
        self.do_wait_idle()
    }

    fn is_idle(&self) -> bool {
        self.pending.lock().tickets.iter().all(|t| self.gpu.is_done(&self.gpu.ctrl, *t))
    }

    fn cursor_size(&self) -> usize {
        if self.cursor.lock().resource != 0 {
            CURSOR_SIDE
        } else {
            0
        }
    }

    fn set_cursor(&self, image: Option<&CursorPlane<'_>>, x: i32, y: i32) {
        self.do_set_cursor(image, x, y)
    }

    fn move_cursor(&self, x: i32, y: i32) {
        self.do_move_cursor(x, y)
    }

    fn report(&self) -> Option<GpuReport> {
        let g = self.gpu;
        let st = g.stats();
        let adapter = match g.edid().as_ref() {
            Some(e) if !e.name.is_empty() => format!("virtio-gpu ({} {})", e.manufacturer_str(), e.name),
            _ => "virtio-gpu".to_string(),
        };
        let side = CURSOR_SIDE as u64;
        Some(GpuReport {
            adapter,
            driver: "virtio_gpu (MaiGPU G0) : 2D, rendu par l'hote".to_string(),
            location: format!("PCI {}", g.location),
            features: g.feature_names(),
            width: self.width,
            height: self.height,
            memory_bytes: self.width as u64 * self.height as u64 * 4 + if self.hw_cursor { side * side * 4 } else { 0 },
            busy_ns: st.busy_ns,
            presents: st.presents,
            transfer_bytes: st.transfer_bytes,
            present_lat_total_ns: st.present_lat_total_ns,
            present_lat_count: st.present_lat_count,
            present_lat_max_ns: st.present_lat_max_ns,
            commands: st.commands,
            errors: st.errors,
            notifies: st.notifies,
            interrupts: st.interrupts,
            hw_cursor: self.hw_cursor,
            cursor_moves: st.cursor_moves,
        })
    }
}

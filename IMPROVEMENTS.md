# 🚀 Améliorations pour Mai OS - Performance, Fluidité & API

Ce document liste les améliorations concrètes pour améliorer les performances, la fluidité et enrichir l'API du système de framebuffer.

## 📊 Table des matières
1. [Performance](#performance)
2. [Fluidité](#fluidité)
3. [API Complète](#api-complète)

---

## ⚡ Performance

### 1. Optimisation du Alpha Blending avec SIMD

**Problème actuel** : `AlphaPixel::blend()` et `composite_buffer()` utilisent des boucles pixel par pixel, très lentes.

**Solution** : Utiliser des opérations batch et potentiellement SIMD pour traiter plusieurs pixels à la fois.

```rust
// Dans pixel.rs - Version optimisée avec batch processing
impl Pixel for AlphaPixel {
    fn composite_buffer(src: &[Self], dest: &mut [Self]) {
        // Traiter par chunks de 4 ou 8 pixels pour meilleure utilisation du cache
        let chunks = src.chunks_exact(4);
        let remainder = chunks.remainder();
        
        for (s_chunk, d_chunk) in chunks.zip(dest.chunks_exact_mut(4)) {
            // Traitement batch de 4 pixels
            for (s, d) in s_chunk.iter().zip(d_chunk.iter_mut()) {
                *d = s.blend(*d);
            }
        }
        
        // Traiter le reste
        for (s, d) in remainder.iter().zip(dest.chunks_exact_mut(4).into_remainder()) {
            *d = s.blend(*d);
        }
    }
    
    // Version SIMD-ready (quand disponible)
    #[cfg(target_feature = "sse2")]
    fn composite_buffer_simd(src: &[Self], dest: &mut [Self]) {
        // Utiliser SSE2 pour traiter 16 pixels à la fois
        // Nécessite unsafe et intrinsics
    }
}
```

**Gain estimé** : 2-4x plus rapide pour les grandes zones.

---

### 2. Optimisation de `fill_circle` avec scanlines horizontales

**Problème actuel** : `fill_circle` utilise `draw_pixel` pour chaque pixel individuellement → très lent.

**Solution** : Utiliser des lignes horizontales comme dans `fill_rectangle`.

```rust
pub fn fill_circle<P: Pixel>(
    framebuffer: &mut Framebuffer<P>,
    center: Coord,
    r: usize,
    pixel: P,
) {
    if r == 0 {
        framebuffer.draw_pixel(center, pixel);
        return;
    }
    let (buf_w, buf_h) = framebuffer.get_size();
    let r_i = r as isize;
    let r2 = (r_i * r_i) as isize;
    let (cx, cy) = (center.x, center.y);
    
    // Pour chaque ligne horizontale
    for dy in -r_i..=r_i {
        let y = cy + dy;
        if y < 0 || y >= buf_h as isize {
            continue;
        }
        
        // Calculer la largeur de la ligne à cette hauteur
        let h = r2 - dy * dy;
        if h < 0 {
            continue;
        }
        
        // Calculer les limites x (approximation entière)
        let half_w = {
            // Approximation: sqrt(h) ≈ h >> (log2(h)/2)
            // Pour no_std, on peut utiliser une table de lookup ou itération
            let mut x_approx = 0isize;
            while x_approx * x_approx <= h && x_approx <= r_i {
                x_approx += 1;
            }
            x_approx - 1
        };
        
        let x_start = core::cmp::max(cx - half_w, 0);
        let x_end = core::cmp::min(cx + half_w + 1, buf_w as isize);
        
        if x_start < x_end {
            let len = (x_end - x_start) as usize;
            let mut row = Vec::with_capacity(len);
            row.resize(len, pixel);
            let idx = (y as usize) * buf_w + (x_start as usize);
            framebuffer.composite_buffer(&row, idx);
        }
    }
}
```

**Gain estimé** : 10-50x plus rapide selon la taille du cercle.

---

### 3. Cache LRU pour le Compositor

**Problème actuel** : Le cache du compositor peut grandir indéfiniment et ne supprime jamais les anciennes entrées.

**Solution** : Implémenter un cache LRU avec limite de taille.

```rust
use alloc::collections::VecDeque;

pub struct FrameCompositor {
    caches: BTreeMap<Coord, CacheBlock>,
    cache_order: VecDeque<Coord>, // Pour LRU
    max_cache_size: usize,
}

impl FrameCompositor {
    const MAX_CACHE_BLOCKS: usize = 1000; // Limite arbitraire
    
    fn evict_lru(&mut self) {
        if self.caches.len() >= Self::MAX_CACHE_BLOCKS {
            if let Some(oldest_key) = self.cache_order.pop_front() {
                self.caches.remove(&oldest_key);
            }
        }
    }
    
    fn mark_recent(&mut self, key: Coord) {
        // Retirer de la queue si présent
        self.cache_order.retain(|&k| k != key);
        // Ajouter à la fin (most recent)
        self.cache_order.push_back(key);
    }
}
```

**Gain estimé** : Meilleure utilisation mémoire, moins de fragmentation.

---

### 4. Hash optimisé pour le cache

**Problème actuel** : `hash()` utilise `DefaultHashBuilder` qui peut être lent pour de gros buffers.

**Solution** : Utiliser un hash rapide comme FNV ou xxHash.

```rust
// Hash FNV-1a (rapide, pas de dépendance externe)
fn hash_fnv<T: Hash>(item: T) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    // Implémentation FNV-1a simplifiée
    // Pour les slices de pixels, on peut hasher seulement un échantillon
    hash
}

// Ou mieux: hash seulement un échantillon des pixels
fn hash_pixel_sample<P: Pixel>(pixels: &[P], sample_rate: usize) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for (i, p) in pixels.iter().enumerate() {
        if i % sample_rate == 0 {
            // Hasher seulement chaque N-ème pixel
            hash ^= hash_pixel(p);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    hash
}
```

**Gain estimé** : 3-5x plus rapide pour le hashing.

---

### 5. Éviter le hash complet du Framebuffer

**Problème actuel** : `impl Hash for Framebuffer` hash tout le buffer → très coûteux.

**Solution** : Ne pas implémenter `Hash` pour `Framebuffer`, ou utiliser un hash partiel.

```rust
// Retirer Hash de Framebuffer, ou utiliser seulement width/height
impl<P: Pixel> Hash for Framebuffer<P> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.width.hash(state);
        self.height.hash(state);
        // Ne pas hasher le buffer entier !
    }
}
```

**Gain estimé** : Évite des calculs inutiles lors de l'utilisation dans des structures de données.

---

## 🎬 Fluidité

### 6. Double Buffering / Triple Buffering

**Problème actuel** : Pas de double buffering visible → possible flickering.

**Solution** : Ajouter un système de double buffering au niveau du framebuffer final.

```rust
pub struct DoubleBufferedFramebuffer<P: Pixel> {
    front: Framebuffer<P>,  // Affiché à l'écran
    back: Framebuffer<P>,   // En cours de rendu
    swapped: bool,
}

impl<P: Pixel> DoubleBufferedFramebuffer<P> {
    pub fn swap_buffers(&mut self) {
        // Échanger les pointeurs ou copier back -> front
        // Dans un vrai système, on utiliserait des pointeurs et un flip
        core::mem::swap(&mut self.front, &mut self.back);
    }
    
    pub fn back_buffer(&mut self) -> &mut Framebuffer<P> {
        &mut self.back
    }
    
    pub fn present(&mut self) {
        self.swap_buffers();
    }
}
```

**Gain estimé** : Élimine le flickering, rendu plus fluide.

---

### 7. Dirty Rectangle Tracking amélioré

**Problème actuel** : Le système de dirty rectangles existe mais pourrait être optimisé.

**Solution** : Ajouter un système de coalescing des rectangles sales.

```rust
pub struct DirtyRegionTracker {
    dirty_rects: Vec<Rectangle>,
    max_rects: usize,
}

impl DirtyRegionTracker {
    pub fn mark_dirty(&mut self, rect: Rectangle) {
        self.dirty_rects.push(rect);
        
        // Coalescer si trop de rectangles
        if self.dirty_rects.len() > self.max_rects {
            self.coalesce();
        }
    }
    
    fn coalesce(&mut self) {
        // Fusionner les rectangles qui se chevauchent ou sont adjacents
        // Algorithme simplifié: trouver le bounding box de tous
        if let Some(first) = self.dirty_rects.first() {
            let mut bbox = *first;
            for rect in &self.dirty_rects[1..] {
                bbox = bbox.union(rect);
            }
            self.dirty_rects.clear();
            self.dirty_rects.push(bbox);
        }
    }
    
    pub fn take_dirty(&mut self) -> Vec<Rectangle> {
        core::mem::take(&mut self.dirty_rects)
    }
}
```

**Gain estimé** : Moins d'appels au compositor, meilleure performance.

---

### 8. V-Sync / Frame Rate Limiting

**Problème actuel** : Pas de contrôle du framerate → peut rendre trop vite ou trop lent.

**Solution** : Ajouter un système de frame timing.

```rust
pub struct FrameLimiter {
    target_fps: u32,
    frame_time_ns: u64,
    last_frame_time: u64,
}

impl FrameLimiter {
    pub fn new(target_fps: u32) -> Self {
        let frame_time_ns = 1_000_000_000 / target_fps as u64;
        Self {
            target_fps,
            frame_time_ns,
            last_frame_time: 0,
        }
    }
    
    pub fn wait_for_next_frame(&mut self) {
        // Attendre jusqu'au prochain frame
        // Nécessite un système de timing (RTC, HPET, etc.)
        let current_time = get_current_time_ns();
        let elapsed = current_time - self.last_frame_time;
        
        if elapsed < self.frame_time_ns {
            let sleep_time = self.frame_time_ns - elapsed;
            // sleep_ns(sleep_time); // À implémenter
        }
        
        self.last_frame_time = get_current_time_ns();
    }
}
```

**Gain estimé** : Rendu plus prévisible, moins de consommation CPU.

---

## 🛠️ API Complète

### 9. Support des Polygones

```rust
pub fn draw_polygon<P: Pixel>(
    framebuffer: &mut Framebuffer<P>,
    vertices: &[Coord],
    pixel: P,
) {
    if vertices.len() < 3 {
        return;
    }
    
    // Dessiner les lignes entre les sommets
    for i in 0..vertices.len() {
        let start = vertices[i];
        let end = vertices[(i + 1) % vertices.len()];
        draw_line(framebuffer, start, end, pixel);
    }
}

pub fn fill_polygon<P: Pixel>(
    framebuffer: &mut Framebuffer<P>,
    vertices: &[Coord],
    pixel: P,
) {
    // Algorithme scanline pour remplir le polygone
    // 1. Trouver min_y et max_y
    // 2. Pour chaque ligne y, trouver les intersections avec les bords
    // 3. Remplir entre les intersections
}
```

---

### 10. Support des Ellipses

```rust
pub fn draw_ellipse<P: Pixel>(
    framebuffer: &mut Framebuffer<P>,
    center: Coord,
    radius_x: usize,
    radius_y: usize,
    pixel: P,
) {
    // Algorithme de Bresenham pour ellipses
    // Similaire au cercle mais avec deux rayons
}

pub fn fill_ellipse<P: Pixel>(
    framebuffer: &mut Framebuffer<P>,
    center: Coord,
    radius_x: usize,
    radius_y: usize,
    pixel: P,
) {
    // Utiliser scanlines comme pour fill_circle
}
```

---

### 11. Support des Courbes (Bézier, Splines)

```rust
pub fn draw_bezier_curve<P: Pixel>(
    framebuffer: &mut Framebuffer<P>,
    p0: Coord,
    p1: Coord,
    p2: Coord,
    p3: Coord,
    pixel: P,
    steps: usize,
) {
    // Courbe de Bézier cubique
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        let point = bezier_point(p0, p1, p2, p3, t);
        framebuffer.draw_pixel(point, pixel);
    }
}

fn bezier_point(p0: Coord, p1: Coord, p2: Coord, p3: Coord, t: f32) -> Coord {
    let u = 1.0 - t;
    let tt = t * t;
    let uu = u * u;
    let uuu = uu * u;
    let ttt = tt * t;
    
    Coord::new(
        (uuu * p0.x as f32 + 3.0 * uu * t * p1.x as f32 + 
         3.0 * u * tt * p2.x as f32 + ttt * p3.x as f32) as isize,
        (uuu * p0.y as f32 + 3.0 * uu * t * p1.y as f32 + 
         3.0 * u * tt * p2.y as f32 + ttt * p3.y as f32) as isize,
    )
}
```

---

### 12. Support des Gradients

```rust
pub fn fill_rectangle_gradient<P: Pixel>(
    framebuffer: &mut Framebuffer<P>,
    coordinate: Coord,
    width: usize,
    height: usize,
    start_color: P,
    end_color: P,
    direction: GradientDirection,
) {
    match direction {
        GradientDirection::Horizontal => {
            for x in 0..width {
                let t = x as f32 / width as f32;
                let color = P::weight_blend(start_color, end_color, t);
                // Dessiner une ligne verticale avec cette couleur
            }
        }
        GradientDirection::Vertical => {
            for y in 0..height {
                let t = y as f32 / height as f32;
                let color = P::weight_blend(start_color, end_color, t);
                // Dessiner une ligne horizontale avec cette couleur
            }
        }
        GradientDirection::Radial(center) => {
            // Gradient radial depuis le centre
        }
    }
}

pub enum GradientDirection {
    Horizontal,
    Vertical,
    Radial(Coord),
}
```

---

### 13. Clipping Rectangle

```rust
pub struct ClipRegion {
    rect: Option<Rectangle>,
}

impl ClipRegion {
    pub fn new(rect: Option<Rectangle>) -> Self {
        Self { rect }
    }
    
    pub fn clip_coord(&self, coord: Coord) -> Option<Coord> {
        if let Some(ref r) = self.rect {
            if r.contains(coord) {
                Some(coord)
            } else {
                None
            }
        } else {
            Some(coord)
        }
    }
    
    pub fn clip_rect(&self, rect: Rectangle) -> Option<Rectangle> {
        if let Some(ref clip) = self.rect {
            clip.intersection(&rect)
        } else {
            Some(rect)
        }
    }
}

// Ajouter à Framebuffer
impl<P: Pixel> Framebuffer<P> {
    pub fn with_clip<F>(&mut self, clip: ClipRegion, f: F)
    where
        F: FnOnce(&mut Framebuffer<P>),
    {
        // Sauvegarder l'ancien clip, appliquer le nouveau, exécuter f, restaurer
    }
}
```

---

### 14. Support des Sprites/Images

```rust
pub struct Sprite<P: Pixel> {
    width: usize,
    height: usize,
    pixels: Vec<P>,
}

impl<P: Pixel> Sprite<P> {
    pub fn from_slice(width: usize, height: usize, pixels: &[P]) -> Self {
        Self {
            width,
            height,
            pixels: pixels.to_vec(),
        }
    }
    
    pub fn draw(&self, framebuffer: &mut Framebuffer<P>, pos: Coord) {
        let (fb_w, fb_h) = framebuffer.get_size();
        let start_x = core::cmp::max(pos.x, 0) as usize;
        let start_y = core::cmp::max(pos.y, 0) as usize;
        let end_x = core::cmp::min(pos.x + self.width as isize, fb_w as isize) as usize;
        let end_y = core::cmp::min(pos.y + self.height as isize, fb_h as isize) as usize;
        
        for y in start_y..end_y {
            let src_y = y - start_y;
            if src_y >= self.height {
                break;
            }
            
            let src_start = src_y * self.width;
            let src_end = src_start + core::cmp::min(end_x - start_x, self.width);
            let dst_start = y * fb_w + start_x;
            
            framebuffer.composite_buffer(
                &self.pixels[src_start..src_end],
                dst_start,
            );
        }
    }
    
    pub fn draw_scaled(&self, framebuffer: &mut Framebuffer<P>, pos: Coord, scale: f32) {
        // Version avec scaling (nécessite interpolation)
    }
}
```

---

### 15. Transformations (Rotation, Scaling)

```rust
pub struct Transform {
    translation: Coord,
    rotation: f32, // en radians
    scale: (f32, f32),
}

impl Transform {
    pub fn identity() -> Self {
        Self {
            translation: Coord::new(0, 0),
            rotation: 0.0,
            scale: (1.0, 1.0),
        }
    }
    
    pub fn apply(&self, coord: Coord) -> Coord {
        // Appliquer rotation, scale, puis translation
        let cos_r = self.rotation.cos();
        let sin_r = self.rotation.sin();
        
        let x = coord.x as f32;
        let y = coord.y as f32;
        
        let rotated_x = x * cos_r - y * sin_r;
        let rotated_y = x * sin_r + y * cos_r;
        
        let scaled_x = rotated_x * self.scale.0;
        let scaled_y = rotated_y * self.scale.1;
        
        Coord::new(
            (scaled_x + self.translation.x as f32) as isize,
            (scaled_y + self.translation.y as f32) as isize,
        )
    }
}

// Utilisation avec clipping pour éviter les calculs inutiles
pub fn draw_sprite_transformed<P: Pixel>(
    sprite: &Sprite<P>,
    framebuffer: &mut Framebuffer<P>,
    transform: &Transform,
) {
    // Dessiner le sprite avec transformation appliquée
}
```

---

### 16. Text Rendering amélioré

```rust
pub struct TextRenderer {
    font: Font,
    cache: BTreeMap<char, Sprite<AlphaPixel>>, // Cache des glyphes rendus
}

impl TextRenderer {
    pub fn draw_text<P: Pixel>(
        &mut self,
        framebuffer: &mut Framebuffer<P>,
        text: &str,
        pos: Coord,
        size: usize,
        color: P,
    ) {
        let mut x = pos.x;
        for ch in text.chars() {
            if let Some(glyph) = self.get_glyph(ch, size) {
                glyph.draw(framebuffer, Coord::new(x, pos.y));
                x += glyph.width() as isize;
            }
        }
    }
    
    pub fn measure_text(&self, text: &str, size: usize) -> (usize, usize) {
        // Retourner la taille du texte sans le dessiner
    }
    
    pub fn draw_text_wrapped<P: Pixel>(
        &mut self,
        framebuffer: &mut Framebuffer<P>,
        text: &str,
        rect: Rectangle,
        size: usize,
        color: P,
    ) {
        // Dessiner le texte avec retour à la ligne automatique
    }
}
```

---

### 17. Anti-aliasing pour les lignes et cercles

```rust
pub fn draw_line_antialiased<P: Pixel>(
    framebuffer: &mut Framebuffer<P>,
    start: Coord,
    end: Coord,
    pixel: P,
) {
    // Algorithme de Wu pour l'anti-aliasing
    // Dessine des pixels avec alpha variable selon la distance à la ligne
}

pub fn draw_circle_antialiased<P: Pixel>(
    framebuffer: &mut Framebuffer<P>,
    center: Coord,
    r: usize,
    pixel: P,
) {
    // Version anti-aliased du cercle
    // Utilise l'alpha pour lisser les bords
}
```

---

## 📝 Priorités d'implémentation

### Phase 1 (Impact immédiat sur performance)
1. ✅ Optimisation `fill_circle` avec scanlines
2. ✅ Cache LRU pour compositor
3. ✅ Hash optimisé pour cache
4. ✅ Retirer hash complet de Framebuffer

### Phase 2 (Fluidité)
5. Double buffering
6. Dirty rectangle coalescing
7. Frame rate limiting

### Phase 3 (API complète)
8. Polygones et ellipses
9. Gradients
10. Clipping
11. Sprites/images
12. Transformations
13. Text rendering amélioré
14. Anti-aliasing

---

## 🔧 Notes d'implémentation

- **SIMD** : Peut nécessiter des features gates et unsafe code
- **Timing** : Frame limiting nécessite un système de timing (RTC/HPET)
- **Mathématiques** : Certaines fonctions (sin, cos, sqrt) peuvent nécessiter des approximations en no_std
- **Mémoire** : Certaines optimisations peuvent augmenter l'utilisation mémoire (cache, double buffering)

---

## 📚 Ressources

- [Bresenham's algorithms](https://en.wikipedia.org/wiki/Bresenham%27s_line_algorithm)
- [Alpha blending optimization](https://en.wikipedia.org/wiki/Alpha_compositing)
- [Dirty rectangle tracking](https://en.wikipedia.org/wiki/Dirty_rectangle_tracking)
- [Double buffering](https://en.wikipedia.org/wiki/Multiple_buffering)

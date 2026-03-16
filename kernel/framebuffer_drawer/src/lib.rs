//! This crate contains a series of basic draw functions to draw onto a framebuffer.
//! Displayables invoke these basic functions to display themselves onto a framebuffer.
//! The coordinate in these interfaces is relative to the origin(top-left point) of the framebuffer.

#![no_std]

extern crate alloc;
extern crate framebuffer;
extern crate shapes;

use alloc::vec::Vec;
use framebuffer::{Framebuffer, Pixel};
use shapes::Coord;

/// Draws a line using Bresenham's algorithm. Pixels outside the framebuffer are skipped.
#[inline]
pub fn draw_line<P: Pixel>(
    framebuffer: &mut Framebuffer<P>,
    start: Coord,
    end: Coord,
    pixel: P,
) {
    let mut x0 = start.x;
    let mut y0 = start.y;
    let x1 = end.x;
    let y1 = end.y;

    let dx = (x1 - x0).abs();
    let dy = (y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = if dx > dy { dx } else { -dy } / 2;

    loop {
        let coord = Coord::new(x0, y0);
        if framebuffer.contains(coord) {
            framebuffer.draw_pixel(coord, pixel);
        }
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = err;
        if e2 > -dx {
            err -= dy;
            x0 += sx;
        }
        if e2 < dy {
            err += dx;
            y0 += sy;
        }
    }
}

/// Draws the border of a rectangle. Pixels outside the framebuffer are skipped.
#[inline]
pub fn draw_rectangle<P: Pixel>(
    framebuffer: &mut Framebuffer<P>,
    coordinate: Coord,
    width: usize,
    height: usize,
    pixel: P,
) {
    if width == 0 || height == 0 || !framebuffer.overlaps_with(coordinate, width, height) {
        return;
    }
    let (w, h) = (width as isize, height as isize);
    let tl = coordinate;
    let tr = coordinate + (w - 1, 0);
    let bl = coordinate + (0, h - 1);
    let br = coordinate + (w - 1, h - 1);
    draw_line(framebuffer, tl, tr, pixel);
    draw_line(framebuffer, tr, br, pixel);
    draw_line(framebuffer, br, bl, pixel);
    draw_line(framebuffer, bl, tl, pixel);
}

/// Fills a rectangle with a solid color. Uses row-wise composite for speed.
#[inline]
pub fn fill_rectangle<P: Pixel>(
    framebuffer: &mut Framebuffer<P>,
    coordinate: Coord,
    width: usize,
    height: usize,
    pixel: P,
) {
    let (buffer_width, buffer_height) = framebuffer.get_size();
    if !framebuffer.overlaps_with(coordinate, width, height) { return; }
    let start_x = core::cmp::max(coordinate.x, 0) as usize;
    let start_y = core::cmp::max(coordinate.y, 0) as usize;
    let end_x = core::cmp::min(coordinate.x + width as isize, buffer_width as isize) as usize;
    let end_y = core::cmp::min(coordinate.y + height as isize, buffer_height as isize) as usize;
    let row_width = end_x - start_x;
    if row_width == 0 { return; }

    // ZERO allocation — on écrit directement dans le buffer
    let buf = framebuffer.buffer_mut();
    for y in start_y..end_y {
        let row_start = y * buffer_width + start_x;
        buf[row_start..row_start + row_width].fill(pixel);
    }
}

/// Draws the outline of a circle (midpoint algorithm). `center` is the center, `r` is the radius in pixels.
#[inline]
pub fn draw_circle<P: Pixel>(
    framebuffer: &mut Framebuffer<P>,
    center: Coord,
    r: usize,
    pixel: P,
) {
    if r == 0 {
        framebuffer.draw_pixel(center, pixel);
        return;
    }
    let r = r as isize;
    let (cx, cy) = (center.x, center.y);
    let mut x = r;
    let mut y = 0;
    let mut err = 1 - r;

    while x >= y {
        let points = [
            Coord::new(cx + x, cy + y),
            Coord::new(cx - x, cy + y),
            Coord::new(cx + x, cy - y),
            Coord::new(cx - x, cy - y),
            Coord::new(cx + y, cy + x),
            Coord::new(cx - y, cy + x),
            Coord::new(cx + y, cy - x),
            Coord::new(cx - y, cy - x),
        ];
        for c in &points {
            if framebuffer.contains(*c) {
                framebuffer.draw_pixel(*c, pixel);
            }
        }

        y += 1;
        if err <= 0 {
            err += 2 * y + 1;
        } else {
            x -= 1;
            err += 2 * (y - x) + 1;
        }
    }
}

/// Fills a circle with a solid color using optimized horizontal scanlines.
/// `center` is the center, `r` is the radius in pixels.
/// This version is much faster than pixel-by-pixel drawing.
#[inline]
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
    
    // Pour chaque ligne horizontale, calculer la largeur à remplir
    for dy in -r_i..=r_i {
        let y = cy + dy;
        if y < 0 || y >= buf_h as isize {
            continue;
        }
        
        // Calculer h = r² - dy² pour cette ligne
        let h = r2 - dy * dy;
        if h < 0 {
            continue;
        }
        
        // Approximation de sqrt(h) par recherche binaire simple
        // On cherche le plus grand x tel que x² <= h
        let mut x_approx = 0isize;
        while x_approx * x_approx <= h && x_approx <= r_i {
            x_approx += 1;
        }
        let half_w = x_approx - 1;
        
        if half_w < 0 {
            continue;
        }
        
        // Calculer les limites x pour cette ligne
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

/// Draws the outline of a polygon. `vertices` must contain at least 3 points.
#[inline]
pub fn draw_polygon<P: Pixel>(
    framebuffer: &mut Framebuffer<P>,
    vertices: &[Coord],
    pixel: P,
) {
    if vertices.len() < 3 {
        return;
    }
    
    // Draw lines between consecutive vertices, closing the polygon
    for i in 0..vertices.len() {
        let start = vertices[i];
        let end = vertices[(i + 1) % vertices.len()];
        draw_line(framebuffer, start, end, pixel);
    }
}

/// Draws an ellipse outline. `center` is the center, `radius_x` and `radius_y` are the radii.
#[inline]
pub fn draw_ellipse<P: Pixel>(
    framebuffer: &mut Framebuffer<P>,
    center: Coord,
    radius_x: usize,
    radius_y: usize,
    pixel: P,
) {
    if radius_x == 0 || radius_y == 0 {
        return;
    }
    
    let rx = radius_x as isize;
    let ry = radius_y as isize;
    let (cx, cy) = (center.x, center.y);
    
    // Midpoint ellipse algorithm (simplified)
    let mut x = 0isize;
    let mut y = ry;
    let rx2 = rx * rx;
    let ry2 = ry * ry;
    let two_rx2 = 2 * rx2;
    let two_ry2 = 2 * ry2;
    let mut p;
    
    // Region 1
    p = (ry2 - (rx2 * ry) + (rx2 / 4)) as isize;
    let mut px = 0isize;
    let mut py = two_rx2 * y;
    
    while px < py {
        let points = [
            Coord::new(cx + x, cy + y),
            Coord::new(cx - x, cy + y),
            Coord::new(cx + x, cy - y),
            Coord::new(cx - x, cy - y),
        ];
        for c in &points {
            if framebuffer.contains(*c) {
                framebuffer.draw_pixel(*c, pixel);
            }
        }
        
        x += 1;
        px += two_ry2;
        if p < 0 {
            p += ry2 + px;
        } else {
            y -= 1;
            py -= two_rx2;
            p += ry2 + px - py;
        }
    }
    
    // Region 2
    p = ((ry2 * ((x + 1) * (x + 1))) + (rx2 * ((y - 1) * (y - 1))) - (rx2 * ry2)) as isize;
    while y >= 0 {
        let points = [
            Coord::new(cx + x, cy + y),
            Coord::new(cx - x, cy + y),
            Coord::new(cx + x, cy - y),
            Coord::new(cx - x, cy - y),
        ];
        for c in &points {
            if framebuffer.contains(*c) {
                framebuffer.draw_pixel(*c, pixel);
            }
        }
        
        y -= 1;
        py -= two_rx2;
        if p > 0 {
            p += rx2 - py;
        } else {
            x += 1;
            px += two_ry2;
            p += rx2 - py + px;
        }
    }
}

/// Fills an ellipse with a solid color using horizontal scanlines.
#[inline]
pub fn fill_ellipse<P: Pixel>(
    framebuffer: &mut Framebuffer<P>,
    center: Coord,
    radius_x: usize,
    radius_y: usize,
    pixel: P,
) {
    if radius_x == 0 || radius_y == 0 {
        return;
    }
    
    let (buf_w, buf_h) = framebuffer.get_size();
    let rx = radius_x as isize;
    let ry = radius_y as isize;
    let rx2 = rx * rx;
    let ry2 = ry * ry;
    let (cx, cy) = (center.x, center.y);
    
    // For each horizontal line, calculate width to fill
    for dy in -ry..=ry {
        let y = cy + dy;
        if y < 0 || y >= buf_h as isize {
            continue;
        }
        
        // Calculate h = ry² - dy² for this line
        let h = ry2 - dy * dy;
        if h < 0 {
            continue;
        }
        
        // Calculate half width: sqrt(rx² * h / ry²)
        // Simplified: we need sqrt(rx² * h / ry²) = rx * sqrt(h) / ry
        // Approximate sqrt(h) by binary search
        let mut x_approx = 0isize;
        let h_scaled = (rx2 * h) / ry2;
        while x_approx * x_approx <= h_scaled && x_approx <= rx {
            x_approx += 1;
        }
        let half_w = x_approx - 1;
        
        if half_w < 0 {
            continue;
        }
        
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

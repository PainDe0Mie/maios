//! This crate contains a series of basic draw functions to draw onto a framebuffer.
//! Displayables invoke these basic functions to display themselves onto a framebuffer.
//! The coordinate in these interfaces is relative to the origin(top-left point) of the framebuffer.

#![no_std]

extern crate framebuffer;
extern crate shapes;

use framebuffer::{Framebuffer, Pixel};
use shapes::Coord;

/// Integer square root: returns the largest `x` such that `x*x <= n`.
#[inline]
fn isqrt(n: isize) -> isize {
    if n <= 0 { return 0; }
    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

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
/// Zero-allocation: writes directly into the framebuffer buffer.
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
    let r2 = r_i * r_i;
    let (cx, cy) = (center.x, center.y);

    let buf = framebuffer.buffer_mut();
    for dy in -r_i..=r_i {
        let y = cy + dy;
        if y < 0 || y >= buf_h as isize {
            continue;
        }
        let h = r2 - dy * dy;
        if h < 0 { continue; }

        // Integer sqrt via binary search (fast, no alloc)
        let half_w = isqrt(h);
        let x_start = core::cmp::max(cx - half_w, 0) as usize;
        let x_end = core::cmp::min(cx + half_w + 1, buf_w as isize) as usize;
        if x_start >= x_end { continue; }

        let row_start = (y as usize) * buf_w + x_start;
        buf[row_start..row_start + (x_end - x_start)].fill(pixel);
    }
}

/// Draws an anti-aliased line using Xiaolin Wu's algorithm.
/// Produces smoother lines by blending edge pixels with the background.
#[inline]
pub fn draw_line_aa<P: Pixel>(
    framebuffer: &mut Framebuffer<P>,
    start: Coord,
    end: Coord,
    pixel: P,
) {
    let (mut x0, mut y0) = (start.x as f32, start.y as f32);
    let (mut x1, mut y1) = (end.x as f32, end.y as f32);

    let steep = {
        let dy = if y1 > y0 { y1 - y0 } else { y0 - y1 };
        let dx = if x1 > x0 { x1 - x0 } else { x0 - x1 };
        dy > dx
    };
    if steep {
        core::mem::swap(&mut x0, &mut y0);
        core::mem::swap(&mut x1, &mut y1);
    }
    if x0 > x1 {
        core::mem::swap(&mut x0, &mut x1);
        core::mem::swap(&mut y0, &mut y1);
    }

    let dx = x1 - x0;
    let dy = y1 - y0;
    let gradient = if dx < 0.001 { 1.0 } else { dy / dx };

    // First endpoint
    let xend = (x0 + 0.5) as isize;
    let yend = y0 + gradient * (xend as f32 - x0);
    let xpxl1 = xend;
    let mut intery = yend + gradient;

    // Second endpoint
    let xend2 = (x1 + 0.5) as isize;
    let xpxl2 = xend2;

    // Main loop
    for x in (xpxl1 + 1)..xpxl2 {
        let y = intery as isize;
        let fpart = intery - (y as f32);

        // Plot two pixels per column with appropriate weights
        let c1 = Coord::new(if steep { y } else { x }, if steep { x } else { y });
        let c2 = Coord::new(if steep { y + 1 } else { x }, if steep { x } else { y + 1 });

        if framebuffer.contains(c1) {
            if let Some(bg) = framebuffer.get_pixel(c1) {
                let blended = P::weight_blend(pixel, bg, 1.0 - fpart);
                framebuffer.draw_pixel(c1, blended);
            }
        }
        if framebuffer.contains(c2) {
            if let Some(bg) = framebuffer.get_pixel(c2) {
                let blended = P::weight_blend(pixel, bg, fpart);
                framebuffer.draw_pixel(c2, blended);
            }
        }

        intery += gradient;
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
/// Zero-allocation: writes directly into the framebuffer buffer.
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

    let buf = framebuffer.buffer_mut();
    for dy in -ry..=ry {
        let y = cy + dy;
        if y < 0 || y >= buf_h as isize {
            continue;
        }
        let h = ry2 - dy * dy;
        if h < 0 { continue; }

        let h_scaled = (rx2 * h) / ry2;
        let half_w = isqrt(h_scaled);
        let x_start = core::cmp::max(cx - half_w, 0) as usize;
        let x_end = core::cmp::min(cx + half_w + 1, buf_w as isize) as usize;
        if x_start >= x_end { continue; }

        let row_start = (y as usize) * buf_w + x_start;
        buf[row_start..row_start + (x_end - x_start)].fill(pixel);
    }
}

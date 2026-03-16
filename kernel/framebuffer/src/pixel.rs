//! Defines the `Pixel` trait as well as basic pixel formats (RGB/RGBA). 

use core::hash::Hash;
use color::Color;
use zerocopy::FromBytes;

/// A pixel provides methods to blend with others.
pub trait Pixel: Copy + Hash + FromBytes {
    /// Composites the `src` pixel slice to the `dest` pixel slice.
    fn composite_buffer(src: &[Self], dest: &mut[Self]);
    
    /// blend with another pixel considering their extra channel.
    fn blend(self, other: Self) -> Self;

    /// Blend two pixels linearly with weights, as `blend` for `origin` and (1-`blend`) for `other`.
    fn weight_blend(origin: Self, other: Self, blend: f32) -> Self;
}


#[derive(Hash, Debug, Clone, Copy, FromBytes, Default)]
/// An RGB Pixel is a pixel with no extra channel.
pub struct RGBPixel {
    pub blue: u8,
    pub green: u8,
    pub red: u8,
    _channel: u8,
}

#[derive(Hash, Debug, Clone, Copy, FromBytes, Default)]
/// An RGBA pixel with an alpha channel.
pub struct AlphaPixel {
    pub blue: u8,
    pub green: u8,
    pub red: u8,
    pub alpha: u8
}

impl Pixel for RGBPixel {
    #[inline(always)]
    fn composite_buffer(src: &[Self], dest: &mut [Self]) {
        dest.copy_from_slice(src);
    }

    #[inline(always)]
    fn blend(self, _other: Self) -> Self {
        self
    }

    fn weight_blend(origin: Self, other: Self, blend: f32) -> Self {
        let t = blend.clamp(0.0, 1.0);
        let u = 1.0 - t;
        // Round manually: add 0.5 and truncate (no_std compatible)
        let round_u8 = |v: f32| {
            let clamped = v.clamp(0.0, 255.0);
            (clamped + 0.5) as u8
        };
        RGBPixel {
            _channel: 0,
            red: round_u8(origin.red as f32 * t + other.red as f32 * u),
            green: round_u8(origin.green as f32 * t + other.green as f32 * u),
            blue: round_u8(origin.blue as f32 * t + other.blue as f32 * u),
        }
    }
}

impl From<Color> for RGBPixel {
    fn from(color: Color) -> Self {
        RGBPixel {
            _channel: 0,
            red: color.red(),
            green: color.green(),
            blue: color.blue(),
        }
    }
}

impl Pixel for AlphaPixel {
    /// Composites src onto dest with alpha blending.
    /// alpha == 0 means fully opaque (overwrite), alpha == 255 means fully transparent (skip).
    /// Optimized: scans for contiguous opaque runs and bulk-copies them.
    #[inline]
    fn composite_buffer(src: &[Self], dest: &mut [Self]) {
        let len = src.len().min(dest.len());
        let mut i = 0;
        while i < len {
            if src[i].alpha == 0 {
                // Find the length of the contiguous opaque run
                let run_start = i;
                while i < len && src[i].alpha == 0 {
                    i += 1;
                }
                // Bulk copy the entire opaque run
                dest[run_start..i].copy_from_slice(&src[run_start..i]);
            } else if src[i].alpha == 255 {
                // Fully transparent — skip
                i += 1;
            } else {
                // Semi-transparent — blend
                dest[i] = src[i].blend(dest[i]);
                i += 1;
            }
        }
    }

    #[inline]
    fn blend(self, other: Self) -> Self {
        let alpha = self.alpha as u16;
        let red = self.red;
        let green = self.green;
        let blue = self.blue;
        // let ori_alpha = other.alpha;
        let ori_red = other.red;
        let ori_green = other.green;
        let ori_blue = other.blue;
        // let new_alpha = (((alpha as u16) * (255 - alpha) + (ori_alpha as u16) * alpha) / 255) as u8;
        let new_red = (((red as u16) * (255 - alpha) + (ori_red as u16) * alpha) / 255) as u8;
        let new_green = (((green as u16) * (255 - alpha) + (ori_green as u16) * alpha) / 255) as u8;
        let new_blue = (((blue as u16) * (255 - alpha) + (ori_blue as u16) * alpha) / 255) as u8;
        AlphaPixel {
            alpha: alpha as u8,
            red: new_red,
            green: new_green,
            blue: new_blue
        }
    }

    fn weight_blend(origin: Self, other: Self, blend: f32) -> Self {
        let t = blend.clamp(0.0, 1.0);
        let u = 1.0 - t;
        // Round manually: add 0.5 and truncate (no_std compatible)
        let round_u8 = |v: f32| {
            let clamped = v.clamp(0.0, 255.0);
            (clamped + 0.5) as u8
        };
        AlphaPixel {
            alpha: round_u8(origin.alpha as f32 * t + other.alpha as f32 * u),
            red: round_u8(origin.red as f32 * t + other.red as f32 * u),
            green: round_u8(origin.green as f32 * t + other.green as f32 * u),
            blue: round_u8(origin.blue as f32 * t + other.blue as f32 * u),
        }
    }
}

impl From<Color> for AlphaPixel {
    fn from(color: Color) -> Self {
        AlphaPixel {
            alpha: color.transparency(),
            red: color.red(),
            green: color.green(),
            blue: color.blue(),
        }
    }
}

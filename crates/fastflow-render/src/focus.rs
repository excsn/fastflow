//! Keeps the focused window sharp and blurs and dims everything else.

use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use fastflow_core::geom::Rect;

use crate::{Frame, RenderError, Result};

#[derive(Debug, Clone, Copy)]
pub struct FocusSettings {
    /// Blur radius in output pixels.
    pub blur: f64,
    /// 0 leaves the background's brightness alone, 1 makes it black.
    pub dim: f64,
    /// Width of the soft edge in output pixels.
    pub feather: f64,
}

/// A window to keep sharp, in output pixels. `weight` fades it in and out during a switch.
#[derive(Debug, Clone, Copy)]
pub struct FocusMask {
    pub rect: Rect,
    pub corner: f64,
    pub weight: f64,
}

/// The blur runs on a copy shrunk so its radius is a few pixels, then scaled back up. A wide
/// blur at full size would cost more than the rest of the frame.
const SMALL_RADIUS: f64 = 3.0;
const BOX_PASSES: usize = 3;

#[derive(Default)]
pub struct FocusBlur {
    resizer: Resizer,
    small: Vec<u8>,
    scratch: Vec<u8>,
    blurred: Vec<u8>,
}

fn resize_err(e: impl std::fmt::Display) -> RenderError {
    RenderError::Decode(format!("focus blur: {e}"))
}

impl FocusBlur {
    /// With no masks the whole frame blurs. So does a set whose weights are all zero.
    pub fn apply(
        &mut self,
        frame: &mut Frame,
        masks: &[FocusMask],
        s: &FocusSettings,
    ) -> Result<()> {
        self.blur(frame, s.blur)?;

        let (w, h) = (frame.width as usize, frame.height as usize);
        let keep = 1.0 - s.dim.clamp(0.0, 1.0) as f32;
        let feather = s.feather.max(0.5);
        let mut row_alpha = vec![0f32; w];
        for y in 0..h {
            let py = y as f64 + 0.5;
            row_alpha.iter_mut().for_each(|a| *a = 0.0);
            for m in masks.iter().filter(|m| m.weight > 0.0) {
                let r = &m.rect;
                if py < r.y - feather || py > r.y + r.h + feather {
                    continue;
                }
                let x0 = ((r.x - feather).floor().max(0.0) as usize).min(w);
                let x1 = ((r.x + r.w + feather).ceil().max(0.0) as usize).min(w);
                for (x, a) in row_alpha.iter_mut().enumerate().take(x1).skip(x0) {
                    let d = rounded_rect_distance(x as f64 + 0.5, py, r, m.corner);
                    let cover = smoothstep((0.5 - d / feather).clamp(0.0, 1.0)) * m.weight;
                    *a = a.max(cover as f32);
                }
            }
            let row = y * w * 4;
            for (x, &a) in row_alpha.iter().enumerate() {
                if a >= 1.0 {
                    continue;
                }
                let i = row + x * 4;
                for c in 0..3 {
                    let sharp = frame.data[i + c] as f32;
                    let soft = self.blurred[i + c] as f32 * keep;
                    frame.data[i + c] = (sharp * a + soft * (1.0 - a)).round() as u8;
                }
            }
        }
        Ok(())
    }

    fn blur(&mut self, frame: &Frame, radius: f64) -> Result<()> {
        let factor = (radius / SMALL_RADIUS).max(1.0);
        let sw = ((frame.width as f64 / factor).round() as u32).max(1);
        let sh = ((frame.height as f64 / factor).round() as u32).max(1);
        self.small.resize((sw * sh * 4) as usize, 0);
        self.blurred.resize(frame.data.len(), 0);

        let src = ImageRef::new(frame.width, frame.height, &frame.data, PixelType::U8x4)
            .map_err(resize_err)?;
        let mut small =
            Image::from_slice_u8(sw, sh, &mut self.small, PixelType::U8x4).map_err(resize_err)?;
        let down = ResizeOptions::new()
            .resize_alg(ResizeAlg::Convolution(FilterType::Box))
            .use_alpha(false);
        self.resizer
            .resize(&src, &mut small, &down)
            .map_err(resize_err)?;

        let r = (radius / factor).round().max(1.0) as usize;
        for _ in 0..BOX_PASSES {
            box_blur(
                &mut self.small,
                &mut self.scratch,
                sw as usize,
                sh as usize,
                r,
            );
        }

        let small = ImageRef::new(sw, sh, &self.small, PixelType::U8x4).map_err(resize_err)?;
        let mut full = Image::from_slice_u8(
            frame.width,
            frame.height,
            &mut self.blurred,
            PixelType::U8x4,
        )
        .map_err(resize_err)?;
        let up = ResizeOptions::new()
            .resize_alg(ResizeAlg::Convolution(FilterType::Bilinear))
            .use_alpha(false);
        self.resizer
            .resize(&small, &mut full, &up)
            .map_err(resize_err)
    }
}

fn smoothstep(x: f64) -> f64 {
    x * x * (3.0 - 2.0 * x)
}

/// Signed distance from a point to a rounded rect: negative inside.
fn rounded_rect_distance(px: f64, py: f64, r: &Rect, corner: f64) -> f64 {
    let corner = corner.min(r.w / 2.0).min(r.h / 2.0).max(0.0);
    let (cx, cy) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
    let qx = (px - cx).abs() - (r.w / 2.0 - corner);
    let qy = (py - cy).abs() - (r.h / 2.0 - corner);
    let outside = qx.max(0.0).hypot(qy.max(0.0));
    outside + qx.max(qy).min(0.0) - corner
}

/// One horizontal and one vertical pass of a box blur with edge clamping.
fn box_blur(img: &mut [u8], tmp: &mut Vec<u8>, w: usize, h: usize, r: usize) {
    tmp.resize(img.len(), 0);
    let n = (2 * r + 1) as u32;
    for y in 0..h {
        for c in 0..4 {
            let at = |x: isize| img[(y * w + x.clamp(0, w as isize - 1) as usize) * 4 + c] as u32;
            let mut sum: u32 = (-(r as isize)..=r as isize).map(at).sum();
            for x in 0..w {
                tmp[(y * w + x) * 4 + c] = (sum / n) as u8;
                sum += at(x as isize + r as isize + 1);
                sum -= at(x as isize - r as isize);
            }
        }
    }
    for x in 0..w {
        for c in 0..4 {
            let at = |y: isize| tmp[(y.clamp(0, h as isize - 1) as usize * w + x) * 4 + c] as u32;
            let mut sum: u32 = (-(r as isize)..=r as isize).map(at).sum();
            for y in 0..h {
                img[(y * w + x) * 4 + c] = (sum / n) as u8;
                sum += at(y as isize + r as isize + 1);
                sum -= at(y as isize - r as isize);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Alternating black and white columns: a blur turns them grey, sharp pixels stay pure.
    fn stripes(w: u32, h: u32) -> Frame {
        let mut f = Frame::black(w, h);
        for y in 0..h {
            for x in (0..w).step_by(2) {
                let i = ((y * w + x) * 4) as usize;
                f.data[i..i + 3].copy_from_slice(&[255, 255, 255]);
            }
        }
        f
    }

    fn value(f: &Frame, x: u32, y: u32) -> u8 {
        f.data[((y * f.width + x) * 4) as usize]
    }

    fn settings() -> FocusSettings {
        FocusSettings {
            blur: 12.0,
            dim: 0.0,
            feather: 2.0,
        }
    }

    fn mask(weight: f64) -> FocusMask {
        FocusMask {
            rect: Rect {
                x: 40.0,
                y: 40.0,
                w: 80.0,
                h: 80.0,
            },
            corner: 8.0,
            weight,
        }
    }

    #[test]
    fn inside_stays_sharp_and_outside_blurs() {
        let mut f = stripes(160, 160);
        FocusBlur::default()
            .apply(&mut f, &[mask(1.0)], &settings())
            .unwrap();
        assert_eq!((value(&f, 80, 80), value(&f, 81, 80)), (255, 0));
        let far = value(&f, 10, 10);
        assert!(
            (60..=200).contains(&far),
            "background should be grey, got {far}"
        );
    }

    #[test]
    fn a_half_weight_mask_is_half_sharp() {
        let mut f = stripes(160, 160);
        FocusBlur::default()
            .apply(&mut f, &[mask(0.5)], &settings())
            .unwrap();
        let v = value(&f, 80, 80);
        assert!((150..=235).contains(&v), "{v}");
    }

    #[test]
    fn no_masks_blurs_everything() {
        let mut f = stripes(160, 160);
        FocusBlur::default()
            .apply(&mut f, &[], &settings())
            .unwrap();
        let (a, b) = (value(&f, 80, 80), value(&f, 81, 80));
        assert!(
            a.abs_diff(b) < 60,
            "stripes should be smeared, got {a} and {b}"
        );
    }

    #[test]
    fn rounded_corners_are_outside() {
        let r = Rect {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 100.0,
        };
        assert!(rounded_rect_distance(1.0, 1.0, &r, 20.0) > 0.0);
        assert!(rounded_rect_distance(50.0, 1.0, &r, 20.0) < 0.0);
    }
}

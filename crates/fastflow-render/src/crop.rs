use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use fastflow_core::geom::Rect;

use crate::{Frame, RenderError, Result};

/// Crops a normalized rect out of a frame and resizes it into another, the one place normalized
/// coordinates become pixels. The float crop box carries subpixel camera positions into the
/// resampler; rounding the rect per frame is what makes camera motion stutter.
pub struct Cropper {
    resizer: Resizer,
    scratch: Vec<u8>,
}

impl Default for Cropper {
    fn default() -> Self {
        Cropper {
            resizer: Resizer::new(),
            scratch: Vec::new(),
        }
    }
}

fn resize_err(e: impl std::fmt::Display) -> RenderError {
    RenderError::Decode(format!("resize: {e}"))
}

impl Cropper {
    /// Any part of `rect` outside the frame comes out black.
    pub fn crop_into(&mut self, src: &Frame, rect: Rect, dst: &mut Frame) -> Result<()> {
        let (sw, sh) = (src.width as f64, src.height as f64);
        let (dw, dh) = (dst.width as f64, dst.height as f64);
        let (ix0, iy0) = (rect.x.max(0.0), rect.y.max(0.0));
        let (ix1, iy1) = ((rect.x + rect.w).min(1.0), (rect.y + rect.h).min(1.0));
        if ix1 <= ix0 || iy1 <= iy0 {
            dst.data
                .copy_from_slice(&Frame::black(dst.width, dst.height).data);
            return Ok(());
        }

        let options = ResizeOptions::new()
            .resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3))
            .use_alpha(false)
            .crop(ix0 * sw, iy0 * sh, (ix1 - ix0) * sw, (iy1 - iy0) * sh);
        let src_img =
            ImageRef::new(src.width, src.height, &src.data, PixelType::U8x4).map_err(resize_err)?;

        let covers =
            ix0 == rect.x && iy0 == rect.y && ix1 == rect.x + rect.w && iy1 == rect.y + rect.h;
        if covers {
            let mut dst_img =
                Image::from_slice_u8(dst.width, dst.height, &mut dst.data, PixelType::U8x4)
                    .map_err(resize_err)?;
            return self
                .resizer
                .resize(&src_img, &mut dst_img, &options)
                .map_err(resize_err);
        }

        let x0 = (((ix0 - rect.x) / rect.w) * dw).round() as u32;
        let y0 = (((iy0 - rect.y) / rect.h) * dh).round() as u32;
        let x1 = ((((ix1 - rect.x) / rect.w) * dw).round() as u32).min(dst.width);
        let y1 = ((((iy1 - rect.y) / rect.h) * dh).round() as u32).min(dst.height);
        let (pw, ph) = (x1.saturating_sub(x0).max(1), y1.saturating_sub(y0).max(1));
        self.scratch.resize((pw * ph * 4) as usize, 0);
        let mut part =
            Image::from_slice_u8(pw, ph, &mut self.scratch, PixelType::U8x4).map_err(resize_err)?;
        self.resizer
            .resize(&src_img, &mut part, &options)
            .map_err(resize_err)?;

        dst.data
            .copy_from_slice(&Frame::black(dst.width, dst.height).data);
        let stride = dst.width as usize * 4;
        let row = pw as usize * 4;
        for y in 0..ph as usize {
            let d = (y0 as usize + y) * stride + x0 as usize * 4;
            let end = (d + row).min((y0 as usize + y + 1) * stride);
            dst.data[d..end].copy_from_slice(&self.scratch[y * row..y * row + (end - d)]);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Red carries x and green carries y, so any pixel says where it came from.
    fn gradient(w: u32, h: u32) -> Frame {
        let mut f = Frame::black(w, h);
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                f.data[i] = (x * 255 / (w - 1)) as u8;
                f.data[i + 1] = (y * 255 / (h - 1)) as u8;
            }
        }
        f
    }

    fn px(f: &Frame, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * f.width + x) * 4) as usize;
        f.data[i..i + 4].try_into().unwrap()
    }

    #[test]
    fn full_rect_is_a_plain_resize() {
        let src = gradient(256, 256);
        let mut dst = Frame::black(128, 128);
        let full = Rect {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
        };
        Cropper::default().crop_into(&src, full, &mut dst).unwrap();
        let [r, g, _, _] = px(&dst, 127, 127);
        assert!(r > 245 && g > 245);
        let [r, g, _, _] = px(&dst, 0, 0);
        assert!(r < 10 && g < 10);
    }

    #[test]
    fn a_quarter_rect_shows_that_quarter() {
        let src = gradient(256, 256);
        let mut dst = Frame::black(64, 64);
        let br = Rect {
            x: 0.5,
            y: 0.5,
            w: 0.5,
            h: 0.5,
        };
        Cropper::default().crop_into(&src, br, &mut dst).unwrap();
        let [r, g, _, _] = px(&dst, 0, 0);
        assert!(
            (120..=140).contains(&r) && (120..=140).contains(&g),
            "{r} {g}"
        );
    }

    #[test]
    fn area_outside_the_frame_is_black() {
        let src = gradient(100, 100);
        let mut dst = Frame::black(100, 50);
        let wide = Rect {
            x: -0.5,
            y: 0.0,
            w: 2.0,
            h: 1.0,
        };
        Cropper::default().crop_into(&src, wide, &mut dst).unwrap();
        assert_eq!(px(&dst, 5, 25), [0, 0, 0, 255]);
        assert_eq!(px(&dst, 95, 25), [0, 0, 0, 255]);
        assert_ne!(px(&dst, 50, 25), [0, 0, 0, 255]);
    }
}

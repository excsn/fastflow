use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_graphics::geometry::CGRect;
use core_graphics::window::{
    copy_window_info, kCGNullWindowID, kCGWindowBounds, kCGWindowLayer,
    kCGWindowListExcludeDesktopElements, kCGWindowListOptionOnScreenOnly, kCGWindowNumber,
    kCGWindowOwnerPID,
};
use fastflow_core::geom::{Point, Rect};
use fastflow_core::recording::WindowInfo;

use super::display::{self, rect};
use crate::{DesktopError, Result, WindowCaps, WindowSource};

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGRectMakeWithDictionaryRepresentation(dict: CFDictionaryRef, rect: *mut CGRect) -> bool;
}

/// Samples on-screen windows intersecting one display, front to back.
pub struct MacWindowSource {
    surface: Rect,
}

impl MacWindowSource {
    pub fn new(surface_pt: Rect) -> Self {
        MacWindowSource {
            surface: surface_pt,
        }
    }
}

impl WindowSource for MacWindowSource {
    fn caps(&self) -> WindowCaps {
        WindowCaps {
            can_read_other_app_geometry: true,
        }
    }

    fn sample(&mut self) -> Result<Vec<WindowInfo>> {
        let list: CFArray = copy_window_info(
            kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
            kCGNullWindowID,
        )
        .ok_or_else(|| DesktopError("CGWindowListCopyWindowInfo returned null".into()))?;

        let keys = unsafe {
            [
                CFString::wrap_under_get_rule(kCGWindowNumber),
                CFString::wrap_under_get_rule(kCGWindowOwnerPID),
                CFString::wrap_under_get_rule(kCGWindowLayer),
                CFString::wrap_under_get_rule(kCGWindowBounds),
            ]
        };

        let mut out = Vec::new();
        for item in list.iter() {
            let dict: CFDictionary<CFString, CFType> =
                unsafe { CFDictionary::wrap_under_get_rule(*item as CFDictionaryRef) };
            let number = |k: &CFString| {
                dict.find(k)
                    .and_then(|v| v.downcast::<CFNumber>())
                    .and_then(|n| n.to_i64())
            };
            let (Some(id), Some(pid), Some(layer)) =
                (number(&keys[0]), number(&keys[1]), number(&keys[2]))
            else {
                continue;
            };
            let Some(bounds) = dict.find(&keys[3]) else {
                continue;
            };
            let mut cg = CGRect::default();
            let ok = unsafe {
                CGRectMakeWithDictionaryRepresentation(
                    bounds.as_CFTypeRef() as CFDictionaryRef,
                    &mut cg,
                )
            };
            let bounds = rect(cg);
            if !ok || !bounds.intersects(&self.surface) {
                continue;
            }
            out.push(WindowInfo {
                id: id as u32,
                pid: pid as i32,
                layer: layer as i32,
                rect: bounds.normalized_in(&self.surface),
            });
        }
        Ok(out)
    }

    fn cursor(&mut self) -> Result<Point> {
        display::cursor()
            .map(|p| p.normalized_in(&self.surface))
            .ok_or_else(|| DesktopError("cursor position unavailable".into()))
    }
}

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
  pub x: f64,
  pub y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
  pub x: f64,
  pub y: f64,
  pub w: f64,
  pub h: f64,
}

impl Rect {
  pub fn intersects(&self, other: &Rect) -> bool {
    self.x < other.x + other.w
      && other.x < self.x + self.w
      && self.y < other.y + other.h
      && other.y < self.y + self.h
  }

  /// Expresses `self` as fractions of `surface`. Both must be in the same unit and origin.
  pub fn normalized_in(&self, surface: &Rect) -> Rect {
    Rect {
      x: (self.x - surface.x) / surface.w,
      y: (self.y - surface.y) / surface.h,
      w: self.w / surface.w,
      h: self.h / surface.h,
    }
  }
}

impl Point {
  pub fn normalized_in(&self, surface: &Rect) -> Point {
    Point {
      x: (self.x - surface.x) / surface.w,
      y: (self.y - surface.y) / surface.h,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn normalizes_against_an_offset_surface() {
    let surface = Rect {
      x: 100.0,
      y: 50.0,
      w: 200.0,
      h: 100.0,
    };
    let r = Rect {
      x: 150.0,
      y: 75.0,
      w: 100.0,
      h: 50.0,
    }
    .normalized_in(&surface);
    assert_eq!(
      r,
      Rect {
        x: 0.25,
        y: 0.25,
        w: 0.5,
        h: 0.5
      }
    );
    let p = Point { x: 300.0, y: 150.0 }.normalized_in(&surface);
    assert_eq!(p, Point { x: 1.0, y: 1.0 });
  }

  #[test]
  fn touching_edges_do_not_intersect() {
    let a = Rect {
      x: 0.0,
      y: 0.0,
      w: 10.0,
      h: 10.0,
    };
    let b = Rect {
      x: 10.0,
      y: 0.0,
      w: 10.0,
      h: 10.0,
    };
    assert!(!a.intersects(&b));
    assert!(a.intersects(&Rect { x: 9.0, ..b }));
  }
}

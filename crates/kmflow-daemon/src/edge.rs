use kmflow_proto::{Edge, ScreenInfo};

const DEAD_ZONE_PX: u32 = 50;

#[derive(Debug, Clone)]
pub struct EdgeDetector {
    screen: ScreenInfo,
    target_edge: Edge,
    dead_zone: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeHit {
    None,
    Triggered(Edge),
}

impl EdgeDetector {
    pub fn new(screen: ScreenInfo, target_edge: Edge) -> Self {
        Self {
            screen,
            target_edge,
            dead_zone: DEAD_ZONE_PX,
        }
    }

    pub fn with_dead_zone(mut self, px: u32) -> Self {
        self.dead_zone = px;
        self
    }

    pub fn check(&self, x: i32, y: i32) -> EdgeHit {
        // Don't trigger in corners (dead zones)
        if self.in_corner(x, y) {
            return EdgeHit::None;
        }

        match self.target_edge {
            Edge::Right => {
                if x >= self.screen.width as i32 - 1 {
                    EdgeHit::Triggered(Edge::Right)
                } else {
                    EdgeHit::None
                }
            }
            Edge::Left => {
                if x <= 0 {
                    EdgeHit::Triggered(Edge::Left)
                } else {
                    EdgeHit::None
                }
            }
            Edge::Top => {
                if y <= 0 {
                    EdgeHit::Triggered(Edge::Top)
                } else {
                    EdgeHit::None
                }
            }
            Edge::Bottom => {
                if y >= self.screen.height as i32 - 1 {
                    EdgeHit::Triggered(Edge::Bottom)
                } else {
                    EdgeHit::None
                }
            }
        }
    }

    fn in_corner(&self, x: i32, y: i32) -> bool {
        let w = self.screen.width as i32;
        let h = self.screen.height as i32;
        let dz = self.dead_zone as i32;

        let in_top = y < dz;
        let in_bottom = y > h - dz;
        let in_left = x < dz;
        let in_right = x > w - dz;

        // Corner = intersection of two edges
        (in_top || in_bottom) && (in_left || in_right)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kmflow_proto::ScreenPosition;

    fn test_screen() -> ScreenInfo {
        ScreenInfo {
            width: 1920,
            height: 1080,
            scale_factor: 1.0,
            position: ScreenPosition {
                edge: Edge::Right,
                monitor_id: 0,
            },
        }
    }

    #[test]
    fn edge_trigger_right() {
        let detector = EdgeDetector::new(test_screen(), Edge::Right);
        assert_eq!(detector.check(1919, 540), EdgeHit::Triggered(Edge::Right));
        assert_eq!(detector.check(1918, 540), EdgeHit::None);
    }

    #[test]
    fn dead_zone_blocks_corner() {
        let detector = EdgeDetector::new(test_screen(), Edge::Right);
        // Top-right corner should be blocked
        assert_eq!(detector.check(1919, 10), EdgeHit::None);
        // Bottom-right corner should be blocked
        assert_eq!(detector.check(1919, 1075), EdgeHit::None);
        // Middle-right should trigger
        assert_eq!(detector.check(1919, 540), EdgeHit::Triggered(Edge::Right));
    }
}

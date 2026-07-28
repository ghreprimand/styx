//! Pure display-geometry logic shared by every receiver backend.
//!
//! None of this touches a platform API. It was extracted from the macOS
//! injector so the Linux backend can reuse the cursor clamping and
//! edge-resolution rules verbatim rather than reimplementing them and
//! rediscovering the same bugs. Everything here is unit-testable on any
//! host, which also means the macOS-only logic finally gets test coverage
//! that runs in CI on Linux.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

/// A display rectangle in the platform's global coordinate space.
///
/// On macOS this is CG global coordinates (y grows downward, origin at the
/// top-left of the main display). On Linux it is the compositor's layout
/// space as declared in the config. Both share the y-down convention, so
/// "bottom" consistently means maximum y.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayBounds {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

/// The unioned extent of the edge-owning displays along the axis parallel
/// to the crossover edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EdgeSpan {
    pub min: f64,
    pub max: f64,
}

/// How close two monitor edges must be (in points) to count as occupying
/// the same return-edge column. Handles displays whose outer edges do not
/// line up exactly -- e.g. a portrait monitor stacked above a laptop
/// display where both face the sender on their right side.
pub const EDGE_ALIGN_TOLERANCE: f64 = 64.0;

/// Given the cursor's post-clamp position and the set of displays that own
/// the return edge, decide whether the cursor has reached the outer return
/// edge. If it has, pin the returned coordinate to that edge display's OWN
/// outer boundary and report `true`.
///
/// Detection is done against the specific edge display whose perpendicular
/// span contains the cursor -- not against the global bounding box. This
/// matters when the edge displays are not flush. Example: a portrait
/// monitor (right edge x=1440) stacked above a laptop panel (right edge
/// x=1470). The caller's global clamp caps x at 1469, which is past the
/// portrait's right edge, so while the cursor is on the portrait it can
/// drift into the x in (1440, 1469] band that belongs to no display. There
/// a fixed 1 px "at the edge" test can only be satisfied by chance, so the
/// cursor appears to stick until the user jitters it back into that band.
/// Pinning to the edge display's own boundary removes the dead zone and
/// makes the crossover fire the moment the cursor reaches the portrait's
/// edge. Detecting per-display also keeps a non-edge monitor that merely
/// sits at the extreme x from falsely triggering return, and lets any of
/// several stacked edge displays send the signal.
pub fn resolve_edge_hit(
    edge_displays: &[DisplayBounds],
    return_edge: Edge,
    x: f64,
    y: f64,
) -> (f64, f64, bool) {
    let mut nx = x;
    let mut ny = y;
    let hit = edge_displays
        .iter()
        .find(|d| match return_edge {
            Edge::Right | Edge::Left => ny >= d.min_y && ny < d.max_y,
            Edge::Top | Edge::Bottom => nx >= d.min_x && nx < d.max_x,
        })
        .map(|d| match return_edge {
            Edge::Right => {
                if nx >= d.max_x - 1.0 { nx = d.max_x - 1.0; true } else { false }
            }
            Edge::Left => {
                if nx <= d.min_x { nx = d.min_x; true } else { false }
            }
            Edge::Bottom => {
                if ny >= d.max_y - 1.0 { ny = d.max_y - 1.0; true } else { false }
            }
            Edge::Top => {
                if ny <= d.min_y { ny = d.min_y; true } else { false }
            }
        })
        .unwrap_or(false);
    (nx, ny, hit)
}

/// Filter a full display list down to those sitting at the return edge.
/// For `Edge::Right` that is every display whose right edge is within
/// `EDGE_ALIGN_TOLERANCE` of the overall rightmost x; analogously for the
/// other edges.
pub fn edge_displays_of(all: &[DisplayBounds], return_edge: Edge) -> Vec<DisplayBounds> {
    if all.is_empty() {
        return Vec::new();
    }
    let extreme = match return_edge {
        Edge::Right => all.iter().map(|d| d.max_x).fold(f64::MIN, f64::max),
        Edge::Left => all.iter().map(|d| d.min_x).fold(f64::MAX, f64::min),
        Edge::Bottom => all.iter().map(|d| d.max_y).fold(f64::MIN, f64::max),
        Edge::Top => all.iter().map(|d| d.min_y).fold(f64::MAX, f64::min),
    };
    all.iter()
        .filter(|d| {
            let own = match return_edge {
                Edge::Right => d.max_x,
                Edge::Left => d.min_x,
                Edge::Bottom => d.max_y,
                Edge::Top => d.min_y,
            };
            (own - extreme).abs() <= EDGE_ALIGN_TOLERANCE
        })
        .copied()
        .collect()
}

/// Union the Y span (left/right edges) or X span (top/bottom edges) over
/// a set of edge-owning displays.
pub fn span_of_displays(displays: &[DisplayBounds], return_edge: Edge) -> EdgeSpan {
    if displays.is_empty() {
        return EdgeSpan { min: 0.0, max: 1080.0 };
    }
    let mut min = f64::MAX;
    let mut max = f64::MIN;
    for d in displays {
        let (a, b) = match return_edge {
            Edge::Left | Edge::Right => (d.min_y, d.max_y),
            Edge::Top | Edge::Bottom => (d.min_x, d.max_x),
        };
        if a < min { min = a; }
        if b > max { max = b; }
    }
    EdgeSpan { min, max }
}

/// Constrain a tentative cursor position to the union of all active displays,
/// reproducing the per-display constraint macOS applies to real HID input.
///
/// If the point already lies inside some display it is returned unchanged --
/// this lets the cursor move freely across shared borders between adjacent
/// displays. Otherwise it is snapped to the nearest in-bounds point across all
/// displays. This prevents the cursor coming to rest in a phantom region of the
/// global bounding box that belongs to no display -- e.g. the band directly
/// below a short built-in panel when a taller display sits beside it. Events
/// parked in that band never satisfy the Dock's bottom-edge reveal test,
/// because the cursor is below the panel's real bottom edge rather than on it.
///
/// The half-open test (`x < max_x`, `y < max_y`) matches the per-display edge
/// pinning elsewhere in this file: the reachable maximum on each axis is
/// `max - 1.0`, i.e. the display's real last row/column.
pub fn clamp_to_displays(displays: &[DisplayBounds], x: f64, y: f64) -> (f64, f64) {
    if displays.is_empty() {
        return (x, y);
    }
    let inside = displays
        .iter()
        .any(|d| x >= d.min_x && x < d.max_x && y >= d.min_y && y < d.max_y);
    if inside {
        return (x, y);
    }
    let mut best = (x, y);
    let mut best_dist = f64::MAX;
    for d in displays {
        let cx = x.clamp(d.min_x, d.max_x - 1.0);
        let cy = y.clamp(d.min_y, d.max_y - 1.0);
        let dist = (cx - x).powi(2) + (cy - y).powi(2);
        if dist < best_dist {
            best_dist = dist;
            best = (cx, cy);
        }
    }
    best
}

/// Compute the entry-edge cursor position for a crossover arriving with
/// `from_bottom` pixels of offset from the bottom of the combined edge span.
///
/// Clamps to the span and picks the specific edge display containing the
/// target position, falling back to the nearest one if the target lands in
/// a gap between stacked displays. Returns `None` only when there are no
/// edge displays at all, in which case the caller should leave the cursor
/// where it is.
pub fn place_from_bottom(
    edge_displays: &[DisplayBounds],
    edge_span: EdgeSpan,
    return_edge: Edge,
    from_bottom: f64,
) -> Option<(f64, f64)> {
    if edge_displays.is_empty() {
        return None;
    }
    let pos = (edge_span.max - from_bottom).clamp(edge_span.min, edge_span.max);
    let target = edge_displays
        .iter()
        .find(|d| match return_edge {
            Edge::Left | Edge::Right => pos >= d.min_y && pos < d.max_y,
            Edge::Top | Edge::Bottom => pos >= d.min_x && pos < d.max_x,
        })
        .or_else(|| {
            edge_displays.iter().min_by(|a, b| {
                let ma = match return_edge {
                    Edge::Left | Edge::Right => (a.min_y + a.max_y) * 0.5,
                    Edge::Top | Edge::Bottom => (a.min_x + a.max_x) * 0.5,
                };
                let mb = match return_edge {
                    Edge::Left | Edge::Right => (b.min_y + b.max_y) * 0.5,
                    Edge::Top | Edge::Bottom => (b.min_x + b.max_x) * 0.5,
                };
                (ma - pos)
                    .abs()
                    .partial_cmp(&(mb - pos).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        })?;
    Some(match return_edge {
        Edge::Right => (target.max_x - 2.0, pos.clamp(target.min_y, target.max_y - 1.0)),
        Edge::Left => (target.min_x + 2.0, pos.clamp(target.min_y, target.max_y - 1.0)),
        Edge::Bottom => (pos.clamp(target.min_x, target.max_x - 1.0), target.max_y - 2.0),
        Edge::Top => (pos.clamp(target.min_x, target.max_x - 1.0), target.min_y + 2.0),
    })
}

/// The cursor's pixel distance from the bottom of the edge span, plus the
/// span's total height. This is the pair carried by `CaptureBegin` and
/// `ReturnToSender` so the far side can map the crossover proportionally.
pub fn from_bottom_of(edge_span: EdgeSpan, return_edge: Edge, x: f64, y: f64) -> (f64, f64) {
    let pos = match return_edge {
        Edge::Left | Edge::Right => y,
        Edge::Top | Edge::Bottom => x,
    };
    let height = edge_span.max - edge_span.min;
    let from_bottom = (edge_span.max - pos).clamp(0.0, height);
    (from_bottom, height)
}

/// Which edge, if any, the cursor reached on this motion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeHit {
    /// Ordinary motion; the cursor stayed on this machine.
    None,
    /// The cursor reached the edge facing the upstream sender.
    Return,
    /// The cursor reached the edge facing the downstream peer.
    Forward,
}

/// One crossover edge: which side it is, which displays own it, and their
/// unioned span along the parallel axis.
#[derive(Debug, Clone)]
pub struct EdgeConfig {
    pub edge: Edge,
    pub displays: Vec<DisplayBounds>,
    pub span: EdgeSpan,
}

impl EdgeConfig {
    pub fn build(all: &[DisplayBounds], edge: Edge) -> Self {
        let displays = edge_displays_of(all, edge);
        let span = span_of_displays(&displays, edge);
        EdgeConfig { edge, displays, span }
    }
}

/// Resolve one motion step: clamp to the display union, then decide whether
/// the cursor reached a crossover edge, pinning it there if so.
///
/// `forward` is `Some` only when the downstream link is healthy. Passing
/// `None` makes this collapse to exactly the return-edge-only behaviour that
/// predates the relay -- the degradation guarantee is therefore structural
/// rather than a promise: a disarmed forward edge is not a branch that might
/// misbehave, it is an argument that is not there.
///
/// The forward edge is tested first. The two edges are required to differ at
/// construction, so no position can satisfy both, and the order only settles
/// the degenerate single-display case where a caller ignored that rule.
pub fn resolve_motion(
    displays: &[DisplayBounds],
    fallback: DisplayBounds,
    ret: &EdgeConfig,
    forward: Option<&EdgeConfig>,
    cursor: (f64, f64),
    delta: (f64, f64),
) -> ((f64, f64), EdgeHit) {
    let target_x = cursor.0 + delta.0;
    let target_y = cursor.1 + delta.1;

    let (cx, cy) = if displays.is_empty() {
        (
            target_x.clamp(fallback.min_x, fallback.max_x - 1.0),
            target_y.clamp(fallback.min_y, fallback.max_y - 1.0),
        )
    } else {
        clamp_to_displays(displays, target_x, target_y)
    };

    if let Some(f) = forward {
        let (fx, fy, hit) = resolve_edge_hit(&f.displays, f.edge, cx, cy);
        if hit {
            return ((fx, fy), EdgeHit::Forward);
        }
    }

    let (rx, ry, hit) = resolve_edge_hit(&ret.displays, ret.edge, cx, cy);
    ((rx, ry), if hit { EdgeHit::Return } else { EdgeHit::None })
}

/// Bounding box over every display. Used only as a fallback when the
/// per-display list is empty.
pub fn bounding_box(displays: &[DisplayBounds]) -> DisplayBounds {
    let mut min_x = f64::MAX;
    let mut min_y = f64::MAX;
    let mut max_x = f64::MIN;
    let mut max_y = f64::MIN;
    for d in displays {
        min_x = min_x.min(d.min_x);
        min_y = min_y.min(d.min_y);
        max_x = max_x.max(d.max_x);
        max_y = max_y.max(d.max_y);
    }
    if min_x >= max_x {
        return DisplayBounds { min_x: 0.0, min_y: 0.0, max_x: 1920.0, max_y: 1080.0 };
    }
    DisplayBounds { min_x, min_y, max_x, max_y }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> DisplayBounds {
        DisplayBounds { min_x, min_y, max_x, max_y }
    }

    // A three-display macOS layout in CG global coordinates:
    //   built-in (main):  x[0,1470]   y[0,956]    -- short bottom edge
    //   22" external:     x[-1503,0]  y[0,1002]   -- taller, defines global max_y
    //   27" portrait:     x[0,1440]   y[-2560,0]  -- stacked above built-in
    // The global bounding box bottom is 1002, so the OLD global clamp let the
    // cursor sail to y=1001 -- 45px below the built-in's real bottom (956),
    // into a phantom band that belongs to no display. The Dock never revealed.
    fn real_layout() -> Vec<DisplayBounds> {
        vec![
            db(0.0, 0.0, 1470.0, 956.0),      // built-in
            db(-1503.0, 0.0, 0.0, 1002.0),    // 22" external (left)
            db(0.0, -2560.0, 1440.0, 0.0),    // 27" portrait (above)
        ]
    }

    // THE BUG: pushing the cursor to the bottom of the built-in overshoots to
    // the global ceiling (y=1001). That point is on no display, so it must be
    // snapped back onto the built-in's real bottom row (y=955), where the Dock
    // can finally see it. x is unchanged because it stays within the built-in.
    #[test]
    fn builtin_bottom_overshoot_snaps_to_real_edge() {
        let d = real_layout();
        let (x, y) = clamp_to_displays(&d, 500.0, 1001.0);
        assert_eq!((x, y), (500.0, 955.0));
    }

    // The left external worked before because its real bottom (1002) equals the
    // global max_y. Confirm the fix keeps it working: a point on its true bottom
    // row is inside the display and passes through untouched.
    #[test]
    fn external_bottom_still_reachable() {
        let d = real_layout();
        let (x, y) = clamp_to_displays(&d, -700.0, 1001.0);
        assert_eq!((x, y), (-700.0, 1001.0));
    }

    // A point comfortably inside the built-in is never perturbed -- the clamp
    // must not interfere with ordinary motion.
    #[test]
    fn interior_point_untouched() {
        let d = real_layout();
        assert_eq!(clamp_to_displays(&d, 700.0, 400.0), (700.0, 400.0));
    }

    // The cursor must move freely across the shared x=0 border between the
    // built-in and the left external at a y both share, with no snapping.
    #[test]
    fn crosses_shared_border_freely() {
        let d = real_layout();
        assert_eq!(clamp_to_displays(&d, -1.0, 400.0), (-1.0, 400.0)); // on external
        assert_eq!(clamp_to_displays(&d, 0.0, 400.0), (0.0, 400.0));   // on built-in
    }

    // Overshooting below the portrait (its bottom is y=0) while horizontally
    // over the built-in region must snap onto the built-in, not strand the
    // cursor in the seam.
    #[test]
    fn portrait_to_builtin_seam_has_no_deadzone() {
        let d = real_layout();
        // y=10 is inside the built-in (x in [0,1470)) -> untouched.
        assert_eq!(clamp_to_displays(&d, 700.0, 10.0), (700.0, 10.0));
    }

    // Empty display list falls back to identity (caller then uses the global
    // box). Guards the degenerate no-display path.
    #[test]
    fn empty_displays_is_identity() {
        assert_eq!(clamp_to_displays(&[], 12.0, 34.0), (12.0, 34.0));
    }

    // Mirrors a real stacked layout reported in the field: a portrait
    // monitor (right edge x=1440, occupying y in [-2560, 0)) sitting above
    // a built-in laptop panel (right edge x=1470, y in [0, 956)). Both own
    // the Right return edge (within EDGE_ALIGN_TOLERANCE of the extreme x
    // of 1470). The global bounding box right edge is 1470, so the caller
    // clamps cursor x to 1469.
    fn stacked_right_edge() -> Vec<DisplayBounds> {
        vec![
            db(0.0, 0.0, 1470.0, 956.0),       // built-in
            db(0.0, -2560.0, 1440.0, 0.0),     // portrait above
        ]
    }

    // The regression: on the portrait, the cursor clamped to the global
    // max_x (1469) lands past the portrait's own right edge (1440). The old
    // code required x to be inside [1439, 1440) to register a hit, so it
    // stuck. The fix must register a hit and pin x back to 1439.
    #[test]
    fn portrait_drifted_past_own_edge_hits_and_pins() {
        let displays = stacked_right_edge();
        let (nx, _ny, hit) = resolve_edge_hit(&displays, Edge::Right, 1469.0, -1000.0);
        assert!(hit, "cursor on portrait at/over its right edge should register a hit");
        assert_eq!(nx, 1439.0, "x should be pinned to the portrait's own outer edge");
    }

    // The built-in panel always worked because its right edge equals the
    // global max_x; confirm the refactor keeps it working.
    #[test]
    fn builtin_at_edge_still_hits() {
        let displays = stacked_right_edge();
        let (nx, _ny, hit) = resolve_edge_hit(&displays, Edge::Right, 1469.0, 500.0);
        assert!(hit);
        assert_eq!(nx, 1469.0);
    }

    // Being on the portrait but not yet at its edge must NOT trigger return,
    // and must not move the cursor.
    #[test]
    fn portrait_not_at_edge_does_not_hit() {
        let displays = stacked_right_edge();
        let (nx, ny, hit) = resolve_edge_hit(&displays, Edge::Right, 700.0, -1000.0);
        assert!(!hit);
        assert_eq!((nx, ny), (700.0, -1000.0));
    }

    // A position whose y falls in the gap between the two stacked displays
    // (the built-in tops out at y=956, nothing owns the edge at y=975) is
    // owned by no edge display, so no hit fires and the cursor is untouched.
    #[test]
    fn gap_between_stacked_displays_does_not_hit() {
        let displays = stacked_right_edge();
        let (nx, ny, hit) = resolve_edge_hit(&displays, Edge::Right, 1469.0, 975.0);
        assert!(!hit);
        assert_eq!((nx, ny), (1469.0, 975.0));
    }

    // Left return edge, single display anchored at x=0: reaching x<=0 pins
    // to 0 and hits.
    #[test]
    fn left_edge_pins_to_min_x() {
        let displays = vec![db(0.0, 0.0, 1440.0, 2560.0)];
        let (nx, _ny, hit) = resolve_edge_hit(&displays, Edge::Left, 0.0, 1000.0);
        assert!(hit);
        assert_eq!(nx, 0.0);
    }

    // Bottom return edge: reaching y>=max_y-1 pins and hits.
    #[test]
    fn bottom_edge_pins_to_max_y() {
        let displays = vec![db(0.0, 0.0, 1920.0, 1080.0)];
        let (_nx, ny, hit) = resolve_edge_hit(&displays, Edge::Bottom, 500.0, 1079.0);
        assert!(hit);
        assert_eq!(ny, 1079.0);
    }

    // --- Coverage for logic newly extracted from the macOS injector. ---
    // These paths existed before but were unreachable from any test that
    // could run off a Mac.

    // Only the displays flush with the extreme edge (within tolerance) own
    // the return edge. The left external's right edge is x=0, nowhere near
    // the extreme of 1470, so it must be excluded.
    #[test]
    fn edge_displays_picks_only_flush_ones() {
        let picked = edge_displays_of(&real_layout(), Edge::Right);
        assert_eq!(picked.len(), 2, "built-in (1470) and portrait (1440) are within 64pt");
        assert!(picked.iter().all(|d| d.max_x >= 1440.0));
    }

    // A display set back further than EDGE_ALIGN_TOLERANCE is not an edge
    // display. 1470 - 1200 = 270 > 64.
    #[test]
    fn edge_displays_excludes_beyond_tolerance() {
        let displays = vec![db(0.0, 0.0, 1470.0, 956.0), db(0.0, -1000.0, 1200.0, 0.0)];
        let picked = edge_displays_of(&displays, Edge::Right);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].max_x, 1470.0);
    }

    // The span unions the perpendicular extent across stacked edge displays:
    // portrait y[-2560,0) plus built-in y[0,956) is a continuous [-2560, 956).
    #[test]
    fn span_unions_stacked_displays() {
        let span = span_of_displays(&stacked_right_edge(), Edge::Right);
        assert_eq!((span.min, span.max), (-2560.0, 956.0));
    }

    // Round trip: a cursor placed from a given from_bottom must report that
    // same from_bottom back. This is the invariant that keeps a crossover and
    // the return crossover landing at the same height.
    #[test]
    fn place_and_report_round_trip() {
        let displays = stacked_right_edge();
        let span = span_of_displays(&displays, Edge::Right);
        for from_bottom in [0.0, 100.0, 956.0, 2000.0, 3516.0] {
            let (x, y) = place_from_bottom(&displays, span, Edge::Right, from_bottom).unwrap();
            let (back, _h) = from_bottom_of(span, Edge::Right, x, y);
            assert!(
                (back - from_bottom).abs() <= 1.0,
                "from_bottom {from_bottom} round-tripped to {back}",
            );
        }
    }

    // Entering at from_bottom=0 means the very bottom of the span, which
    // belongs to the built-in panel, not the portrait.
    #[test]
    fn place_at_bottom_lands_on_builtin() {
        let displays = stacked_right_edge();
        let span = span_of_displays(&displays, Edge::Right);
        let (x, y) = place_from_bottom(&displays, span, Edge::Right, 0.0).unwrap();
        assert_eq!(x, 1468.0, "2pt inset from the built-in's right edge");
        assert_eq!(y, 955.0, "clamped to the built-in's last row");
    }

    // Entering high up the span lands on the portrait, which is the display
    // that actually occupies that y range.
    #[test]
    fn place_high_lands_on_portrait() {
        let displays = stacked_right_edge();
        let span = span_of_displays(&displays, Edge::Right);
        let (x, _y) = place_from_bottom(&displays, span, Edge::Right, 3000.0).unwrap();
        assert_eq!(x, 1438.0, "2pt inset from the portrait's own right edge");
    }

    // No edge displays means no placement is possible; the caller keeps the
    // cursor where it is rather than warping it to a fabricated coordinate.
    #[test]
    fn place_with_no_displays_is_none() {
        let span = EdgeSpan { min: 0.0, max: 1080.0 };
        assert!(place_from_bottom(&[], span, Edge::Right, 100.0).is_none());
    }

    // A from_bottom beyond the span is clamped rather than escaping the
    // display area.
    #[test]
    fn place_clamps_out_of_range_from_bottom() {
        let displays = vec![db(0.0, 0.0, 1920.0, 1080.0)];
        let span = span_of_displays(&displays, Edge::Right);
        let (_x, y) = place_from_bottom(&displays, span, Edge::Right, 99999.0).unwrap();
        assert_eq!(y, 0.0, "clamped to the top of the display");
    }

    #[test]
    fn bounding_box_unions_all() {
        let b = bounding_box(&real_layout());
        assert_eq!((b.min_x, b.min_y, b.max_x, b.max_y), (-1503.0, -2560.0, 1470.0, 1002.0));
    }

    #[test]
    fn bounding_box_empty_is_default() {
        let b = bounding_box(&[]);
        assert_eq!((b.max_x, b.max_y), (1920.0, 1080.0));
    }

    // --- resolve_motion: relay edge handling ---
    //
    // The mac's layout for these: a single display, right edge facing the
    // workstation (return), left edge facing the laptop (forward).

    fn mac() -> Vec<DisplayBounds> {
        vec![db(0.0, 0.0, 1470.0, 956.0)]
    }

    fn cfgs() -> (EdgeConfig, EdgeConfig) {
        let d = mac();
        (
            EdgeConfig::build(&d, Edge::Right),
            EdgeConfig::build(&d, Edge::Left),
        )
    }

    // THE DEGRADATION GUARANTEE. With the downstream link down the forward
    // argument is None, and pushing left must pin the cursor at x=0 with no
    // crossover -- byte-identical to the pre-relay behaviour.
    #[test]
    fn disarmed_forward_edge_pins_and_does_not_cross() {
        let d = mac();
        let (ret, _fwd) = cfgs();
        let (pos, hit) = resolve_motion(&d, bounding_box(&d), &ret, None, (100.0, 500.0), (-500.0, 0.0));
        assert_eq!(hit, EdgeHit::None, "must not cross with the link down");
        assert_eq!(pos.0, 0.0, "cursor pins at the left edge as it does today");
    }

    // Armed, the same motion crosses.
    #[test]
    fn armed_forward_edge_crosses() {
        let d = mac();
        let (ret, fwd) = cfgs();
        let (pos, hit) = resolve_motion(&d, bounding_box(&d), &ret, Some(&fwd), (100.0, 500.0), (-500.0, 0.0));
        assert_eq!(hit, EdgeHit::Forward);
        assert_eq!(pos.0, 0.0);
    }

    // The return edge keeps working regardless of the forward edge's state.
    #[test]
    fn return_edge_unaffected_by_arming() {
        let d = mac();
        let (ret, fwd) = cfgs();
        for forward in [None, Some(&fwd)] {
            let (_pos, hit) = resolve_motion(&d, bounding_box(&d), &ret, forward, (1400.0, 500.0), (500.0, 0.0));
            assert_eq!(hit, EdgeHit::Return, "return edge must fire either way");
        }
    }

    // Interior motion is never a crossover, armed or not.
    #[test]
    fn interior_motion_never_crosses() {
        let d = mac();
        let (ret, fwd) = cfgs();
        let (pos, hit) = resolve_motion(&d, bounding_box(&d), &ret, Some(&fwd), (700.0, 400.0), (10.0, 10.0));
        assert_eq!(hit, EdgeHit::None);
        assert_eq!(pos, (710.0, 410.0));
    }

    // Arming the forward edge must not change where ordinary motion lands.
    #[test]
    fn arming_does_not_perturb_ordinary_motion() {
        let d = mac();
        let (ret, fwd) = cfgs();
        let a = resolve_motion(&d, bounding_box(&d), &ret, None, (700.0, 400.0), (5.0, -5.0));
        let b = resolve_motion(&d, bounding_box(&d), &ret, Some(&fwd), (700.0, 400.0), (5.0, -5.0));
        assert_eq!(a, b);
    }

    // A crossover height must survive the round trip through the forward
    // edge, so leaving and returning land at the same place.
    #[test]
    fn forward_edge_height_round_trips() {
        let d = mac();
        let (_ret, fwd) = cfgs();
        for from_bottom in [0.0, 200.0, 955.0] {
            let (x, y) = place_from_bottom(&fwd.displays, fwd.span, fwd.edge, from_bottom).unwrap();
            let (back, _h) = from_bottom_of(fwd.span, fwd.edge, x, y);
            assert!((back - from_bottom).abs() <= 1.0, "{from_bottom} -> {back}");
        }
    }

    // Stacked displays: the forward edge spans both, mirroring the return
    // edge's behaviour on the same layout.
    #[test]
    fn forward_edge_spans_stacked_displays() {
        let d = vec![
            db(0.0, 0.0, 1470.0, 956.0),
            db(0.0, -2560.0, 1440.0, 0.0),
        ];
        let fwd = EdgeConfig::build(&d, Edge::Left);
        assert_eq!(fwd.displays.len(), 2, "both share the left edge at x=0");
        assert_eq!((fwd.span.min, fwd.span.max), (-2560.0, 956.0));
    }
}

//! Compact vector paths.
//!
//! A path is stored as two arrays sharing one allocation: points fill the
//! buffer from the front, one-byte verbs fill it from the back (16 per slot).
//! A line costs 17 bytes and a cubic 49, instead of the 56 bytes every
//! element of a `Vec<PathElement>` pays for the largest variant. Line-heavy
//! drawings (CAD sheets, maps) are dominated by that difference, and a short
//! `m l S` path still needs exactly one allocation.

use zpdf_core::{Point, Rect};

/// One path segment, produced by value when iterating a [`Path`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PathElement {
    MoveTo(Point),
    LineTo(Point),
    CurveTo(Point, Point, Point),
    Close,
}

const MOVE: u8 = 0;
const LINE: u8 = 1;
const CURVE: u8 = 2;
const CLOSE: u8 = 3;
/// Points consumed by each verb, indexed by verb.
const POINTS: [usize; 4] = [1, 1, 3, 0];
const VERBS_PER_SLOT: usize = 16;
/// First allocation, in slots: an `m l` path or a triangle fits.
const MIN_SLOTS: usize = 4;

/// A vector path in user or page space.
///
/// Build it with [`move_to`](Self::move_to) and friends, [`push`](Self::push)
/// or [`FromIterator`]; read it with [`iter`](Self::iter), which yields
/// [`PathElement`]s by value, or [`points`](Self::points).
pub struct Path {
    /// `points` occupy `slots[..points]`. Verb `i` is byte `i % 16` of
    /// `slots[slots.len() - 1 - i / 16]`, its bits stored in the two `f64`
    /// words. A verb slot starts zeroed and verbs are at most 3, so it never
    /// holds a NaN pattern and plain copies keep its bits. Slots in between
    /// are spare capacity.
    slots: Vec<Point>,
    points: u32,
    verbs: u32,
}

impl Path {
    pub const fn new() -> Self {
        Self {
            slots: Vec::new(),
            points: 0,
            verbs: 0,
        }
    }

    /// An empty path with room for `elements` segments holding `points`
    /// points in total (one per move/line, three per curve, none per close).
    pub fn with_capacity(elements: usize, points: usize) -> Self {
        let mut path = Self::new();
        path.reserve(elements, points);
        path
    }

    /// Reserve room for at least `elements` more segments and `points` more
    /// points. Like [`Vec::reserve`], it may reserve more to keep repeated
    /// appends amortized; an empty path reserves exactly.
    pub fn reserve(&mut self, elements: usize, points: usize) {
        let needed = self.slots_needed(elements, points);
        if needed > self.slots.len() {
            self.reallocate(needed.max(self.slots.len() * 2));
        }
    }

    pub fn move_to(&mut self, p: Point) {
        self.push_verb(MOVE, &[p]);
    }

    pub fn line_to(&mut self, p: Point) {
        self.push_verb(LINE, &[p]);
    }

    pub fn curve_to(&mut self, c1: Point, c2: Point, end: Point) {
        self.push_verb(CURVE, &[c1, c2, end]);
    }

    pub fn close(&mut self) {
        self.push_verb(CLOSE, &[]);
    }

    pub fn rect(&mut self, r: Rect) {
        self.reserve(5, 4);
        self.move_to(Point::new(r.x0, r.y0));
        self.line_to(Point::new(r.x1, r.y0));
        self.line_to(Point::new(r.x1, r.y1));
        self.line_to(Point::new(r.x0, r.y1));
        self.close();
    }

    pub fn push(&mut self, element: PathElement) {
        match element {
            PathElement::MoveTo(p) => self.move_to(p),
            PathElement::LineTo(p) => self.line_to(p),
            PathElement::CurveTo(c1, c2, end) => self.curve_to(c1, c2, end),
            PathElement::Close => self.close(),
        }
    }

    /// Number of segments (elements), not points.
    pub fn len(&self) -> usize {
        self.verbs as usize
    }

    pub fn is_empty(&self) -> bool {
        self.verbs == 0
    }

    /// Every point of every segment, in path order. Curves contribute their
    /// two control points before the end point.
    pub fn points(&self) -> &[Point] {
        &self.slots[..self.points as usize]
    }

    /// Mutable access to the points, e.g. to transform the path in place.
    /// The segment structure cannot change through it.
    pub fn points_mut(&mut self) -> &mut [Point] {
        &mut self.slots[..self.points as usize]
    }

    /// The end point of the last drawing segment (closes are skipped), or
    /// `None` for a path without points.
    pub fn last_point(&self) -> Option<Point> {
        self.points().last().copied()
    }

    pub fn iter(&self) -> PathIter<'_> {
        PathIter {
            path: self,
            front: 0,
            back: self.verbs as usize,
            point_front: 0,
            point_back: self.points as usize,
        }
    }

    /// A copy with every point mapped through `f`, allocated once at its
    /// exact size.
    pub fn map_points(&self, mut f: impl FnMut(Point) -> Point) -> Path {
        let verb_slots = self.verb_slots();
        let mut slots = Vec::with_capacity(self.points as usize + verb_slots);
        slots.extend(self.points().iter().map(|&p| f(p)));
        slots.extend_from_slice(&self.slots[self.slots.len() - verb_slots..]);
        Path {
            slots,
            points: self.points,
            verbs: self.verbs,
        }
    }

    /// Keep the first `len` segments. The allocation is retained.
    pub fn truncate(&mut self, len: usize) {
        if len >= self.len() {
            return;
        }
        let removed: usize = (len..self.len())
            .map(|i| POINTS[self.verb(i) as usize])
            .sum();
        self.points -= removed as u32;
        self.verbs = len as u32;
    }

    /// Remove every segment. The allocation is retained.
    pub fn clear(&mut self) {
        self.points = 0;
        self.verbs = 0;
    }

    fn verb_slots(&self) -> usize {
        (self.verbs as usize).div_ceil(VERBS_PER_SLOT)
    }

    fn slots_needed(&self, elements: usize, points: usize) -> usize {
        let verbs = (self.verbs as usize)
            .checked_add(elements)
            .filter(|&n| n <= u32::MAX as usize);
        let points = (self.points as usize)
            .checked_add(points)
            .filter(|&n| n <= u32::MAX as usize);
        match (verbs, points) {
            (Some(verbs), Some(points)) => points + verbs.div_ceil(VERBS_PER_SLOT),
            _ => panic!("path capacity overflow"),
        }
    }

    /// Move into a buffer of `len` slots: points to its front, verb slots to
    /// its back.
    fn reallocate(&mut self, len: usize) {
        let verb_slots = self.verb_slots();
        let points = self.points as usize;
        let mut slots = Vec::with_capacity(len);
        slots.extend_from_slice(&self.slots[..points]);
        slots.resize(len - verb_slots, Point::zero());
        slots.extend_from_slice(&self.slots[self.slots.len() - verb_slots..]);
        self.slots = slots;
    }

    fn push_verb(&mut self, verb: u8, points: &[Point]) {
        let needed = self.slots_needed(1, points.len());
        if needed > self.slots.len() {
            self.reallocate(needed.max(MIN_SLOTS).max(self.slots.len() * 2));
        }
        let start = self.points as usize;
        self.slots[start..start + points.len()].copy_from_slice(points);
        self.points += points.len() as u32;
        let index = self.verbs as usize;
        let slot = self.slots.len() - 1 - index / VERBS_PER_SLOT;
        let slot = &mut self.slots[slot];
        let byte = index % VERBS_PER_SLOT;
        if byte == 0 {
            // A slot freed by `truncate` or `clear` may still hold a point.
            *slot = Point::zero();
        }
        let word = if byte < 8 { &mut slot.x } else { &mut slot.y };
        let shift = byte % 8 * 8;
        *word = f64::from_bits(word.to_bits() & !(0xFF << shift) | (verb as u64) << shift);
        self.verbs += 1;
    }

    fn verb(&self, index: usize) -> u8 {
        let slot = self.slots[self.slots.len() - 1 - index / VERBS_PER_SLOT];
        let byte = index % VERBS_PER_SLOT;
        let word = if byte < 8 { slot.x } else { slot.y };
        (word.to_bits() >> (byte % 8 * 8)) as u8
    }
}

fn element(verb: u8, points: &[Point]) -> PathElement {
    match verb {
        MOVE => PathElement::MoveTo(points[0]),
        LINE => PathElement::LineTo(points[0]),
        CURVE => PathElement::CurveTo(points[0], points[1], points[2]),
        _ => PathElement::Close,
    }
}

impl Default for Path {
    fn default() -> Self {
        Self::new()
    }
}

/// Clones are allocated at their exact size, without spare capacity.
impl Clone for Path {
    fn clone(&self) -> Self {
        self.map_points(|p| p)
    }
}

impl PartialEq for Path {
    fn eq(&self, other: &Self) -> bool {
        self.verbs == other.verbs
            && self.points() == other.points()
            && (0..self.len()).all(|i| self.verb(i) == other.verb(i))
    }
}

impl std::fmt::Debug for Path {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        struct Elements<'a>(&'a Path);
        impl std::fmt::Debug for Elements<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_list().entries(self.0.iter()).finish()
            }
        }
        f.debug_struct("Path")
            .field("elements", &Elements(self))
            .finish()
    }
}

impl FromIterator<PathElement> for Path {
    fn from_iter<I: IntoIterator<Item = PathElement>>(iter: I) -> Self {
        let mut path = Path::new();
        path.extend(iter);
        path
    }
}

impl Extend<PathElement> for Path {
    fn extend<I: IntoIterator<Item = PathElement>>(&mut self, iter: I) {
        for element in iter {
            self.push(element);
        }
    }
}

impl<'a> IntoIterator for &'a Path {
    type Item = PathElement;
    type IntoIter = PathIter<'a>;

    fn into_iter(self) -> PathIter<'a> {
        self.iter()
    }
}

/// Iterator over a [`Path`]'s segments, by value.
#[derive(Clone)]
pub struct PathIter<'a> {
    path: &'a Path,
    front: usize,
    back: usize,
    point_front: usize,
    point_back: usize,
}

impl Iterator for PathIter<'_> {
    type Item = PathElement;

    fn next(&mut self) -> Option<PathElement> {
        if self.front == self.back {
            return None;
        }
        let verb = self.path.verb(self.front);
        self.front += 1;
        let start = self.point_front;
        self.point_front += POINTS[verb as usize];
        Some(element(verb, &self.path.slots[start..self.point_front]))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.back - self.front;
        (len, Some(len))
    }
}

impl DoubleEndedIterator for PathIter<'_> {
    fn next_back(&mut self) -> Option<PathElement> {
        if self.front == self.back {
            return None;
        }
        self.back -= 1;
        let verb = self.path.verb(self.back);
        let end = self.point_back;
        self.point_back -= POINTS[verb as usize];
        Some(element(verb, &self.path.slots[self.point_back..end]))
    }
}

impl ExactSizeIterator for PathIter<'_> {}

impl std::iter::FusedIterator for PathIter<'_> {}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: f64, y: f64) -> Point {
        Point::new(x, y)
    }

    fn sample() -> Vec<PathElement> {
        vec![
            PathElement::MoveTo(p(0.0, 0.0)),
            PathElement::LineTo(p(1.0, 0.0)),
            PathElement::CurveTo(p(1.0, 1.0), p(0.5, 2.0), p(0.0, 1.0)),
            PathElement::Close,
            PathElement::MoveTo(p(-3.5, f64::MAX)),
            PathElement::Close,
        ]
    }

    #[test]
    fn round_trips_elements_forwards_and_backwards() {
        let path: Path = sample().into_iter().collect();
        assert_eq!(path.len(), 6);
        assert_eq!(path.iter().len(), 6);
        assert_eq!(path.iter().collect::<Vec<_>>(), sample());
        let mut reversed = sample();
        reversed.reverse();
        assert_eq!(path.iter().rev().collect::<Vec<_>>(), reversed);
        assert_eq!(path.last_point(), Some(p(-3.5, f64::MAX)));
        assert_eq!(path.points().len(), 6);
        // Mixed directions meet without double counting.
        let mut iter = path.iter();
        assert_eq!(iter.next(), Some(sample()[0]));
        assert_eq!(iter.next_back(), Some(PathElement::Close));
        assert_eq!(iter.len(), 4);
        assert_eq!(iter.collect::<Vec<_>>(), sample()[1..5]);
    }

    #[test]
    fn verbs_survive_growth_across_many_slots() {
        let mut path = Path::new();
        let mut expected = Vec::new();
        for i in 0..1000 {
            let x = i as f64;
            let element = match i % 4 {
                0 => PathElement::MoveTo(p(x, -x)),
                1 => PathElement::LineTo(p(x, 1.0)),
                2 => PathElement::CurveTo(p(x, 2.0), p(x, 3.0), p(x, 4.0)),
                _ => PathElement::Close,
            };
            path.push(element);
            expected.push(element);
        }
        assert_eq!(path.iter().collect::<Vec<_>>(), expected);
        assert_eq!(path, expected.iter().copied().collect::<Path>());
        let clone = path.clone();
        assert_eq!(clone, path);
        assert_eq!(clone.slots.len(), 1250 + 1000usize.div_ceil(16));

        path.truncate(37);
        assert_eq!(path.iter().collect::<Vec<_>>(), expected[..37]);
        path.line_to(p(9.0, 9.0));
        assert_eq!(
            path.iter().next_back(),
            Some(PathElement::LineTo(p(9.0, 9.0)))
        );
        assert_eq!(path.len(), 38);
        path.clear();
        assert!(path.is_empty() && path.points().is_empty());
    }

    #[test]
    fn reused_slots_hold_only_verb_bits() {
        let mut path = Path::new();
        for _ in 0..10 {
            path.curve_to(p(f64::NAN, 1.0), p(2.0, f64::NAN), p(f64::NAN, f64::NAN));
        }
        path.clear();
        for _ in 0..100 {
            path.close();
        }
        assert!(path.iter().all(|element| element == PathElement::Close));
        let verbs = &path.slots[path.slots.len() - path.verb_slots()..];
        assert!(verbs
            .iter()
            .all(|slot| !slot.x.is_nan() && !slot.y.is_nan()));
    }

    #[test]
    fn line_elements_cost_seventeen_bytes() {
        // Exact-size copies: 16-byte points plus one verb byte per segment,
        // rounded up to whole 16-byte verb slots.
        let mut path = Path::new();
        for i in 0..1600 {
            if i % 2 == 0 {
                path.move_to(p(i as f64, 0.0));
            } else {
                path.line_to(p(i as f64, 1.0));
            }
        }
        let copy = path.clone();
        assert_eq!(copy.slots.capacity() * 16, 1600 * 17);
        // A whole `x y m x y l` stroke fits one 48-byte allocation.
        let mut short = Path::new();
        short.move_to(p(0.0, 0.0));
        short.line_to(p(1.0, 1.0));
        assert_eq!(short.clone().slots.capacity(), 3);
        assert!(std::mem::size_of::<Path>() <= 32);
        assert_eq!(std::mem::size_of::<PathElement>(), 56);
    }

    #[test]
    fn map_points_and_points_mut_keep_structure() {
        let path: Path = sample().into_iter().collect();
        let moved = path.map_points(|q| p(q.x + 1.0, q.y));
        let mut in_place = path.clone();
        for q in in_place.points_mut() {
            q.x += 1.0;
        }
        assert_eq!(moved, in_place);
        assert_eq!(
            moved.iter().nth(2),
            Some(PathElement::CurveTo(p(2.0, 1.0), p(1.5, 2.0), p(1.0, 1.0)))
        );
        assert_ne!(moved, path);
    }

    #[test]
    fn rect_and_debug_match_the_element_list() {
        let mut path = Path::new();
        path.rect(Rect::new(0.0, 0.0, 2.0, 1.0));
        assert_eq!(path.len(), 5);
        assert_eq!(path.slots.len(), 5, "rect reserves exactly once");
        let elements: Vec<_> = path.iter().collect();
        assert_eq!(
            format!("{path:?}"),
            format!("Path {{ elements: {elements:?} }}")
        );
        assert_eq!(Path::default(), Path::new());
    }
}

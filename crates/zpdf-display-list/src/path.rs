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
    /// points. Like [`Vec::reserve`], a path holding segments may reserve
    /// more to keep repeated appends amortized; an empty path, including a
    /// cleared one, reserves exactly.
    pub fn reserve(&mut self, elements: usize, points: usize) {
        let needed = self.slots_needed(elements, points);
        if needed > self.slots.len() {
            let len = if self.is_empty() {
                needed
            } else {
                needed.max(self.slots.len() * 2)
            };
            self.reallocate(len);
        }
    }

    /// Release spare capacity, keeping exactly the space in use.
    pub fn shrink_to_fit(&mut self) {
        let used = self.points as usize + self.verb_slots();
        if used < self.slots.len() {
            self.reallocate(used);
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

    /// Append every segment of `other`. Room is reserved once and the
    /// points are copied as one block.
    pub fn extend_from_path(&mut self, other: &Path) {
        self.reserve(other.len(), other.points().len());
        let start = self.points as usize;
        self.slots[start..start + other.points().len()].copy_from_slice(other.points());
        self.points += other.points;
        for index in 0..other.len() {
            self.write_verb(other.verb(index));
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

    /// The last point stored, i.e. the end point of the last segment other
    /// than a close, or `None` for a path without points. This is not the
    /// PDF current point, which a close (`h`, `re`) returns to the start of
    /// the subpath.
    pub fn last_point(&self) -> Option<Point> {
        self.points().last().copied()
    }

    pub fn iter(&self) -> PathIter<'_> {
        PathIter {
            path: self,
            cursor: Cursor::new(self),
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

    /// Slots needed to hold `elements` and `points` more. Counts are kept as
    /// `u32`; exceeding that, or `usize` on 32-bit targets, panics.
    fn slots_needed(&self, elements: usize, points: usize) -> usize {
        let within = |n: usize| (n <= u32::MAX as usize).then_some(n);
        let verbs = (self.verbs as usize).checked_add(elements).and_then(within);
        let points = (self.points as usize).checked_add(points).and_then(within);
        verbs
            .zip(points)
            .and_then(|(verbs, points)| points.checked_add(verbs.div_ceil(VERBS_PER_SLOT)))
            .expect("path capacity overflow")
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
        self.write_verb(verb);
    }

    /// Append a verb whose slot is already reserved.
    fn write_verb(&mut self, verb: u8) {
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

fn point_count(element: &PathElement) -> usize {
    match element {
        PathElement::MoveTo(_) | PathElement::LineTo(_) => 1,
        PathElement::CurveTo(..) => 3,
        PathElement::Close => 0,
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

/// Lists the segments an iterator has yet to produce.
struct Elements<'a>(PathIter<'a>);

impl std::fmt::Debug for Elements<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.0.clone()).finish()
    }
}

impl std::fmt::Debug for Path {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Path")
            .field("elements", &Elements(self.iter()))
            .finish()
    }
}

/// Exact size: points are counted before allocating.
impl From<Vec<PathElement>> for Path {
    fn from(elements: Vec<PathElement>) -> Self {
        let points = elements.iter().map(point_count).sum();
        let mut path = Path::with_capacity(elements.len(), points);
        for element in elements {
            path.push(element);
        }
        path
    }
}

/// The result is stored at its exact size, like a [`Clone`].
impl FromIterator<PathElement> for Path {
    fn from_iter<I: IntoIterator<Item = PathElement>>(iter: I) -> Self {
        let mut path = Path::new();
        path.extend(iter);
        // The size hint counts segments, not points, so curves can outgrow
        // the estimate and leave growth slack behind.
        path.shrink_to_fit();
        path
    }
}

impl Extend<PathElement> for Path {
    fn extend<I: IntoIterator<Item = PathElement>>(&mut self, iter: I) {
        let iter = iter.into_iter();
        // One point per segment is exact for moves and lines; curves grow
        // the buffer as they arrive.
        let (segments, _) = iter.size_hint();
        self.reserve(segments, segments);
        for element in iter {
            self.push(element);
        }
    }
}

impl<'a> Extend<&'a PathElement> for Path {
    fn extend<I: IntoIterator<Item = &'a PathElement>>(&mut self, iter: I) {
        self.extend(iter.into_iter().copied());
    }
}

impl<'a> IntoIterator for &'a Path {
    type Item = PathElement;
    type IntoIter = PathIter<'a>;

    fn into_iter(self) -> PathIter<'a> {
        self.iter()
    }
}

impl IntoIterator for Path {
    type Item = PathElement;
    type IntoIter = PathIntoIter;

    fn into_iter(self) -> PathIntoIter {
        PathIntoIter {
            cursor: Cursor::new(&self),
            path: self,
        }
    }
}

/// The next segment from each end of a path.
#[derive(Clone)]
struct Cursor {
    front: usize,
    back: usize,
    point_front: usize,
    point_back: usize,
}

impl Cursor {
    fn new(path: &Path) -> Self {
        Self {
            front: 0,
            back: path.len(),
            point_front: 0,
            point_back: path.points().len(),
        }
    }

    fn len(&self) -> usize {
        self.back - self.front
    }

    fn next(&mut self, path: &Path) -> Option<PathElement> {
        if self.front == self.back {
            return None;
        }
        let verb = path.verb(self.front);
        self.front += 1;
        let start = self.point_front;
        self.point_front += POINTS[verb as usize];
        Some(element(verb, &path.slots[start..self.point_front]))
    }

    fn next_back(&mut self, path: &Path) -> Option<PathElement> {
        if self.front == self.back {
            return None;
        }
        self.back -= 1;
        let verb = path.verb(self.back);
        let end = self.point_back;
        self.point_back -= POINTS[verb as usize];
        Some(element(verb, &path.slots[self.point_back..end]))
    }
}

/// Iterator over a [`Path`]'s segments, by value.
#[derive(Clone)]
pub struct PathIter<'a> {
    path: &'a Path,
    cursor: Cursor,
}

impl Iterator for PathIter<'_> {
    type Item = PathElement;

    fn next(&mut self) -> Option<PathElement> {
        self.cursor.next(self.path)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.cursor.len(), Some(self.cursor.len()))
    }
}

impl DoubleEndedIterator for PathIter<'_> {
    fn next_back(&mut self) -> Option<PathElement> {
        self.cursor.next_back(self.path)
    }
}

impl ExactSizeIterator for PathIter<'_> {}

impl std::iter::FusedIterator for PathIter<'_> {}

impl std::fmt::Debug for PathIter<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("PathIter")
            .field(&Elements(self.clone()))
            .finish()
    }
}

/// Owning iterator over a [`Path`]'s segments.
#[derive(Clone)]
pub struct PathIntoIter {
    path: Path,
    cursor: Cursor,
}

impl Iterator for PathIntoIter {
    type Item = PathElement;

    fn next(&mut self) -> Option<PathElement> {
        self.cursor.next(&self.path)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.cursor.len(), Some(self.cursor.len()))
    }
}

impl DoubleEndedIterator for PathIntoIter {
    fn next_back(&mut self) -> Option<PathElement> {
        self.cursor.next_back(&self.path)
    }
}

impl ExactSizeIterator for PathIntoIter {}

impl std::iter::FusedIterator for PathIntoIter {}

impl std::fmt::Debug for PathIntoIter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let remaining = PathIter {
            path: &self.path,
            cursor: self.cursor.clone(),
        };
        f.debug_tuple("PathIntoIter")
            .field(&Elements(remaining))
            .finish()
    }
}

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
        let mut iter = path.iter();
        iter.next();
        assert_eq!(
            format!("{iter:?}"),
            format!("PathIter({:?})", &elements[1..])
        );
        let mut owned = path.clone().into_iter();
        owned.next_back();
        assert_eq!(
            format!("{owned:?}"),
            format!("PathIntoIter({:?})", &elements[..4])
        );
    }

    #[test]
    fn collected_and_converted_paths_are_exact() {
        let curve = PathElement::CurveTo(p(1.0, 2.0), p(3.0, 4.0), p(5.0, 6.0));
        let curves = vec![curve; 1500];
        // 4500 points and 94 verb slots: 73,504 bytes, where a
        // `Vec<PathElement>` holds 84,000.
        let exact = 4500 + 1500usize.div_ceil(16);
        let collected: Path = curves.clone().into_iter().collect();
        assert_eq!(collected.slots.capacity(), exact);
        assert_eq!(Path::from(curves.clone()).slots.capacity(), exact);
        let lines: Path = (0..1500)
            .map(|i| PathElement::LineTo(p(i as f64, 0.0)))
            .collect();
        assert_eq!(lines.slots.capacity(), 1500 + 94);

        let mut joined = Path::from(vec![PathElement::MoveTo(p(0.0, 0.0))]);
        joined.extend_from_path(&collected);
        assert_eq!(joined.len(), 1501);
        assert_eq!(joined.points().len(), 4501);
        assert_eq!(joined.iter().skip(1).collect::<Vec<_>>(), curves);
    }

    #[test]
    fn reserve_on_an_empty_path_is_exact() {
        let mut path = Path::with_capacity(16, 19);
        assert_eq!(path.slots.len(), 20);
        for i in 0..16 {
            path.line_to(p(i as f64, 0.0));
        }
        path.clear();
        path.reserve(16, 20);
        assert_eq!(path.slots.len(), 21, "a cleared path does not double");
        path.line_to(p(0.0, 0.0));
        path.reserve(16, 20);
        assert_eq!(path.slots.len(), 42, "a path holding segments amortizes");
        path.shrink_to_fit();
        assert_eq!(path.slots.len(), 2);
    }

    #[test]
    #[should_panic(expected = "path capacity overflow")]
    fn oversized_reservations_panic_instead_of_wrapping() {
        Path::new().reserve(1, usize::MAX);
    }

    /// The `docs/CHANGELOG.md` migration table, one block per row.
    #[test]
    fn changelog_migration_rows_compile() {
        let elements = sample();
        // `Path { elements }`
        let path = Path::from(elements.clone());
        assert_eq!(path, elements.clone().into_iter().collect::<Path>());
        // `for e in &path.elements { match *e { .. } }`
        let mut closes = 0;
        for e in &path {
            match e {
                PathElement::Close => closes += 1,
                PathElement::MoveTo(_) | PathElement::LineTo(_) | PathElement::CurveTo(..) => {}
            }
        }
        assert_eq!(closes, 2);
        // `for e in path.elements { .. }`
        let mut owned = Vec::new();
        for e in path.clone() {
            owned.push(e);
        }
        assert_eq!(owned, elements);
        // `path.elements.len()`, `path.elements.is_empty()`
        assert_eq!((path.len(), path.is_empty()), (6, false));
        // `path.elements.push(e)`, `path.elements.extend(..)`
        let mut built = Path::new();
        built.push(elements[0]);
        built.extend(&elements[1..3]);
        built.extend(elements[3..].iter().copied());
        assert_eq!(built, path);
        // `path.elements.extend_from_slice(&other.elements)`
        built.extend_from_path(&path);
        assert_eq!(built.len(), 12);
        // Visiting every point
        let mut moved = path.map_points(|q| p(q.x, -q.y));
        for q in moved.points_mut() {
            q.y = -q.y;
        }
        assert_eq!(moved.points(), path.points());
    }

    /// Deterministic SplitMix64, so a failure reproduces from its seed.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }

        fn coordinate(&mut self) -> f64 {
            const SPECIAL: [u64; 12] = [
                0x0000_0000_0000_0000, // +0
                0x8000_0000_0000_0000, // -0
                0x0000_0000_0000_0001, // smallest subnormal
                0x800F_FFFF_FFFF_FFFF, // negative subnormal
                0x7FF0_0000_0000_0000, // +inf
                0xFFF0_0000_0000_0000, // -inf
                0x7FF8_0000_0000_0000, // quiet NaN
                0x7FF0_0000_0000_0001, // signalling NaN
                0xFFF4_0000_DEAD_BEEF, // negative signalling NaN with payload
                0x7FEF_FFFF_FFFF_FFFF, // f64::MAX
                0x3FF0_0000_0000_0000, // 1.0
                0xC08F_4000_0000_0000, // -1000.0
            ];
            match self.below(3) {
                0 => f64::from_bits(SPECIAL[self.below(SPECIAL.len())]),
                1 => f64::from_bits(self.next()),
                _ => self.below(2000) as f64 - 1000.0,
            }
        }

        fn point(&mut self) -> Point {
            Point::new(self.coordinate(), self.coordinate())
        }

        fn element(&mut self) -> PathElement {
            match self.below(4) {
                0 => PathElement::MoveTo(self.point()),
                1 => PathElement::LineTo(self.point()),
                2 => PathElement::CurveTo(self.point(), self.point(), self.point()),
                _ => PathElement::Close,
            }
        }

        fn elements(&mut self, max: usize) -> Vec<PathElement> {
            (0..self.below(max + 1)).map(|_| self.element()).collect()
        }
    }

    fn point_bits(point: Point) -> (u64, u64) {
        (point.x.to_bits(), point.y.to_bits())
    }

    fn element_bits(element: PathElement) -> (u8, Vec<(u64, u64)>) {
        match element {
            PathElement::MoveTo(a) => (MOVE, vec![point_bits(a)]),
            PathElement::LineTo(a) => (LINE, vec![point_bits(a)]),
            PathElement::CurveTo(a, b, c) => {
                (CURVE, vec![point_bits(a), point_bits(b), point_bits(c)])
            }
            PathElement::Close => (CLOSE, Vec::new()),
        }
    }

    fn model_points(model: &[PathElement]) -> Vec<Point> {
        model
            .iter()
            .flat_map(|&element| match element {
                PathElement::MoveTo(a) | PathElement::LineTo(a) => vec![a],
                PathElement::CurveTo(a, b, c) => vec![a, b, c],
                PathElement::Close => Vec::new(),
            })
            .collect()
    }

    /// Replace the model's points in path order with `f(index, point)`.
    fn map_model(model: &mut [PathElement], mut f: impl FnMut(usize, Point) -> Point) {
        let mut index = 0;
        let mut map = |q: &mut Point| {
            *q = f(index, *q);
            index += 1;
        };
        for element in model {
            match element {
                PathElement::MoveTo(a) | PathElement::LineTo(a) => map(a),
                PathElement::CurveTo(a, b, c) => {
                    map(a);
                    map(b);
                    map(c);
                }
                PathElement::Close => {}
            }
        }
    }

    /// Walk from both ends in a random order, checking every element.
    fn walk(
        rng: &mut Rng,
        mut iter: impl DoubleEndedIterator<Item = PathElement> + ExactSizeIterator,
        model: &[PathElement],
    ) {
        let (mut front, mut back) = (0, model.len());
        while front < back {
            assert_eq!(iter.len(), back - front);
            if rng.below(2) == 0 {
                let element = iter.next().unwrap();
                assert_eq!(element_bits(element), element_bits(model[front]));
                front += 1;
            } else {
                let element = iter.next_back().unwrap();
                back -= 1;
                assert_eq!(element_bits(element), element_bits(model[back]));
            }
        }
        assert_eq!(iter.len(), 0);
        assert!(iter.next().is_none() && iter.next_back().is_none());
    }

    fn check(rng: &mut Rng, path: &Path, model: &[PathElement]) {
        assert!(path.points as usize + path.verb_slots() <= path.slots.len());
        assert_eq!(
            (path.len(), path.is_empty()),
            (model.len(), model.is_empty())
        );
        let points = model_points(model);
        let bits: Vec<_> = points.iter().map(|&q| point_bits(q)).collect();
        assert_eq!(
            path.points()
                .iter()
                .map(|&q| point_bits(q))
                .collect::<Vec<_>>(),
            bits
        );
        assert_eq!(path.last_point().map(point_bits), bits.last().copied());
        assert_eq!(
            path.iter().map(element_bits).collect::<Vec<_>>(),
            model.iter().map(|&e| element_bits(e)).collect::<Vec<_>>()
        );
        walk(rng, path.iter(), model);
        walk(rng, path.clone().into_iter(), model);
        assert_eq!(
            format!("{path:?}"),
            format!("Path {{ elements: {model:?} }}")
        );
        let has_nan = points.iter().any(|q| q.x.is_nan() || q.y.is_nan());
        assert_eq!(*path == Path::from(model.to_vec()), !has_nan);
    }

    #[test]
    fn matches_a_vec_model_under_random_operations() {
        for seed in 0..8 {
            let mut rng = Rng(seed);
            let mut path = Path::new();
            let mut model: Vec<PathElement> = Vec::new();
            for _ in 0..600 {
                match rng.below(16) {
                    0..=4 => {
                        let element = rng.element();
                        path.push(element);
                        model.push(element);
                    }
                    5 => {
                        let len = rng.below(model.len() + 3);
                        path.truncate(len);
                        model.truncate(len);
                    }
                    6 if rng.below(4) == 0 => {
                        path.clear();
                        model.clear();
                    }
                    6 => path = path.clone(),
                    7 => {
                        let swap = |_, q: Point| Point::new(q.y, q.x);
                        path = path.map_points(|q| swap(0, q));
                        map_model(&mut model, swap);
                    }
                    8 => path.reserve(rng.below(40), rng.below(80)),
                    9 if !path.points().is_empty() => {
                        let index = rng.below(path.points().len());
                        let point = rng.point();
                        path.points_mut()[index] = point;
                        map_model(&mut model, |i, q| if i == index { point } else { q });
                    }
                    10 => {
                        let mut fresh = Path::with_capacity(
                            rng.below(model.len() + 4),
                            rng.below(2 * model.len() + 4),
                        );
                        if rng.below(2) == 0 {
                            fresh.extend(model.iter().copied());
                        } else {
                            fresh.extend(&model);
                        }
                        path = fresh;
                    }
                    11 => {
                        let other = rng.elements(12);
                        path.extend_from_path(&Path::from(other.clone()));
                        model.extend(other);
                    }
                    12 if model.len() < 200 => {
                        let copy = path.clone();
                        path.extend_from_path(&copy);
                        model.extend_from_within(..);
                    }
                    13 => {
                        let r = Rect::new(
                            rng.coordinate(),
                            rng.coordinate(),
                            rng.coordinate(),
                            rng.coordinate(),
                        );
                        path.rect(r);
                        model.extend([
                            PathElement::MoveTo(Point::new(r.x0, r.y0)),
                            PathElement::LineTo(Point::new(r.x1, r.y0)),
                            PathElement::LineTo(Point::new(r.x1, r.y1)),
                            PathElement::LineTo(Point::new(r.x0, r.y1)),
                            PathElement::Close,
                        ]);
                    }
                    14 => {
                        path = match rng.below(3) {
                            0 => path.into_iter().collect(),
                            1 => Path::from(path.iter().collect::<Vec<_>>()),
                            _ => {
                                path.shrink_to_fit();
                                path
                            }
                        };
                    }
                    _ => {
                        let other = rng.elements(8);
                        path.extend(other.iter());
                        model.extend(other);
                    }
                }
                if model.len() > 400 {
                    path.truncate(100);
                    model.truncate(100);
                }
                check(&mut rng, &path, &model);
            }
        }
    }

    #[test]
    fn reserved_room_is_never_outgrown() {
        let mut rng = Rng(99);
        for _ in 0..300 {
            let prefix = rng.elements(40);
            let tail = rng.elements(60);
            let points = tail.iter().map(point_count).sum();
            let buffer = |path: &Path| (path.slots.as_ptr(), path.slots.len());

            let mut fresh = Path::with_capacity(tail.len(), points);
            let before = buffer(&fresh);
            tail.iter().for_each(|&e| fresh.push(e));
            assert_eq!(buffer(&fresh), before);

            let mut grown = Path::from(prefix.clone());
            grown.reserve(tail.len(), points);
            let before = buffer(&grown);
            tail.iter().for_each(|&e| grown.push(e));
            assert_eq!(buffer(&grown), before);

            let other = Path::from(tail.clone());
            let mut joined = Path::from(prefix);
            joined.reserve(other.len(), other.points().len());
            let before = buffer(&joined);
            joined.extend_from_path(&other);
            assert_eq!(buffer(&joined), before);
            assert_eq!(
                joined.iter().map(element_bits).collect::<Vec<_>>(),
                grown.iter().map(element_bits).collect::<Vec<_>>()
            );
        }
    }
}

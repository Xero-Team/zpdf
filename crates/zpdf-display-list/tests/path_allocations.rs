//! Short paths, the bulk of line-heavy drawings, must not pay a second heap
//! allocation for storing verbs apart from points.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use zpdf_core::{Point, Rect};
use zpdf_display_list::{Path, PathElement};

struct Counting;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call forwards to `System` unchanged; only a counter is added.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocations<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let out = std::hint::black_box(f());
    (out, ALLOCATIONS.load(Ordering::Relaxed) - before)
}

// One test only: the harness runs nothing else in this binary concurrently.
#[test]
fn short_paths_take_one_allocation() {
    let (line, count) = allocations(|| {
        let mut path = Path::new();
        path.move_to(Point::new(0.0, 0.0));
        path.line_to(Point::new(10.0, 5.0));
        path
    });
    assert_eq!(count, 1, "an `m l` path");

    let (rect, count) = allocations(|| {
        let mut path = Path::new();
        path.rect(Rect::new(0.0, 0.0, 4.0, 2.0));
        path
    });
    assert_eq!(count, 1, "a `re` path");

    let (_, count) = allocations(|| line.clone());
    assert_eq!(count, 1, "a clone");
    let (_, count) = allocations(|| rect.map_points(|p| Point::new(p.x * 2.0, p.y)));
    assert_eq!(count, 1, "a transformed copy");
    let (_, count) = allocations(|| rect.iter().filter(|e| *e == PathElement::Close).count());
    assert_eq!(count, 0, "iteration");
    assert_eq!(allocations(Path::new).1, 0, "an empty path");
}

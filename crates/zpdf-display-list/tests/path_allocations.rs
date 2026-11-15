//! Short paths, the bulk of line-heavy drawings, must not pay a second heap
//! allocation for storing verbs apart from points.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use zpdf_core::{Point, Rect};
use zpdf_display_list::{Path, PathElement};

struct Counting;

thread_local! {
    // Even a single-test binary has harness threads that can allocate while the
    // test runs. Const-initialized, non-dropping TLS does not allocate itself.
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

fn record_allocation() {
    // Allocators must not unwind, including during thread-local destruction.
    let _ = ALLOCATIONS.try_with(|count| count.set(count.get().wrapping_add(1)));
}

// SAFETY: every call forwards to `System` unchanged; only a counter is added.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocation();
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_allocation();
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record_allocation();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocations<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let before = ALLOCATIONS.with(Cell::get);
    let out = std::hint::black_box(f());
    (out, ALLOCATIONS.with(Cell::get).wrapping_sub(before))
}

#[test]
fn allocation_counts_exclude_other_threads() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let start = AtomicBool::new(false);
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            while !start.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }
            let allocation = std::hint::black_box(Box::new(42_u64));
            done.store(true, Ordering::Release);
            drop(allocation);
        });
        let (_, count) = allocations(|| {
            start.store(true, Ordering::Release);
            while !done.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }
        });
        worker.join().unwrap();
        assert_eq!(count, 0, "another thread's allocations must not be counted");
    });
}

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

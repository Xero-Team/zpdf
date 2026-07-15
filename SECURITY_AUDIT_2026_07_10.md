# Security Audit Report - 2026-07-10

## Executive Summary

Comprehensive memory safety and performance audit conducted via automated static analysis.

**Findings:**
- **Critical:** 0 (no use-after-free, double-free, or reference cycles)
- **High:** 10 (all DoS/memory exhaustion paths)
- **Medium:** 6 (performance inefficiencies and config inconsistencies)
- **Low:** 2 (edge case handling)

**Root Causes:**
1. Security limits not propagated through all layers (filters, decoders, caches)
2. Per-item limits exist but no cumulative/aggregate budgets
3. Unchecked arithmetic in attacker-controlled size calculations
4. Unbounded caches and stack growth

## High Severity Issues

### H1: Decompression Bomb - Filter Output Bypasses ParseLimits
**Impact:** 1 GiB allocation per filter stage, multi-filter chains can exhaust memory

**Location:** `crates/zpdf-parser/src/filters.rs:32-395`

**Root Cause:** `decode_stream()` has hardcoded 1 GiB limit, ignores `ParseLimits::max_stream_bytes`

**Fix:** Thread `ParseLimits` through filter chain, enforce cumulative budget

---

### H2: JPEG Decoder Allocates Before Validation
**Impact:** Malicious JPEG dimensions bypass image pixel limits

**Location:** `crates/zpdf-parser/src/filters.rs:230`, `crates/zpdf-image/src/lib.rs:156`

**Root Cause:** `zune_jpeg::JpegDecoder::decode()` called before dimension checks

**Fix:** Parse JPEG header first, validate dimensions against budget before decode

---

### H3: Xref Parsing Ignores max_objects
**Impact:** Millions of xref entries + unchecked `u32` arithmetic → panic or wrap

**Location:** `crates/zpdf-parser/src/xref.rs:188-307`

**Root Cause:** Only recovery scan enforces `max_objects`; valid xref table/stream parsing doesn't check

**Fix:** Track cumulative xref entry count, use `checked_add` for object IDs

---

### H4: Unbounded Object Caches
**Impact:** Document-lifetime retention of all decoded objects + object streams

**Location:** `crates/zpdf-parser/src/lib.rs:43-541`

**Root Cause:** `object_cache` and `objstm_cache` have no byte limits, only default 5M entry limit

**Fix:** Add byte-aware LRU with configurable budget

---

### H5: Unbounded Image Cache + Stencil Amplification
**Impact:** Repeated stencil draws create unlimited duplicate 400 MiB images

**Location:** `crates/zpdf-image/src/lib.rs:34-218`, `crates/zpdf-content/src/interpreter.rs:3083`

**Root Cause:** Stencils excluded from dedup cache, inserted fresh each draw

**Fix:** Cache stencils by `(ObjectId, fill_color)`, add total byte budget with LRU

---

### H6: Unbounded Operand Stack
**Impact:** Operand-only streams (no operators) bypass deadline checks, allocate unlimited heap

**Location:** `crates/zpdf-content/src/interpreter.rs:505-734`

**Root Cause:** No operand count/byte limit; deadline sampling tied to operator count

**Fix:** Add `max_operand_stack` limit, sample deadline every N tokens

---

### H7: Graphics/MC Stack Growth
**Impact:** Millions of `q` operators → multi-gigabyte state clones

**Location:** `crates/zpdf-content/src/interpreter.rs:755-1191`

**Root Cause:** No nesting depth limit; configured `max_page_operators` unused

**Fix:** Cap graphics state depth at 256, marked-content depth at 128

---

### H8: CPU Blend Groups Allocate Full Page
**Impact:** Nested transparency groups × 256 MiB/group → rapid OOM

**Location:** `crates/zpdf-render-cpu/src/lib.rs:584-1590`

**Root Cause:** Allocates full page rect, ignores group bounds

**Fix:** Crop to `group_bbox ∩ page_bbox ∩ clip_bbox`, add nesting depth limit

---

### H9: CPU Soft-Mask Cache Unbounded
**Impact:** Each mask is 64 MiB full-page plane, cloned on every use

**Location:** `crates/zpdf-render-cpu/src/lib.rs:38-1527`

**Root Cause:** No byte budget, clones entire plane for offset reuse

**Fix:** Return `Arc<[u8]>` for zero-offset, add byte budget + LRU

---

### H10: GPU Page Allocation Unchecked
**Impact:** Valid dimensions but huge area × MSAA → multi-GiB allocation

**Location:** `crates/zpdf-render-wgpu/src/lib.rs:147`, `crates/zpdf-render-wgpu/src/target.rs:34-94`

**Root Cause:** Only checks max texture dimension per axis, not total pixels or bytes

**Fix:** Add total pixel budget (match CPU 64M), checked arithmetic for staging buffer size

---

## Medium Severity Issues

### M1: Font Cache Not Byte-Bounded
**Location:** `crates/zpdf-font/src/lib.rs:1798`
**Fix:** Replace 256-entry cap with byte budget

### M2: Tiling Pattern Budget Check Skipped
**Location:** `crates/zpdf-content/src/interpreter.rs:4017`
**Fix:** Check `over_budget()` inside tile content loop

### M3: GPU Images Uploaded Simultaneously
**Location:** `crates/zpdf-render-wgpu/src/lib.rs:392`
**Fix:** Add per-page GPU texture byte budget, upload on demand

### M4: GPU Readback Blocks Indefinitely
**Location:** `crates/zpdf-render-wgpu/src/target.rs:185`
**Fix:** Add timeout, provide async API

### M5: Hard-Coded Limits Ignore Caller Config
**Location:** `crates/zpdf-image/src/lib.rs:216`, `crates/zpdf-content/src/interpreter.rs:174`
**Fix:** Pass `ParseLimits` through, eliminate duplicate constants

### M6: Filter Chain Allocation Inefficiency
**Location:** `crates/zpdf-parser/src/filters.rs:27-208`
**Fix:** Reusable scratch buffers, avoid initial clone

---

## Implementation Plan

### Phase 1: Core Budget Infrastructure (Priority 1)
1. ✅ Audit complete
2. ⏳ Create `SecurityBudget` type in `zpdf-core`
3. ⏳ Thread through parser → document → content → render

### Phase 2: Parser & Filter Fixes (H1-H3)
4. ⏳ Filter cumulative output budget
5. ⏳ JPEG header pre-validation
6. ⏳ Xref entry counting + checked arithmetic

### Phase 3: Cache Byte Limits (H4-H5, M1)
7. ⏳ Object cache LRU with byte tracking
8. ⏳ Image cache byte budget + stencil keying
9. ⏳ Font cache byte budget

### Phase 4: Interpreter Stack Limits (H6-H7)
10. ⏳ Operand stack depth + memory tracking
11. ⏳ Graphics state nesting cap
12. ⏳ Marked-content nesting cap

### Phase 5: Renderer Memory Control (H8-H10)
13. ⏳ CPU blend group bbox cropping + nesting limit
14. ⏳ CPU soft-mask Arc sharing + byte budget
15. ⏳ GPU page pixel budget + checked arithmetic
16. ⏳ GPU texture budget

### Phase 6: Config Consistency & Polish (M2-M6, L1-L2)
17. ⏳ Eliminate hard-coded limit duplicates
18. ⏳ Tile budget checking
19. ⏳ Checked ID counters
20. ⏳ Image dimension validation

### Phase 7: Testing & Validation
21. ⏳ Fuzz targets for each limit
22. ⏳ Adversarial test corpus
23. ⏳ Memory profiling benchmarks

---

## Testing Strategy

- **Unit tests:** Each limit boundary (just under, at, just over)
- **Integration tests:** Malicious PDFs targeting each vulnerability
- **Fuzz targets:** cargo-fuzz for parser, filters, content interpreter
- **Benchmarks:** Memory high-water mark tracking

---

## Notes

- No unsafe code audit needed (pure Rust, minimal unsafe in tiny-skia/wgpu deps)
- Path traversal already mitigated in CLI extraction
- RAII prevents resource leaks
- Focus is entirely DoS prevention via resource exhaustion

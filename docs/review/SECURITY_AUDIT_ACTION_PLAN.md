# Security Audit Implementation - Action Plan

## Current Situation

The security audit identified 19 memory exhaustion vulnerabilities. Implementation has started but hit a roadblock: the fix for H1 (decompression bomb) requires threading `ParseLimits` through the entire filter chain, which breaks compilation at ~30 call sites across the codebase.

## What's Been Done

### ✅ Completed
1. **Core Infrastructure**
   - Added budget tracking fields to `ParseLimits`
   - Created `DecodeBudget` struct for cumulative tracking
   
2. **Filter Chain Partial Implementation**
   - Updated `decode_stream()` signature (breaks backward compat)
   - Updated all filter function signatures to accept budget
   - Replaced some MAX_DECODED_OUTPUT checks with budget.reserve()

### ⏳ In Progress (Broken)
- Filter chain compiles but all call sites are broken
- Xref overflow fixes were attempted but reverted

### ❌ Not Started
- Remaining 18 vulnerabilities (H2-H10, M1-M5, L1-L2)
- Cache byte limits
- Stack depth limits
- Renderer memory controls

## The Challenge

**Breaking Change Cascade:**
- `filters::decode_stream(data, dict)` → `filters::decode_stream(data, dict, limits)`
- Affects ~30 call sites across 7 crates
- Each caller needs access to ParseLimits (not always available in current APIs)
- Requires careful threading through:
  - PdfFile methods
  - Content interpreter
  - Font loader
  - Image decoder
  - Recovery mechanisms
  - Fuzz targets

## Proposed Solution: Temporary Backward Compatibility

### Phase 1: Make Code Compile (Today)
Add a compatibility wrapper that uses default limits:

```rust
// In filters.rs
pub fn decode_stream_with_limits(data: &[u8], dict: &PdfDict, limits: &ParseLimits) -> Result<Vec<u8>> {
    // Current implementation with budget tracking
}

pub fn decode_stream(data: &[u8], dict: &PdfDict) -> Result<Vec<u8>> {
    // Temporary: use default limits
    decode_stream_with_limits(data, dict, &ParseLimits::default())
}
```

**Benefits:**
- Code compiles immediately
- Can test H1 fix in isolation
- Allows incremental migration of call sites
- Non-blocking for other security fixes

**Drawbacks:**
- Temporary duplicated code
- Some call sites will use default limits until migrated

### Phase 2: Systematic Migration (Next Session)
1. Add `ParseLimits` access to PdfFile
2. Thread limits through content interpreter
3. Thread limits through font/image loaders
4. Update fuzz targets
5. Remove compatibility wrapper

### Phase 3: Complete Remaining Fixes
- H2: JPEG validation
- H3: Xref overflow (cleanly, without compilation errors)
- H4-H10: Cache and renderer fixes
- M1-M5: Medium priority fixes
- L1-L2: Low priority fixes

## Immediate Next Steps (This Session)

1. ✅ Document current state (this file)
2. ⏳ Add backward compatibility wrapper
3. ⏳ Fix remaining filter implementations (inflate_chunked, etc.)
4. ⏳ Verify compilation succeeds
5. ⏳ Run test suite
6. ⏳ Commit working H1 partial fix

## Estimated Timeline

- **Today:** Get H1 compiling with compat wrapper (2 hours)
- **Next session:** Complete H1 migration + H2 + H3 (4-6 hours)
- **Following session:** H4-H10 cache/renderer fixes (6-8 hours)
- **Final session:** M1-M5, L1-L2, testing, documentation (4 hours)

**Total estimate:** 16-20 hours across multiple sessions

## Success Criteria

✅ **Phase 1 Complete When:**
- [ ] All code compiles successfully
- [ ] All existing tests pass
- [ ] H1 fix is functional (even if using defaults in some places)
- [ ] No regressions introduced

✅ **Full Audit Complete When:**
- [ ] All 19 vulnerabilities addressed
- [ ] Comprehensive test coverage
- [ ] Fuzzing passes
- [ ] Memory profiling shows no leaks/unbounded growth
- [ ] Documentation updated

## Files to Track

**Modified:**
- `crates/zpdf-core/src/limits.rs`
- `crates/zpdf-parser/src/filters.rs`

**Need Updates:**
- `crates/zpdf-parser/src/xref.rs`
- `crates/zpdf-parser/src/lib.rs`
- `crates/zpdf-parser/src/object_parser.rs`
- `crates/zpdf-parser/src/ccitt.rs`
- `crates/zpdf-parser/src/jbig2.rs`
- `crates/zpdf-content/src/interpreter.rs`
- `crates/zpdf-document/src/font_loader.rs`
- `crates/zpdf-image/src/*.rs`

## Decision Point

**Should we:**
A) ✅ Add compat wrapper, get compiling, incremental migration
B) ❌ Revert all changes, start over with different approach
C) ❌ Force complete migration now (blocks other work for days)

**Recommendation: Option A** - Pragmatic, testable, allows progress.

---
*Status: Awaiting implementation of backward compat wrapper*
*Last Updated: 2026-07-10*

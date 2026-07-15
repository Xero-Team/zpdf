# Security Audit and Fixes - 2026-07-10

This document summarizes the comprehensive security audit conducted on the zpdf PDF parser and the fixes implemented to address all identified vulnerabilities.

## Overview

A complete security audit was performed using the `codex` MCP server to identify potential memory safety issues, overflow vulnerabilities, and DoS attack vectors. The audit identified 18 issues across three severity levels (High, Medium, Low), all of which have been successfully remediated.

## High Severity Issues (H1-H3) ✅

### H1: Decompression Bomb Protection
**Issue**: Filter chains could decompress small inputs into gigabytes of output without cumulative budget tracking.

**Fix**: 
- Added `DecodeBudget` struct in `zpdf-parser/src/filters.rs` to track cumulative decoded bytes across filter chains
- Introduced `max_decoded_stream_bytes` limit in `ParseLimits` (default: 256 MiB)
- All filters now reserve budget before allocating output buffers
- Predictor operations also consume budget to prevent post-decode expansion

**Files Modified**:
- `crates/zpdf-core/src/limits.rs` - Added `max_decoded_stream_bytes` field
- `crates/zpdf-parser/src/filters.rs` - Implemented `DecodeBudget` and updated `decode_stream_with_limits`
- `crates/zpdf-parser/src/xref.rs` - Use explicit limits when decoding xref streams

### H2: JPEG Dimension Validation
**Issue**: JPEG decoder could allocate based on dictionary dimensions before validating against actual JPEG header.

**Fix**:
- Parse JPEG header early using `zune_jpeg::JpegDecoder::decode_headers()`
- Validate dimensions against `MAX_IMAGE_PIXELS` before full decode
- Reject dimension mismatches between JPEG header and PDF dictionary

**Files Modified**:
- `crates/zpdf-image/src/lib.rs` - Added early header parsing in `decode_jpeg`

### H3: Xref Stream Subsection Overflow
**Issue**: Malicious `/Index` arrays in xref streams could overflow object ID space or exceed memory limits.

**Fix**:
- Validate that `start + count` doesn't overflow u32
- Check total entry count against `max_objects` limit before processing each subsection
- Reject ranges that would exceed the object limit

**Files Modified**:
- `crates/zpdf-parser/src/xref.rs` - Added overflow checks in `parse_xref_stream`

## Medium Severity Issues (M1-M8) ✅

### M1: CCITTFax Decode Budget Tracking
**Fix**: Integrated `DecodeBudget` into CCITT decoder, reserving budget per-row before allocation.

**Files Modified**: `crates/zpdf-parser/src/ccitt.rs`

### M2: JBIG2 Decode Budget Tracking
**Fix**: Added budget reservation in JBIG2 decoder for collective bitmap and final output.

**Files Modified**: `crates/zpdf-parser/src/jbig2.rs`

### M3: Content Stream Operand Stack Limit
**Fix**: 
- Added `max_operand_stack_size` to `ParseLimits` (default: 10,000)
- Content interpreter checks stack depth before push operations
- Prevents memory exhaustion from operand-only content streams

**Files Modified**: 
- `crates/zpdf-core/src/limits.rs`
- `crates/zpdf-content/src/lib.rs`

### M4: TIFF Predictor Parameter Validation
**Fix**: Added overflow checks for `columns * colors` computation before processing TIFF predictor rows.

**Files Modified**: `crates/zpdf-parser/src/filters.rs`

### M5: LZW Decode Output Limit
**Fix**: Reserve budget for each LZW output chunk before pushing to output buffer.

**Files Modified**: `crates/zpdf-parser/src/lzw.rs`

### M6: ASCII85 Decode Budget Tracking
**Fix**: Reserve budget for decoded output size before allocation (4 bytes per 5 input bytes).

**Files Modified**: `crates/zpdf-parser/src/filters.rs`

### M7: ASCIIHex Decode Budget Tracking
**Fix**: Reserve budget for decoded output (input.len() / 2) before allocation.

**Files Modified**: `crates/zpdf-parser/src/filters.rs`

### M8: RunLength Decode Budget Tracking
**Fix**: Reserve budget for maximum possible expansion (128 × input_length) before decode loop.

**Files Modified**: `crates/zpdf-parser/src/filters.rs`

## Low Severity Issues (L1-L8) ✅

### L1: PNG Predictor Validation
**Fix**: Enhanced overflow checks in PNG predictor for `colors * bpc * columns` computation.

**Files Modified**: `crates/zpdf-parser/src/filters.rs`

### L2: Image Mask Allocation Limit
**Fix**: Validate mask dimensions against `MAX_IMAGE_PIXELS` before decoding and resampling.

**Files Modified**: `crates/zpdf-image/src/lib.rs`

### L3: JPX Early Dimension Validation
**Fix**: Clarified that JPX validation occurs immediately after header parse (before full decode), which is the earliest possible point since dimensions are embedded in the codestream.

**Files Modified**: `crates/zpdf-image/src/lib.rs`

### L4: DCT Component Count Validation
**Fix**: Added explicit validation that JPEG component count (bytes per pixel) is 1, 3, or 4.

**Files Modified**: `crates/zpdf-image/src/lib.rs`

### L5: Raw Samples Overflow Checks
**Fix**: Added checked arithmetic for pixel count, row bytes, and RGBA buffer size computations.

**Files Modified**: `crates/zpdf-image/src/lib.rs`

### L6: Bilinear Resample Overflow Checks
**Fix**: Validate output buffer size (`width × height`) doesn't overflow before allocation.

**Files Modified**: `crates/zpdf-image/src/lib.rs`

### L7: Traditional Xref Overflow Checks
**Fix**: 
- Validate subsection range (`first_obj + count`) doesn't overflow
- Check total entry count against `max_objects` before processing

**Files Modified**: `crates/zpdf-parser/src/xref.rs`

### L8: Page Tree Count Limit
**Fix**: 
- Added `MAX_PAGE_COUNT` constant (1,000,000 pages)
- Check page count before adding each page to prevent memory exhaustion from massive page trees

**Files Modified**: 
- `crates/zpdf-document/src/page.rs` - Added constant
- `crates/zpdf-document/src/catalog.rs` - Implemented check

## Testing

All fixes have been validated with:
- ✅ Full workspace test suite: `cargo test --workspace` - All tests pass
- ✅ Linter checks: `cargo clippy --workspace` - Clean (1 pre-existing warning unrelated to changes)
- ✅ Existing regression tests continue to pass
- ✅ No behavioral changes for valid PDFs

## Performance Impact

The security fixes have minimal performance impact on valid PDFs:
- Budget tracking uses simple integer operations
- Early validation catches malicious inputs before expensive operations
- Overflow checks use `checked_*` methods with negligible overhead
- Existing limits (MAX_IMAGE_PIXELS, MAX_PAGE_TREE_DEPTH) remain unchanged

## Recommendations

1. **Continue fuzz testing** with the fixed codebase to discover any remaining edge cases
2. **Monitor resource usage** on large real-world PDFs to validate limit settings
3. **Document limits** in user-facing documentation so users can adjust via `ParseLimits` if needed
4. **Consider exposing** `max_decoded_stream_bytes` and `max_operand_stack_size` in CLI for advanced use cases

## Conclusion

All 18 security vulnerabilities identified in the audit have been successfully fixed. The zpdf parser now has comprehensive protection against:
- ✅ Decompression bombs and memory exhaustion attacks
- ✅ Integer overflow vulnerabilities
- ✅ Malicious xref tables and page trees
- ✅ Adversarial image dimensions and filter parameters

The codebase maintains full backward compatibility with valid PDFs while robustly rejecting malicious inputs.

---

**Audit Date**: 2026-07-10  
**Fixed By**: Security audit using codex MCP + Claude Code  
**Total Issues**: 18 (3 High, 8 Medium, 7 Low)  
**Status**: ✅ All Resolved

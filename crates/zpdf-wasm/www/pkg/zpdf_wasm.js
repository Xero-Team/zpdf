/**
 * Options for [`optimize`]. Construct with `new OptimizeOptions()`, then set
 * fields. `linearize` and the rewrite path are mutually exclusive — setting
 * `linearize = true` ignores `compress`/`max_image_dim`.
 */
export class OptimizeOptions {
    __destroy_into_raw() {
        const ptr = this.__wbg_ptr;
        this.__wbg_ptr = 0;
        OptimizeOptionsFinalization.unregister(this);
        return ptr;
    }
    free() {
        const ptr = this.__destroy_into_raw();
        wasm.__wbg_optimizeoptions_free(ptr, 0);
    }
    /**
     * @returns {boolean}
     */
    get compress() {
        const ret = wasm.__wbg_get_optimizeoptions_compress(this.__wbg_ptr);
        return ret !== 0;
    }
    /**
     * @returns {boolean}
     */
    get linearize() {
        const ret = wasm.__wbg_get_optimizeoptions_linearize(this.__wbg_ptr);
        return ret !== 0;
    }
    /**
     * @returns {number | undefined}
     */
    get max_image_dim() {
        const ret = wasm.__wbg_get_optimizeoptions_max_image_dim(this.__wbg_ptr);
        return ret === Number.MAX_SAFE_INTEGER ? undefined : ret;
    }
    constructor() {
        const ret = wasm.optimizeoptions_new();
        this.__wbg_ptr = ret;
        OptimizeOptionsFinalization.register(this, this.__wbg_ptr, this);
        return this;
    }
    /**
     * @param {boolean} arg0
     */
    set compress(arg0) {
        wasm.__wbg_set_optimizeoptions_compress(this.__wbg_ptr, arg0);
    }
    /**
     * @param {boolean} arg0
     */
    set linearize(arg0) {
        wasm.__wbg_set_optimizeoptions_linearize(this.__wbg_ptr, arg0);
    }
    /**
     * @param {number | null} [arg0]
     */
    set max_image_dim(arg0) {
        wasm.__wbg_set_optimizeoptions_max_image_dim(this.__wbg_ptr, isLikeNone(arg0) ? Number.MAX_SAFE_INTEGER : (arg0) >>> 0);
    }
}
if (Symbol.dispose) OptimizeOptions.prototype[Symbol.dispose] = OptimizeOptions.prototype.free;

/**
 * A rendered page raster: tightly packed RGBA8, ready for
 * `ImageData`/`putImageData` (the page composites over an opaque background,
 * so premultiplied and straight alpha coincide).
 */
export class PageBitmap {
    static __wrap(ptr) {
        const obj = Object.create(PageBitmap.prototype);
        obj.__wbg_ptr = ptr;
        PageBitmapFinalization.register(obj, obj.__wbg_ptr, obj);
        return obj;
    }
    __destroy_into_raw() {
        const ptr = this.__wbg_ptr;
        this.__wbg_ptr = 0;
        PageBitmapFinalization.unregister(this);
        return ptr;
    }
    free() {
        const ptr = this.__destroy_into_raw();
        wasm.__wbg_pagebitmap_free(ptr, 0);
    }
    /**
     * @returns {number}
     */
    get height() {
        const ret = wasm.pagebitmap_height(this.__wbg_ptr);
        return ret >>> 0;
    }
    /**
     * The RGBA8 pixels (copied out to a `Uint8Array`).
     * @returns {Uint8Array}
     */
    rgba() {
        const ret = wasm.pagebitmap_rgba(this.__wbg_ptr);
        var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v1;
    }
    /**
     * @returns {number}
     */
    get width() {
        const ret = wasm.pagebitmap_width(this.__wbg_ptr);
        return ret >>> 0;
    }
}
if (Symbol.dispose) PageBitmap.prototype[Symbol.dispose] = PageBitmap.prototype.free;

/**
 * An open PDF document.
 */
export class Pdf {
    static __wrap(ptr) {
        const obj = Object.create(Pdf.prototype);
        obj.__wbg_ptr = ptr;
        PdfFinalization.register(obj, obj.__wbg_ptr, obj);
        return obj;
    }
    __destroy_into_raw() {
        const ptr = this.__wbg_ptr;
        this.__wbg_ptr = 0;
        PdfFinalization.unregister(this);
        return ptr;
    }
    free() {
        const ptr = this.__destroy_into_raw();
        wasm.__wbg_pdf_free(ptr, 0);
    }
    /**
     * Open a PDF from bytes (empty-password encrypted files open too).
     * @param {Uint8Array} data
     * @returns {Pdf}
     */
    static open(data) {
        const ptr0 = passArray8ToWasm0(data, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.pdf_open(ptr0, len0);
        if (ret[2]) {
            throw takeFromExternrefTable0(ret[1]);
        }
        return Pdf.__wrap(ret[0]);
    }
    /**
     * Open an encrypted PDF with a user or owner password.
     * @param {Uint8Array} data
     * @param {string} password
     * @returns {Pdf}
     */
    static open_with_password(data, password) {
        const ptr0 = passArray8ToWasm0(data, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passStringToWasm0(password, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len1 = WASM_VECTOR_LEN;
        const ret = wasm.pdf_open_with_password(ptr0, len0, ptr1, len1);
        if (ret[2]) {
            throw takeFromExternrefTable0(ret[1]);
        }
        return Pdf.__wrap(ret[0]);
    }
    /**
     * @returns {number}
     */
    get page_count() {
        const ret = wasm.pdf_page_count(this.__wbg_ptr);
        return ret >>> 0;
    }
    /**
     * `[width, height]` of a page's effective box in PDF points (72 dpi),
     * with `/Rotate` taken into account (90°/270° swap the sides).
     * @param {number} index
     * @returns {Float64Array}
     */
    page_size(index) {
        const ret = wasm.pdf_page_size(this.__wbg_ptr, index);
        if (ret[3]) {
            throw takeFromExternrefTable0(ret[2]);
        }
        var v1 = getArrayF64FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 8, 8);
        return v1;
    }
    /**
     * Export a page as a standalone vector SVG document.
     * @param {number} index
     * @returns {string}
     */
    page_svg(index) {
        let deferred2_0;
        let deferred2_1;
        try {
            const ret = wasm.pdf_page_svg(this.__wbg_ptr, index);
            var ptr1 = ret[0];
            var len1 = ret[1];
            if (ret[3]) {
                ptr1 = 0; len1 = 0;
                throw takeFromExternrefTable0(ret[2]);
            }
            deferred2_0 = ptr1;
            deferred2_1 = len1;
            return getStringFromWasm0(ptr1, len1);
        } finally {
            wasm.__wbindgen_free(deferred2_0, deferred2_1, 1);
        }
    }
    /**
     * Extract a page's text in reading order (struct-tree order when the
     * document is tagged, geometric order otherwise).
     * @param {number} index
     * @returns {string}
     */
    page_text(index) {
        let deferred2_0;
        let deferred2_1;
        try {
            const ret = wasm.pdf_page_text(this.__wbg_ptr, index);
            var ptr1 = ret[0];
            var len1 = ret[1];
            if (ret[3]) {
                ptr1 = 0; len1 = 0;
                throw takeFromExternrefTable0(ret[2]);
            }
            deferred2_0 = ptr1;
            deferred2_1 = len1;
            return getStringFromWasm0(ptr1, len1);
        } finally {
            wasm.__wbindgen_free(deferred2_0, deferred2_1, 1);
        }
    }
    /**
     * Supply a raw TTF/OTF/TTC face; rebuild a page to use newly registered fonts.
     * @param {Uint8Array} data
     * @param {number} face_index
     * @param {string} alias
     * @returns {number}
     */
    register_font(data, face_index, alias) {
        const ptr0 = passArray8ToWasm0(data, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passStringToWasm0(alias, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len1 = WASM_VECTOR_LEN;
        const ret = wasm.pdf_register_font(this.__wbg_ptr, ptr0, len0, face_index, ptr1, len1);
        if (ret[2]) {
            throw takeFromExternrefTable0(ret[1]);
        }
        return ret[0] >>> 0;
    }
    /**
     * Register compressed bytes without decoding until the named face is needed.
     * The host performs fetch/cache operations before calling this method.
     * @param {Uint8Array} data
     * @param {number} face_index
     * @param {string} alias
     * @param {number} decoded_limit
     * @returns {number}
     */
    register_zlib_font(data, face_index, alias, decoded_limit) {
        const ptr0 = passArray8ToWasm0(data, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passStringToWasm0(alias, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len1 = WASM_VECTOR_LEN;
        const ret = wasm.pdf_register_zlib_font(this.__wbg_ptr, ptr0, len0, face_index, ptr1, len1, decoded_limit);
        if (ret[2]) {
            throw takeFromExternrefTable0(ret[1]);
        }
        return ret[0] >>> 0;
    }
    /**
     * Rasterize a page with the CPU backend. `scale` is relative to 72 dpi
     * (i.e. `scale = dpi / 72`), clamped to a sane range.
     * @param {number} index
     * @param {number} scale
     * @returns {PageBitmap}
     */
    render_page(index, scale) {
        const ret = wasm.pdf_render_page(this.__wbg_ptr, index, scale);
        if (ret[2]) {
            throw takeFromExternrefTable0(ret[1]);
        }
        return PageBitmap.__wrap(ret[0]);
    }
    /**
     * Document title from the info dictionary, if any.
     * @returns {string | undefined}
     */
    get title() {
        const ret = wasm.pdf_title(this.__wbg_ptr);
        let v1;
        if (ret[0] !== 0) {
            v1 = getStringFromWasm0(ret[0], ret[1]);
            wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        }
        return v1;
    }
}
if (Symbol.dispose) Pdf.prototype[Symbol.dispose] = Pdf.prototype.free;

/**
 * A mutable PDF editor wrapping an [`zpdf_writer::IncrementalWriter`]. Open a
 * document, queue any number of edits (page ops, metadata, stamps,
 * annotations, redaction, form fill, merge), then call [`PdfEditor::save`] to
 * serialize them as **one incremental update** (the original bytes plus the
 * new revision — the smallest possible output, matching the CLI's
 * composition). Page indices are 0-based (JS convention; the CLI is 1-based).
 *
 * `split_pages` and `optimize` are one-shot free functions instead — they
 * fully rewrite or extract, so they don't fit the incremental-editor model.
 */
export class PdfEditor {
    static __wrap(ptr) {
        const obj = Object.create(PdfEditor.prototype);
        obj.__wbg_ptr = ptr;
        PdfEditorFinalization.register(obj, obj.__wbg_ptr, obj);
        return obj;
    }
    __destroy_into_raw() {
        const ptr = this.__wbg_ptr;
        this.__wbg_ptr = 0;
        PdfEditorFinalization.unregister(this);
        return ptr;
    }
    free() {
        const ptr = this.__destroy_into_raw();
        wasm.__wbg_pdfeditor_free(ptr, 0);
    }
    /**
     * @param {number} page
     * @param {number} x0
     * @param {number} y0
     * @param {number} x1
     * @param {number} y1
     * @param {number} r
     * @param {number} g
     * @param {number} b
     * @param {number | null | undefined} ir
     * @param {number | null | undefined} ig
     * @param {number | null | undefined} ib
     * @param {number} width
     */
    add_circle(page, x0, y0, x1, y1, r, g, b, ir, ig, ib, width) {
        const ret = wasm.pdfeditor_add_circle(this.__wbg_ptr, page, x0, y0, x1, y1, r, g, b, !isLikeNone(ir), isLikeNone(ir) ? 0 : ir, !isLikeNone(ig), isLikeNone(ig) ? 0 : ig, !isLikeNone(ib), isLikeNone(ib) ? 0 : ib, width);
        if (ret[1]) {
            throw takeFromExternrefTable0(ret[0]);
        }
    }
    /**
     * @param {number} page
     * @param {number} x0
     * @param {number} y0
     * @param {number} x1
     * @param {number} y1
     * @param {string} contents
     * @param {number | null} [size]
     * @param {number | null} [r]
     * @param {number | null} [g]
     * @param {number | null} [b]
     */
    add_freetext(page, x0, y0, x1, y1, contents, size, r, g, b) {
        const ptr0 = passStringToWasm0(contents, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.pdfeditor_add_freetext(this.__wbg_ptr, page, x0, y0, x1, y1, ptr0, len0, !isLikeNone(size), isLikeNone(size) ? 0 : size, !isLikeNone(r), isLikeNone(r) ? 0 : r, !isLikeNone(g), isLikeNone(g) ? 0 : g, !isLikeNone(b), isLikeNone(b) ? 0 : b);
        if (ret[1]) {
            throw takeFromExternrefTable0(ret[0]);
        }
    }
    /**
     * @param {number} page
     * @param {number} x0
     * @param {number} y0
     * @param {number} x1
     * @param {number} y1
     * @param {number} r
     * @param {number} g
     * @param {number} b
     */
    add_highlight(page, x0, y0, x1, y1, r, g, b) {
        const ret = wasm.pdfeditor_add_highlight(this.__wbg_ptr, page, x0, y0, x1, y1, r, g, b);
        if (ret[1]) {
            throw takeFromExternrefTable0(ret[0]);
        }
    }
    /**
     * @param {number} page
     * @param {number} x1
     * @param {number} y1
     * @param {number} x2
     * @param {number} y2
     * @param {number} r
     * @param {number} g
     * @param {number} b
     * @param {number} width
     */
    add_line(page, x1, y1, x2, y2, r, g, b, width) {
        const ret = wasm.pdfeditor_add_line(this.__wbg_ptr, page, x1, y1, x2, y2, r, g, b, width);
        if (ret[1]) {
            throw takeFromExternrefTable0(ret[0]);
        }
    }
    /**
     * @param {number} page
     * @param {number} x
     * @param {number} y
     * @param {string} contents
     * @param {number | null} [r]
     * @param {number | null} [g]
     * @param {number | null} [b]
     * @param {string | null} [icon]
     */
    add_note(page, x, y, contents, r, g, b, icon) {
        const ptr0 = passStringToWasm0(contents, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        var ptr1 = isLikeNone(icon) ? 0 : passStringToWasm0(icon, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        var len1 = WASM_VECTOR_LEN;
        const ret = wasm.pdfeditor_add_note(this.__wbg_ptr, page, x, y, ptr0, len0, !isLikeNone(r), isLikeNone(r) ? 0 : r, !isLikeNone(g), isLikeNone(g) ? 0 : g, !isLikeNone(b), isLikeNone(b) ? 0 : b, ptr1, len1);
        if (ret[1]) {
            throw takeFromExternrefTable0(ret[0]);
        }
    }
    /**
     * @param {number} page
     * @param {number} x0
     * @param {number} y0
     * @param {number} x1
     * @param {number} y1
     * @param {number} r
     * @param {number} g
     * @param {number} b
     * @param {number | null | undefined} ir
     * @param {number | null | undefined} ig
     * @param {number | null | undefined} ib
     * @param {number} width
     */
    add_square(page, x0, y0, x1, y1, r, g, b, ir, ig, ib, width) {
        const ret = wasm.pdfeditor_add_square(this.__wbg_ptr, page, x0, y0, x1, y1, r, g, b, !isLikeNone(ir), isLikeNone(ir) ? 0 : ir, !isLikeNone(ig), isLikeNone(ig) ? 0 : ig, !isLikeNone(ib), isLikeNone(ib) ? 0 : ib, width);
        if (ret[1]) {
            throw takeFromExternrefTable0(ret[0]);
        }
    }
    /**
     * @param {number} page
     * @param {number} x0
     * @param {number} y0
     * @param {number} x1
     * @param {number} y1
     * @param {number} r
     * @param {number} g
     * @param {number} b
     */
    add_squiggly(page, x0, y0, x1, y1, r, g, b) {
        const ret = wasm.pdfeditor_add_squiggly(this.__wbg_ptr, page, x0, y0, x1, y1, r, g, b);
        if (ret[1]) {
            throw takeFromExternrefTable0(ret[0]);
        }
    }
    /**
     * @param {number} page
     * @param {number} x0
     * @param {number} y0
     * @param {number} x1
     * @param {number} y1
     * @param {number} r
     * @param {number} g
     * @param {number} b
     */
    add_strikeout(page, x0, y0, x1, y1, r, g, b) {
        const ret = wasm.pdfeditor_add_strikeout(this.__wbg_ptr, page, x0, y0, x1, y1, r, g, b);
        if (ret[1]) {
            throw takeFromExternrefTable0(ret[0]);
        }
    }
    /**
     * @param {number} page
     * @param {number} x0
     * @param {number} y0
     * @param {number} x1
     * @param {number} y1
     * @param {number} r
     * @param {number} g
     * @param {number} b
     */
    add_underline(page, x0, y0, x1, y1, r, g, b) {
        const ret = wasm.pdfeditor_add_underline(this.__wbg_ptr, page, x0, y0, x1, y1, r, g, b);
        if (ret[1]) {
            throw takeFromExternrefTable0(ret[0]);
        }
    }
    /**
     * Delete the given (0-based) pages. Deleting every page is an error.
     * @param {Uint32Array} pages
     */
    delete_pages(pages) {
        const ptr0 = passArray32ToWasm0(pages, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.pdfeditor_delete_pages(this.__wbg_ptr, ptr0, len0);
        if (ret[1]) {
            throw takeFromExternrefTable0(ret[0]);
        }
    }
    /**
     * Fill AcroForm fields by fully-qualified name. `names` and `values` are
     * parallel arrays (wasm-bindgen has no `Vec<(String,String)>` without
     * serde). Errors if the document has no AcroForm, or on a signature field.
     * @param {string[]} names
     * @param {string[]} values
     */
    fill_fields(names, values) {
        const ptr0 = passArrayJsValueToWasm0(names, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passArrayJsValueToWasm0(values, wasm.__wbindgen_malloc);
        const len1 = WASM_VECTOR_LEN;
        const ret = wasm.pdfeditor_fill_fields(this.__wbg_ptr, ptr0, len0, ptr1, len1);
        if (ret[1]) {
            throw takeFromExternrefTable0(ret[0]);
        }
    }
    /**
     * Append all pages of `other` to this document. Returns the number of
     * pages appended. Call `save()` to materialize.
     * @param {Uint8Array} other
     * @returns {number}
     */
    merge(other) {
        const ptr0 = passArray8ToWasm0(other, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.pdfeditor_merge(this.__wbg_ptr, ptr0, len0);
        if (ret[2]) {
            throw takeFromExternrefTable0(ret[1]);
        }
        return ret[0] >>> 0;
    }
    /**
     * Open a PDF for editing (empty-password encrypted files open too).
     * @param {Uint8Array} data
     * @returns {PdfEditor}
     */
    static open(data) {
        const ptr0 = passArray8ToWasm0(data, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.pdfeditor_open(ptr0, len0);
        if (ret[2]) {
            throw takeFromExternrefTable0(ret[1]);
        }
        return PdfEditor.__wrap(ret[0]);
    }
    /**
     * Open an encrypted PDF with a user or owner password.
     * @param {Uint8Array} data
     * @param {string} password
     * @returns {PdfEditor}
     */
    static open_with_password(data, password) {
        const ptr0 = passArray8ToWasm0(data, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passStringToWasm0(password, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len1 = WASM_VECTOR_LEN;
        const ret = wasm.pdfeditor_open_with_password(ptr0, len0, ptr1, len1);
        if (ret[2]) {
            throw takeFromExternrefTable0(ret[1]);
        }
        return PdfEditor.__wrap(ret[0]);
    }
    /**
     * @returns {number}
     */
    get page_count() {
        const ret = wasm.pdfeditor_page_count(this.__wbg_ptr);
        return ret >>> 0;
    }
    /**
     * Redact regions of a page: `rects` is a flat `[x0,y0,x1,y1, …]` array;
     * `fill` is an optional `[r,g,b]` cover colour (pass `null` to remove
     * content without painting a box).
     * @param {number} page
     * @param {Float64Array} rects
     * @param {Float64Array | null} [fill]
     */
    redact(page, rects, fill) {
        const ptr0 = passArrayF64ToWasm0(rects, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        var ptr1 = isLikeNone(fill) ? 0 : passArrayF64ToWasm0(fill, wasm.__wbindgen_malloc);
        var len1 = WASM_VECTOR_LEN;
        const ret = wasm.pdfeditor_redact(this.__wbg_ptr, page, ptr0, len0, ptr1, len1);
        if (ret[1]) {
            throw takeFromExternrefTable0(ret[0]);
        }
    }
    /**
     * Register an authoring font for Unicode form values and text stamps.
     * @param {Uint8Array} bytes
     * @param {number} face_index
     * @returns {number}
     */
    register_font(bytes, face_index) {
        const ptr0 = passArray8ToWasm0(bytes, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.pdfeditor_register_font(this.__wbg_ptr, ptr0, len0, face_index);
        if (ret[2]) {
            throw takeFromExternrefTable0(ret[1]);
        }
        return ret[0] >>> 0;
    }
    /**
     * Reorder pages: `order[i]` is the current position of the page that becomes
     * new page `i` (a full permutation of `0..page_count`).
     * @param {Uint32Array} order
     */
    reorder_pages(order) {
        const ptr0 = passArray32ToWasm0(order, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.pdfeditor_reorder_pages(this.__wbg_ptr, ptr0, len0);
        if (ret[1]) {
            throw takeFromExternrefTable0(ret[0]);
        }
    }
    /**
     * Set a page's absolute rotation to a multiple of 90 degrees.
     * @param {number} page
     * @param {number} degrees
     */
    rotate_page(page, degrees) {
        const ret = wasm.pdfeditor_rotate_page(this.__wbg_ptr, page, degrees);
        if (ret[1]) {
            throw takeFromExternrefTable0(ret[0]);
        }
    }
    /**
     * Serialize all queued edits as one incremental update → `Uint8Array`.
     * @returns {Uint8Array}
     */
    save() {
        const ret = wasm.pdfeditor_save(this.__wbg_ptr);
        if (ret[3]) {
            throw takeFromExternrefTable0(ret[2]);
        }
        var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v1;
    }
    /**
     * @param {string | null} [value]
     */
    set_author(value) {
        var ptr0 = isLikeNone(value) ? 0 : passStringToWasm0(value, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        var len0 = WASM_VECTOR_LEN;
        const ret = wasm.pdfeditor_set_author(this.__wbg_ptr, ptr0, len0);
        if (ret[1]) {
            throw takeFromExternrefTable0(ret[0]);
        }
    }
    /**
     * @param {string | null} [value]
     */
    set_creator(value) {
        var ptr0 = isLikeNone(value) ? 0 : passStringToWasm0(value, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        var len0 = WASM_VECTOR_LEN;
        const ret = wasm.pdfeditor_set_creator(this.__wbg_ptr, ptr0, len0);
        if (ret[1]) {
            throw takeFromExternrefTable0(ret[0]);
        }
    }
    /**
     * @param {string | null} [value]
     */
    set_keywords(value) {
        var ptr0 = isLikeNone(value) ? 0 : passStringToWasm0(value, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        var len0 = WASM_VECTOR_LEN;
        const ret = wasm.pdfeditor_set_keywords(this.__wbg_ptr, ptr0, len0);
        if (ret[1]) {
            throw takeFromExternrefTable0(ret[0]);
        }
    }
    /**
     * @param {string | null} [value]
     */
    set_producer(value) {
        var ptr0 = isLikeNone(value) ? 0 : passStringToWasm0(value, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        var len0 = WASM_VECTOR_LEN;
        const ret = wasm.pdfeditor_set_producer(this.__wbg_ptr, ptr0, len0);
        if (ret[1]) {
            throw takeFromExternrefTable0(ret[0]);
        }
    }
    /**
     * @param {string | null} [value]
     */
    set_subject(value) {
        var ptr0 = isLikeNone(value) ? 0 : passStringToWasm0(value, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        var len0 = WASM_VECTOR_LEN;
        const ret = wasm.pdfeditor_set_subject(this.__wbg_ptr, ptr0, len0);
        if (ret[1]) {
            throw takeFromExternrefTable0(ret[0]);
        }
    }
    /**
     * @param {string | null} [value]
     */
    set_title(value) {
        var ptr0 = isLikeNone(value) ? 0 : passStringToWasm0(value, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        var len0 = WASM_VECTOR_LEN;
        const ret = wasm.pdfeditor_set_title(this.__wbg_ptr, ptr0, len0);
        if (ret[1]) {
            throw takeFromExternrefTable0(ret[0]);
        }
    }
    /**
     * Stamp a raw RGBA8 image on a page. `pixels` is tightly packed
     * `width*height*4` bytes; the alpha channel becomes a `/SMask`.
     * @param {number} page
     * @param {Uint8Array} pixels
     * @param {number} width
     * @param {number} height
     * @param {number} x
     * @param {number} y
     * @param {number} w
     * @param {number} h
     */
    stamp_image_rgba(page, pixels, width, height, x, y, w, h) {
        const ptr0 = passArray8ToWasm0(pixels, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.pdfeditor_stamp_image_rgba(this.__wbg_ptr, page, ptr0, len0, width, height, x, y, w, h);
        if (ret[1]) {
            throw takeFromExternrefTable0(ret[0]);
        }
    }
    /**
     * Stamp a text string on a page in a Standard-14 font. `r/g/b` are
     * DeviceRGB in `[0,1]`; `font` is a Standard-14 name (e.g. `"Helvetica"`).
     * @param {number} page
     * @param {string} text
     * @param {number} x
     * @param {number} y
     * @param {string} font
     * @param {number} size
     * @param {number} r
     * @param {number} g
     * @param {number} b
     */
    stamp_text(page, text, x, y, font, size, r, g, b) {
        const ptr0 = passStringToWasm0(text, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passStringToWasm0(font, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len1 = WASM_VECTOR_LEN;
        const ret = wasm.pdfeditor_stamp_text(this.__wbg_ptr, page, ptr0, len0, x, y, ptr1, len1, size, r, g, b);
        if (ret[1]) {
            throw takeFromExternrefTable0(ret[0]);
        }
    }
}
if (Symbol.dispose) PdfEditor.prototype[Symbol.dispose] = PdfEditor.prototype.free;

/**
 * Garbage-collect / recompress / downsample a PDF, or linearize it for fast
 * web view. (Encryption and PDF/A conversion are not exposed to wasm in this
 * round — they need more option types; use the native facade for those.)
 * @param {Uint8Array} data
 * @param {OptimizeOptions} opts
 * @returns {Uint8Array}
 */
export function optimize(data, opts) {
    const ptr0 = passArray8ToWasm0(data, wasm.__wbindgen_malloc);
    const len0 = WASM_VECTOR_LEN;
    _assertClass(opts, OptimizeOptions);
    var ptr1 = opts.__destroy_into_raw();
    const ret = wasm.optimize(ptr0, len0, ptr1);
    if (ret[3]) {
        throw takeFromExternrefTable0(ret[2]);
    }
    var v3 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
    wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
    return v3;
}

/**
 * Extract selected (0-based) pages into a fresh self-contained PDF.
 * @param {Uint8Array} data
 * @param {Uint32Array} pages
 * @returns {Uint8Array}
 */
export function split_pages(data, pages) {
    const ptr0 = passArray8ToWasm0(data, wasm.__wbindgen_malloc);
    const len0 = WASM_VECTOR_LEN;
    const ptr1 = passArray32ToWasm0(pages, wasm.__wbindgen_malloc);
    const len1 = WASM_VECTOR_LEN;
    const ret = wasm.split_pages(ptr0, len0, ptr1, len1);
    if (ret[3]) {
        throw takeFromExternrefTable0(ret[2]);
    }
    var v3 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
    wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
    return v3;
}

/**
 * One-time module init: surface panics as readable console errors.
 */
export function start() {
    wasm.start();
}
function __wbg_get_imports() {
    const import0 = {
        __proto__: null,
        __wbg_Error_30c8987f7c2ed4e2: function(arg0, arg1) {
            const ret = Error(getStringFromWasm0(arg0, arg1));
            return ret;
        },
        __wbg___wbindgen_is_function_1f9d30630b8b1d3d: function(arg0) {
            const ret = typeof(arg0) === 'function';
            return ret;
        },
        __wbg___wbindgen_is_object_3c45d4f2dde4e749: function(arg0) {
            const val = arg0;
            const ret = typeof(val) === 'object' && val !== null;
            return ret;
        },
        __wbg___wbindgen_is_string_90b56bc79aad6f6c: function(arg0) {
            const ret = typeof(arg0) === 'string';
            return ret;
        },
        __wbg___wbindgen_is_undefined_8865fb403f8fe9d8: function(arg0) {
            const ret = arg0 === undefined;
            return ret;
        },
        __wbg___wbindgen_string_get_0380ccaa2f57f0d9: function(arg0, arg1) {
            const obj = arg1;
            const ret = typeof(obj) === 'string' ? obj : undefined;
            var ptr1 = isLikeNone(ret) ? 0 : passStringToWasm0(ret, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
            var len1 = WASM_VECTOR_LEN;
            getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
            getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
        },
        __wbg___wbindgen_throw_41e9ee4f547fc59a: function(arg0, arg1) {
            throw new Error(getStringFromWasm0(arg0, arg1));
        },
        __wbg_call_187d372bd5fdd4aa: function() { return handleError(function (arg0, arg1, arg2) {
            const ret = arg0.call(arg1, arg2);
            return ret;
        }, arguments); },
        __wbg_crypto_38df2bab126b63dc: function(arg0) {
            const ret = arg0.crypto;
            return ret;
        },
        __wbg_error_757e9472f8410341: function(arg0, arg1) {
            let deferred0_0;
            let deferred0_1;
            try {
                deferred0_0 = arg0;
                deferred0_1 = arg1;
                console.error(getStringFromWasm0(arg0, arg1));
            } finally {
                wasm.__wbindgen_free(deferred0_0, deferred0_1, 1);
            }
        },
        __wbg_getRandomValues_c44a50d8cfdaebeb: function() { return handleError(function (arg0, arg1) {
            arg0.getRandomValues(arg1);
        }, arguments); },
        __wbg_length_7f3c00c40364105e: function(arg0) {
            const ret = arg0.length;
            return ret;
        },
        __wbg_msCrypto_bd5a034af96bcba6: function(arg0) {
            const ret = arg0.msCrypto;
            return ret;
        },
        __wbg_new_227d7c05414eb861: function() {
            const ret = new Error();
            return ret;
        },
        __wbg_new_with_length_3da0ad195f6f63ba: function(arg0) {
            const ret = new Uint8Array(arg0 >>> 0);
            return ret;
        },
        __wbg_node_84ea875411254db1: function(arg0) {
            const ret = arg0.node;
            return ret;
        },
        __wbg_process_44c7a14e11e9f69e: function(arg0) {
            const ret = arg0.process;
            return ret;
        },
        __wbg_prototypesetcall_bc27214492979395: function(arg0, arg1, arg2) {
            Uint8Array.prototype.set.call(getArrayU8FromWasm0(arg0, arg1), arg2);
        },
        __wbg_randomFillSync_6c25eac9869eb53c: function() { return handleError(function (arg0, arg1) {
            arg0.randomFillSync(arg1);
        }, arguments); },
        __wbg_require_b4edbdcf3e2a1ef0: function() { return handleError(function () {
            const ret = module.require;
            return ret;
        }, arguments); },
        __wbg_stack_3b0d974bbf31e44f: function(arg0, arg1) {
            const ret = arg1.stack;
            const ptr1 = passStringToWasm0(ret, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
            const len1 = WASM_VECTOR_LEN;
            getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
            getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
        },
        __wbg_static_accessor_GLOBAL_266715b9d96ba635: function() {
            const ret = typeof global === 'undefined' ? null : global;
            return isLikeNone(ret) ? 0 : addToExternrefTable0(ret);
        },
        __wbg_static_accessor_GLOBAL_THIS_10fb7dc1ae063179: function() {
            const ret = typeof globalThis === 'undefined' ? null : globalThis;
            return isLikeNone(ret) ? 0 : addToExternrefTable0(ret);
        },
        __wbg_static_accessor_SELF_0b583911f537483a: function() {
            const ret = typeof self === 'undefined' ? null : self;
            return isLikeNone(ret) ? 0 : addToExternrefTable0(ret);
        },
        __wbg_static_accessor_WINDOW_d7f903d1508cbdc4: function() {
            const ret = typeof window === 'undefined' ? null : window;
            return isLikeNone(ret) ? 0 : addToExternrefTable0(ret);
        },
        __wbg_subarray_002b94d5e13d1411: function(arg0, arg1, arg2) {
            const ret = arg0.subarray(arg1 >>> 0, arg2 >>> 0);
            return ret;
        },
        __wbg_versions_276b2795b1c6a219: function(arg0) {
            const ret = arg0.versions;
            return ret;
        },
        __wbindgen_generic_0000000000000001: function(arg0, arg1) {
            // Cast intrinsic for `Ref(Slice(U8)) -> NamedExternref("Uint8Array")`.
            const ret = getArrayU8FromWasm0(arg0, arg1);
            return ret;
        },
        __wbindgen_generic_0000000000000002: function(arg0, arg1) {
            // Cast intrinsic for `Ref(String) -> Externref`.
            const ret = getStringFromWasm0(arg0, arg1);
            return ret;
        },
        __wbindgen_init_externref_table: function() {
            const table = wasm.__wbindgen_externrefs;
            const offset = table.grow(4);
            table.set(0, undefined);
            table.set(offset + 0, undefined);
            table.set(offset + 1, null);
            table.set(offset + 2, true);
            table.set(offset + 3, false);
        },
    };
    return {
        __proto__: null,
        "./zpdf_wasm_bg.js": import0,
    };
}

const OptimizeOptionsFinalization = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(ptr => wasm.__wbg_optimizeoptions_free(ptr, 1));
const PageBitmapFinalization = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(ptr => wasm.__wbg_pagebitmap_free(ptr, 1));
const PdfFinalization = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(ptr => wasm.__wbg_pdf_free(ptr, 1));
const PdfEditorFinalization = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(ptr => wasm.__wbg_pdfeditor_free(ptr, 1));

function addToExternrefTable0(obj) {
    const idx = wasm.__externref_table_alloc();
    wasm.__wbindgen_externrefs.set(idx, obj);
    return idx;
}

function _assertClass(instance, klass) {
    if (!(instance instanceof klass)) {
        throw new Error(`expected instance of ${klass.name}`);
    }
}

function getArrayF64FromWasm0(ptr, len) {
    ptr = ptr >>> 0;
    return getFloat64ArrayMemory0().subarray(ptr / 8, ptr / 8 + len);
}

function getArrayU8FromWasm0(ptr, len) {
    ptr = ptr >>> 0;
    return getUint8ArrayMemory0().subarray(ptr / 1, ptr / 1 + len);
}

let cachedDataViewMemory0 = null;
function getDataViewMemory0() {
    if (cachedDataViewMemory0 === null || cachedDataViewMemory0.buffer.detached === true || (cachedDataViewMemory0.buffer.detached === undefined && cachedDataViewMemory0.buffer !== wasm.memory.buffer)) {
        cachedDataViewMemory0 = new DataView(wasm.memory.buffer);
    }
    return cachedDataViewMemory0;
}

let cachedFloat64ArrayMemory0 = null;
function getFloat64ArrayMemory0() {
    if (cachedFloat64ArrayMemory0 === null || cachedFloat64ArrayMemory0.byteLength === 0) {
        cachedFloat64ArrayMemory0 = new Float64Array(wasm.memory.buffer);
    }
    return cachedFloat64ArrayMemory0;
}

function getStringFromWasm0(ptr, len) {
    return decodeText(ptr >>> 0, len);
}

let cachedUint32ArrayMemory0 = null;
function getUint32ArrayMemory0() {
    if (cachedUint32ArrayMemory0 === null || cachedUint32ArrayMemory0.byteLength === 0) {
        cachedUint32ArrayMemory0 = new Uint32Array(wasm.memory.buffer);
    }
    return cachedUint32ArrayMemory0;
}

let cachedUint8ArrayMemory0 = null;
function getUint8ArrayMemory0() {
    if (cachedUint8ArrayMemory0 === null || cachedUint8ArrayMemory0.byteLength === 0) {
        cachedUint8ArrayMemory0 = new Uint8Array(wasm.memory.buffer);
    }
    return cachedUint8ArrayMemory0;
}

function handleError(f, args) {
    try {
        return f.apply(this, args);
    } catch (e) {
        const idx = addToExternrefTable0(e);
        wasm.__wbindgen_exn_store(idx);
    }
}

function isLikeNone(x) {
    return x === undefined || x === null;
}

function passArray32ToWasm0(arg, malloc) {
    const ptr = malloc(arg.length * 4, 4) >>> 0;
    getUint32ArrayMemory0().set(arg, ptr / 4);
    WASM_VECTOR_LEN = arg.length;
    return ptr;
}

function passArray8ToWasm0(arg, malloc) {
    const ptr = malloc(arg.length * 1, 1) >>> 0;
    getUint8ArrayMemory0().set(arg, ptr / 1);
    WASM_VECTOR_LEN = arg.length;
    return ptr;
}

function passArrayF64ToWasm0(arg, malloc) {
    const ptr = malloc(arg.length * 8, 8) >>> 0;
    getFloat64ArrayMemory0().set(arg, ptr / 8);
    WASM_VECTOR_LEN = arg.length;
    return ptr;
}

function passArrayJsValueToWasm0(array, malloc) {
    const ptr = malloc(array.length * 4, 4) >>> 0;
    for (let i = 0; i < array.length; i++) {
        const add = addToExternrefTable0(array[i]);
        getDataViewMemory0().setUint32(ptr + 4 * i, add, true);
    }
    WASM_VECTOR_LEN = array.length;
    return ptr;
}

function passStringToWasm0(arg, malloc, realloc) {
    if (realloc === undefined) {
        const buf = cachedTextEncoder.encode(arg);
        const ptr = malloc(buf.length, 1) >>> 0;
        getUint8ArrayMemory0().subarray(ptr, ptr + buf.length).set(buf);
        WASM_VECTOR_LEN = buf.length;
        return ptr;
    }

    let len = arg.length;
    let ptr = malloc(len, 1) >>> 0;

    const mem = getUint8ArrayMemory0();

    let offset = 0;

    for (; offset < len; offset++) {
        const code = arg.charCodeAt(offset);
        if (code > 0x7F) break;
        mem[ptr + offset] = code;
    }
    if (offset !== len) {
        if (offset !== 0) {
            arg = arg.slice(offset);
        }
        ptr = realloc(ptr, len, len = offset + arg.length * 3, 1) >>> 0;
        const view = getUint8ArrayMemory0().subarray(ptr + offset, ptr + len);
        const ret = cachedTextEncoder.encodeInto(arg, view);

        offset += ret.written;
        ptr = realloc(ptr, len, offset, 1) >>> 0;
    }

    WASM_VECTOR_LEN = offset;
    return ptr;
}

function takeFromExternrefTable0(idx) {
    const value = wasm.__wbindgen_externrefs.get(idx);
    wasm.__externref_table_dealloc(idx);
    return value;
}

let cachedTextDecoder = new TextDecoder('utf-8', { ignoreBOM: true, fatal: true });
cachedTextDecoder.decode();
const MAX_SAFARI_DECODE_BYTES = 2146435072;
let numBytesDecoded = 0;
function decodeText(ptr, len) {
    numBytesDecoded += len;
    if (numBytesDecoded >= MAX_SAFARI_DECODE_BYTES) {
        cachedTextDecoder = new TextDecoder('utf-8', { ignoreBOM: true, fatal: true });
        cachedTextDecoder.decode();
        numBytesDecoded = len;
    }
    return cachedTextDecoder.decode(getUint8ArrayMemory0().subarray(ptr, ptr + len));
}

const cachedTextEncoder = new TextEncoder();

if (!('encodeInto' in cachedTextEncoder)) {
    cachedTextEncoder.encodeInto = function (arg, view) {
        const buf = cachedTextEncoder.encode(arg);
        view.set(buf);
        return {
            read: arg.length,
            written: buf.length
        };
    };
}

let WASM_VECTOR_LEN = 0;

let wasmModule, wasmInstance, wasm;
function __wbg_finalize_init(instance, module) {
    wasmInstance = instance;
    wasm = instance.exports;
    wasmModule = module;
    cachedDataViewMemory0 = null;
    cachedFloat64ArrayMemory0 = null;
    cachedUint32ArrayMemory0 = null;
    cachedUint8ArrayMemory0 = null;
    wasm.__wbindgen_start();
    return wasm;
}

async function __wbg_load(module, imports) {
    if (typeof Response === 'function' && module instanceof Response) {
        if (!module.ok) {
            throw new Error(`failed to fetch Wasm: ${module.status} ${module.statusText} fetching '${module.url}'`);
        }

        if (typeof WebAssembly.instantiateStreaming === 'function') {
            try {
                return await WebAssembly.instantiateStreaming(module, imports);
            } catch (e) {
                const validResponse = expectedResponseType(module.type);

                if (validResponse && module.headers.get('Content-Type') !== 'application/wasm') {
                    console.warn("`WebAssembly.instantiateStreaming` failed because your server does not serve Wasm with `application/wasm` MIME type. Falling back to `WebAssembly.instantiate` which is slower. Original error:\n", e);

                } else { throw e; }
            }
        }

        const bytes = await module.arrayBuffer();
        return await WebAssembly.instantiate(bytes, imports);
    } else {
        const instance = await WebAssembly.instantiate(module, imports);

        if (instance instanceof WebAssembly.Instance) {
            return { instance, module };
        } else {
            return instance;
        }
    }

    function expectedResponseType(type) {
        switch (type) {
            case 'basic': case 'cors': case 'default': return true;
        }
        return false;
    }
}

function initSync(module) {
    if (wasm !== undefined) return wasm;


    if (module !== undefined) {
        if (Object.getPrototypeOf(module) === Object.prototype) {
            ({module} = module)
        } else {
            console.warn('using deprecated parameters for `initSync()`; pass a single object instead')
        }
    }

    const imports = __wbg_get_imports();
    if (!(module instanceof WebAssembly.Module)) {
        module = new WebAssembly.Module(module);
    }
    const instance = new WebAssembly.Instance(module, imports);
    return __wbg_finalize_init(instance, module);
}

async function __wbg_init(module_or_path) {
    if (wasm !== undefined) return wasm;


    if (module_or_path !== undefined) {
        if (Object.getPrototypeOf(module_or_path) === Object.prototype) {
            ({module_or_path} = module_or_path)
        } else {
            console.warn('using deprecated parameters for the initialization function; pass a single object instead')
        }
    }

    if (module_or_path === undefined) {
        module_or_path = new URL('zpdf_wasm_bg.wasm', import.meta.url);
    }
    const imports = __wbg_get_imports();

    if (typeof module_or_path === 'string' || (typeof Request === 'function' && module_or_path instanceof Request) || (typeof URL === 'function' && module_or_path instanceof URL)) {
        module_or_path = fetch(module_or_path);
    }

    const { instance, module } = await __wbg_load(await module_or_path, imports);

    return __wbg_finalize_init(instance, module);
}

export { initSync, __wbg_init as default };

//! GPU pass timing via timestamp queries (P3.8).
//!
//! Purely additive telemetry: when the adapter lacks `Features::TIMESTAMP_QUERY`
//! (checked once at context creation, see `GpuContext::timestamps_supported`),
//! every method here degrades to a no-op / `None` rather than erroring — a
//! render must never fail because timing is unavailable.

use crate::context::GpuContext;

/// Brackets one page's GPU work with a begin/end timestamp pair. Built fresh
/// per `end_page()` call (the query set is tiny; no need to pool it).
pub struct GpuTimer {
    query_set: Option<wgpu::QuerySet>,
    resolve_buf: Option<wgpu::Buffer>,
    readback_buf: Option<wgpu::Buffer>,
    period: f32,
}

impl GpuTimer {
    /// Create a timer. All internals are `None` when the context doesn't
    /// support timestamp queries, making every other method a no-op.
    pub fn new(ctx: &GpuContext) -> Self {
        if !ctx.timestamps_supported {
            return Self {
                query_set: None,
                resolve_buf: None,
                readback_buf: None,
                period: 0.0,
            };
        }
        let query_set = ctx.device.create_query_set(&wgpu::QuerySetDescriptor {
            label: Some("zpdf-gpu-timer"),
            ty: wgpu::QueryType::Timestamp,
            count: 2,
        });
        // 2 x u64 timestamps.
        let resolve_buf = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("zpdf-timer-resolve"),
            size: 16,
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let readback_buf = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("zpdf-timer-readback"),
            size: 16,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Self {
            query_set: Some(query_set),
            resolve_buf: Some(resolve_buf),
            readback_buf: Some(readback_buf),
            period: ctx.timestamp_period,
        }
    }

    /// Write the "begin" timestamp (query index 0). No-op if unsupported.
    /// Call before recording any passes, outside a render pass.
    pub fn write_begin(&self, encoder: &mut wgpu::CommandEncoder) {
        if let Some(qs) = &self.query_set {
            encoder.write_timestamp(qs, 0);
        }
    }

    /// Write the "end" timestamp (query index 1). No-op if unsupported.
    /// Call after the last pass, outside a render pass.
    pub fn write_end(&self, encoder: &mut wgpu::CommandEncoder) {
        if let Some(qs) = &self.query_set {
            encoder.write_timestamp(qs, 1);
        }
    }

    /// Record the query-set -> resolve-buffer -> readback-buffer copies. Must
    /// run after both timestamps are written, in the same encoder, before
    /// submit. No-op if unsupported.
    pub fn record_resolve(&self, encoder: &mut wgpu::CommandEncoder) {
        if let (Some(qs), Some(resolve), Some(readback)) =
            (&self.query_set, &self.resolve_buf, &self.readback_buf)
        {
            encoder.resolve_query_set(qs, 0..2, resolve, 0);
            encoder.copy_buffer_to_buffer(resolve, 0, readback, 0, 16);
        }
    }

    /// Map the readback buffer (call after `queue.submit`) and compute the
    /// elapsed nanoseconds. Blocks on `device.poll`. `None` when unsupported
    /// or on any readback failure — timing is never load-bearing.
    pub fn resolve_ns(&self, device: &wgpu::Device) -> Option<u64> {
        let readback = self.readback_buf.as_ref()?;
        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device.poll(wgpu::PollType::wait_indefinitely()).ok()?;
        rx.try_recv().ok()?.ok()?;
        let view = slice.get_mapped_range();
        let start = u64::from_le_bytes(view[0..8].try_into().ok()?);
        let end = u64::from_le_bytes(view[8..16].try_into().ok()?);
        drop(view);
        readback.unmap();
        Some(((end.saturating_sub(start)) as f64 * self.period as f64) as u64)
    }
}

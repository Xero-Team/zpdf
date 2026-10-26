use zpdf_render_wgpu::{GpuContext, WgpuRenderError};

#[test]
fn concurrent_headless_adapter_requests_do_not_panic() {
    let threads: Vec<_> = (0..4)
        .map(|_| {
            std::thread::spawn(|| match GpuContext::new_headless() {
                Ok(context) => Some(context.identity().backend.clone()),
                Err(WgpuRenderError::NoAdapter) => None,
                Err(error) => panic!("headless context: {error}"),
            })
        })
        .collect();
    let backends: Vec<_> = threads
        .into_iter()
        .filter_map(|thread| thread.join().expect("adapter request panicked"))
        .collect();
    if backends.is_empty() {
        eprintln!("skipping GPU context test: no adapter");
        return;
    }
    assert_eq!(backends.len(), 4, "adapter availability must be consistent");
    assert!(backends.iter().all(|backend| backend == &backends[0]));
}

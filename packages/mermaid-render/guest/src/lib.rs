//! Portable Mermaid guest ABI v1. No filesystem, network, or WASI imports.
//!
//! The host owns its input allocation until `render` returns. Render output is
//! a separate allocation: status byte (0 = SVG, 1 = error) followed by UTF-8.
//! Both allocations must be returned with `dealloc(ptr, exact_len)`.

/// Allocate zero-initialized bytes that the host may fill in linear memory.
#[unsafe(no_mangle)]
pub extern "C" fn alloc(len: u32) -> u32 {
    let bytes = vec![0_u8; len as usize].into_boxed_slice();
    Box::into_raw(bytes).cast::<u8>() as u32
}

/// # Safety
/// `ptr` and `len` must describe a live allocation returned by this guest,
/// and the allocation must not be accessed after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dealloc(ptr: u32, len: u32) {
    let slice = std::ptr::slice_from_raw_parts_mut(ptr as *mut u8, len as usize);
    // SAFETY: the ABI requires an exact, uniquely owned allocation.
    drop(unsafe { Box::from_raw(slice) });
}

/// # Safety
/// `ptr..ptr+len` must be readable guest memory for the duration of this call.
/// The return packs the independent output pointer high, byte length low.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn render(ptr: u32, len: u32) -> u64 {
    // SAFETY: the host validates and supplies a live input allocation.
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let result = std::str::from_utf8(bytes)
        .map_err(|error| error.to_string())
        .and_then(|source| {
            static FONT: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
            FONT.get_or_init(|| {
                mermaid_rs_renderer::set_text_measurement_font(
                    include_bytes!("../BmuxDiagram-Regular.ttf").to_vec(),
                    0,
                )
                .map_err(|error| error.to_string())
            })
            .as_ref()
            .map_err(Clone::clone)?;
            let mut options = mermaid_rs_renderer::RenderOptions::default();
            options.theme.font_family = "BMUX Diagram".into();
            mermaid_rs_renderer::render_strict(source, options).map_err(|error| error.to_string())
        });
    let (status, text) = match result {
        Ok(svg) => (0, svg),
        Err(error) => (1, error),
    };
    let mut output = Vec::with_capacity(text.len() + 1);
    output.push(status);
    output.extend_from_slice(text.as_bytes());
    let output = output.into_boxed_slice();
    let len = u32::try_from(output.len()).expect("guest output fits wasm32 memory");
    let ptr = Box::into_raw(output).cast::<u8>() as u32;
    (u64::from(ptr) << 32) | u64::from(len)
}

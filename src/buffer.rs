//! Conversion of client buffers into `slint::Image`.

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use smithay::reexports::wayland_server::protocol::{wl_buffer::WlBuffer, wl_shm};
use smithay::wayland::shm::{with_buffer_contents, BufferData};

/// The shm formats that [`image_from_buffer`] understands.
pub const SHM_FORMATS: [wl_shm::Format; 2] = [wl_shm::Format::Argb8888, wl_shm::Format::Xrgb8888];

/// The largest buffer that is shown, in either direction.
pub const MAX_DIMENSION: usize = 16384;
/// The largest shm buffer that is shown. Every commit copies the buffer, so without a limit
/// a client could have the compositor allocate whatever the client's pool holds.
pub const MAX_SHM_PIXELS: usize = 8192 * 8192;
/// How much memory the copies of all shm buffers together may take, unless
/// `Compositor::set_shm_memory_limit()` says otherwise. One buffer can be within its own
/// limit, many of them on many surfaces still add up.
pub const DEFAULT_SHM_MEMORY_LIMIT: usize = 1 << 30;

/// The memory a copy of a wl_shm buffer takes, or `None` if it is not one.
pub fn shm_bytes(buffer: &WlBuffer) -> Option<usize> {
    with_buffer_contents(buffer, |_, _, data: BufferData| {
        usize::try_from(data.width).ok()?.checked_mul(usize::try_from(data.height).ok()?)?.checked_mul(4)
    })
    .ok()
    .flatten()
}

/// Copies a wl_shm buffer into an image. Returns `None` for anything else, and for buffers
/// that are larger than [`MAX_DIMENSION`] or [`MAX_SHM_PIXELS`].
pub fn image_from_buffer(buffer: &WlBuffer) -> Option<Image> {
    with_buffer_contents(buffer, |ptr, len, data: BufferData| {
        let (width, height) = (usize::try_from(data.width).ok()?, usize::try_from(data.height).ok()?);
        let (offset, stride) = (usize::try_from(data.offset).ok()?, usize::try_from(data.stride).ok()?);
        let opaque = match data.format {
            wl_shm::Format::Argb8888 => false,
            wl_shm::Format::Xrgb8888 => true,
            _ => return None,
        };
        // Sizes come from the client: none of this may overflow, also where usize is 32 bit.
        let row = width.checked_mul(4)?;
        if width == 0 || height == 0 || stride < row {
            return None;
        }
        if width > MAX_DIMENSION || height > MAX_DIMENSION || width.checked_mul(height)? > MAX_SHM_PIXELS {
            return None;
        }
        let end = stride.checked_mul(height - 1)?.checked_add(row)?.checked_add(offset)?;
        if end > len {
            return None;
        }
        // SAFETY: smithay keeps the pool mapped for the duration of the callback and the
        // range was checked against `len` above.
        let src = unsafe { std::slice::from_raw_parts(ptr, len) };

        let mut pixels = SharedPixelBuffer::<Rgba8Pixel>::new(width as u32, height as u32);
        for (index, out) in pixels.make_mut_slice().chunks_exact_mut(width).enumerate() {
            let line = &src[offset + index * stride..][..row];
            // Both formats are little endian 32 bit words, so B, G, R, A in memory.
            // Wayland colors are premultiplied, like the image created below.
            for (dst, bgra) in out.iter_mut().zip(line.as_chunks::<4>().0) {
                *dst = Rgba8Pixel { r: bgra[2], g: bgra[1], b: bgra[0], a: if opaque { 255 } else { bgra[3] } };
            }
        }
        Some(Image::from_rgba8_premultiplied(pixels))
    })
    .ok()
    .flatten()
}

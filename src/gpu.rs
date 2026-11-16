//! Showing client buffers that live on the GPU, without copying them.
//!
//! Such a buffer is turned into an `EGLImage`, that into a texture in the OpenGL context Slint
//! renders with, and Slint is given the texture as an image. This needs Slint to render with
//! OpenGL ES on EGL, which is what the winit and linuxkms backends do with the FemtoVG and
//! Skia OpenGL renderers.
//!
//! Clients hand over their buffers in one of two ways:
//!
//! - as dmabufs, with the linux-dmabuf protocol, which is what Mesa does,
//! - with a protocol private to the driver, which the EGL of the compositor has to provide
//!   (`EGL_WL_bind_wayland_display`). The drivers of Vivante GPUs work only this way.

use std::collections::HashMap;
use std::ffi::{c_void, CStr};
use std::num::NonZeroU32;

use slint::{BorrowedOpenGLTextureBuilder, BorrowedOpenGLTextureOrigin, Image};
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::{Buffer, Format, Fourcc};
use smithay::backend::egl::{ffi as egl_ffi, EGLDevice, EGLDisplay};
use smithay::backend::renderer::gles::ffi as gl;
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::{DisplayHandle, Resource};
use smithay::wayland::dmabuf::get_dmabuf;

use crate::RefusalReason;

pub type EglImage = egl_ffi::egl::types::EGLImage;

/// The dmabuf formats a texture with red, green, blue and alpha can show.
const FORMATS: [Fourcc; 4] = [Fourcc::Argb8888, Fourcc::Xrgb8888, Fourcc::Abgr8888, Fourcc::Xbgr8888];

/// How many buffers on the GPU of all clients together are taken. A client needs a handful
/// per window.
const MAX_GPU_BUFFERS: usize = 4096;
/// How much memory the buffers on the GPU of all clients together may take, counted as
/// four bytes per pixel, unless `Compositor::set_gpu_memory_limit()` says otherwise. The
/// memory is the clients', but nothing else keeps them from asking for all there is.
pub const DEFAULT_GPU_MEMORY_LIMIT: usize = 2 << 30;

struct Texture {
    id: NonZeroU32,
    image: Image,
    /// Owns the `EGLImage` of a buffer of the driver's protocol.
    #[cfg(feature = "bind-wayland-display")]
    _contents: Option<smithay::backend::egl::EGLBuffer>,
}

pub struct GpuImporter {
    egl: EGLDisplay,
    gl: gl::Gles2,
    /// What became of the dmabufs clients created, by the id of their `wl_buffer`.
    dmabuf_images: HashMap<ObjectId, EglImage>,
    textures: HashMap<ObjectId, Texture>,
    /// The number of pixels of every buffer that is kept, dmabuf or not.
    pixels: HashMap<ObjectId, usize>,
    pixel_limit: usize,
    #[cfg(feature = "bind-wayland-display")]
    reader: Option<smithay::backend::egl::display::EGLBufferReader>,
}

impl GpuImporter {
    /// Looks at the OpenGL context that is current.
    ///
    /// # Safety
    ///
    /// `get_proc_address` has to resolve the functions of the current context, and the
    /// importer may only be used and dropped while that context is current.
    pub unsafe fn new(get_proc_address: &dyn Fn(&CStr) -> *const c_void) -> Result<Self, String> {
        use egl_ffi::egl;

        egl_ffi::make_sure_egl_is_loaded().map_err(|e| e.to_string())?;
        let display = egl::GetCurrentDisplay();
        let context = egl::GetCurrentContext();
        if display == egl::NO_DISPLAY || context == egl::NO_CONTEXT {
            return Err("Slint does not render with EGL".into());
        }

        // smithay wants to know a config of the display. It only looks at it, so any will do
        // when the context was created without one.
        let mut config_id = 0;
        egl::QueryContext(display, context, egl::CONFIG_ID as i32, &mut config_id);
        let attributes = [egl::CONFIG_ID as i32, config_id, egl::NONE as i32];
        let mut config = std::ptr::null();
        let mut count = 0;
        if config_id == 0
            || egl::ChooseConfig(display, attributes.as_ptr(), &mut config, 1, &mut count) == 0
            || count == 0
        {
            egl::GetConfigs(display, &mut config, 1, &mut count);
        }
        if config.is_null() {
            return Err("the EGL display has no configuration".into());
        }
        let egl = EGLDisplay::from_raw(display, config).map_err(|e| e.to_string())?;

        let gl = gl::Gles2::load_with(|name| match std::ffi::CString::new(name) {
            Ok(name) => get_proc_address(&name),
            Err(_) => std::ptr::null(),
        });
        let extensions = gl.GetString(gl::EXTENSIONS);
        let has_egl_image = !extensions.is_null()
            && CStr::from_ptr(extensions.cast()).to_string_lossy().split(' ').any(|e| e == "GL_OES_EGL_image");
        if !has_egl_image || !gl.EGLImageTargetTexture2DOES.is_loaded() {
            return Err("OpenGL cannot use EGL images as textures".into());
        }

        Ok(Self {
            egl,
            gl,
            dmabuf_images: HashMap::new(),
            textures: HashMap::new(),
            pixels: HashMap::new(),
            pixel_limit: DEFAULT_GPU_MEMORY_LIMIT / 4,
            #[cfg(feature = "bind-wayland-display")]
            reader: None,
        })
    }

    /// What to tell clients about dmabufs: the device to render on, so that nothing has to
    /// be copied, and the formats and modifiers that can be shown.
    pub fn dmabuf_support(&self) -> Result<(libc::dev_t, Vec<Format>), String> {
        let device = EGLDevice::device_for_display(&self.egl).map_err(|e| e.to_string())?;
        if device.is_software() {
            return Err("Slint renders in software".into());
        }
        let node = device.try_get_render_node().ok().flatten().ok_or("the device Slint renders on is unknown")?;
        let formats: Vec<Format> =
            self.egl.dmabuf_texture_formats().iter().copied().filter(|f| FORMATS.contains(&f.code)).collect();
        if formats.is_empty() {
            return Err("the GPU cannot import dmabufs".into());
        }
        Ok((node.dev_id(), formats))
    }

    /// Lets the EGL of the driver offer its own protocol for buffers to clients.
    #[cfg(feature = "bind-wayland-display")]
    pub fn bind_wayland_display(&mut self, display: &DisplayHandle) -> Result<(), String> {
        self.reader = Some(self.egl.bind_wl_display(display).map_err(|e| e.to_string())?);
        Ok(())
    }

    #[cfg(not(feature = "bind-wayland-display"))]
    pub fn bind_wayland_display(&mut self, _display: &DisplayHandle) -> Result<(), String> {
        Err("built without the bind-wayland-display feature".into())
    }

    /// Whether the GPU takes this dmabuf. Does not need the OpenGL context.
    ///
    /// The result is to be passed to [`Self::register_dmabuf`] or [`Self::destroy_image`].
    ///
    /// An error with a reason means that the buffer is one too many or too large, one
    /// without that it is nothing the GPU can show.
    pub fn check_dmabuf(&self, dmabuf: &Dmabuf) -> Result<EglImage, Option<RefusalReason>> {
        if !FORMATS.contains(&dmabuf.format().code) {
            return Err(None);
        }
        self.admit(dmabuf.width() as usize, dmabuf.height() as usize).map_err(Some)?;
        self.egl.create_image_from_dmabuf(dmabuf).map_err(|_| None)
    }

    /// Whether there is room for another buffer of this size.
    fn admit(&self, width: usize, height: usize) -> Result<(), RefusalReason> {
        if width > crate::buffer::MAX_DIMENSION || height > crate::buffer::MAX_DIMENSION {
            return Err(RefusalReason::TooLarge);
        }
        let held: usize = self.pixels.values().sum();
        if self.pixels.len() >= MAX_GPU_BUFFERS || held.saturating_add(width * height) > self.pixel_limit {
            return Err(RefusalReason::MemoryLimit);
        }
        Ok(())
    }

    /// Sets how much memory the buffers may take, at four bytes per pixel.
    pub fn set_memory_limit(&mut self, bytes: usize) {
        self.pixel_limit = bytes / 4;
    }

    /// Remembers what [`Self::check_dmabuf`] found out for the `wl_buffer` the dmabuf became.
    pub fn register_dmabuf(&mut self, buffer: ObjectId, image: EglImage, dmabuf: &Dmabuf) {
        self.pixels.insert(buffer.clone(), dmabuf.width() as usize * dmabuf.height() as usize);
        self.dmabuf_images.insert(buffer, image);
    }

    /// Whether anything is kept for a buffer.
    pub fn knows(&self, buffer: &ObjectId) -> bool {
        self.dmabuf_images.contains_key(buffer) || self.textures.contains_key(buffer)
    }

    /// The image of a buffer, if it has a texture already.
    pub fn image(&self, buffer: &ObjectId) -> Option<Image> {
        self.textures.get(buffer).map(|texture| texture.image.clone())
    }

    /// Creates the texture and image of a buffer. The OpenGL context has to be current.
    pub fn import(&mut self, buffer: &WlBuffer) -> Option<Image> {
        let id = buffer.id();
        if let Some(image) = self.image(&id) {
            return Some(image);
        }

        if let Ok(dmabuf) = get_dmabuf(buffer) {
            let egl_image = *self.dmabuf_images.get(&id)?;
            let size = dmabuf.size();
            let opaque = matches!(dmabuf.format().code, Fourcc::Xrgb8888 | Fourcc::Xbgr8888);
            let (texture, image) = self.texture(egl_image, size.w, size.h, dmabuf.y_inverted(), opaque)?;
            self.textures.insert(
                id,
                Texture {
                    id: texture,
                    image: image.clone(),
                    #[cfg(feature = "bind-wayland-display")]
                    _contents: None,
                },
            );
            return Some(image);
        }

        #[cfg(feature = "bind-wayland-display")]
        {
            use smithay::backend::egl::Format;
            let contents = self.reader.as_ref()?.egl_buffer_contents(buffer).ok()?;
            // Video formats would need a shader of their own to be shown.
            if !matches!(contents.format, Format::RGB | Format::RGBA) {
                return None;
            }
            // These buffers show up when they are committed, so this is where they count.
            let (width, height) = (contents.size.w.max(0) as usize, contents.size.h.max(0) as usize);
            self.admit(width, height).ok()?;
            self.pixels.insert(id.clone(), width * height);
            let (texture, image) = self.texture(
                contents.image(0)?,
                contents.size.w,
                contents.size.h,
                contents.y_inverted,
                contents.format == Format::RGB,
            )?;
            self.textures.insert(id, Texture { id: texture, image: image.clone(), _contents: Some(contents) });
            Some(image)
        }

        #[cfg(not(feature = "bind-wayland-display"))]
        None
    }

    /// `opaque` is for formats without alpha. What their fourth byte holds is undefined, and
    /// some drivers hand it out as alpha: Vulkan of the Raspberry Pi 4 leaves it at zero,
    /// which made the window transparent.
    fn texture(
        &self,
        egl_image: EglImage,
        width: i32,
        height: i32,
        y_inverted: bool,
        opaque: bool,
    ) -> Option<(NonZeroU32, Image)> {
        let (width, height) = (u32::try_from(width).ok()?, u32::try_from(height).ok()?);

        // SAFETY: the context is current, and the binding Slint relies on is put back.
        let id = unsafe {
            let mut previous = 0;
            self.gl.GetIntegerv(gl::TEXTURE_BINDING_2D, &mut previous);
            // Errors that are pending are not about what follows.
            while self.gl.GetError() != gl::NO_ERROR {}
            let mut id = 0;
            self.gl.GenTextures(1, &mut id);
            self.gl.BindTexture(gl::TEXTURE_2D, id);
            self.gl.TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
            self.gl.TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
            self.gl.TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as i32);
            self.gl.TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as i32);
            self.gl.EGLImageTargetTexture2DOES(gl::TEXTURE_2D, egl_image);
            if opaque {
                self.gl.TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_SWIZZLE_A, gl::ONE as i32);
            }
            let failed = self.gl.GetError() != gl::NO_ERROR;
            self.gl.BindTexture(gl::TEXTURE_2D, previous as u32);
            if failed {
                // A texture the driver did not fill would be shown as garbage or black.
                self.gl.DeleteTextures(1, &id);
                return None;
            }
            NonZeroU32::new(id)?
        };

        let origin =
            if y_inverted { BorrowedOpenGLTextureOrigin::BottomLeft } else { BorrowedOpenGLTextureOrigin::TopLeft };
        // SAFETY: the texture exists until `forget()` deletes it, which happens when the
        // buffer is destroyed and with that cannot be the content of a surface anymore.
        let image = unsafe { BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(id, [width, height].into()) }
            .origin(origin)
            .build();
        Some((id, image))
    }

    /// Frees what belongs to a buffer. The OpenGL context has to be current.
    pub fn forget(&mut self, buffer: &ObjectId) {
        if let Some(texture) = self.textures.remove(buffer) {
            // SAFETY: the context is current and the texture is one of ours.
            unsafe { self.gl.DeleteTextures(1, &texture.id.get()) };
        }
        if let Some(image) = self.dmabuf_images.remove(buffer) {
            self.destroy_image(image);
        }
        self.pixels.remove(buffer);
    }

    pub fn destroy_image(&self, image: EglImage) {
        // SAFETY: the image was created on this display and is not used anymore.
        unsafe { egl_ffi::egl::DestroyImageKHR(**self.egl.get_display_handle(), image) };
    }
}

impl Drop for GpuImporter {
    fn drop(&mut self) {
        let buffers: Vec<ObjectId> = self.dmabuf_images.keys().chain(self.textures.keys()).cloned().collect();
        for buffer in buffers {
            self.forget(&buffer);
        }
    }
}

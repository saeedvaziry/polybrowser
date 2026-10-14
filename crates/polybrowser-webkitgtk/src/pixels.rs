use super::*;
pub(super) unsafe fn snapshot_pixels(g: &Api, surface: gpointer) -> Result<Image> {
    unsafe {
        g.cairo_surface_flush(surface);
        let mapped = g.cairo_surface_map_to_image(surface, ptr::null_mut());
        let width = u32::try_from(g.cairo_image_surface_get_width(mapped)).unwrap_or(0);
        let height = u32::try_from(g.cairo_image_surface_get_height(mapped)).unwrap_or(0);
        let stride = usize::try_from(g.cairo_image_surface_get_stride(mapped)).unwrap_or(0);
        let data = g.cairo_image_surface_get_data(mapped);
        let format = g.cairo_image_surface_get_format(mapped);
        let valid = g.cairo_surface_status(mapped) == 0
            && width > 0
            && height > 0
            && u64::from(width) * u64::from(height) <= 64 * 1024 * 1024
            && stride >= width as usize * 4
            && !data.is_null()
            && matches!(format, 0 | 1);
        let result = if valid {
            let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
            for y in 0..height as usize {
                for pixel in std::slice::from_raw_parts(data.add(y * stride), width as usize * 4)
                    .chunks_exact(4)
                {
                    let pixel = u32::from_ne_bytes(pixel.try_into().unwrap());
                    let a = if format == 1 {
                        255
                    } else {
                        (pixel >> 24) as u8
                    };
                    let channel = |value: u8| {
                        if a == 0 {
                            0
                        } else {
                            u8::try_from(u32::from(value) * 255 / u32::from(a)).unwrap_or(255)
                        }
                    };
                    rgba.extend_from_slice(&[
                        channel((pixel >> 16) as u8),
                        channel((pixel >> 8) as u8),
                        channel(pixel as u8),
                        a,
                    ]);
                }
            }
            Ok(Image {
                width,
                height,
                rgba,
            })
        } else {
            Err(error("invalid screenshot size or format"))
        };
        g.cairo_surface_unmap_image(surface, mapped);
        result
    }
}

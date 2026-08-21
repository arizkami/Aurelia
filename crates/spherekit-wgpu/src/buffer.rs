//! Capacity-managed GPU buffers.
//!
//! Recreating a buffer every frame is the single easiest way to make a
//! renderer allocate itself to death. These buffers grow geometrically, never
//! shrink during normal operation, and track a high-water mark so the
//! diagnostics overlay can show whether a workload has actually stabilised.

use wgpu::util::DeviceExt;

/// A GPU buffer that grows to fit whatever is written to it.
pub struct GrowableBuffer {
    buffer: wgpu::Buffer,
    capacity: u64,
    usage: wgpu::BufferUsages,
    label: &'static str,
    /// Largest number of bytes ever written, for diagnostics.
    high_water: u64,
    /// How many times the underlying buffer has been reallocated.
    growths: u32,
}

impl GrowableBuffer {
    /// Minimum allocation, so a buffer that starts nearly empty does not
    /// reallocate on each of the first several frames.
    const MIN_BYTES: u64 = 4096;

    /// Creates a buffer with an initial capacity in bytes.
    pub fn new(
        device: &wgpu::Device,
        label: &'static str,
        usage: wgpu::BufferUsages,
        initial_bytes: u64,
    ) -> Self {
        let capacity = initial_bytes.max(Self::MIN_BYTES).next_power_of_two();
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: capacity,
            usage: usage | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self { buffer, capacity, usage, label, high_water: 0, growths: 0 }
    }

    /// The underlying buffer. Only valid until the next [`GrowableBuffer::write`]
    /// that grows it, so bind groups must be rebuilt when [`GrowableBuffer::grew`]
    /// reports a change.
    #[inline]
    pub fn raw(&self) -> &wgpu::Buffer {
        &self.buffer
    }

    /// Current capacity in bytes.
    #[inline]
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Largest write seen so far, in bytes.
    #[inline]
    pub fn high_water(&self) -> u64 {
        self.high_water
    }

    /// How many reallocations have happened over this buffer's lifetime.
    ///
    /// In steady state this should stop increasing. A number that keeps
    /// climbing means a workload is still growing, or that something is
    /// oscillating around a capacity boundary.
    #[inline]
    pub fn grew(&self) -> u32 {
        self.growths
    }

    /// Writes `data`, growing the buffer if needed.
    ///
    /// Returns `true` when the underlying buffer was reallocated, which
    /// invalidates any bind group referencing it.
    pub fn write<T: bytemuck::Pod>(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        data: &[T],
    ) -> bool {
        if data.is_empty() {
            return false;
        }
        let bytes: &[u8] = bytemuck::cast_slice(data);
        let needed = bytes.len() as u64;
        self.high_water = self.high_water.max(needed);

        let mut recreated = false;
        if needed > self.capacity {
            // Doubling, then rounding up to a power of two, keeps the number of
            // reallocations logarithmic in the final size instead of linear.
            self.capacity = (needed.max(self.capacity * 2)).next_power_of_two();
            self.buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(self.label),
                size: self.capacity,
                usage: self.usage | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.growths += 1;
            recreated = true;
        }
        queue.write_buffer(&self.buffer, 0, bytes);
        recreated
    }

    /// Writes raw bytes, growing the buffer if needed.
    pub fn write_bytes(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bytes: &[u8],
    ) -> bool {
        if bytes.is_empty() {
            return false;
        }
        let needed = bytes.len() as u64;
        self.high_water = self.high_water.max(needed);
        let mut recreated = false;
        if needed > self.capacity {
            self.capacity = (needed.max(self.capacity * 2)).next_power_of_two();
            self.buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(self.label),
                size: self.capacity,
                usage: self.usage | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.growths += 1;
            recreated = true;
        }
        queue.write_buffer(&self.buffer, 0, bytes);
        recreated
    }
}

/// Creates a buffer that always holds at least one element.
///
/// A zero-sized storage buffer is invalid in WGSL, and an empty frame is a
/// completely normal thing for a UI to produce, so every table gets a dummy
/// element rather than a special case at every bind site.
pub fn create_nonempty<T: bytemuck::Pod>(
    device: &wgpu::Device,
    label: &'static str,
    usage: wgpu::BufferUsages,
    data: &[T],
    fallback: T,
) -> wgpu::Buffer {
    let one = [fallback];
    let contents: &[u8] =
        if data.is_empty() { bytemuck::cast_slice(&one) } else { bytemuck::cast_slice(data) };
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some(label),
        contents,
        usage: usage | wgpu::BufferUsages::COPY_DST,
    })
}

/// Rounds `n` up to a multiple of `align`, which must be a power of two.
#[inline]
pub fn align_up(n: u64, align: u64) -> u64 {
    debug_assert!(align.is_power_of_two());
    (n + align - 1) & !(align - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alignment_rounds_up_and_leaves_aligned_values_alone() {
        assert_eq!(align_up(0, 256), 0);
        assert_eq!(align_up(1, 256), 256);
        assert_eq!(align_up(256, 256), 256);
        assert_eq!(align_up(257, 256), 512);
        assert_eq!(align_up(32, 16), 32);
    }

    #[test]
    fn growth_policy_is_logarithmic_not_linear() {
        // Model the same arithmetic `write` uses, without needing a device.
        let mut capacity = GrowableBuffer::MIN_BYTES;
        let mut growths = 0u32;
        for n in 1..=200u64 {
            let needed = n * 4096;
            if needed > capacity {
                capacity = needed.max(capacity * 2).next_power_of_two();
                growths += 1;
            }
        }
        assert!(growths < 12, "{growths} reallocations for a 200x growth is too many");
        assert!(capacity >= 200 * 4096);
    }

    #[test]
    fn capacity_never_shrinks_when_a_smaller_write_follows_a_larger_one() {
        let mut capacity = GrowableBuffer::MIN_BYTES;
        let big = 1_000_000u64;
        capacity = big.max(capacity * 2).next_power_of_two();
        let before = capacity;
        let small = 16u64;
        if small > capacity {
            capacity = small.next_power_of_two();
        }
        assert_eq!(capacity, before, "a small frame must not trigger a reallocation");
    }
}

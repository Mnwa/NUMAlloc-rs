/// Number of size classes for small object allocation.
pub const NUM_SIZE_CLASSES: usize = 12;

/// Size classes: powers of 2 from 8 to 16384 bytes.
pub const SIZE_CLASSES: [usize; NUM_SIZE_CLASSES] = [
    8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384,
];

/// Size of each BiBOP bag (32 KB).
pub const BAG_SIZE: usize = 32 * 1024;

/// Maximum object size managed via bags (objects larger than this use mmap directly).
pub const SMALL_LIMIT: usize = 16384;

/// Returns the index of the smallest size class that can hold `size` bytes.
/// Returns `None` if `size` is 0 or exceeds `SMALL_LIMIT`.
pub fn size_class_index(size: usize) -> Option<usize> {
    if size == 0 || size > SMALL_LIMIT {
        return None;
    }
    if size <= 8 {
        return Some(0);
    }
    // Size classes are powers of 2 starting at 8 = 2^3.
    // ceil(log2(size)) gives the power needed; subtract 3 for the index.
    let bits = usize::BITS - (size - 1).leading_zeros();
    Some((bits as usize).saturating_sub(3))
}

/// Returns the allocation size for a given size class index.
pub const fn size_for_class(index: usize) -> usize {
    SIZE_CLASSES[index]
}

/// Returns the number of objects that fit in one bag for a given size class.
#[cfg(test)]
pub const fn objects_per_bag(class_index: usize) -> usize {
    BAG_SIZE / SIZE_CLASSES[class_index]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_size_class_index_boundaries() {
        assert_eq!(size_class_index(0), None);
        assert_eq!(size_class_index(1), Some(0)); // -> 8
        assert_eq!(size_class_index(8), Some(0)); // -> 8
        assert_eq!(size_class_index(9), Some(1)); // -> 16
        assert_eq!(size_class_index(16), Some(1)); // -> 16
        assert_eq!(size_class_index(17), Some(2)); // -> 32
        assert_eq!(size_class_index(32), Some(2)); // -> 32
        assert_eq!(size_class_index(33), Some(3)); // -> 64
        assert_eq!(size_class_index(64), Some(3));
        assert_eq!(size_class_index(128), Some(4));
        assert_eq!(size_class_index(256), Some(5));
        assert_eq!(size_class_index(512), Some(6));
        assert_eq!(size_class_index(1024), Some(7));
        assert_eq!(size_class_index(2048), Some(8));
        assert_eq!(size_class_index(4096), Some(9));
        assert_eq!(size_class_index(8192), Some(10));
        assert_eq!(size_class_index(16384), Some(11));
        assert_eq!(size_class_index(16385), None);
    }

    #[test]
    fn test_size_class_covers_request() {
        for size in 1..=SMALL_LIMIT {
            let idx = size_class_index(size).unwrap();
            assert!(
                size_for_class(idx) >= size,
                "size_class {} ({}B) too small for {}B",
                idx,
                size_for_class(idx),
                size
            );
        }
    }

    #[test]
    fn test_objects_per_bag() {
        assert_eq!(objects_per_bag(0), BAG_SIZE / 8); // 4096
        assert_eq!(objects_per_bag(11), BAG_SIZE / 16384); // 2
    }
}

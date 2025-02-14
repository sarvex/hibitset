use std::default::Default;
use std::fmt::{Debug, Error as FormatError, Formatter};
use std::iter::repeat;
use std::marker::PhantomData;
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use util::*;
use {BitSetLike, DrainableBitSet};

/// This is similar to a [`BitSet`] but allows setting of value
/// without unique ownership of the structure
///
/// An `AtomicBitSet` has the ability to add an item to the set
/// without unique ownership (given that the set is big enough).
/// Removing elements does require unique ownership as an effect
/// of the hierarchy it holds. Worst case multiple writers set the
/// same bit twice (but only is told they set it).
///
/// It is possible to atomically remove from the set, but not at the
/// same time as atomically adding. This is because there is no way
/// to know if layer 1-3 would be left in a consistent state if they are
/// being cleared and set at the same time.
///
/// `AtromicBitSet` resolves this race by disallowing atomic
/// clearing of bits.
///
/// [`BitSet`]: ../struct.BitSet.html
#[derive(Debug)]
pub struct AtomicBitSet {
    layer3: AtomicUsize,
    layer2: Vec<AtomicUsize>,
    layer1: Vec<AtomicBlock>,
}

impl AtomicBitSet {
    /// Creates an empty `AtomicBitSet`.
    pub fn new() -> AtomicBitSet {
        Default::default()
    }

    /// Adds `id` to the `AtomicBitSet`. Returns `true` if the value was
    /// already in the set.
    ///
    /// Because we cannot safely extend an AtomicBitSet without unique ownership
    /// this will panic if the Index is out of range.
    #[inline]
    pub fn add_atomic(&self, id: Index) -> bool {
        let (_, p1, p2) = offsets(id);

        // While it is tempting to check of the bit was set and exit here if it
        // was, this can result in a data race. If this thread and another
        // thread both set the same bit it is possible for the second thread
        // to exit before l3 was set. Resulting in the iterator to be in an
        // incorrect state. The window is small, but it exists.
        let set = self.layer1[p1].add(id);
        self.layer2[p2].fetch_or(id.mask(SHIFT2), Ordering::Relaxed);
        self.layer3.fetch_or(id.mask(SHIFT3), Ordering::Relaxed);
        set
    }

    /// Adds `id` to the `BitSet`. Returns `true` if the value was
    /// already in the set.
    #[inline]
    pub fn add(&mut self, id: Index) -> bool {
        use std::sync::atomic::Ordering::Relaxed;

        let (_, p1, p2) = offsets(id);
        if self.layer1[p1].add(id) {
            return true;
        }

        self.layer2[p2].store(self.layer2[p2].load(Relaxed) | id.mask(SHIFT2), Relaxed);
        self.layer3
            .store(self.layer3.load(Relaxed) | id.mask(SHIFT3), Relaxed);
        false
    }

    /// Removes `id` from the set, returns `true` if the value
    /// was removed, and `false` if the value was not set
    /// to begin with.
    #[inline]
    pub fn remove(&mut self, id: Index) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        let (_, p1, p2) = offsets(id);

        // if the bitmask was set we need to clear
        // its bit from layer0 to 3. the layers above only
        // should be cleared if the bit cleared was the last bit
        // in its set
        //
        // These are used over a `fetch_and` because we have a mutable
        // access to the AtomicBitSet so this is sound (and faster)
        if !self.layer1[p1].remove(id) {
            return false;
        }
        if self.layer1[p1].mask.load(Ordering::Relaxed) != 0 {
            return true;
        }

        let v = self.layer2[p2].load(Relaxed) & !id.mask(SHIFT2);
        self.layer2[p2].store(v, Relaxed);
        if v != 0 {
            return true;
        }

        let v = self.layer3.load(Relaxed) & !id.mask(SHIFT3);
        self.layer3.store(v, Relaxed);
        return true;
    }

    /// Returns `true` if `id` is in the set.
    #[inline]
    pub fn contains(&self, id: Index) -> bool {
        let i = id.offset(SHIFT2);
        self.layer1[i].contains(id)
    }

    /// Clear all bits in the set
    pub fn clear(&mut self) {
        // This is the same hierarchical-striding used in the iterators.
        // Using this technique we can avoid clearing segments of the bitset
        // that are already clear. In the best case when the set is already cleared,
        // this will only touch the highest layer.

        let (mut m3, mut m2) = (self.layer3.swap(0, Ordering::Relaxed), 0usize);
        let mut offset = 0;

        loop {
            if m2 != 0 {
                let bit = m2.trailing_zeros() as usize;
                m2 &= !(1 << bit);

                // layer 1 & 0 are cleared unconditionally. it's only 32-64 words
                // and the extra logic to select the correct works is slower
                // then just clearing them all.
                self.layer1[offset + bit].clear();
                continue;
            }

            if m3 != 0 {
                let bit = m3.trailing_zeros() as usize;
                m3 &= !(1 << bit);
                offset = bit << BITS;
                m2 = self.layer2[bit].swap(0, Ordering::Relaxed);
                continue;
            }
            break;
        }
    }
}

impl BitSetLike for AtomicBitSet {
    #[inline]
    fn layer3(&self) -> usize {
        self.layer3.load(Ordering::Relaxed)
    }
    #[inline]
    fn layer2(&self, i: usize) -> usize {
        self.layer2[i].load(Ordering::Relaxed)
    }
    #[inline]
    fn layer1(&self, i: usize) -> usize {
        self.layer1[i].mask.load(Ordering::Relaxed)
    }
    #[inline]
    fn layer0(&self, i: usize) -> usize {
        let (o1, o0) = (i >> BITS, i & ((1 << BITS) - 1));
        self.layer1[o1]
            .atom
            .get()
            .map(|layer0| layer0[o0].load(Ordering::Relaxed))
            .unwrap_or(0)
    }
    #[inline]
    fn contains(&self, i: Index) -> bool {
        self.contains(i)
    }
}

impl DrainableBitSet for AtomicBitSet {
    #[inline]
    fn remove(&mut self, i: Index) -> bool {
        self.remove(i)
    }
}

impl Default for AtomicBitSet {
    fn default() -> Self {
        AtomicBitSet {
            layer3: Default::default(),
            layer2: repeat(0)
                .map(|_| AtomicUsize::new(0))
                .take(1 << BITS)
                .collect(),
            layer1: repeat(0)
                .map(|_| AtomicBlock::new())
                .take(1 << (2 * BITS))
                .collect(),
        }
    }
}

struct OnceAtom {
    inner: AtomicPtr<[AtomicUsize; 1 << BITS]>,
    marker: PhantomData<Option<Box<[AtomicUsize; 1 << BITS]>>>,
}

impl Drop for OnceAtom {
    fn drop(&mut self) {
        let ptr = *self.inner.get_mut();
        if !ptr.is_null() {
            // SAFETY: If the pointer is not null, we created it from
            // `Box::into_raw` in `Self::atom_get_or_init`.
            unsafe { Box::from_raw(ptr) };
        }
    }
}

impl OnceAtom {
    fn new() -> Self {
        Self {
            inner: AtomicPtr::new(ptr::null_mut()),
            marker: PhantomData,
        }
    }

    fn get_or_init(&self) -> &[AtomicUsize; 1 << BITS] {
        let current_ptr = self.inner.load(Ordering::Acquire);
        let ptr = if current_ptr.is_null() {
            const ZERO: AtomicUsize = AtomicUsize::new(0);
            let new_ptr = Box::into_raw(Box::new([ZERO; 1 << BITS]));
            if let Err(existing_ptr) = self.inner.compare_exchange(
                ptr::null_mut(),
                new_ptr,
                // On success, Release matches any Acquire loads of the non-null
                // pointer, to ensure the new box is visible to other threads.
                Ordering::Release,
                Ordering::Acquire,
            ) {
                // SAFETY: We obtained this pointer from `Box::into_raw` above
                // and failed to publish it to the `AtomicPtr`.
                unsafe { Box::from_raw(new_ptr) };
                existing_ptr
            } else {
                new_ptr
            }
        } else {
            current_ptr
        };

        // SAFETY: We checked that this pointer is not null (either by
        // `.is_null()` check, `compare_exhange`, or from `Box::into_raw`). We
        // created from `Box::into_raw` (at some point) and we only use it to
        // create immutable references (unless we have exclusive access to self)
        unsafe { &*ptr }
    }

    fn get(&self) -> Option<&[AtomicUsize; 1 << BITS]> {
        let ptr = self.inner.load(Ordering::Acquire);
        // SAFETY: If it is not null, we created this pointer from
        // `Box::into_raw` and only use it to create immutable references
        // (unless we have exclusive access to self)
        unsafe { ptr.as_ref() }
    }

    fn get_mut(&mut self) -> Option<&mut [AtomicUsize; 1 << BITS]> {
        let ptr = self.inner.get_mut();
        // SAFETY: If this is not null, we created this pointer from
        // `Box::into_raw` and we have an exclusive borrow of self.
        unsafe { ptr.as_mut() }
    }
}

struct AtomicBlock {
    mask: AtomicUsize,
    atom: OnceAtom,
}

impl AtomicBlock {
    fn new() -> AtomicBlock {
        AtomicBlock {
            mask: AtomicUsize::new(0),
            atom: OnceAtom::new(),
        }
    }

    fn add(&self, id: Index) -> bool {
        let (i, m) = (id.row(SHIFT1), id.mask(SHIFT0));
        let old = self.atom.get_or_init()[i].fetch_or(m, Ordering::Relaxed);
        self.mask.fetch_or(id.mask(SHIFT1), Ordering::Relaxed);
        old & m != 0
    }

    fn contains(&self, id: Index) -> bool {
        self.atom
            .get()
            .map(|layer0| layer0[id.row(SHIFT1)].load(Ordering::Relaxed) & id.mask(SHIFT0) != 0)
            .unwrap_or(false)
    }

    fn remove(&mut self, id: Index) -> bool {
        if let Some(layer0) = self.atom.get_mut() {
            let (i, m) = (id.row(SHIFT1), !id.mask(SHIFT0));
            let v = layer0[i].get_mut();
            let was_set = *v & id.mask(SHIFT0) == id.mask(SHIFT0);
            *v = *v & m;
            if *v == 0 {
                // no other bits are set
                // so unset bit in the next level up
                *self.mask.get_mut() &= !id.mask(SHIFT1);
            }
            was_set
        } else {
            false
        }
    }

    fn clear(&mut self) {
        *self.mask.get_mut() = 0;
        self.atom.get_mut().map(|layer0| {
            for l in layer0 {
                *l.get_mut() = 0;
            }
        });
    }
}

impl Debug for AtomicBlock {
    fn fmt(&self, f: &mut Formatter) -> Result<(), FormatError> {
        f.debug_struct("AtomicBlock")
            .field("mask", &self.mask)
            .field("atom", &self.atom.get().unwrap().iter())
            .finish()
    }
}

#[cfg(test)]
mod atomic_set_test {
    use {AtomicBitSet, BitSetAnd, BitSetLike};

    #[test]
    fn insert() {
        let mut c = AtomicBitSet::new();
        for i in 0..1_000 {
            assert!(!c.add(i));
            assert!(c.add(i));
        }

        for i in 0..1_000 {
            assert!(c.contains(i));
        }
    }

    #[test]
    fn insert_100k() {
        let mut c = AtomicBitSet::new();
        for i in 0..100_000 {
            assert!(!c.add(i));
            assert!(c.add(i));
        }

        for i in 0..100_000 {
            assert!(c.contains(i));
        }
    }

    #[test]
    fn add_atomic() {
        let c = AtomicBitSet::new();
        for i in 0..1_000 {
            assert!(!c.add_atomic(i));
            assert!(c.add_atomic(i));
        }

        for i in 0..1_000 {
            assert!(c.contains(i));
        }
    }

    #[test]
    fn add_atomic_100k() {
        let c = AtomicBitSet::new();
        for i in 0..100_000 {
            assert!(!c.add_atomic(i));
            assert!(c.add_atomic(i));
        }

        for i in 0..100_000 {
            assert!(c.contains(i));
        }
    }

    #[test]
    fn remove() {
        let mut c = AtomicBitSet::new();
        for i in 0..1_000 {
            assert!(!c.add(i));
        }

        for i in 0..1_000 {
            assert!(c.contains(i));
            assert!(c.remove(i));
            assert!(!c.contains(i));
            assert!(!c.remove(i));
        }
    }

    #[test]
    fn iter() {
        let mut c = AtomicBitSet::new();
        for i in 0..100_000 {
            c.add(i);
        }

        let mut count = 0;
        for (idx, i) in c.iter().enumerate() {
            count += 1;
            assert_eq!(idx, i as usize);
        }
        assert_eq!(count, 100_000);
    }

    #[test]
    fn iter_odd_even() {
        let mut odd = AtomicBitSet::new();
        let mut even = AtomicBitSet::new();
        for i in 0..100_000 {
            if i % 2 == 1 {
                odd.add(i);
            } else {
                even.add(i);
            }
        }

        assert_eq!((&odd).iter().count(), 50_000);
        assert_eq!((&even).iter().count(), 50_000);
        assert_eq!(BitSetAnd(&odd, &even).iter().count(), 0);
    }

    #[test]
    fn clear() {
        let mut set = AtomicBitSet::new();
        for i in 0..1_000 {
            set.add(i);
        }

        assert_eq!((&set).iter().sum::<u32>(), 500_500 - 1_000);

        assert_eq!((&set).iter().count(), 1_000);
        set.clear();
        assert_eq!((&set).iter().count(), 0);

        for i in 0..1_000 {
            set.add(i * 64);
        }

        assert_eq!((&set).iter().count(), 1_000);
        set.clear();
        assert_eq!((&set).iter().count(), 0);

        for i in 0..1_000 {
            set.add(i * 1_000);
        }

        assert_eq!((&set).iter().count(), 1_000);
        set.clear();
        assert_eq!((&set).iter().count(), 0);

        for i in 0..100 {
            set.add(i * 10_000);
        }

        assert_eq!((&set).iter().count(), 100);
        set.clear();
        assert_eq!((&set).iter().count(), 0);

        for i in 0..10 {
            set.add(i * 10_000);
        }

        assert_eq!((&set).iter().count(), 10);
        set.clear();
        assert_eq!((&set).iter().count(), 0);
    }
}

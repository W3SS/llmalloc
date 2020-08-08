//! Implementation of Linux specific calls.

use core::{alloc::Layout, marker, ptr, sync::atomic};

use llmalloc_core::{self, PowerOf2};

use super::{NumaNodeIndex, Configuration, Platform, ThreadLocal};

/// Implementation of the Configuration trait, for Linux.
#[derive(Default)]
pub(crate) struct LLConfiguration;

impl Configuration for LLConfiguration {
    //  2 MB
    const LARGE_PAGE_SIZE: PowerOf2 = unsafe { PowerOf2::new_unchecked(2 * 1024 * 1024) };

    //  1 GB
    const HUGE_PAGE_SIZE: PowerOf2 = unsafe { PowerOf2::new_unchecked(1024 * 1024 * 1024) };
}

/// Implementation of the Platform trait, for Linux.
#[derive(Default)]
pub(crate) struct LLPlatform;

impl LLPlatform {
    /// Creates an instance.
    pub(crate) const fn new() -> Self { Self }
}

impl llmalloc_core::Platform for LLPlatform {
    unsafe fn allocate(&self, layout: Layout) -> *mut u8 {
        const HUGE_PAGE_SIZE: PowerOf2 = LLConfiguration::HUGE_PAGE_SIZE;

        assert!(layout.size() % HUGE_PAGE_SIZE == 0,
            "Incorrect size: {} % {} != 0", layout.size(), HUGE_PAGE_SIZE.value());
        assert!(layout.align() <= HUGE_PAGE_SIZE.value(),
            "Incorrect alignment: {} > {}", layout.align(), HUGE_PAGE_SIZE.value());

        let candidate = mmap_simplified(layout.size());

        assert!(candidate as usize % HUGE_PAGE_SIZE == 0,
            "Incorrect alignment of allocation: {} % {} != 0", candidate as usize, HUGE_PAGE_SIZE.value());

        candidate
    }

    unsafe fn deallocate(&self, pointer: *mut u8, layout: Layout) {
        let result = munmap(pointer, layout.size());
        assert!(result != 0, "{}", result);
    }
}

impl Platform for LLPlatform {
    #[cold]
    #[inline(never)]
    fn current_node(&self) -> NumaNodeIndex {
        let mut cpu = 0u32;
        let mut node = 0u32;
        unsafe { getcpu(&mut cpu as *mut _, &mut node as *mut _, ptr::null_mut()) };

        select_node(NumaNodeIndex::new(node))
    }
}

/// Implementation of the ThreadLocal trait, for Linux.
pub(crate) struct LLThreadLocal<T> {
    key: atomic::AtomicI64,
    destructor: *const u8,
    _marker: marker::PhantomData<*const T>,
}

impl<T> LLThreadLocal<T> {
    const UNINITIALIZED: i64 = -1;
    const UNDER_INITIALIZATION: i64 = -2;

    /// Creates an uninitialized instance.
    pub(crate) const fn new(destructor: *const u8) -> Self {
        let key = atomic::AtomicI64::new(-1);
        let destructor = destructor;
        let _marker = marker::PhantomData;

        LLThreadLocal { key, destructor, _marker }
    }

    #[inline(always)]
    fn get_key(&self) -> u32 {
        let key = self.key.load(atomic::Ordering::Relaxed);
        if key >= 0 { key as u32} else { unsafe { self.initialize() } }
    }

    #[cold]
    #[inline(never)]
    unsafe fn initialize(&self) -> u32 {
        let mut key = self.key.load(atomic::Ordering::Relaxed);

        if self.key.compare_and_swap(Self::UNINITIALIZED, Self::UNDER_INITIALIZATION, atomic::Ordering::Relaxed)
            == Self::UNINITIALIZED
        {
            key = self.create_key();
            self.key.store(key, atomic::Ordering::Relaxed);
        }

        while key < 0 {
            pthread_yield();
            key = self.key.load(atomic::Ordering::Relaxed);
        }

        key as u32
    }

    #[cold]
    unsafe fn create_key(&self) -> i64 {
        let mut key = 0u32;

        let result = pthread_key_create(&mut key as *mut _, self.destructor);
        assert!(result == 0, "Could not create thread-local key: {}", result);

        key as i64
    }
}

impl<T> ThreadLocal<T> for LLThreadLocal<T> {
    fn get(&self) -> *mut T {
        let key = self.key.load(atomic::Ordering::Relaxed);

        //  If key is not initialized, then a null pointer is returned.
        unsafe { pthread_getspecific(key as u32) as *mut T }
    }

    #[cold]
    #[inline(never)]
    fn set(&self, value: *mut T) {
        let key = self.get_key();

        let result = unsafe { pthread_setspecific(key, value as *mut u8) };
        assert!(result == 0, "Could not set thread-local value for {}: {}", key, result);
    }
}

unsafe impl<T> Sync for LLThreadLocal<T> {}

//  Selects the "best" node.
//
//  The Linux kernel sometimes distinguishes nodes even though their distance is 11, when the distance to self is 10.
//  This may lead to over-allocation, hence it is judged best to "cluster" the nodes together.
//
//  This function will therefore return the smallest node number whose distance to the `original` is less than or
//  equal to 11.
fn select_node(original: NumaNodeIndex) -> NumaNodeIndex {
    let original = original.value() as i32;

    for current in 0..original {
        if unsafe { numa_distance(current, original) } <= 11 {
            return NumaNodeIndex::new(current as u32);
        }
    }

    NumaNodeIndex::new(original as u32)
}

//  Wrapper around mmap.
//
//  Returns a pointer to `size` bytes of memory aligned on a HUGE PAGE boundary, or null.
unsafe fn mmap_simplified(size: usize) -> *mut u8 {
    const FAILURE: *mut u8 = !0 as *mut u8;

    const PROT_READ: i32 = 1;
    const PROT_WRITE: i32 = 2;

    const MAP_ANONYMOUS: i32 = 0x20;
    const MAP_HUGETLB: i32 = 0x40000;
    const MAP_HUGE_1GB: i32 = 30 << MAP_HUGE_SHIFT;

    const MAP_HUGE_SHIFT: u8 = 26;

    let addr = ptr::null_mut();
    let length = size;
    let prot = PROT_READ | PROT_WRITE;
    let flags = MAP_ANONYMOUS | MAP_HUGETLB | MAP_HUGE_1GB;
    //  When used in conjunction with MAP_ANONYMOUS, fd is mandated to be -1 on some implementations.
    let fd = -1;
    //  When used in conjunction with MAP_ANONYMOUS, offset is mandated to be 0 on some implementations.
    let offset = 0;

    let result = mmap(addr, length, prot, flags, fd, offset);

    if result != FAILURE {
        result
    } else {
        ptr::null_mut()
    }
}

#[link(name = "c")]
extern "C" {
    //  Returns the current index of the CPU and NUMA node on which the thread is executed.
    //
    //  `_tcache` is a legacy parameter, no longer used, and should be null.
    //
    //  The only possible error is EFAULT, for arguments pointing outside the address space.
    fn getcpu(cpu: *mut u32, node: *mut u32, _tcache: *mut u8) -> i32;

    //  Refer to: https://man7.org/linux/man-pages/man2/mmap.2.html
    fn mmap(addr: *mut u8, length: usize, prot: i32, flags: i32, fd: i32, offset: isize) -> *mut u8;

    //  Refer to: https://man7.org/linux/man-pages/man2/mmap.2.html
    fn munmap(addr: *mut u8, length: usize) -> i32;
}

#[link(name = "numa")]
extern "C" {
    //  Returns the distance between two NUMA nodes.
    //
    //  A node has a distance 10 to itself; factors should be multiples of 10, although 11 and 21 has been observed.
    fn numa_distance(left: i32, right: i32) -> i32;
}

#[link(name = "pthread")]
extern "C" {
    //  Initializes the value of the thread-local key.
    //
    //  Errors:
    //  -   EAGAIN: if the system lacked the necessary resources.
    //  -   ENOMEM: if insufficient memory exists to create the key.
    fn pthread_key_create(key: *mut u32, destructor: *const u8) -> i32;

    //  Gets the pointer to the thread-local value stored for key, or null.
    fn pthread_getspecific(key: u32) -> *mut u8;

    //  Sets the pointer to the thread-local value stored for key.
    //
    //  Errors:
    //  -   ENOMEM: if insufficient memory exists to associate the value with the key.
    //  -   EINVAL: if the key value is invalid.
    fn pthread_setspecific(key: u32, value: *mut u8) -> i32;

    //  Yields.
    //
    //  Errors:
    //  -   None known.
    fn pthread_yield() -> i32;
}

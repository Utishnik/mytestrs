use crossbeam_utils::CachePadded;
use std::cell::Cell;
use std::fmt;
use std::ops::Deref;
use std::ptr;
use std::sync::Arc;

// ============================================================
//               ПЛАТФОРМЕННЫЙ СЛОЙ ВЫДЕЛЕНИЯ ПАМЯТИ
// ============================================================

struct RawAllocation {
    ptr: *mut u8,
    size: usize,
    is_huge: bool,
}

#[cfg(windows)]
mod platform {
    use super::RawAllocation;
    use std::ptr;
    use windows_sys::Win32::System::Memory::*;

    pub fn try_alloc_huge(size: usize) -> Option<RawAllocation> {
        unsafe {
            let ptr = VirtualAlloc(
                ptr::null_mut(),
                size,
                MEM_COMMIT | MEM_RESERVE | MEM_LARGE_PAGES,
                PAGE_READWRITE,
            );
            if ptr.is_null() {
                None
            } else {
                Some(RawAllocation {
                    ptr: ptr as *mut u8,
                    size,
                    is_huge: true,
                })
            }
        }
    }

    pub fn alloc_normal(size: usize) -> RawAllocation {
        unsafe {
            let ptr = VirtualAlloc(
                ptr::null_mut(),
                size,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            );
            assert!(!ptr.is_null(), "VirtualAlloc failed");
            RawAllocation {
                ptr: ptr as *mut u8,
                size,
                is_huge: false,
            }
        }
    }

    pub fn lock_memory(ptr: *mut u8, size: usize) -> bool {
        unsafe { VirtualLock(ptr as *const _, size) != 0 }
    }

    pub fn free(alloc: RawAllocation) {
        unsafe {
            VirtualFree(alloc.ptr as *mut _, 0, MEM_RELEASE);
        }
    }
}

#[cfg(unix)]
mod platform {
    use super::RawAllocation;
    use std::ptr;

    pub fn try_alloc_huge(size: usize) -> Option<RawAllocation> {
        #[cfg(target_os = "linux")]
        {
            unsafe {
                let ptr = libc::mmap(
                    ptr::null_mut(),
                    size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_HUGETLB,
                    -1,
                    0,
                );
                if ptr == libc::MAP_FAILED {
                    return None;
                }
                Some(RawAllocation {
                    ptr: ptr as *mut u8,
                    size,
                    is_huge: true,
                })
            }
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = size;
            None
        }
    }

    pub fn alloc_normal(size: usize) -> RawAllocation {
        unsafe {
            let ptr = libc::mmap(
                ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            );
            assert!(ptr != libc::MAP_FAILED, "mmap failed");
            RawAllocation {
                ptr: ptr as *mut u8,
                size,
                is_huge: false,
            }
        }
    }

    pub fn lock_memory(ptr: *mut u8, size: usize) -> bool {
        unsafe { libc::mlock(ptr as *const libc::c_void, size) == 0 }
    }

    pub fn free(alloc: RawAllocation) {
        unsafe {
            libc::munmap(alloc.ptr as *mut libc::c_void, alloc.size);
        }
    }
}

// ============================================================
//                  ЕДИНАЯ АРЕНА (КРОССПЛАТФОРМА)
// ============================================================

struct SharedArena {
    alloc: RawAllocation,
}

unsafe impl Send for SharedArena {}
unsafe impl Sync for SharedArena {}

impl SharedArena {
    fn new(total_capacity: usize) -> Self {
        let alloc = platform::try_alloc_huge(total_capacity)
            .unwrap_or_else(|| platform::alloc_normal(total_capacity));

        if verbose_enabled() {
            println!(
                "  [Arena] Выделено {} MB, huge pages: {}",
                total_capacity / (1024 * 1024),
                alloc.is_huge
            );
        }

        // Глобальный pre-fault убран: страницы теперь трогает каждый поток
        // самостоятельно (arena_full/arena_light -> bump.prefault_local()),
        // чтобы обеспечить first-touch в локальной NUMA-ноде.

        // Lock в RAM (best effort) - закомментировано по желанию
        /*
        let locked = platform::lock_memory(alloc.ptr, total_capacity);
        println!(
            "  [Arena] mlock/VirtualLock: {}",
            if locked { "OK" } else { "SKIPPED" }
        );
        */

        Self { alloc }
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.alloc.ptr, self.alloc.size) }
    }

    fn split<'a>(&'a self, num_threads: usize) -> Vec<CachePadded<ThreadBump<'a>>> {
        let memory = self.as_slice();
        let chunk_size = memory.len() / num_threads;

        (0..num_threads)
            .map(|i| {
                let start = i * chunk_size;
                let end = if i == num_threads - 1 {
                    memory.len()
                } else {
                    start + chunk_size
                };

                CachePadded::new(ThreadBump {
                    memory: &memory[start..end],
                    offset: Cell::new(0),
                })
            })
            .collect()
    }
}

impl Drop for SharedArena {
    fn drop(&mut self) {
        let alloc = RawAllocation {
            ptr: self.alloc.ptr,
            size: self.alloc.size,
            is_huge: self.alloc.is_huge,
        };
        platform::free(alloc);
    }
}

// ============================================================
//            THREAD-LOCAL BUMP (БЕЗ СИНХРОНИЗАЦИИ)
// ============================================================

struct ThreadBump<'a> {
    memory: &'a [u8],
    offset: Cell<usize>,
}

impl<'a> ThreadBump<'a> {
    #[inline(always)]
    fn alloc_raw(&self, size: usize, align: usize) -> *mut u8 {
        let current = self.offset.get();
        let aligned = (current + align - 1) & !(align - 1);
        let new_offset = aligned + size;

        if new_offset > self.memory.len() {
            panic!(
                "Arena OOM: need {} at offset {}, cap {}",
                size,
                aligned,
                self.memory.len()
            );
        }

        self.offset.set(new_offset);
        unsafe { self.memory.as_ptr().add(aligned) as *mut u8 }
    }

    #[inline(always)]
    fn alloc_str(&self, s: &str) -> &str {
        let bytes = s.as_bytes();
        let ptr = self.alloc_raw(bytes.len(), 1);
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(ptr, bytes.len()))
        }
    }

    #[inline(always)]
    fn alloc_uninit_slice<T>(&self, count: usize) -> *mut T {
        if count == 0 {
            return ptr::null_mut();
        }
        let size = count * core::mem::size_of::<T>();
        let align = core::mem::align_of::<T>();
        self.alloc_raw(size, align) as *mut T
    }

    fn reset(&self) {
        self.offset.set(0);
    }

    /// First-touch: трогаем каждую страницу из текущего потока, чтобы она
    /// отмапилась в локальной NUMA-ноде этого потока (после set_for_current).
    fn prefault_local(&self) {
        unsafe {
            let ptr = self.memory.as_ptr() as *mut u8;
            let len = self.memory.len();
            for i in (0..len).step_by(4096) {
                ptr::write_volatile(ptr.add(i), 0u8);
            }
            std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn allocated_bytes(&self) -> usize {
        self.offset.get()
    }
}

// ============================================================
//          ARENA VEC / ARENA STRING (ВСЁ ИЗ ОДНОГО БУФЕРА)
// ============================================================

struct ArenaVec<T> {
    ptr: *mut T,
    len: usize,
    cap: usize,
}

impl<T> ArenaVec<T> {
    #[hotpath::measure]
    #[inline(always)]
    fn with_capacity_in(capacity: usize, bump: &ThreadBump) -> Self {
        if capacity == 0 {
            return Self {
                ptr: ptr::null_mut(),
                len: 0,
                cap: 0,
            };
        }
        let ptr = bump.alloc_uninit_slice::<T>(capacity);
        Self {
            ptr,
            len: 0,
            cap: capacity,
        }
    }

    #[inline(always)]
    fn push(&mut self, value: T, bump: &ThreadBump) {
        if self.len == self.cap {
            self.grow(bump);
        }
        unsafe {
            self.ptr.add(self.len).write(value);
        }
        self.len += 1;
    }

    fn from_slice_in(slice: &[T], bump: &ThreadBump) -> Self
    where
        T: Copy,
    {
        let len = slice.len();
        let ptr = bump.alloc_uninit_slice::<T>(len);
        unsafe {
            ptr::copy_nonoverlapping(slice.as_ptr(), ptr, len);
        }
        Self { ptr, len, cap: len }
    }

    fn grow(&mut self, bump: &ThreadBump) {
        let new_cap = if self.cap == 0 { 4 } else { self.cap * 2 };
        let new_ptr = bump.alloc_uninit_slice::<T>(new_cap);
        if self.len > 0 {
            unsafe {
                ptr::copy_nonoverlapping(self.ptr, new_ptr, self.len);
            }
        }
        self.ptr = new_ptr;
        self.cap = new_cap;
    }

    /// Возвращает срез элементов (безопасно, если len > 0)
    fn as_slice(&self) -> &[T] {
        if self.len == 0 {
            return &[];
        }
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl<T> Drop for ArenaVec<T> {
    fn drop(&mut self) {
        for i in 0..self.len {
            unsafe {
                self.ptr.add(i).drop_in_place();
            }
        }
    }
}

// ---------- ArenaString (владеющая строка на базе ArenaVec<u8>) ----------

struct ArenaString {
    vec: ArenaVec<u8>,
}

impl ArenaString {
    #[inline(always)]
    fn from_str_in(s: &str, bump: &ThreadBump) -> Self {
        let vec = ArenaVec::from_slice_in(s.as_bytes(), bump);
        Self { vec }
    }
}

impl Deref for ArenaString {
    type Target = str;
    fn deref(&self) -> &str {
        // Безопасно: мы создаём только из &str и не изменяем байты после
        unsafe { std::str::from_utf8_unchecked(self.vec.as_slice()) }
    }
}

impl fmt::Display for ArenaString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.deref())
    }
}

// ============================================================
//               ОСНОВНЫЕ БЕНЧМАРК-ФУНКЦИИ
// ============================================================

use bumpalo::Bump;
use bumpalo::collections::String as BumpString;
use bumpalo::collections::Vec as BumpVec;
use mimalloc::MiMalloc;
use spin::Mutex as SpinMutex;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

// --- MiMalloc ---
pub fn mimm(smt: bool) {
    let core_ids = get_cores(smt);
    std::thread::scope(|s| {
        for core_id in core_ids.iter() {
            s.spawn(move || {
                core_affinity::set_for_current(*core_id);
                for _ in 0..3 {
                    let mut vectr: Vec<Vec<String>> = Vec::with_capacity(40000);
                    for _ in 0..200 {
                        for _ in 0..200 {
                            let mut vec = Vec::with_capacity(400);
                            for _ in 0..100 {
                                vec.push("stroka".to_string());
                            }
                            vectr.push(vec);
                        }
                    }
                    core::hint::black_box(&vectr);
                }
            });
        }
    });
}

pub fn mimm_light(smt: bool) {
    let core_ids = get_cores(smt);
    std::thread::scope(|s| {
        for core_id in core_ids.iter() {
            s.spawn(move || {
                core_affinity::set_for_current(*core_id);
                for _ in 0..3 {
                    let mut vectr: Vec<Vec<String>> = Vec::with_capacity(10000);
                    for _ in 0..100 {
                        for _ in 0..100 {
                            let mut vec = Vec::with_capacity(400);
                            for _ in 0..100 {
                                vec.push("stroka".to_string());
                            }
                            vectr.push(vec);
                        }
                    }
                    core::hint::black_box(&vectr);
                }
            });
        }
    });
}

// --- Per-thread bumpalo ---
pub fn bump_scope_m(chunk_size: usize, smt: bool) {
    let core_ids = get_cores(smt);
    std::thread::scope(|s| {
        for core_id in core_ids.iter() {
            s.spawn(move || {
                core_affinity::set_for_current(*core_id);
                let mut bump = Bump::with_capacity(chunk_size);
                for _ in 0..3 {
                    let capacity = 4 * 100 * 100;
                    let mut vectr = BumpVec::with_capacity_in(capacity, &bump);
                    for _ in 0..200 {
                        for _ in 0..200 {
                            let mut vec = BumpVec::with_capacity_in(400, &bump);
                            for _ in 0..100 {
                                vec.push(BumpString::from_str_in("stroka", &bump));
                            }
                            vectr.push(vec);
                        }
                    }
                    core::hint::black_box(&vectr);
                    drop(vectr); // <-- явно дропаем vectr
                    bump.reset(); // теперь можно сбросить
                }
            });
        }
    });
}

pub fn bump_scope_m_light(chunk_size: usize, smt: bool) {
    let core_ids = get_cores(smt);
    std::thread::scope(|s| {
        for core_id in core_ids.iter() {
            s.spawn(move || {
                core_affinity::set_for_current(*core_id);
                let mut bump = Bump::with_capacity(chunk_size);
                for _ in 0..3 {
                    let capacity = 100 * 100;
                    let mut vectr = BumpVec::with_capacity_in(capacity, &bump);
                    for _ in 0..100 {
                        for _ in 0..100 {
                            let mut vec = BumpVec::with_capacity_in(400, &bump);
                            for _ in 0..100 {
                                vec.push(BumpString::from_str_in("stroka", &bump));
                            }
                            vectr.push(vec);
                        }
                    }
                    core::hint::black_box(&vectr);
                    drop(vectr);
                    bump.reset();
                }
            });
        }
    });
}

// --- Shared bump + spinmutex ---
fn do_work_full(bump: &Bump) {
    let capacity = 4 * 100 * 100;
    let mut vectr = BumpVec::with_capacity_in(capacity, bump);
    for _ in 0..200 {
        for _ in 0..200 {
            let mut vec = BumpVec::with_capacity_in(400, bump);
            for _ in 0..100 {
                vec.push(BumpString::from_str_in("stroka", bump));
            }
            vectr.push(vec);
        }
    }
    core::hint::black_box(&vectr);
}

fn do_work_light(bump: &Bump) {
    let capacity = 100 * 100;
    let mut vectr = BumpVec::with_capacity_in(capacity, bump);
    for _ in 0..100 {
        for _ in 0..100 {
            let mut vec = BumpVec::with_capacity_in(400, bump);
            for _ in 0..100 {
                vec.push(BumpString::from_str_in("stroka", bump));
            }
            vectr.push(vec);
        }
    }
    core::hint::black_box(&vectr);
}

pub fn bump_shared_m(chunk_size: usize, smt: bool) {
    let core_ids = get_cores(smt);
    let total = chunk_size * core_ids.len() * 2;
    let shared = Arc::new(SpinMutex::new(Bump::with_capacity(total)));

    std::thread::scope(|s| {
        for core_id in core_ids.iter() {
            let shared = Arc::clone(&shared);
            s.spawn(move || {
                core_affinity::set_for_current(*core_id);
                for _ in 0..3 {
                    let mut guard = shared.lock();
                    do_work_full(&guard);
                    guard.reset();
                }
            });
        }
    });
}

pub fn bump_shared_m_light(chunk_size: usize, smt: bool) {
    let core_ids = get_cores(smt);
    let total = chunk_size * core_ids.len() * 2;
    let shared = Arc::new(SpinMutex::new(Bump::with_capacity(total)));

    std::thread::scope(|s| {
        for core_id in core_ids.iter() {
            let shared = Arc::clone(&shared);
            s.spawn(move || {
                core_affinity::set_for_current(*core_id);
                for _ in 0..3 {
                    let mut guard = shared.lock();
                    do_work_light(&guard);
                    guard.reset();
                }
            });
        }
    });
}

// --- Shared arena (single big buffer) ---
pub fn arena_full(chunk_size: usize, smt: bool) {
    let core_ids = get_cores(smt);
    let total_capacity = chunk_size * core_ids.len();
    if verbose_enabled() {
        println!("[TOTAL CAPACITY]:  {}", total_capacity);
    }
    let arena = SharedArena::new(total_capacity);
    let bumps = arena.split(core_ids.len());

    std::thread::scope(|s| {
        for (core_id, bump) in core_ids.iter().zip(bumps) {
            s.spawn(move || {
                core_affinity::set_for_current(*core_id);
                hotpath::measure_block!("prefault", {
                    bump.prefault_local(); // first-touch в локальной NUMA-ноде
                });
                for _ in 0..3 {
                    hotpath::measure_block!("alloc", {
                        let mut vectr: ArenaVec<ArenaVec<ArenaString>> =
                            ArenaVec::with_capacity_in(40000, &bump);
                        for _ in 0..200 {
                            for _ in 0..200 {
                                let mut vec: ArenaVec<ArenaString> =
                                    ArenaVec::with_capacity_in(400, &bump);
                                for _ in 0..100 {
                                    vec.push(ArenaString::from_str_in("stroka", &bump), &bump);
                                }
                                vectr.push(vec, &bump);
                            }
                        }
                        core::hint::black_box(&vectr);
                        drop(vectr);
                    });
                    hotpath::measure_block!("reset", {
                        bump.reset();
                    });
                }
            });
        }
    });
}

pub fn arena_light(chunk_size: usize, smt: bool) {
    let core_ids = get_cores(smt);
    let total_capacity = chunk_size * core_ids.len();
    if verbose_enabled() {
        println!("[TOTAL CAPACITY]:  {}", total_capacity);
    }
    let arena = SharedArena::new(total_capacity);
    let bumps = arena.split(core_ids.len());

    std::thread::scope(|s| {
        for (core_id, bump) in core_ids.iter().zip(bumps.into_iter()) {
            s.spawn(move || {
                core_affinity::set_for_current(*core_id);
                hotpath::measure_block!("prefault", {
                    bump.prefault_local(); // first-touch в локальной NUMA-ноде
                });
                for _ in 0..3 {
                    hotpath::measure_block!("alloc", {
                        let mut vectr: ArenaVec<ArenaVec<ArenaString>> =
                            ArenaVec::with_capacity_in(10000, &bump);
                        for _ in 0..100 {
                            for _ in 0..100 {
                                let mut vec: ArenaVec<ArenaString> =
                                    ArenaVec::with_capacity_in(400, &bump);
                                for _ in 0..100 {
                                    vec.push(ArenaString::from_str_in("stroka", &bump), &bump);
                                }
                                vectr.push(vec, &bump);
                            }
                        }
                        core::hint::black_box(&vectr);
                        drop(vectr);
                    });
                    hotpath::measure_block!("reset", {
                        bump.reset();
                    });
                }
            });
        }
    });
}

// ============================================================
//                        PGO
// ============================================================

pub fn profile_bump_chunk_size_full() -> usize {
    let bump = Bump::new();
    let capacity = 4 * 100 * 100;
    let mut vectr = BumpVec::with_capacity_in(capacity, &bump);
    for _ in 0..200 {
        for _ in 0..200 {
            let mut vec = BumpVec::with_capacity_in(400, &bump);
            for _ in 0..100 {
                vec.push(BumpString::from_str_in("stroka", &bump));
            }
            vectr.push(vec);
        }
    }
    let used = bump.allocated_bytes();
    core::hint::black_box(&vectr);
    drop(vectr);
    let recommended = used * 120 / 100;
    ((recommended + (1024 * 1024 - 1)) / (1024 * 1024)) * (1024 * 1024)
}

pub fn profile_bump_chunk_size_light() -> usize {
    let bump = Bump::new();
    let capacity = 100 * 100;
    let mut vectr = BumpVec::with_capacity_in(capacity, &bump);
    for _ in 0..100 {
        for _ in 0..100 {
            let mut vec = BumpVec::with_capacity_in(400, &bump);
            for _ in 0..100 {
                vec.push(BumpString::from_str_in("stroka", &bump));
            }
            vectr.push(vec);
        }
    }
    let used = bump.allocated_bytes();
    core::hint::black_box(&vectr);
    drop(vectr);
    let recommended = used * 120 / 100;
    ((recommended + (1024 * 1024 - 1)) / (1024 * 1024)) * (1024 * 1024)
}

pub fn profile_arena_chunk_size_full() -> usize {
    let arena = SharedArena::new(1024 * 1024 * 1024);
    let bumps = arena.split(1);
    let bump = &bumps[0];

    let mut vectr: ArenaVec<ArenaVec<ArenaString>> = ArenaVec::with_capacity_in(40000, bump);
    for _ in 0..200 {
        for _ in 0..200 {
            let mut vec: ArenaVec<ArenaString> = ArenaVec::with_capacity_in(400, bump);
            for _ in 0..100 {
                vec.push(ArenaString::from_str_in("stroka", bump), bump);
            }
            vectr.push(vec, bump);
        }
    }
    let used = bump.allocated_bytes();
    core::hint::black_box(&vectr);
    drop(vectr);
    let recommended = used * 105 / 100;
    ((recommended + (1024 * 1024 - 1)) / (1024 * 1024)) * (1024 * 1024)
}

pub fn profile_arena_chunk_size_light() -> usize {
    let arena = SharedArena::new(512 * 1024 * 1024);
    let bumps = arena.split(1);
    let bump = &bumps[0];

    let mut vectr: ArenaVec<ArenaVec<ArenaString>> = ArenaVec::with_capacity_in(10000, bump);
    for _ in 0..100 {
        for _ in 0..100 {
            let mut vec: ArenaVec<ArenaString> = ArenaVec::with_capacity_in(400, bump);
            for _ in 0..100 {
                vec.push(ArenaString::from_str_in("stroka", bump), bump);
            }
            vectr.push(vec, bump);
        }
    }
    let used = bump.allocated_bytes();
    core::hint::black_box(&vectr);
    drop(vectr);
    let recommended = used * 105 / 100;
    ((recommended + (1024 * 1024 - 1)) / (1024 * 1024)) * (1024 * 1024)
}

// ============================================================
//                        УТИЛИТЫ
// ============================================================

/// Отладочные принты арены показываются только при R3_VERBOSE=1,
/// чтобы не засорять вывод `cargo bench`.
fn verbose_enabled() -> bool {
    std::env::var("R3_VERBOSE").is_ok()
}

fn get_cores(smt: bool) -> Vec<core_affinity::CoreId> {
    let all = core_affinity::get_core_ids().unwrap();
    if smt {
        all
    } else {
        let physical_count = num_cpus::get_physical();
        all.into_iter().take(physical_count).collect()
    }
}

fn median(times: &[u128]) -> u128 {
    let mut sorted = times.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

// ============================================================
//                          MAIN
// ============================================================

#[hotpath::main]
pub fn run() {
    let all_cores = core_affinity::get_core_ids().unwrap();
    println!("Логических ядер: {}", all_cores.len());
    println!("Физических ядер: {}", num_cpus::get_physical());
    println!();

    // PGO для bumpalo
    let pgo_full_bump = profile_bump_chunk_size_full();
    let pgo_light_bump = profile_bump_chunk_size_light();
    println!("PGO bump full: {} MB", pgo_full_bump / (1024 * 1024));
    println!("PGO bump light: {} MB", pgo_light_bump / (1024 * 1024));
    println!();

    // PGO для shared arena
    let pgo_full_arena = profile_arena_chunk_size_full();
    let pgo_light_arena = profile_arena_chunk_size_light();
    println!("PGO arena full: {} MB", pgo_full_arena / (1024 * 1024));
    println!("PGO arena light: {} MB", pgo_light_arena / (1024 * 1024));
    println!();

    run_benchmarks(
        true,
        pgo_full_bump,
        pgo_light_bump,
        pgo_full_arena,
        pgo_light_arena,
    );
    run_benchmarks(
        false,
        pgo_full_bump,
        pgo_light_bump,
        pgo_full_arena,
        pgo_light_arena,
    );
}

fn run_benchmarks(
    smt: bool,
    pgo_full_bump: usize,
    pgo_light_bump: usize,
    pgo_full_arena: usize,
    pgo_light_arena: usize,
) {
    let mode_str = if smt {
        "SMT (all logical cores)"
    } else {
        "NO SMT (physical cores only)"
    };
    println!("\n########## Режим: {} ##########\n", mode_str);

    // Прогрев
    for _ in 0..5 {
        mimm(smt);
        mimm_light(smt);
        bump_scope_m(pgo_full_bump, smt);
        bump_scope_m_light(pgo_light_bump, smt);
        bump_shared_m(pgo_full_bump, smt);
        bump_shared_m_light(pgo_light_bump, smt);
        arena_full(pgo_full_arena, smt);
        arena_light(pgo_light_arena, smt);
    }

    println!("=== FULL VERSION ({}) ===", mode_str);
    for round in 0..3 {
        let mimm_times: Vec<u128> = (0..10)
            .map(|_| {
                let start = std::time::Instant::now();
                mimm(smt);
                start.elapsed().as_micros()
            })
            .collect();
        let bump_times: Vec<u128> = (0..10)
            .map(|_| {
                let start = std::time::Instant::now();
                bump_scope_m(pgo_full_bump, smt);
                start.elapsed().as_micros()
            })
            .collect();
        let shared_times: Vec<u128> = (0..10)
            .map(|_| {
                let start = std::time::Instant::now();
                bump_shared_m(pgo_full_bump, smt);
                start.elapsed().as_micros()
            })
            .collect();
        let arena_times: Vec<u128> = (0..10)
            .map(|_| {
                let start = std::time::Instant::now();
                arena_full(pgo_full_arena, smt);
                start.elapsed().as_micros()
            })
            .collect();

        println!(
            "Round {}: MIMALOC = {} µs, Bump = {} µs, SharedBump = {} µs, Arena = {} µs",
            round + 1,
            median(&mimm_times),
            median(&bump_times),
            median(&shared_times),
            median(&arena_times)
        );
    }

    println!("\n=== LIGHT VERSION ({}) ===", mode_str);
    for round in 0..3 {
        let mimm_times: Vec<u128> = (0..10)
            .map(|_| {
                let start = std::time::Instant::now();
                mimm_light(smt);
                start.elapsed().as_micros()
            })
            .collect();
        let bump_times: Vec<u128> = (0..10)
            .map(|_| {
                let start = std::time::Instant::now();
                bump_scope_m_light(pgo_light_bump, smt);
                start.elapsed().as_micros()
            })
            .collect();
        let shared_times: Vec<u128> = (0..10)
            .map(|_| {
                let start = std::time::Instant::now();
                bump_shared_m_light(pgo_light_bump, smt);
                start.elapsed().as_micros()
            })
            .collect();
        let arena_times: Vec<u128> = (0..10)
            .map(|_| {
                let start = std::time::Instant::now();
                arena_light(pgo_light_arena, smt);
                start.elapsed().as_micros()
            })
            .collect();

        println!(
            "Round {}: MIMALOC = {} µs, Bump = {} µs, SharedBump = {} µs, Arena = {} µs",
            round + 1,
            median(&mimm_times),
            median(&bump_times),
            median(&shared_times),
            median(&arena_times)
        );
    }
}

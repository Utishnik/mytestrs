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
    use super::{RawAllocation, verbose_enabled};
    use std::ptr;
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_NOT_ALL_ASSIGNED, GetLastError, HANDLE, LUID,
    };
    use windows_sys::Win32::Security::{
        AdjustTokenPrivileges, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Memory::*;
    use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// Включаем SeLockMemoryPrivilege ("Lock pages in memory"), если она
    /// выдана процессу в локальной политике безопасности. Без неё
    /// VirtualAlloc(MEM_LARGE_PAGES) не работает, и мы молча откатываемся
    /// на обычные 4KB страницы.
    fn enable_lock_memory_privilege() {
        unsafe {
            let mut token: HANDLE = ptr::null_mut();
            if OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &mut token,
            ) == 0
            {
                return;
            }

            let mut luid: LUID = core::mem::zeroed();
            let found = LookupPrivilegeValueW(
                ptr::null(),
                windows_sys::w!("SeLockMemoryPrivilege"),
                &mut luid,
            ) != 0;

            let mut enabled = false;
            if found {
                let mut tp: TOKEN_PRIVILEGES = core::mem::zeroed();
                tp.PrivilegeCount = 1;
                tp.Privileges[0].Luid = luid;
                tp.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;
                let adjusted =
                    AdjustTokenPrivileges(token, 0, &tp, 0, ptr::null_mut(), ptr::null_mut()) != 0;
                // AdjustTokenPrivileges может вернуть успех, но не включить
                // привилегию — тогда GetLastError == ERROR_NOT_ALL_ASSIGNED.
                enabled = adjusted && GetLastError() != ERROR_NOT_ALL_ASSIGNED;
            }
            CloseHandle(token);

            if verbose_enabled() {
                println!(
                    "  [Arena] SeLockMemoryPrivilege: {}",
                    if enabled {
                        "ENABLED"
                    } else {
                        "недоступна (large pages выключены)"
                    }
                );
            }
        }
    }

    pub fn try_alloc_huge(size: usize) -> Option<RawAllocation> {
        static PRIV: OnceLock<()> = OnceLock::new();
        PRIV.get_or_init(enable_lock_memory_privilege);

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

    #[allow(dead_code)]
    pub fn lock_memory(ptr: *mut u8, size: usize) -> bool {
        unsafe { VirtualLock(ptr as *const _, size) != 0 }
    }

    pub fn free(alloc: RawAllocation) {
        unsafe {
            VirtualFree(alloc.ptr as *mut _, 0, MEM_RELEASE);
        }
    }

    /// Размер обычной страницы (обычно 4 KB).
    pub fn page_size() -> usize {
        static PAGE: OnceLock<usize> = OnceLock::new();
        *PAGE.get_or_init(|| unsafe {
            let mut info: SYSTEM_INFO = core::mem::zeroed();
            GetSystemInfo(&mut info);
            info.dwPageSize as usize
        })
    }

    /// Минимальный размер large page (обычно 2 MB).
    pub fn huge_page_size() -> usize {
        static HUGE: OnceLock<usize> = OnceLock::new();
        *HUGE.get_or_init(|| unsafe { GetLargePageMinimum() })
    }

    /// На Windows large pages выделяются физически сразу в VirtualAlloc —
    /// demand paging для них отсутствует, prefault не нужен.
    pub fn large_pages_precommitted() -> bool {
        true
    }

    /// Аналога MADV_POPULATE_WRITE на Windows нет.
    pub fn populate_write(_ptr: *mut u8, _len: usize) -> bool {
        false
    }

    /// Асинхронный prefetch через PrefetchVirtualMemory. По умолчанию
    /// ВЫКЛЮЧЕН: на практике worker-потоки ядра фолтят страницы в другом
    /// контексте (конкуренция за working-set lock, чужая NUMA-нода), что
    /// только замедляет prefault. Включается через R3_ASYNC_PREFETCH=1
    /// для экспериментов.
    pub fn prefetch_async(ptr: *mut u8, len: usize) {
        if len == 0 || std::env::var_os("R3_ASYNC_PREFETCH").is_none() {
            return;
        }
        unsafe {
            let range = WIN32_MEMORY_RANGE_ENTRY {
                VirtualAddress: ptr as *mut _,
                NumberOfBytes: len,
            };
            PrefetchVirtualMemory(GetCurrentProcess(), 1, &range, 0);
        }
    }
}

#[cfg(unix)]
mod platform {
    use super::RawAllocation;
    use std::ptr;
    use std::sync::OnceLock;

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

    #[allow(dead_code)]
    pub fn lock_memory(ptr: *mut u8, size: usize) -> bool {
        unsafe { libc::mlock(ptr as *const libc::c_void, size) == 0 }
    }

    pub fn free(alloc: RawAllocation) {
        unsafe {
            libc::munmap(alloc.ptr as *mut libc::c_void, alloc.size);
        }
    }

    /// Размер обычной страницы (обычно 4 KB).
    pub fn page_size() -> usize {
        static PAGE: OnceLock<usize> = OnceLock::new();
        *PAGE.get_or_init(|| unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize })
    }

    /// Реальный размер huge-страницы (Hugepagesize из /proc/meminfo); с такой
    /// гранулярностью фолтится регион с MAP_HUGETLB.
    pub fn huge_page_size() -> usize {
        static HUGE: OnceLock<usize> = OnceLock::new();
        *HUGE.get_or_init(|| {
            #[cfg(target_os = "linux")]
            {
                if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
                    if let Some(kb) = meminfo.lines().find_map(|line| {
                        line.strip_prefix("Hugepagesize:")
                            .and_then(|rest| rest.trim().strip_suffix("kB"))
                            .and_then(|num| num.trim().parse::<usize>().ok())
                    }) {
                        return kb * 1024;
                    }
                }
            }
            2 * 1024 * 1024
        })
    }

    /// На Linux MAP_HUGETLB всё равно demand-fault'ится (по одной huge-странице),
    /// поэтому prefault нужен.
    pub fn large_pages_precommitted() -> bool {
        false
    }

    /// Быстрый prefault на запись одним syscall (Linux >= 5.14). Страницы
    /// фолтятся в контексте вызывающего потока, поэтому NUMA first-touch
    /// сохраняется. На старых ядрах вернёт EINVAL -> false -> fallback.
    pub fn populate_write(ptr: *mut u8, len: usize) -> bool {
        #[cfg(target_os = "linux")]
        unsafe {
            libc::madvise(ptr as *mut libc::c_void, len, libc::MADV_POPULATE_WRITE) == 0
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (ptr, len);
            false
        }
    }

    /// На Linux асинхронный prefetch не нужен: madvise покрывает всё синхронно.
    pub fn prefetch_async(_ptr: *mut u8, _len: usize) {}
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

    fn split(&self, num_threads: usize) -> Vec<CachePadded<ThreadBump>> {
        self.split_with(num_threads, BumpDir::Forward)
    }

    /// Разбить арену на `num_threads` непересекающихся чанков заданного
    /// направления заполнения. Для `MiddleOut` каждый чанк стартует из своей
    /// середины.
    fn split_with(&self, num_threads: usize, dir: BumpDir) -> Vec<CachePadded<ThreadBump>> {
        let base = self.alloc.ptr;
        let total = self.alloc.size;
        let chunk_size = total / num_threads;

        (0..num_threads)
            .map(|i| {
                let start = i * chunk_size;
                let end = if i == num_threads - 1 {
                    total
                } else {
                    start + chunk_size
                };
                let len = end - start;
                CachePadded::new(ThreadBump {
                    ptr: unsafe { base.add(start) },
                    len,
                    lo: Cell::new(0),
                    hi: Cell::new(0),
                    toggle: Cell::new(false),
                    dir,
                    is_huge: self.alloc.is_huge,
                })
            })
            .collect()
    }

    /// Чётные потоки заполняют свой чанк справа налево (`Backward`), нечётные —
    /// слева направо (`Forward`). Так соседние чанки «доходят» до общей границы
    /// навстречу друг другу (идея совместного использования памяти соседа).
    fn split_alternating(&self, num_threads: usize) -> Vec<CachePadded<ThreadBump>> {
        let base = self.alloc.ptr;
        let total = self.alloc.size;
        let chunk_size = total / num_threads;

        (0..num_threads)
            .map(|i| {
                let dir = if i % 2 == 0 {
                    BumpDir::Backward
                } else {
                    BumpDir::Forward
                };
                let start = i * chunk_size;
                let end = if i == num_threads - 1 {
                    total
                } else {
                    start + chunk_size
                };
                let len = end - start;
                CachePadded::new(ThreadBump {
                    ptr: unsafe { base.add(start) },
                    len,
                    lo: Cell::new(0),
                    hi: Cell::new(0),
                    toggle: Cell::new(false),
                    dir,
                    is_huge: self.alloc.is_huge,
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

/// Направление заполнения thread-local bump-аллокатора.
///
/// * `Forward`  — стандарт: слева направо (cursor растёт от 0 вверх).
/// * `Backward` — справа налево (right-to-left). Соседний поток справа,
///   заполняющий свой чанк слева направо, «доходит» до той же границы —
///   отсюда идея, что сосед может дотянуться до чужой памяти.
/// * `MiddleOut`— старт из середины чанка с чередованием вправо/влево.
///   Два соседа, встречаясь от середин своих чанков, делят общую границу.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BumpDir {
    Forward,
    Backward,
    MiddleOut,
}

struct ThreadBump {
    ptr: *mut u8,
    len: usize,
    /// Байт, выделенный с левого края (для Forward — сама граница; для
    /// MiddleOut — величина левой «половины» от середины).
    lo: Cell<usize>,
    /// Байт, выделенный с правого края.
    hi: Cell<usize>,
    /// Для MiddleOut: чётность следующего выделения (false -> влево, true -> вправо).
    toggle: Cell<bool>,
    dir: BumpDir,
    is_huge: bool,
}

// Каждый ThreadBump владеет непересекающимся регионом памяти арены и
// используется ровно одним потоком, поэтому сырой указатель безопасно
// объявить Send (как и для любого per-thread arena-аллокатора).
unsafe impl Send for ThreadBump {}

/// Выравнивание любого типа как ассоциированная константа. Позволяет
/// передать `align_of::<T>()` в позицию const-generic аргумента (прямой
/// вызов `align_of::<T>()` в аргументе const-generic запрещён компилятором,
/// а через ассоциированную константу — разрешён).
impl ThreadBump {
    #[inline(always)]
    fn alloc_raw<const ALIGN: usize>(&self, size: usize) -> *mut u8 {
        // `lo` — байт выделено с левого края (для Backward/MiddleOut семантика
        // своя, см. ниже). `hi` — байт выделено с правого края.
        // ALIGN — константа времени компиляции (задаётся в точке вызова,
        // см. alloc_uninit_slice), поэтому маска выравнивания считается на этапе
        // компиляции, а не прокидывается через аргумент в рантайме.
        let (off, new_lo, new_hi) = match self.dir {
            // Forward: растём от начала чанка вверх.
            BumpDir::Forward => {
                let off = (self.lo.get() + ALIGN - 1) & !(ALIGN - 1);
                let end = off + size;
                if end > self.len {
                    panic!(
                        "Arena OOM: need {} at offset {}, free {} (cap {})",
                        size,
                        off,
                        self.len - self.lo.get(),
                        self.len
                    );
                }
                (off, end, 0)
            }
            // Backward: растём от конца чанка вниз.
            BumpDir::Backward => {
                if size > self.len - self.hi.get() {
                    panic!(
                        "Arena OOM: need {} bytes, only {} free (cap {})",
                        size,
                        self.len - self.hi.get(),
                        self.len
                    );
                }
                let off = (self.len - self.hi.get() - size) & !(ALIGN - 1);
                (off, 0, self.len - off)
            }
            // MiddleOut: из середины наружу, чередуя стороны.
            BumpDir::MiddleOut => {
                let mid = self.len / 2;
                let side = self.toggle.get();
                self.toggle.set(!side);
                if !side {
                    // левая сторона: занята [mid - lo, mid), растёт вниз
                    let base = mid - self.lo.get();
                    if size > base {
                        panic!(
                            "Arena OOM: need {} at offset {}, free {} (cap {})",
                            size,
                            mid - self.lo.get(),
                            base,
                            self.len
                        );
                    }
                    let off = (base - size) & !(ALIGN - 1);
                    (off, mid - off, self.hi.get())
                } else {
                    // правая сторона: занята [mid, mid + hi), растёт вверх
                    let base = mid + self.hi.get();
                    let off = (base + ALIGN - 1) & !(ALIGN - 1);
                    let end = off + size;
                    if end > self.len {
                        panic!(
                            "Arena OOM: need {} at offset {}, free {} (cap {})",
                            size,
                            off,
                            self.len - end,
                            self.len
                        );
                    }
                    (off, self.lo.get(), off + size - mid)
                }
            }
        };

        self.lo.set(new_lo);
        self.hi.set(new_hi);
        unsafe { self.ptr.add(off) }
    }

    #[inline(always)]
    fn alloc_uninit_slice<T, const ALIGN: usize>(&self, count: usize) -> *mut T {
        if count == 0 {
            return ptr::null_mut();
        }
        let size = count * core::mem::size_of::<T>();
        // ALIGN прокинут как const generic из точки вызова (см. ArenaVec),
        // поэтому выравнивание известно на этапе компиляции.
        self.alloc_raw::<ALIGN>(size) as *mut T
    }

    fn reset(&self) {
        self.lo.set(0);
        self.hi.set(0);
        self.toggle.set(false);
    }

    /// First-touch: фолтим страницы ИСПОЛЬЗОВАННЫХ областей из текущего
    /// потока, чтобы они легли в локальную NUMA-ноду.
    fn prefault_local(&self) {
        if self.is_huge && platform::large_pages_precommitted() {
            return;
        }
        let (lo, hi) = (self.lo.get(), self.hi.get());
        match self.dir {
            BumpDir::Forward => self.prefault_range(0, lo),
            BumpDir::Backward => {
                let start = self.len - hi;
                if start < self.len {
                    self.prefault_range(start, self.len);
                }
            }
            BumpDir::MiddleOut => {
                let mid = self.len / 2;
                // левая половина [mid - lo, mid) и правая [mid, mid + hi)
                self.prefault_range(mid - lo, mid);
                let rend = mid + hi;
                if rend > mid {
                    self.prefault_range(mid, rend);
                }
            }
        }
    }

    /// Фолт поддиапазона [start, end) с OS-оптимизациями и touch-loop fallback.
    fn prefault_range(&self, start: usize, end: usize) {
        if start >= end {
            return;
        }
        let len = end - start;
        let ptr = unsafe { self.ptr.add(start) };
        if platform::populate_write(ptr, len) {
            return;
        }
        platform::prefetch_async(ptr, len);
        let stride = if self.is_huge {
            platform::huge_page_size()
        } else {
            platform::page_size()
        };
        unsafe {
            let mut i = start;
            while i < end {
                ptr::write_volatile(self.ptr.add(i), 0u8);
                i += stride;
            }
        }
    }

    fn allocated_bytes(&self) -> usize {
        // Forward: lo; Backward: hi; MiddleOut: lo (левая половина) + hi (правая).
        self.lo.get() + self.hi.get()
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
    fn with_capacity_in<const ALIGN: usize>(capacity: usize, bump: &ThreadBump) -> Self {
        if capacity == 0 {
            return Self {
                ptr: ptr::null_mut(),
                len: 0,
                cap: 0,
            };
        }
        let ptr = bump.alloc_uninit_slice::<T, ALIGN>(capacity);
        Self {
            ptr,
            len: 0,
            cap: capacity,
        }
    }

    #[inline(always)]
    fn push<const ALIGN: usize>(&mut self, value: T, bump: &ThreadBump) {
        if self.len == self.cap {
            self.grow::<ALIGN>(bump);
        }
        unsafe {
            self.ptr.add(self.len).write(value);
        }
        self.len += 1;
    }

    fn from_slice_in<const ALIGN: usize>(slice: &[T], bump: &ThreadBump) -> Self
    where
        T: Copy,
    {
        let len = slice.len();
        let ptr = bump.alloc_uninit_slice::<T, ALIGN>(len);
        unsafe {
            ptr::copy_nonoverlapping(slice.as_ptr(), ptr, len);
        }
        Self { ptr, len, cap: len }
    }

    fn grow<const ALIGN: usize>(&mut self, bump: &ThreadBump) {
        let new_cap = if self.cap == 0 { 4 } else { self.cap * 2 };
        let new_ptr = bump.alloc_uninit_slice::<T, ALIGN>(new_cap);
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
        // элемент строки — u8, выравнивание = 1
        let vec = ArenaVec::from_slice_in::<1>(s.as_bytes(), bump);
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
/// Раскладка чанков арены по потокам.
enum ArenaLayout {
    /// Все чанки одного направления.
    Uniform(BumpDir),
    /// Чётные — Backward, нечётные — Forward (соседи заполняют общую границу
    /// навстречу друг другу).
    Neighbors,
}

/// Единое тело бенчмарка shared-арены. `full` управляет объёмом работы
/// (FULL: 200×200×100, LIGHT: 100×100×100), `layout` — направлением заполнения.
fn arena_bench(chunk_size: usize, smt: bool, full: bool, layout: ArenaLayout) {
    let core_ids = get_cores(smt);
    let total_capacity = chunk_size * core_ids.len();
    if verbose_enabled() {
        println!("[TOTAL CAPACITY]:  {}", total_capacity);
    }
    let arena = SharedArena::new(total_capacity);
    let bumps = match layout {
        ArenaLayout::Uniform(dir) => arena.split_with(core_ids.len(), dir),
        ArenaLayout::Neighbors => arena.split_alternating(core_ids.len()),
    };

    let (vcap, outer, inner) = if full {
        (40000, 200, 200)
    } else {
        (10000, 100, 100)
    };

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
                            ArenaVec::with_capacity_in::<
                                { core::mem::align_of::<ArenaVec<ArenaString>>() },
                            >(vcap, &bump);
                        for _ in 0..outer {
                            for _ in 0..inner {
                                let mut vec: ArenaVec<ArenaString> =
                                    ArenaVec::with_capacity_in::<
                                        { core::mem::align_of::<ArenaString>() },
                                    >(400, &bump);
                                for _ in 0..100 {
                                    vec.push::<{ core::mem::align_of::<ArenaString>() }>(
                                        ArenaString::from_str_in("stroka", &bump),
                                        &bump,
                                    );
                                }
                                vectr.push::<{ core::mem::align_of::<ArenaVec<ArenaString>>() }>(
                                    vec, &bump,
                                );
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

pub fn arena_full(chunk_size: usize, smt: bool) {
    arena_bench(
        chunk_size,
        smt,
        true,
        ArenaLayout::Uniform(BumpDir::Forward),
    );
}

pub fn arena_light(chunk_size: usize, smt: bool) {
    arena_bench(
        chunk_size,
        smt,
        false,
        ArenaLayout::Uniform(BumpDir::Forward),
    );
}

/// Версия `arena_full` с заданным направлением заполнения.
pub fn arena_full_dir(chunk_size: usize, smt: bool, dir: BumpDir) {
    arena_bench(chunk_size, smt, true, ArenaLayout::Uniform(dir));
}

/// Версия `arena_light` с заданным направлением заполнения.
pub fn arena_light_dir(chunk_size: usize, smt: bool, dir: BumpDir) {
    arena_bench(chunk_size, smt, false, ArenaLayout::Uniform(dir));
}

/// Полная версия: чанки соседей заполняют общую границу навстречу друг другу.
pub fn arena_full_neighbors(chunk_size: usize, smt: bool) {
    arena_bench(chunk_size, smt, true, ArenaLayout::Neighbors);
}

/// Лёгкая версия: чанки соседей заполняют общую границу навстречу друг другу.
pub fn arena_light_neighbors(chunk_size: usize, smt: bool) {
    arena_bench(chunk_size, smt, false, ArenaLayout::Neighbors);
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

    let mut vectr: ArenaVec<ArenaVec<ArenaString>> = ArenaVec::with_capacity_in::<
        { core::mem::align_of::<ArenaVec<ArenaString>>() },
    >(40000, bump);
    for _ in 0..200 {
        for _ in 0..200 {
            let mut vec: ArenaVec<ArenaString> =
                ArenaVec::with_capacity_in::<{ core::mem::align_of::<ArenaString>() }>(400, bump);
            for _ in 0..100 {
                vec.push::<{ core::mem::align_of::<ArenaString>() }>(
                    ArenaString::from_str_in("stroka", bump),
                    bump,
                );
            }
            vectr.push::<{ core::mem::align_of::<ArenaVec<ArenaString>>() }>(vec, bump);
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

    let mut vectr: ArenaVec<ArenaVec<ArenaString>> = ArenaVec::with_capacity_in::<
        { core::mem::align_of::<ArenaVec<ArenaString>>() },
    >(10000, bump);
    for _ in 0..100 {
        for _ in 0..100 {
            let mut vec: ArenaVec<ArenaString> =
                ArenaVec::with_capacity_in::<{ core::mem::align_of::<ArenaString>() }>(400, bump);
            for _ in 0..100 {
                vec.push::<{ core::mem::align_of::<ArenaString>() }>(
                    ArenaString::from_str_in("stroka", bump),
                    bump,
                );
            }
            vectr.push::<{ core::mem::align_of::<ArenaVec<ArenaString>>() }>(vec, bump);
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

    run_directional_benchmarks(true, pgo_full_arena, pgo_light_arena);
    run_directional_benchmarks(false, pgo_full_arena, pgo_light_arena);
}

fn run_directional_benchmarks(smt: bool, pgo_full_arena: usize, pgo_light_arena: usize) {
    let mode_str = if smt {
        "SMT (all logical cores)"
    } else {
        "NO SMT (physical cores only)"
    };
    println!("\n########## Directional Arena: {} ##########\n", mode_str);

    let full_variants: &[(&str, fn(usize, bool))] = &[
        ("Forward", |c, s| arena_full_dir(c, s, BumpDir::Forward)),
        ("Backward", |c, s| arena_full_dir(c, s, BumpDir::Backward)),
        ("MiddleOut", |c, s| arena_full_dir(c, s, BumpDir::MiddleOut)),
        ("Neighbors", |c, s| arena_full_neighbors(c, s)),
    ];
    println!("=== Directional FULL ({}) ===", mode_str);
    for (name, f) in full_variants {
        let t: Vec<u128> = (0..10)
            .map(|_| {
                let start = std::time::Instant::now();
                f(pgo_full_arena, smt);
                start.elapsed().as_micros()
            })
            .collect();
        println!("  {:<10}: {} µs", name, median(&t));
    }

    let light_variants: &[(&str, fn(usize, bool))] = &[
        ("Forward", |c, s| arena_light_dir(c, s, BumpDir::Forward)),
        ("Backward", |c, s| arena_light_dir(c, s, BumpDir::Backward)),
        ("MiddleOut", |c, s| {
            arena_light_dir(c, s, BumpDir::MiddleOut)
        }),
        ("Neighbors", |c, s| arena_light_neighbors(c, s)),
    ];
    println!("=== Directional LIGHT ({}) ===", mode_str);
    for (name, f) in light_variants {
        let t: Vec<u128> = (0..10)
            .map(|_| {
                let start = std::time::Instant::now();
                f(pgo_light_arena, smt);
                start.elapsed().as_micros()
            })
            .collect();
        println!("  {:<10}: {} µs", name, median(&t));
    }
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

// ============================================================
//                        ТЕСТЫ
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic;

    /// Один bump заданного направления на `cap` байт.
    /// Возвращает также арену — она должна жить всё время жизни bump'а
    /// (иначе память освободится, а указатель в ThreadBump станет висячим).
    fn make_one(dir: BumpDir, cap: usize) -> (SharedArena, CachePadded<ThreadBump>) {
        let arena = SharedArena::new(cap);
        let mut v = arena.split_with(1, dir);
        (arena, v.pop().unwrap())
    }

    #[test]
    fn forward_fills_low_to_high() {
        let (_arena, bump) = make_one(BumpDir::Forward, 4096);
        let p0 = bump.alloc_raw::<1>(16) as usize;
        let p1 = bump.alloc_raw::<1>(16) as usize;
        assert!(p1 > p0, "forward должен расти вверх");
        assert!(p0 >= bump.ptr as usize);
        assert!(p1 + 16 <= bump.ptr as usize + bump.len);
        assert_eq!(bump.allocated_bytes(), 32);
    }

    #[test]
    fn backward_fills_high_to_low() {
        let (_arena, bump) = make_one(BumpDir::Backward, 4096);
        let p0 = bump.alloc_raw::<1>(16) as usize;
        let p1 = bump.alloc_raw::<1>(16) as usize;
        assert!(p1 < p0, "backward должен расти вниз");
        let end = bump.ptr as usize + bump.len;
        assert!(p0 + 16 <= end);
        assert!(p0 >= end - 32, "backward стартует с конца чанка");
        assert!(p1 >= end - 64);
        assert_eq!(bump.allocated_bytes(), 32);
    }

    #[test]
    fn middleout_alternates_sides() {
        let (_arena, bump) = make_one(BumpDir::MiddleOut, 4096);
        let base = bump.ptr as usize;
        let mid = bump.len / 2;
        assert_eq!(bump.lo.get(), 0, "MiddleOut стартует с нуля (lo)");
        assert_eq!(bump.hi.get(), 0, "MiddleOut стартует с нуля (hi)");

        // Сравниваем относительные смещения внутри чанка (адреса абсолютны).
        let rel = |p: usize| p - base;

        // 1-я аллокация — левая сторона (ниже середины), 2-я — правая (выше).
        let p0 = rel(bump.alloc_raw::<1>(16) as usize);
        let p1 = rel(bump.alloc_raw::<1>(16) as usize);
        assert!(p0 < mid, "1-я (левая) ниже середины");
        assert!(p1 >= mid, "2-я (правая) выше середины");
        assert!(p0 < p1, "стороны не должны пересекаться");
        assert_eq!(bump.allocated_bytes(), 32);

        // после ещё двух аллокаций регионы остаются непересекающимися
        let p2 = rel(bump.alloc_raw::<1>(16) as usize);
        let p3 = rel(bump.alloc_raw::<1>(16) as usize);
        assert!(p2 < p0, "3-я (левая) ещё ниже 1-й");
        assert!(p3 > p1, "4-я (правая) ещё выше 2-й");
        assert_eq!(bump.allocated_bytes(), 64);
    }

    #[test]
    fn forward_oom_panics() {
        let (_arena, bump) = make_one(BumpDir::Forward, 64);
        let _ = bump.alloc_raw::<1>(64); // ровно заполняет
        let res = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            bump.alloc_raw::<1>(1);
        }));
        assert!(res.is_err(), "ожидался OOM-panic");
    }

    #[test]
    fn backward_oom_panics() {
        let (_arena, bump) = make_one(BumpDir::Backward, 64);
        let _ = bump.alloc_raw::<1>(64);
        let res = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            bump.alloc_raw::<1>(1);
        }));
        assert!(res.is_err(), "ожидался OOM-panic");
    }

    #[test]
    fn middleout_oom_panics() {
        let (_arena, bump) = make_one(BumpDir::MiddleOut, 64);
        let res = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            bump.alloc_raw::<1>(64);
        }));
        assert!(
            res.is_err(),
            "ожидался OOM-panic (середина даёт 0 свободных)"
        );
    }

    #[test]
    fn reset_reuses_memory_forward() {
        let (_arena, bump) = make_one(BumpDir::Forward, 4096);
        let p0 = bump.alloc_raw::<1>(16);
        assert_eq!(bump.allocated_bytes(), 16);
        bump.reset();
        assert_eq!(bump.allocated_bytes(), 0);
        let p1 = bump.alloc_raw::<1>(16);
        assert_eq!(p0, p1, "после reset первая аллокация по тому же адресу");
    }

    #[test]
    fn reset_returns_middleout_to_middle() {
        let (_arena, bump) = make_one(BumpDir::MiddleOut, 4096);
        let _ = bump.alloc_raw::<1>(16);
        let _ = bump.alloc_raw::<1>(16);
        assert!(bump.allocated_bytes() > 0);
        bump.reset();
        assert_eq!(bump.lo.get(), 0);
        assert_eq!(bump.hi.get(), 0);
        assert_eq!(bump.allocated_bytes(), 0);
    }

    #[test]
    fn allocated_bytes_grows_monotonically() {
        let (_arena, bump) = make_one(BumpDir::Forward, 8192);
        let mut prev = 0;
        for _ in 0..10 {
            let _ = bump.alloc_raw::<8>(100);
            let now = bump.allocated_bytes();
            assert!(now > prev, "allocated_bytes должен расти");
            prev = now;
        }
    }

    #[test]
    fn prefault_does_not_crash_all_modes() {
        for dir in [BumpDir::Forward, BumpDir::Backward, BumpDir::MiddleOut] {
            let (_arena, bump) = make_one(dir, 1 << 16);
            let _ = bump.alloc_raw::<8>(1024);
            bump.prefault_local(); // на заполненном регионе
            bump.reset();
            bump.prefault_local(); // и на пустом
        }
    }

    /// Проверяем, что данные корректны вне зависимости от направления.
    fn data_integrity(dir: BumpDir) {
        let (_arena, bump) = make_one(dir, 1 << 20);
        let mut v: ArenaVec<ArenaString> =
            ArenaVec::with_capacity_in::<{ core::mem::align_of::<ArenaString>() }>(8, &bump);
        v.push::<{ core::mem::align_of::<ArenaString>() }>(
            ArenaString::from_str_in("hello", &bump),
            &bump,
        );
        v.push::<{ core::mem::align_of::<ArenaString>() }>(
            ArenaString::from_str_in("world", &bump),
            &bump,
        );
        v.push::<{ core::mem::align_of::<ArenaString>() }>(
            ArenaString::from_str_in("строка", &bump),
            &bump,
        );
        let slice = v.as_slice();
        assert_eq!(slice.len(), 3);
        assert_eq!(slice[0].deref(), "hello");
        assert_eq!(slice[1].deref(), "world");
        assert_eq!(slice[2].deref(), "строка");
    }

    #[test]
    fn data_integrity_forward() {
        data_integrity(BumpDir::Forward);
    }

    #[test]
    fn data_integrity_backward() {
        data_integrity(BumpDir::Backward);
    }

    #[test]
    fn data_integrity_middleout() {
        data_integrity(BumpDir::MiddleOut);
    }

    #[test]
    fn neighbors_alternate_directions() {
        let arena = SharedArena::new(4096 * 4);
        let bumps = arena.split_alternating(4);
        assert_eq!(bumps[0].dir, BumpDir::Backward);
        assert_eq!(bumps[1].dir, BumpDir::Forward);
        assert_eq!(bumps[2].dir, BumpDir::Backward);
        assert_eq!(bumps[3].dir, BumpDir::Forward);
        // чанки идут подряд и не пересекаются
        for i in 0..3 {
            let end = bumps[i].ptr as usize + bumps[i].len;
            assert_eq!(end, bumps[i + 1].ptr as usize);
        }
    }

    #[test]
    fn neighbors_meet_at_boundary() {
        let arena = SharedArena::new(8192);
        let bumps = arena.split_alternating(2);
        let (b0, b1) = (&bumps[0], &bumps[1]);
        let boundary = b0.ptr as usize + b0.len;
        assert_eq!(boundary, b1.ptr as usize);

        // b0 — Backward: первая аллокация упирается в правый край чанка (к границе).
        let p0 = b0.alloc_raw::<1>(64) as usize;
        assert_eq!(p0, boundary - 64, "Backward стартует у границы");

        // b1 — Forward: первая аллокация — само начало чанка (у той же границы).
        let p1 = b1.alloc_raw::<1>(64) as usize;
        assert_eq!(p1, boundary, "Forward стартует ровно у границы");
    }

    #[test]
    fn split_with_middleout_starts_at_middle() {
        let arena = SharedArena::new(4096);
        let bumps = arena.split_with(1, BumpDir::MiddleOut);
        let b = &bumps[0];
        assert_eq!(b.lo.get(), 0);
        assert_eq!(b.hi.get(), 0);
    }
}

use crossbeam_utils::CachePadded;
use std::cell::Cell;
use std::fmt;
use std::ops::Deref;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

// ============================================================
//               ПЛАТФОРМЕННЫЙ СЛОЙ ВЫДЕЛЕНИЯ ПАМЯТИ
// ============================================================

struct RawAllocation {
    ptr: *mut u8,
    size: usize,
    is_huge: bool,
}

/// Список дополнительных чанков, выделенных «где-то в памяти» в режиме доноров,
/// плюс курсор заполнения последнего чанка (чтобы не делать по VirtualAlloc на
/// каждый блок). Все чанки освобождаются в `Drop` `ThreadBump`.
struct FallbackChunks {
    chunks: Vec<RawAllocation>,
    /// Байт занято в последнем чанке (`chunks.last()`).
    used: usize,
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
                    lo: AtomicUsize::new(0),
                    hi: AtomicUsize::new(0),
                    toggle: Cell::new(false),
                    dir,
                    is_huge: self.alloc.is_huge,
                    neighbor_idx: None,
                    array: ptr::null(),
                    can_give: false,
                    self_index: 0,
                    donor_array: ptr::null(),
                    donor_count: 0,
                    base_chunk: 0,
                    fallback: SpinMutex::new(FallbackChunks {
                        chunks: Vec::new(),
                        used: 0,
                    }),
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
                    lo: AtomicUsize::new(0),
                    hi: AtomicUsize::new(0),
                    toggle: Cell::new(false),
                    dir,
                    is_huge: self.alloc.is_huge,
                    neighbor_idx: None,
                    array: ptr::null(),
                    can_give: false,
                    self_index: 0,
                    donor_array: ptr::null(),
                    donor_count: 0,
                    base_chunk: 0,
                    fallback: SpinMutex::new(FallbackChunks {
                        chunks: Vec::new(),
                        used: 0,
                    }),
                })
            })
            .collect()
    }

    /// Объединённый регион ПАРЫ: соседние потоки (2k — `Backward`, 2k+1 —
    /// `Forward`) делят ОДИН регион размером `2 * chunk_size` и заполняют его
    /// навстречу друг другу от внешних краёв к середине. Когда одна сторона
    /// доходит до середины и в своём чанке места больше нет, она «берёт»
    /// смежную свободную половину соседа (через lock-free CAS по счётчику
    /// соседа, см. `ThreadBump::try_borrow`). Если соседей нечётное число,
    /// последний поток получает собственный изолированный чанк без соседа.
    fn split_paired(&self, num_threads: usize) -> Vec<CachePadded<ThreadBump>> {
        let base = self.alloc.ptr;
        let total = self.alloc.size;
        let chunk_size = total / num_threads;
        let is_huge = self.alloc.is_huge;

        let bumps: Vec<CachePadded<ThreadBump>> = (0..num_threads)
            .map(|i| {
                if i % 2 == 0 {
                    // Чётный — ведущий чанк пары: общий регион [2k*cs, 2k*cs+2cs).
                    let pair_start = (i / 2) * 2 * chunk_size;
                    let combined_len = if i + 1 < num_threads {
                        2 * chunk_size
                    } else {
                        chunk_size
                    };
                    CachePadded::new(ThreadBump {
                        ptr: unsafe { base.add(pair_start) },
                        len: combined_len,
                        lo: AtomicUsize::new(0),
                        hi: AtomicUsize::new(0),
                        toggle: Cell::new(false),
                        dir: BumpDir::Backward,
                        is_huge,
                        neighbor_idx: None,
                        array: ptr::null(),
                        can_give: false,
                        self_index: 0,
                        donor_array: ptr::null(),
                        donor_count: 0,
                        base_chunk: 0,
                        fallback: SpinMutex::new(FallbackChunks {
                            chunks: Vec::new(),
                            used: 0,
                        }),
                    })
                } else {
                    // Нечётный — тот же самый объединённый регион пары, что и у
                    // чётного (i-1), заполняется слева направо.
                    let pair_start = ((i - 1) / 2) * 2 * chunk_size;
                    CachePadded::new(ThreadBump {
                        ptr: unsafe { base.add(pair_start) },
                        len: 2 * chunk_size,
                        lo: AtomicUsize::new(0),
                        hi: AtomicUsize::new(0),
                        toggle: Cell::new(false),
                        dir: BumpDir::Forward,
                        is_huge,
                        neighbor_idx: None,
                        array: ptr::null(),
                        can_give: false,
                        self_index: 0,
                        donor_array: ptr::null(),
                        donor_count: 0,
                        base_chunk: 0,
                        fallback: SpinMutex::new(FallbackChunks {
                            chunks: Vec::new(),
                            used: 0,
                        }),
                    })
                }
            })
            .collect();

        // Проставляем индекс соседа и указатель на массив внутри каждой пары.
        // Адрес буфера Vec стабилен (не переезжает при move/заимствовании),
        // поэтому raw-указатель остаётся валидным, пока живут потоки.
        let array = bumps.as_ptr();
        let mut k = 0;
        while k + 1 < num_threads {
            unsafe {
                (*(array.add(k) as *const ThreadBump as *mut ThreadBump)).neighbor_idx =
                    Some(k + 1);
                (*(array.add(k + 1) as *const ThreadBump as *mut ThreadBump)).neighbor_idx =
                    Some(k);
                (*(array.add(k) as *const ThreadBump as *mut ThreadBump)).array = array;
                (*(array.add(k + 1) as *const ThreadBump as *mut ThreadBump)).array = array;
            }
            k += 2;
        }

        bumps
    }

    /// Режим «доноры» (отдельная версия): каждый поток получает свой чанк
    /// (`Forward`), а некоторые чанки помечаются как способные отдавать память
    /// (`can_give`). Когда у потока в своём чанке кончается место, он проходит
    /// по списку доноров и берёт блок с «другой стороны» региона донора; если и
    /// у доноров нет свободного — выделяет новый чанк «где-то в памяти»
    /// (см. `ThreadBump::try_take_from_donors` / `grow_fallback`).
    ///
    /// `donor_every` — каждый `donor_every`-й чанк (начиная с 0) помечается
    /// донором.
    fn split_donors(&self, num_threads: usize, donor_every: usize) -> Vec<CachePadded<ThreadBump>> {
        let base = self.alloc.ptr;
        let total = self.alloc.size;
        let chunk_size = total / num_threads;
        let is_huge = self.alloc.is_huge;

        let bumps: Vec<CachePadded<ThreadBump>> = (0..num_threads)
            .map(|i| {
                let can_give = donor_every > 0 && i % donor_every == 0;
                CachePadded::new(ThreadBump {
                    ptr: unsafe { base.add(i * chunk_size) },
                    len: chunk_size,
                    lo: AtomicUsize::new(0),
                    hi: AtomicUsize::new(0),
                    toggle: Cell::new(false),
                    dir: BumpDir::Forward,
                    is_huge,
                    neighbor_idx: None,
                    array: ptr::null(),
                    can_give,
                    self_index: i,
                    donor_array: ptr::null(), // заполним ниже
                    donor_count: 0,
                    base_chunk: chunk_size,
                    fallback: SpinMutex::new(FallbackChunks {
                        chunks: Vec::new(),
                        used: 0,
                    }),
                })
            })
            .collect();

        // Проставляем указатель на массив и число элементов — буфер Vec
        // стабилен, поэтому raw-указатель валиден, пока живут потоки.
        let array = bumps.as_ptr();
        for i in 0..num_threads {
            unsafe {
                (*(array.add(i) as *const ThreadBump as *mut ThreadBump)).donor_array = array;
                (*(array.add(i) as *const ThreadBump as *mut ThreadBump)).donor_count = num_threads;
            }
        }

        bumps
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
    ///
    /// Для режима пары (`Neighbors`/`Pair`) эти счётчики читаются и
    /// модифицируются соседним потоком через `neighbor` (CAS, relaxed),
    /// поэтому они atomic, а не `Cell`.
    lo: AtomicUsize,
    /// Байт, выделенный с правого края.
    hi: AtomicUsize,
    /// Для MiddleOut: чётность следующего выделения (false -> влево, true -> вправо).
    toggle: Cell<bool>,
    dir: BumpDir,
    is_huge: bool,
    /// В режиме объединённого региона пары: индекс соседнего `ThreadBump` той
    /// же пары (в массиве `array`). Используется для «займа» памяти у соседа
    /// при переполнении своей стороны. `None` — поток работает в собственном
    /// изолированном чанке.
    neighbor_idx: Option<usize>,
    /// Указатель на первый элемент массива bumps (стабилен: массив живёт всё
    /// время работы потоков и никогда не переезжает). Нужен, чтобы по
    /// `neighbor_idx` добраться до счётчиков соседа. `null` для не-пар.
    array: *const CachePadded<ThreadBump>,
    // ===== Поля режима «доноры» (отдельная версия) =====
    /// Помечен ли этот bump как способный отдавать память («донор»).
    can_give: bool,
    /// Собственный индекс в массиве bumps (чтобы не брать у самого себя).
    self_index: usize,
    /// Указатель на массив bumps для обхода доноров (`null` вне режима доноров).
    donor_array: *const CachePadded<ThreadBump>,
    /// Число элементов в массиве bumps.
    donor_count: usize,
    /// Размер первичного чанка (для размера fallback-чанков).
    base_chunk: usize,
    /// Дополнительные чанки, выделенные «где-то в памяти» при нехватке у
    /// доноров. Освобождаются в `Drop`.
    fallback: SpinMutex<FallbackChunks>,
    // (������ ���������� ���������� fallback-����� �������� ������ FallbackChunks)
}

// Каждый ThreadBump владеет непересекающимся регионом памяти арены и
// используется ровно одним потоком, поэтому сырой указатель безопасно
// объявить Send (как и для любого per-thread arena-аллокатора). Кроме того,
// при раздаче пар (`split_paired`) несколько потоков держат `&ThreadBump`
// одного и того же (никогда не переезжающего) массива и обращаются друг к
// другу только через atomic-счётчики `lo`/`hi` и неизменяемое `dir`, поэтому
// структура корректна и как `Sync`.
unsafe impl Send for ThreadBump {}
unsafe impl Sync for ThreadBump {}

impl Drop for ThreadBump {
    fn drop(&mut self) {
        // Первичный регион арены освобождает `SharedArena`; здесь — только
        // дополнительные fallback-чанки, выделенные «где-то в памяти».
        let mut fb = self.fallback.lock();
        for a in fb.chunks.drain(..) {
            platform::free(a);
        }
    }
}

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
            // Forward: растём от начала чанка вверх. В режиме пары собственная
            // половина — нижняя [0, mid); середина региона `mid = len/2`.
            //
            // Обновление `lo` делаем через CAS, потому что тот же счётчик
            // параллельно может расширять сосед (заём памяти), см. `try_borrow`.
            BumpDir::Forward => {
                let mid = if self.neighbor_idx.is_some() {
                    self.len / 2
                } else {
                    self.len
                };
                loop {
                    let cur = self.lo.load(Ordering::Relaxed);
                    let off = (cur + ALIGN - 1) & !(ALIGN - 1);
                    let end = off + size;
                    if end <= mid {
                        if self
                            .lo
                            .compare_exchange_weak(cur, end, Ordering::Relaxed, Ordering::Relaxed)
                            .is_ok()
                        {
                            return unsafe { self.ptr.add(off) };
                        }
                    } else if let Some(p) = self.try_borrow::<ALIGN>(size) {
                        return p;
                    } else if self.donor_array != ptr::null() {
                        // Режим доноров: сначала пробуем взять чанк у помеченных
                        // доноров (с другой стороны их региона), иначе — новый
                        // чанк «где-то в памяти».
                        if let Some(p) = self.try_take_from_donors::<ALIGN>(size) {
                            return p;
                        }
                        return self.grow_fallback::<ALIGN>(size);
                    } else {
                        panic!(
                            "Arena OOM: need {} at offset {}, free {} (cap {})",
                            size,
                            off,
                            mid - self.lo.load(Ordering::Relaxed),
                            self.len
                        );
                    }
                }
            }
            // Backward: растём от конца чанка вниз. В режиме пары собственная
            // половина — верхняя [mid, len). Обновление `hi` — через CAS
            // (тот же счётчик параллельно может расширять сосед-заёмщик).
            BumpDir::Backward => {
                let mid = if self.neighbor_idx.is_some() {
                    self.len / 2
                } else {
                    0
                };
                loop {
                    let cur = self.hi.load(Ordering::Relaxed);
                    let off = (self.len - cur - size) & !(ALIGN - 1);
                    if off >= mid {
                        let new_hi = self.len - off;
                        if self
                            .hi
                            .compare_exchange_weak(
                                cur,
                                new_hi,
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            )
                            .is_ok()
                        {
                            return unsafe { self.ptr.add(off) };
                        }
                    } else if let Some(p) = self.try_borrow::<ALIGN>(size) {
                        return p;
                    } else if self.donor_array != ptr::null() {
                        if let Some(p) = self.try_take_from_donors::<ALIGN>(size) {
                            return p;
                        }
                        return self.grow_fallback::<ALIGN>(size);
                    } else {
                        panic!(
                            "Arena OOM: need {} bytes, only {} free (cap {})",
                            size,
                            self.len - cur - mid,
                            self.len
                        );
                    }
                }
            }
            // MiddleOut: из середины наружу, чередуя стороны.
            BumpDir::MiddleOut => {
                let mid = self.len / 2;
                let side = self.toggle.get();
                self.toggle.set(!side);
                if !side {
                    // левая сторона: занята [mid - lo, mid), растёт вниз
                    let base = mid - self.lo.load(Ordering::Relaxed);
                    if size > base {
                        panic!(
                            "Arena OOM: need {} at offset {}, free {} (cap {})",
                            size,
                            mid - self.lo.load(Ordering::Relaxed),
                            base,
                            self.len
                        );
                    }
                    let off = (base - size) & !(ALIGN - 1);
                    (off, mid - off, self.hi.load(Ordering::Relaxed))
                } else {
                    // правая сторона: занята [mid, mid + hi), растёт вверх
                    let base = mid + self.hi.load(Ordering::Relaxed);
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
                    (off, self.lo.load(Ordering::Relaxed), off + size - mid)
                }
            }
        };

        self.lo.store(new_lo, Ordering::Relaxed);
        self.hi.store(new_hi, Ordering::Relaxed);
        unsafe { self.ptr.add(off) }
    }

    /// Попытка «занять» память у соседа объединённого региона, когда в своей
    /// половине места больше нет. Возвращает `Some(ptr)` при успехе или `None`,
    /// если и у соседа недостаточно свободной смежной половины.
    ///
    /// Каждый из пары владеет своей половиной региона (`mid = len/2`):
    /// Forward-сосед — нижней [0, mid), Backward-сосед — верхней [mid, len).
    /// Когда свою половину мы заполнили и дошли до середины, мы продолжаем
    /// в смежную свободную половину соседа (ту, что примыкает к середине),
    /// расширяя его счётчик (`lo` или `hi`) через lock-free CAS. Поскольку
    /// сосед тоже может одновременно расширять свой счётчик, CAS гарантирует,
    /// что два потока не займут один и тот же кусок.
    ///
    /// TODO (пока заглушка): дальше — поиск по БОЛЕЕ ДАЛЬНИМ соседям, а если и у
    /// них мало осталось — аллокация нового чанка. Сейчас при неудаче у соседа
    /// возвращаем `None` (вызывающий паникует с OOM).
    #[inline(always)]
    fn try_borrow<const ALIGN: usize>(&self, size: usize) -> Option<*mut u8> {
        let idx = self.neighbor_idx?;
        let nb_ref = unsafe { &*self.array.add(idx) };
        let mid = self.len / 2;
        if nb_ref.dir == BumpDir::Forward {
            // Сосед владеет нижней половиной [0, mid); берём у него сверху —
            // из его свободной части [lo_odd, mid), смежной с нашей серединой.
            loop {
                let cur = nb_ref.lo.load(Ordering::Relaxed);
                let off = (cur + ALIGN - 1) & !(ALIGN - 1);
                let end = off + size;
                if end > mid {
                    return None;
                }
                if nb_ref
                    .lo
                    .compare_exchange_weak(cur, end, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    return Some(unsafe { self.ptr.add(off) });
                }
            }
        } else {
            // Сосед владеет верхней половиной [mid, len); берём у него снизу —
            // из его свободной части [mid, len - hi_even), смежной с серединой.
            loop {
                let cur = nb_ref.hi.load(Ordering::Relaxed);
                let off = (self.len - cur - size) & !(ALIGN - 1);
                if off < mid {
                    return None;
                }
                let new_hi = self.len - off;
                if nb_ref
                    .hi
                    .compare_exchange_weak(cur, new_hi, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    return Some(unsafe { self.ptr.add(off) });
                }
            }
        }
    }

    // ===== Режим «доноры»: заём чанка у помеченных доноров, иначе новый чанк =====

    /// Обойти все помеченные доноры и попытаться взять у одного из них блок
    /// `size` с «другой стороны» его региона (противоположной его заполнению).
    /// Возвращает `Some(ptr)` при успехе или `None`, если ни у одного донора
    /// нет свободной смежной половины.
    #[inline(always)]
    fn try_take_from_donors<const ALIGN: usize>(&self, size: usize) -> Option<*mut u8> {
        if self.donor_array == ptr::null() {
            return None;
        }
        let arr = self.donor_array;
        for i in 0..self.donor_count {
            if i == self.self_index {
                continue;
            }
            let d = unsafe { &*arr.add(i) };
            if !d.can_give {
                continue;
            }
            if let Some(p) = self.take_from_donor::<ALIGN>(d, size) {
                return Some(p);
            }
        }
        None
    }

    /// Взять блок `size` из региона донора `d` с противоположной его заполнению
    /// стороны, расширяя соответствующий счётчик донора через lock-free CAS.
    #[inline(always)]
    fn take_from_donor<const ALIGN: usize>(&self, d: &ThreadBump, size: usize) -> Option<*mut u8> {
        if d.dir == BumpDir::Forward {
            // Донор заполняет низ [0, lo); отдаём с высокой стороны [len - hi, len).
            loop {
                let cur = d.hi.load(Ordering::Relaxed);
                let off = (d.len - cur - size) & !(ALIGN - 1);
                if off < d.lo.load(Ordering::Relaxed) {
                    return None; // упёрлись бы в собственные данные донора
                }
                let new_hi = d.len - off;
                if d.hi
                    .compare_exchange_weak(cur, new_hi, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    return Some(unsafe { d.ptr.add(off) });
                }
            }
        } else {
            // Донор заполняет высокую [len - hi, len); отдаём с низкой [0, lo).
            loop {
                let cur = d.lo.load(Ordering::Relaxed);
                let off = (cur + ALIGN - 1) & !(ALIGN - 1);
                let end = off + size;
                if end > d.len - d.hi.load(Ordering::Relaxed) {
                    return None;
                }
                if d.lo
                    .compare_exchange_weak(cur, end, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    return Some(unsafe { d.ptr.add(off) });
                }
            }
        }
    }

    /// Выделить совершенно новый чанк «где-то в памяти» (через платформенный
    /// аллокатор) и вернуть в нём указатель на блок `size`. Чанк сохраняется в
    /// `fallback` и освобождается в `Drop`. Адрес возвращается вызывающему — его
    /// собственный регион при этом не трогается.
    #[inline(always)]
    fn grow_fallback<const ALIGN: usize>(&self, size: usize) -> *mut u8 {
        let page = platform::page_size();
        // Размер чанка — хотя бы базовый чанк, выровнен по странице; блок `size`
        // выровнен внутри.
        let need = (size + ALIGN - 1) & !(ALIGN - 1);
        let mut fb = self.fallback.lock();
        // Если в текущем последнем чанке не хватает места — выделяем новый.
        let full = match fb.chunks.last() {
            Some(c) => fb.used + need > c.size,
            None => true,
        };
        if full {
            let alloc_size = need.max(self.base_chunk).next_multiple_of(page);
            let alloc = platform::alloc_normal(alloc_size);
            fb.chunks.push(alloc);
            fb.used = 0;
        }
        let chunk = fb.chunks.last().unwrap();
        let off = (fb.used + ALIGN - 1) & !(ALIGN - 1);
        let ptr = unsafe { chunk.ptr.add(off) };
        fb.used = off + size;
        ptr
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
        self.lo.store(0, Ordering::Relaxed);
        self.hi.store(0, Ordering::Relaxed);
        self.toggle.set(false);
    }

    /// First-touch: фолтим страницы ИСПОЛЬЗОВАННЫХ областей из текущего
    /// потока, чтобы они легли в локальную NUMA-ноду.
    fn prefault_local(&self) {
        if self.is_huge && platform::large_pages_precommitted() {
            return;
        }
        let (lo, hi) = (
            self.lo.load(Ordering::Relaxed),
            self.hi.load(Ordering::Relaxed),
        );
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
        self.lo.load(Ordering::Relaxed) + self.hi.load(Ordering::Relaxed)
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
    /// Чётные+нечётные соседи делят ОДИН объединённый регион и при нехватке
    /// места берут память друг у друга (см. `SharedArena::split_paired`).
    Pair,
    /// Некоторые чанки помечены как доноры: при нехватке места у соседей берём
    /// блок с другой стороны их региона, иначе — новый чанк в памяти
    /// (см. `SharedArena::split_donors`).
    Donors,
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
        ArenaLayout::Pair => arena.split_paired(core_ids.len()),
        ArenaLayout::Donors => arena.split_donors(core_ids.len(), 4),
    };

    let (vcap, outer, inner) = if full {
        (40000, 200, 200)
    } else {
        (10000, 100, 100)
    };

    std::thread::scope(|s| {
        for (core_id, i) in core_ids.iter().zip(0..bumps.len()) {
            // `&bumps[i]` — разделяемая ссылка в никогда не переезжающий массив.
            // `CachePadded<ThreadBump>: Sync` (см. `unsafe impl Sync`), поэтому
            // сама ссылка `Send` и её можно отдать потоку. Каждый чанк
            // использует ровно один поток; заём у соседа идёт только через
            // atomic-счётчики (см. `try_borrow`).
            let core = *core_id;
            let bump = &bumps[i];
            s.spawn(move || {
                core_affinity::set_for_current(core);
                hotpath::measure_block!("prefault", {
                    bump.prefault_local(); // first-touch в локальной NUMA-ноде
                });
                for _ in 0..3 {
                    hotpath::measure_block!("alloc", {
                        let mut vectr: ArenaVec<ArenaVec<ArenaString>> =
                            ArenaVec::with_capacity_in::<
                                { core::mem::align_of::<ArenaVec<ArenaString>>() },
                            >(vcap, bump);
                        for _ in 0..outer {
                            for _ in 0..inner {
                                let mut vec: ArenaVec<ArenaString> =
                                    ArenaVec::with_capacity_in::<
                                        { core::mem::align_of::<ArenaString>() },
                                    >(400, bump);
                                for _ in 0..100 {
                                    vec.push::<{ core::mem::align_of::<ArenaString>() }>(
                                        ArenaString::from_str_in("stroka", bump),
                                        bump,
                                    );
                                }
                                vectr.push::<{ core::mem::align_of::<ArenaVec<ArenaString>>() }>(
                                    vec, bump,
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

/// Полная версия: соседи делят ОДИН регион и берут память друг у друга.
pub fn arena_full_pair(chunk_size: usize, smt: bool) {
    arena_bench(chunk_size, smt, true, ArenaLayout::Pair);
}

/// Лёгкая версия: соседи делят ОДИН регион и берут память друг у друга.
pub fn arena_light_pair(chunk_size: usize, smt: bool) {
    arena_bench(chunk_size, smt, false, ArenaLayout::Pair);
}

/// Полная версия: доноры отдают память с другой стороны, иначе новый чанк.
pub fn arena_full_donors(chunk_size: usize, smt: bool) {
    arena_bench(chunk_size, smt, true, ArenaLayout::Donors);
}

/// Лёгкая версия: доноры отдают память с другой стороны, иначе новый чанк.
pub fn arena_light_donors(chunk_size: usize, smt: bool) {
    arena_bench(chunk_size, smt, false, ArenaLayout::Donors);
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
        ("Pair", |c, s| arena_full_pair(c, s)),
        ("Donors", |c, s| arena_full_donors(c, s)),
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
        ("Pair", |c, s| arena_light_pair(c, s)),
        ("Donors", |c, s| arena_light_donors(c, s)),
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
        assert_eq!(
            bump.lo.load(Ordering::Relaxed),
            0,
            "MiddleOut стартует с нуля (lo)"
        );
        assert_eq!(
            bump.hi.load(Ordering::Relaxed),
            0,
            "MiddleOut стартует с нуля (hi)"
        );

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
        assert_eq!(bump.lo.load(Ordering::Relaxed), 0);
        assert_eq!(bump.hi.load(Ordering::Relaxed), 0);
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
        assert_eq!(b.lo.load(Ordering::Relaxed), 0);
        assert_eq!(b.hi.load(Ordering::Relaxed), 0);
    }

    // ---- Пара: объединённый регион и заём памяти у соседа ----

    #[test]
    fn pair_shares_combined_region() {
        let arena = SharedArena::new(8192);
        let bumps = arena.split_paired(2);
        assert_eq!(bumps[0].dir, BumpDir::Backward);
        assert_eq!(bumps[1].dir, BumpDir::Forward);
        // Оба указывают на ОДИН объединённый регион в 2*chunk_size.
        assert_eq!(bumps[0].ptr, bumps[1].ptr, "пара делит один регион");
        assert_eq!(bumps[0].len, 8192);
        assert_eq!(bumps[1].len, 8192);
        assert!(bumps[0].neighbor_idx.is_some());
        assert!(bumps[1].neighbor_idx.is_some());
    }

    #[test]
    fn pair_fills_toward_middle_without_overlap() {
        let arena = SharedArena::new(8192);
        let bumps = arena.split_paired(2);
        let (even, odd) = (&bumps[0], &bumps[1]); // Backward / Forward
        let base = arena.alloc.ptr as usize;

        // odd растёт в своей нижней половине [0, 4096), even — в верхней [4096, 8192).
        let o0 = odd.alloc_raw::<1>(1000) as usize;
        let o1 = odd.alloc_raw::<1>(1000) as usize;
        assert_eq!(o0, base, "Forward стартует с начала региона");
        assert_eq!(o1, base + 1000, "Forward растёт вверх");

        let e0 = even.alloc_raw::<1>(1000) as usize;
        let e1 = even.alloc_raw::<1>(1000) as usize;
        assert_eq!(e0, base + 8192 - 1000, "Backward стартует с конца региона");
        assert_eq!(e1, base + 8192 - 2000, "Backward растёт вниз");

        // Собственные половины не пересекаются (середина — граница).
        assert!(e1 + 1000 <= base + 8192);
        assert!(
            o1 + 1000 <= base + 4096,
            "odd не выходит за середину своей половины"
        );
        assert!(
            e0 >= base + 4096,
            "even не выходит за середину своей половины"
        );
    }

    #[test]
    fn pair_odd_can_use_evens_half_when_even_idle() {
        // even почти ничего не берёт, odd забирает свою половину и
        // продолжает в смежную свободную половину even (заём памяти соседа).
        let arena = SharedArena::new(8192);
        let bumps = arena.split_paired(2);
        let (even, odd) = (&bumps[0], &bumps[1]);
        let base = arena.alloc.ptr as usize;

        let _ = even.alloc_raw::<1>(16); // even занял кроху сверху: [8176, 8192)
        let _ = odd.alloc_raw::<1>(4096); // odd заполнил свою половину [0, 4096)
        // Теперь odd занимает у even свободную половину [4096, 8176).
        let borrowed = odd.alloc_raw::<1>(4000) as usize;
        assert_eq!(
            borrowed,
            base + 4176,
            "заём — в половине соседа, смежной к середине"
        );
        assert!(borrowed >= base + 4096, "в половине even");
        assert!(
            borrowed + 4000 <= base + 8192 - 16,
            "не задевает 16 байт even"
        );
    }

    #[test]
    fn pair_borrow_extends_neighbor_counter() {
        // odd занимает низ своей половины, even доходит до середины, затем even
        // заимствует смежную свободную половину odd через счётчик соседа.
        let arena = SharedArena::new(8192);
        let bumps = arena.split_paired(2);
        let (even, odd) = (&bumps[0], &bumps[1]);
        let base = arena.alloc.ptr as usize;

        let _ = odd.alloc_raw::<1>(1000); // odd: [0, 1000)
        let _ = even.alloc_raw::<1>(4096); // even заполнил свою половину [4096, 8192)
        // even упирается в середину и берёт 100 байт из свободной части odd [1000, 4096).
        let borrowed = even.alloc_raw::<1>(100) as usize;
        assert_eq!(
            borrowed,
            base + 1000,
            "заём — в смежной свободной половине соседа"
        );
        assert_eq!(
            odd.lo.load(Ordering::Relaxed),
            1100,
            "счётчик соседа расширен займом"
        );
        // не пересекается с данными odd [0,1000) и even [4096,8192)
        assert!(borrowed >= base + 1000);
        assert!(borrowed + 100 <= base + 4096);
    }

    #[test]
    fn pair_concurrent_borrow_no_overlap() {
        // Два потока одновременно: чётный (even) забирает больше своей половины
        // и заимствует у нечётного (odd) через lock-free CAS, нечётный — в своей
        // половине. Проверяем, что ни один не затёр данные другого.
        let arena = SharedArena::new(2 * 1024 * 1024); // 2 MB: половина = 1 MB
        let bumps = arena.split_paired(2);

        std::thread::scope(|s| {
            // even: 200_000 * 8 = 1.6 MB > своей половины (1 MB) -> заимствует 0.6 MB.
            s.spawn(|| {
                let bump = &bumps[0];
                let mut ptrs: Vec<*mut usize> = Vec::with_capacity(200_000);
                for j in 0..200_000usize {
                    let p = bump.alloc_raw::<8>(8) as *mut usize;
                    unsafe { *p = 0xA000 + j };
                    ptrs.push(p);
                }
                for (j, p) in ptrs.iter().enumerate() {
                    assert_eq!(unsafe { **p }, 0xA000 + j, "even: данные затёрты соседом");
                }
            });
            // odd: 50_000 * 8 = 0.4 MB < своей половины (1 MB), свою не покидает.
            s.spawn(|| {
                let bump = &bumps[1];
                let mut ptrs: Vec<*mut usize> = Vec::with_capacity(50_000);
                for j in 0..50_000usize {
                    let p = bump.alloc_raw::<8>(8) as *mut usize;
                    unsafe { *p = 0xB000 + j };
                    ptrs.push(p);
                }
                for (j, p) in ptrs.iter().enumerate() {
                    assert_eq!(unsafe { **p }, 0xB000 + j, "odd: данные затёрты соседом");
                }
            });
        });
    }

    // ---- Доноры: заём с «другой стороны» и fallback-чанк ----

    #[test]
    fn donors_take_from_donor_other_side() {
        // 2 потока, индекс 0 — донор (0 % 2 == 0). Донор остаётся пустым, а
        // поток 1 переполняет свой чанк и должен забрать блок с высокой стороны
        // региона донора (противоположной заполнению Forward-донора низом).
        let arena = SharedArena::new(2 * 1024 * 1024); // 1 MB на потока
        let bumps = arena.split_donors(2, 2);
        let (donor, needy) = (&bumps[0], &bumps[1]);
        assert!(donor.can_give);
        assert!(!needy.can_give);

        // Поток 1: 200_000 * 8 = 1.6 MB > своего чанка (1 MB) -> берёт у донора.
        let mut ptrs: Vec<*mut usize> = Vec::with_capacity(200_000);
        for j in 0..200_000usize {
            let p = needy.alloc_raw::<8>(8) as *mut usize;
            unsafe { *p = 0xC000 + j };
            ptrs.push(p);
        }
        // Последние блоки должны лежать в регионе донора (отданы с высокой стороны).
        let d_start = donor.ptr as usize;
        let d_end = d_start + donor.len;
        let last = *ptrs.last().unwrap() as usize;
        assert!(last >= d_start && last < d_end, "блок взят не у донора");
        // Счётчик донора расширился с высокой стороны.
        assert!(donor.hi.load(Ordering::Relaxed) > 0);
        // Данные не затёрты.
        for (j, p) in ptrs.iter().enumerate() {
            assert_eq!(unsafe { **p }, 0xC000 + j, "данные затёрты донором/соседом");
        }
        // У донора своя память свободна (lo не трогали).
        assert_eq!(donor.lo.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn donors_fallback_when_no_donor_free() {
        // Ни один донор не помечен (donor_every = 1000): при нехватке места
        // выделяется новый чанк «где-то в памяти» (grow_fallback). Проверяем, что
        // он находится ВНЕ основной арены и данные живы; Drop освобождает его.
        let arena = SharedArena::new(1 << 20);
        let bumps = arena.split_donors(4, 0);
        let needy = &bumps[1];
        assert!(!bumps.iter().any(|b| b.can_give));

        let arena_start = arena.alloc.ptr as usize;
        let arena_end = arena_start + arena.alloc.size;

        let mut ptrs: Vec<*mut usize> = Vec::with_capacity(300_000);
        for j in 0..300_000usize {
            let p = needy.alloc_raw::<8>(8) as *mut usize;
            unsafe { *p = 0xD000 + j };
            ptrs.push(p);
        }
        // Хотя бы часть блоков должна уйти в fallback (вне арены).
        let outside = ptrs
            .iter()
            .any(|&p| (p as usize) < arena_start || (p as usize) >= arena_end);
        assert!(outside, "ожидался fallback-чанк вне арены");
        for (j, p) in ptrs.iter().enumerate() {
            assert_eq!(unsafe { **p }, 0xD000 + j);
        }
        // fallback непуст (Drop позже освободит).
        assert!(!needy.fallback.lock().chunks.is_empty());
    }
}

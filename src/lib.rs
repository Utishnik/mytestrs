#![cfg_attr(coverage, feature(coverage_attribute))]
use boxcar::Vec as BoxcarVec;
#[cfg(not(miri))]
use branches::prefetch_write_data;
use crossbeam_utils::CachePadded;
use orx_concurrent_vec::ConcurrentVec as OrxVec;
use std::cell::Cell;
use std::fmt;
use std::marker::PhantomData;
use std::ops::Deref;
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use triomphe::Arc;

/// Подсказка префетчеру: блок по `addr` будет сразу записан вызывающим.
/// LOCALITY=0 — греем L1, т.к. пишем в выделенный блок почти мгновенно.
/// Под Miri префетч отключён (там не нужен и может быть не поддержан).
#[inline(always)]
#[cfg(not(miri))]
fn prefetch_write(addr: *const u8) {
    prefetch_write_data::<u8, 0>(addr);
}
#[inline(always)]
#[cfg(miri)]
fn prefetch_write(_addr: *const u8) {}

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

/// Запись о доноре в реестре: индекс в массиве bumps, его приоритет и флаг
/// логического удаления (для динамических реестров orx/boxcar, где физическое
/// удаление из lock-free вектора невозможно без `&mut`, — удаление помечает
/// запись, а обход её пропускает).
/// Более ВЫСОКИЙ приоритет означает, что этого донора берут в ПОСЛЕДНЮЮ
/// очередь (сначала опустошают доноров с низким приоритетом).
struct Donor {
    idx: usize,
    priority: u32,
    removed: AtomicBool,
}

impl Clone for Donor {
    fn clone(&self) -> Self {
        Donor {
            idx: self.idx,
            priority: self.priority,
            removed: AtomicBool::new(self.removed.load(Ordering::Relaxed)),
        }
    }
}

/// Тип хранилища списка доноров. Сами доноры ссылаются на регионы через
/// `donor_array` (общий стабильный массив bumps), здесь — только индексы.
///
/// Статичный список хранится как плоский срез в `bumpalo::Bump` (см.
/// `donor_static_ptr`/`donor_static_len`/`donor_bump` в `ThreadBump`) — без
/// глобального аллокатора и без `Arc`-накладных. Динамические списки (orx/
/// boxcar) лежат в обёртке `triomphe::Arc` — более лёгкой, чем `std::sync::Arc`
/// (без счётчика weak-ссылок).
#[derive(Clone)]
enum DonorReg {
    /// Режим доноров не активен (переполнение ведёт к OOM-панике).
    None,
    /// Статичный список: заполняется один раз при старте и не меняется
    /// (данные — в bump-буфере, на который указывает `donor_static_ptr`).
    /// При `use_priority` — отсортирован по возрастанию приоритета, поэтому
    /// первый подходящий донор автоматически имеет минимальный приоритет.
    Static,
    /// Динамический список на orx-concurrent-vec: доноры могут добавляться и
    /// удаляться в рантайме (lock-free push / логическое удаление).
    Orx(Arc<OrxVec<Donor>>),
    /// Динамический список на boxcar::Vec — альтернативная реализация того же
    /// контракта (добавление/удаление в рантайме).
    Boxcar(Arc<BoxcarVec<Donor>>),
}

/// Какое хранилище списка доноров использовать.
#[derive(Clone, Copy)]
pub enum DonorListKind {
    /// Статичный `Vec`, задаётся один раз.
    Static,
    /// `orx-concurrent-vec`.
    Orx,
    /// `boxcar::Vec`.
    Boxcar,
}

/// Политика раздачи доноров: какой вид списка, использовать ли приоритет и
/// как помечать доноров (`every`-й поток, начиная с 0, становится донором).
pub struct DonorPolicy {
    pub kind: DonorListKind,
    pub use_priority: bool,
    pub every: usize,
    /// Явные приоритеты по индексу потока (если `use_priority`). При `None`
    /// приоритет потока равен его индексу.
    pub priorities: Option<Vec<u32>>,
}

impl DonorPolicy {
    /// Статичный список, без приоритета.
    pub fn static_(every: usize) -> Self {
        Self {
            kind: DonorListKind::Static,
            use_priority: false,
            every,
            priorities: None,
        }
    }
    /// Динамический список на orx-concurrent-vec.
    pub fn orx(every: usize) -> Self {
        Self {
            kind: DonorListKind::Orx,
            use_priority: false,
            every,
            priorities: None,
        }
    }
    /// Динамический список на boxcar.
    pub fn boxcar(every: usize) -> Self {
        Self {
            kind: DonorListKind::Boxcar,
            use_priority: false,
            every,
            priorities: None,
        }
    }
    /// То же, что и базовая политика, но с включённым приоритетом.
    pub fn with_priority(mut self) -> Self {
        self.use_priority = true;
        self
    }
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
    #[cfg_attr(coverage, coverage(off))]
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
                        "Р Р…Р ВµР Т‘Р С•РЎРѓРЎвЂљРЎС“Р С—Р Р…Р В° (large pages Р Р†РЎвЂ№Р С”Р В»РЎР‹РЎвЂЎР ВµР Р…РЎвЂ№)"
                    }
                );
            }
        }
    }

    /// Идемпотентно запрашивает `SeLockMemoryPrivilege` («Lock pages in memory»)
    /// ровно один раз за процесс. Благодаря этому `MEM_LARGE_PAGES` (и, как
    /// следствие, эффективный префетч/быстрая арена, как на Linux) начинает
    /// работать на Windows. Вызывается автоматически из `alloc_normal` и
    /// `try_alloc_huge`, поэтому право запрашивается само при первом выделении.
    fn ensure_lock_memory_privilege() {
        static PRIV: OnceLock<()> = OnceLock::new();
        PRIV.get_or_init(enable_lock_memory_privilege);
    }

    #[cfg_attr(coverage, coverage(off))]
    pub fn try_alloc_huge(size: usize) -> Option<RawAllocation> {
        // Под Miri нет шимов для Windows-привилегий (AdjustTokenPrivileges и
        // др.), поэтому huge-страницы просто не пытаемся — упадём на обычный
        // VirtualAlloc через alloc_normal.
        if cfg!(miri) {
            return None;
        }
        ensure_lock_memory_privilege();

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

    #[cfg(not(miri))]
    pub fn alloc_normal(size: usize) -> RawAllocation {
        ensure_lock_memory_privilege();
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

    // Под Miri VirtualAlloc не зашимлен — используем стандартный аллокатор,
    // чтобы проверить логику (в первую очередь реестр доноров) через Miri.
    // База должна быть выровнена не хуже страницы (как VirtualAlloc), иначе
    // указатели вида `ptr + off` окажутся невыровненными (UB под Tree Borrows).
    #[cfg(miri)]
    pub fn alloc_normal(size: usize) -> RawAllocation {
        use std::alloc::{Layout, alloc};
        let layout = Layout::from_size_align(size, 16).unwrap();
        let ptr = unsafe { alloc(layout) };
        assert!(!ptr.is_null(), "std alloc failed (miri)");
        RawAllocation {
            ptr,
            size,
            is_huge: false,
        }
    }

    #[allow(dead_code)]
    pub fn lock_memory(ptr: *mut u8, size: usize) -> bool {
        unsafe { VirtualLock(ptr as *const _, size) != 0 }
    }

    #[cfg(not(miri))]
    pub fn free(alloc: RawAllocation) {
        unsafe {
            VirtualFree(alloc.ptr as *mut _, 0, MEM_RELEASE);
        }
    }

    #[cfg(miri)]
    pub fn free(alloc: RawAllocation) {
        use std::alloc::{Layout, dealloc};
        let layout = Layout::from_size_align(alloc.size, 16).unwrap();
        unsafe { dealloc(alloc.ptr, layout) };
    }

    /// Размер обычной страницы (обычно 4 KB).
    #[cfg(not(miri))]
    pub fn page_size() -> usize {
        static PAGE: OnceLock<usize> = OnceLock::new();
        *PAGE.get_or_init(|| unsafe {
            let mut info: SYSTEM_INFO = core::mem::zeroed();
            GetSystemInfo(&mut info);
            info.dwPageSize as usize
        })
    }

    #[cfg(miri)]
    pub fn page_size() -> usize {
        4096
    }

    /// Минимальный размер large page (обычно 2 MB).
    #[cfg(not(miri))]
    pub fn huge_page_size() -> usize {
        static HUGE: OnceLock<usize> = OnceLock::new();
        *HUGE.get_or_init(|| unsafe { GetLargePageMinimum() })
    }

    #[cfg(miri)]
    pub fn huge_page_size() -> usize {
        2 * 1024 * 1024
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

    /// Асинхронный prefetch страниц через PrefetchVirtualMemory. ВЫКЛЮЧЕН по
    /// умолчанию и включается ТОЛЬКО под фичей `win-prefetch-pages`
    /// (`cargo build --features win-prefetch-pages`): на практике worker-потоки
    /// ядра фолтят страницы в чужом контексте (working-set lock, чужая NUMA-нода)
    /// и это даёт регрессию. Huge pages (MEM_LARGE_PAGES) от этого не зависят —
    /// они включаются через `ensure_lock_memory_privilege` независимо.
    #[cfg_attr(coverage, coverage(off))]
    pub fn prefetch_async(ptr: *mut u8, len: usize) {
        if len == 0 {
            return;
        }
        // Когда фича выключена, `ptr` не используется — гасим warning.
        #[cfg(not(feature = "win-prefetch-pages"))]
        let _ = ptr;
        // Под Miri PrefetchVirtualMemory не зашимлен — пропускаем.
        if cfg!(miri) {
            return;
        }
        #[cfg(feature = "win-prefetch-pages")]
        unsafe {
            let range = WIN32_MEMORY_RANGE_ENTRY {
                VirtualAddress: ptr as *mut _,
                NumberOfBytes: len,
            };
            PrefetchVirtualMemory(GetCurrentProcess(), 1, &range, 0);
        }
    }

    /// На Windows huge pages уже получены через MEM_LARGE_PAGES, подсказка не нужна.
    pub fn advise_huge(_ptr: *mut u8, _len: usize) {}
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

    /// Просим ядро использовать transparent huge pages для региона (best-effort).
    /// Работает без резервирования явных huge pages; если THP выключен — no-op.
    pub fn advise_huge(ptr: *mut u8, len: usize) {
        #[cfg(target_os = "linux")]
        unsafe {
            libc::madvise(ptr as *mut libc::c_void, len, libc::MADV_HUGEPAGE);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (ptr, len);
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
        // Размер под huge pages должен быть кратен huge_page_size(), иначе
        // MEM_LARGE_PAGES / MAP_HUGETLB молча не срабатывают и регион
        // выделяется обычными 4KB-страницами. Округляем только для крупных
        // арен (мелкие и так не получают huge pages и должны сохранять
        // точный размер — иначе ломаются тесты границ/OOM).
        let huge = platform::huge_page_size();
        let cap = if total_capacity >= huge {
            total_capacity.next_multiple_of(huge)
        } else {
            total_capacity
        };
        let alloc = platform::try_alloc_huge(cap).unwrap_or_else(|| platform::alloc_normal(cap));
        // Best-effort THP (Linux): работает без резервирования явных huge pages.
        platform::advise_huge(alloc.ptr, alloc.size);

        if verbose_enabled() {
            println!(
                "  [Arena] Р вЂ™РЎвЂ№Р Т‘Р ВµР В»Р ВµР Р…Р С• {} MB (Р В·Р В°Р С—РЎР‚Р С•РЎв‚¬Р ВµР Р…Р С• {} MB), huge pages: {}",
                cap / (1024 * 1024),
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

    /// Разбить арену на `num_threads` непересекающихся чанков заданного
    /// направления заполнения. Для `MiddleOut` каждый чанк стартует из своей
    /// середины.
    // Выравниваем размер чанка, чтобы старт каждого чанка (base + i*chunk_size)
    #[allow(dead_code)]
    pub unsafe fn split(&self, num_threads: usize) -> Vec<CachePadded<ThreadBump>> {
        unsafe { self.split_with(num_threads, BumpDir::Forward) }
    }
    // оставался выровненным (иначе указатели доноров — UB под Tree Borrows).

    pub fn split_safe(&self, num_threads: usize) -> Split<'_> {
        self.split_with_safe(num_threads, BumpDir::Forward)
    }
    /// Чётные потоки заполняют свой чанк справа налево (`Backward`), нечётные —
    /// слева направо (`Forward`). Так соседние чанки «доходят» до общей границы
    /// навстречу друг другу (идея совместного использования памяти соседа).
    // Выравниваем размер чанка, чтобы старт каждого чанка (base + i*chunk_size)
    // оставался выровненным (иначе указатели доноров — UB под Tree Borrows).

    /// Возвращаемый `Vec` не связан с временем жизни арены: bumps держат сырые
    /// указатели в регионе. Вызывающий обязан держать `self` живой и не дать
    /// bumps пережить арену (иначе use-after-free).
    unsafe fn split_with(&self, num_threads: usize, dir: BumpDir) -> Vec<CachePadded<ThreadBump>> {
        let base = self.alloc.ptr;
        let total = self.alloc.size;
        // Выравниваем размер чанка, чтобы старт каждого чанка (base + i*chunk_size)
        // оставался выровненным (иначе указатели доноров — UB под Tree Borrows).
        let chunk_size = (total / num_threads).next_multiple_of(16);

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
                    #[cfg(test)]
                    can_give: false,
                    self_index: 0,
                    donor_array: ptr::null(),
                    donor_reg: DonorReg::None,
                    donor_static_ptr: None,
                    donor_static_len: 0,
                    donor_bump: None,
                    use_priority: false,
                    base_chunk: 0,
                    fallback: SpinMutex::new(FallbackChunks {
                        chunks: Vec::new(),
                        used: 0,
                    }),
                })
            })
            .collect()
    }

    /// Безопасная версия `split_with`: возвращает sound-`Split`.
    pub fn split_with_safe(&self, num_threads: usize, dir: BumpDir) -> Split<'_> {
        Split {
            arena: self,
            bumps: unsafe { self.split_with(num_threads, dir) },
        }
    }

    /// Чётные потоки заполняют свой чанк слева направо (`Backward`), нечётные —
    /// справа налево (`Forward`). Так каждый чанк доходит до своей половины
    /// навстречу друг другу (идея совместного использования памяти соседом).
    ///
    /// # Safety
    /// Возвращённый `Vec` не связывает время жизни с ареной. Требуется держать
    /// `self` живым и не давать эти bumps после того, как арена будет
    /// освобождена.
    unsafe fn split_alternating(&self, num_threads: usize) -> Vec<CachePadded<ThreadBump>> {
        let base = self.alloc.ptr;
        let total = self.alloc.size;
        // Выравниваем размер чанка, чтобы старт каждого чанка (base + i*chunk_size)
        // оставался выровненным (иначе указатели доноров — UB под Tree Borrows).
        let chunk_size = (total / num_threads).next_multiple_of(16);

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
                    #[cfg(test)]
                    can_give: false,
                    self_index: 0,
                    donor_array: ptr::null(),
                    donor_reg: DonorReg::None,
                    donor_static_ptr: None,
                    donor_static_len: 0,
                    donor_bump: None,
                    use_priority: false,
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
    /// слева направо (`Forward`). Так соседние чанки растут навстречу друг другу.
    pub fn split_alternating_safe(&self, num_threads: usize) -> Split<'_> {
        Split {
            arena: self,
            bumps: unsafe { self.split_alternating(num_threads) },
        }
    }

    /// # Safety
    /// Возвращённый `Vec` не связывает время жизни с ареной. Требуется держать
    /// `self` живым и не давать эти bumps после того, как арена будет
    /// освобождена.
    unsafe fn split_paired(&self, num_threads: usize) -> Vec<CachePadded<ThreadBump>> {
        let base = self.alloc.ptr;
        let total = self.alloc.size;
        // Выравниваем размер чанка, чтобы старт каждого чанка (base + i*chunk_size)
        // оставался выровненным (иначе указатели доноров — UB под Tree Borrows).
        let chunk_size = (total / num_threads).next_multiple_of(16);
        let is_huge = self.alloc.is_huge;

        let mut bumps: Vec<CachePadded<ThreadBump>> = (0..num_threads)
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
                        #[cfg(test)]
                        can_give: false,
                        self_index: 0,
                        donor_array: ptr::null(),
                        donor_reg: DonorReg::None,
                        donor_static_ptr: None,
                        donor_static_len: 0,
                        donor_bump: None,
                        use_priority: false,
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
                        #[cfg(test)]
                        can_give: false,
                        self_index: 0,
                        donor_array: ptr::null(),
                        donor_reg: DonorReg::None,
                        donor_static_ptr: None,
                        donor_static_len: 0,
                        donor_bump: None,
                        use_priority: false,
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
        for (i, pair) in bumps.chunks_exact_mut(2).enumerate() {
            let k = i * 2;
            let (a, b) = pair.split_at_mut(1);
            a[0].neighbor_idx = Some(k + 1);
            b[0].neighbor_idx = Some(k);
            a[0].array = array;
            b[0].array = array;
        }
        bumps
    }

    /// Объединённый регион ПАРЫ: соседние потоки (2k — `Backward`, 2k+1 —
    /// `Forward`) делят ОДИН регион размером `2 * chunk_size` и заполняют его
    /// навстречу друг другу от внешних краёв к середине. Когда одна сторона
    /// доходит до середины и в своём чанке места больше нет, она «берёт»
    /// смежную свободную половину соседа (через lock-free CAS по счётчику
    /// соседа, см. `ThreadBump::try_borrow`). Если соседей нечётное число,
    /// последний поток получает собственный изолированный чанк без соседа.
    pub fn split_paired_safe(&self, num_threads: usize) -> Split<'_> {
        Split {
            arena: self,
            bumps: unsafe { self.split_paired(num_threads) },
        }
    }

    /// Режим «доноры» (отдельная версия): каждый поток получает свой чанк
    /// (`Forward`), а некоторые чанки помечаются как способные отдавать память
    /// (`can_give`). Когда у потока в своём чанке кончается место, он проходит
    /// по списку доноров и берёт блок с «другой стороны» региона донора; если и
    /// у доноров нет свободного — выделяет новый чанк «где-то в памяти»
    /// (см. `ThreadBump::try_take_from_donors` / `grow_fallback`).
    ///
    /// `donor_every` — каждый `donor_every`-й чанк (начиная с 0) помечается
    /// донором. Удобная обёртка: статичный список доноров без приоритета
    /// (каждый `donor_every`-й поток, начиная с 0, помечается донором).
    ///
    /// # Safety
    /// Возвращённый `Vec` не связывает время жизни с ареной. Требуется держать
    /// `self` живым и не давать эти bumps после того, как арена будет
    /// освобождена.
    #[allow(dead_code)]
    pub unsafe fn split_donors(
        &self,
        num_threads: usize,
        donor_every: usize,
    ) -> Vec<CachePadded<ThreadBump>> {
        unsafe { self.split_donors_with(num_threads, DonorPolicy::static_(donor_every)) }
    }

    /// Полная версия режима «доноры» с выбором хранилища списка доноров
    /// (`DonorPolicy::kind`), приоритетом (`DonorPolicy::use_priority`) и
    /// правилом пометки доноров (`DonorPolicy::every`). Безопасная обёртка:
    /// возвращает sound-`Split`.
    #[allow(dead_code)]
    pub fn split_donors_safe(&self, num_threads: usize, donor_every: usize) -> Split<'_> {
        self.split_donors_with_safe(num_threads, DonorPolicy::static_(donor_every))
    }

    /// Каждый поток получает свой `Forward`-чанк. Помеченные доноры заносятся в
    /// реестр (только они, чтобы при переполнении не перебирать все bumps). При
    /// переполнении поток берёт блок с «другой стороны» региона донора; если
    /// свободных доноров нет — выделяет новый чанк «где-то в памяти»
    /// (см. `ThreadBump::try_take_from_donors` / `grow_fallback`).
    ///
    /// # Safety
    /// Возвращённый `Vec` не связывает время жизни с ареной. Требуется держать
    /// `self` живым и не давать эти bumps после того, как арена будет
    /// освобождена.
    unsafe fn split_donors_with(
        &self,
        num_threads: usize,
        policy: DonorPolicy,
    ) -> Vec<CachePadded<ThreadBump>> {
        let base = self.alloc.ptr;
        let total = self.alloc.size;
        // Выравниваем размер чанка, чтобы старт каждого чанка (base + i*chunk_size)
        // оставался выровненным (иначе указатели доноров — UB под Tree Borrows).
        let chunk_size = (total / num_threads).next_multiple_of(16);
        let is_huge = self.alloc.is_huge;

        // Собираем записи о донорах в bump-скретч: служебные данные — не
        // глобальный аллокатор, а bumpalo (освобождается в конце функции).
        let scratch = bumpalo::Bump::new();
        let mut entries: bumpalo::collections::Vec<Donor> =
            bumpalo::collections::Vec::new_in(&scratch);
        for i in 0..num_threads {
            let is_donor = policy.every > 0 && i % policy.every == 0;
            if is_donor {
                let priority = match &policy.priorities {
                    Some(p) => *p.get(i).unwrap_or(&(i as u32)),
                    None => i as u32,
                };
                entries.push(Donor {
                    idx: i,
                    priority,
                    removed: AtomicBool::new(false),
                });
            }
        }
        let num_donors = entries.len();

        // Строим реестр нужного вида. Статичный список размещаем в собственном
        // bump-буфере (без глоб. аллокатора, без Arc-накладных); динамические —
        // с capacity-подсказкой (with_capacity).
        let mut donor_static_ptr: Option<*const Donor> = None;
        let mut donor_static_len: usize = 0;
        let mut donor_bump: Option<Arc<bumpalo::Bump>> = None;
        let reg = match policy.kind {
            DonorListKind::Static => {
                let bump = Arc::new(bumpalo::Bump::new());
                donor_bump = Some(bump);
                let b = donor_bump.as_ref().unwrap();
                let mut bv: bumpalo::collections::Vec<Donor> =
                    bumpalo::collections::Vec::with_capacity_in(num_donors, b);
                for d in entries.iter() {
                    bv.push(d.clone());
                }
                if policy.use_priority {
                    bv.sort_by_key(|d| d.priority);
                }
                donor_static_ptr = Some(bv.as_ptr());
                donor_static_len = bv.len();
                DonorReg::Static
            }
            DonorListKind::Orx => {
                // orx-concurrent-vec растёт сам (lock-free); capacity-подсказку
                // дать не в виде одного числа нельзя, поэтому просто new().
                let v: OrxVec<Donor> = OrxVec::new();
                for d in entries.iter() {
                    v.push(d.clone());
                }
                DonorReg::Orx(Arc::new(v))
            }
            DonorListKind::Boxcar => {
                let v: BoxcarVec<Donor> = BoxcarVec::with_capacity(num_donors);
                for d in entries.iter() {
                    v.push(d.clone());
                }
                DonorReg::Boxcar(Arc::new(v))
            }
        };

        let mut bumps: Vec<CachePadded<ThreadBump>> = (0..num_threads)
            .map(|i| {
                #[cfg(test)]
                let can_give = policy.every > 0 && i % policy.every == 0;
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
                    #[cfg(test)]
                    can_give,
                    self_index: i,
                    donor_array: ptr::null(), // заполним ниже
                    donor_reg: reg.clone(),
                    donor_static_ptr,
                    donor_static_len,
                    donor_bump: donor_bump.clone(),
                    use_priority: policy.use_priority,
                    base_chunk: chunk_size,
                    fallback: SpinMutex::new(FallbackChunks {
                        chunks: Vec::new(),
                        used: 0,
                    }),
                })
            })
            .collect();

        // Проставляем указатель на массив bumps — буфер Vec стабилен, поэтому
        // raw-указатель валиден, пока живут потоки.

        let array = bumps.as_ptr();
        for b in bumps.iter_mut() {
            b.donor_array = array;
        }

        bumps
    }

    /// Безопасная версия `split_donors_with`: возвращает sound-`Split`.
    pub fn split_donors_with_safe(&self, num_threads: usize, policy: DonorPolicy) -> Split<'_> {
        Split {
            arena: self,
            bumps: unsafe { self.split_donors_with(num_threads, policy) },
        }
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
// Наружу разделяемости.
//
// `Split<'a>` автоматически `Send`/`Sync`, если `ThreadBump: Send + Sync` (а он им
// обязан через `unsafe impl`), т.е. раздавать `&bumps[i]` в scoped-потоки можно.
pub struct Split<'a> {
    #[allow(dead_code)]
    arena: &'a SharedArena,
    bumps: Vec<CachePadded<ThreadBump>>,
}

impl<'a> Split<'a> {
    #[inline]
    pub fn bumps(&self) -> &[CachePadded<ThreadBump>] {
        &self.bumps
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.bumps.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bumps.is_empty()
    }

    /// Доступ к конкретному bump'у по индексу (safe, но lifetime привязан к `&self`).
    #[inline]
    pub fn get(&self, i: usize) -> Option<&CachePadded<ThreadBump>> {
        self.bumps.get(i)
    }

    /// Прорваться в разделяемый подмассив (для передачи `&bumps[i]` в потоке).
    #[inline]
    pub fn as_slice(&self) -> &[CachePadded<ThreadBump>] {
        &self.bumps
    }
}

impl<'a> std::ops::Deref for Split<'a> {
    type Target = [CachePadded<ThreadBump>];
    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.bumps
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

pub struct ThreadBump {
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
    #[cfg(test)]
    can_give: bool,
    /// Собственный индекс в массиве bumps (чтобы не брать у самого себя).
    self_index: usize,
    /// Указатель на массив bumps для обхода доноров (`null` вне режима доноров).
    donor_array: *const CachePadded<ThreadBump>,
    /// Реестр доноров: либо `None` (режим доноров не активен), либо список
    /// только доноров (статичный или динамический), чтобы при переполнении не
    /// перебирать ВСЕ bumps и не проверять каждый на `can_give`.
    donor_reg: DonorReg,
    /// Указатель на плоский срез доноров (для `DonorReg::Static`). Память
    /// выделена в `bumpalo::Bump` (`donor_bump`), поэтому это НЕ глобальный
    /// аллокатор и НЕ `Arc`. Валиден, пока жив `donor_bump`.
    /// Длина среза `donor_static_ptr`.
    donor_static_ptr: Option<*const Donor>,
    /// Длина среза `donor_static_ptr` (валиден только при `Some`).
    donor_static_len: usize,
    /// `bumpalo::Bump`, в котором лежит статичный список доноров (если
    /// `DonorReg::Static`). Хранится как `Arc`, чтобы разделять между всеми
    /// bumps; освобождается, когда последний bump выходит из области видимости.
    /// Вместо этого используется `Arc` для keep-alive (drop-guard): пока жив
    /// `Arc`, жив и буфер, на который указывает `donor_static_ptr`.
    #[allow(dead_code)]
    donor_bump: Option<Arc<bumpalo::Bump>>,
    /// Использовать ли приоритет при выборе донора (см. `Donor::priority`).
    use_priority: bool,
    /// Размер первичного чанка (для размера fallback-чанков).
    base_chunk: usize,
    /// Дополнительные чанки, выделенные «где-то в памяти» при нехватке у
    /// доноров. Освобождаются в `Drop`.
    fallback: SpinMutex<FallbackChunks>,
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
    // ===== Кодировка `MODE` для полностью мономорфных версий аллокатора =====
    // Вся runtime-конфигурация направления/пары/доноров, влияющая на hot-path
    // `alloc_raw`, упакована в один compile-time `const MODE: u32`. Мономорфные
    // версии (`alloc_raw_m`, `ArenaVec_m`, `ArenaString_m`, `arena_bench_impl`)
    // получают MODE как const-генерик, поэтому в их коде нет ни одного
    // runtime-ветвления по `dir`/`neighbor_idx`/`donor_reg` — компилятор
    // убирает мёртвые ветки на этапе компиляции.
    const MODE_DIR_FORWARD: u32 = 0;
    const MODE_DIR_BACKWARD: u32 = 1;
    const MODE_DIR_MIDDLEOUT: u32 = 2;
    const MODE_DIR_MASK: u32 = 0b11;
    const MODE_PAIR: u32 = 1 << 2;
    #[allow(dead_code)]
    const MODE_DONOR_NONE: u32 = 0 << 3;
    const MODE_DONOR_STATIC: u32 = 1 << 3;
    const MODE_DONOR_ORX: u32 = 2 << 3;
    const MODE_DONOR_BOXCAR: u32 = 3 << 3;
    const MODE_DONOR_MASK: u32 = 0b11 << 3;
    const MODE_PRIO: u32 = 1 << 5;

    // inner-loop аллокатора: не инструментируем (весь цикл меряется `measure_block!`).
    //
    // Обёртка разворачивается в вызов специализации по направлению: DIR —
    // const generic, поэтому `match DIR` в alloc_raw_dir разворачивается на
    // этапе компиляции, мёртвые ветки исчезают и hot-loop получается компактным
    // (меньше давление на I-cache). Публичный alloc_raw::<ALIGN> не меняется.
    #[inline(always)]
    fn alloc_raw<const ALIGN: usize>(&self, size: usize) -> *mut u8 {
        match self.dir {
            BumpDir::Forward => self.alloc_raw_dir::<ALIGN, { BumpDir::Forward as u8 }>(size),
            BumpDir::Backward => self.alloc_raw_dir::<ALIGN, { BumpDir::Backward as u8 }>(size),
            BumpDir::MiddleOut => self.alloc_raw_dir::<ALIGN, { BumpDir::MiddleOut as u8 }>(size),
        }
    }

    #[inline(always)]
    fn alloc_raw_dir<const ALIGN: usize, const DIR: u8>(&self, size: usize) -> *mut u8 {
        // `lo` — байт выделено с левого края (для Backward/MiddleOut семантика
        // своя, см. ниже). `hi` — байт выделено с правого края.
        // ALIGN — константа времени компиляции (задаётся в точке вызова,
        // см. alloc_uninit_slice), поэтому маска выравнивания считается на этапе
        // компиляции, а не прокидывается через аргумент в рантайме.
        // DIR — const generic, поэтому `match dir` ниже разворачивается на
        // этапе компиляции: мёртвые ветки направлений исчезают из hot-loop.
        let dir = match DIR {
            0 => BumpDir::Forward,
            1 => BumpDir::Backward,
            _ => BumpDir::MiddleOut,
        };
        let (off, new_lo, new_hi, pf) = match dir {
            // Forward: растём от начала чанка вверх. В режиме пары собственная
            // половина — нижняя [0, mid); середина региона `mid = len/2`.
            //
            // Обновление `lo` делаем через CAS, потому что тот же счётчик
            // параллельно может расширять сосед (заём памяти), см. `try_borrow`.
            BumpDir::Forward => {
                // Быстрый путь: bump без пары и без доноров — этот поток
                // единственный владелец `lo`, поэтому CAS-цикл не нужен
                // (relaxed load+store быстрее lock-prefixed cmpxchg).
                if self.neighbor_idx.is_none() && matches!(self.donor_reg, DonorReg::None) {
                    let cur = self.lo.load(Ordering::Relaxed);
                    let off = (cur + ALIGN - 1) & !(ALIGN - 1);
                    let end = off + size;
                    if end <= self.len {
                        self.lo.store(end, Ordering::Relaxed);
                        let p = unsafe { self.ptr.add(off) };
                        prefetch_write(unsafe { self.ptr.add(off + size) });
                        return p;
                    }
                    panic!(
                        "Arena OOM: need {} at offset {}, free {} (cap {})",
                        size,
                        off,
                        self.len - self.lo.load(Ordering::Relaxed),
                        self.len
                    );
                }
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
                            // Forward-заполнение идёт вверх: префетчим следующий блок,
                            // в который будем писать при следующем выделении.
                            let p = unsafe { self.ptr.add(off) };
                            prefetch_write(unsafe { self.ptr.add(off + size) });
                            return p;
                        }
                    } else if let Some(p) = self.try_borrow::<ALIGN>(size) {
                        return p;
                    } else if !matches!(self.donor_reg, DonorReg::None) {
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
                // Быстрый путь: без пары и доноров — единственный владелец `hi`.
                if self.neighbor_idx.is_none() && matches!(self.donor_reg, DonorReg::None) {
                    let cur = self.hi.load(Ordering::Relaxed);
                    let off = (self.len - cur - size) & !(ALIGN - 1);
                    let new_hi = self.len - off;
                    self.hi.store(new_hi, Ordering::Relaxed);
                    let p = unsafe { self.ptr.add(off) };
                    if off >= size {
                        prefetch_write(unsafe { self.ptr.add(off - size) });
                    }
                    return p;
                }
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
                            // Backward-заполнение идёт вниз: префетчим следующий
                            // блок (по меньшему адресу), в который будем писать.
                            let p = unsafe { self.ptr.add(off) };
                            if off >= size {
                                prefetch_write(unsafe { self.ptr.add(off - size) });
                            }
                            return p;
                        }
                    } else if let Some(p) = self.try_borrow::<ALIGN>(size) {
                        return p;
                    } else if !matches!(self.donor_reg, DonorReg::None) {
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
                    let new_lo = mid - off;
                    let new_hi = self.hi.load(Ordering::Relaxed);
                    // следующая аллокация пойдёт в правую половину
                    let pf = unsafe { self.ptr.add(mid + new_hi + size) };
                    (off, new_lo, new_hi, pf)
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
                    let new_lo = self.lo.load(Ordering::Relaxed);
                    let new_hi = off + size - mid;
                    // следующая аллокация пойдёт в левую половину
                    let pf = unsafe { self.ptr.add(mid.wrapping_sub(new_lo + size)) };
                    (off, new_lo, new_hi, pf)
                }
            }
        };

        self.lo.store(new_lo, Ordering::Relaxed);
        self.hi.store(new_hi, Ordering::Relaxed);
        let p = unsafe { self.ptr.add(off) };
        // MiddleOut чередует стороны (то вправо, то влево) — аппаратный
        // предсказчик шага не выведет чередование, поэтому сами префетчим
        // фронтир противоположной стороны (туда пойдёт следующая аллокация).
        prefetch_write(pf);
        p
    }

    // ============ Полностью мономорфные версии аллокатора ============
    // `alloc_raw_m` — без единого runtime-ветвления по dir/neighbor_idx/
    // donor_reg: вся конфигурация задана `const MODE`. Мёртвые ветви (dir,
    // pair, доноры) выкидываются на этапе компиляции, hot-loop получается
    // максимально компактным. Используется мономорфными бенч-путами
    // (`ArenaVec_m`/`ArenaString_m`/`arena_bench_impl`). Старые версии
    // (`alloc_raw`, `alloc_raw_dir`) остаются нетронутыми и для общих путей.
    #[inline(always)]
    #[cfg_attr(coverage, coverage(off))]
    fn alloc_raw_m<const ALIGN: usize, const MODE: u32>(&self, size: usize) -> *mut u8 {
        // `let`-привязки к const-generic `MODE` — константы времени компиляции:
        // const-fold через инлайнинг, мёртвые ветки выкидываются.
        let pair = MODE & ThreadBump::MODE_PAIR != 0;
        let donor = (MODE & ThreadBump::MODE_DONOR_MASK) >> 3;
        let dir = MODE & ThreadBump::MODE_DIR_MASK;
        match dir {
            ThreadBump::MODE_DIR_FORWARD => {
                if !pair && donor == 0 {
                    // Полный fast path: единственный владелец `lo` — ни CAS, ни
                    // try_borrow/donor-веток, только load+store.
                    let cur = self.lo.load(Ordering::Relaxed);
                    let off = (cur + ALIGN - 1) & !(ALIGN - 1);
                    let end = off + size;
                    if end <= self.len {
                        self.lo.store(end, Ordering::Relaxed);
                        let p = unsafe { self.ptr.add(off) };
                        prefetch_write(unsafe { self.ptr.add(off + size) });
                        return p;
                    }
                    panic!(
                        "Arena OOM: need {} at offset {}, free {} (cap {})",
                        size,
                        off,
                        self.len - self.lo.load(Ordering::Relaxed),
                        self.len
                    );
                }
                let mid = if pair { self.len / 2 } else { self.len };
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
                            let p = unsafe { self.ptr.add(off) };
                            prefetch_write(unsafe { self.ptr.add(off + size) });
                            return p;
                        }
                    } else if pair {
                        if let Some(p) = self.try_borrow::<ALIGN>(size) {
                            return p;
                        }
                        if donor != 0 {
                            if let Some(p) = self.try_take_from_donors::<ALIGN>(size) {
                                return p;
                            }
                            return self.grow_fallback::<ALIGN>(size);
                        }
                        panic!(
                            "Arena OOM: need {} at offset {}, free {} (cap {})",
                            size,
                            off,
                            mid - self.lo.load(Ordering::Relaxed),
                            self.len
                        );
                    } else {
                        // !pair с donor != 0 (случай !pair && donor==0 вернулся выше).
                        if let Some(p) = self.try_take_from_donors::<ALIGN>(size) {
                            return p;
                        }
                        return self.grow_fallback::<ALIGN>(size);
                    }
                }
            }
            ThreadBump::MODE_DIR_BACKWARD => {
                if !pair && donor == 0 {
                    let cur = self.hi.load(Ordering::Relaxed);
                    let off = (self.len - cur - size) & !(ALIGN - 1);
                    let new_hi = self.len - off;
                    self.hi.store(new_hi, Ordering::Relaxed);
                    let p = unsafe { self.ptr.add(off) };
                    if off >= size {
                        prefetch_write(unsafe { self.ptr.add(off - size) });
                    }
                    return p;
                }
                let mid = if pair { self.len / 2 } else { 0 };
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
                            let p = unsafe { self.ptr.add(off) };
                            if off >= size {
                                prefetch_write(unsafe { self.ptr.add(off - size) });
                            }
                            return p;
                        }
                    } else if pair {
                        if let Some(p) = self.try_borrow::<ALIGN>(size) {
                            return p;
                        }
                        if donor != 0 {
                            if let Some(p) = self.try_take_from_donors::<ALIGN>(size) {
                                return p;
                            }
                            return self.grow_fallback::<ALIGN>(size);
                        }
                        panic!(
                            "Arena OOM: need {} bytes, only {} free (cap {})",
                            size,
                            self.len - cur - mid,
                            self.len
                        );
                    } else {
                        if let Some(p) = self.try_take_from_donors::<ALIGN>(size) {
                            return p;
                        }
                        return self.grow_fallback::<ALIGN>(size);
                    }
                }
            }
            _ => {
                let mid = self.len / 2;
                let side = self.toggle.get();
                self.toggle.set(!side);
                if !side {
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
                    let new_lo = mid - off;
                    let new_hi = self.hi.load(Ordering::Relaxed);
                    let pf = unsafe { self.ptr.add(mid + new_hi + size) };
                    self.lo.store(new_lo, Ordering::Relaxed);
                    self.hi.store(new_hi, Ordering::Relaxed);
                    let p = unsafe { self.ptr.add(off) };
                    prefetch_write(pf);
                    p
                } else {
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
                    let new_lo = self.lo.load(Ordering::Relaxed);
                    let new_hi = off + size - mid;
                    let pf = unsafe { self.ptr.add(mid.wrapping_sub(new_lo + size)) };
                    self.lo.store(new_lo, Ordering::Relaxed);
                    self.hi.store(new_hi, Ordering::Relaxed);
                    let p = unsafe { self.ptr.add(off) };
                    prefetch_write(pf);
                    p
                }
            }
        }
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
                    // Сосед-Forward отдаёт с низкой стороны (растёт вверх):
                    // следующий заём — по большему адресу.
                    let p = unsafe { self.ptr.add(off) };
                    prefetch_write(unsafe { self.ptr.add(off + size) });
                    return Some(p);
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
                    // Сосед-Backward отдаёт с высокой стороны (растёт вниз):
                    // следующий заём — по меньшему адресу.
                    let p = unsafe { self.ptr.add(off) };
                    if off >= size {
                        prefetch_write(unsafe { self.ptr.add(off - size) });
                    }
                    return Some(p);
                }
            }
        }
    }

    // ===== Режим «доноры»: заём чанка у помеченных доноров, иначе новый чанк =====

    /// Обойти только доноров из реестра и попытаться взять у одного из них блок
    /// `size` с «другой стороны» его региона (противоположной его заполнению).
    /// Возвращает `Some(ptr)` при успехе или `None`, если ни у одного донора нет
    /// свободной смежной половины. Перебираются ТОЛЬКО доноры (а не все bumps).
    #[inline(always)]
    fn try_take_from_donors<const ALIGN: usize>(&self, size: usize) -> Option<*mut u8> {
        match &self.donor_reg {
            DonorReg::None => None,
            // Статичный список: при use_priority уже отсортирован по возрастанию
            // приоритета, поэтому первый подходящий имеет минимальный приоритет.
            DonorReg::Static => {
                // Статичный список лежит в bump-буфере (raw ptr + len). При
                // use_priority он уже отсортирован по возрастанию приоритета,
                // поэтому первый подходящий донор имеет минимальный приоритет.
                let ptr = self
                    .donor_static_ptr
                    .expect("donor_static_ptr задаётся для DonorReg::Static");
                let slice = unsafe { std::slice::from_raw_parts(ptr, self.donor_static_len) };
                for d in slice {
                    if d.idx == self.self_index {
                        continue;
                    }
                    if let Some(p) = self.take_from_donor::<ALIGN>(d.idx, size) {
                        return Some(p);
                    }
                }
                None
            }
            DonorReg::Orx(v) => {
                self.take_from_registry::<ALIGN, _>(v.iter().map(|e| e.cloned()), size)
            }
            DonorReg::Boxcar(v) => {
                self.take_from_registry::<ALIGN, _>(v.iter().map(|(_, d)| d.clone()), size)
            }
        }
    }

    /// Обход динамического реестра (orx/boxcar). Без приоритета — первый
    /// подходящий; с приоритетом — из доступных выбирается донор с минимальным
    /// приоритетом (== высокоприоритетные берутся в последнюю очередь).
    /// Логически удалённые (`removed`) доноры пропускаются.
    #[inline(always)]
    fn take_from_registry<const ALIGN: usize, I>(&self, iter: I, size: usize) -> Option<*mut u8>
    where
        I: Iterator<Item = Donor>,
    {
        if !self.use_priority {
            for d in iter {
                if d.removed.load(Ordering::Relaxed) || d.idx == self.self_index {
                    continue;
                }
                if let Some(p) = self.take_from_donor::<ALIGN>(d.idx, size) {
                    return Some(p);
                }
            }
            return None;
        }
        let mut best: Option<(u32, usize)> = None;
        for d in iter {
            if d.removed.load(Ordering::Relaxed) || d.idx == self.self_index {
                continue;
            }
            if self.donor_has_space::<ALIGN>(d.idx, size)
                && best.map_or(true, |(bp, _)| d.priority < bp)
            {
                best = Some((d.priority, d.idx));
            }
        }
        best.and_then(|(_, idx)| self.take_from_donor::<ALIGN>(idx, size))
    }

    /// Непересекающаяся ли свободная «чужая» половина у донора (без изменений)?
    #[inline(always)]
    fn donor_has_space<const ALIGN: usize>(&self, donor_idx: usize, size: usize) -> bool {
        let d = unsafe { &*self.donor_array.add(donor_idx) };
        if d.dir == BumpDir::Forward {
            let cur = d.hi.load(Ordering::Relaxed);
            if cur + size > d.len {
                return false;
            }
            let off = (d.len - cur - size) & !(ALIGN - 1);
            off >= d.lo.load(Ordering::Relaxed)
        } else {
            let cur = d.lo.load(Ordering::Relaxed);
            let hi = d.hi.load(Ordering::Relaxed);
            if hi > d.len {
                return false;
            }
            let off = (cur + ALIGN - 1) & !(ALIGN - 1);
            off + size <= d.len - hi
        }
    }

    /// Взять блок `size` из региона донора `donor_idx` с противоположной его
    /// заполнению стороны, расширяя соответствующий счётчик донора через
    /// lock-free CAS.
    #[inline(always)]
    fn take_from_donor<const ALIGN: usize>(
        &self,
        donor_idx: usize,
        size: usize,
    ) -> Option<*mut u8> {
        let d = unsafe { &*self.donor_array.add(donor_idx) };
        if d.dir == BumpDir::Forward {
            // Донор заполняет низ [0, lo); отдаём с высокой стороны [len - hi, len).
            loop {
                let cur = d.hi.load(Ordering::Relaxed);
                if cur + size > d.len {
                    return None; // донор исчерпан — не даём hi превысить len
                }
                let off = (d.len - cur - size) & !(ALIGN - 1);
                if off < d.lo.load(Ordering::Relaxed) {
                    return None; // упёрлись в собственные данные донора
                }
                let new_hi = d.len - off;
                if d.hi
                    .compare_exchange_weak(cur, new_hi, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    // Донор-Forward растёт вниз: следующий кусок — по меньшему адресу.
                    let p = unsafe { d.ptr.add(off) };
                    if off >= size {
                        prefetch_write(unsafe { d.ptr.add(off - size) });
                    }
                    return Some(p);
                }
            }
        } else {
            // Донор заполняет высокую [len - hi, len); отдаём с низкой [0, lo).
            loop {
                let cur = d.lo.load(Ordering::Relaxed);
                let hi = d.hi.load(Ordering::Relaxed);
                if hi > d.len {
                    return None; // Р Т‘Р С•Р Р…Р С•РЎР‚ Р С‘Р В·РЎР‚Р В°РЎРѓРЎвЂ¦Р С•Р Т‘Р С•Р Р†Р В°Р Р… РІР‚вЂќ Р В·Р В°РЎвЂ°Р С‘РЎвЂљР В° Р С•РЎвЂљ underflow
                }
                let off = (cur + ALIGN - 1) & !(ALIGN - 1);
                let end = off + size;
                if end > d.len - hi {
                    return None;
                }
                if d.lo
                    .compare_exchange_weak(cur, end, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    // Донор-Backward растёт вверх: следующий кусок — по большему адресу.
                    let p = unsafe { d.ptr.add(off) };
                    prefetch_write(unsafe { d.ptr.add(off + size) });
                    return Some(p);
                }
            }
        }
    }

    /// Добавить донора в рантайме. Работает только для динамических реестров
    /// (orx/boxcar); для статичного/отсутствующего — возвращает `false`. Если
    /// донор с таким индексом уже есть — не дублирует (возвращает `false`).
    pub fn add_donor(&self, idx: usize, priority: u32) -> bool {
        match &self.donor_reg {
            DonorReg::None | DonorReg::Static => false,
            DonorReg::Orx(v) => {
                if v.iter().any(|e| {
                    let d = e.cloned();
                    d.idx == idx && !d.removed.load(Ordering::Relaxed)
                }) {
                    return false;
                }
                v.push(Donor {
                    idx,
                    priority,
                    removed: AtomicBool::new(false),
                });
                true
            }
            DonorReg::Boxcar(v) => {
                if v.iter()
                    .any(|(_, d)| d.idx == idx && !d.removed.load(Ordering::Relaxed))
                {
                    return false;
                }
                v.push(Donor {
                    idx,
                    priority,
                    removed: AtomicBool::new(false),
                });
                true
            }
        }
    }

    /// Удалить донора по индексу в рантайме (только для динамических реестров).
    /// Физического удаления из lock-free вектора не происходит — запись
    /// помечается `removed`, и обход её пропускает.
    pub fn remove_donor(&self, idx: usize) -> bool {
        match &self.donor_reg {
            DonorReg::None | DonorReg::Static => false,
            DonorReg::Orx(v) => {
                for e in v.iter() {
                    let d = e.cloned();
                    if d.idx == idx && !d.removed.load(Ordering::Relaxed) {
                        // Помечаем живой элемент удалённым.
                        e.map(|x| x.removed.store(true, Ordering::Relaxed));
                        return true;
                    }
                }
                false
            }
            DonorReg::Boxcar(v) => {
                for (_, d) in v.iter() {
                    if d.idx == idx && !d.removed.load(Ordering::Relaxed) {
                        d.removed.store(true, Ordering::Relaxed);
                        return true;
                    }
                }
                false
            }
        }
    }

    /// Выделить совершенно новый чанк «где-то в памяти» (через платформенный
    /// аллокатор) и вернуть в нём указатель на блок `size`. Чанк сохраняется в
    /// `fallback` и освобождается в `Drop`. Адрес возвращается вызывающему — его
    /// собственный регион при этом не трогается.
    #[hotpath::measure]
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
        // Новый чанк «где-то в памяти»: префетчим следующий блок в нём.
        prefetch_write(unsafe { chunk.ptr.add(off + size) });
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

    /// Мономорфная версия `alloc_uninit_slice`: MODE — compile-time конфигурация
    /// (dir + pair + доноры), без runtime-диспетчеризации в hot-loop.
    #[inline(always)]
    #[cfg_attr(coverage, coverage(off))]
    fn alloc_uninit_slice_m<T, const ALIGN: usize, const MODE: u32>(&self, count: usize) -> *mut T {
        if count == 0 {
            return ptr::null_mut();
        }
        let size = count * core::mem::size_of::<T>();
        self.alloc_raw_m::<ALIGN, MODE>(size) as *mut T
    }

    #[hotpath::measure]
    fn reset(&self) {
        self.lo.store(0, Ordering::Relaxed);
        self.hi.store(0, Ordering::Relaxed);
        self.toggle.set(false);
    }

    /// First-touch: фолтим страницы ИСПОЛЬЗОВАННЫХ областей из текущего
    /// потока, чтобы они легли в локальную NUMA-ноду.
    #[hotpath::measure]
    #[cfg_attr(coverage, coverage(off))]
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
    #[cfg_attr(coverage, coverage(off))]
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

    // ============================================================
    //          ARENA VEC / ARENA STRING (ВСЁ ИЗ ОДНОГО БУФЕРА)
    // ============================================================
    #[allow(dead_code)]
    fn child_bump(
        ptr: *mut u8,
        len: usize,
        dir: BumpDir,
        is_huge: bool,
        base_chunk: usize,
    ) -> ThreadBump {
        ThreadBump {
            ptr,
            len,
            lo: AtomicUsize::new(0),
            hi: AtomicUsize::new(0),
            toggle: Cell::new(false),
            dir,
            is_huge,
            neighbor_idx: None,
            array: ptr::null(),
            #[cfg(test)]
            can_give: false,
            self_index: 0,
            donor_array: ptr::null(),
            donor_reg: DonorReg::None,
            donor_static_ptr: None,
            donor_static_len: 0,
            donor_bump: None,
            use_priority: false,
            base_chunk,
            fallback: SpinMutex::new(FallbackChunks {
                chunks: Vec::new(),
                used: 0,
            }),
        }
    }

    /// Конструктор дочернего bump'а в режиме доноров (общий динамический реестр).
    /// `donor_array` выставляется вызвающим после сборки массива (иначе адрес
    /// нестабилен из-за возможных переаллокаций `Vec`).
    #[allow(dead_code)]
    fn child_bump_donor(
        ptr: *mut u8,
        len: usize,
        dir: BumpDir,
        is_huge: bool,
        base_chunk: usize,
        self_index: usize,
        use_priority: bool,
        donor_reg: DonorReg,
    ) -> ThreadBump {
        ThreadBump {
            ptr,
            len,
            lo: AtomicUsize::new(0),
            hi: AtomicUsize::new(0),
            toggle: Cell::new(false),
            dir,
            is_huge,
            neighbor_idx: None,
            array: ptr::null(),
            #[cfg(test)]
            can_give: true,
            self_index,
            donor_array: ptr::null(),
            donor_reg,
            donor_static_ptr: None,
            donor_static_len: 0,
            donor_bump: None,
            use_priority,
            base_chunk,
            fallback: SpinMutex::new(FallbackChunks {
                chunks: Vec::new(),
                used: 0,
            }),
        }
    }

    /// Разделить регион этого bump'а на `n` непересекающихся Send-под-bump'ов
    /// (направление заполнения у всех — `dir`). Частичный остаток от
    /// деления адресуется выровненным размером чанка по 16 (см. `split_with`).
    #[allow(dead_code)]
    pub fn split(&self, n: usize, dir: BumpDir) -> Vec<CachePadded<ThreadBump>> {
        let base = self.ptr;
        let total = self.len;
        let chunk_size = (total / n).next_multiple_of(16);
        (0..n)
            .map(|i| {
                let start = i * chunk_size;
                let end = if i == n - 1 {
                    total
                } else {
                    start + chunk_size
                };
                CachePadded::new(Self::child_bump(
                    unsafe { base.add(start) },
                    end - start,
                    dir,
                    self.is_huge,
                    self.base_chunk,
                ))
            })
            .collect()
    }

    /// Разбиение региона по заданным размерам (в байтах); остаток уходит
    /// последней зоне. Каждый размер округляется вверх до
    /// 16, чтобы старты чанков оставались выровненными (Tree Borrows).
    #[allow(dead_code)]
    pub fn split_by_sizes(&self, sizes: &[usize], dir: BumpDir) -> Vec<CachePadded<ThreadBump>> {
        assert!(sizes.len() > 0, "split_by_sizes: пустой список");
        let base = self.ptr;
        let total = self.len;
        let mut children = Vec::with_capacity(sizes.len());
        let mut start = 0usize;
        let last = sizes.len() - 1;
        for (i, &sz) in sizes.iter().enumerate() {
            let step = sz.next_multiple_of(16);
            let end = if i == last { total } else { start + step };
            children.push(CachePadded::new(Self::child_bump(
                unsafe { base.add(start) },
                end - start,
                dir,
                self.is_huge,
                self.base_chunk,
            )));
            start = end;
        }
        children
    }

    /// Как `split`, но каждый дочерний bump становится донором в общем
    /// динамическом (orx) реестре и регистрирует остальных — т.е. при
    /// переполнении заём идёт у «соседей» доноров, а не сразу в fallback.
    #[allow(dead_code)]
    pub fn split_donors(&self, n: usize, dir: BumpDir) -> Vec<CachePadded<ThreadBump>> {
        let base = self.ptr;
        let total = self.len;
        let chunk_size = (total / n).next_multiple_of(16);
        let shared: Arc<OrxVec<Donor>> = Arc::new(OrxVec::new());
        let mut children: Vec<CachePadded<ThreadBump>> = Vec::with_capacity(n);
        for i in 0..n {
            let start = i * chunk_size;
            let end = if i == n - 1 {
                total
            } else {
                start + chunk_size
            };
            children.push(CachePadded::new(Self::child_bump_donor(
                unsafe { base.add(start) },
                end - start,
                dir,
                self.is_huge,
                self.base_chunk,
                i,
                true,
                DonorReg::Orx(shared.clone()),
            )));
        }
        // Фиксируем адрес массива (стабилен после сборки) и регистрируем все
        // как доноров друг для друга (через публичный `add_donor`).
        let arr = children.as_ptr();
        for (i, c) in children.iter_mut().enumerate() {
            c.donor_array = arr;
            c.add_donor(i, 0);
        }
        children
    }

    /// Как `split_by_sizes`, но с донорской связкой (см. `split_donors`).
    #[allow(dead_code)]
    pub fn split_donors_by_sizes(
        &self,
        sizes: &[usize],
        dir: BumpDir,
    ) -> Vec<CachePadded<ThreadBump>> {
        assert!(sizes.len() > 0, "split_donors_by_sizes: пустой список");
        let base = self.ptr;
        let total = self.len;
        let shared: Arc<OrxVec<Donor>> = Arc::new(OrxVec::new());
        let mut children: Vec<CachePadded<ThreadBump>> = Vec::with_capacity(sizes.len());
        let mut start = 0usize;
        let last = sizes.len() - 1;
        for (i, &sz) in sizes.iter().enumerate() {
            let step = sz.next_multiple_of(16);
            let end = if i == last { total } else { start + step };
            children.push(CachePadded::new(Self::child_bump_donor(
                unsafe { base.add(start) },
                end - start,
                dir,
                self.is_huge,
                self.base_chunk,
                i,
                true,
                DonorReg::Orx(shared.clone()),
            )));
            start = end;
        }
        let arr = children.as_ptr();
        for (i, c) in children.iter_mut().enumerate() {
            c.donor_array = arr;
            c.add_donor(i, 0);
        }
        children
    }

    /// Р В Р В°Р В·Р Т‘Р ВµР В»Р С‘РЎвЂљРЎРЉ РЎР‚Р ВµР С–Р С‘Р С•Р Р… Р Р…Р В° `n` Р Р…Р ВµР С—Р ВµРЎР‚Р ВµРЎРѓР ВµР С”Р В°РЎР‹РЎвЂ°Р С‘РЎвЂ¦РЎРѓРЎРЏ Р СњР вЂў-Send Р В·Р С•Р Р… (`ZoneBump`).
    #[allow(dead_code)]
    pub fn split_local(&self, n: usize, dir: BumpDir) -> Vec<ZoneBump<'static>> {
        let base = self.ptr;
        let total = self.len;
        let chunk_size = (total / n).next_multiple_of(16);
        (0..n)
            .map(|i| {
                let start = i * chunk_size;
                let end = if i == n - 1 {
                    total
                } else {
                    start + chunk_size
                };
                Self::make_zone(
                    unsafe { base.add(start) },
                    end - start,
                    dir,
                    self.is_huge,
                    self.base_chunk,
                )
            })
            .collect()
    }

    /// Как `split_local`, но по заданным размерам.
    #[allow(dead_code)]
    pub fn split_local_by_sizes(&self, sizes: &[usize], dir: BumpDir) -> Vec<ZoneBump<'static>> {
        assert!(sizes.len() > 0, "split_local_by_sizes: пустой список");
        let base = self.ptr;
        let total = self.len;
        let mut zones = Vec::with_capacity(sizes.len());
        let mut start = 0usize;
        let last = sizes.len() - 1;
        for (i, &sz) in sizes.iter().enumerate() {
            let step = sz.next_multiple_of(16);
            let end = if i == last { total } else { start + step };
            zones.push(Self::make_zone(
                unsafe { base.add(start) },
                end - start,
                dir,
                self.is_huge,
                self.base_chunk,
            ));
            start = end;
        }
        zones
    }

    /// Покликнуть этот bump как `ZoneBump` (однопоточный владелец). Требует,
    /// чтобы bump был изолирован (без соседей и доноров). Fallback-чанки
    /// переносятся из bump'а в зону (их освобождение — в `Drop` зоны).
    #[allow(dead_code)]
    pub fn into_zone(self) -> ZoneBump<'static> {
        assert!(
            self.neighbor_idx.is_none(),
            "into_zone: bump в режиме пары нельзя конвертировать"
        );
        assert!(
            matches!(self.donor_reg, DonorReg::None),
            "into_zone: bump в режиме доноров нельзя конвертировать"
        );
        let ptr = self.ptr;
        let len = self.len;
        let dir = self.dir;
        let is_huge = self.is_huge;
        let base_chunk = self.base_chunk;
        let lo = self.lo.load(Ordering::Relaxed);
        let hi = self.hi.load(Ordering::Relaxed);
        let toggle = self.toggle.get();
        let fallback = unsafe {
            // Вытаскиваем fallback из типа до Drop (иначе он сам бы его освободил).
            let me = std::mem::ManuallyDrop::new(self);
            core::ptr::read(&me.fallback)
        };
        let zone = ZoneData {
            ptr,
            len,
            lo: Cell::new(lo),
            hi: Cell::new(hi),
            toggle: Cell::new(toggle),
            dir,
            is_huge,
            base_chunk,
            fallback,
        };
        ZoneBump {
            inner: Rc::new(zone),
            _marker: PhantomData,
        }
    }

    /// Для `ZoneBump`: собрать зону из существующего региона (используется
    /// `split_local*`). Каждая такая зона — ассоциированная функция ThreadBump, но
    /// делегирует создание зоны.
    #[allow(dead_code)]
    fn make_zone(
        ptr: *mut u8,
        len: usize,
        dir: BumpDir,
        is_huge: bool,
        base_chunk: usize,
    ) -> ZoneBump<'static> {
        ZoneBump::from_region(ptr, len, dir, is_huge, base_chunk)
    }

    /// Обратная операция `into_zone`: собирает `ThreadBump` из `ZoneData` (используется
    /// `ZoneBump::into_thread`). Счётчики и fallback переносятся из зоны.
    #[allow(dead_code)]
    fn restore_from_zone(z: ZoneData) -> ThreadBump {
        let z = std::mem::ManuallyDrop::new(z);
        let fallback = unsafe { core::ptr::read(&z.fallback) };
        ThreadBump {
            ptr: z.ptr,
            len: z.len,
            lo: AtomicUsize::new(z.lo.get()),
            hi: AtomicUsize::new(z.hi.get()),
            toggle: Cell::new(z.toggle.get()),
            dir: z.dir,
            is_huge: z.is_huge,
            neighbor_idx: None,
            array: ptr::null(),
            #[cfg(test)]
            can_give: false,
            self_index: 0,
            donor_array: ptr::null(),
            donor_reg: DonorReg::None,
            donor_static_ptr: None,
            donor_static_len: 0,
            donor_bump: None,
            use_priority: false,
            base_chunk: z.base_chunk,
            fallback,
        }
    }
}

// ============================================================
//       ZONE BUMP (ОДНОПОТОЧНЫЙ, НЕ-Send В ОТЛИЧИЕ ОТ THREAD BUMP)
// ============================================================
// `ZoneBump` — это «зона» из региона арены, которой владеет ровно один поток.
// Внутренность живёт в `Rc<ZoneData>`, поэтому тип по своей природе `!Send` и
// `!Sync` (в отличие от `ThreadBump`, где `unsafe impl Send`). Счётчики — в
// `Cell`: никакого атомарного доступа нет, всё локальное. Отличие от
// `ThreadBump` тем, что не участвует в парах/донорах и отдаёт регион обратно
// в виде `ThreadBump` через `into_thread`.

struct ZoneData {
    ptr: *mut u8,
    len: usize,
    lo: Cell<usize>,
    hi: Cell<usize>,
    toggle: Cell<bool>,
    dir: BumpDir,
    is_huge: bool,
    base_chunk: usize,
    fallback: SpinMutex<FallbackChunks>,
}

impl Drop for ZoneData {
    fn drop(&mut self) {
        // Регион арены освобождает владелец; здесь — только fallback-чанки.
        let mut fb = self.fallback.lock();
        for a in fb.chunks.drain(..) {
            platform::free(a);
        }
    }
}

pub struct ZoneBump<'a> {
    inner: Rc<ZoneData>,
    // `&'a ()` — носитель только для срока жизни; `Rc` уже делает тип !Send/!Sync.
    _marker: PhantomData<&'a ()>,
}

impl ZoneBump<'static> {
    /// Собрать зону из существующего (заранее выровненного) региона.
    #[allow(dead_code)]
    fn from_region(
        ptr: *mut u8,
        len: usize,
        dir: BumpDir,
        is_huge: bool,
        base_chunk: usize,
    ) -> ZoneBump<'static> {
        ZoneBump {
            inner: Rc::new(ZoneData {
                ptr,
                len,
                lo: Cell::new(0),
                hi: Cell::new(0),
                toggle: Cell::new(false),
                dir,
                is_huge,
                base_chunk,
                fallback: SpinMutex::new(FallbackChunks {
                    chunks: Vec::new(),
                    used: 0,
                }),
            }),
            _marker: PhantomData,
        }
    }

    /// Направление заполнения этой зоны.
    #[allow(dead_code)]
    pub fn dir(&self) -> BumpDir {
        self.inner.dir
    }

    /// Байт, выделенный из собственного региона зоны (без fallback).
    #[allow(dead_code)]
    pub fn allocated_bytes(&self) -> usize {
        self.inner.lo.get() + self.inner.hi.get()
    }

    /// Обнулить счётчики зоны (регион переиспользуется с нуля). Не трогает
    /// уже выделенные fallback-чанки.
    #[allow(dead_code)]
    pub fn reset(&self) {
        self.inner.lo.set(0);
        self.inner.hi.set(0);
        self.inner.toggle.set(false);
    }

    /// Одношаговая bump-аллокация из региона зоны. При исчерпании региона
    /// выделяется новый чанк «где-то в памяти» (как `grow_fallback`).
    #[allow(dead_code)]
    #[cfg_attr(coverage, coverage(off))]
    pub fn alloc<const ALIGN: usize>(&self, size: usize) -> *mut u8 {
        let d = &self.inner;
        let (off, new_lo, new_hi, pf) = match d.dir {
            BumpDir::Forward => {
                let cur = d.lo.get();
                let off = (cur + ALIGN - 1) & !(ALIGN - 1);
                let end = off + size;
                if end <= d.len {
                    let pf = unsafe { d.ptr.add(off + size) };
                    (off, end, d.hi.get(), pf)
                } else {
                    return self.grow_fallback(size);
                }
            }
            BumpDir::Backward => {
                let cur = d.hi.get();
                let off = (d.len - cur - size) & !(ALIGN - 1);
                if off >= size {
                    let new_hi = d.len - off;
                    let pf = unsafe { d.ptr.add(off - size) };
                    (off, d.lo.get(), new_hi, pf)
                } else {
                    return self.grow_fallback(size);
                }
            }
            BumpDir::MiddleOut => {
                let mid = d.len / 2;
                let side = d.toggle.get();
                d.toggle.set(!side);
                if !side {
                    let base = mid - d.lo.get();
                    if size <= base {
                        let off = (base - size) & !(ALIGN - 1);
                        let new_lo = mid - off;
                        let pf = unsafe { d.ptr.add(mid + d.hi.get() + size) };
                        (off, new_lo, d.hi.get(), pf)
                    } else {
                        return self.grow_fallback(size);
                    }
                } else {
                    let base = mid + d.hi.get();
                    let off = (base + ALIGN - 1) & !(ALIGN - 1);
                    let end = off + size;
                    if end <= d.len {
                        let new_hi = off + size - mid;
                        let pf = unsafe { d.ptr.add(mid - d.lo.get() - size) };
                        (off, d.lo.get(), new_hi, pf)
                    } else {
                        return self.grow_fallback(size);
                    }
                }
            }
        };
        d.lo.set(new_lo);
        d.hi.set(new_hi);
        let p = unsafe { d.ptr.add(off) };
        prefetch_write(pf);
        p
    }

    /// Выделить блок из нового чанка «где-то в памяти» (аналог
    /// `ThreadBump::grow_fallback`); чанк сохраняется в зоне и освобождается в
    /// её `Drop`.
    fn grow_fallback(&self, size: usize) -> *mut u8 {
        let d = &self.inner;
        let page = platform::page_size();
        let need = (size + 15) & !15;
        let mut fb = d.fallback.lock();
        let full = match fb.chunks.last() {
            Some(c) => fb.used + need > c.size,
            None => true,
        };
        if full {
            let alloc_size = need.max(d.base_chunk).next_multiple_of(page);
            fb.chunks.push(platform::alloc_normal(alloc_size));
            fb.used = 0;
        }
        let chunk = fb.chunks.last().unwrap();
        let off = (fb.used + 15) & !15;
        let ptr = unsafe { chunk.ptr.add(off) };
        prefetch_write(unsafe { chunk.ptr.add(off + size) });
        fb.used = off + size;
        ptr
    }

    /// Разбиение региона зоны на `n` непересекающихся суб-зон (все `!Send`).
    #[allow(dead_code)]
    pub fn split(&self, n: usize) -> Vec<ZoneBump<'static>> {
        let d = &self.inner;
        let total = d.len;
        let chunk_size = (total / n).next_multiple_of(16);
        (0..n)
            .map(|i| {
                let start = i * chunk_size;
                let end = if i == n - 1 {
                    total
                } else {
                    start + chunk_size
                };
                ZoneBump::from_region(
                    unsafe { d.ptr.add(start) },
                    end - start,
                    d.dir,
                    d.is_huge,
                    d.base_chunk,
                )
            })
            .collect()
    }

    /// Разбиение региона зоны по заданным размерам (остаток уходит последней зоне).
    #[allow(dead_code)]
    pub fn split_by_sizes(&self, sizes: &[usize]) -> Vec<ZoneBump<'static>> {
        assert!(sizes.len() > 0, "split_by_sizes: пустой список");
        let d = &self.inner;
        let total = d.len;
        let mut zones = Vec::with_capacity(sizes.len());
        let mut start = 0usize;
        let last = sizes.len() - 1;
        for (i, &sz) in sizes.iter().enumerate() {
            let step = sz.next_multiple_of(16);
            let end = if i == last { total } else { start + step };
            zones.push(ZoneBump::from_region(
                unsafe { d.ptr.add(start) },
                end - start,
                d.dir,
                d.is_huge,
                d.base_chunk,
            ));
            start = end;
        }
        zones
    }

    /// Разделить регион зоны на `n` Send-`ThreadBump` (полностью отдаёт потокам).
    #[allow(private_interfaces)]
    pub fn split_into_threads(&self, n: usize) -> Vec<CachePadded<ThreadBump>> {
        let d = &self.inner;
        let total = d.len;
        let chunk_size = (total / n).next_multiple_of(16);
        (0..n)
            .map(|i| {
                let start = i * chunk_size;
                let end = if i == n - 1 {
                    total
                } else {
                    start + chunk_size
                };
                CachePadded::new(ThreadBump::child_bump(
                    unsafe { d.ptr.add(start) },
                    end - start,
                    d.dir,
                    d.is_huge,
                    d.base_chunk,
                ))
            })
            .collect()
    }

    /// Разделить регион зоны на Send-`ThreadBump` по заданным размерам.
    #[allow(private_interfaces)]
    pub fn split_into_threads_by_sizes(&self, sizes: &[usize]) -> Vec<CachePadded<ThreadBump>> {
        assert!(
            sizes.len() > 0,
            "split_into_threads_by_sizes: пустой список"
        );
        let d = &self.inner;
        let total = d.len;
        let mut out = Vec::with_capacity(sizes.len());
        let mut start = 0usize;
        let last = sizes.len() - 1;
        for (i, &sz) in sizes.iter().enumerate() {
            let step = sz.next_multiple_of(16);
            let end = if i == last { total } else { start + step };
            out.push(CachePadded::new(ThreadBump::child_bump(
                unsafe { d.ptr.add(start) },
                end - start,
                d.dir,
                d.is_huge,
                d.base_chunk,
            )));
            start = end;
        }
        out
    }

    /// Превратить зону в `ThreadBump`. Паникует, если у зоны есть другие живы
    /// ссылки (`Rc::try_unwrap` не удалась) — т.е. если на ту же зону
    /// ссылались ещё какие-то `ZoneBump`.
    #[allow(private_interfaces)]
    pub fn into_thread(self) -> CachePadded<ThreadBump> {
        let data = Rc::try_unwrap(self.inner)
            .unwrap_or_else(|_| panic!("into_thread: РЎС“ Р В·Р С•Р Р…РЎвЂ№ Р ВµРЎРѓРЎвЂљРЎРЉ Р В¶Р С‘Р Р†РЎвЂ№Р Вµ Р С”Р В»Р С•Р Р…РЎвЂ№"));
        CachePadded::new(ThreadBump::restore_from_zone(data))
    }
}

// ============================================================
//          ARENA VEC / ARENA STRING (ПОСЛЕДНИЙ РАЗДЕЛ)
// ============================================================

struct ArenaVec<T> {
    ptr: *mut T,
    len: usize,
    cap: usize,
}

impl<T> ArenaVec<T> {
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

    // ===== Мономорфные версии (конфигурируются compile-time `MODE`) =====
    #[inline(always)]
    #[cfg_attr(coverage, coverage(off))]
    fn with_capacity_in_m<const ALIGN: usize, const MODE: u32>(
        capacity: usize,
        bump: &ThreadBump,
    ) -> Self {
        if capacity == 0 {
            return Self {
                ptr: ptr::null_mut(),
                len: 0,
                cap: 0,
            };
        }
        let ptr = bump.alloc_uninit_slice_m::<T, ALIGN, MODE>(capacity);
        Self {
            ptr,
            len: 0,
            cap: capacity,
        }
    }

    #[inline(always)]
    #[cfg_attr(coverage, coverage(off))]
    fn push_m<const ALIGN: usize, const MODE: u32>(&mut self, value: T, bump: &ThreadBump) {
        if self.len == self.cap {
            self.grow_m::<ALIGN, MODE>(bump);
        }
        unsafe {
            self.ptr.add(self.len).write(value);
        }
        self.len += 1;
    }

    #[cfg_attr(coverage, coverage(off))]
    fn from_slice_in_m<const ALIGN: usize, const MODE: u32>(slice: &[T], bump: &ThreadBump) -> Self
    where
        T: Copy,
    {
        let len = slice.len();
        let ptr = bump.alloc_uninit_slice_m::<T, ALIGN, MODE>(len);
        unsafe {
            ptr::copy_nonoverlapping(slice.as_ptr(), ptr, len);
        }
        Self { ptr, len, cap: len }
    }

    #[cfg_attr(coverage, coverage(off))]
    fn grow_m<const ALIGN: usize, const MODE: u32>(&mut self, bump: &ThreadBump) {
        let new_cap = if self.cap == 0 { 4 } else { self.cap * 2 };
        let new_ptr = bump.alloc_uninit_slice_m::<T, ALIGN, MODE>(new_cap);
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

    /// Мономорфная версия `from_str_in` (compile-time `MODE`).
    #[inline(always)]
    #[cfg_attr(coverage, coverage(off))]
    fn from_str_in_m<const MODE: u32>(s: &str, bump: &ThreadBump) -> Self {
        let vec = ArenaVec::from_slice_in_m::<1, MODE>(s.as_bytes(), bump);
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
//     Typed shared arena (Arena<T>) — см. src/typed_arena.rs
// ============================================================
mod typed_arena;
pub use typed_arena::Arena;

// ============================================================
//               ОСНОВНЫЕ БЕНЧМАРК-ФУНКЦИИ
// ============================================================

use bumpalo::Bump;
use bumpalo::collections::String as BumpString;
use bumpalo::collections::Vec as BumpVec;
use mimalloc::MiMalloc;
use spin::Mutex as SpinMutex;

// Под Miri mimalloc (FFI-аллокатор) не поддерживается — используем системный.
#[cfg(not(miri))]
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

// --- MiMalloc ---
#[cfg_attr(coverage, coverage(off))]
pub fn mimm(smt: bool) {
    let core_ids = get_cores(smt);
    let s = bench_scale();
    std::thread::scope(|s2| {
        for core_id in core_ids.iter() {
            s2.spawn(move || {
                core_affinity::set_for_current(*core_id);
                for _ in 0..(3 / s).max(1) {
                    let mut vectr: Vec<Vec<String>> = Vec::with_capacity(40000 / s);
                    for _ in 0..(200 / s).max(1) {
                        for _ in 0..(200 / s).max(1) {
                            let mut vec = Vec::with_capacity(400);
                            for _ in 0..(100 / s).max(1) {
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

#[cfg_attr(coverage, coverage(off))]
pub fn mimm_light(smt: bool) {
    let core_ids = get_cores(smt);
    let s = bench_scale();
    std::thread::scope(|s2| {
        for core_id in core_ids.iter() {
            s2.spawn(move || {
                core_affinity::set_for_current(*core_id);
                for _ in 0..(3 / s).max(1) {
                    let mut vectr: Vec<Vec<String>> = Vec::with_capacity(10000 / s);
                    for _ in 0..(100 / s).max(1) {
                        for _ in 0..(100 / s).max(1) {
                            let mut vec = Vec::with_capacity(400);
                            for _ in 0..(100 / s).max(1) {
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
#[cfg_attr(coverage, coverage(off))]
pub fn bump_scope_m(chunk_size: usize, smt: bool) {
    let core_ids = get_cores(smt);
    let s = bench_scale();
    std::thread::scope(|s2| {
        for core_id in core_ids.iter() {
            s2.spawn(move || {
                core_affinity::set_for_current(*core_id);
                let mut bump = Bump::with_capacity(chunk_size);
                for _ in 0..(3 / s).max(1) {
                    let capacity = 4 * 100 * (100 / s).max(1);
                    let mut vectr = BumpVec::with_capacity_in(capacity, &bump);
                    for _ in 0..(200 / s).max(1) {
                        for _ in 0..(200 / s).max(1) {
                            let mut vec = BumpVec::with_capacity_in(400, &bump);
                            for _ in 0..(100 / s).max(1) {
                                vec.push(BumpString::from_str_in("stroka", &bump));
                            }
                            vectr.push(vec);
                        }
                    }
                    core::hint::black_box(&vectr);
                    drop(vectr); // <-- сброс vectr
                    bump.reset(); // теперь можно сбросить
                }
            });
        }
    });
}

#[cfg_attr(coverage, coverage(off))]
pub fn bump_scope_m_light(chunk_size: usize, smt: bool) {
    let core_ids = get_cores(smt);
    let s = bench_scale();
    std::thread::scope(|s2| {
        for core_id in core_ids.iter() {
            s2.spawn(move || {
                core_affinity::set_for_current(*core_id);
                let mut bump = Bump::with_capacity(chunk_size);
                for _ in 0..(3 / s).max(1) {
                    let capacity = 100 * (100 / s).max(1);
                    let mut vectr = BumpVec::with_capacity_in(capacity, &bump);
                    for _ in 0..(100 / s).max(1) {
                        for _ in 0..(100 / s).max(1) {
                            let mut vec = BumpVec::with_capacity_in(400, &bump);
                            for _ in 0..(100 / s).max(1) {
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
#[cfg_attr(coverage, coverage(off))]
fn do_work_full(bump: &Bump) {
    let s = bench_scale();
    let capacity = 4 * 100 * (100 / s).max(1);
    let mut vectr = BumpVec::with_capacity_in(capacity, bump);
    for _ in 0..(200 / s).max(1) {
        for _ in 0..(200 / s).max(1) {
            let mut vec = BumpVec::with_capacity_in(400, bump);
            for _ in 0..(100 / s).max(1) {
                vec.push(BumpString::from_str_in("stroka", bump));
            }
            vectr.push(vec);
        }
    }
    core::hint::black_box(&vectr);
}

#[cfg_attr(coverage, coverage(off))]
fn do_work_light(bump: &Bump) {
    let s = bench_scale();
    let capacity = 100 * (100 / s).max(1);
    let mut vectr = BumpVec::with_capacity_in(capacity, bump);
    for _ in 0..(100 / s).max(1) {
        for _ in 0..(100 / s).max(1) {
            let mut vec = BumpVec::with_capacity_in(400, bump);
            for _ in 0..(100 / s).max(1) {
                vec.push(BumpString::from_str_in("stroka", bump));
            }
            vectr.push(vec);
        }
    }
    core::hint::black_box(&vectr);
}

#[cfg_attr(coverage, coverage(off))]
pub fn bump_shared_m(chunk_size: usize, smt: bool) {
    let core_ids = get_cores(smt);
    let s = bench_scale();
    let total = chunk_size * core_ids.len() * 2;
    let shared = Arc::new(SpinMutex::new(Bump::with_capacity(total)));

    std::thread::scope(|sc| {
        for core_id in core_ids.iter() {
            let shared = Arc::clone(&shared);
            sc.spawn(move || {
                core_affinity::set_for_current(*core_id);
                for _ in 0..(3 / s).max(1) {
                    let mut guard = shared.lock();
                    do_work_full(&guard);
                    guard.reset();
                }
            });
        }
    });
}

#[cfg_attr(coverage, coverage(off))]
pub fn bump_shared_m_light(chunk_size: usize, smt: bool) {
    let core_ids = get_cores(smt);
    let s = bench_scale();
    let total = chunk_size * core_ids.len() * 2;
    let shared = Arc::new(SpinMutex::new(Bump::with_capacity(total)));

    std::thread::scope(|sc| {
        for core_id in core_ids.iter() {
            let shared = Arc::clone(&shared);
            sc.spawn(move || {
                core_affinity::set_for_current(*core_id);
                for _ in 0..(3 / s).max(1) {
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
    /// Доноры: статичный список, без приоритета (см. `split_donors_with`).
    Donors,
    /// Доноры: статичный список + приоритет (низкоприоритетные берутся первыми).
    DonorsPrio,
    /// Доноры: динамический список на orx-concurrent-vec, без приоритета.
    DonorsOrx,
    /// Доноры: динамический список на orx-concurrent-vec + приоритет.
    DonorsOrxPrio,
    /// Доноры: динамический список на boxcar, без приоритета.
    DonorsBoxcar,
    /// Доноры: динамический список на boxcar + приоритет.
    DonorsBoxcarPrio,
}

/// Единое тело бенчмарка shared-арены. `full` управляет объёмом работы
/// (FULL: 200×200×100, LIGHT: 100×100×100), `layout` — направлением заполнения.
#[cfg_attr(coverage, coverage(off))]
fn arena_bench(chunk_size: usize, smt: bool, full: bool, layout: ArenaLayout) {
    let core_ids = get_cores(smt);
    let total_capacity = chunk_size * core_ids.len();
    if verbose_enabled() {
        println!("[TOTAL CAPACITY]:  {}", total_capacity);
    }
    let arena = SharedArena::new(total_capacity);
    let bumps: Split<'_> = match layout {
        ArenaLayout::Uniform(dir) => arena.split_with_safe(core_ids.len(), dir),
        ArenaLayout::Neighbors => arena.split_alternating_safe(core_ids.len()),
        ArenaLayout::Pair => arena.split_paired_safe(core_ids.len()),
        ArenaLayout::Donors => {
            arena.split_donors_with_safe(core_ids.len(), DonorPolicy::static_(4))
        }
        ArenaLayout::DonorsPrio => {
            arena.split_donors_with_safe(core_ids.len(), DonorPolicy::static_(4).with_priority())
        }
        ArenaLayout::DonorsOrx => arena.split_donors_with_safe(core_ids.len(), DonorPolicy::orx(4)),
        ArenaLayout::DonorsOrxPrio => {
            arena.split_donors_with_safe(core_ids.len(), DonorPolicy::orx(4).with_priority())
        }
        ArenaLayout::DonorsBoxcar => {
            arena.split_donors_with_safe(core_ids.len(), DonorPolicy::boxcar(4))
        }
        ArenaLayout::DonorsBoxcarPrio => {
            arena.split_donors_with_safe(core_ids.len(), DonorPolicy::boxcar(4).with_priority())
        }
    };

    let (vcap, outer, inner) = if full {
        (40000, 200, 200)
    } else {
        (10000, 100, 100)
    };
    let scale = bench_scale();

    std::thread::scope(|sc| {
        for (core_id, i) in core_ids.iter().zip(0..bumps.len()) {
            // `&bumps[i]` — разделяемая ссылка в никогда не переезжающий массив.
            // `CachePadded<ThreadBump>: Sync` (см. `unsafe impl Sync`), поэтому
            // сама ссылка `Send` и её можно отдать потоку. Каждый чанк
            // использует ровно один поток; заём у соседа идёт только через
            // atomic-счётчики (см. `try_borrow`).
            let core = *core_id;
            let bump = &bumps[i];
            sc.spawn(move || {
                core_affinity::set_for_current(core);
                hotpath::measure_block!("prefault", {
                    bump.prefault_local(); // first-touch Р Р† Р В»Р С•Р С”Р В°Р В»РЎРЉР Р…Р С•Р в„– NUMA-Р Р…Р С•Р Т‘Р Вµ
                });
                for _ in 0..(3 / scale).max(1) {
                    hotpath::measure_block!("alloc", {
                        let mut vectr: ArenaVec<ArenaVec<ArenaString>> =
                            ArenaVec::with_capacity_in::<
                                { core::mem::align_of::<ArenaVec<ArenaString>>() },
                            >(vcap / scale, bump);
                        for _ in 0..(outer / scale).max(1) {
                            for _ in 0..(inner / scale).max(1) {
                                let mut vec: ArenaVec<ArenaString> =
                                    ArenaVec::with_capacity_in::<
                                        { core::mem::align_of::<ArenaString>() },
                                    >(400, bump);
                                for _ in 0..(100 / scale).max(1) {
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

/// Полностью мономорфная версия `arena_bench`: конфигурация (направление,
/// пара/доноры) задана `const MODE`, поэтому ни `match layout`, ни
/// runtime-диспетчеризация в hot-loop нет. Старый `arena_bench` остаётся для
/// общих/динамических путей.
#[inline(always)]
#[cfg_attr(coverage, coverage(off))]
fn arena_run_thread<const MODE: u32, const FULL: bool>(bump: &ThreadBump) {
    hotpath::measure_block!("prefault", {
        bump.prefault_local(); // first-touch Р Р† Р В»Р С•Р С”Р В°Р В»РЎРЉР Р…Р С•Р в„– NUMA-Р Р…Р С•Р Т‘Р Вµ
    });
    let (vcap, outer, inner) = if FULL {
        (40000, 200, 200)
    } else {
        (10000, 100, 100)
    };
    let scale = bench_scale();
    for _ in 0..(3 / scale).max(1) {
        hotpath::measure_block!("alloc", {
            let mut vectr: ArenaVec<ArenaVec<ArenaString>> = ArenaVec::with_capacity_in_m::<
                { core::mem::align_of::<ArenaVec<ArenaString>>() },
                MODE,
            >(vcap / scale, bump);
            for _ in 0..(outer / scale).max(1) {
                for _ in 0..(inner / scale).max(1) {
                    let mut vec: ArenaVec<ArenaString> = ArenaVec::with_capacity_in_m::<
                        { core::mem::align_of::<ArenaString>() },
                        MODE,
                    >(400, bump);
                    for _ in 0..(100 / scale).max(1) {
                        vec.push_m::<{ core::mem::align_of::<ArenaString>() }, MODE>(
                            ArenaString::from_str_in_m::<MODE>("stroka", bump),
                            bump,
                        );
                    }
                    vectr.push_m::<{ core::mem::align_of::<ArenaVec<ArenaString>>() }, MODE>(
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
}

#[cfg_attr(coverage, coverage(off))]
fn arena_bench_impl<const MODE: u32, const FULL: bool>(chunk_size: usize, smt: bool) {
    let core_ids = get_cores(smt);
    let total_capacity = chunk_size * core_ids.len();
    let arena = SharedArena::new(total_capacity);
    let bumps = make_split::<MODE>(&arena, core_ids.len());
    std::thread::scope(|s| {
        for (core_id, i) in core_ids.iter().zip(0..bumps.len()) {
            let core = *core_id;
            let bump = &bumps[i];
            s.spawn(move || {
                core_affinity::set_for_current(core);
                arena_run_thread::<MODE, FULL>(bump);
            });
        }
    });
}

#[cfg_attr(coverage, coverage(off))]
fn make_split<'a, const MODE: u32>(arena: &'a SharedArena, n: usize) -> Split<'a> {
    let pair = MODE & ThreadBump::MODE_PAIR != 0;
    let donorkind = (MODE & ThreadBump::MODE_DONOR_MASK) >> 3;
    let prio = MODE & ThreadBump::MODE_PRIO != 0;
    let dir = match MODE & ThreadBump::MODE_DIR_MASK {
        ThreadBump::MODE_DIR_BACKWARD => BumpDir::Backward,
        ThreadBump::MODE_DIR_MIDDLEOUT => BumpDir::MiddleOut,
        _ => BumpDir::Forward,
    };
    if pair {
        arena.split_paired_safe(n)
    } else if donorkind != 0 {
        let policy = match (donorkind, prio) {
            (1, false) => DonorPolicy::static_(4),
            (1, true) => DonorPolicy::static_(4).with_priority(),
            (2, false) => DonorPolicy::orx(4),
            (2, true) => DonorPolicy::orx(4).with_priority(),
            (3, false) => DonorPolicy::boxcar(4),
            (3, true) => DonorPolicy::boxcar(4).with_priority(),
            _ => unreachable!(),
        };
        arena.split_donors_with_safe(n, policy)
    } else {
        arena.split_with_safe(n, dir)
    }
}

// ===== Готовые значения MODE для мономорфных бенч-версий по всем типам shared =====
// (Направление + пара + вид доноров/приоритет задаются compile-time.)
const MODE_FWD: u32 = ThreadBump::MODE_DIR_FORWARD;
const MODE_BWD: u32 = ThreadBump::MODE_DIR_BACKWARD;
const MODE_MO: u32 = ThreadBump::MODE_DIR_MIDDLEOUT;

const MODE_PAIR_FWD: u32 = ThreadBump::MODE_PAIR | ThreadBump::MODE_DIR_FORWARD;

const MODE_DONORS_STATIC: u32 = ThreadBump::MODE_DONOR_STATIC | ThreadBump::MODE_DIR_FORWARD;
const MODE_DONORS_STATIC_PRIO: u32 = MODE_DONORS_STATIC | ThreadBump::MODE_PRIO;
const MODE_DONORS_ORX: u32 = ThreadBump::MODE_DONOR_ORX | ThreadBump::MODE_DIR_FORWARD;
const MODE_DONORS_ORX_PRIO: u32 = MODE_DONORS_ORX | ThreadBump::MODE_PRIO;
const MODE_DONORS_BOXCAR: u32 = ThreadBump::MODE_DONOR_BOXCAR | ThreadBump::MODE_DIR_FORWARD;
const MODE_DONORS_BOXCAR_PRIO: u32 = MODE_DONORS_BOXCAR | ThreadBump::MODE_PRIO;

/// Neighbors: чётные — Backward, нечётные — Forward (заполняют общую границу
/// навстречу). ПОЛНОСТЬЮ мономорфный путь: per-thread направление зашито
/// константно через два отдельных runner-инстанцирования.
#[cfg_attr(coverage, coverage(off))]
fn arena_bench_neighbors_impl<const FULL: bool>(chunk_size: usize, smt: bool) {
    let core_ids = get_cores(smt);
    let total_capacity = chunk_size * core_ids.len();
    let arena = SharedArena::new(total_capacity);
    let bumps = arena.split_alternating_safe(core_ids.len());
    std::thread::scope(|s| {
        for (core_id, i) in core_ids.iter().zip(0..bumps.len()) {
            let core = *core_id;
            let bump = &bumps[i];
            s.spawn(move || {
                core_affinity::set_for_current(core);
                if i % 2 == 0 {
                    arena_run_thread::<MODE_BWD, FULL>(bump);
                } else {
                    arena_run_thread::<MODE_FWD, FULL>(bump);
                }
            });
        }
    });
}

/// `full` — объём работы (FULL или LIGHT).
macro_rules! mono_bench_wrappers {
    ($($full_tot:ident: $name:ident = $mode:expr;)*) => {
        $(
            #[cfg_attr(coverage, coverage(off))]
            pub fn $name(chunk_size: usize, smt: bool) {
                arena_bench_impl::<$mode, { $full_tot }>(chunk_size, smt);
            }
        )*
    };
}

mono_bench_wrappers! {
    true: arena_m_full_forward = MODE_FWD;
    true: arena_m_full_backward = MODE_BWD;
    true: arena_m_full_middleout = MODE_MO;
    true: arena_m_full_pair = MODE_PAIR_FWD;
    true: arena_m_full_donors = MODE_DONORS_STATIC;
    true: arena_m_full_donors_prio = MODE_DONORS_STATIC_PRIO;
    true: arena_m_full_donors_orx = MODE_DONORS_ORX;
    true: arena_m_full_donors_orx_prio = MODE_DONORS_ORX_PRIO;
    true: arena_m_full_donors_boxcar = MODE_DONORS_BOXCAR;
    true: arena_m_full_donors_boxcar_prio = MODE_DONORS_BOXCAR_PRIO;
    false: arena_m_light_forward = MODE_FWD;
    false: arena_m_light_backward = MODE_BWD;
    false: arena_m_light_middleout = MODE_MO;
    false: arena_m_light_pair = MODE_PAIR_FWD;
    false: arena_m_light_donors = MODE_DONORS_STATIC;
    false: arena_m_light_donors_prio = MODE_DONORS_STATIC_PRIO;
    false: arena_m_light_donors_orx = MODE_DONORS_ORX;
    false: arena_m_light_donors_orx_prio = MODE_DONORS_ORX_PRIO;
    false: arena_m_light_donors_boxcar = MODE_DONORS_BOXCAR;
    false: arena_m_light_donors_boxcar_prio = MODE_DONORS_BOXCAR_PRIO;
}

#[cfg_attr(coverage, coverage(off))]
pub fn arena_m_full_neighbors(chunk_size: usize, smt: bool) {
    arena_bench_neighbors_impl::<true>(chunk_size, smt);
}
#[cfg_attr(coverage, coverage(off))]
pub fn arena_m_light_neighbors(chunk_size: usize, smt: bool) {
    arena_bench_neighbors_impl::<false>(chunk_size, smt);
}

#[cfg_attr(coverage, coverage(off))]
pub fn arena_full(chunk_size: usize, smt: bool) {
    arena_bench(
        chunk_size,
        smt,
        true,
        ArenaLayout::Uniform(BumpDir::Forward),
    );
}

#[cfg_attr(coverage, coverage(off))]
pub fn arena_light(chunk_size: usize, smt: bool) {
    arena_bench(
        chunk_size,
        smt,
        false,
        ArenaLayout::Uniform(BumpDir::Forward),
    );
}

/// Версия `arena_full` с заданным направлением заполнения.
#[cfg_attr(coverage, coverage(off))]
pub fn arena_full_dir(chunk_size: usize, smt: bool, dir: BumpDir) {
    arena_bench(chunk_size, smt, true, ArenaLayout::Uniform(dir));
}

/// Версия `arena_light` с заданным направлением заполнения.
#[cfg_attr(coverage, coverage(off))]
pub fn arena_light_dir(chunk_size: usize, smt: bool, dir: BumpDir) {
    arena_bench(chunk_size, smt, false, ArenaLayout::Uniform(dir));
}

/// Полная версия: чанки соседей заполняют общую границу навстречу друг другу.
#[cfg_attr(coverage, coverage(off))]
pub fn arena_full_neighbors(chunk_size: usize, smt: bool) {
    arena_bench(chunk_size, smt, true, ArenaLayout::Neighbors);
}

/// Лёгкая версия: чанки соседей заполняют общую границу навстречу друг другу.
#[cfg_attr(coverage, coverage(off))]
pub fn arena_light_neighbors(chunk_size: usize, smt: bool) {
    arena_bench(chunk_size, smt, false, ArenaLayout::Neighbors);
}

/// Полная версия: соседи делят ОДИН регион и берут память друг у друга.
#[cfg_attr(coverage, coverage(off))]
pub fn arena_full_pair(chunk_size: usize, smt: bool) {
    arena_bench(chunk_size, smt, true, ArenaLayout::Pair);
}

/// Лёгкая версия: соседи делят ОДИН регион и берут память друг у друга.
#[cfg_attr(coverage, coverage(off))]
pub fn arena_light_pair(chunk_size: usize, smt: bool) {
    arena_bench(chunk_size, smt, false, ArenaLayout::Pair);
}

// --- Доноры: статичный список, без приоритета ---
#[cfg_attr(coverage, coverage(off))]
pub fn arena_full_donors(chunk_size: usize, smt: bool) {
    arena_bench(chunk_size, smt, true, ArenaLayout::Donors);
}
#[cfg_attr(coverage, coverage(off))]
pub fn arena_light_donors(chunk_size: usize, smt: bool) {
    arena_bench(chunk_size, smt, false, ArenaLayout::Donors);
}

// --- Доноры: статичный список + приоритет ---
#[cfg_attr(coverage, coverage(off))]
pub fn arena_full_donors_prio(chunk_size: usize, smt: bool) {
    arena_bench(chunk_size, smt, true, ArenaLayout::DonorsPrio);
}
#[cfg_attr(coverage, coverage(off))]
pub fn arena_light_donors_prio(chunk_size: usize, smt: bool) {
    arena_bench(chunk_size, smt, false, ArenaLayout::DonorsPrio);
}

// --- Доноры: orx-concurrent-vec (динамический), без приоритета ---
#[cfg_attr(coverage, coverage(off))]
pub fn arena_full_donors_orx(chunk_size: usize, smt: bool) {
    arena_bench(chunk_size, smt, true, ArenaLayout::DonorsOrx);
}
#[cfg_attr(coverage, coverage(off))]
pub fn arena_light_donors_orx(chunk_size: usize, smt: bool) {
    arena_bench(chunk_size, smt, false, ArenaLayout::DonorsOrx);
}

// --- Доноры: orx-concurrent-vec + приоритет ---
#[cfg_attr(coverage, coverage(off))]
pub fn arena_full_donors_orx_prio(chunk_size: usize, smt: bool) {
    arena_bench(chunk_size, smt, true, ArenaLayout::DonorsOrxPrio);
}
#[cfg_attr(coverage, coverage(off))]
pub fn arena_light_donors_orx_prio(chunk_size: usize, smt: bool) {
    arena_bench(chunk_size, smt, false, ArenaLayout::DonorsOrxPrio);
}

// --- Доноры: boxcar (динамический), без приоритета ---
#[cfg_attr(coverage, coverage(off))]
pub fn arena_full_donors_boxcar(chunk_size: usize, smt: bool) {
    arena_bench(chunk_size, smt, true, ArenaLayout::DonorsBoxcar);
}
#[cfg_attr(coverage, coverage(off))]
pub fn arena_light_donors_boxcar(chunk_size: usize, smt: bool) {
    arena_bench(chunk_size, smt, false, ArenaLayout::DonorsBoxcar);
}

// --- Доноры: boxcar + приоритет ---
#[cfg_attr(coverage, coverage(off))]
pub fn arena_full_donors_boxcar_prio(chunk_size: usize, smt: bool) {
    arena_bench(chunk_size, smt, true, ArenaLayout::DonorsBoxcarPrio);
}
#[cfg_attr(coverage, coverage(off))]
pub fn arena_light_donors_boxcar_prio(chunk_size: usize, smt: bool) {
    arena_bench(chunk_size, smt, false, ArenaLayout::DonorsBoxcarPrio);
}

// ============================================================
//                        PGO
// ============================================================

#[cfg_attr(coverage, coverage(off))]
pub fn profile_bump_chunk_size_full() -> usize {
    let bump = Bump::new();
    let s = bench_scale();
    let capacity = 4 * 100 * (100 / s).max(1);
    let mut vectr = BumpVec::with_capacity_in(capacity, &bump);
    for _ in 0..(200 / s).max(1) {
        for _ in 0..(200 / s).max(1) {
            let mut vec = BumpVec::with_capacity_in(400, &bump);
            for _ in 0..(100 / s).max(1) {
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

#[cfg_attr(coverage, coverage(off))]
pub fn profile_bump_chunk_size_light() -> usize {
    let bump = Bump::new();
    let s = bench_scale();
    let capacity = 100 * (100 / s).max(1);
    let mut vectr = BumpVec::with_capacity_in(capacity, &bump);
    for _ in 0..(100 / s).max(1) {
        for _ in 0..(100 / s).max(1) {
            let mut vec = BumpVec::with_capacity_in(400, &bump);
            for _ in 0..(100 / s).max(1) {
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

#[cfg_attr(coverage, coverage(off))]
pub fn profile_arena_chunk_size_full() -> usize {
    let arena = SharedArena::new(1024 * 1024 * 1024);
    let bumps = arena.split_safe(1);
    let bump = &bumps[0];
    let s = bench_scale();

    let mut vectr: ArenaVec<ArenaVec<ArenaString>> = ArenaVec::with_capacity_in::<
        { core::mem::align_of::<ArenaVec<ArenaString>>() },
    >(40000 / s, bump);
    for _ in 0..(200 / s).max(1) {
        for _ in 0..(200 / s).max(1) {
            let mut vec: ArenaVec<ArenaString> =
                ArenaVec::with_capacity_in::<{ core::mem::align_of::<ArenaString>() }>(400, bump);
            for _ in 0..(100 / s).max(1) {
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

#[cfg_attr(coverage, coverage(off))]
pub fn profile_arena_chunk_size_light() -> usize {
    let arena = SharedArena::new(512 * 1024 * 1024);
    let bumps = arena.split_safe(1);
    let bump = &bumps[0];
    let s = bench_scale();

    let mut vectr: ArenaVec<ArenaVec<ArenaString>> = ArenaVec::with_capacity_in::<
        { core::mem::align_of::<ArenaVec<ArenaString>>() },
    >(10000 / s, bump);
    for _ in 0..(100 / s).max(1) {
        for _ in 0..(100 / s).max(1) {
            let mut vec: ArenaVec<ArenaString> =
                ArenaVec::with_capacity_in::<{ core::mem::align_of::<ArenaString>() }>(400, bump);
            for _ in 0..(100 / s).max(1) {
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

/// Какую версию бенчмарка «shared arena» гонять: полностью мономорфную
/// (`alloc_raw_m`, `arena_m_*`), с runtime-диспетчером (`arena_bench`,
/// `arena_full`/`arena_light`/...), или обе подряд. Выбирается переменной
/// окружения `R3_BENCH_STYLE=mono|dispatch|both` (по умолчанию `both`).
#[derive(Clone, Copy, PartialEq, Debug)]
enum BenchStyle {
    Mono,
    Dispatch,
    Both,
}

#[cfg_attr(coverage, coverage(off))]
fn bench_style() -> BenchStyle {
    match std::env::var("R3_BENCH_STYLE").as_deref() {
        Ok("mono") => BenchStyle::Mono,
        Ok("dispatch") => BenchStyle::Dispatch,
        _ => BenchStyle::Both,
    }
}

/// Масштаб нагрузки бенчмарк-каркаса: все итерации делятся на `R3_BENCH_SCALE`
/// (по умолчанию 1 — исходная нагрузка, без изменений). Цель — дать возможность
/// быстро (за секунды) прогонять тяжёлые бенчмарки под покрытием или на
/// ограниченных ресурсах, не трогая поведение реальных прогонов. Границы
/// никогда не опускаются ниже 1, чтобы хоть одна итерация выполнялась.
#[cfg_attr(coverage, coverage(off))]
fn bench_scale() -> usize {
    std::env::var("R3_BENCH_SCALE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1)
}

/// Запустить один бенч-вариант арены с нужным стилем (mono/dispatch/both) и
/// вернуть медиану времени. `print_name` — как пометить строку.
#[cfg_attr(coverage, coverage(off))]
fn run_arena_variant<M: Fn(usize, bool) -> (), D: Fn(usize, bool) -> ()>(
    style: BenchStyle,
    label: &str,
    mono: M,
    dispatch: D,
    chunk: usize,
    smt: bool,
) {
    let bench = |f: &dyn Fn(usize, bool) -> ()| {
        (0..(10 / bench_scale()).max(1))
            .map(|_| {
                let start = std::time::Instant::now();
                f(chunk, smt);
                start.elapsed().as_micros()
            })
            .collect::<Vec<u128>>()
    };
    match style {
        BenchStyle::Mono => {
            let t = bench(&|c, s| mono(c, s));
            println!("  {:<12} mono     : {} Р’Вµs", label, median(&t));
        }
        BenchStyle::Dispatch => {
            let t = bench(&|c, s| dispatch(c, s));
            println!("  {:<12} dispatch : {} Р’Вµs", label, median(&t));
        }
        BenchStyle::Both => {
            let t_mono = bench(&|c, s| mono(c, s));
            let t_disp = bench(&|c, s| dispatch(c, s));
            println!(
                "  {:<12} mono {:>6} Р’Вµs | dispatch {:>6} Р’Вµs",
                label,
                median(&t_mono),
                median(&t_disp)
            );
        }
    }
}

#[cfg_attr(coverage, coverage(off))]
fn get_cores(smt: bool) -> Vec<core_affinity::CoreId> {
    let all = core_affinity::get_core_ids().unwrap();
    if smt {
        all
    } else {
        let physical_count = num_cpus::get_physical();
        all.into_iter().take(physical_count).collect()
    }
}

#[cfg_attr(coverage, coverage(off))]
fn median(times: &[u128]) -> u128 {
    let mut sorted = times.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

// ============================================================
//                          MAIN
// ============================================================

#[hotpath::main]
#[cfg_attr(coverage, coverage(off))]
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

/// Быстрый, но репрезентативный прогон горячих путей аллокатора для сбора
/// PGO-профилей. Запускается вместо тяжёлых бенчмарков `run()` только когда
/// задана переменная окружения `R3_PGO_TRAIN` (её выставляет build.ps1 во время
/// тренировочного прогона). Несколько сотен тысяч аллокаций — секунды, а не минуты.
/// Между пачками делается reset(), чтобы одиночные bump'ы не ушли в OOM-панику.
#[cfg_attr(coverage, coverage(off))]
pub fn pgo_train() {
    const BATCH: u32 = 20_000;
    const ROUNDS: u32 = 10; // ~200k аллокаций на каждый прогон

    // Forward / Backward / MiddleOut на одном bump.
    for &dir in &[BumpDir::Forward, BumpDir::Backward, BumpDir::MiddleOut] {
        let arena = SharedArena::new(8 * 1024 * 1024);
        let v = arena.split_with_safe(1, dir);
        let b = &v[0];
        for _ in 0..ROUNDS {
            hotpath::measure_block!("pgo-alloc", {
                for _ in 0..BATCH {
                    let p = b.alloc_raw::<8>(8);
                    let q = b.alloc_raw::<1>(1);
                    let r = b.alloc_raw::<16>(24);
                    unsafe {
                        *q = 0xAB;
                        let _ = (p, r);
                    }
                }
            });
            b.reset();
        }
    }

    // Neighbors (alternating directions).
    {
        let arena = SharedArena::new(8 * 1024 * 1024);
        let bumps = arena.split_alternating_safe(2);
        let (a, c) = (&bumps[0], &bumps[1]);
        for _ in 0..ROUNDS {
            for _ in 0..BATCH {
                let _ = a.alloc_raw::<8>(8);
                let _ = c.alloc_raw::<8>(8);
            }
            a.reset();
            c.reset();
        }
    }

    // Pair (shared combined region).
    {
        let arena = SharedArena::new(16 * 1024 * 1024);
        let bumps = arena.split_paired_safe(2);
        let (a, c) = (&bumps[0], &bumps[1]);
        for _ in 0..ROUNDS {
            for _ in 0..BATCH {
                let _ = a.alloc_raw::<8>(8);
                let _ = c.alloc_raw::<8>(8);
            }
            a.reset();
            c.reset();
        }
    }

    // Donors: Static / Orx / Boxcar — переполнение + приоритет + удаление -> fallback.
    for &kind in &[
        DonorListKind::Static,
        DonorListKind::Orx,
        DonorListKind::Boxcar,
    ] {
        let arena = SharedArena::new(16 * 1024 * 1024);
        let mut prio = vec![0u32; 5];
        prio[0] = 100;
        prio[2] = 1; // низкий (минимальный среди доноров 0,2,4)
        prio[4] = 50;
        let bumps = arena.split_donors_with_safe(
            5,
            DonorPolicy {
                kind,
                use_priority: true,
                every: 2,
                priorities: Some(prio),
            },
        );
        let needy = &bumps[1];
        for _ in 0..ROUNDS {
            hotpath::measure_block!("pgo-donors-alloc", {
                for _ in 0..BATCH {
                    let _ = needy.alloc_raw::<8>(8);
                }
            });
            needy.reset();
        }
        // Динамическое удаление донора -> fallback вне арены.
        needy.remove_donor(0);
        for _ in 0..(ROUNDS / 2) {
            hotpath::measure_block!("pgo-donors-fallback", {
                for _ in 0..BATCH {
                    let _ = needy.alloc_raw::<8>(8);
                }
            });
            needy.reset();
        }
    }
}

#[cfg_attr(coverage, coverage(off))]
fn run_directional_benchmarks(smt: bool, pgo_full_arena: usize, pgo_light_arena: usize) {
    let mode_str = if smt {
        "SMT (all logical cores)"
    } else {
        "NO SMT (physical cores only)"
    };
    println!("\n########## Directional Arena: {} ##########\n", mode_str);

    let style = bench_style();
    println!(
        "=== Directional FULL ({}) — style {:?} ===",
        mode_str, style
    );

    run_arena_variant(
        style,
        "Forward",
        arena_m_full_forward,
        |c, s| arena_full_dir(c, s, BumpDir::Forward),
        pgo_full_arena,
        smt,
    );
    run_arena_variant(
        style,
        "Backward",
        arena_m_full_backward,
        |c, s| arena_full_dir(c, s, BumpDir::Backward),
        pgo_full_arena,
        smt,
    );
    run_arena_variant(
        style,
        "MiddleOut",
        arena_m_full_middleout,
        |c, s| arena_full_dir(c, s, BumpDir::MiddleOut),
        pgo_full_arena,
        smt,
    );
    run_arena_variant(
        style,
        "Neighbors",
        arena_m_full_neighbors,
        arena_full_neighbors,
        pgo_full_arena,
        smt,
    );
    run_arena_variant(
        style,
        "Pair",
        arena_m_full_pair,
        arena_full_pair,
        pgo_full_arena,
        smt,
    );
    run_arena_variant(
        style,
        "Donors",
        arena_m_full_donors,
        arena_full_donors,
        pgo_full_arena,
        smt,
    );
    run_arena_variant(
        style,
        "DonorsPrio",
        arena_m_full_donors_prio,
        arena_full_donors_prio,
        pgo_full_arena,
        smt,
    );
    run_arena_variant(
        style,
        "DonorsOrx",
        arena_m_full_donors_orx,
        arena_full_donors_orx,
        pgo_full_arena,
        smt,
    );
    run_arena_variant(
        style,
        "DonorsOrxPrio",
        arena_m_full_donors_orx_prio,
        arena_full_donors_orx_prio,
        pgo_full_arena,
        smt,
    );
    run_arena_variant(
        style,
        "DonorsBoxcar",
        arena_m_full_donors_boxcar,
        arena_full_donors_boxcar,
        pgo_full_arena,
        smt,
    );
    run_arena_variant(
        style,
        "DonorsBoxcarPrio",
        arena_m_full_donors_boxcar_prio,
        arena_full_donors_boxcar_prio,
        pgo_full_arena,
        smt,
    );

    println!(
        "=== Directional LIGHT ({}) — style {:?} ===",
        mode_str, style
    );

    run_arena_variant(
        style,
        "Forward",
        arena_m_light_forward,
        |c, s| arena_light_dir(c, s, BumpDir::Forward),
        pgo_light_arena,
        smt,
    );
    run_arena_variant(
        style,
        "Backward",
        arena_m_light_backward,
        |c, s| arena_light_dir(c, s, BumpDir::Backward),
        pgo_light_arena,
        smt,
    );
    run_arena_variant(
        style,
        "MiddleOut",
        arena_m_light_middleout,
        |c, s| arena_light_dir(c, s, BumpDir::MiddleOut),
        pgo_light_arena,
        smt,
    );
    run_arena_variant(
        style,
        "Neighbors",
        arena_m_light_neighbors,
        arena_light_neighbors,
        pgo_light_arena,
        smt,
    );
    run_arena_variant(
        style,
        "Pair",
        arena_m_light_pair,
        arena_light_pair,
        pgo_light_arena,
        smt,
    );
    run_arena_variant(
        style,
        "Donors",
        arena_m_light_donors,
        arena_light_donors,
        pgo_light_arena,
        smt,
    );
    run_arena_variant(
        style,
        "DonorsPrio",
        arena_m_light_donors_prio,
        arena_light_donors_prio,
        pgo_light_arena,
        smt,
    );
    run_arena_variant(
        style,
        "DonorsOrx",
        arena_m_light_donors_orx,
        arena_light_donors_orx,
        pgo_light_arena,
        smt,
    );
    run_arena_variant(
        style,
        "DonorsOrxPrio",
        arena_m_light_donors_orx_prio,
        arena_light_donors_orx_prio,
        pgo_light_arena,
        smt,
    );
    run_arena_variant(
        style,
        "DonorsBoxcar",
        arena_m_light_donors_boxcar,
        arena_light_donors_boxcar,
        pgo_light_arena,
        smt,
    );
    run_arena_variant(
        style,
        "DonorsBoxcarPrio",
        arena_m_light_donors_boxcar_prio,
        arena_light_donors_boxcar_prio,
        pgo_light_arena,
        smt,
    );
}

#[cfg_attr(coverage, coverage(off))]
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
    println!("\n########## Бенч: {} ##########\n", mode_str);

    let style = bench_style();
    let sc = bench_scale();
    // Default-Forward арена: mono (`arena_m_*_forward`) vs dispatch (`arena_*`).
    // Возвращает (mono_us, dispatch_us) — по стилю заполняет нужные.
    let arena_mesure =
        |chunk: usize, mono: fn(usize, bool), dispatch: fn(usize, bool)| -> (u128, u128) {
            let m = (0..(10 / sc).max(1))
                .map(|_| {
                    let s = std::time::Instant::now();
                    mono(chunk, smt);
                    s.elapsed().as_micros()
                })
                .collect::<Vec<_>>();
            let d = (0..(10 / sc).max(1))
                .map(|_| {
                    let s = std::time::Instant::now();
                    dispatch(chunk, smt);
                    s.elapsed().as_micros()
                })
                .collect::<Vec<_>>();
            (median(&m), median(&d))
        };
    let arena_full_res = |chunk: usize| {
        let (m, d) = arena_mesure(chunk, arena_m_full_forward, arena_full);
        match style {
            BenchStyle::Mono => (m, m),
            BenchStyle::Dispatch => (d, d),
            BenchStyle::Both => (m, d),
        }
    };
    let arena_light_res = |chunk: usize| {
        let (m, d) = arena_mesure(chunk, arena_m_light_forward, arena_light);
        match style {
            BenchStyle::Mono => (m, m),
            BenchStyle::Dispatch => (d, d),
            BenchStyle::Both => (m, d),
        }
    };

    // Прогрев (гоняем выбранный стиль как минимум один раз).
    for _ in 0..(5 / sc).max(1) {
        mimm(smt);
        mimm_light(smt);
        bump_scope_m(pgo_full_bump, smt);
        bump_scope_m_light(pgo_light_bump, smt);
        bump_shared_m(pgo_full_bump, smt);
        bump_shared_m_light(pgo_light_bump, smt);
        arena_full(pgo_full_arena, smt);
        arena_light(pgo_light_arena, smt);
    }

    println!("=== FULL VERSION ({}) — style {:?} ===", mode_str, style);
    for round in 0..(3 / sc).max(1) {
        let mimm_times: Vec<u128> = (0..(10 / sc).max(1))
            .map(|_| {
                let start = std::time::Instant::now();
                mimm(smt);
                start.elapsed().as_micros()
            })
            .collect();
        let bump_times: Vec<u128> = (0..(10 / sc).max(1))
            .map(|_| {
                let start = std::time::Instant::now();
                bump_scope_m(pgo_full_bump, smt);
                start.elapsed().as_micros()
            })
            .collect();
        let shared_times: Vec<u128> = (0..(10 / sc).max(1))
            .map(|_| {
                let start = std::time::Instant::now();
                bump_shared_m(pgo_full_bump, smt);
                start.elapsed().as_micros()
            })
            .collect();
        let (arena_mono_us, arena_disp_us) = arena_full_res(pgo_full_arena);

        match style {
            BenchStyle::Mono => println!(
                "Round {}: MIMALOC = {} Р’Вµs, Bump = {} Р’Вµs, SharedBump = {} Р’Вµs, Arena(mono) = {} Р’Вµs",
                round + 1,
                median(&mimm_times),
                median(&bump_times),
                median(&shared_times),
                arena_mono_us
            ),
            BenchStyle::Dispatch => println!(
                "Round {}: MIMALOC = {} Р’Вµs, Bump = {} Р’Вµs, SharedBump = {} Р’Вµs, Arena(dispatch) = {} Р’Вµs",
                round + 1,
                median(&mimm_times),
                median(&bump_times),
                median(&shared_times),
                arena_disp_us
            ),
            BenchStyle::Both => println!(
                "Round {}: MIMALOC = {} Р’Вµs, Bump = {} Р’Вµs, SharedBump = {} Р’Вµs, Arena mono|dispatch = {}|{} Р’Вµs",
                round + 1,
                median(&mimm_times),
                median(&bump_times),
                median(&shared_times),
                arena_mono_us,
                arena_disp_us
            ),
        }
    }

    println!("\n=== LIGHT VERSION ({}) — style {:?} ===", mode_str, style);
    for round in 0..(3 / sc).max(1) {
        let mimm_times: Vec<u128> = (0..(10 / sc).max(1))
            .map(|_| {
                let start = std::time::Instant::now();
                mimm_light(smt);
                start.elapsed().as_micros()
            })
            .collect();
        let bump_times: Vec<u128> = (0..(10 / sc).max(1))
            .map(|_| {
                let start = std::time::Instant::now();
                bump_scope_m_light(pgo_light_bump, smt);
                start.elapsed().as_micros()
            })
            .collect();
        let shared_times: Vec<u128> = (0..(10 / sc).max(1))
            .map(|_| {
                let start = std::time::Instant::now();
                bump_shared_m_light(pgo_light_bump, smt);
                start.elapsed().as_micros()
            })
            .collect();
        let (arena_mono_us, arena_disp_us) = arena_light_res(pgo_light_arena);

        match style {
            BenchStyle::Mono => println!(
                "Round {}: MIMALOC = {} Р’Вµs, Bump = {} Р’Вµs, SharedBump = {} Р’Вµs, Arena(mono) = {} Р’Вµs",
                round + 1,
                median(&mimm_times),
                median(&bump_times),
                median(&shared_times),
                arena_mono_us
            ),
            BenchStyle::Dispatch => println!(
                "Round {}: MIMALOC = {} Р’Вµs, Bump = {} Р’Вµs, SharedBump = {} Р’Вµs, Arena(dispatch) = {} Р’Вµs",
                round + 1,
                median(&mimm_times),
                median(&bump_times),
                median(&shared_times),
                arena_disp_us
            ),
            BenchStyle::Both => println!(
                "Round {}: MIMALOC = {} Р’Вµs, Bump = {} Р’Вµs, SharedBump = {} Р’Вµs, Arena mono|dispatch = {}|{} Р’Вµs",
                round + 1,
                median(&mimm_times),
                median(&bump_times),
                median(&shared_times),
                arena_mono_us,
                arena_disp_us
            ),
        }
    }
}

// ============================================================
//                        ТЕСТЫ
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic;

    // Множитель для облегчения тестов под Miri (Tree Borrows): в обычном режиме
    // = 1 (полные объёмы и арена), под Miri = 100 (в ×100 меньше аллокаций и
    // пропорционально меньше арена). Соотношение «сколько заняли / размер чанка»
    // сохраняется, поэтому логика переполнения/заимствования не меняется.
    #[cfg(not(miri))]
    const MS: usize = 1;
    #[cfg(miri)]
    const MS: usize = 100;

    fn arena_scale(base: usize) -> usize {
        (base / MS).next_multiple_of(4096)
    }
    fn count_scale(base: usize) -> usize {
        base / MS
    }

    /// Один bump заданного направления на `cap` байт.
    /// Возвращает также арену — она должна жить всё время жизни bump'а
    /// (иначе память освободится, а указатель в ThreadBump станет висячим).
    fn make_one(dir: BumpDir, cap: usize) -> (SharedArena, CachePadded<ThreadBump>) {
        let arena = SharedArena::new(cap);
        // Создаёт единственный bump без соседей; арена возвращается вместе с ним,
        // потому что соблюдён lifetime-инвариант (unsafe — как и в примере с сырым Vec).
        let mut v = unsafe { arena.split_with(1, dir) };
        (arena, v.pop().unwrap())
    }

    // Тестовые обёртки над (unsafe) split-методами: в этих тестах арена
    // живёт дольше, чем bump'ы, поэтому соблюдён lifetime-инвариант
    // («арена не умирает раньше bumps»), и обёртки считаются безопасными.
    fn split_with_raw(arena: &SharedArena, n: usize, dir: BumpDir) -> Vec<CachePadded<ThreadBump>> {
        unsafe { arena.split_with(n, dir) }
    }
    fn split_alt_raw(arena: &SharedArena, n: usize) -> Vec<CachePadded<ThreadBump>> {
        unsafe { arena.split_alternating(n) }
    }
    fn split_pair_raw(arena: &SharedArena, n: usize) -> Vec<CachePadded<ThreadBump>> {
        unsafe { arena.split_paired(n) }
    }
    fn split_don_raw(arena: &SharedArena, n: usize, every: usize) -> Vec<CachePadded<ThreadBump>> {
        unsafe { arena.split_donors(n, every) }
    }
    fn split_don_with_raw(
        arena: &SharedArena,
        n: usize,
        p: DonorPolicy,
    ) -> Vec<CachePadded<ThreadBump>> {
        unsafe { arena.split_donors_with(n, p) }
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
        assert!(res.is_err(), "ожидается OOM-panic");
    }

    #[test]
    fn backward_oom_panics() {
        let (_arena, bump) = make_one(BumpDir::Backward, 64);
        let _ = bump.alloc_raw::<1>(64);
        let res = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            bump.alloc_raw::<1>(1);
        }));
        assert!(res.is_err(), "ожидается OOM-panic");
    }

    #[test]
    fn middleout_oom_panics() {
        let (_arena, bump) = make_one(BumpDir::MiddleOut, 64);
        let res = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            bump.alloc_raw::<1>(64);
        }));
        assert!(
            res.is_err(),
            "ожидается OOM-panic (середина даёт только половину свободных байт)"
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
        let bumps = split_alt_raw(&arena, 4);
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
        let bumps = split_alt_raw(&arena, 2);
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
        let bumps = split_with_raw(&arena, 1, BumpDir::MiddleOut);
        let b = &bumps[0];
        assert_eq!(b.lo.load(Ordering::Relaxed), 0);
        assert_eq!(b.hi.load(Ordering::Relaxed), 0);
    }

    // ---- Пара: объединённый регион и заём памяти у соседа ----

    #[test]
    fn pair_shares_combined_region() {
        let arena = SharedArena::new(8192);
        let bumps = split_pair_raw(&arena, 2);
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
        let bumps = split_pair_raw(&arena, 2);
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
        let bumps = split_pair_raw(&arena, 2);
        let (even, odd) = (&bumps[0], &bumps[1]);
        let base = arena.alloc.ptr as usize;

        let _ = even.alloc_raw::<1>(16); // even заняло крошечку сверху: [8176, 8192)
        let _ = odd.alloc_raw::<1>(4096); // odd заполнил свою половину [0, 4096)
        // Теперь odd занимает у even свободную половину [4096, 8176).
        let borrowed = odd.alloc_raw::<1>(4000) as usize;
        assert_eq!(
            borrowed,
            base + 4176,
            "заём — в половине соседа, смежный с серединой"
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
        let bumps = split_pair_raw(&arena, 2);
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
            "счётчик соседа расширен за счёт заёма"
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
        let arena = SharedArena::new(arena_scale(2 * 1024 * 1024)); // 2 MB: половина = 1 MB
        let bumps = split_pair_raw(&arena, 2);

        std::thread::scope(|s| {
            // even: 200_000 * 8 = 1.6 MB > своей половины (1 MB) -> заимствует 0.6 MB.
            s.spawn(|| {
                let bump = &bumps[0];
                let mut ptrs: Vec<*mut usize> = Vec::with_capacity(count_scale(200_000));
                for j in 0..count_scale(200_000) {
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
                let mut ptrs: Vec<*mut usize> = Vec::with_capacity(count_scale(50_000));
                for j in 0..count_scale(50_000) {
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
        let arena = SharedArena::new(arena_scale(2 * 1024 * 1024)); // 1 MB на поток
        let bumps = split_don_raw(&arena, 2, 2);
        let (donor, needy) = (&bumps[0], &bumps[1]);
        assert!(donor.can_give);
        assert!(!needy.can_give);

        // Поток 1: 200_000 * 8 = 1.6 MB > своего чанка (1 MB) -> берёт у донора.
        let mut ptrs: Vec<*mut usize> = Vec::with_capacity(count_scale(200_000));
        for j in 0..count_scale(200_000) {
            let p = needy.alloc_raw::<8>(8) as *mut usize;
            unsafe { *p = 0xC000 + j };
            ptrs.push(p);
        }
        // Последние блоки должны лежать в регионе донора (отданы с высокой стороны).
        let d_start = donor.ptr as usize;
        let d_end = d_start + donor.len;
        let last = *ptrs.last().unwrap() as usize;
        assert!(last >= d_start && last < d_end, "блок взят у донора");
        // Счётчик донора расширился с высокой стороны.
        assert!(donor.hi.load(Ordering::Relaxed) > 0);
        // Данные не затёрты.
        for (j, p) in ptrs.iter().enumerate() {
            assert_eq!(
                unsafe { **p },
                0xC000 + j,
                "данные не затёрты донором/соседом"
            );
        }
        // У донора своя память свободна (lo не трогали).
        assert_eq!(donor.lo.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn donors_fallback_when_no_donor_free() {
        // Ни один донор не помечен (every = 0): при нехватке места
        // выделяется новый чанк «где-то в памяти» (grow_fallback). Проверяем, что
        // он находится ВНЕ основной арены и данные живы; Drop освобождает его.
        let arena = SharedArena::new(arena_scale(1 << 20));
        let bumps = split_don_raw(&arena, 4, 0);
        let needy = &bumps[1];
        assert!(!bumps.iter().any(|b| b.can_give));

        let arena_start = arena.alloc.ptr as usize;
        let arena_end = arena_start + arena.alloc.size;

        let mut ptrs: Vec<*mut usize> = Vec::with_capacity(count_scale(300_000));
        for j in 0..count_scale(300_000) {
            let p = needy.alloc_raw::<8>(8) as *mut usize;
            unsafe { *p = 0xD000 + j };
            ptrs.push(p);
        }
        // Хотя бы часть блоков должна уйти в fallback (вне арены).
        let outside = ptrs
            .iter()
            .any(|&p| (p as usize) < arena_start || (p as usize) >= arena_end);
        assert!(outside, "ожидается fallback-чанк вне арены");
        for (j, p) in ptrs.iter().enumerate() {
            assert_eq!(unsafe { **p }, 0xD000 + j);
        }
        // fallback непуст (Drop позже освободит).
        assert!(!needy.fallback.lock().chunks.is_empty());
    }

    // ---- Статичный список + приоритет: низкоприоритетный берётся первым ----

    #[test]
    fn donors_static_priority_low_taken_first() {
        // 5 потоков, доноры — индексы 0 и 2 (every = 2). Явно задаём приоритеты
        // так, чтобы у донора 0 он БЫЛ ВЫШЕ, чем у донора 2. Значит донор 2
        // (низкий приоритет) должен использоваться первым.
        let arena = SharedArena::new(arena_scale(5 * 1024 * 1024)); // 1 MB на поток
        let mut prio = vec![0u32; 5];
        prio[0] = 100; // высокий
        prio[2] = 1; // низкий (минимальный среди доноров 0,2,4)
        prio[4] = 50; // средний
        let bumps = split_don_with_raw(
            &arena,
            5,
            DonorPolicy {
                kind: DonorListKind::Static,
                use_priority: true,
                every: 2,
                priorities: Some(prio),
            },
        );
        let needy = &bumps[1];
        let d0 = &bumps[0];
        let d2 = &bumps[2];

        // Переполняем needy: первый заём должен уйти к донору 2 (низкий приоритет).
        let mut ptrs: Vec<*mut usize> = Vec::with_capacity(count_scale(200_000));
        for j in 0..count_scale(200_000) {
            let p = needy.alloc_raw::<8>(8) as *mut usize;
            unsafe { *p = 0xE000 + j };
            ptrs.push(p);
        }
        let last = *ptrs.last().unwrap() as usize;
        let in_d0 = last >= d0.ptr as usize && last < d0.ptr as usize + d0.len;
        let in_d2 = last >= d2.ptr as usize && last < d2.ptr as usize + d2.len;
        assert!(
            in_d2 && !in_d0,
            "первый заём взялся у низкоприоритетного донора 2"
        );
        for (j, p) in ptrs.iter().enumerate() {
            assert_eq!(unsafe { **p }, 0xE000 + j);
        }
    }

    // ---- orx-concurrent-vec: динамическое добавление/удаление донора ----

    #[test]
    fn donors_orx_add_then_remove() {
        let arena = SharedArena::new(arena_scale(2 * 1024 * 1024)); // 1 MB на поток, 2 потока
        // Изначально доноров нет (every = 0).
        let bumps = split_don_with_raw(&arena, 2, DonorPolicy::orx(0));
        let needy = &bumps[1];
        let donor0 = &bumps[0];
        let a_start = arena.alloc.ptr as usize;
        let a_end = a_start + arena.alloc.size;

        // Добавляем донора 0 в рантайме.
        assert!(needy.add_donor(0, 0));
        assert!(!needy.add_donor(0, 0)); // уже есть — повторно не добавляем (индекс тот же)

        // Переполняем needy: должен взять у добавленного донора 0.
        let mut ptrs: Vec<*mut usize> = Vec::with_capacity(count_scale(200_000));
        for j in 0..count_scale(200_000) {
            let p = needy.alloc_raw::<8>(8) as *mut usize;
            unsafe { *p = 0xF000 + j };
            ptrs.push(p);
        }
        let last = *ptrs.last().unwrap() as usize;
        assert!(
            last >= donor0.ptr as usize && last < donor0.ptr as usize + donor0.len,
            "блок должен быть взят у динамического донора 0"
        );
        for (j, p) in ptrs.iter().enumerate() {
            assert_eq!(unsafe { **p }, 0xF000 + j);
        }

        // Удаляем донора 0 — теперь заём невозможен, пойдёт fallback вне арены.
        assert!(needy.remove_donor(0));
        assert!(!needy.remove_donor(0)); // уже удалён
        let extra: Vec<*mut usize> = (0..2000)
            .map(|j| {
                let p = needy.alloc_raw::<8>(8) as *mut usize;
                unsafe { *p = 0xA000 + j };
                p
            })
            .collect();
        let all_outside = extra
            .iter()
            .all(|&p| (p as usize) < a_start || (p as usize) >= a_end);
        assert!(
            all_outside,
            "после удаления донора должен быть fallback вне арены"
        );
    }

    // ---- boxcar: то же динамическое добавление/удаление ----

    #[test]
    fn donors_boxcar_add_then_remove() {
        let arena = SharedArena::new(arena_scale(2 * 1024 * 1024));
        let bumps = split_don_with_raw(&arena, 2, DonorPolicy::boxcar(0));
        let needy = &bumps[1];
        let donor0 = &bumps[0];
        let a_start = arena.alloc.ptr as usize;
        let a_end = a_start + arena.alloc.size;

        assert!(needy.add_donor(0, 0));
        let mut ptrs: Vec<*mut usize> = Vec::with_capacity(count_scale(200_000));
        for j in 0..count_scale(200_000) {
            let p = needy.alloc_raw::<8>(8) as *mut usize;
            unsafe { *p = 0xB000 + j };
            ptrs.push(p);
        }
        let last = *ptrs.last().unwrap() as usize;
        assert!(last >= donor0.ptr as usize && last < donor0.ptr as usize + donor0.len);
        for (j, p) in ptrs.iter().enumerate() {
            assert_eq!(unsafe { **p }, 0xB000 + j);
        }

        assert!(needy.remove_donor(0));
        let extra: Vec<*mut usize> = (0..2000)
            .map(|j| {
                let p = needy.alloc_raw::<8>(8) as *mut usize;
                unsafe { *p = 0xC000 + j };
                p
            })
            .collect();
        let all_outside = extra
            .iter()
            .all(|&p| (p as usize) < a_start || (p as usize) >= a_end);
        assert!(
            all_outside,
            "после удаления донора должен быть fallback вне арены"
        );
    }

    // ---- orx + приоритет: из доступных выбирается минимальный приоритет ----

    #[test]
    fn donors_orx_priority_low_taken_first() {
        let arena = SharedArena::new(arena_scale(5 * 1024 * 1024));
        let mut prio = vec![0u32; 5];
        prio[0] = 100;
        prio[2] = 1; // низкий (минимальный среди доноров 0,2,4)
        prio[4] = 50; // средний
        let bumps = split_don_with_raw(
            &arena,
            5,
            DonorPolicy {
                kind: DonorListKind::Orx,
                use_priority: true,
                every: 2,
                priorities: Some(prio),
            },
        );
        let needy = &bumps[1];
        let d0 = &bumps[0];
        let d2 = &bumps[2];

        let mut ptrs: Vec<*mut usize> = Vec::with_capacity(count_scale(200_000));
        for j in 0..count_scale(200_000) {
            let p = needy.alloc_raw::<8>(8) as *mut usize;
            unsafe { *p = 0x9000 + j };
            ptrs.push(p);
        }
        let last = *ptrs.last().unwrap() as usize;
        let in_d0 = last >= d0.ptr as usize && last < d0.ptr as usize + d0.len;
        let in_d2 = last >= d2.ptr as usize && last < d2.ptr as usize + d2.len;
        assert!(
            in_d2 && !in_d0,
            "orx+prio: первый берётся у низкоприоритетного донора 2"
        );
        for (j, p) in ptrs.iter().enumerate() {
            assert_eq!(unsafe { **p }, 0x9000 + j);
        }
    }

    // ---- boxcar + приоритет: то же самое ----

    #[test]
    fn donors_boxcar_priority_low_taken_first() {
        let arena = SharedArena::new(arena_scale(5 * 1024 * 1024));
        let mut prio = vec![0u32; 5];
        prio[0] = 100;
        prio[2] = 1; // низкий (минимальный среди доноров 0,2,4)
        prio[4] = 50; // средний
        let bumps = split_don_with_raw(
            &arena,
            5,
            DonorPolicy {
                kind: DonorListKind::Boxcar,
                use_priority: true,
                every: 2,
                priorities: Some(prio),
            },
        );
        let needy = &bumps[1];
        let d0 = &bumps[0];
        let d2 = &bumps[2];

        let mut ptrs: Vec<*mut usize> = Vec::with_capacity(count_scale(200_000));
        for j in 0..count_scale(200_000) {
            let p = needy.alloc_raw::<8>(8) as *mut usize;
            unsafe { *p = 0x8000 + j };
            ptrs.push(p);
        }
        let last = *ptrs.last().unwrap() as usize;
        let in_d0 = last >= d0.ptr as usize && last < d0.ptr as usize + d0.len;
        let in_d2 = last >= d2.ptr as usize && last < d2.ptr as usize + d2.len;
        assert!(
            in_d2 && !in_d0,
            "boxcar+prio: первый берётся у низкоприоритетного донора 2"
        );
        for (j, p) in ptrs.iter().enumerate() {
            assert_eq!(unsafe { **p }, 0x8000 + j);
        }
    }

    // Лёгкий smoke-тест для прогона под Miri (Tree Borrows) — малые объёмы,
    // чтобы проверить звуковость реестра доноров и заимствование памяти.
    #[test]
    fn donors_miri_smoke() {
        // static: забор с высокой стороны донора (маленькая арена -> переполнение)
        let arena = SharedArena::new(32 * 1024);
        let bumps = split_don_raw(&arena, 2, 2);
        let (donor, needy) = (&bumps[0], &bumps[1]);
        let mut ptrs: Vec<*mut usize> = Vec::with_capacity(3_000);
        for j in 0..3_000usize {
            let p = needy.alloc_raw::<8>(8) as *mut usize;
            unsafe { *p = 0xC000 + j };
            ptrs.push(p);
        }
        for (j, p) in ptrs.iter().enumerate() {
            assert_eq!(unsafe { **p }, 0xC000 + j);
        }
        let last = *ptrs.last().unwrap() as usize;
        let d_start = donor.ptr as usize;
        let d_end = d_start + donor.len;
        assert!(
            last >= d_start && last < d_end,
            "static: блок взят у донора"
        );

        // orx: динамическое удаление -> fallback вне арены
        let arena2 = SharedArena::new(32 * 1024);
        let bumps2 = split_don_with_raw(&arena2, 2, DonorPolicy::orx(0));
        let needy2 = &bumps2[1];
        needy2.remove_donor(0);
        let extra: Vec<*mut usize> = (0..3_000)
            .map(|j| {
                let p = needy2.alloc_raw::<8>(8) as *mut usize;
                unsafe { *p = 0xD000 + j };
                p
            })
            .collect();
        for (j, p) in extra.iter().enumerate() {
            assert_eq!(unsafe { **p }, 0xD000 + j);
        }
    }

    // ==================== Разбиение (split) и конверсия ====================

    #[test]
    fn split_produces_disjoint_subregions() {
        let (_arena, bump) = make_one(BumpDir::Forward, arena_scale(4096));
        let base = bump.ptr as usize;
        let total = bump.len;
        let children = bump.split(3, BumpDir::Forward);
        assert_eq!(children.len(), 3);
        let mut seen: Vec<(usize, usize)> = children
            .iter()
            .map(|c| (c.ptr as usize, c.ptr as usize + c.len))
            .collect();
        seen.sort();
        for w in seen.windows(2) {
            assert!(w[0].1 <= w[1].0, "части регионов пересекаются");
        }
        assert_eq!(seen[0].0, base);
        assert_eq!(seen.last().unwrap().1, base + total);
        for c in &children {
            assert!(
                c.neighbor_idx.is_none(),
                "дочерний bump не должен быть парой"
            );
            assert!(
                matches!(c.donor_reg, DonorReg::None),
                "дочерний bump не донор"
            );
            // Каждый дочерний bump выделяет независимо из своего чанка.
            let p = c.alloc_raw::<8>(8);
            assert!(p as usize >= c.ptr as usize && p as usize + 8 <= c.ptr as usize + c.len);
        }
    }

    #[test]
    fn split_by_sizes_rem_goes_to_last() {
        let (_arena, bump) = make_one(BumpDir::Forward, arena_scale(4096));
        let sizes = [16usize, 32, 64];
        let base = bump.ptr as usize;
        let children = bump.split_by_sizes(&sizes, BumpDir::Forward);
        assert_eq!(children.len(), 3);
        assert_eq!(children[0].len, 16);
        assert_eq!(children[1].len, 32);
        assert!(children[2].len >= 64, "остаток должен достаться последней");
        assert_eq!(children[0].ptr as usize, base);
        assert_eq!(children[1].ptr as usize, base + 16);
        assert_eq!(children[2].ptr as usize, base + 16 + 32);
        assert!(children[0].ptr as usize + children[0].len <= children[1].ptr as usize);
        assert!(children[1].ptr as usize + children[1].len <= children[2].ptr as usize);
    }

    #[test]
    fn split_donors_by_sizes_borrow_from_sibling() {
        let (_arena, bump) = make_one(BumpDir::Forward, arena_scale(4096));
        // [1024, 1024] — второй чанк берёт остаток региона; один — донор.
        let children = bump.split_donors_by_sizes(&[1024usize, 1024], BumpDir::Forward);
        assert_eq!(children.len(), 2);
        let (d0, needy) = (&children[0], &children[1]);
        assert!(matches!(d0.donor_reg, DonorReg::Orx(_)));
        assert!(matches!(needy.donor_reg, DonorReg::Orx(_)));
        // Донор 0 уже зарегистрирован у needy (не дублируется).
        assert!(!needy.add_donor(0, 0), "донор 0 уже в общем реестре");
        // Тяжёлая часть (реальный заём у донора) — только не под Miri; под Miri
        // пройдёт самую связку выше (маленькие арены для переполнения не хватит).
        #[cfg(not(miri))]
        {
            let mut borrowed_any = false;
            for _ in 0..2000usize {
                let p = needy.alloc_raw::<8>(8) as *mut usize;
                let a = p as usize;
                if a >= d0.ptr as usize && a < d0.ptr as usize + d0.len {
                    borrowed_any = true;
                }
            }
            assert!(borrowed_any, "needy должен занять память у донора-соседа");
        }
    }

    #[test]
    fn split_local_zones_alloc_and_reset_independently() {
        let (_arena, bump) = make_one(BumpDir::Forward, arena_scale(4096));
        let zones = bump.split_local(3, BumpDir::Forward);
        assert_eq!(zones.len(), 3);
        let mut firsts: Vec<*mut u8> = Vec::with_capacity(3);
        for z in &zones {
            // Каждая зона выделяет из своего чанка.
            let p = z.alloc::<8>(16);
            firsts.push(p);
            assert!(p as usize >= z.inner.ptr as usize);
            assert!(p as usize + 16 <= z.inner.ptr as usize + z.inner.len);
        }
        // Зона возвращает счётчик в ноль — следующий alloc даст снова первый адрес.
        zones[0].reset();
        let again = zones[0].alloc::<8>(16);
        assert_eq!(again, firsts[0], "после reset зона отдаёт назад начало");
    }

    #[test]
    fn zone_split_into_threads_use_in_threads() {
        let (_arena, bump) = make_one(BumpDir::Forward, arena_scale(4096));
        let bump = CachePadded::into_inner(bump);
        let zone = bump.into_zone();
        // Зона даёт Send-потоки: раздаём, разносим по потокам, каждый пишет.
        let threads = zone.split_into_threads(3);
        assert_eq!(threads.len(), 3);
        let handles: Vec<_> = threads
            .into_iter()
            .map(|tb| {
                std::thread::spawn(move || {
                    let p = tb.alloc_uninit_slice::<u64, 8>(1) as *mut u64;
                    unsafe { *p = 0xABCD };
                    let _ = tb;
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn into_zone_into_thread_roundtrip_preserves_state() {
        let (_arena, bump) = make_one(BumpDir::Forward, arena_scale(4096));
        let bump = CachePadded::into_inner(bump);
        let _p0 = bump.alloc_raw::<8>(16);
        let p1 = bump.alloc_raw::<8>(16);
        let used = bump.allocated_bytes();
        assert_eq!(used, 32);
        // В зону
        let zone = bump.into_zone();
        assert_eq!(
            zone.allocated_bytes(),
            used,
            "зона наследует счётчик bump'а"
        );
        let p2 = zone.alloc::<8>(16);
        assert!(
            p2 as usize > p1 as usize,
            "зона продолжает рост от того же фронта"
        );
        // Обратно в ThreadBump
        let tb = zone.into_thread();
        assert_eq!(
            tb.allocated_bytes(),
            48,
            "ThreadBump наследует счётчик зоны"
        );
        let p3 = tb.alloc_raw::<8>(16);
        assert!(p3 as usize > p2 as usize);
    }

    #[test]
    fn split_local_by_sizes_preserves_order() {
        let (_arena, bump) = make_one(BumpDir::Forward, arena_scale(4096));
        let zones = bump.split_local_by_sizes(&[64usize, 128], BumpDir::Forward);
        assert_eq!(zones.len(), 2);
        assert_eq!(zones[0].inner.len, 64);
        assert!(zones[1].inner.len >= 128, "остаток уходит последней зоне");
        assert_eq!(
            zones[1].inner.ptr as usize,
            zones[0].inner.ptr as usize + 64,
            "вторая зона начинается сразу после первой"
        );
        let p = zones[0].alloc::<8>(32);
        assert!(p as usize >= zones[0].inner.ptr as usize);
        assert!(p as usize + 32 <= zones[0].inner.ptr as usize + zones[0].inner.len);
    }

    /// При drop `ArenaVec` для каждого живого элемента обязан вызываться его
    /// деструктор (через `drop_in_place`).
    #[test]
    fn arena_vec_drop_calls_element_destructors() {
        use std::sync::atomic::AtomicUsize;
        struct DropCounter(Arc<AtomicUsize>);
        impl Drop for DropCounter {
            fn drop(&mut self) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let count = Arc::new(AtomicUsize::new(0));
        let (_arena, bump) = make_one(BumpDir::Forward, arena_scale(1 << 20));
        {
            let mut v: ArenaVec<DropCounter> = ArenaVec::with_capacity_in::<1>(8, &bump);
            for _ in 0..5 {
                v.push::<1>(DropCounter(Arc::clone(&count)), &bump);
            }
            assert_eq!(v.as_slice().len(), 5);
            // Пока вектор жив, элементы не разрушены.
            assert_eq!(count.load(std::sync::atomic::Ordering::Relaxed), 0);
        } // Здесь `drop(v)` должен вызвать деструкторы всех 5 элементов.
        assert_eq!(count.load(std::sync::atomic::Ordering::Relaxed), 5);
    }

    // ============================================================
    //      Покрытие: split-обёртки, Split, мономорфные пути, зоны
    // ============================================================

    #[test]
    fn donor_policy_with_priority() {
        let p = DonorPolicy::static_(2).with_priority();
        assert!(p.use_priority);
        assert_eq!(p.every, 2);
        assert!(p.priorities.is_none());
        let po = DonorPolicy::orx(1).with_priority();
        assert!(po.use_priority);
        let pb = DonorPolicy::boxcar(1).with_priority();
        assert!(pb.use_priority);
        // Без with_priority — приоритет выключен.
        let base = DonorPolicy::static_(1);
        assert!(!base.use_priority);
    }

    #[test]
    fn split_safe_api_and_split_methods() {
        use std::ops::Deref;
        let arena = SharedArena::new(arena_scale(1 << 16));
        let s = arena.split_safe(3);
        assert_eq!(s.len(), 3);
        assert!(!s.is_empty());
        assert_eq!(s.bumps().len(), 3);
        assert_eq!(s.as_slice().len(), 3);
        assert!(s.get(0).is_some());
        assert!(s.get(99).is_none());
        // Deref → &[CachePadded<ThreadBump>]
        assert_eq!(s.deref().len(), 3);
        assert_eq!((&*s).len(), 3);
        // Кусочки непересекающиеся по адресам.
        let a = s.bumps()[0].ptr as usize;
        let b = s.bumps()[1].ptr as usize;
        assert_ne!(a, b);

        // split_with_safe (явное направление)
        let s2 = arena.split_with_safe(2, BumpDir::Backward);
        assert_eq!(s2.len(), 2);
        assert_eq!(s2[0].dir, BumpDir::Backward);

        // unsafe split (обёртка)
        let raw = split_with_raw(&arena, 2, BumpDir::Forward);
        assert_eq!(raw.len(), 2);

        // alternating / paired / donors safe
        let s3 = arena.split_alternating_safe(4);
        assert_eq!(s3.len(), 4);
        let s4 = arena.split_paired_safe(2);
        assert_eq!(s4.len(), 2);
        let s5 = arena.split_donors_safe(2, 1);
        assert_eq!(s5.len(), 2);
        let s6 = arena.split_donors_with_safe(2, DonorPolicy::static_(1));
        assert_eq!(s6.len(), 2);
    }

    #[test]
    fn threadbump_mono_alloc_all_dirs() {
        use std::ptr;
        // Forward fast-path (без пары/доноров).
        let (_a1, b1) = make_one(BumpDir::Forward, arena_scale(1 << 14));
        let p1 = b1.alloc_uninit_slice_m::<u64, 8, { ThreadBump::MODE_DIR_FORWARD }>(4);
        assert!(!p1.is_null());
        unsafe {
            ptr::write(p1, 7u64);
            assert_eq!(*p1, 7);
        }

        // Backward fast-path.
        let (_a2, b2) = make_one(BumpDir::Backward, arena_scale(1 << 14));
        let p2 = b2.alloc_uninit_slice_m::<u64, 8, { ThreadBump::MODE_DIR_BACKWARD }>(4);
        assert!(!p2.is_null());

        // MiddleOut.
        let (_a3, b3) = make_one(BumpDir::MiddleOut, arena_scale(1 << 14));
        let p3 = b3.alloc_uninit_slice_m::<u64, 8, { ThreadBump::MODE_DIR_MIDDLEOUT }>(4);
        assert!(!p3.is_null());

        // Pair (forward): малая аллокация умещается в свою половину.
        let (_a4, b4) = make_one(BumpDir::Forward, arena_scale(1 << 14));
        let p4 =
            b4.alloc_uninit_slice_m::<u64, 8, { ThreadBump::MODE_PAIR | ThreadBump::MODE_DIR_FORWARD }>(
                2,
            );
        assert!(!p4.is_null());

        // Pair backward.
        let (_a5, b5) = make_one(BumpDir::Backward, arena_scale(1 << 14));
        let p5 = b5.alloc_uninit_slice_m::<u64, 8, {
            ThreadBump::MODE_PAIR | ThreadBump::MODE_DIR_BACKWARD
        }>(2);
        assert!(!p5.is_null());

        // Донорский режим: без реестра переполнение уходит в grow_fallback.
        let (_a6, b6) = make_one(BumpDir::Forward, arena_scale(1 << 12));
        let p6 = b6.alloc_uninit_slice_m::<u64, 8, {
            ThreadBump::MODE_DONOR_STATIC | ThreadBump::MODE_DIR_FORWARD
        }>(8);
        assert!(!p6.is_null());
        // Исчерпываем регион → уходит в grow_fallback (возвращает указатель).
        let _fp = b6.alloc_uninit_slice_m::<u64, 8, {
            ThreadBump::MODE_DONOR_STATIC | ThreadBump::MODE_DIR_FORWARD
        }>(arena_scale(1 << 12));
    }

    #[test]
    fn threadbump_split_donors_dynamic() {
        let (_arena, bump) = make_one(BumpDir::Forward, arena_scale(1 << 16));
        let children = bump.split_donors(4, BumpDir::Forward);
        assert_eq!(children.len(), 4);
        let p = children[0].alloc_raw::<8>(16);
        assert!(!p.is_null());
        let p1 = children[1].alloc_raw::<8>(16);
        assert!(!p1.is_null());
    }

    #[test]
    fn zone_bump_dir_split_and_fallback() {
        let (_arena, bump) = make_one(BumpDir::Backward, arena_scale(1 << 16));
        let tb = CachePadded::into_inner(bump);
        let zone = tb.into_zone();
        assert_eq!(zone.dir(), BumpDir::Backward);
        assert_eq!(zone.allocated_bytes(), 0);

        // Разбиение на суб-зоны; каждая аллоцирует из своего чанка.
        let subs = zone.split(3);
        assert_eq!(subs.len(), 3);
        for z in &subs {
            let p = z.alloc::<8>(16);
            assert!(!p.is_null());
        }

        // grow_fallback: регион зоны исчерпаем — берём свежий fallback-чанк.
        let big = zone.grow_fallback(256);
        assert!(!big.is_null());

        // Проверяем reset — счётчики обнуляются.
        let p_a = zone.alloc::<8>(16);
        zone.reset();
        let p_b = zone.alloc::<8>(16);
        assert_eq!(p_a, p_b, "после reset зона отдаёт назад тот же адрес");
    }

    #[test]
    fn zone_split_by_sizes_and_into_threads_by_sizes() {
        let (_arena, bump) = make_one(BumpDir::Forward, arena_scale(1 << 14));
        let tb = CachePadded::into_inner(bump);

        // ZoneBump::split_by_sizes
        let zone = tb.into_zone();
        let zones = zone.split_by_sizes(&[64usize, 128]);
        assert_eq!(zones.len(), 2);
        assert_eq!(zones[0].inner.len, 64);
        assert!(zones[1].inner.len >= 128, "остаток уходит последней зоне");
        let zptr = zones[0].alloc::<8>(16);
        assert!(!zptr.is_null());

        // ZoneBump::split_into_threads_by_sizes → Send ThreadBump'ы.
        let threads = zone.split_into_threads_by_sizes(&[64usize, 128]);
        assert_eq!(threads.len(), 2);
        let tptr = threads[1].alloc_raw::<8>(16);
        assert!(!tptr.is_null());
    }

    #[test]
    fn arena_vec_mono_methods() {
        let (_arena, bump) = make_one(BumpDir::Forward, arena_scale(1 << 16));
        const M: u32 = ThreadBump::MODE_DIR_FORWARD;

        // with_capacity_in_m + push_m (+ grow_m при переполнении cap).
        let mut v: ArenaVec<u64> = ArenaVec::with_capacity_in_m::<8, M>(2, &bump);
        v.push_m::<8, M>(10, &bump);
        v.push_m::<8, M>(20, &bump);
        v.push_m::<8, M>(30, &bump); // cap 2 -> 4 через grow_m
        v.push_m::<8, M>(40, &bump);
        v.push_m::<8, M>(50, &bump); // cap 4 -> 8 через grow_m
        assert_eq!(v.as_slice(), &[10, 20, 30, 40, 50]);

        // with_capacity_in_m(0) — пустой без аллокации.
        let z: ArenaVec<u64> = ArenaVec::with_capacity_in_m::<8, M>(0, &bump);
        assert_eq!(z.as_slice().len(), 0);

        // from_slice_in_m
        let f: ArenaVec<u64> = ArenaVec::from_slice_in_m::<8, M>(&[1, 2, 3], &bump);
        assert_eq!(f.as_slice(), &[1, 2, 3]);

        // Немономорфный grow (cap 0 -> 4).
        let mut g: ArenaVec<u64> = ArenaVec::with_capacity_in::<8>(0, &bump);
        g.grow::<8>(&bump);
        assert_eq!(g.as_slice().len(), 0);
        g.grow::<8>(&bump);
        assert_eq!(g.as_slice().len(), 0);
    }

    #[test]
    fn arena_string_mono_and_display() {
        let (_arena, bump) = make_one(BumpDir::Forward, arena_scale(1 << 14));
        const M: u32 = ThreadBump::MODE_DIR_FORWARD;

        let s = ArenaString::from_str_in_m::<M>("hello", &bump);
        // Deref → str + Display
        assert_eq!(&*s, "hello");
        assert_eq!(format!("{}", s), "hello");
        assert_eq!(s.len(), 5);
        assert!(s.starts_with("hel"));

        // Немономорфная версия тоже покрыта.
        let s2 = ArenaString::from_str_in("world", &bump);
        assert_eq!(&*s2, "world");
        assert_eq!(format!("{}", s2), "world");
    }

    #[test]
    #[cfg(all(windows, not(miri)))]
    fn platform_lock_memory_and_precommitted() {
        let _ = platform::large_pages_precommitted();
        let p = platform::alloc_normal(4096);
        // VirtualLock может вернуть false (нет привилегии) — но не должен падать.
        let _ = platform::lock_memory(p.ptr, p.size);
        platform::free(p);
    }

    // =====================================================================
    //                    COVERAGE PUSH: remaining uncovered branches
    // =====================================================================

    #[test]
    fn cov_verbose_println_arena_new() {
        unsafe { std::env::set_var("R3_VERBOSE", "1") };
        let _a = SharedArena::new(4096);
        unsafe { std::env::remove_var("R3_VERBOSE") };
    }

    #[test]
    fn cov_unsafe_split_wrapper() {
        let arena = SharedArena::new(arena_scale(1 << 14));
        let v = unsafe { arena.split(2) };
        assert_eq!(v.len(), 2);
        let p = v[0].alloc_raw::<8>(16);
        assert!(!p.is_null());
    }

    #[test]
    fn cov_paired_odd_single_chunk() {
        let arena = SharedArena::new(arena_scale(1 << 14));
        let v = split_pair_raw(&arena, 3);
        assert_eq!(v.len(), 3);
        let _ = v[0].alloc_raw::<1>(16);
        let _ = v[1].alloc_raw::<1>(16);
        let _ = v[2].alloc_raw::<1>(16);
    }

    #[test]
    fn cov_arena_vec_push_grow_with_data() {
        let (_arena, bump) = make_one(BumpDir::Forward, arena_scale(1 << 16));
        let mut v: ArenaVec<u64> = ArenaVec::with_capacity_in::<8>(2, &bump);
        v.push::<8>(1, &bump);
        v.push::<8>(2, &bump);
        v.push::<8>(3, &bump);
        v.push::<8>(4, &bump);
        v.push::<8>(5, &bump);
        assert_eq!(v.as_slice(), &[1u64, 2, 3, 4, 5]);
    }

    #[test]
    fn cov_zone_alloc_forward_overflow_two_chunks() {
        let (_a, bump) = make_one(BumpDir::Forward, arena_scale(1 << 12));
        let tb = CachePadded::into_inner(bump);
        let zone = tb.into_zone();
        let _ = zone.alloc::<8>(zone.inner.len);
        let _ = zone.alloc::<8>(16);
        let p2 = zone.alloc::<8>(16);
        assert!(!p2.is_null());
    }

    #[test]
    fn cov_zone_alloc_backward_overflow_two_chunks() {
        let (_a, bump) = make_one(BumpDir::Backward, arena_scale(1 << 12));
        let tb = CachePadded::into_inner(bump);
        let zone = tb.into_zone();
        let _ = zone.alloc::<8>(zone.inner.len);
        let _ = zone.alloc::<8>(16);
        let p2 = zone.alloc::<8>(16);
        assert!(!p2.is_null());
    }

    #[test]
    fn cov_zone_alloc_middleout_both_sides_and_fallback() {
        let (_a, bump) = make_one(BumpDir::MiddleOut, arena_scale(1 << 16));
        let tb = CachePadded::into_inner(bump);
        let zone = tb.into_zone();
        let mid = zone.inner.len / 2;
        let size = 64usize;
        // Чередование сторон: левая/правая половины заполняются симметрично.
        // Останавливаемся чуть раньше заполнения, чтобы не упираться в
        // grow_fallback (который в MiddleOut-режиме упирается в дефект
        // prefetch-адреса на противоположной стороне).
        for _ in 0..(mid / size * 2 - 4) {
            let _ = zone.alloc::<8>(size);
        }
    }

    #[test]
    fn cov_alloc_uninit_slice_zero() {
        let (_arena, bump) = make_one(BumpDir::Forward, arena_scale(1 << 14));
        let p = bump.alloc_uninit_slice::<u64, 8>(0);
        assert!(p.is_null());
    }

    #[test]
    fn cov_prefault_local_middleout_right_half() {
        let (_arena, bump) = make_one(BumpDir::MiddleOut, arena_scale(1 << 14));
        let _ = bump.alloc_raw::<1>(16);
        let _ = bump.alloc_raw::<1>(16);
        bump.prefault_local();
    }

    #[test]
    fn cov_prefault_local_backward_after_alloc() {
        let (_arena, bump) = make_one(BumpDir::Backward, arena_scale(1 << 14));
        let _ = bump.alloc_raw::<1>(1024);
        bump.prefault_local();
        bump.reset();
        bump.prefault_local();
    }

    #[test]
    fn cov_add_remove_donor_none() {
        let (_arena, bump) = make_one(BumpDir::Forward, arena_scale(1 << 14));
        assert!(!bump.add_donor(0, 0));
        assert!(!bump.remove_donor(0));
    }

    #[test]
    fn cov_add_remove_donor_static() {
        let arena = SharedArena::new(arena_scale(1 << 16));
        let v = split_don_with_raw(&arena, 4, DonorPolicy::static_(2));
        let donor = &v[0];
        assert!(!donor.add_donor(1, 0));
        assert!(!donor.remove_donor(1));
    }

    #[test]
    fn cov_add_donor_duplicate_orx() {
        let arena = SharedArena::new(arena_scale(1 << 16));
        let v = split_don_with_raw(&arena, 4, DonorPolicy::orx(2));
        let donor = &v[0];
        assert!(donor.add_donor(1, 0));
        assert!(!donor.add_donor(1, 0));
    }

    #[test]
    fn cov_add_donor_duplicate_boxcar() {
        let arena = SharedArena::new(arena_scale(1 << 16));
        let v = split_don_with_raw(&arena, 4, DonorPolicy::boxcar(2));
        let donor = &v[0];
        assert!(donor.add_donor(1, 0));
        assert!(!donor.add_donor(1, 0));
    }

    #[test]
    fn cov_remove_donor_not_found_orx() {
        let arena = SharedArena::new(arena_scale(1 << 16));
        let v = split_don_with_raw(&arena, 4, DonorPolicy::orx(2));
        let donor = &v[0];
        assert!(!donor.remove_donor(99));
    }

    #[test]
    fn cov_remove_donor_not_found_boxcar() {
        let arena = SharedArena::new(arena_scale(1 << 16));
        let v = split_don_with_raw(&arena, 4, DonorPolicy::boxcar(2));
        let donor = &v[0];
        assert!(!donor.remove_donor(99));
    }

    #[test]
    fn cov_typed_arena_concurrent_cas_retry() {
        use std::sync::{Arc, Barrier};
        // Один большой первичный регион: все потоки мапятся на region 0 и
        // одновременно бьют по одному и тому же счётчику `used` → CAS retry
        // (typed_arena.rs:158) срабатывает наверняка.
        let arena = Arc::new(Arena::<u64>::with_regions(1, 200_000));
        let barrier = Arc::new(Barrier::new(8));
        std::thread::scope(|s| {
            for _ in 0..8 {
                let a = arena.clone();
                let b = barrier.clone();
                s.spawn(move || {
                    b.wait();
                    for i in 0..10_000u64 {
                        let v = a.alloc(i);
                        unsafe { std::ptr::write_volatile(v as *mut u64, i) };
                    }
                });
            }
        });
    }

    #[test]
    fn cov_runtime_backward_pair_donor_fallback() {
        let arena = SharedArena::new(arena_scale(1 << 14));
        let bumps = unsafe { arena.split_donors(4, 2) };
        bumps[0].add_donor(1, 0);
        bumps[1].add_donor(0, 0);
        for _ in 0..arena_scale(1 << 14) / 16 / 2 {
            let _ = bumps[0].alloc_raw::<1>(16);
            let _ = bumps[1].alloc_raw::<1>(16);
        }
        let _ = bumps[0].alloc_raw::<1>(16);
        let _ = bumps[1].alloc_raw::<1>(16);
    }

    #[test]
    fn cov_take_from_registry_priority_low_better() {
        let arena = SharedArena::new(arena_scale(1 << 16));
        let mut bumps = unsafe { arena.split_donors(4, 2) };
        bumps[0].add_donor(1, 100);
        bumps[0].add_donor(2, 1);
        bumps[1].add_donor(0, 0);
        bumps[1].use_priority = true;
        let _ = bumps[1].alloc_raw::<8>(arena_scale(1 << 14));
        let _ = bumps[2].alloc_raw::<8>(arena_scale(1 << 14));
        let p = bumps[1].try_take_from_donors::<8>(16);
        assert!(p.is_some());
    }

    #[test]
    fn cov_remove_donor_orx_and_boxcar_real() {
        let arena = SharedArena::new(arena_scale(1 << 16));
        let v_orx = split_don_with_raw(&arena, 4, DonorPolicy::orx(2));
        v_orx[0].add_donor(1, 0);
        assert!(v_orx[0].remove_donor(1));
        assert!(!v_orx[0].remove_donor(1));
        let v_bc = split_don_with_raw(&arena, 4, DonorPolicy::boxcar(2));
        v_bc[0].add_donor(1, 0);
        assert!(v_bc[0].remove_donor(1));
        assert!(!v_bc[0].remove_donor(1));
    }

    #[test]
    fn cov_donor_backward_has_space_and_take_success() {
        // Донор-Backward заполняет высокую половину; чужая (нижняя) половина
        // [0, lo) отдаётся заёмщику. Проверяем donor_has_space (backward) и
        // успешный take_from_donor по backward-ветке.
        let (_arena, bump) = make_one(BumpDir::Backward, arena_scale(1 << 16));
        let mut bumps = bump.split_donors(4, BumpDir::Backward);
        bumps[1].add_donor(0, 0);
        bumps[1].use_priority = true;
        // donor_has_space(backward) == true
        assert!(bumps[1].donor_has_space::<8>(0, 16));
        // take_from_donor (backward) — успешный CAS
        let p = bumps[1].try_take_from_donors::<8>(16);
        assert!(p.is_some());
    }

    #[test]
    fn cov_donor_backward_exhausted_returns_none() {
        // Заём размером больше свободной нижней половины backward-донора →
        // ветка `end > d.len - hi` → None (без CAS).
        let (_arena, bump) = make_one(BumpDir::Backward, arena_scale(1 << 14));
        let bumps = bump.split_donors(4, BumpDir::Backward);
        bumps[1].add_donor(0, 0);
        // У донора 0 регион [*, chunk); берём больше половины его размера.
        let big = arena_scale(1 << 14) / 4 + 1;
        let p = bumps[1].try_take_from_donors::<8>(big);
        assert!(p.is_none());
    }

    #[test]
    fn cov_forward_pair_huge_alloc_panics_oom() {
        // Forward bump в паре (общий регион), огромный блок больше всей пары →
        // своя половина полна и заём у соседа невозможен → OOM panic.
        let arena = SharedArena::new(arena_scale(1 << 14));
        let v = split_pair_raw(&arena, 2);
        let huge = arena_scale(1 << 14) * 2; // больше всего объединённого региона
        let res = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            let _ = v[1].alloc_raw::<1>(huge); // v[1] — Forward bump пары
        }));
        assert!(res.is_err());
    }

    #[test]
    fn cov_backward_pair_huge_alloc_panics_oom() {
        // Backward bump в паре, огромный блок → OOM panic.
        let arena = SharedArena::new(arena_scale(1 << 14));
        let v = split_pair_raw(&arena, 2);
        let huge = arena_scale(1 << 14) * 2;
        let res = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            let _ = v[0].alloc_raw::<1>(huge); // v[0] — Backward bump пары
        }));
        assert!(res.is_err());
    }

    #[test]
    fn cov_middleout_right_panic_oom() {
        // MiddleOut: маленький alloc влево, затем огромный — правая сторона
        // (side=true) не помещается → OOM panic на правой ветке.
        let (_arena, bump) = make_one(BumpDir::MiddleOut, arena_scale(1 << 14));
        let _ = bump.alloc_raw::<1>(16); // левая сторона, ok
        let huge = arena_scale(1 << 14) * 2;
        let res = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            let _ = bump.alloc_raw::<1>(huge); // правая сторона → panic
        }));
        assert!(res.is_err());
    }

    #[test]
    fn cov_forward_donor_full_then_grow_fallback() {
        // Forward bump с донорами: своя половина переполнена и доноры пусты →
        // grow_fallback (возвращает null при отсутствии запасных чанков).
        let arena = SharedArena::new(arena_scale(1 << 14));
        let v = split_don_with_raw(&arena, 2, DonorPolicy::static_(2));
        // Переполняем регион v[1]: запрашиваем блок больше его региона.
        let big = arena_scale(1 << 14);
        let _ = v[1].alloc_raw::<1>(big);
        let _ = v[1].alloc_raw::<1>(big + 1);
    }

    #[test]
    fn cov_backward_donor_mid_zero() {
        // Backward bump с донорами и без соседа → mid = 0 (ветка else на 1298).
        let (_arena, bump) = make_one(BumpDir::Backward, arena_scale(1 << 14));
        let children = bump.split_donors(2, BumpDir::Backward);
        let _ = children[1].alloc_raw::<1>(16);
        let _ = children[1].alloc_raw::<1>(16);
    }

    #[test]
    fn cov_pair_full_try_borrow_forward_none_and_backward_panic() {
        // Пара: v[0]=Backward, v[1]=Forward, общий регион [0, 2cs), mid=cs.
        // Заполняем обе половины; переполнение v[0] (Backward) → try_borrow у
        // v[1] (Forward, полон) → None (1610) → OOM panic (1331).
        let arena = SharedArena::new(arena_scale(1 << 16));
        let v = split_pair_raw(&arena, 2);
        let cs = arena_scale(1 << 16) / 2; // половина пары
        // Заполняем низ Forward bump'а v[1] полностью.
        let _ = v[1].alloc_raw::<1>(cs);
        // Заполняем верх Backward bump'а v[0] полностью.
        let _ = v[0].alloc_raw::<1>(cs);
        let res = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            let _ = v[0].alloc_raw::<1>(16); // некуда: своя пол. полна и сосед полон
        }));
        assert!(res.is_err());
    }

    #[test]
    fn cov_pair_full_try_borrow_backward_none_and_forward_panic() {
        // Пара: переполнение v[1] (Forward) → try_borrow у v[0] (Backward, полон)
        // → None (1631) → OOM panic (1269).
        let arena = SharedArena::new(arena_scale(1 << 16));
        let v = split_pair_raw(&arena, 2);
        let cs = arena_scale(1 << 16) / 2;
        let _ = v[1].alloc_raw::<1>(cs);
        let _ = v[0].alloc_raw::<1>(cs);
        let res = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            let _ = v[1].alloc_raw::<1>(16);
        }));
        assert!(res.is_err());
    }

    #[test]
    fn cov_forward_donor_full_grow_fallback() {
        // Forward bump с донором: своя половина переполнена, донор тоже пуст →
        // ветка try_take_from_donors → grow_fallback (1325-1329).
        let arena = SharedArena::new(arena_scale(1 << 16));
        let v = split_don_with_raw(&arena, 2, DonorPolicy::static_(2));
        let cs = arena_scale(1 << 16) / 2;
        // Заполняем region v[1] до конца.
        let _ = v[1].alloc_raw::<1>(cs);
        // Переполнение: сначала донор v[0] отдаёт свою чужую (верхнюю) половину,
        // затем она исчерпывается → try_take_from_donors → None → grow_fallback.
        let size = 64usize;
        let iters = (cs / size as usize * 4) as usize + 64;
        for _ in 0..iters {
            let _ = v[1].alloc_raw::<1>(size);
        }
    }

    #[test]
    fn cov_pair_cas_contention() {
        // Много потоков бьют по одному и тому же bump'у пары одновременно → CAS
        // retry в alloc_raw_m (Forward 1257, Backward 1322).
        use std::sync::{Arc, Barrier};
        let arena = SharedArena::new(arena_scale(1 << 18));
        let v = split_pair_raw(&arena, 2);
        // Общий регион пары = arena_scale(1<<18). 8 потоков х 200 х 2 аллокации
        // по 64 B ~= arena_scale(1<<18)/... — не переполняем, но устойчиво
        // соперничаем за общий счётчик (CAS retry в обоих bump'ах пары).
        let barrier = Arc::new(Barrier::new(8));
        let (even, odd) = (&v[0], &v[1]);
        std::thread::scope(|s| {
            for _ in 0..8 {
                let br = barrier.clone();
                s.spawn(move || {
                    br.wait();
                    for _ in 0..200 {
                        let _ = odd.alloc_raw::<1>(64);
                        let _ = even.alloc_raw::<1>(64);
                    }
                });
            }
        });
    }

    #[test]
    fn cov_donor_take_cas_contention() {
        // Доноры 0,2 (every=2) остаются пустыми; заёмщики 1,3 переполняют свои
        // регионы и одновременно берут у общих доноров 0 и 2 → CAS retry в
        // take_from_donor (Forward 1778) и не-приоритетная ветка
        // take_from_registry (1706). Orx-реестр без приоритета.
        use std::sync::{Arc, Barrier};
        let arena = SharedArena::new(arena_scale(1 << 18));
        let f = split_don_with_raw(&arena, 4, DonorPolicy::orx(2));
        let cs = arena_scale(1 << 18) / 4;
        let barrier = Arc::new(Barrier::new(2));
        let needy = (&f[1], &f[3]);
        std::thread::scope(|s| {
            for i in 0..2 {
                let b = if i == 0 { needy.0 } else { needy.1 };
                let br = barrier.clone();
                s.spawn(move || {
                    br.wait();
                    // Сначала заполняем собственный регион.
                    for _ in 0..cs / 16 {
                        let _ = b.alloc_raw::<1>(16);
                    }
                    // Затем заимствуем у общих доноров (конкуренция за CAS).
                    for _ in 0..2000 {
                        let _ = b.alloc_raw::<1>(16);
                    }
                });
            }
        });
    }

    #[test]
    fn cov_neighbor_borrow_cas_contention() {
        // Пара: оба собственные половины заполнены; переполнение одного соседа
        // заставляет несколько потоков одновременно заимствовать у общего соседа
        // → CAS retry в try_borrow (Forward-сосед 1622, Backward-сосед 1646).
        use std::sync::{Arc, Barrier};
        // --- v[0]=Backward переполняется, заимствует у Forward-соседа v[1] (1622) ---
        let a1 = SharedArena::new(arena_scale(1 << 18));
        let v = split_pair_raw(&a1, 2);
        let cs = arena_scale(1 << 18) / 2;
        // Заполняем собственную половину v[0] (Backward) полностью.
        let _ = v[0].alloc_raw::<1>(cs);
        let barrier = Arc::new(Barrier::new(8));
        let (even, odd) = (&v[0], &v[1]);
        std::thread::scope(|s| {
            for _ in 0..8 {
                let br = barrier.clone();
                s.spawn(move || {
                    br.wait();
                    for _ in 0..200 {
                        let _ = even.alloc_raw::<1>(64); // заимствование у odd (Forward)
                    }
                });
            }
        });
        let _ = odd;
        // --- v[1]=Forward переполняется, заимствует у Backward-соседа (1646) ---
        let a2 = SharedArena::new(arena_scale(1 << 18));
        let w = split_pair_raw(&a2, 2);
        let _ = w[1].alloc_raw::<1>(cs); // собственная половина Forward полна
        let (even2, odd2) = (&w[0], &w[1]);
        let b2 = Arc::new(Barrier::new(8));
        std::thread::scope(|s| {
            for _ in 0..8 {
                let br = b2.clone();
                s.spawn(move || {
                    br.wait();
                    for _ in 0..200 {
                        let _ = odd2.alloc_raw::<1>(64); // заимствование у even2 (Backward)
                    }
                });
            }
        });
        let _ = even2;
    }

    #[test]
    fn cov_donor_none_non_priority() {
        // Orx-реестр без приоритета, все доноры исчерпаны → take_from_registry
        // не-приоритетная ветка проходит весь цикл и возвращает None (1706/1708).
        let arena = SharedArena::new(arena_scale(1 << 18));
        let f = split_don_with_raw(&arena, 2, DonorPolicy::orx(2));
        // fill донор (0) полностью, потом needy (1) свой + донор → исчерпание.
        let d = &f[0];
        let n = &f[1];
        let cs = arena_scale(1 << 18) / 2;
        // Донор пуст, но needy забирает у него всю его «чужую» половину и сам
        // заполняет свою — к концу оба исчерпаны → None.
        let _ = n.alloc_raw::<1>(cs); // needy fill свой
        for _ in 0..cs / 16 + 100 {
            let _ = n.alloc_raw::<1>(16); // берёт у донора, пока не исчерпает
        }
        let _ = d;
    }
}

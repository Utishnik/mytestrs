// ============================================================
//     TYPED SHARED ARENA (аналог typed-arena, multi-chunk)
// ============================================================
// Вынесен в отдельный файл от «обычной» shared-арены (lib.rs).
//
// `Arena<T>` — арена объектов одного типа `T`, реализованная поверх общего
// буфера (`SharedArena`), разбитого на per-thread регионы. Каждый поток
// бампает в своём регионе без блокировок (атомарный счётчик `used`);
// "shared" = общий большой буфер, разделяемый между потоками.
//
// Публичная поверхность повторяет API крейта typed-arena (v2.0.2):
// new / with_capacity / alloc / alloc_extend / len / into_vec / iter /
// iter_mut / clear / Default / (unsafe) alloc_uninitialized + вспомогательные.
//
// Модель регионов: каждый поток имеет первичный регион из общего буфера
// (primary). Если он исчерпан, поток берёт дополнительный "spill"-регион
// (выделяется платформенным аллокатором) и продолжает туда. Все регионы
// (primary + spill) отслеживаются, поэтому глобальные операции
// (len/iter/iter_mut/into_vec/clear/Drop) корректно обходят ВСЕ элементы,
// даже после роста. Глобальные операции требуют эксклюзивного доступа
// (все потоки, работавшие с ареной, завершились) — так же, как `&mut`-методы
// typed-arena.

use super::*;

/// Один непрерывный регион памяти для бампа одному потоку (Forward).
struct ArenaRegion {
    ptr: *mut u8,
    len: usize,
    /// Байт, занято начиная с `ptr` (атомарный bump текущего потока).
    used: AtomicUsize,
}

// Thread-local: индекс первичного региона для текущего потока. Кэш
// валиден только для той же арены (сверяем по адресу первого региона).
thread_local! {
    static SLOT: Cell<(usize, usize)> = const { Cell::new((usize::MAX, usize::MAX)) };
}

pub struct Arena<T> {
    /// Общий буфер (владеет памятью primary-регионов). Поле живёт только для
    /// владения: `SharedArena` освобождает первичный буфер в своём `Drop`.
    #[allow(dead_code)]
    arena: SharedArena,
    /// Первичные per-thread регионы (стабильный вектор, не переезжает).
    primary: Vec<ArenaRegion>,
    /// Дополнительные регионы (spill), когда первичный исчерпан.
    spill: SpinMutex<Vec<ArenaRegion>>,
    /// Круговой счётчик раздачи первичных регионов потокам.
    next_slot: AtomicUsize,
    /// Суммарный размер общего буфера (для capacity()).
    total_bytes: usize,
    _pd: PhantomData<T>,
}

// Арена владеет memory; регионы — независимые непересекающиеся области,
// каждая работает только со своим `used` (atomic). Send+Sync корректны.
unsafe impl<T: Send> Send for Arena<T> {}
unsafe impl<T: Send> Sync for Arena<T> {}

impl<T> Arena<T> {
    /// Создать арену с ёмкостью по умолчанию: 1 регион на 64 элемента `T`.
    /// Для нескольких регионов используйте `with_regions`.
    pub fn new() -> Self {
        Self::with_regions(1, 64)
    }

    /// Создать арену с суммарной ёмкостью на `n` элементов `T` в 1 регионе.
    /// Для нескольких регионов используйте `with_regions`.
    pub fn with_capacity(n: usize) -> Self {
        Self::with_regions(1, n)
    }

    /// Гибкий конструктор: ровно `regions` первичных per-thread регионов,
    /// каждый рассчитан на `elements_per_region` элементов `T`. Позволяет
    /// явно задавать число регионов и их размер, тестировать переполнение
    /// регионов и spill-регионы.
    pub fn with_regions(regions: usize, elements_per_region: usize) -> Self {
        let per_region_bytes = elements_per_region * core::mem::size_of::<T>().max(1);
        Self::with_regions_bytes(regions.max(1), per_region_bytes)
    }

    /// Внутренний конструктор: `regions` регионов, каждый шириной
    /// `per_region_bytes` байт.
    fn with_regions_bytes(regions: usize, per_region_bytes: usize) -> Self {
        // Гарантируем, что первичные регионы разбиваются корректно при ЛЮБОМ
        // числе регионов/размерах: делаем размер каждого чанка кратным 16 и
        // общий размер = chunk * regions. Иначе `split_with` при мелких аренах
        // даёт перекрывающиеся/выпадающие чанки.
        let chunk = per_region_bytes.next_multiple_of(16).max(16);
        let actual_total = chunk * regions;
        let arena = SharedArena::new(actual_total);
        // Один первичный регион на поток, направление Forward.
        let bumps = unsafe { arena.split_with(regions, BumpDir::Forward) };
        let primary = bumps
            .into_iter()
            .map(|b| ArenaRegion {
                // Первичный регион живёт, пока жив `arena`.
                ptr: b.ptr,
                len: b.len,
                used: AtomicUsize::new(0),
            })
            .collect::<Vec<_>>();
        Arena {
            arena,
            primary,
            spill: SpinMutex::new(Vec::new()),
            next_slot: AtomicUsize::new(0),
            total_bytes: actual_total,
            _pd: PhantomData,
        }
    }

    /// Число первичных (per-thread) регионов.
    pub fn regions(&self) -> usize {
        self.primary.len()
    }

    /// Первый регион как идентификатор арены (для thread-local кэша) —
    /// по адресу первичного буфера.
    #[inline]
    fn arena_key(&self) -> usize {
        self.primary.first().map(|r| r.ptr as usize).unwrap_or(0)
    }

    /// Индекс первичного региона текущего потока (кэш в thread-local).
    #[inline]
    fn thread_slot(&self) -> usize {
        let n = self.primary.len().max(1);
        let key = self.arena_key();
        let slot = SLOT.with(|c| c.get());
        let idx = if slot.0 == key && slot.1 < n {
            slot.1
        } else {
            let idx = self.next_slot.fetch_add(1, Ordering::Relaxed) % n;
            SLOT.with(|c| c.set((key, idx)));
            idx
        };
        idx
    }

    /// Forward-bump в регионе: вернуть выровненный указатель на `size` байт,
    /// либо `None`, если в регионе нет места.
    #[inline]
    fn region_alloc(region: &ArenaRegion, size: usize, align: usize) -> Option<*mut u8> {
        let base = region.ptr as usize;
        let cap = region.len;
        let mut cur = region.used.load(Ordering::Relaxed);
        loop {
            let addr = (base + cur + align - 1) & !(align - 1);
            let new_cur = (addr - base).checked_add(size)?;
            if new_cur > cap {
                return None;
            }
            match region.used.compare_exchange_weak(
                cur,
                new_cur,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(addr as *mut u8),
                Err(actual) => cur = actual,
            }
        }
    }

    /// Размер нового spill-региона: не меньше двойного типичного первичного
    /// региона, кратно странице.
    fn spill_size(&self) -> usize {
        let primary_avg = self.total_bytes / self.primary.len().max(1) * 2;
        let page = platform::page_size();
        primary_avg.max(page).next_multiple_of(page)
    }

    /// Занять spill-регион (выделить новый через платформенный аллокатор).
    fn take_spill_region(&self) -> ArenaRegion {
        let size = self.spill_size();
        let alloc = platform::alloc_normal(size);
        ArenaRegion {
            ptr: alloc.ptr,
            len: alloc.size,
            used: AtomicUsize::new(0),
        }
    }

    /// Зарезервировать `size` выровненных байт в регионе текущего потока.
    /// Сначала пробуем первичный регион, затем существующие spill-регионы,
    /// и только при нехватке везде — выделяем новый spill. Возвращает указатель.
    fn alloc_bytes(&self, size: usize) -> *mut u8 {
        let align = core::mem::align_of::<T>();
        let slot = self.thread_slot();
        if let Some(p) = Self::region_alloc(&self.primary[slot], size, align) {
            return p;
        }
        let mut spill = self.spill.lock();
        for r in spill.iter() {
            if let Some(p) = Self::region_alloc(r, size, align) {
                return p;
            }
        }
        let region = self.take_spill_region();
        let p = Self::region_alloc(&region, size, align).expect("spill region large enough");
        spill.push(region);
        p
    }

    /// Аллоцировать объект `T` в регионе текущего потока и вернуть `&mut T`.
    pub fn alloc(&self, value: T) -> &mut T {
        let p = self.alloc_bytes(core::mem::size_of::<T>()) as *mut T;
        unsafe {
            ptr::write(p, value);
            &mut *p
        }
    }

    /// Аллоцировать элементы из итератора и вернуть непрерывный мутабельный
    /// срез. Собирает во временный `Vec`, чтобы знать точное число и выделить
    /// одним блоком (гарантия непрерывности, как у typed-arena).
    pub fn alloc_extend<I: IntoIterator<Item = T>>(&self, iterable: I) -> &mut [T] {
        let items: Vec<T> = iterable.into_iter().collect();
        let count = items.len();
        if count == 0 {
            return &mut [];
        }
        let size = count * core::mem::size_of::<T>();
        let p = self.alloc_bytes(size) as *mut T;
        for (i, v) in items.into_iter().enumerate() {
            unsafe {
                ptr::write(p.add(i), v);
            }
        }
        unsafe { &mut *ptr::slice_from_raw_parts_mut(p, count) }
    }

    /// Суммарное число выделенных элементов по всем регионам.
    pub fn len(&self) -> usize {
        let mut n = 0usize;
        for r in &self.primary {
            n += r.used.load(Ordering::Relaxed) / core::mem::size_of::<T>().max(1);
        }
        for r in self.spill.lock().iter() {
            n += r.used.load(Ordering::Relaxed) / core::mem::size_of::<T>().max(1);
        }
        n
    }

    /// Пустая ли арена.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Ёмкость: суммарный размер общего буфера (без spill-регионов).
    pub fn capacity(&self) -> usize {
        self.total_bytes
    }

    /// Непрерывный неинициализированный срез в регионе текущего потока.
    /// Вызывает `alloc_bytes(num * size_of::<T>())`, т.е. выделяет ровно под
    /// `num` элементов одним блоком.
    pub unsafe fn alloc_uninitialized(&self, num: usize) -> &mut [core::mem::MaybeUninit<T>] {
        if num == 0 {
            return &mut [];
        }
        let size = num * core::mem::size_of::<T>();
        let p = self.alloc_bytes(size) as *mut core::mem::MaybeUninit<T>;
        unsafe { &mut *ptr::slice_from_raw_parts_mut(p, num) }
    }

    /// Убедиться, что в регионе текущего потока уже есть непрерывное место под
    /// `num` элементов. Если нет — это no-op (bump всегда возьмёт следующий
    /// регион при нехватке).
    pub fn reserve_extend(&self, num: usize) {
        let _ = num;
    }

    /// Итератор по всем элементам (по всем регионам, в порядке выделения).
    pub fn iter(&self) -> Iter<'_, T> {
        Iter {
            arena: self,
            region: 0,
            spill_head: 0,
            idx: 0,
        }
    }

    /// Итератор с мутабельным доступом (как в typed-arena::iter_mut).
    pub fn iter_mut(&mut self) -> IterMut<'_, T> {
        let p = self as *mut Self;
        IterMut {
            arena: unsafe { &mut *p },
            region: 0,
            idx: 0,
        }
    }

    /// Превратить арену в `Vec<T>` (порядок выделения). Эксклюзивный доступ.
    ///
    /// `ptr::read` делает побитовую копию каждого элемента (Arc refcount не
    /// увеличивается!). Поэтому после чтения我们必须 обнулить `used` всех
    /// регионов — иначе `Drop::drop → clear()` вызовет `drop_in_place` на уже
    /// прочитанных объектах (use-after-free). После обнуления `Drop` находит
    /// `used == 0` и пропускает; spill-регионы освобождаются через `platform::free`.
    pub fn into_vec(self) -> Vec<T> {
        let len = self.len();
        let mut out = Vec::with_capacity(len);
        for r in &self.primary {
            let count = r.used.load(Ordering::Relaxed) / core::mem::size_of::<T>().max(1);
            let base = r.ptr as *const T;
            for i in 0..count {
                unsafe {
                    out.push(ptr::read(base.add(i)));
                }
            }
            r.used.store(0, Ordering::Relaxed);
        }
        for r in self.spill.lock().iter() {
            let count = r.used.load(Ordering::Relaxed) / core::mem::size_of::<T>().max(1);
            let base = r.ptr as *const T;
            for i in 0..count {
                unsafe {
                    out.push(ptr::read(base.add(i)));
                }
            }
            r.used.store(0, Ordering::Relaxed);
        }
        out
    }

    /// Сбросить арену: разрушить все элементы, затем вернуть все регионы к нулю,
    /// чтобы их можно было переиспользовать.
    pub fn clear(&mut self) {
        for r in &self.primary {
            let count = r.used.load(Ordering::Relaxed) / core::mem::size_of::<T>().max(1);
            for i in 0..count {
                unsafe {
                    ptr::drop_in_place((r.ptr as *mut T).add(i));
                }
            }
            r.used.store(0, Ordering::Relaxed);
        }
        let spill = self.spill.lock();
        for r in spill.iter() {
            let count = r.used.load(Ordering::Relaxed) / core::mem::size_of::<T>().max(1);
            for i in 0..count {
                unsafe {
                    ptr::drop_in_place((r.ptr as *mut T).add(i));
                }
            }
            r.used.store(0, Ordering::Relaxed);
        }
    }
}

impl<T> Default for Arena<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Drop for Arena<T> {
    fn drop(&mut self) {
        // Разрушаем все элементы всех регионов. Primary-буфер освобождает
        // `SharedArena`; spill-регионы — тут же, через `platform::free`.
        self.clear();
        let spill = self.spill.get_mut();
        for r in spill.drain(..) {
            platform::free(RawAllocation {
                ptr: r.ptr,
                size: r.len,
                is_huge: false,
            });
        }
    }
}

/// Итератор по всем элементам `Arena<T>` (primary затем spill).
pub struct Iter<'a, T> {
    arena: &'a Arena<T>,
    /// Индекс в `primary` (пока `region < primary.len()`), затем spill.
    region: usize,
    /// Сколько spill-регионов уже пройдено (снимок на начало итерации).
    spill_head: usize,
    idx: usize,
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = &'a T;
    fn next(&mut self) -> Option<&'a T> {
        loop {
            if self.region < self.arena.primary.len() {
                let r = &self.arena.primary[self.region];
                let count = r.used.load(Ordering::Relaxed) / core::mem::size_of::<T>().max(1);
                if self.idx < count {
                    let item = unsafe { &*(r.ptr as *const T).add(self.idx) };
                    self.idx += 1;
                    return Some(item);
                }
                self.region += 1;
                self.idx = 0;
                continue;
            }
            // Спill-регионы.
            let spill = self.arena.spill.lock();
            if self.spill_head < spill.len() {
                let r = &spill[self.spill_head];
                let count = r.used.load(Ordering::Relaxed) / core::mem::size_of::<T>().max(1);
                if self.idx < count {
                    let item = unsafe { &*(r.ptr as *const T).add(self.idx) };
                    self.idx += 1;
                    return Some(item);
                }
                // Регион исчерпан — только теперь переходим к следующему spill.
                self.spill_head += 1;
                self.idx = 0;
                continue;
            }
            return None;
        }
    }
}

/// Итератор с мутабельным доступом по всем элементам `Arena<T>`.
pub struct IterMut<'a, T> {
    arena: &'a mut Arena<T>,
    region: usize,
    idx: usize,
}

impl<'a, T> Iterator for IterMut<'a, T> {
    type Item = &'a mut T;
    fn next(&mut self) -> Option<&'a mut T> {
        // `&mut self` даёт эксклюзивный доступ: регионы не могут меняться во
        // время итерации, а `get_mut` убирает необходимость держать блокировку.
        let arena = unsafe { &mut *(self.arena as *mut Arena<T>) };
        let n_primary = arena.primary.len();
        loop {
            if self.region < n_primary {
                let r = &arena.primary[self.region];
                let count = r.used.load(Ordering::Relaxed) / core::mem::size_of::<T>().max(1);
                if self.idx < count {
                    let p = unsafe { (r.ptr as *mut T).add(self.idx) };
                    self.idx += 1;
                    return Some(unsafe { &mut *p });
                }
                self.region += 1;
                self.idx = 0;
                continue;
            }
            let spill = arena.spill.get_mut();
            let sidx = self.region - n_primary;
            let r = spill.get(sidx)?;
            let count = r.used.load(Ordering::Relaxed) / core::mem::size_of::<T>().max(1);
            if self.idx < count {
                let p = unsafe { (r.ptr as *mut T).add(self.idx) };
                self.idx += 1;
                return Some(unsafe { &mut *p });
            }
            self.idx = 0;
            self.region += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Масштабирование объёмов под Miri (Tree Borrows, несколько сидов): под
    // Miri всё в ×100 меньше, соотношение «занято / размер региона» сохраняется,
    // поэтому логика переполнения и spill-регионов не меняется.
    #[cfg(not(miri))]
    const MS: usize = 1;
    #[cfg(miri)]
    const MS: usize = 100;

    fn count_scale(base: usize) -> usize {
        base / MS
    }

    #[derive(Debug, PartialEq, Eq)]
    struct NoClone(u32);

    /// Timed: под Miri арены крупнее треубемых — ждать дорого.
    fn scaled_per(per: usize) -> usize {
        per.div_ceil(MS).max(1)
    }

    // ---------- Конструкторы ----------

    #[test]
    fn constructors_default_capacity_and_regions() {
        let a = Arena::<u64>::new();
        assert!(!a.is_empty() || a.capacity() > 0);
        assert!(a.regions() >= 1);
        assert_eq!(a.len(), 0);
        assert!(a.capacity() > 0);

        let b = Arena::<u64>::default();
        assert!(b.capacity() > 0);

        let c = Arena::<u64>::with_capacity(100);
        assert!(c.capacity() >= 100 * size_of::<u64>());
        assert_eq!(c.len(), 0);
        assert!(c.regions() >= 1);
    }

    #[test]
    fn with_regions_explicit_counts() {
        // Гибкий API: явное число регионов и размер каждого.
        let per = scaled_per(64);
        for regions in [1usize, 2, 4, 8] {
            let arena = Arena::<u64>::with_regions(regions, per);
            assert_eq!(arena.regions(), regions);
            // Ёмкость >= regions * per_region элементов × размер.
            assert!(arena.capacity() >= regions * per * size_of::<u64>());
        }
        // 1 регион тоже корректен.
        let a = Arena::<u64>::with_regions(1, scaled_per(8));
        assert_eq!(a.regions(), 1);
    }

    // ---------- alloc ----------

    #[test]
    fn alloc_distinct_and_len() {
        let arena = Arena::<u64>::with_regions(1, scaled_per(16));
        let a = arena.alloc(10u64);
        let b = arena.alloc(20u64);
        let c = arena.alloc(30u64);
        assert_eq!(*a, 10);
        assert_eq!(*b, 20);
        assert_eq!(*c, 30);
        assert_ne!(a as *const u64, b as *const u64);
        assert_ne!(b as *const u64, c as *const u64);
        assert_eq!(arena.len(), 3);
        *a = 99;
        assert_eq!(*a, 99);
        assert_eq!(arena.len(), 3);
    }

    #[test]
    fn alloc_non_zero_sized_many_across_regions() {
        // Преодолеваем границу первичных регионов; значения должны сохраниться
        // в порядке выделения для итератора.
        let per = scaled_per(1000);
        let arena = Arena::<u64>::with_regions(2, per);
        for i in 0..count_scale(5000) {
            assert_eq!(*arena.alloc(i as u64), i as u64);
        }
        assert_eq!(arena.len(), count_scale(5000));
    }

    #[test]
    fn alloc_of_string_values() {
        let arena = Arena::<String>::with_regions(2, scaled_per(10));
        let s = "hello";
        let a = arena.alloc(s.to_string());
        let b = arena.alloc(format!("{}-world", s));
        assert_eq!(*a, "hello");
        assert_eq!(*b, "hello-world");
        // Мутация через &mut.
        b.push('!');
        assert_eq!(*b, "hello-world!");
        assert_eq!(arena.len(), 2);
    }

    #[test]
    fn alloc_non_default_non_clone_type() {
        let arena = Arena::<NoClone>::new();
        let a = arena.alloc(NoClone(1));
        let b = arena.alloc(NoClone(2));
        assert_eq!(*a, NoClone(1));
        assert_eq!(*b, NoClone(2));
    }

    // ---------- len / is_empty / capacity ----------

    #[test]
    fn len_and_is_empty_transitions() {
        let mut arena = Arena::<u32>::with_regions(1, scaled_per(8));
        assert!(arena.is_empty());
        assert_eq!(arena.len(), 0);
        arena.alloc(1u32);
        assert!(!arena.is_empty());
        assert_eq!(arena.len(), 1);
        arena.alloc(2u32);
        assert_eq!(arena.len(), 2);
        arena.clear();
        assert!(arena.is_empty());
        assert_eq!(arena.len(), 0);
    }

    #[test]
    fn capacity_is_bytes_and_positive() {
        let arena = Arena::<u64>::with_regions(3, scaled_per(4));
        // Ёмкость — суммарный байтовый размер общего буфера и кратна 16 (выравнивание чанка).
        assert!(arena.capacity() > 0);
        assert_eq!(arena.capacity() % 16, 0);
        // Ёмкость не меньше числа элементов × размер.
        arena.alloc(1u64);
        arena.alloc(2u64);
        assert!(arena.capacity() >= arena.len() * size_of::<u64>());
    }

    // ---------- alloc_extend / reserve_extend / alloc_uninitialized ----------

    #[test]
    fn alloc_extend_values_and_contiguity() {
        let arena = Arena::<u64>::with_regions(1, scaled_per(4));
        let s = arena.alloc_extend(10..21u64);
        assert_eq!(s.len(), 11);
        for (i, x) in s.iter().enumerate() {
            assert_eq!(*x, 10 + i as u64);
        }
        for i in 0..s.len() - 1 {
            assert_eq!(
                &s[i + 1] as *const u64 as usize,
                &s[i] as *const u64 as usize + size_of::<u64>()
            );
        }
        assert_eq!(arena.len(), 11);
    }

    #[test]
    fn alloc_extend_empty_is_noop() {
        let arena = Arena::<u64>::new();
        let e: &mut [u64] = arena.alloc_extend(std::iter::empty::<u64>());
        assert_eq!(e.len(), 0);
        assert_eq!(arena.len(), 0);
    }

    #[test]
    fn alloc_extend_over_region_boundary() {
        // Больше одного региона — получаем непрерывный срез из spill/следующего.
        let arena = Arena::<u32>::with_regions(1, scaled_per(2));
        let s = arena.alloc_extend(0..count_scale(1000) as u32);
        assert_eq!(s.len() as u64, count_scale(1000) as u64);
        for (i, x) in s.iter().enumerate() {
            assert_eq!(*x, i as u32);
        }
    }

    #[test]
    fn reserve_extend_does_not_panic() {
        let arena = Arena::<u64>::new();
        arena.reserve_extend(count_scale(64));
        arena.reserve_extend(0);
        let x = arena.alloc(7u64);
        assert_eq!(*x, 7);
    }

    #[test]
    fn alloc_uninitialized_then_write() {
        let arena = Arena::<u32>::with_regions(1, scaled_per(4));
        let s = unsafe { arena.alloc_uninitialized(4) };
        s[0].write(11);
        s[1].write(22);
        s[2].write(33);
        s[3].write(44);
        let got: Vec<u32> = arena.iter().copied().collect();
        assert_eq!(got, vec![11, 22, 33, 44]);
    }

    #[test]
    fn alloc_uninitialized_zero() {
        let arena = Arena::<u64>::new();
        let e = unsafe { arena.alloc_uninitialized(0) };
        assert_eq!(e.len(), 0);
        assert_eq!(arena.len(), 0);
    }

    // ---------- iter / iter_mut / into_vec ----------

    #[test]
    fn iter_and_iter_mut() {
        let mut arena = Arena::<i32>::with_regions(1, scaled_per(8));
        for i in 0..5 {
            arena.alloc(i);
        }
        let got: Vec<i32> = arena.iter().copied().collect();
        assert_eq!(got, vec![0, 1, 2, 3, 4]);
        {
            for x in arena.iter_mut() {
                *x *= 10;
            }
        }
        let got2: Vec<i32> = arena.iter().copied().collect();
        assert_eq!(got2, vec![0, 10, 20, 30, 40]);
        assert_eq!(arena.len(), 5);
    }

    #[test]
    fn iter_returns_none_after_end_and_empty() {
        let arena = Arena::<u64>::new();
        let mut it = arena.iter();
        assert!(it.next().is_none());
        assert!(it.next().is_none());
    }

    #[test]
    fn iter_over_spill_all_regions() {
        let arena = Arena::<u64>::with_regions(2, scaled_per(1));
        for i in 0..count_scale(1000) {
            arena.alloc(i as u64);
        }
        let it: Vec<u64> = arena.iter().copied().collect();
        assert_eq!(it.len(), count_scale(1000));
        for (i, x) in it.iter().enumerate() {
            assert_eq!(*x, i as u64);
        }
        // iter_mut обходит те же значения.
        {
            let mut arena_mut = Arena::<u64>::with_regions(2, scaled_per(1));
            for i in 0..count_scale(1000) {
                arena_mut.alloc(i as u64);
            }
            for x in arena_mut.iter_mut() {
                *x += 1;
            }
            let got: Vec<u64> = arena_mut.iter().copied().collect();
            for (i, x) in got.iter().enumerate() {
                assert_eq!(*x, (i + 1) as u64);
            }
        }
    }

    #[test]
    fn into_vec_preserves_order() {
        let arena = Arena::<u8>::with_regions(2, scaled_per(1));
        for i in 0..count_scale(2000) {
            arena.alloc((i % 256) as u8);
        }
        let v = arena.into_vec();
        assert_eq!(v.len(), count_scale(2000));
        for (i, x) in v.iter().enumerate() {
            assert_eq!(*x, (i % 256) as u8);
        }
    }

    #[test]
    fn iter_mut_returns_none_after_end() {
        let mut arena = Arena::<u64>::with_regions(1, scaled_per(2));
        for i in 0..count_scale(300) {
            arena.alloc(i as u64);
        }
        let mut seen = 0;
        {
            let mut im = arena.iter_mut();
            while im.next().is_some() {
                seen += 1;
            }
            assert!(im.next().is_none());
        }
        assert_eq!(seen, count_scale(300));
    }

    // ---------- clear / reuse / Drop ----------

    #[test]
    fn clear_drops_and_reuses() {
        struct DropCounter(Arc<AtomicUsize>);
        impl Drop for DropCounter {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let count = Arc::new(AtomicUsize::new(0));
        let mut arena = Arena::<DropCounter>::with_regions(1, scaled_per(4));
        for _ in 0..3 {
            arena.alloc(DropCounter(Arc::clone(&count)));
        }
        assert_eq!(count.load(Ordering::Relaxed), 0);
        arena.clear();
        assert_eq!(count.load(Ordering::Relaxed), 3);
        assert_eq!(arena.len(), 0);
        for _ in 0..3 {
            arena.alloc(DropCounter(Arc::clone(&count)));
        }
        assert_eq!(arena.len(), 3);
    }

    #[test]
    fn clear_after_spill_reuses_regions() {
        struct DropCounter(Arc<AtomicUsize>);
        impl Drop for DropCounter {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let count = Arc::new(AtomicUsize::new(0));
        let mut arena = Arena::<DropCounter>::with_regions(1, scaled_per(1));
        for _ in 0..count_scale(200) {
            arena.alloc(DropCounter(Arc::clone(&count)));
        }
        assert_eq!(count.load(Ordering::Relaxed), 0);
        arena.clear();
        assert_eq!(count.load(Ordering::Relaxed), count_scale(200));
        assert!(arena.is_empty());
        // Переиспользование после spill.
        for _ in 0..count_scale(200) {
            arena.alloc(DropCounter(Arc::clone(&count)));
        }
        assert_eq!(arena.len(), count_scale(200));
        assert_eq!(count.load(Ordering::Relaxed), count_scale(200));
        let v = arena.into_vec();
        // into_vec zeros used counters → Drop::clear finds nothing to drop.
        assert_eq!(count.load(Ordering::Relaxed), count_scale(200));
        assert_eq!(v.len(), count_scale(200));
        drop(v); // Vec elements (ptr::read copies) are dropped
        assert_eq!(count.load(Ordering::Relaxed), 2 * count_scale(200));
    }

    #[test]
    fn drop_destroys_all_elements() {
        struct DropCounter(Arc<AtomicUsize>);
        impl Drop for DropCounter {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let count = Arc::new(AtomicUsize::new(0));
        {
            let arena = Arena::<DropCounter>::with_regions(1, scaled_per(2));
            for _ in 0..6 {
                arena.alloc(DropCounter(Arc::clone(&count)));
            }
            assert_eq!(count.load(Ordering::Relaxed), 0);
        }
        assert_eq!(count.load(Ordering::Relaxed), 6);
    }

    // ---------- Send / Sync ----------

    #[test]
    fn arena_is_send_and_sync_for_send_t() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Arena<String>>();
        assert_send_sync::<Arena<u64>>();
        let arena = Arena::<u64>::new();
        // Передача в другой поток — компилируется только если Arena: Send.
        let h = std::thread::spawn(move || {
            let _ = arena.len();
        });
        h.join().unwrap();
    }

    // ---------- Multi-threaded (общая "shared"-природа) ----------

    #[test]
    fn multithreaded_alloc_disjoint_and_total_len() {
        let regions = 4;
        let per = scaled_per(64);
        let arena = Arena::<u64>::with_regions(regions, per);
        let n_threads = 4;
        let per_thread = count_scale(2000);

        std::thread::scope(|s| {
            for t in 0..n_threads {
                let arena = &arena;
                s.spawn(move || {
                    let mut v = Vec::new();
                    for i in 0..per_thread {
                        // Каждый поток пишет уникальные метки.
                        let val = (t as u64) * 1_000_000 + i as u64;
                        let r = arena.alloc(val);
                        v.push(*r); // фиксируем прочитанное значение
                    }
                    // Значения, прочитанные через возвращённые &mut, совпадают.
                    for (idx, expect) in v.iter().enumerate() {
                        assert_eq!(*expect, (t as u64) * 1_000_000 + idx as u64);
                    }
                });
            }
        });

        assert_eq!(arena.len(), n_threads * per_thread);
        // Все элементы присутствуют в итераторе (минимум: суммарный счёт совпал).
        let seen = arena.iter().count();
        assert_eq!(seen, n_threads * per_thread);
    }

    // ---------- Несколько размеров регионов ----------

    #[test]
    fn several_region_sizes_are_consistent() {
        // Проверяем согласованность len/iter/into_vec при различных размерах
        // первичных регионов и числах регионов (спилл в любом случае).
        let total = count_scale(5000);
        for regions in [1usize, 2, 4] {
            for per in [1usize, scaled_per(8), scaled_per(64)] {
                let arena = Arena::<u64>::with_regions(regions, per);
                for i in 0..total {
                    arena.alloc(i as u64);
                }
                assert_eq!(arena.len(), total);
                let it: Vec<u64> = arena.iter().copied().collect();
                assert_eq!(it.len(), total);
                for (i, x) in it.iter().enumerate() {
                    assert_eq!(*x, i as u64);
                }
            }
        }
    }

    #[test]
    fn allocations_nowhere_overlap() {
        // Гарантия отсутствия перекрытий даже при spill: адреса всех аллокаций
        // одного размера попарно различны.
        let arena = Arena::<u64>::with_regions(2, scaled_per(1));
        let mut addrs = Vec::new();
        for _ in 0..count_scale(300) {
            let p = arena.alloc(0u64) as *const u64 as usize;
            addrs.push(p);
        }
        addrs.sort_unstable();
        for w in addrs.windows(2) {
            assert_ne!(w[0], w[1]);
        }
        assert_eq!(arena.len(), count_scale(300));
    }
}

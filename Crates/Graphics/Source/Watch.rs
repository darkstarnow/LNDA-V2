use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::Mutex;

pub(crate) const PAGE: usize = 64 << 10;
const KERNEL_PAGE: usize = 4096;

pub(crate) struct WriteJournal {
    asked: u64,
    written: BTreeMap<usize, u64>,
    found: Vec<usize>,
    ahead: Vec<(usize, usize)>,
    ahead_asked: u64,
    kept: bool,
}

static JOURNAL: Mutex<WriteJournal> = Mutex::new(WriteJournal {
    asked: 0,
    written: BTreeMap::new(),
    found: Vec::new(),
    ahead: Vec::new(),
    ahead_asked: 0,
    kept: false,
});

const AHEAD_GAP: usize = (8 << 20) / PAGE;
const AHEAD_MAX: usize = (1 << 30) / PAGE;

#[cfg(feature = "diagnostics")]
static SLOW: [std::sync::atomic::AtomicU64; 3] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 3];

const BLOCK: usize = PAGE;

#[derive(Default)]
struct CleanBlocks(BTreeMap<usize, usize>);

impl CleanBlocks {
    fn insert(&mut self, mut start: usize, mut end: usize) {
        if start >= end {
            return;
        }
        if let Some((&before, &stop)) = self.0.range(..=start).next_back()
            && stop >= start
        {
            if stop >= end {
                return;
            }
            start = before;
        }
        while let Some((&next, &stop)) = self.0.range(start..=end).next() {
            self.0.remove(&next);
            end = end.max(stop);
        }
        self.0.insert(start, end);
    }

    fn remove(&mut self, block: usize) {
        let Some((&start, &end)) = self.0.range(..=block).next_back() else {
            return;
        };
        if block >= end {
            return;
        }
        if start == block {
            self.0.remove(&start);
        } else {
            self.0.insert(start, block);
        }
        if block + 1 < end {
            self.0.insert(block + 1, end);
        }
    }

    fn gaps(&self, first: usize, last: usize) -> Vec<Range<usize>> {
        let mut cursor = first;
        if let Some((_, &end)) = self.0.range(..=first).next_back() {
            cursor = cursor.max(end);
        }
        let mut gaps = Vec::new();
        if cursor < last {
            for (&start, &end) in self.0.range(cursor..last) {
                if cursor < start {
                    gaps.push(cursor..start);
                }
                cursor = end;
            }
            if cursor < last {
                gaps.push(cursor..last);
            }
        }
        gaps
    }
}

pub(crate) struct PageGuards {
    protected: BTreeSet<usize>,
    clean: CleanBlocks,
    absent: BTreeSet<usize>,
    aliased: BTreeSet<usize>,
}

static GUARDS: Mutex<PageGuards> = Mutex::new(PageGuards {
    protected: BTreeSet::new(),
    clean: CleanBlocks(BTreeMap::new()),
    absent: BTreeSet::new(),
    aliased: BTreeSet::new(),
});

pub(crate) const MANY_VIEWS: usize = 64;

#[cfg(feature = "diagnostics")]
pub(crate) static ALIASES: [std::sync::atomic::AtomicU64; 3] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 3];

struct Mapping {
    changing: Option<Range<usize>>,
    readers: Vec<Range<usize>>,
}

static MAPPING: Mutex<Mapping> = Mutex::new(Mapping {
    changing: None,
    readers: Vec::new(),
});
static WRITER: Mutex<()> = Mutex::new(());

pub struct TitleMemory;

impl WriteJournal {
    pub(crate) fn ask_ahead(ranges: &[(usize, usize)]) {
        let mut pieces: Vec<(usize, usize)> = ranges
            .iter()
            .filter(|r| r.0 < r.1)
            .map(|&(start, end)| (start / PAGE, end.div_ceil(PAGE)))
            .collect();
        if pieces.len() < 2 {
            return;
        }
        pieces.sort_unstable();
        let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
        let j = &mut *j;
        if j.kept {
            return;
        }
        j.asked += 1;
        j.ahead_asked = j.asked;
        j.ahead.clear();
        let mut i = 0;
        while i < pieces.len() {
            let first = pieces[i].0;
            let mut last = pieces[i].1;
            let mut k = i + 1;
            while k < pieces.len()
                && pieces[k].0 <= last + AHEAD_GAP
                && last.max(pieces[k].1) - first <= AHEAD_MAX
            {
                last = last.max(pieces[k].1);
                k += 1;
            }
            WriteJournal::ask_run(j, &pieces[i..k]);
            i = k;
        }
    }

    #[cfg(feature = "diagnostics")]
    fn slow(us: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        if us >= 1000 {
            SLOW[0].fetch_add(1, Relaxed);
            SLOW[1].fetch_add(us, Relaxed);
        }
        SLOW[2].fetch_max(us, Relaxed);
    }

    #[cfg(feature = "diagnostics")]
    pub(crate) fn slow_asks() -> [u64; 3] {
        [0, 1, 2].map(|i| SLOW[i].swap(0, std::sync::atomic::Ordering::Relaxed))
    }

    pub(crate) fn ask_ahead_done() {
        let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
        if !j.kept {
            j.ahead.clear();
        }
    }

    pub(crate) fn once() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("LEONIDA_ASK_ONCE").is_none_or(|v| v != "0"))
    }

    pub(crate) fn ask_once(ranges: &[(usize, usize)]) {
        if !WriteJournal::once() {
            return;
        }
        WriteJournal::ask_once_end();
        WriteJournal::ask_ahead(ranges);
        let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
        let j = &mut *j;
        if j.ahead.is_empty() {
            j.asked += 1;
            j.ahead_asked = j.asked;
        }
        for &(start, end) in ranges {
            if start >= end {
                continue;
            }
            let (first, last) = (start / PAGE, end.div_ceil(PAGE));
            if !j.ahead.iter().any(|&(a, b)| a <= first && last <= b)
                && WriteJournal::ask_kernel(j, first, last)
            {
                j.ahead.push((first, last));
            }
        }
        j.kept = !j.ahead.is_empty();
    }

    pub(crate) fn ask_once_end() {
        let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
        if j.kept {
            j.kept = false;
            j.ahead.clear();
        }
    }

    pub(crate) fn wrote(at: usize, len: usize) {
        let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
        if !j.kept || len == 0 {
            return;
        }
        j.asked += 1;
        let now = j.asked;
        for page in at / PAGE..at.saturating_add(len).div_ceil(PAGE) {
            j.written.insert(page, now);
        }
    }

    fn ask_run(j: &mut WriteJournal, pieces: &[(usize, usize)]) {
        if pieces.len() < 2 {
            return;
        }
        let first = pieces[0].0;
        let last = pieces.iter().map(|p| p.1).max().unwrap_or(first);
        if WriteJournal::ask_kernel(j, first, last) {
            j.ahead.push((first, last));
            return;
        }
        let (a, b) = pieces.split_at(pieces.len() / 2);
        WriteJournal::ask_run(j, a);
        WriteJournal::ask_run(j, b);
    }

    fn ask_kernel(j: &mut WriteJournal, first: usize, last: usize) -> bool {
        use windows_sys::Win32::System::Memory::GetWriteWatch;
        const WRITE_WATCH_FLAG_RESET: u32 = 1;
        let mut count = (last - first) * PAGE / KERNEL_PAGE;
        j.found.resize(count.max(j.found.len()), 0);
        let mut granularity = 0;
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        let failed = unsafe {
            GetWriteWatch(
                WRITE_WATCH_FLAG_RESET,
                (first * PAGE) as *const _,
                (last - first) * PAGE,
                j.found.as_mut_ptr().cast(),
                &mut count,
                &mut granularity,
            )
        } != 0;
        if failed {
            return false;
        }
        #[cfg(feature = "diagnostics")]
        {
            use crate::copystat::C;
            C::add(C::Watches, 1);
            C::add(C::WatchUs, started.elapsed().as_micros() as u64);
            WriteJournal::slow(started.elapsed().as_micros() as u64);
        }
        let now = j.ahead_asked;
        for k in 0..count {
            let page = j.found[k] / PAGE;
            j.written.insert(page, now);
        }
        true
    }

    pub(crate) fn asked(start: usize, end: usize) -> Option<(u64, Vec<(usize, u64)>)> {
        let (first, last) = (start / PAGE, end.div_ceil(PAGE));
        let j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
        j.ahead
            .iter()
            .any(|&(a, b)| a <= first && last <= b)
            .then(|| {
                (
                    j.ahead_asked,
                    j.written
                        .range(first..last)
                        .map(|(&p, &w)| (p, w))
                        .collect(),
                )
            })
    }

    pub(crate) fn writes(
        start: usize,
        end: usize,
        guarded: bool,
    ) -> Option<(u64, Vec<(usize, u64)>)> {
        use windows_sys::Win32::System::Memory::GetWriteWatch;
        const WRITE_WATCH_FLAG_RESET: u32 = 1;
        let (first, last) = (start / PAGE, end.div_ceil(PAGE));
        let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
        let j = &mut *j;
        if j.ahead.iter().any(|&(a, b)| a <= first && last <= b) {
            return Some((
                j.ahead_asked,
                j.written
                    .range(first..last)
                    .map(|(&p, &w)| (p, w))
                    .collect(),
            ));
        }
        let mut count = (last - first) * PAGE / KERNEL_PAGE;
        j.found.resize(count.max(j.found.len()), 0);
        let mut granularity = 0;
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        let failed = unsafe {
            GetWriteWatch(
                WRITE_WATCH_FLAG_RESET,
                (first * PAGE) as *const _,
                (last - first) * PAGE,
                j.found.as_mut_ptr().cast(),
                &mut count,
                &mut granularity,
            )
        } != 0;
        let guarded = if failed {
            count = 0;
            if !guarded {
                return None;
            }
            #[cfg(feature = "diagnostics")]
            let guarding = std::time::Instant::now();
            let written = PageGuards::guard(first * PAGE, last * PAGE);
            #[cfg(feature = "diagnostics")]
            crate::copystat::C::add(
                crate::copystat::C::GuardUs,
                guarding.elapsed().as_micros() as u64,
            );
            written?
        } else {
            Vec::new()
        };
        #[cfg(feature = "diagnostics")]
        {
            use crate::copystat::C;
            C::add(C::Watches, 1);
            C::add(C::WatchUs, started.elapsed().as_micros() as u64);
            WriteJournal::slow(started.elapsed().as_micros() as u64);
        }
        j.asked += 1;
        let now = j.asked;
        let pages = j.found[..count].iter().map(|&at| at / PAGE).chain(guarded);
        for page in pages.collect::<Vec<_>>() {
            j.written.insert(page, now);
        }
        Some((
            now,
            j.written
                .range(first..last)
                .map(|(&p, &w)| (p, w))
                .collect(),
        ))
    }
}

impl PageGuards {
    fn lock() -> std::sync::MutexGuard<'static, PageGuards> {
        GUARDS.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn guard(start: usize, end: usize) -> Option<Vec<usize>> {
        static ASK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let known = !*ASK
            .get_or_init(|| std::env::var_os("LEONIDA_GUARD_ASK").is_some_and(|v| v == "1"))
            && {
                let g = PageGuards::lock();
                let first = start / BLOCK;
                g.protected.contains(&first) || g.absent.contains(&first)
            };
        if !known && !PageGuards::is_view(start, end) {
            return None;
        }
        TitleMemory::install();
        let mut g = PageGuards::lock();
        let mut written = Vec::new();
        let (first, last) = (start / BLOCK, end.div_ceil(BLOCK));
        let mut uncommitted_until = first;
        for gap in g.clean.gaps(first, last) {
            let mut run = None;
            for block in gap.clone() {
                if g.absent.contains(&block) {
                    if block >= uncommitted_until {
                        uncommitted_until = PageGuards::uncommitted_end(block);
                    }
                    if block < uncommitted_until {
                        if let Some(from) = run.take() {
                            PageGuards::protect(&mut g, from, block);
                        }
                        continue;
                    }
                    g.absent.remove(&block);
                }
                written.extend(block * BLOCK / PAGE..(block + 1) * BLOCK / PAGE);
                if g.protected.contains(&block) {
                    g.clean.insert(block, block + 1);
                } else {
                    run.get_or_insert(block);
                    continue;
                }
                if let Some(from) = run.take() {
                    PageGuards::protect(&mut g, from, block);
                }
            }
            if let Some(from) = run.take() {
                PageGuards::protect(&mut g, from, gap.end);
            }
        }
        Some(written)
    }

    fn is_view(start: usize, end: usize) -> bool {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_MAPPED, MEMORY_BASIC_INFORMATION, VirtualQuery,
        };
        static HOLES: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let holes =
            *HOLES.get_or_init(|| std::env::var_os("LEONIDA_GUARD_HOLES").is_none_or(|v| v != "0"));
        let mut at = start;
        loop {
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
            if n == 0 {
                return false;
            }
            if info.Type == MEM_MAPPED {
                return true;
            }
            if info.State == MEM_COMMIT || !holes {
                return false;
            }
            let next = (info.BaseAddress as usize).saturating_add(info.RegionSize);
            if next <= at || next >= end {
                return true;
            }
            at = next;
        }
    }

    fn protect(g: &mut PageGuards, from: usize, to: usize) {
        PageGuards::protect_here(g, from, to);
        if !PageGuards::view_guard() {
            return;
        }
        for (base, bytes) in crate::inplace::SectionViews::between(
            from.saturating_mul(BLOCK),
            to.saturating_mul(BLOCK),
        ) {
            let first = (base / BLOCK).max(from);
            let last = (base.saturating_add(bytes).div_ceil(BLOCK)).min(to);
            for block in first..last {
                let shown = crate::inplace::SectionViews::pages_of(block * BLOCK);
                if shown.len() < 2 || shown.len() > MANY_VIEWS {
                    continue;
                }
                #[cfg(feature = "diagnostics")]
                ALIASES[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                for alias in shown {
                    let alias = alias / BLOCK;
                    g.aliased.insert(alias);
                    if alias != block {
                        PageGuards::protect_alias(g, alias);
                    }
                }
            }
        }
    }

    fn view_guard() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("LEONIDA_VIEW_GUARD").is_none_or(|v| v != "0"))
    }

    fn protect_alias(g: &mut PageGuards, block: usize) {
        use windows_sys::Win32::System::Memory::{PAGE_READONLY, VirtualProtect};
        if g.protected.contains(&block) {
            return;
        }
        let mut old = 0;
        if unsafe { VirtualProtect((block * BLOCK) as *const _, BLOCK, PAGE_READONLY, &mut old) }
            != 0
        {
            g.protected.insert(block);
        }
    }

    fn protect_here(g: &mut PageGuards, from: usize, to: usize) {
        use windows_sys::Win32::System::Memory::{PAGE_READONLY, VirtualProtect};
        let set = |from: usize, to: usize| {
            let mut old = 0;
            unsafe {
                VirtualProtect(
                    (from * BLOCK) as *const _,
                    (to - from) * BLOCK,
                    PAGE_READONLY,
                    &mut old,
                ) != 0
            }
        };
        let note = |g: &mut PageGuards, from: usize, to: usize| {
            g.protected.extend(from..to);
            g.clean.insert(from, to);
        };
        if set(from, to) {
            note(g, from, to);
            return;
        }
        for block in from..to {
            if set(block, block + 1) {
                note(g, block, block + 1);
            } else if block < PageGuards::uncommitted_end(block) {
                g.absent.insert(block);
            }
        }
    }

    fn uncommitted_end(block: usize) -> usize {
        use windows_sys::Win32::System::Memory::{
            MEM_FREE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, VirtualQuery,
        };
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        let n = unsafe { VirtualQuery((block * BLOCK) as *const _, &mut info, size_of_val(&info)) };
        if n != 0 && matches!(info.State, MEM_RESERVE | MEM_FREE) {
            (info.BaseAddress as usize).saturating_add(info.RegionSize) / BLOCK
        } else {
            block
        }
    }

    fn open(g: &mut PageGuards, start: usize, end: usize) -> bool {
        let any = PageGuards::open_here(g, start, end);
        if start >= end || g.aliased.is_empty() {
            return any;
        }
        let aliased: Vec<usize> = g
            .aliased
            .range(start / BLOCK..end.div_ceil(BLOCK))
            .copied()
            .collect();
        for block in aliased {
            g.aliased.remove(&block);
            for alias in crate::inplace::SectionViews::pages_of(block * BLOCK) {
                if alias / BLOCK != block
                    && PageGuards::open_here(g, alias, alias.saturating_add(BLOCK))
                {
                    #[cfg(feature = "diagnostics")]
                    ALIASES[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                g.aliased.remove(&(alias / BLOCK));
            }
        }
        any
    }

    fn open_here(g: &mut PageGuards, start: usize, end: usize) -> bool {
        use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
        let blocks: Vec<usize> = g
            .protected
            .range(start / BLOCK..end.div_ceil(BLOCK))
            .copied()
            .collect();
        for &block in &blocks {
            g.protected.remove(&block);
            g.clean.remove(block);
            let mut old = 0;
            unsafe { VirtualProtect((block * BLOCK) as *const _, BLOCK, PAGE_READWRITE, &mut old) };
        }
        !blocks.is_empty()
    }
}

impl TitleMemory {
    pub fn will_write(at: usize, len: usize) {
        PageGuards::open(&mut PageGuards::lock(), at, at.saturating_add(len));
        crate::descriptor::host::RemapLog::remapped(at, len);
    }

    pub fn bytes_written(bytes: &[u8]) {
        if !bytes.is_empty() {
            crate::descriptor::host::RemapLog::remapped(bytes.as_ptr() as usize, bytes.len());
        }
    }

    pub(crate) fn views_mapped(section: usize, offset: u64, len: usize) {
        if !PageGuards::view_guard() {
            return;
        }
        let ranges = crate::inplace::SectionViews::of_section(section, offset, len);
        if ranges.is_empty() {
            return;
        }
        let mut g = PageGuards::lock();
        for &(at, n) in &ranges {
            if PageGuards::open_here(&mut g, at, at.saturating_add(n)) {
                #[cfg(feature = "diagnostics")]
                ALIASES[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    pub fn remapping() -> Remapping {
        TitleMemory::remapping_range(ALL_MEMORY)
    }

    pub fn remapping_range(range: Range<usize>) -> Remapping {
        let writer = WRITER.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = MAPPING.lock().unwrap_or_else(|e| e.into_inner());
        state.changing = Some(range.clone());
        drop(
            MAPPING_CHANGED
                .wait_while(state, |s| {
                    s.readers.iter().any(|r| Mapping::overlaps(r, &range))
                })
                .unwrap_or_else(|e| e.into_inner()),
        );
        let previous = crate::inplace::SectionViews::physical_spans(&range);
        Remapping {
            _writer: writer,
            range,
            previous,
        }
    }

    pub fn reading() -> Reading {
        TitleMemory::reading_range(ALL_MEMORY)
    }

    pub fn reading_range(range: Range<usize>) -> Reading {
        let state = MAPPING.lock().unwrap_or_else(|e| e.into_inner());
        MAPPING_CHANGED
            .wait_while(state, |s| s.changing(&range))
            .unwrap_or_else(|e| e.into_inner())
            .pin(range)
    }

    pub(crate) fn reading_now(range: Range<usize>) -> Option<Reading> {
        let mut state = match MAPPING.try_lock() {
            Ok(state) => state,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        (!state.changing(&range)).then(|| state.pin(range))
    }

    pub fn remapped(at: usize, len: usize) {
        crate::descriptor::host::RemapLog::remapped(at, len);
        crate::inplace::SectionViews::gone(at, len);
        crate::inplace::TitleSections::freeing(at, len);
        let mut g = PageGuards::lock();
        let range = at / BLOCK..at.saturating_add(len).div_ceil(BLOCK);
        let absent: Vec<usize> = g.absent.range(range.clone()).copied().collect();
        for block in absent {
            g.absent.remove(&block);
        }
        let blocks: Vec<usize> = g.protected.range(range).copied().collect();
        for block in blocks {
            g.protected.remove(&block);
            g.clean.remove(block);
        }
        let aliased: Vec<usize> = g
            .aliased
            .range(at / BLOCK..at.saturating_add(len).div_ceil(BLOCK))
            .copied()
            .collect();
        for block in aliased {
            g.aliased.remove(&block);
        }
    }

    fn install() {
        use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, NTSTATUS};
        use windows_sys::Win32::System::Diagnostics::Debug::{
            AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
            EXCEPTION_POINTERS,
        };
        unsafe extern "system" fn on_write(info: *mut EXCEPTION_POINTERS) -> i32 {
            let r = unsafe { &*(*info).ExceptionRecord };
            const WRITE: usize = 1;
            if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION as NTSTATUS
                || r.NumberParameters < 2
                || r.ExceptionInformation[0] != WRITE
            {
                return EXCEPTION_CONTINUE_SEARCH;
            }
            let at = r.ExceptionInformation[1];
            let mut g = PageGuards::lock();
            if PageGuards::open(&mut g, at, at + 1) {
                #[cfg(feature = "diagnostics")]
                crate::copystat::C::add(crate::copystat::C::GuardFaults, 1);
                return EXCEPTION_CONTINUE_EXECUTION;
            }
            if TitleMemory::writable(at) {
                EXCEPTION_CONTINUE_EXECUTION
            } else {
                EXCEPTION_CONTINUE_SEARCH
            }
        }
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| unsafe {
            AddVectoredExceptionHandler(1, Some(on_write));
        });
    }

    fn writable(at: usize) -> bool {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_READWRITE, VirtualQuery,
        };
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
        n != 0 && info.State == MEM_COMMIT && info.Protect == PAGE_READWRITE
    }
}

impl Mapping {
    fn overlaps(a: &Range<usize>, b: &Range<usize>) -> bool {
        a.start.max(b.start) < a.end.min(b.end)
    }
}

static MAPPING_CHANGED: std::sync::Condvar = std::sync::Condvar::new();
const ALL_MEMORY: Range<usize> = 0..usize::MAX;

impl Mapping {
    fn changing(&self, range: &Range<usize>) -> bool {
        self.changing
            .as_ref()
            .is_some_and(|w| Mapping::overlaps(w, range))
    }

    fn pin(&mut self, range: Range<usize>) -> Reading {
        self.readers.push(range.clone());
        Reading(range)
    }
}

pub struct Remapping {
    _writer: std::sync::MutexGuard<'static, ()>,
    range: Range<usize>,
    previous: Vec<crate::inplace::PhysicalSpan>,
}

impl Drop for Remapping {
    fn drop(&mut self) {
        let current = crate::inplace::SectionViews::physical_spans(&self.range);
        crate::inplace::SectionViews::physical_changes(
            &self.previous,
            &current,
            crate::writeback::WritebackLog::remapped,
        );
        MAPPING.lock().unwrap_or_else(|e| e.into_inner()).changing = None;
        MAPPING_CHANGED.notify_all();
    }
}

pub struct Reading(Range<usize>);

impl Drop for Reading {
    fn drop(&mut self) {
        let mut state = MAPPING.lock().unwrap_or_else(|e| e.into_inner());
        let i = state
            .readers
            .iter()
            .position(|r| *r == self.0)
            .expect("active mapping reader");
        state.readers.swap_remove(i);
        if state.changing(&self.0) {
            MAPPING_CHANGED.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };
    const MEM_WRITE_WATCH: u32 = 0x20_0000;

    #[test]
    fn clean_runs_match_individual_pages_after_overlaps_and_invalidations() {
        let mut clean = CleanBlocks::default();
        let mut pages = BTreeSet::new();
        let check = |clean: &CleanBlocks, pages: &BTreeSet<usize>| {
            for first in 0..=24 {
                for last in first..=24 {
                    assert_eq!(
                        clean
                            .gaps(first, last)
                            .into_iter()
                            .flatten()
                            .collect::<Vec<_>>(),
                        (first..last)
                            .filter(|block| !pages.contains(block))
                            .collect::<Vec<_>>()
                    );
                }
            }
        };
        for (first, last) in [(4, 9), (12, 20), (9, 12), (6, 15), (2, 5), (17, 22), (9, 9)] {
            clean.insert(first, last);
            pages.extend(first..last);
            check(&clean, &pages);
        }
        for block in [13, 2, 21, 14, 12, 0, 10, 20, 13] {
            clean.remove(block);
            pages.remove(&block);
            check(&clean, &pages);
        }
        clean.insert(0, 24);
        assert!(clean.gaps(0, 24).is_empty());
        assert_eq!(clean.0.len(), 1);
    }

    #[test]
    fn written_pages_are_reported_to_every_asker() {
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                4 * PAGE,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(p, 0);
        let (start, end) = (p, p + 4 * PAGE);
        let (_, before) = WriteJournal::writes(start, end, false).expect("watched");
        assert!(before.is_empty());
        unsafe { ((p + PAGE + 8) as *mut u8).write(1) };
        let (now, after) = WriteJournal::writes(start, end, false).expect("watched");
        assert_eq!(after, [(p / PAGE + 1, now)]);
        let (later, again) = WriteJournal::writes(p + PAGE, p + 2 * PAGE, false).expect("watched");
        assert!(later > now);
        assert_eq!(again, [(p / PAGE + 1, now)]);
        let mut plain = vec![0u8; PAGE];
        assert!(
            WriteJournal::writes(
                plain.as_mut_ptr() as usize,
                plain.as_ptr() as usize + PAGE,
                true
            )
            .is_none()
        );
        unsafe { VirtualFree(p as *mut _, 0, MEM_RELEASE) };
    }

    #[test]
    fn kernel_reads_into_guarded_blocks_after_opening() {
        use std::io::{Read, Seek, Write};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, UnmapViewOfFile,
        };
        let view = unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * BLOCK).Value as usize
        };
        assert_ne!(view, 0);
        assert!(PageGuards::guard(view, view + 2 * BLOCK).is_some());
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"from the disk").unwrap();
        let read = |file: &mut std::fs::File| {
            file.rewind().unwrap();
            let into = unsafe { std::slice::from_raw_parts_mut((view + 100) as *mut u8, 13) };
            file.read(into).is_ok_and(|n| n == 13)
        };
        assert!(
            !read(&mut file),
            "the kernel does not fault into our handler"
        );
        TitleMemory::will_write(view + 100, 13);
        assert!(read(&mut file));
        let landed = unsafe { std::slice::from_raw_parts((view + 100) as *const u8, 13) };
        assert_eq!(landed, b"from the disk");
        assert_eq!(
            PageGuards::guard(view, view + 2 * BLOCK),
            Some((view / PAGE..view / PAGE + 1).collect())
        );
        unsafe {
            UnmapViewOfFile(
                windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: view as *mut _,
                },
            )
        };
        TitleMemory::remapped(view, 2 * BLOCK);
    }

    #[test]
    fn writes_in_partly_committed_view_blocks_are_reported() {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
            SEC_RESERVE, UnmapViewOfFile,
        };

        const SIZE: usize = 3 * BLOCK;
        struct View(MEMORY_MAPPED_VIEW_ADDRESS);
        impl Drop for View {
            fn drop(&mut self) {
                TitleMemory::remapped(self.0.Value as usize, SIZE);
                unsafe { UnmapViewOfFile(self.0) };
            }
        }
        let view = unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE | SEC_RESERVE,
                0,
                SIZE as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let view = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, SIZE);
            CloseHandle(section);
            assert!(!view.Value.is_null());
            View(view)
        };
        let start = view.0.Value as usize;
        let commit = |at: usize, len| {
            let committed = unsafe { VirtualAlloc(at as _, len, MEM_COMMIT, PAGE_READWRITE) };
            assert_eq!(committed as usize, at);
        };
        let early = start + BLOCK + 3 * KERNEL_PAGE;
        let late = start + 2 * BLOCK + 3 * KERNEL_PAGE;
        commit(start, BLOCK);
        commit(early, KERNEL_PAGE);
        let refresh = || PageGuards::guard(start, start + SIZE).unwrap();
        assert_eq!(refresh(), [start / PAGE, early / PAGE, late / PAGE]);
        assert!(!refresh().contains(&(late / PAGE)));

        commit(late, KERNEL_PAGE);
        for at in [early, late] {
            assert!(TitleMemory::writable(at));
            unsafe { (at as *mut u8).write_volatile(1) };
        }
        assert_eq!(refresh(), [early / PAGE, late / PAGE]);
    }

    #[test]
    fn a_range_that_begins_in_an_unmapped_block_is_guarded() {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER,
            MEM_RESERVE_PLACEHOLDER, MapViewOfFile3, PAGE_NOACCESS, UnmapViewOfFile2,
            VirtualAlloc2,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let (p, section) = unsafe {
            let p = VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                3 * BLOCK,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            ) as usize;
            assert_ne!(p, 0);
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (3 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            (p, section)
        };
        let map = |block: usize| {
            let view = unsafe {
                VirtualFree(
                    (p + block * BLOCK) as *mut _,
                    BLOCK,
                    MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                );
                MapViewOfFile3(
                    section,
                    GetCurrentProcess(),
                    (p + block * BLOCK) as *const _,
                    (block * BLOCK) as u64,
                    BLOCK,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                )
            };
            assert_eq!(view.Value as usize, p + block * BLOCK);
            view
        };
        let pages = |block: usize| (p + block * BLOCK) / PAGE..(p + (block + 1) * BLOCK) / PAGE;
        let all: Vec<usize> = (0..3).flat_map(pages).collect();
        assert!(PageGuards::is_view(p, p + 3 * BLOCK));
        let middle = map(1);
        TitleMemory::remapped(p + BLOCK, BLOCK);
        assert!(PageGuards::is_view(p, p + 3 * BLOCK));
        assert_eq!(PageGuards::guard(p, p + 3 * BLOCK), Some(all));
        assert_eq!(
            PageGuards::guard(p, p + 3 * BLOCK),
            Some(Vec::new()),
            "nothing written since"
        );
        unsafe { ((p + BLOCK + 8) as *mut u8).write_volatile(1) };
        assert_eq!(
            PageGuards::guard(p, p + 3 * BLOCK),
            Some(pages(1).collect())
        );
        let first = map(0);
        TitleMemory::remapped(p, BLOCK);
        assert_eq!(
            PageGuards::guard(p, p + 3 * BLOCK),
            Some(pages(0).collect())
        );
        assert_eq!(PageGuards::guard(p, p + 3 * BLOCK), Some(Vec::new()));
        let mut local = [0u8; 16];
        let at = local.as_mut_ptr() as usize;
        assert!(!PageGuards::is_view(at, at + local.len()));
        unsafe {
            UnmapViewOfFile2(GetCurrentProcess(), first, MEM_PRESERVE_PLACEHOLDER);
            UnmapViewOfFile2(GetCurrentProcess(), middle, MEM_PRESERVE_PLACEHOLDER);
            for block in 0..3 {
                VirtualFree((p + block * BLOCK) as *mut _, 0, MEM_RELEASE);
            }
            windows_sys::Win32::Foundation::CloseHandle(section);
        }
        TitleMemory::remapped(p, 3 * BLOCK);
    }

    #[test]
    fn unmapped_blocks_are_reported_once_until_mapped() {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER,
            MEM_RESERVE_PLACEHOLDER, MapViewOfFile3, PAGE_NOACCESS, UnmapViewOfFile2,
            VirtualAlloc2,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let (p, section) = unsafe {
            let p = VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                3 * BLOCK,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            ) as usize;
            assert_ne!(p, 0);
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            (p, section)
        };
        let map = |block: usize| {
            let view = unsafe {
                VirtualFree(
                    (p + block * BLOCK) as *mut _,
                    BLOCK,
                    MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                );
                MapViewOfFile3(
                    section,
                    GetCurrentProcess(),
                    (p + block * BLOCK) as *const _,
                    (block * BLOCK) as u64,
                    BLOCK,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                )
            };
            assert_eq!(view.Value as usize, p + block * BLOCK);
            view
        };
        let first = map(0);
        let pages = |block: usize| (p + block * BLOCK) / PAGE..(p + (block + 1) * BLOCK) / PAGE;
        let all: Vec<usize> = (0..3).flat_map(pages).collect();
        assert_eq!(PageGuards::guard(p, p + 3 * BLOCK), Some(all));
        assert_eq!(
            PageGuards::guard(p, p + 3 * BLOCK),
            Some(Vec::new()),
            "nothing written since"
        );
        let second = map(1);
        TitleMemory::remapped(p + BLOCK, BLOCK);
        assert_eq!(
            PageGuards::guard(p, p + 3 * BLOCK),
            Some(pages(1).collect())
        );
        assert_eq!(PageGuards::guard(p, p + 3 * BLOCK), Some(Vec::new()));
        unsafe {
            UnmapViewOfFile2(GetCurrentProcess(), first, MEM_PRESERVE_PLACEHOLDER);
            UnmapViewOfFile2(GetCurrentProcess(), second, MEM_PRESERVE_PLACEHOLDER);
            for block in 0..3 {
                VirtualFree((p + block * BLOCK) as *mut _, 0, MEM_RELEASE);
            }
            windows_sys::Win32::Foundation::CloseHandle(section);
        }
        TitleMemory::remapped(p, 3 * BLOCK);
    }

    fn two_views(bytes: usize) -> (usize, usize, usize) {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile,
        };
        unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                bytes as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let one = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, bytes).Value as usize;
            let two = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, bytes).Value as usize;
            assert_ne!(one, 0);
            assert_ne!(two, 0);
            crate::inplace::TitleSections::view(one, bytes, section as usize, 0);
            crate::inplace::TitleSections::view(two, bytes, section as usize, 0);
            (section as usize, one, two)
        }
    }

    #[test]
    #[ignore = "run on Windows to measure mapped-view guard checks"]
    fn measures_mapped_guard_scans() {
        use std::time::Instant;
        for blocks in [8, 1024] {
            let size = blocks * BLOCK;
            let (section, main, alias) = two_views(size);
            assert_eq!(PageGuards::guard(main, main + size).unwrap().len(), blocks);
            for dirty in [false, true] {
                let rounds = if dirty { 500 } else { 12_000 };
                let mut samples = Vec::new();
                for _ in 0..5 {
                    let begin = Instant::now();
                    for cycle in 0..rounds {
                        let block = (cycle * 17) % blocks;
                        if dirty {
                            unsafe { ((alias + block * BLOCK) as *mut u8).write_volatile(1) };
                        }
                        let changed = PageGuards::guard(main, main + size).unwrap();
                        if dirty {
                            assert_eq!(changed, [(main + block * BLOCK) / PAGE]);
                        } else {
                            assert!(changed.is_empty());
                        }
                    }
                    samples.push(begin.elapsed().as_secs_f64() * 1e6 / rounds as f64);
                }
                samples.sort_by(f64::total_cmp);
                println!(
                    "{blocks} blocks, alias write={dirty}: median {:.3} us/check; samples {samples:?}",
                    samples[2]
                );
            }
            drop_views(section, &[main, alias], size);
        }
    }

    fn drop_views(section: usize, views: &[usize], bytes: usize) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
        for &view in views {
            TitleMemory::remapped(view, bytes);
            unsafe {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: view as *mut _,
                })
            };
        }
        unsafe { CloseHandle(section as *mut std::ffi::c_void) };
    }

    #[test]
    fn a_write_through_another_view_is_reported_at_the_guarded_one() {
        const SIZE: usize = 2 * BLOCK;
        let (section, guarded, other) = two_views(SIZE);
        let pages = |block: usize| (guarded + block * BLOCK) / PAGE;
        assert_eq!(
            PageGuards::guard(guarded, guarded + SIZE),
            Some(vec![pages(0), pages(1)])
        );
        assert_eq!(
            PageGuards::guard(guarded, guarded + SIZE),
            Some(Vec::new()),
            "nothing written yet"
        );
        unsafe { ((other + BLOCK + 8) as *mut u8).write(1) };
        assert_eq!(
            PageGuards::guard(guarded, guarded + SIZE),
            Some(vec![pages(1)])
        );
        assert_eq!(PageGuards::guard(guarded, guarded + SIZE), Some(Vec::new()));
        drop_views(section, &[guarded, other], SIZE);
    }

    #[test]
    fn a_view_mapped_beside_a_guarded_one_makes_it_check_again() {
        const SIZE: usize = 2 * BLOCK;
        let (section, guarded, other) = two_views(SIZE);
        let pages = |block: usize| (guarded + block * BLOCK) / PAGE;
        assert_eq!(
            PageGuards::guard(guarded, guarded + SIZE),
            Some(vec![pages(0), pages(1)])
        );
        assert_eq!(
            PageGuards::guard(guarded, guarded + SIZE),
            Some(Vec::new()),
            "nothing written yet"
        );
        TitleMemory::remapped(other, SIZE);
        crate::inplace::TitleSections::view(other, SIZE, section, 0);
        assert_eq!(
            PageGuards::guard(guarded, guarded + SIZE),
            Some(vec![pages(0), pages(1)])
        );
        assert_eq!(PageGuards::guard(guarded, guarded + SIZE), Some(Vec::new()));
        drop_views(section, &[guarded, other], SIZE);
    }

    #[test]
    fn a_write_told_at_one_view_is_reported_at_the_others() {
        const SIZE: usize = 2 * BLOCK;
        let (section, guarded, other) = two_views(SIZE);
        let pages = |block: usize| (guarded + block * BLOCK) / PAGE;
        assert_eq!(
            PageGuards::guard(guarded, guarded + SIZE),
            Some(vec![pages(0), pages(1)])
        );
        assert_eq!(
            PageGuards::guard(guarded, guarded + SIZE),
            Some(Vec::new()),
            "nothing written yet"
        );
        TitleMemory::will_write(other + BLOCK, BLOCK);
        assert_eq!(
            PageGuards::guard(guarded, guarded + SIZE),
            Some(vec![pages(1)])
        );
        assert_eq!(PageGuards::guard(guarded, guarded + SIZE), Some(Vec::new()));
        TitleMemory::will_write(other + BLOCK / 2, BLOCK);
        assert_eq!(
            PageGuards::guard(guarded, guarded + SIZE),
            Some(vec![pages(0), pages(1)])
        );
        drop_views(section, &[guarded, other], SIZE);
    }

    #[test]
    fn a_write_is_reported_at_every_checked_view() {
        const SIZE: usize = 2 * BLOCK;
        let (section, one, two) = two_views(SIZE);
        let pages = |view: usize, block: usize| (view + block * BLOCK) / PAGE;
        assert_eq!(
            PageGuards::guard(one, one + SIZE),
            Some(vec![pages(one, 0), pages(one, 1)])
        );
        assert_eq!(
            PageGuards::guard(two, two + SIZE),
            Some(vec![pages(two, 0), pages(two, 1)]),
            "the second view's first check"
        );
        assert_eq!(
            PageGuards::guard(one, one + SIZE),
            Some(Vec::new()),
            "nothing written yet"
        );
        assert_eq!(
            PageGuards::guard(two, two + SIZE),
            Some(Vec::new()),
            "nothing written yet"
        );
        unsafe { ((two + BLOCK + 8) as *mut u8).write(1) };
        assert_eq!(
            PageGuards::guard(one, one + SIZE),
            Some(vec![pages(one, 1)])
        );
        assert_eq!(
            PageGuards::guard(two, two + SIZE),
            Some(vec![pages(two, 1)]),
            "written through this view, checked second"
        );
        unsafe { ((one + 8) as *mut u8).write(1) };
        assert_eq!(
            PageGuards::guard(two, two + SIZE),
            Some(vec![pages(two, 0)])
        );
        assert_eq!(
            PageGuards::guard(one, one + SIZE),
            Some(vec![pages(one, 0)]),
            "written through this view, checked second"
        );
        assert_eq!(PageGuards::guard(one, one + SIZE), Some(Vec::new()));
        assert_eq!(PageGuards::guard(two, two + SIZE), Some(Vec::new()));
        drop_views(section, &[one, two], SIZE);
    }

    #[test]
    fn a_page_of_many_views_is_guarded_one_view_at_a_time() {
        let (section, one, two) = two_views(BLOCK);
        let filler = one + (1 << 45);
        for n in 0..MANY_VIEWS {
            crate::inplace::TitleSections::view(filler + n * BLOCK, BLOCK, section, 0);
        }
        assert_eq!(PageGuards::guard(one, one + BLOCK), Some(vec![one / PAGE]));
        assert!(
            TitleMemory::writable(two),
            "another view of it is left alone"
        );
        unsafe { ((one + 8) as *mut u8).write(1) };
        assert_eq!(PageGuards::guard(one, one + BLOCK), Some(vec![one / PAGE]));
        TitleMemory::remapped(filler, MANY_VIEWS * BLOCK);
        drop_views(section, &[one, two], BLOCK);
    }

    #[test]
    fn ranges_past_the_address_space_end_do_not_overflow() {
        let top = usize::MAX - 16;
        TitleMemory::will_write(top, PAGE);
        TitleMemory::remapped(top, usize::MAX);
    }
}

#[cfg(test)]
mod mapping_tests {
    use super::TitleMemory;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    #[test]
    fn reads_in_place_and_remappings_wait_for_each_other() {
        let changed = Arc::new(AtomicBool::new(false));
        let reading = TitleMemory::reading();
        let (began, wait) = mpsc::channel();
        let change = {
            let changed = changed.clone();
            std::thread::spawn(move || {
                began.send(()).unwrap();
                let _changing = TitleMemory::remapping_range(0x10000..0x20000);
                changed.store(true, Ordering::SeqCst);
            })
        };
        wait.recv().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !changed.load(Ordering::SeqCst),
            "the mapping changed under a read in place"
        );
        drop(reading);
        change.join().unwrap();
        assert!(changed.load(Ordering::SeqCst));
        let read = Arc::new(AtomicBool::new(false));
        let changing = TitleMemory::remapping();
        let (began, wait) = mpsc::channel();
        let reader = {
            let read = read.clone();
            std::thread::spawn(move || {
                began.send(()).unwrap();
                let _reading = TitleMemory::reading_range(0x30000..0x40000);
                read.store(true, Ordering::SeqCst);
            })
        };
        wait.recv().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !read.load(Ordering::SeqCst),
            "a read in place began during a change of mapping"
        );
        drop(changing);
        reader.join().unwrap();
        assert!(read.load(Ordering::SeqCst));
    }
}

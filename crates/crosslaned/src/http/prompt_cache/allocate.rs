//! Pure prompt-cache allocation. Every value is a token count on one profile's
//! local basis (`full`); the result is scaled onto the round's normalized input
//! `I` so that `read + write + uncached == I` always holds.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Ttl {
    FiveMinutes,
    OneHour,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Boundary {
    Exact,
    /// Older OpenAI semantics: cached reads round down to 128 tokens. The
    /// rounded-off part is uncached input, never a cache write. No catalog
    /// model uses it today (GPT-5.6 reports exact boundaries).
    #[allow(dead_code)]
    Round128,
}

/// A breakpoint that can write a cache entry, as a cumulative prefix count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WritePoint {
    pub cum: u64,
    pub ttl: Ttl,
}

/// A prefix found in the snapshot. `long_lived` means a valid 1h slot exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Hit {
    pub cum: u64,
    pub long_lived: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Allocation {
    pub read: u64,
    pub write_5m: u64,
    pub write_1h: u64,
    pub uncached: u64,
}

impl Allocation {
    pub fn uncached_only(input: u64) -> Self {
        Self {
            uncached: input,
            ..Self::default()
        }
    }

    pub fn write(&self) -> u64 {
        self.write_5m.saturating_add(self.write_1h)
    }
}

/// Choose the longest hit that the official mixed-TTL rule accepts: a hit held
/// only by a 5m entry does not count when a 1h breakpoint after it misses.
pub(crate) fn select_hit(hits: &[Hit], writes: &[WritePoint]) -> Option<Hit> {
    let mut ordered = hits.to_vec();
    ordered.sort_by(|a, b| b.cum.cmp(&a.cum));
    ordered.into_iter().find(|hit| {
        hit.long_lived
            || !writes
                .iter()
                .any(|write| write.ttl == Ttl::OneHour && write.cum > hit.cum)
    })
}

fn scale(shared: u64, x: u64, full: u64) -> u64 {
    // x <= full, so the quotient never exceeds `shared` and fits in u64.
    ((u128::from(shared) * u128::from(x)) / u128::from(full)) as u64
}

/// Returns `None` when no local cache accounting applies; callers then report
/// the whole input as uncached and must not publish entries.
pub(crate) fn allocate(
    input: u64,
    full: u64,
    writes: &[WritePoint],
    hit: Option<Hit>,
    min_tokens: u64,
    boundary: Boundary,
) -> Option<Allocation> {
    if input == 0 || full == 0 {
        return None;
    }
    let shared = input.min(full);
    let qualified: Vec<WritePoint> = writes
        .iter()
        .copied()
        .filter(|write| write.cum < full && scale(shared, write.cum, full) >= min_tokens)
        .collect();
    let last = qualified.iter().map(|write| write.cum).max()?;
    let matched = hit
        .map(|hit| hit.cum)
        .filter(|cum| scale(shared, *cum, full) >= min_tokens)
        .unwrap_or(0)
        .min(last);
    let read_exact = scale(shared, matched, full);
    let read = match boundary {
        Boundary::Exact => read_exact,
        Boundary::Round128 => read_exact / 128 * 128,
    };
    // Position B: the last 1h breakpoint after the hit, or the hit itself.
    let long_end = qualified
        .iter()
        .filter(|write| write.ttl == Ttl::OneHour && write.cum > matched)
        .map(|write| write.cum)
        .max()
        .unwrap_or(matched);
    let write_1h = scale(shared, long_end, full).saturating_sub(read_exact);
    let write_5m = scale(shared, last, full).saturating_sub(scale(shared, long_end, full));
    let cached = read.saturating_add(write_1h).saturating_add(write_5m);
    Some(Allocation {
        read,
        write_5m,
        write_1h,
        uncached: input.checked_sub(cached)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn short(cum: u64) -> WritePoint {
        WritePoint {
            cum,
            ttl: Ttl::FiveMinutes,
        }
    }

    fn hit(cum: u64) -> Option<Hit> {
        Some(Hit {
            cum,
            long_lived: false,
        })
    }

    fn triple(allocation: Allocation) -> (u64, u64, u64) {
        (allocation.read, allocation.write(), allocation.uncached)
    }

    fn run(input: u64, full: u64, last: u64, matched: u64, min: u64) -> Option<(u64, u64, u64)> {
        let matched = (matched > 0).then_some(matched).and_then(hit);
        allocate(input, full, &[short(last)], matched, min, Boundary::Exact).map(triple)
    }

    #[test]
    fn plan_fixtures_hold() {
        assert_eq!(run(4000, 16000, 12000, 8000, 512), Some((2000, 1000, 1000)));
        assert_eq!(run(2200, 2200, 2100, 2100, 512), Some((2100, 0, 100)));
        assert_eq!(run(4000, 3000, 2400, 1600, 512), Some((1600, 800, 1600)));
        assert_eq!(run(1100, 1100, 1024, 0, 1024), Some((0, 1024, 76)));
        assert_eq!(run(1100, 1100, 1023, 0, 1024), None);
        assert_eq!(run(10000, 10000, 3000, 0, 1024), Some((0, 3000, 7000)));
    }

    #[test]
    fn degenerate_inputs_skip_local_accounting() {
        assert_eq!(run(0, 1000, 900, 0, 1), None);
        assert_eq!(run(1000, 0, 0, 0, 1), None);
        // A breakpoint with no tail is not a valid write point.
        assert_eq!(run(2000, 2000, 2000, 0, 1), None);
        assert_eq!(allocate(2000, 2000, &[], None, 1, Boundary::Exact), None);
    }

    #[test]
    fn mixed_ttl_fixture_splits_long_and_short_writes() {
        let writes = [
            WritePoint {
                cum: 3000,
                ttl: Ttl::OneHour,
            },
            short(4000),
        ];
        let hits = [Hit {
            cum: 2000,
            long_lived: false,
        }];
        // The 5m hit at 2000 precedes a missed 1h breakpoint, so it is void.
        assert_eq!(select_hit(&hits, &writes), None);
        let allocation = allocate(5000, 5000, &writes, None, 512, Boundary::Exact).unwrap();
        assert_eq!(triple(allocation), (0, 4000, 1000));
        assert_eq!((allocation.write_1h, allocation.write_5m), (3000, 1000));
    }

    #[test]
    fn long_lived_hits_survive_later_long_breakpoints() {
        let writes = [
            WritePoint {
                cum: 3000,
                ttl: Ttl::OneHour,
            },
            short(4000),
        ];
        let hits = [Hit {
            cum: 2000,
            long_lived: true,
        }];
        let selected = select_hit(&hits, &writes);
        assert_eq!(selected.map(|hit| hit.cum), Some(2000));
        let allocation = allocate(5000, 5000, &writes, selected, 512, Boundary::Exact).unwrap();
        assert_eq!(
            (allocation.read, allocation.write_1h, allocation.write_5m),
            (2000, 1000, 1000)
        );
    }

    #[test]
    fn short_hits_count_when_no_long_breakpoint_follows() {
        let writes = [short(4000)];
        let hits = [hit(1000).unwrap(), hit(3000).unwrap()];
        assert_eq!(select_hit(&hits, &writes).map(|hit| hit.cum), Some(3000));
    }

    #[test]
    fn round128_moves_the_remainder_to_uncached() {
        let allocation = allocate(
            3000,
            3000,
            &[short(2900)],
            hit(2000),
            1024,
            Boundary::Round128,
        )
        .unwrap();
        assert_eq!(allocation.read, 1920);
        assert_eq!(allocation.write(), 900);
        assert_eq!(allocation.uncached, 180);
    }

    #[test]
    fn hits_below_the_threshold_do_not_read() {
        let allocation =
            allocate(5000, 5000, &[short(4000)], hit(300), 1024, Boundary::Exact).unwrap();
        assert_eq!(triple(allocation), (0, 4000, 1000));
    }

    #[test]
    fn allocation_is_conserved_and_non_negative() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = |bound: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % bound.max(1)
        };
        for _ in 0..5000 {
            let full = next(200_000) + 1;
            let input = next(400_000);
            let last = next(full);
            let matched = next(last + 1);
            let min = next(5000);
            let boundary = if next(2) == 0 {
                Boundary::Exact
            } else {
                Boundary::Round128
            };
            let Some(allocation) =
                allocate(input, full, &[short(last)], hit(matched), min, boundary)
            else {
                continue;
            };
            assert_eq!(
                allocation.read + allocation.write() + allocation.uncached,
                input
            );
            assert!(allocation.uncached >= 1, "a positive tail always remains");
            if matched == last && boundary == Boundary::Exact {
                assert_eq!(allocation.write(), 0, "a warm prefix writes nothing");
            }
        }
    }
}
